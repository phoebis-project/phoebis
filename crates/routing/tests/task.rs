use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fake_upstream::{stays_for, wait_for, wait_for_async};
use phoebis_oauth::{Clock, ExtraUsage, UsageSnapshot, UsageWindow};
use phoebis_routing::{
    credits_from_usage, decide, lane_key, resolve, sample_from_usage, shed_dwell_ms, spawn,
    AffinityReport, BandWeight, BurnRates, BurnUnit, ClientClass, ConversationLookup, CreditsState,
    DecisionRecord, DecisionSummary, Exhaustion, HoldFacts, HoldReason, Judged, JudgedCharge,
    KeyRefusal, LongContextRates, MemberSpend, MoveCause, PinRef, PinUsage, Portability,
    PredecessorQuery, Presented, RateTable, Rates, Refusal, RefusalFacts, RefusedKey,
    RefusedRequest, RegimeRates, ReloadError, RenewalEnd, Report, ResolveError, RouterConfig,
    RouterHandle, RoutingState, SelectAnswer, SelectRequest, SelectResult, SharePricer,
    SpendResetSource, SpendWindow, UtilizationSample, WindowReading, WindowRestart, WindowSource,
    WriterStatus, COMMANDS, FAST_CREDITS_REASON, FAST_RATE_LIMIT_REASON, FIVE_HOUR,
    IN_FLIGHT_RECLAIM_MS, MODEL_REJECTED_REASON, RESET_SLACK_MS, SEVEN_DAY, SEVEN_DAY_FABLE,
    WRITER_DEPTH,
};
use phoebis_store::{
    key_prefix, AccountKind, AccountRow, AccountState, ApiKeyRow, Cancellation,
    CompanionPermissions, CooldownRow, CooldownScope, KeyFeatureAccounts, LapsedAgentRow,
    MemberWrite, ModelRule, OveragePolicy, OverageRow, PinBurn, PinKey, PinMark, PinRow, PoolRow,
    RoutingLoad, RulesMode, Shedding, StateCause, StorageKey, Store, Strategy, SubscriptionRow,
    ThinkingSum, WindowRow, WindowSetRow,
};

const HOUR: i64 = 3_600_000;

/// The wall clock, for a case that reads no stamp.
fn wall() -> Clock {
    Arc::new(|| jiff::Timestamp::now().as_millisecond())
}

fn seeded(ids: &[&str]) -> Arc<Store> {
    seeded_kind(AccountKind::ApiKey, ids)
}

/// The same pool over OAuth accounts, which are the only ones a usage sample is ever pulled for.
fn seeded_oauth(ids: &[&str]) -> Arc<Store> {
    seeded_kind(AccountKind::Oauth, ids)
}

fn seeded_kind(kind: AccountKind, ids: &[&str]) -> Arc<Store> {
    seed(
        Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap()),
        kind,
        ids,
    )
}

/// `store` with pool `p`'s burn average at 600 s, for a case whose figures fold over it: the case's own window,
/// whatever the stored default.
fn averaged(store: Arc<Store>) -> Arc<Store> {
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.shedding.burn_average_seconds = 600;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    store
}

/// The pool `p` over `ids` and the key `k`, written into a store the caller opened.
fn seed(store: Arc<Store>, kind: AccountKind, ids: &[&str]) -> Arc<Store> {
    for id in ids {
        store
            .insert_account(&AccountRow::for_tests(id, kind))
            .unwrap();
    }
    store
        .insert_pool(
            &PoolRow {
                strategy: Strategy::Priority,
                prefer_subscription: false,
                ..PoolRow::for_tests("p")
            },
            "test",
        )
        .unwrap();
    store
        .set_pool_members(
            "p",
            &ids.iter()
                .map(|id| MemberWrite {
                    account_id: id.to_string(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                })
                .collect::<Vec<_>>(),
            "test",
        )
        .unwrap();
    store
        .insert_api_key(
            &ApiKeyRow {
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
                companion_permissions: CompanionPermissions::default(),
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-secret").to_vec(),
                prefix: "secret00".into(),
                incarnation: 1,
                created_at: 0,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
                feature_accounts: KeyFeatureAccounts::default(),
            },
            "test",
        )
        .unwrap();
    store
}

fn clock(now: &Arc<AtomicI64>) -> Clock {
    let now = now.clone();
    Arc::new(move || now.load(Ordering::SeqCst))
}

/// A session id no other call of this suite answered, for a case that measures the strategy or the eligible set
/// with no pin in play.
fn fresh_session() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("s-fresh-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn request(
    handle: &phoebis_routing::RouterHandle,
    session: Option<&str>,
    now: i64,
) -> SelectRequest {
    let s = handle.snapshot();
    SelectRequest {
        request_id: "r-1".into(),
        attempt: 1,
        seq: 1,
        key_id: "k".into(),
        key_generation: s.key("k").unwrap().created_at,
        agent_id: "main".into(),
        session_id: session.map(String::from).unwrap_or_else(fresh_session),
        canonical_model: "claude-sonnet-5".into(),
        folded_model: "claude-sonnet-5".into(),
        candidates: resolve(
            &s,
            s.key("k").unwrap(),
            "claude-sonnet-5",
            "claude-sonnet-5",
        )
        .unwrap(),
        portability: Portability::Portable,
        credit_account: None,
        attempted: vec![],
        now,
        received_at: now,
        fallback_ms: 60_000,
        revision: s.revision,
        probe: false,
        holder: None,
        count: false,
        request_class: None,
        context_compacted: false,
        boundaries: Arc::from([]),
        client: ClientClass::ClaudeCode,
        entrypoint: None,
        static_boundaries: Arc::from([]),
        marker: false,
        parent_agent_id: None,
        failed_over: None,
        pin_ended: false,
        holds: Vec::new(),
        dialed: false,
        held_out: false,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        fast: false,
        classifier: false,
        body_tokens: 0,
        effort: None,
    }
}

/// How long a store write may take to reach disk before the test calls it lost. Only ever waited out in
/// full when the write never lands.
const WRITTEN: Duration = Duration::from_secs(2);

/// How long a full writer channel's worth of writes may take to land: a thousand statements in one wake, on a debug
/// build under the parallel suite. Only ever waited out in full when they never land.
const DRAINED: Duration = Duration::from_secs(10);

/// The window a write that must never land is given to prove it. A bounded negative check has to wait.
const NOT_WRITTEN: Duration = Duration::from_millis(50);

/// The same request, admitted under a snapshot the registry has since moved past.
fn stale(mut req: SelectRequest) -> SelectRequest {
    req.revision = req.revision.saturating_sub(1);
    req
}

/// A minimal affinity report: one message boundary, no cache evidence, under `account_id`'s current
/// incarnation and key `k`'s current generation.
fn affinity_report(
    handle: &phoebis_routing::RouterHandle,
    account_id: &str,
    now: i64,
) -> AffinityReport {
    let s = handle.snapshot();
    AffinityReport {
        account_id: account_id.into(),
        incarnation: s.account(account_id).unwrap().incarnation,
        key_id: "k".into(),
        key_generation: s.key("k").unwrap().created_at,
        canonical_model: "claude-sonnet-5".into(),
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        root: "s".into(),
        static_boundaries: Arc::from([]),
        message_boundaries: Arc::from(["b1".to_string()]),
        static_sizes: Vec::new(),
        static_ttl_ms: None,
        message_ttl_ms: None,
        cache_touched: false,
        prefix_tokens: 0,
        now,
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        uniform_ttl_ms: None,
        request_id: "r-1".into(),
        seq: 1,
        attempt: 1,
        message_id: None,
    }
}

#[tokio::test]
async fn a_selection_is_written_through_and_a_registry_write_shows_up_after_reload() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let answer = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    assert!(matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "a"));
    let pinned = wait_for(WRITTEN, || {
        store.load_routing().unwrap().pins.first().cloned()
    })
    .await;
    assert_eq!(pinned.account_id, "a");
    store
        .insert_account(&AccountRow::for_tests("b", AccountKind::ApiKey))
        .unwrap();
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "b".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    assert!(
        handle
            .snapshot()
            .members("p")
            .iter()
            .all(|m| m.account_id == "a"),
        "nothing moves before the reload"
    );
    handle.reload().await.unwrap();
    let answer = handle
        .select(request(&handle, Some("s1"), 1_001))
        .await
        .unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "b"),
        "the pin on a expired because a left the pool"
    );
    store.revoke_api_key("k", 2, "test").unwrap();
    handle.reload().await.unwrap();
    assert!(
        matches!(
            handle.snapshot().authenticate("phoebis-secret", 1_002),
            Err(RefusedKey {
                refusal: KeyRefusal::Revoked,
                ..
            })
        ),
        "a revoked key answers 401 at once"
    );
}

#[tokio::test]
async fn spawn_answers_an_attached_guardrail_through_the_first_snapshot_before_any_reload() {
    use phoebis_guardrails::{Condition, GuardrailAction, GuardrailMode, Operator, Subject};
    use phoebis_store::GuardrailRow;

    let store = seeded(&["a"]);
    store
        .insert_guardrail(
            &GuardrailRow {
                id: "auxiliary".into(),
                name: "Auxiliary".into(),
                description: String::new(),
                enabled: true,
                mode: GuardrailMode::Observe,
                action: GuardrailAction::Block,
                conditions: vec![Condition {
                    subject: Subject::RequestClass,
                    operator: Operator::Equals,
                    value: Some("auxiliary".into()),
                    name: None,
                    path: None,
                    selector: None,
                }],
                preset: None,
                incarnation: 1,
                created_at: 0,
                updated_at: 0,
            },
            &["p".to_string()],
            &[],
            "test",
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let ids = |handle: &RouterHandle| {
        let snapshot = handle.snapshot();
        snapshot
            .guardrails_for(snapshot.key("k").unwrap())
            .iter()
            .map(|g| g.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&handle), ["auxiliary"]);
    handle.reload().await.unwrap();
    assert_eq!(
        ids(&handle),
        ["auxiliary"],
        "a reload compiles the set again"
    );
    store.delete_guardrail("auxiliary", "test").unwrap();
    handle.reload().await.unwrap();
    assert!(ids(&handle).is_empty(), "a delete reaches the next reload");
}

#[tokio::test]
async fn a_request_admitted_under_an_older_registry_is_re_resolved_against_the_current_one() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // Admitted with both candidates; then `a` is deleted before the selection runs.
    let admitted = request(&handle, Some("s1"), 1_000);
    store.delete_account("a", "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted.clone())).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "b"),
        "the deleted candidate is dropped, not selected"
    );
    // The pool's policy changed under the request: the model is refused rather than routed.
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.model_rules = vec![
        ModelRule {
            pattern: "claude-opus-5".into(),
            not: false,
        },
        ModelRule {
            pattern: "*".into(),
            not: true,
        },
    ];
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted.clone())).await.unwrap();
    assert!(matches!(
        answer.result,
        SelectResult::Refused(Refusal::Resolve(ResolveError::ModelNotAllowed))
    ));
    store.revoke_api_key("k", 2, "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted)).await.unwrap();
    assert!(
        matches!(
            answer.result,
            SelectResult::Refused(Refusal::Key(KeyRefusal::Revoked))
        ),
        "a key revoked after admission is refused as admission refuses it"
    );
}

#[tokio::test]
async fn a_request_admitted_under_a_key_that_was_replaced_under_its_id_is_refused() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let admitted = request(&handle, Some("s1"), 1_000);
    // The key is deleted and created again under its id before the selection runs: another key, whatever
    // the id says.
    let mut again = store.api_key("k").unwrap().unwrap();
    again.secret_digest = store.api_key_digest("phoebis-reissued").to_vec();
    store.delete_api_key("k", "test").unwrap();
    store.insert_api_key(&again, "test").unwrap();
    handle.reload().await.unwrap();
    assert_ne!(
        store.api_key("k").unwrap().unwrap().created_at,
        admitted.key_generation,
        "the store stamped the replacement with its own generation"
    );
    let answer = handle.select(stale(admitted)).await.unwrap();
    assert!(
        matches!(
            answer.result,
            SelectResult::Refused(Refusal::Key(KeyRefusal::Replaced))
        ),
        "refused as a refused id is, never resolved against the replacement: {:?}",
        answer.result
    );
    assert!(
        handle.pins().await.unwrap().is_empty(),
        "no pin was written under the replacement key"
    );
}

#[tokio::test]
async fn a_request_admitted_under_a_key_deleted_before_selection_is_refused_as_unknown() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let admitted = request(&handle, Some("s1"), 1_000);
    store.delete_api_key("k", "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted)).await.unwrap();
    assert!(
        matches!(
            answer.result,
            SelectResult::Refused(Refusal::Key(KeyRefusal::Unknown))
        ),
        "the key the request was admitted on is gone: {:?}",
        answer.result
    );
    assert!(
        handle.pins().await.unwrap().is_empty(),
        "no pin was written under the deleted key"
    );
}

#[tokio::test]
async fn a_pool_with_no_member_at_admission_and_one_added_before_the_selection_sends_the_clients_spelling(
) {
    let store = seeded(&[]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // The client asked for the alias and the catalog resolved it to a dated id; the body carries the alias.
    let (canonical, folded) = ("claude-opus-5-5-20260901", "claude-opus-5-5");
    let s = handle.snapshot();
    let admitted = SelectRequest {
        canonical_model: canonical.into(),
        folded_model: folded.into(),
        candidates: resolve(&s, s.key("k").unwrap(), canonical, folded).unwrap(),
        lane: lane_key(canonical, "standard", "none", "-"),
        ..request(&handle, Some("s1"), 1_000)
    };
    assert!(
        admitted.candidates.is_empty(),
        "the pool had no member at admission"
    );
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::ApiKey))
        .unwrap();
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted)).await.unwrap();
    let SelectResult::Selected(selection) = answer.result else {
        panic!(
            "the member added since admission serves: {:?}",
            answer.result
        );
    };
    assert_eq!(selection.account.id, "a");
    assert_eq!(
        selection.upstream_model, folded,
        "the re-resolve sends the client's spelling as every other path does, never the canonical id"
    );
}

#[tokio::test]
async fn a_failed_reload_keeps_the_old_snapshot_and_says_so() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    store.revoke_api_key("k", 2, "test").unwrap();
    store.debug_fail_next_load();
    assert!(matches!(handle.reload().await, Err(ReloadError::Load(_))));
    assert!(
        handle
            .snapshot()
            .authenticate("phoebis-secret", 1_003)
            .is_ok(),
        "the old snapshot still authenticates the revoked key"
    );
    handle.reload().await.unwrap();
    assert!(
        matches!(
            handle.snapshot().authenticate("phoebis-secret", 1_004),
            Err(RefusedKey {
                refusal: KeyRefusal::Revoked,
                ..
            })
        ),
        "the next successful reload publishes the revocation"
    );
}

#[tokio::test]
async fn five_reloads_against_a_parked_loop_cost_at_most_two_loads_and_answer_every_caller() {
    let now = Arc::new(AtomicI64::new(1_000));
    let store = seeded(&["a"]);
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let before = store.debug_load_count();
    let park = handle.park_loop().await;
    let reloads: Vec<_> = (0..5)
        .map(|_| {
            let h = handle.clone();
            tokio::spawn(async move { h.reload().await })
        })
        .collect();
    drop(park);
    for reload in reloads {
        reload.await.unwrap().unwrap();
    }
    assert!(
        store.debug_load_count() - before <= 2,
        "{}",
        store.debug_load_count() - before
    );
}

#[tokio::test]
async fn a_selection_is_answered_while_a_registry_load_runs() {
    let now = Arc::new(AtomicI64::new(1_000));
    let store = seeded(&["a"]);
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let hold = store.debug_hold_next_load();
    let reload = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.reload().await })
    };
    wait_for(WRITTEN, || hold.is_parked().then_some(())).await;
    let answer = tokio::time::timeout(WRITTEN, handle.select(request(&handle, Some("s1"), 1_000)))
        .await
        .expect("the selection is answered while the load waits on the store")
        .unwrap();
    assert!(matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "a"));
    drop(hold);
    reload.await.unwrap().unwrap();
}

#[tokio::test]
async fn dropping_the_last_handle_ends_the_loop_and_frees_the_stores_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = seed(
        Arc::new(Store::open(&path, &key, wall()).unwrap()),
        AccountKind::ApiKey,
        &["a"],
    );
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let answer = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    assert!(matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "a"));
    drop(answer);
    drop(store);
    // No `shutdown`: the loop has to end on its own once nothing can send to it, releasing its store.
    drop(handle);
    let reopened = wait_for(DRAINED, || Store::open(&path, &key, wall()).ok()).await;
    drop(reopened);
}

#[tokio::test]
async fn shutdown_lands_every_acknowledged_write_before_it_returns() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    for i in 0..200 {
        let answer = handle
            .select(request(&handle, Some(&format!("s{i}")), 1_000 + i))
            .await
            .unwrap();
        assert!(matches!(answer.result, SelectResult::Selected(_)));
    }
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::ReauthRequired,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
            cause: StateCause::AuthFailed,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    handle.shutdown().await.unwrap();
    let load = store.load_routing().unwrap();
    assert_eq!(
        (load.pins.len(), load.cooldowns.len()),
        (200, 1),
        "nothing acknowledged was lost"
    );
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired,
        "including the mark queued last, which shutdown drains like any other write"
    );
    assert!(
        handle.select(request(&handle, None, 2_000)).await.is_err(),
        "the task is gone"
    );
}

#[tokio::test]
async fn a_router_handle_reads_alive_until_its_task_has_shut_down() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    // The readiness answer reads a clone, so the clone must see the end the shutting handle caused.
    let held = handle.clone();
    assert!(held.is_alive(), "a running task takes commands");
    handle.shutdown().await.unwrap();
    assert!(
        !held.is_alive(),
        "a task that shut down takes none, the condition every ask reports as RouterGone"
    );
}

/// Asserts the drop table reads `count` under `kind` and zero under every other kind, every kind present in the
/// listed order.
fn assert_dropped(handle: &RouterHandle, kind: &str, count: u64) {
    let dropped = handle.dropped_reports();
    assert_eq!(
        dropped.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        Report::KINDS,
        "every kind is read, in the listed order"
    );
    for (k, n) in dropped {
        let expected = if k == kind { count } else { 0 };
        assert_eq!(n, expected, "the drops counted under {k}");
    }
}

#[tokio::test]
async fn a_report_the_stopped_task_cannot_take_counts_one_drop_under_its_kind_and_nothing_under_the_others(
) {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    assert_dropped(&handle, "cooldown", 0);
    handle.shutdown().await.unwrap();
    assert_eq!(
        handle
            .report(model_cooldown("a", "model:claude-sonnet-5", 1_000))
            .await,
        None
    );
    assert_dropped(&handle, "cooldown", 1);
}

#[tokio::test]
async fn a_refusal_the_stopped_task_cannot_take_counts_one_drop() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(handle.dropped_refusals(), 0);
    handle.shutdown().await.unwrap();
    let (refused, refusal) = drift_refusal();
    handle.record_refusal(refused, refusal);
    assert_eq!(handle.dropped_refusals(), 1);
    assert_dropped(&handle, "cooldown", 0);
}

#[tokio::test]
async fn a_refusal_queued_behind_the_shutdown_lands_with_the_drain() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let park = handle.park_loop().await;
    let stopping = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    wait_for(WRITTEN, || (handle.queued() == 1).then_some(())).await;
    let (refused, refusal) = drift_refusal();
    handle.record_refusal(refused, refusal);
    drop(park);
    stopping.await.unwrap().unwrap();
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(
        (summary.kind, record.request.request_id.as_str()),
        ("refused", "r-drift")
    );
    assert_eq!(handle.dropped_refusals(), 0, "the mailbox took it");
}

#[tokio::test]
async fn a_parked_loop_holds_every_control_fact_and_drops_none() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let park = handle.park_loop().await;
    // The loop took its own park command off the queue, so every slot is free and only the last report waits for one.
    let reports: Vec<_> = (0..COMMANDS + 1)
        .map(|_| {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .report(model_cooldown("a", "model:claude-sonnet-5", 1_000))
                    .await
            })
        })
        .collect();
    wait_for(WRITTEN, || (handle.queued() == COMMANDS).then_some(())).await;
    assert_dropped(&handle, "cooldown", 0);
    drop(park);
    for report in reports {
        assert_eq!(report.await.unwrap(), None, "a cooldown takes no number");
    }
    let cooldowns = handle.cooldowns().await.unwrap();
    assert_eq!(
        cooldowns
            .iter()
            .filter(|c| c.account_id == "a"
                && c.scope == CooldownScope::Model("claude-sonnet-5".into()))
            .count(),
        1,
        "the held reports land once the loop runs again: {cooldowns:?}"
    );
    assert_dropped(&handle, "cooldown", 0);
}

#[tokio::test]
async fn two_samples_of_one_key_replace_each_other_and_the_newest_observed_alone_lands() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded_oauth(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let park = handle.park_loop().await;
    // The newer reading arrives first, so only the merge rule, never the arrival order, keeps it.
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.3, None)))
        .await;
    handle
        .report(observed("a", sample_with(1_500, FIVE_HOUR, 0.9, None)))
        .await;
    drop(park);
    let samples = handle.samples().await.unwrap();
    let (_, sample) = samples
        .iter()
        .find(|(id, _)| id == "a")
        .expect("a's sample landed");
    assert_eq!(
        sample
            .reading(FIVE_HOUR)
            .map(|r| (r.utilization, r.observed_at)),
        Some((0.3, 2_000)),
        "the newer observation alone"
    );
}

#[tokio::test]
async fn a_stale_account_state_never_replaces_a_current_pending_one() {
    let store = seeded(&["a"]);
    // A reconnect: the lifecycle moves on and the credential generation starts again.
    assert_eq!(store.bump_lifecycle("a", StateCause::Operator).unwrap(), 2);
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k2"))
        .unwrap();
    let credential = store.credential_generation("a").unwrap().unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let park = handle.park_loop().await;
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::ReauthRequired,
            incarnation: 1,
            lifecycle_generation: 2,
            credential_generation: credential,
            cause: StateCause::AuthFailed,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    // The previous lifecycle's verdict, under a credential generation the reconnect has not reached again.
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::FundingRequired,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: credential + 2,
            cause: StateCause::FundingRefused,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    drop(park);
    wait_for(WRITTEN, || {
        (store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
}

/// The handle counts a dropped report by looking its kind up in `Report::KINDS` and panics on a name the list lacks,
/// so one report of every variant pins `kind()` against the list.
#[tokio::test]
async fn every_report_kind_is_named_in_the_drop_counters_list_in_its_order() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let pin = PinRef {
        key_id: "k".into(),
        session_id: "s1".into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        epoch: 1,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        entrypoint: None,
        effort: None,
    };
    let reports = [
        Report::AccountState {
            account_id: "a".into(),
            state: AccountState::ReauthRequired,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
            cause: StateCause::AuthFailed,
            detail: None,
            dispatched_ordinal: None,
        },
        Report::Accounting { degraded: true },
        Report::Affinity(affinity_report(&handle, "a", 1_000)),
        Report::AgentEnded {
            key_id: "k".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
        },
        model_cooldown("a", "model:claude-sonnet-5", 1_000),
        credits_at(true, None, None, 1_000),
        Report::Ended {
            request_id: "r-1".into(),
            attempt: 1,
            at: 1_000,
            cause: "canceled",
        },
        Report::Head {
            request_id: "r".into(),
            attempt: 1,
            at: 1_000,
        },
        Report::MemberSpend(Vec::new()),
        Report::Overage {
            account_id: "a".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: None,
                reset: None,
                observed_at: 1_000,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        },
        Report::PinInvalidated {
            account_id: "a".into(),
            pin: pin.clone(),
            mark: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        },
        Report::PinReleased {
            pin: pin.clone(),
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        },
        renewal(pin, 1_000, PinUsage::default(), false, 0),
        observed("a", sample(1_000)),
        Report::SpendHead {
            account_id: "a".into(),
            utilization: Some(0.5),
            reset: None,
            observed_at: 1_000,
            incarnation: 1,
        },
        a_million_input("a", 1_000),
    ];
    for report in &reports {
        // No wildcard, so a new variant fails to compile here until it is named; it also needs a report above and a
        // name in `KINDS`.
        match report {
            Report::AccountState { .. }
            | Report::Accounting { .. }
            | Report::Affinity(_)
            | Report::AgentEnded { .. }
            | Report::Cooldown { .. }
            | Report::Credits { .. }
            | Report::Ended { .. }
            | Report::Head { .. }
            | Report::MemberSpend(_)
            | Report::Overage { .. }
            | Report::PinInvalidated { .. }
            | Report::PinReleased { .. }
            | Report::PinRenewed { .. }
            | Report::Sample { .. }
            | Report::SpendHead { .. }
            | Report::Usage { .. } => {}
        }
    }
    assert_eq!(
        reports.iter().map(Report::kind).collect::<Vec<_>>(),
        Report::KINDS,
        "every variant's kind is listed under the same spelling, in order, and the list names nothing else"
    );
}

#[tokio::test]
async fn a_state_verdict_about_a_replaced_credential_changes_nothing() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let (_, generation) = store.api_key_credential("a").unwrap().unwrap();
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k2"))
        .unwrap();
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::ReauthRequired,
            lifecycle_generation: 1,
            credential_generation: generation,
            incarnation: 1,
            cause: StateCause::AuthFailed,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    // The verdict named the key the store has since replaced, so the writer fences it out.
    stays_for(NOT_WRITTEN, || {
        store.account("a").unwrap().unwrap().state == AccountState::Ready
    })
    .await;
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::ReauthRequired,
            lifecycle_generation: 1,
            credential_generation: generation + 1,
            incarnation: 1,
            cause: StateCause::AuthFailed,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    wait_for(WRITTEN, || {
        (store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
}

#[tokio::test]
async fn two_concurrent_first_requests_of_one_session_land_on_one_account() {
    let store = seeded(&["a", "b", "c"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    for _ in 0..5 {
        let session = fresh_session();
        let (x, y) = tokio::join!(
            handle.select(request(&handle, Some(&session), 1_000)),
            handle.select(request(&handle, Some(&session), 1_000))
        );
        let id = |r: SelectResult| match r {
            SelectResult::Selected(s) => s.account.id,
            _ => panic!(),
        };
        let (x, y) = (id(x.unwrap().result), id(y.unwrap().result));
        assert_eq!(x, y);
    }
}

#[tokio::test]
async fn a_cooldown_survives_a_restart_and_an_expired_row_does_not_revive() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    {
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        handle
            .report(Report::Cooldown {
                account_id: "a".into(),
                row: CooldownRow {
                    account_id: "a".into(),
                    scope: CooldownScope::Account,
                    until: 1_000 + 2 * HOUR,
                    set_at: 0,
                    reason: "rate_limit".into(),
                    window: "five_hour".into(),
                    source_request_id: None,
                    incarnation: 1,
                    spend_share: None,
                },
                pin: None,
                lifecycle_generation: 1,
                credential_generation: 1,
                incarnation: 1,
            })
            .await;
        wait_for(WRITTEN, || {
            store
                .load_routing()
                .unwrap()
                .cooldowns
                .into_iter()
                .find(|c| c.scope == CooldownScope::Account)
        })
        .await;
    }
    store
        .upsert_cooldown(&CooldownRow {
            account_id: "a".into(),
            scope: CooldownScope::Model("claude-opus-5".into()),
            until: 500,
            set_at: 0,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        })
        .unwrap();
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let answer = handle.select(request(&handle, None, 1_000)).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Exhausted(Exhaustion::Timed { deadline, .. }) if deadline == 1_000 + 2 * HOUR)
    );
    let cooldowns = handle.cooldowns().await.unwrap();
    assert_eq!(
        cooldowns.len(),
        1,
        "the expired model row was purged at start"
    );
    assert!(handle
        .clear_cooldown("a", CooldownScope::Account)
        .await
        .unwrap());
    assert!(matches!(
        handle
            .select(request(&handle, None, 1_000))
            .await
            .unwrap()
            .result,
        SelectResult::Selected(_)
    ));
}

#[tokio::test]
async fn a_persisted_resolution_keys_the_dated_pin_after_a_restart_before_any_refresh() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let req = |handle: &phoebis_routing::RouterHandle, now: i64| {
        let s = handle.snapshot();
        SelectRequest {
            request_id: "r-1".into(),
            attempt: 1,
            seq: 1,
            key_id: "k".into(),
            key_generation: s.key("k").unwrap().created_at,
            agent_id: "main".into(),
            session_id: "s1".into(),
            canonical_model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            candidates: resolve(
                &s,
                s.key("k").unwrap(),
                "claude-sonnet-4-5",
                "claude-sonnet-4-5",
            )
            .unwrap(),
            portability: Portability::Portable,
            credit_account: None,
            attempted: vec![],
            now,
            received_at: now,
            fallback_ms: 60_000,
            revision: s.revision,
            probe: false,
            holder: None,
            count: false,
            request_class: None,
            context_compacted: false,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            pin_ended: false,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
        }
    };
    let epoch;
    {
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        let answer = handle.select(req(&handle, 1_000)).await.unwrap();
        let SelectResult::Selected(selection) = answer.result else {
            panic!("expected a selection")
        };
        assert_eq!(selection.account.id, "a");
        epoch = selection.pin.unwrap().epoch;
        handle
            .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 1_500)
            .await
            .unwrap();
        wait_for(WRITTEN, || {
            let load = store.load_routing().unwrap();
            let resolved = load.resolutions.iter().any(|r| {
                r.spelling == "claude-sonnet-4-5" && r.dated_id == "claude-sonnet-4-5-20250929"
            });
            let moved = load
                .pins
                .iter()
                .any(|p| p.model == "claude-sonnet-4-5-20250929");
            (resolved && moved).then_some(())
        })
        .await;
    }
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let answer = handle.select(req(&handle, 2_000)).await.unwrap();
    let SelectResult::Selected(selection) = answer.result else {
        panic!("expected a selection")
    };
    assert_eq!(
        (selection.account.id.as_str(), selection.pin.unwrap().epoch),
        ("a", epoch),
        "the pin renews under the dated id rather than starting over"
    );
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Model("claude-sonnet-4-5".into()),
                until: 2_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    let cooled = wait_for_async(WRITTEN, || async {
        handle
            .cooldowns()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.scope == CooldownScope::Model("claude-sonnet-4-5-20250929".into()))
    })
    .await;
    assert_eq!(cooled.until, 2_000 + HOUR);
}

/// A model cooldown for `account_id` under `scope`, an hour past `now`, as the relay reports one.
fn model_cooldown(account_id: &str, scope: &str, now: i64) -> Report {
    Report::Cooldown {
        account_id: account_id.into(),
        row: CooldownRow {
            account_id: account_id.into(),
            scope: CooldownScope::parse(scope).unwrap(),
            until: now + HOUR,
            set_at: now,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    }
}

#[tokio::test]
async fn resolve_answers_once_the_state_holds_the_spellings_pins_and_cooldowns_under_the_dated_id()
{
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let s = handle.snapshot();
    let answer = handle
        .select(SelectRequest {
            request_id: "r-1".into(),
            attempt: 1,
            seq: 1,
            key_id: "k".into(),
            key_generation: s.key("k").unwrap().created_at,
            agent_id: "main".into(),
            session_id: "s1".into(),
            canonical_model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            candidates: resolve(
                &s,
                s.key("k").unwrap(),
                "claude-sonnet-4-5",
                "claude-sonnet-4-5",
            )
            .unwrap(),
            portability: Portability::Portable,
            credit_account: None,
            attempted: vec![],
            now: 1_000,
            received_at: 1_000,
            fallback_ms: 60_000,
            revision: s.revision,
            probe: false,
            holder: None,
            count: false,
            request_class: None,
            context_compacted: false,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            pin_ended: false,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
        })
        .await
        .unwrap();
    let SelectResult::Selected(selection) = answer.result else {
        panic!("expected a selection")
    };
    let other = if selection.account.id == "a" {
        "b"
    } else {
        "a"
    };
    handle
        .report(model_cooldown(other, "model:claude-sonnet-4-5", 1_000))
        .await;
    handle
        .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 1_500)
        .await
        .unwrap();
    let pins = handle.pins().await.unwrap();
    assert_eq!(
        pins.iter().map(|p| p.model.as_str()).collect::<Vec<_>>(),
        ["claude-sonnet-4-5-20250929"]
    );
    let cooldowns = handle.cooldowns().await.unwrap();
    assert_eq!(
        cooldowns
            .iter()
            .map(|c| c.scope.to_string())
            .collect::<Vec<_>>(),
        ["model:claude-sonnet-4-5-20250929"]
    );
}

#[tokio::test]
async fn a_pin_under_the_folded_spelling_holds_its_member_for_a_non_portable_turn_after_the_resolution(
) {
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = seeded(&["a", "b"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.strategy = Strategy::RoundRobin;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let turn = |model: &str, portability: Portability, at: i64| SelectRequest {
        portability,
        ..turn_of(&handle, "s1", "main", model, at)
    };
    let SelectResult::Selected(opening) = handle
        .select(turn("claude-sonnet-4-5", Portability::Portable, 1_000))
        .await
        .unwrap()
        .result
    else {
        panic!()
    };
    assert_eq!(opening.account.id, "a");
    let epoch = opening.pin.unwrap().epoch;
    handle
        .resolve("claude-sonnet-4-5", DATED, 2_000)
        .await
        .unwrap();
    let spelled = PinKey {
        key_id: "k".to_string(),
        session_id: "s1".to_string(),
        agent_id: "main".to_string(),
        model: "claude-sonnet-4-5".to_string(),
    };
    let keys: Vec<PinKey> = handle
        .pins()
        .await
        .unwrap()
        .iter()
        .map(PinRow::key)
        .collect();
    assert!(
        !keys.contains(&spelled),
        "the pin under the folded spelling is dropped"
    );
    assert!(
        keys.iter().any(|key| key.model == DATED),
        "and re-written under the dated id: {keys:?}"
    );
    assert_eq!(
        handle
            .canonicals(vec!["claude-sonnet-4-5".into()])
            .await
            .unwrap(),
        [DATED],
        "and the resolution itself travels with them"
    );
    now.store(3_000, Ordering::SeqCst);
    let SelectResult::Selected(sel) = handle
        .select(turn(DATED, Portability::Hold, 3_000))
        .await
        .unwrap()
        .result
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
    now.store(4_000, Ordering::SeqCst);
    assert_eq!(
        landed_on(
            handle
                .select(turn("claude-sonnet-4-5", Portability::Hold, 4_000))
                .await
                .unwrap()
        )
        .0,
        "a",
        "the folded spelling still finds the same pin"
    );
}

#[tokio::test]
async fn a_rejection_cooldown_under_the_folded_spelling_still_excludes_the_member_under_the_dated_id(
) {
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(500));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let rejection = |until: i64| Report::Cooldown {
        account_id: "a".into(),
        row: CooldownRow {
            account_id: "a".into(),
            scope: CooldownScope::Model("claude-sonnet-4-5".into()),
            until,
            set_at: 0,
            reason: MODEL_REJECTED_REASON.into(),
            window: "model_rejection".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    };
    handle.report(rejection(1_000 + HOUR)).await;
    handle
        .resolve("claude-sonnet-4-5", DATED, 500)
        .await
        .unwrap();
    now.store(1_000, Ordering::SeqCst);
    // Each call names a fresh session, so the second request finds no pin of the first.
    let dated = || {
        let s = handle.snapshot();
        SelectRequest {
            canonical_model: DATED.into(),
            folded_model: DATED.into(),
            candidates: resolve(&s, s.key("k").unwrap(), DATED, DATED).unwrap(),
            lane: lane_key(DATED, "standard", "none", "-"),
            ..request(&handle, None, 1_000)
        }
    };
    assert_eq!(landed_on(handle.select(dated()).await.unwrap()).0, "b");
    let mut solo = dated();
    solo.candidates.retain(|c| c.account_id != "b");
    assert_eq!(
        handle.select(solo).await.unwrap().result,
        SelectResult::Exhausted(Exhaustion::ModelRejected),
        "no second attempt on the cooled member"
    );
    handle.report(rejection(2_000 + HOUR)).await;
    assert_eq!(
        handle
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .find(|c| c.account_id == "a" && c.scope == CooldownScope::Model(DATED.into()))
            .map(|c| c.until),
        Some(2_000 + HOUR),
        "a cooldown recorded under the folded scope after the landing is stored under the dated scope"
    );
}

#[tokio::test]
async fn a_resolution_between_two_turns_keeps_the_pin_warm_on_the_dated_id() {
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let turn = |model: &str, at: i64, boundaries: &[&str]| SelectRequest {
        request_class: Some("main".into()),
        boundaries: boundaries.iter().map(|b| b.to_string()).collect(),
        ..turn_of(&handle, "s1", "main", model, at)
    };
    handle
        .select(turn("claude-sonnet-4-5", AT, &["m1"]))
        .await
        .unwrap();
    handle
        .resolve("claude-sonnet-4-5", DATED, AT + 500)
        .await
        .unwrap();
    let dated_lane = lane_key(DATED, "standard", "none", "-");
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|p| p.lane.as_str())
            .collect::<Vec<_>>(),
        [dated_lane.as_str()],
        "the moved pin's lane names the model the next request composes it from"
    );
    now.store(AT + 1_000, Ordering::SeqCst);
    handle
        .select(turn(DATED, AT + 1_000, &["m1", "m2"]))
        .await
        .unwrap();
    let pin = handle
        .pins()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.session_id == "s1" && p.model == DATED)
        .expect("the pin moved to the dated id");
    assert_eq!(
        pin.last_refresh_at,
        AT + 1_000,
        "the continuation after the resolution refreshes its pin"
    );
}

/// Account `a`'s model cooldown under the spelling is in the router's memory while the store has lost the
/// account behind its back, so the rekey's cooldown upsert breaks `cooldowns.account_id REFERENCES
/// accounts(id)` and the whole landing rolls back.
async fn a_refused_rekey() -> (Arc<Store>, phoebis_routing::RouterHandle) {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(model_cooldown("a", "model:claude-sonnet-4-5", 1_000))
        .await;
    wait_for(WRITTEN, || {
        (!store.load_routing().unwrap().cooldowns.is_empty()).then_some(())
    })
    .await;
    store.delete_account("a", "test").unwrap();
    handle
        .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 1_500)
        .await
        .unwrap();
    // The rekey's batch has left the channel, so the next report wakes the writer on its own rather than joining
    // the rekey's wake, where it would land beside the refusal instead of after it.
    wait_for(WRITTEN, || {
        (handle.writer_status().depth == 0).then_some(())
    })
    .await;
    (store, handle)
}

fn restore_a(store: &Store) {
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::ApiKey))
        .unwrap();
}

/// Waits for the writer to land `b`'s cooldown under `scope`, which it only reaches after every batch before it.
async fn landed(store: &Store, scope: &str) {
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .cooldowns
            .iter()
            .any(|c| c.account_id == "b" && c.scope.to_string() == scope)
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_rekey_the_store_refused_is_retried_once_at_the_next_write_and_then_dropped() {
    let (store, handle) = a_refused_rekey().await;
    restore_a(&store);
    handle
        .report(model_cooldown("b", "model:claude-opus-5", 1_000))
        .await;
    landed(&store, "model:claude-opus-5").await;
    let load = store.load_routing().unwrap();
    assert_eq!(
        load.resolutions
            .iter()
            .map(|r| (r.spelling.as_str(), r.dated_id.as_str()))
            .collect::<Vec<_>>(),
        [("claude-sonnet-4-5", "claude-sonnet-4-5-20250929")],
        "the retry landed the spelling row"
    );
    assert!(
        load.cooldowns.iter().any(|c| c.account_id == "a"
            && c.scope == CooldownScope::Model("claude-sonnet-4-5-20250929".into())),
        "beside the row it moved"
    );

    let (store, handle) = a_refused_rekey().await;
    handle
        .report(model_cooldown("b", "model:claude-opus-5", 1_000))
        .await;
    landed(&store, "model:claude-opus-5").await;
    restore_a(&store);
    handle
        .report(model_cooldown("b", "model:claude-haiku-4-5", 1_000))
        .await;
    landed(&store, "model:claude-haiku-4-5").await;
    assert!(
        store.load_routing().unwrap().resolutions.is_empty(),
        "a rekey that failed its one retry is dropped, not retried at every later write"
    );
}

/// Waits until the router's memory holds `b`'s cooldown under every scope: the loop has handled each report and
/// queued its write by then.
async fn queued(handle: &RouterHandle, scopes: &[String]) {
    wait_for_async(WRITTEN, || async {
        let cooldowns = handle.cooldowns().await.unwrap();
        scopes
            .iter()
            .all(|scope| {
                cooldowns
                    .iter()
                    .any(|c| c.account_id == "b" && c.scope.to_string() == *scope)
            })
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_burst_of_reports_queued_behind_one_wake_lands_in_one_commit() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // Read and armed before the task first runs, so the batch the purge tick queues at the start lands in one of
    // the two wakes counted below.
    let before = store.debug_routing_commits();
    let hold = store.debug_hold_next_routing_write();
    handle
        .report(model_cooldown("a", "model:claude-opus-5", 1_000))
        .await;
    wait_for(WRITTEN, || hold.is_parked().then_some(())).await;
    let scopes: Vec<String> = (0..20).map(|i| format!("model:claude-burst-{i}")).collect();
    for scope in &scopes {
        handle.report(model_cooldown("b", scope, 1_000)).await;
    }
    // Released before this, a report still in the command queue would land in a wake of its own.
    queued(&handle, &scopes).await;
    hold.release();
    wait_for(WRITTEN, || {
        (store.debug_routing_commits() >= before + 2).then_some(())
    })
    .await;
    assert_eq!(
        store.load_routing().unwrap().cooldowns.len(),
        21,
        "every report landed"
    );
    assert_eq!(
        store.debug_routing_commits() - before,
        2,
        "the parked wake and one wake for everything queued behind it"
    );
}

/// A router whose writer is parked on its first wake, the purge tick's batch: armed before the spawn, so every
/// write a test makes after this answers goes to the channel, and past `WRITER_DEPTH` of them to the backlog.
async fn parked_writer(
    store: &Arc<Store>,
    now: &Arc<AtomicI64>,
) -> (RouterHandle, phoebis_store::Hold) {
    let hold = store.debug_hold_next_routing_write();
    let handle = spawn(store.clone(), clock(now), RouterConfig::default()).unwrap();
    wait_for(WRITTEN, || hold.is_parked().then_some(())).await;
    (handle, hold)
}

/// A write of its own row per call: `b`'s cooldown under a scope no request asks for.
async fn fill(handle: &RouterHandle, index: usize) {
    handle
        .report(model_cooldown("b", &format!("model:fill-{index}"), 1_000))
        .await;
}

#[tokio::test]
async fn a_full_writer_channel_defers_its_writes_and_lands_them_once_a_slot_frees() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    let pin = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    // The placement and `WRITER_DEPTH - 1` renewals fill the channel; the last two renewals find it full and
    // share the pin's one row in the backlog.
    for i in 0..WRITER_DEPTH as i64 + 1 {
        handle
            .report(renewal(
                pin.clone(),
                1_001 + i,
                PinUsage::default(),
                false,
                0,
            ))
            .await;
    }
    // Asked behind the renewals, so the loop has handled every one of them.
    handle.pins().await.unwrap();
    assert_eq!(
        handle.writer_status(),
        WriterStatus {
            depth: WRITER_DEPTH,
            deferred: 1,
            degraded: true,
            failed_batches: 0,
        },
        "two writes past the channel's bound, held in memory as their row's last"
    );
    hold.release();
    let last = 1_001 + WRITER_DEPTH as i64;
    wait_for(DRAINED, || {
        let status = handle.writer_status();
        let landed = store
            .load_routing()
            .unwrap()
            .pins
            .iter()
            .any(|p| p.session_id == "s1" && p.last_used_at == last);
        (status.deferred == 0 && !status.degraded && landed).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_delete_arriving_while_the_backlog_holds_the_pins_upsert_never_recreates_the_pin() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    // Other rows fill the channel: a renewal of the pin would mark it served, and a release would leave it.
    for index in 0..WRITER_DEPTH {
        fill(&handle, index).await;
    }
    let pin = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    assert_eq!(
        handle.writer_status().deferred,
        1,
        "the placement waits in the backlog as its pin's row"
    );
    hold.release();
    handle
        .report(Report::PinReleased {
            pin,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    wait_for(DRAINED, || {
        let landed = store.load_routing().unwrap().cooldowns.len();
        (handle.writer_status().deferred == 0 && landed == WRITER_DEPTH).then_some(())
    })
    .await;
    handle.shutdown().await.unwrap();
    assert!(
        store
            .load_routing()
            .unwrap()
            .pins
            .iter()
            .all(|p| p.session_id != "s1"),
        "the release lands after the placement it releases, or replaces it in the backlog"
    );
}

#[tokio::test]
async fn a_return_arriving_while_the_backlog_holds_the_pins_release_lands_the_returns_row() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    // The placement and `WRITER_DEPTH - 1` fillers fill the channel, so the last filler, the release and the return
    // wait in the backlog.
    let pin = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    for index in 0..WRITER_DEPTH {
        fill(&handle, index).await;
    }
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(Report::PinReleased {
            pin,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    handle
        .select(SelectRequest {
            request_id: "r-2".into(),
            ..request(&handle, Some("s1"), 2_000)
        })
        .await
        .unwrap();
    assert_eq!(
        handle.writer_status().deferred,
        2,
        "the last filler and the pin's row, where the return's upsert replaced the release"
    );
    hold.release();
    wait_for(DRAINED, || {
        let landed = store.load_routing().unwrap().cooldowns.len();
        (handle.writer_status().deferred == 0 && landed == WRITER_DEPTH).then_some(())
    })
    .await;
    handle.shutdown().await.unwrap();
    assert_eq!(
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .map(|p| (p.session_id, p.created_at))
            .collect::<Vec<_>>(),
        [("s1".to_string(), 2_000)],
        "the return's row lands over the release it replaced in the backlog"
    );
}

#[tokio::test]
async fn a_shutdown_with_a_full_writer_channel_lands_every_deferred_write() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    for index in 0..WRITER_DEPTH + 1 {
        fill(&handle, index).await;
    }
    for session in ["s1", "s2", "s3"] {
        handle
            .select(request(&handle, Some(session), 1_000))
            .await
            .unwrap();
    }
    let placed = handle.pins().await.unwrap();
    assert_eq!(
        handle.writer_status().deferred,
        4,
        "the last filler and the three placements wait in the backlog, each placement as its pin's row"
    );
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    // Past this the loop has left its turn, so only the shutdown's own flush can take the backlog; a release
    // before it lets the turn's `writer_room` arm land it instead.
    wait_for(WRITTEN, || (!handle.is_alive()).then_some(())).await;
    hold.release();
    shutdown.await.unwrap().unwrap();
    let load = store.load_routing().unwrap();
    for pin in &placed {
        assert!(
            load.pins.contains(pin),
            "{} lands as placed: {:?}",
            pin.session_id,
            load.pins
        );
    }
    assert_eq!(placed.len(), 3);
    assert_eq!(load.cooldowns.len(), WRITER_DEPTH + 1);
}

/// `a_refused_rekey`'s shape with the writer parked on an earlier write before the resolution, so the refused
/// rekey and whatever the test queues next share the wake after the release.
async fn a_refused_rekey_behind_a_hold() -> (Arc<Store>, RouterHandle, phoebis_store::Hold) {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(model_cooldown("a", "model:claude-sonnet-4-5", 1_000))
        .await;
    wait_for(WRITTEN, || {
        (!store.load_routing().unwrap().cooldowns.is_empty()).then_some(())
    })
    .await;
    store.delete_account("a", "test").unwrap();
    let hold = store.debug_hold_next_routing_write();
    handle
        .report(model_cooldown("b", "model:claude-opus-5", 1_000))
        .await;
    wait_for(WRITTEN, || hold.is_parked().then_some(())).await;
    handle
        .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 1_500)
        .await
        .unwrap();
    (store, handle, hold)
}

fn resolutions(store: &Store) -> Vec<(String, String)> {
    store
        .load_routing()
        .unwrap()
        .resolutions
        .into_iter()
        .map(|r| (r.spelling, r.dated_id))
        .collect()
}

#[tokio::test]
async fn a_batch_with_one_refused_write_lands_every_other_write() {
    let (store, handle, hold) = a_refused_rekey_behind_a_hold().await;
    let behind = ["model:claude-haiku-4-5", "model:claude-sonnet-5"].map(String::from);
    for scope in &behind {
        handle.report(model_cooldown("b", scope, 1_000)).await;
    }
    queued(&handle, &behind).await;
    hold.release();
    for scope in &behind {
        landed(&store, scope).await;
    }
    assert!(
        resolutions(&store).is_empty(),
        "the rekey the store refused was skipped"
    );

    restore_a(&store);
    handle
        .report(model_cooldown("b", "model:claude-fable-5", 1_000))
        .await;
    landed(&store, "model:claude-fable-5").await;
    assert_eq!(
        resolutions(&store),
        [(
            "claude-sonnet-4-5".to_string(),
            "claude-sonnet-4-5-20250929".to_string()
        )],
        "and carried to its one retry at the next wake"
    );
}

#[tokio::test]
async fn a_refused_rekey_superseded_by_a_later_rekey_of_its_spelling_in_the_batch_is_dropped_not_retried(
) {
    let (store, handle, hold) = a_refused_rekey_behind_a_hold().await;
    // The router's memory moved `a`'s cooldown under the first dated id, so this rekey carries no row of `a`.
    handle
        .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20251001", 1_600)
        .await
        .unwrap();
    hold.release();
    let later = [(
        "claude-sonnet-4-5".to_string(),
        "claude-sonnet-4-5-20251001".to_string(),
    )];
    wait_for(WRITTEN, || (resolutions(&store) == later).then_some(())).await;

    // With `a` back, a retry of the first rekey would land and put its dated id back.
    restore_a(&store);
    handle
        .report(model_cooldown("b", "model:claude-haiku-4-5", 1_000))
        .await;
    landed(&store, "model:claude-haiku-4-5").await;
    assert_eq!(
        resolutions(&store),
        later,
        "the superseded rekey was dropped, not retried"
    );
}

#[tokio::test]
async fn disconnecting_an_account_cancels_its_in_flight_relays_and_a_pause_does_not() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = first.result else {
        panic!()
    };
    assert!(!first.cancel.is_cancelled());
    store.set_enabled(&sel.account.id, false, "test").unwrap();
    handle.reload().await.unwrap();
    assert!(
        !first.cancel.is_cancelled(),
        "a pause governs routing only; the admitted request finishes"
    );
    let second = handle
        .select(request(&handle, Some("s1"), 1_001))
        .await
        .unwrap();
    assert!(
        matches!(second.result, SelectResult::Selected(ref s) if s.account.id != sel.account.id),
        "the paused account serves nothing new"
    );
    assert!(
        !second.cancel.is_cancelled(),
        "a fresh generation for the other account"
    );
    let SelectResult::Selected(other) = second.result else {
        panic!()
    };
    store
        .bump_lifecycle(&other.account.id, StateCause::Operator)
        .unwrap();
    handle.reload().await.unwrap();
    assert!(
        second.cancel.is_cancelled(),
        "a disconnect cancels the account's in-flight relays"
    );
    assert!(
        !first.cancel.is_cancelled(),
        "the other account's generation is untouched"
    );
    let third = handle
        .select(request(&handle, Some("s1"), 1_002))
        .await
        .unwrap();
    assert!(
        matches!(third.result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "one paused, one disconnected: only an operator can clear it"
    );
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|p| (p.session_id.as_str(), p.account_id.as_str()))
            .collect::<Vec<_>>(),
        [("s1", other.account.id.as_str())],
        "the pin stays on the disconnected account until it expires"
    );
    store.set_enabled(&sel.account.id, true, "test").unwrap();
    store.delete_account(&other.account.id, "test").unwrap();
    handle.reload().await.unwrap();
    let fourth = handle
        .select(request(&handle, Some("s2"), 1_003))
        .await
        .unwrap();
    assert!(
        matches!(fourth.result, SelectResult::Selected(ref s) if s.account.id == sel.account.id)
    );
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|p| (p.session_id.as_str(), p.account_id.as_str()))
            .collect::<Vec<_>>(),
        [("s2", sel.account.id.as_str())],
        "s1's pin left with its deleted account"
    );
}

#[tokio::test]
async fn a_key_is_touched_at_most_once_a_minute() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.touch_key("k", Presented::Live, None).await;
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)).then_some(())
    })
    .await;
    now.store(11_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Live, None).await;
    // Ten seconds on from the last touch, which is inside the minute, so nothing is written.
    stays_for(NOT_WRITTEN, || {
        store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)
    })
    .await;
    now.store(62_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Live, None).await;
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(62_000)).then_some(())
    })
    .await;
}

/// A retired presentation right after a live one still stamps the retired secret's last use: the two are
/// debounced apart, so the minute the live touch started does not swallow the retired one.
#[tokio::test]
async fn a_retired_presentation_inside_the_minute_after_a_live_one_is_still_written() {
    let store = seeded(&["a"]);
    let rotated_at = Some(500);
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("phoebis-next"),
            &key_prefix("phoebis-next"),
            500,
            "t",
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.touch_key("k", Presented::Live, rotated_at).await;
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)).then_some(())
    })
    .await;
    now.store(11_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Retired, rotated_at).await;
    wait_for(WRITTEN, || {
        (store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at
            == Some(11_000))
        .then_some(())
    })
    .await;
    now.store(21_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Retired, rotated_at).await;
    stays_for(NOT_WRITTEN, || {
        store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at
            == Some(11_000)
    })
    .await;
    // The key rotates again right away: the secret this presentation names is the newly retired one, a
    // different secret from the one the debounce slot above still remembers, so its first touch is not
    // swallowed by a grace period that belongs to the rotation before it.
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("phoebis-later"),
            &key_prefix("phoebis-later"),
            30_000,
            "t",
        )
        .unwrap();
    now.store(21_500, Ordering::SeqCst);
    handle
        .touch_key("k", Presented::Retired, Some(30_000))
        .await;
    wait_for(WRITTEN, || {
        (store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at
            == Some(21_500))
        .then_some(())
    })
    .await;
    // A touch that authenticated under the earlier rotation lands after a later one: the new grace stays
    // unpresented.
    now.store(90_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Retired, rotated_at).await;
    stays_for(NOT_WRITTEN, || {
        store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at
            == Some(21_500)
    })
    .await;
}

#[test]
fn a_usage_snapshot_becomes_a_sample_and_overage_evidence() {
    let window = |key: &str, utilization: f64| UsageWindow {
        key: key.into(),
        utilization: Some(utilization),
        resets_at: Some(9_000),
    };
    let snapshot = |five: f64, seven: f64, enabled: bool| UsageSnapshot {
        account_id: "a".into(),
        identity_generation: 1,
        fetched_at: 5_000,
        windows: vec![window("five_hour", five), window("seven_day", seven)],
        fable_window: None,
        extra_usage: Some(ExtraUsage {
            is_enabled: enabled,
            monthly_limit: None,
            used_credits: None,
            utilization: None,
            currency: None,
            decimal_places: None,
            disabled_reason: None,
        }),
        raw_keys: vec![],
        error: None,
        spend: None,
        fable_week_share: None,
    };
    let (sample, overage) = sample_from_usage(&snapshot(100.0, 40.0, true), 1);
    assert_eq!(
        (
            sample.reading(FIVE_HOUR).map(|r| r.utilization),
            sample.reading(SEVEN_DAY).map(|r| r.utilization),
            sample.observed_at,
            sample.poll,
        ),
        (Some(1.0), Some(0.4), 5_000, true)
    );
    assert!(overage
        .as_ref()
        .is_some_and(|o| o.in_use && o.reset == Some(9_000)));
    let both = UsageSnapshot {
        windows: vec![
            UsageWindow {
                key: "five_hour".into(),
                utilization: Some(60.0),
                resets_at: Some(9_000),
            },
            UsageWindow {
                key: "seven_day".into(),
                utilization: Some(100.0),
                resets_at: Some(90_000),
            },
        ],
        ..snapshot(0.0, 0.0, true)
    };
    assert_eq!(
        sample_from_usage(&both, 1).1.unwrap().reset,
        Some(90_000),
        "the reset is the latest among the exhausted windows, not the earliest of all"
    );
    let (distinct, _) = sample_from_usage(&both, 1);
    assert_eq!(
        (
            distinct.reading(FIVE_HOUR).and_then(|r| r.reset),
            distinct.reading(SEVEN_DAY).and_then(|r| r.reset),
        ),
        (Some(9_000), Some(90_000)),
        "each window's reset travels with its own utilization"
    );
    assert_eq!(
        sample.reading(SEVEN_DAY_FABLE),
        None,
        "an account without a fable window has no fable allowance to read"
    );
    let fable = UsageSnapshot {
        fable_window: Some(UsageWindow {
            key: "fable".into(),
            utilization: Some(75.0),
            resets_at: Some(80_000),
        }),
        ..snapshot(10.0, 20.0, true)
    };
    let (fable, _) = sample_from_usage(&fable, 1);
    assert_eq!(
        (
            fable.reading(SEVEN_DAY_FABLE).map(|r| r.utilization),
            fable.reading(SEVEN_DAY_FABLE).and_then(|r| r.reset),
        ),
        (Some(0.75), Some(80_000)),
        "and the fable window is a fraction like the shared ones, with its own reset"
    );
    assert!(
        sample_from_usage(&snapshot(100.0, 40.0, false), 1)
            .1
            .is_some_and(|o| !o.in_use),
        "without extra usage a full window is not overage"
    );
    assert!(sample_from_usage(&snapshot(12.5, 40.0, true), 1)
        .1
        .is_some_and(|o| !o.in_use));
}

#[tokio::test]
async fn a_cooldown_about_a_replaced_credential_is_dropped_and_the_pin_survives() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let answer = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    assert!(matches!(answer.result, SelectResult::Selected(_)));
    wait_for(WRITTEN, || {
        store.load_routing().unwrap().pins.first().cloned()
    })
    .await;
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k2"))
        .unwrap();
    handle.reload().await.unwrap();
    // A rejection that left the upstream while the first key was still in use.
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "permission".into(),
                window: "none".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin_of(&handle).await),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    stays_for(NOT_WRITTEN, || {
        store.load_routing().unwrap().cooldowns.is_empty()
    })
    .await;
    assert!(
        handle.cooldowns().await.unwrap().is_empty(),
        "the entitlements the rejection was about belong to a key the account no longer holds"
    );
    assert_eq!(
        handle.pins().await.unwrap().len(),
        1,
        "and the session it would have moved keeps its account"
    );
    // The same report against the key the account holds now is recorded.
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "permission".into(),
                window: "none".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 2,
            incarnation: 1,
        })
        .await;
    wait_for(WRITTEN, || store.load_routing().unwrap().cooldowns.pop()).await;
}

/// The pin the one session in these tests holds, as the relay would name it in a report.
async fn pin_of(handle: &phoebis_routing::RouterHandle) -> phoebis_routing::PinRef {
    let row = handle
        .pins()
        .await
        .unwrap()
        .pop()
        .expect("a pinned session");
    phoebis_routing::PinRef {
        key_id: row.key_id,
        session_id: row.session_id,
        agent_id: row.agent_id,
        model: row.model,
        epoch: row.epoch,
        lane: row.lane,
        entrypoint: None,
        effort: None,
    }
}

#[tokio::test]
async fn an_account_recreated_under_a_used_id_inherits_none_of_its_routing_state() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: 1_000 + 48 * HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "seven_day".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    wait_for(WRITTEN, || store.load_routing().unwrap().cooldowns.pop()).await;
    let answer = handle.select(request(&handle, None, 1_000)).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Exhausted(_)),
        "the cooldown is in force"
    );

    store.delete_account("a", "test").unwrap();
    handle.reload().await.unwrap();
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::ApiKey))
        .unwrap();
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".to_string(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();

    let answer = handle.select(request(&handle, None, 1_000)).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "a"),
        "the id was reused, and the days of cooldown its last holder earned did not come with it"
    );
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

/// The account the recreated-account test inserts, holding a secret at generation 1: under a used id it takes
/// the next incarnation while both generations read 1 again.
fn insert_account_a(store: &Store) {
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::ApiKey))
        .unwrap();
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".to_string(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
}

/// The seeded key again under its id, which stamps it a later creation time.
fn insert_key_k(store: &Store) {
    store
        .insert_api_key(
            &ApiKeyRow {
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
                companion_permissions: CompanionPermissions::default(),
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-reissued").to_vec(),
                prefix: key_prefix("phoebis-reissued"),
                incarnation: 1,
                created_at: 0,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
                feature_accounts: KeyFeatureAccounts::default(),
            },
            "test",
        )
        .unwrap();
}

fn cooldown_row(account: &str) -> CooldownRow {
    CooldownRow {
        account_id: account.into(),
        scope: CooldownScope::Account,
        until: 1_000 + 48 * HOUR,
        set_at: 0,
        reason: "rate_limit".into(),
        window: "seven_day".into(),
        source_request_id: None,
        incarnation: 1,
        spend_share: None,
    }
}

#[tokio::test]
async fn a_delayed_verdict_from_the_previous_holder_leaves_the_new_one_alone() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::FundingRequired,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
            cause: StateCause::FundingRefused,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: cooldown_row("a"),
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    // The ask is handled after a drain that holds both reports, so both are applied before this answers.
    assert!(handle.cooldowns().await.unwrap().is_empty());
    // Shutdown drains the writer, so a verdict that was going to land has landed by now.
    handle.shutdown().await.unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready,
        "both generations read 1 again, and the verdict was still about the holder that is gone"
    );
    assert!(store.load_routing().unwrap().cooldowns.is_empty());
}

/// The relay's credit-balance mark on `account_id`'s first holder.
fn funding_mark(account_id: &str) -> Report {
    Report::AccountState {
        account_id: account_id.into(),
        state: AccountState::FundingRequired,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
        cause: StateCause::FundingRefused,
        detail: None,
        dispatched_ordinal: None,
    }
}

/// The seeded pool over the one API-key account `a`, keyed at credential generation 1 so the mark's fenced write
/// lands.
fn keyed_a() -> Arc<Store> {
    let store = seeded(&["a"]);
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    store
}

/// Waits for the reload after the mark's write, so the snapshot a selection reads holds the mark.
async fn snapshot_marked(handle: &RouterHandle, account_id: &str) {
    wait_for(WRITTEN, || {
        handle
            .snapshot()
            .account(account_id)
            .filter(|a| a.state == AccountState::FundingRequired)
            .map(|_| ())
    })
    .await;
}

fn placed_on(answer: &phoebis_routing::SelectAnswer) -> Option<&str> {
    match &answer.result {
        SelectResult::Selected(selection) => Some(selection.account.id.as_str()),
        _ => None,
    }
}

#[tokio::test]
async fn a_landed_funding_mark_stamps_its_retry_the_configured_interval_after_the_landing() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        seeded(&["a"]),
        clock(&now),
        RouterConfig {
            funding_retry: Duration::from_secs(120),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    assert!(handle.funding_retries().await.unwrap().is_empty());
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 1_000 + 120_000)]
    );
}

#[tokio::test]
async fn a_funding_retry_holds_its_one_slot_until_the_claiming_attempt_s_ticket_drops() {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(funding_mark("a")).await;
    snapshot_marked(&handle, "a").await;
    assert_eq!(
        placed_on(
            &handle
                .select(request(&handle, None, 300_999))
                .await
                .unwrap()
        ),
        None,
        "the retry is not due yet"
    );
    now.store(301_000, Ordering::SeqCst);
    let first = handle
        .select(request(&handle, None, 301_000))
        .await
        .unwrap();
    assert_eq!(placed_on(&first), Some("a"), "the retry is due");
    // Past the relay's body read bound (60 s) and its upstream head bound (600 s): the claim lasts as long as
    // the attempt does, not as long as either bound.
    let later = 301_000 + 11 * 60_000;
    now.store(later, Ordering::SeqCst);
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, later)).await.unwrap()),
        None,
        "the first attempt still holds the one slot, on a pool whose cap is above one"
    );
    drop(first);
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, later)).await.unwrap()),
        Some("a"),
        "the first attempt ended, so the next request tries the account"
    );
}

/// A refusal and an older retry's success that coalesce before the loop drains them: the refusal is the newer
/// evidence, so the success clears nothing and the account waits out the retry interval from the refusal's mark.
#[tokio::test]
async fn a_funding_refusal_queued_beside_an_older_retry_s_success_keeps_the_account_held() {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(funding_mark("a")).await;
    snapshot_marked(&handle, "a").await;
    now.store(301_000, Ordering::SeqCst);
    let retry = handle
        .select(request(&handle, None, 301_000))
        .await
        .unwrap();
    assert_eq!(placed_on(&retry), Some("a"), "the retry is due");
    // The retry went out when it was due and answered 200, and a request after it was refused again. The refusal's
    // report reaches the queue first, the retry's success after it.
    let parked = handle.park_loop().await;
    handle.report(funding_mark("a")).await;
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::Ready,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
            cause: StateCause::FundingRetry,
            detail: None,
            dispatched_ordinal: retry.funding_ordinal,
        })
        .await;
    drop(parked);
    drop(retry);
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 301_000 + 300_000)],
        "the refusal's mark stands, stamped when the loop drained it"
    );
    handle.shutdown().await.unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::FundingRequired
    );
}

/// A reload that starts between a funding mark's stamp and the writer's commit reads the account `Ready`, as the
/// registry still says. The mark's write is not acknowledged yet, so the retry stays, and the five-minute wait the
/// commit leads to holds.
#[tokio::test]
async fn a_reload_between_a_funding_mark_s_stamp_and_its_commit_keeps_the_five_minute_wait() {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 301_000)]
    );
    now.store(2_000, Ordering::SeqCst);
    handle.reload().await.unwrap();
    assert_eq!(
        handle.snapshot().account("a").map(|a| a.state),
        Some(AccountState::Ready),
        "the load read the registry before the mark's commit"
    );
    hold.release();
    snapshot_marked(&handle, "a").await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 301_000)],
        "the retry outlived the load that predates the commit"
    );
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 2_000)).await.unwrap()),
        None,
        "the five-minute wait holds"
    );
}

/// A verdict formed under a key the operator then replaced, and the verdict under the new key: each stamps the
/// account's funding retry, and each write parks behind a hold of its own. The first write's fences refuse it, and
/// its acknowledgment names the superseded stamp, so a load that reads the account `Ready` before the newer write's
/// commit keeps the retry, and the newer write's own acknowledgment is the one that credits it.
#[tokio::test]
async fn a_superseded_verdicts_refused_write_leaves_the_newer_funding_mark_unacknowledged_until_its_own_write_lands(
) {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    // With the purge tick held, the first mark's write is the writer's first wake, so it parks on the first hold.
    let first = store.debug_hold_next_routing_write();
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            funding_retry: Duration::from_secs(120),
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle.report(funding_mark("a")).await;
    wait_for(WRITTEN, || first.is_parked().then_some(())).await;
    let second = store.debug_hold_next_routing_write();
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k2"))
        .unwrap();
    handle.reload().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::FundingRequired,
            lifecycle_generation: 1,
            credential_generation: 2,
            incarnation: 1,
            cause: StateCause::FundingRefused,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 2_000 + 120_000)],
        "the verdict under the new key stamped the retry again"
    );
    first.release();
    // The newer write's wake parks only once the writer has handed the loop the first write's acknowledgment.
    wait_for(WRITTEN, || second.is_parked().then_some(())).await;
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready,
        "the first write named the replaced key, so its fences refused it"
    );
    handle.reload().await.unwrap();
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 2_000 + 120_000)],
        "the refused write's acknowledgment names the superseded stamp, so the load before the newer commit drops \
         nothing"
    );
    second.release();
    snapshot_marked(&handle, "a").await;
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 2_000)).await.unwrap()),
        None,
        "the two-minute wait from the newer mark holds"
    );
    // The operator's Clear: the newer write's acknowledgment credited the retry, so a load at a later revision
    // drops it.
    store
        .set_state("a", AccountState::Ready, StateCause::Operator)
        .unwrap();
    handle.reload().await.unwrap();
    assert!(handle.funding_retries().await.unwrap().is_empty());
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 2_000)).await.unwrap()),
        Some("a"),
        "the cleared account is a plain candidate"
    );
}

/// A verdict about a key the account has since replaced, which the loop finds superseded when it handles it: it
/// stamps no retry and its write carries no stamp. A later mark's retry meets that write's acknowledgment, refused by
/// its fences, and stays unacknowledged until the later mark's own write is acknowledged.
#[tokio::test]
async fn a_verdict_superseded_at_handling_writes_no_stamp_and_its_refused_write_credits_no_funding_mark(
) {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    // With the purge tick held, the superseded verdict's write is the writer's first wake, so it parks on the first
    // hold.
    let first = store.debug_hold_next_routing_write();
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            funding_retry: Duration::from_secs(120),
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::FundingRequired,
            lifecycle_generation: 1,
            credential_generation: 0,
            incarnation: 1,
            cause: StateCause::FundingRefused,
            detail: None,
            dispatched_ordinal: None,
        })
        .await;
    wait_for(WRITTEN, || first.is_parked().then_some(())).await;
    assert!(
        handle.funding_retries().await.unwrap().is_empty(),
        "the verdict named a key the account has replaced, so it stamps nothing"
    );
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 1_000 + 120_000)]
    );
    let second = store.debug_hold_next_routing_write();
    first.release();
    // The mark's wake parks only once the writer has handed the loop the superseded write's acknowledgment.
    wait_for(WRITTEN, || second.is_parked().then_some(())).await;
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready,
        "the superseded verdict's fences refused its write"
    );
    handle.reload().await.unwrap();
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 1_000 + 120_000)],
        "the unstamped acknowledgment names no entry, so the load before the mark's commit drops nothing"
    );
    second.release();
    snapshot_marked(&handle, "a").await;
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 1_000)).await.unwrap()),
        None,
        "the two-minute wait from the mark holds"
    );
    // The operator's Clear: the mark's own acknowledgment credited the retry, so a load at a later revision drops
    // it.
    store
        .set_state("a", AccountState::Ready, StateCause::Operator)
        .unwrap();
    handle.reload().await.unwrap();
    assert!(handle.funding_retries().await.unwrap().is_empty());
}

/// A retry's success clears the account's retry while the mark's write still waits in the writer, the clock steps back
/// to the mark's instant, and a second mark stamps a new retry there. The first mark's acknowledgment names its own
/// entry, so it credits nothing, a load that reads the account `Ready` between the success's commit and the second
/// mark's keeps the second mark's retry, and the second mark's own acknowledgment is the one that credits it.
#[tokio::test]
async fn a_funding_marks_acknowledgment_credits_no_later_mark_stamped_at_the_same_instant_after_a_clock_stepped_back(
) {
    let store = keyed_a();
    // The registry holds the mark from before the start, so a selection claims the first mark's retry while that
    // mark's write still waits.
    store
        .set_state(
            "a",
            AccountState::FundingRequired,
            StateCause::FundingRefused,
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(10_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 310_000)]
    );
    now.store(310_000, Ordering::SeqCst);
    let retry = handle
        .select(request(&handle, None, 310_000))
        .await
        .unwrap();
    assert_eq!(placed_on(&retry), Some("a"), "the retry is due");
    assert_eq!(
        retry.funding_ordinal,
        Some(1),
        "the claim answers the first mark's ordinal"
    );
    now.store(310_500, Ordering::SeqCst);
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::Ready,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
            cause: StateCause::FundingRetry,
            detail: None,
            dispatched_ordinal: retry.funding_ordinal,
        })
        .await;
    drop(retry);
    assert!(
        handle.funding_retries().await.unwrap().is_empty(),
        "the success cleared the first mark's retry"
    );
    // The mark's and the success's writes share the wake after the purge's, which parks here before its commit.
    let between = store.debug_hold_next_routing_write();
    hold.release();
    wait_for(WRITTEN, || between.is_parked().then_some(())).await;
    now.store(10_000, Ordering::SeqCst);
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 310_000)],
        "the second mark's retry runs from the clock it read"
    );
    let last = store.debug_hold_next_routing_write();
    between.release();
    // The second mark's wake parks only once the writer has handed the loop the first wake's acknowledgment.
    wait_for(WRITTEN, || last.is_parked().then_some(())).await;
    handle.reload().await.unwrap();
    assert_eq!(
        handle.snapshot().account("a").map(|a| a.state),
        Some(AccountState::Ready),
        "the load read the success's commit before the second mark's"
    );
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 310_000)],
        "the first mark's acknowledgment names its own entry, so the load before the second mark's commit drops nothing"
    );
    last.release();
    snapshot_marked(&handle, "a").await;
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 10_000)).await.unwrap()),
        None,
        "the five-minute wait from the second mark holds"
    );
    now.store(310_000, Ordering::SeqCst);
    let second = handle
        .select(request(&handle, None, 310_000))
        .await
        .unwrap();
    assert_eq!(
        second.funding_ordinal,
        Some(2),
        "the second mark took the first one's ordinal plus one"
    );
    drop(second);
    // The operator's Clear: the second mark's own acknowledgment credited the retry, so a load at a later revision
    // drops it.
    store
        .set_state("a", AccountState::Ready, StateCause::Operator)
        .unwrap();
    handle.reload().await.unwrap();
    assert!(handle.funding_retries().await.unwrap().is_empty());
}

/// A mark whose write waits in the writer's backlog, then a verdict about the key the operator replaced since, which
/// the loop finds superseded and stamps nothing for: the later write takes the held one's row in the backlog and keeps
/// its stamp, so the refused write's acknowledgment still credits the mark's retry, and a load that reads the account
/// `Ready` drops it.
#[tokio::test]
async fn a_later_unstamped_funding_write_in_the_backlog_keeps_the_held_marks_stamp_so_no_stale_retry_stands(
) {
    let store = seeded(&["a", "b"]);
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let (handle, hold) = parked_writer(&store, &now).await;
    for index in 0..WRITER_DEPTH {
        fill(&handle, index).await;
    }
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 301_000)]
    );
    assert_eq!(
        handle.writer_status().deferred,
        1,
        "the mark's write waits in the backlog"
    );
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k2"))
        .unwrap();
    handle.reload().await.unwrap();
    let held = handle.writer_status().deferred;
    now.store(2_000, Ordering::SeqCst);
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 301_000)],
        "the verdict about the replaced key stamps nothing"
    );
    assert_eq!(
        handle.writer_status().deferred,
        held,
        "the later write takes the held one's row"
    );
    hold.release();
    // Both writes name the replaced key, so the one that lands is refused, and no load moves the account's revision.
    wait_for_async(DRAINED, || {
        let handle = handle.clone();
        async move {
            handle.reload().await.unwrap();
            handle
                .funding_retries()
                .await
                .unwrap()
                .is_empty()
                .then_some(())
        }
    })
    .await;
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
}

/// A retry the selection placed under one mark stays in flight while two later refusals mark the account again, the
/// clock stepped back between them, so the second one's instant falls behind the retry's dispatch. The retry was
/// dispatched under an older mark than the account's newest, so its success clears neither.
#[tokio::test]
async fn a_funding_retrys_success_dispatched_under_an_older_mark_clears_nothing_across_a_clock_stepped_back(
) {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(funding_mark("a")).await;
    snapshot_marked(&handle, "a").await;
    now.store(301_000, Ordering::SeqCst);
    let retry = handle
        .select(request(&handle, None, 301_000))
        .await
        .unwrap();
    assert_eq!(placed_on(&retry), Some("a"), "the retry is due");
    assert_eq!(retry.funding_ordinal, Some(1));
    now.store(302_000, Ordering::SeqCst);
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 302_000 + 300_000)]
    );
    now.store(200_000, Ordering::SeqCst);
    handle.report(funding_mark("a")).await;
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 200_000 + 300_000)],
        "the third mark's retry runs from the stepped-back clock"
    );
    handle
        .report(Report::AccountState {
            account_id: "a".into(),
            state: AccountState::Ready,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
            cause: StateCause::FundingRetry,
            detail: None,
            dispatched_ordinal: retry.funding_ordinal,
        })
        .await;
    drop(retry);
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 200_000 + 300_000)],
        "the success went out under the first mark, so the newest mark stands"
    );
    handle.shutdown().await.unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::FundingRequired
    );
}

#[tokio::test]
async fn after_a_restart_the_first_request_retries_a_marked_account_at_once_and_holds_the_second_off(
) {
    let store = seeded(&["a"]);
    store
        .set_state(
            "a",
            AccountState::FundingRequired,
            StateCause::FundingRefused,
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert!(
        handle.funding_retries().await.unwrap().is_empty(),
        "the store keeps no instant for the mark"
    );
    let first = handle.select(request(&handle, None, 1_000)).await.unwrap();
    assert_eq!(placed_on(&first), Some("a"));
    assert_eq!(
        handle.funding_retries().await.unwrap(),
        vec![("a".to_string(), 1_000)],
        "the claim stamps the request's instant"
    );
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 2_000)).await.unwrap()),
        None,
        "the second request finds the slot taken while the first lives"
    );
    drop(first);
    assert_eq!(
        placed_on(&handle.select(request(&handle, None, 2_000)).await.unwrap()),
        Some("a")
    );
}

#[tokio::test]
async fn a_reload_reading_the_account_ready_or_under_a_new_holder_drops_its_funding_retry() {
    let store = keyed_a();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(funding_mark("a")).await;
    snapshot_marked(&handle, "a").await;
    assert_eq!(handle.funding_retries().await.unwrap().len(), 1);
    now.store(2_000, Ordering::SeqCst);
    // The operator's Clear: a store write and a reload, never a report.
    store
        .set_state("a", AccountState::Ready, StateCause::Operator)
        .unwrap();
    handle.reload().await.unwrap();
    assert!(handle.funding_retries().await.unwrap().is_empty());

    handle.report(funding_mark("a")).await;
    snapshot_marked(&handle, "a").await;
    assert_eq!(handle.funding_retries().await.unwrap().len(), 1);
    now.store(3_000, Ordering::SeqCst);
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    store
        .set_state(
            "a",
            AccountState::FundingRequired,
            StateCause::FundingRefused,
        )
        .unwrap();
    handle.reload().await.unwrap();
    assert!(
        handle.funding_retries().await.unwrap().is_empty(),
        "the new holder's mark is its own, and its first request retries at once"
    );
}

#[tokio::test]
async fn a_pin_mark_from_the_previous_holder_leaves_the_new_holders_pin_alone() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();
    handle
        .select(request(&handle, Some("s"), 1_000))
        .await
        .unwrap();
    wait_for(WRITTEN, || {
        store.load_routing().unwrap().pins.first().cloned()
    })
    .await;
    // The mark names the live pin, epoch and all, so only the incarnation keeps it from ending the session.
    let pin = pin_of(&handle).await;
    let mark = |incarnation| Report::PinInvalidated {
        account_id: "a".into(),
        pin: pin.clone(),
        mark: None,
        incarnation,
        lifecycle_generation: 1,
        credential_generation: 1,
    };
    handle.report(mark(1)).await;
    assert_eq!(
        handle.pins().await.unwrap().len(),
        1,
        "the mark was about the holder the id had before"
    );
    stays_for(NOT_WRITTEN, || {
        !store.load_routing().unwrap().pins.is_empty()
    })
    .await;
    handle.report(mark(2)).await;
    assert!(
        handle.pins().await.unwrap().is_empty(),
        "the same mark from the current holder ends the pin"
    );
    wait_for(WRITTEN, || {
        store.load_routing().unwrap().pins.is_empty().then_some(())
    })
    .await;
}

#[tokio::test]
async fn one_reload_over_a_delete_and_a_recreate_keeps_none_of_the_previous_holders_state() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s"), 1_000))
        .await
        .unwrap();
    let first_epoch = match &first.result {
        SelectResult::Selected(s) => s.pin.as_ref().expect("a session pin").epoch,
        other => panic!("{other:?}"),
    };
    wait_for(WRITTEN, || {
        store.load_routing().unwrap().pins.first().cloned()
    })
    .await;
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    handle
        .report(Report::Overage {
            account_id: "a".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: Some(1_100),
                reset: None,
                observed_at: 1_100,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    handle
        .report(observed("a", sample_with(1_100, FIVE_HOUR, 1.0, None)))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    assert_eq!(handle.overages().await.unwrap().len(), 1);
    assert_eq!(handle.samples().await.unwrap().len(), 1);

    // A key deleted and created again under the same id, and an account delete and recreate under the same id,
    // all committed before the one reload.
    store.delete_api_key("k", "test").unwrap();
    insert_key_k(&store);
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();

    assert!(
        handle.pins().await.unwrap().is_empty(),
        "the pin named a key generation that is gone"
    );
    assert!(handle.cooldowns().await.unwrap().is_empty());
    assert!(handle.overages().await.unwrap().is_empty());
    assert!(handle.samples().await.unwrap().is_empty());
    assert!(
        first.cancel.is_cancelled(),
        "a stream on the previous holder ends with it"
    );
    let answer = handle
        .select(request(&handle, Some("s"), 2_000))
        .await
        .unwrap();
    let fresh = matches!(answer.result, SelectResult::Selected(ref s)
        if s.pin.as_ref().is_some_and(|p| p.epoch != first_epoch));
    assert!(
        fresh,
        "the new holder's first session request minted a fresh epoch"
    );
}

#[tokio::test]
async fn a_key_created_again_under_its_id_before_one_reload_inherits_no_pin() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .select(request(&handle, Some("s"), 1_000))
        .await
        .unwrap();
    assert_eq!(handle.pins().await.unwrap().len(), 1);
    let mut events = handle.subscribe_events();
    store.delete_api_key("k", "test").unwrap();
    insert_key_k(&store);
    handle.reload().await.unwrap();
    assert!(
        handle.pins().await.unwrap().is_empty(),
        "the account is the same one, but the session belonged to the key that is gone"
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { pin: PinKey { session_id, .. }, reasons, .. } if session_id == "s" && reasons == vec!["deleted"])
    );
    handle
        .select(request(&handle, Some("s"), 2_000))
        .await
        .unwrap();
    let (_, record) = next_decision(&mut events).await;
    assert_eq!(
        pin_events_of(&record),
        [("pin_created", None)],
        "the session's next request under the new key is placed afresh, with no move off the old key's pin"
    );
}

/// The pin events among `seen`, in order: the kind, the account the event names first (the one a move left) and the
/// list it carries.
fn pin_events_seen(
    seen: &[phoebis_routing::RouterEvent],
) -> Vec<(&'static str, &str, Vec<&'static str>)> {
    use phoebis_routing::RouterEvent;
    seen.iter()
        .filter_map(|event| match event {
            RouterEvent::PinCreated { account_id, .. } => {
                Some(("pin_created", account_id.as_str(), Vec::new()))
            }
            RouterEvent::PinExpired {
                account_id,
                reasons,
                ..
            } => Some(("pin_expired", account_id.as_str(), reasons.clone())),
            RouterEvent::PinMigrated {
                from_account,
                reasons,
                ..
            } => Some(("pin_migrated", from_account.as_str(), reasons.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_session_whose_account_was_deleted_lands_its_next_request_inside_the_ttl_as_a_move_under_deleted(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let to = if from == "a" { "b" } else { "a" };
    let mut events = handle.subscribe_events();
    store.delete_account(&from, "test").unwrap();
    handle.reload().await.unwrap();
    // The last instant of the pool's hour-long affinity TTL from the pin's last use, far past a failover's minute.
    let later = 1_000 + HOUR;
    now.store(later, Ordering::SeqCst);
    let answer = handle
        .select(request(&handle, Some("s1"), later))
        .await
        .unwrap();
    let moved = match &answer.result {
        SelectResult::Selected(s) => s.trace.moved,
        other => panic!("{other:?}"),
    };
    let seen = drained(&mut events).await;
    let (summary, record) = seen
        .iter()
        .find_map(|event| match event {
            RouterEvent::Decision {
                summary, record, ..
            } => Some((summary.clone(), record.clone())),
            _ => None,
        })
        .expect("the landing's decision");
    assert_eq!(
        (
            summary.rule,
            moved,
            record.outcome.rule,
            record.outcome.from_account_id.as_deref(),
            pin_events_of(&record)
        ),
        (
            Some("deleted"),
            Some("deleted"),
            Some("deleted"),
            Some(from.as_str()),
            vec![("pin_migrated", Some("deleted"))]
        ),
        "the landing moves the session off the deleted account"
    );
    assert_eq!(
        pin_events_seen(&seen),
        [("pin_migrated", from.as_str(), vec!["deleted"])],
        "neither the purge nor the landing writes an expiry or a fresh pin, and the move carries the purge's list"
    );
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|pin| (
                pin.session_id.as_str(),
                pin.account_id.as_str(),
                pin.shed.as_ref().map(|stamp| stamp.from.as_str())
            ))
            .collect::<Vec<_>>(),
        [("s1", to, None)],
        "the landed pin stamps no source, since the account the stamp would name is gone"
    );
}

#[tokio::test]
async fn a_pin_of_a_holder_created_again_under_its_id_before_one_reload_expires_and_its_session_is_placed_afresh(
) {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .select(request(&handle, Some("s"), 1_000))
        .await
        .unwrap();
    let mut events = handle.subscribe_events();
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();
    handle
        .select(request(&handle, Some("s"), 2_000))
        .await
        .unwrap();
    assert_eq!(
        pin_events_seen(&drained(&mut events).await),
        [
            ("pin_expired", "a", vec!["deleted"]),
            ("pin_created", "a", Vec::new())
        ],
        "the id names another account now, so the previous holder's pin expires and the session starts afresh"
    );
}

#[tokio::test]
async fn a_deleted_accounts_session_idle_past_the_ttl_expires_under_deleted_at_that_purge_and_is_placed_afresh(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let mut events = handle.subscribe_events();
    store.delete_account(&from, "test").unwrap();
    handle.reload().await.unwrap();
    // One millisecond past the pool's hour-long affinity TTL from the pin's last use, with no request between.
    let lapsed = 1_000 + HOUR + 1;
    now.store(lapsed, Ordering::SeqCst);
    handle.reload().await.unwrap();
    let expiries: Vec<_> = drained(&mut events)
        .await
        .into_iter()
        .filter_map(|event| match event {
            RouterEvent::PinExpired {
                account_id,
                reasons,
                pool_id,
                at,
                ..
            } => Some((account_id, reasons, pool_id, at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        expiries,
        [(from.clone(), vec!["deleted"], Some("p".to_string()), lapsed)],
        "the one expiry lands at the purge past the TTL, with the list and the pool the purge's own expiry carries"
    );
    handle
        .select(request(&handle, Some("s1"), lapsed))
        .await
        .unwrap();
    let (_, record) = next_decision(&mut events).await;
    assert_eq!(
        (
            record.outcome.from_account_id.as_deref(),
            record.outcome.rule,
            pin_events_of(&record)
        ),
        (None, None, vec![("pin_created", None)]),
        "the session's next request is a fresh placement that names no source"
    );
}

#[tokio::test]
async fn a_request_past_the_ttl_ahead_of_any_purge_ends_a_deleted_accounts_trace_with_its_expiry_and_places_afresh(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    // No purge tick runs, so the reload's purge is the one sweep ahead of the request.
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let mut events = handle.subscribe_events();
    store.delete_account(&from, "test").unwrap();
    handle.reload().await.unwrap();
    // One millisecond past the pool's hour-long affinity TTL from the pin's last use, with no purge since the reload.
    let lapsed = 1_000 + HOUR + 1;
    now.store(lapsed, Ordering::SeqCst);
    handle
        .select(request(&handle, Some("s1"), lapsed))
        .await
        .unwrap();
    let seen = drained(&mut events).await;
    let record = seen
        .iter()
        .find_map(|event| match event {
            RouterEvent::Decision { record, .. } => Some(record.clone()),
            _ => None,
        })
        .expect("the request's decision");
    assert_eq!(
        pin_events_of(&record),
        [("pin_expired", Some("deleted")), ("pin_created", None)],
        "the request that meets the lapsed trace writes its expiry ahead of the fresh pin"
    );
    let expiries: Vec<_> = seen
        .into_iter()
        .filter_map(|event| match event {
            RouterEvent::PinExpired {
                account_id,
                reasons,
                pool_id,
                ..
            } => Some((account_id, reasons, pool_id)),
            _ => None,
        })
        .collect();
    assert_eq!(
        expiries,
        [(from, vec!["deleted"], Some("p".to_string()))],
        "the one expiry carries the purge's list and the key's pool"
    );
}

#[tokio::test]
async fn an_account_created_again_under_a_deleted_accounts_id_ends_its_trace_with_an_expiry_and_a_fresh_placement(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .select(request(&handle, Some("s"), 1_000))
        .await
        .unwrap();
    let mut events = handle.subscribe_events();
    store.delete_account("a", "test").unwrap();
    handle.reload().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    insert_account_a(&store);
    handle.reload().await.unwrap();
    let expiries: Vec<_> = drained(&mut events)
        .await
        .into_iter()
        .filter_map(|event| match event {
            RouterEvent::PinExpired {
                account_id,
                reasons,
                at,
                ..
            } => Some((account_id, reasons, at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        expiries,
        [("a".to_string(), vec!["deleted"], 2_000)],
        "the reload that finds the id held again ends the trace with one expiry"
    );
    handle
        .select(request(&handle, Some("s"), 3_000))
        .await
        .unwrap();
    let (_, record) = next_decision(&mut events).await;
    assert_eq!(
        pin_events_of(&record),
        [("pin_created", None)],
        "the new holder's session starts afresh, with no move off the id it holds"
    );
}

#[tokio::test]
async fn a_key_created_again_under_its_id_ends_a_deleted_accounts_trace_with_an_expiry_and_a_fresh_placement(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let mut events = handle.subscribe_events();
    store.delete_account(&from, "test").unwrap();
    handle.reload().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    store.delete_api_key("k", "test").unwrap();
    insert_key_k(&store);
    handle.reload().await.unwrap();
    let expiries: Vec<_> = drained(&mut events)
        .await
        .into_iter()
        .filter_map(|event| match event {
            RouterEvent::PinExpired {
                account_id,
                reasons,
                pool_id,
                at,
                ..
            } => Some((account_id, reasons, pool_id, at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        expiries,
        [(from, vec!["deleted"], Some("p".to_string()), 2_000)],
        "the reload that finds the key replaced ends the trace with one expiry"
    );
    handle
        .select(request(&handle, Some("s1"), 3_000))
        .await
        .unwrap();
    let (_, record) = next_decision(&mut events).await;
    assert_eq!(
        pin_events_of(&record),
        [("pin_created", None)],
        "the session under the new key is placed afresh, with no move off the deleted account"
    );
}

#[tokio::test]
async fn a_failovers_entry_past_its_minute_hands_nothing_on_inside_the_pools_ttl_and_leaves_in_silence(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    // No purge tick runs, so the request is the one reader that meets the entry.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let SelectResult::Selected(selection) = first.result else {
        panic!("{:?}", first.result)
    };
    let from = selection.account.id;
    handle
        .report(Report::Cooldown {
            account_id: from.clone(),
            row: CooldownRow {
                account_id: from,
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(selection.pin.expect("a session pin")),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    // The ask queues behind the report, so the entry takes its instant before the clock moves.
    handle.pins().await.unwrap();
    // One millisecond past a failover's one-minute window, inside the pool's hour-long affinity TTL.
    let later = 1_000 + 60_000 + 1;
    now.store(later, Ordering::SeqCst);
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), later))
        .await
        .unwrap();
    let record = drained(&mut events)
        .await
        .into_iter()
        .find_map(|event| match event {
            RouterEvent::Decision { record, .. } => Some(record),
            _ => None,
        })
        .expect("the request's decision");
    assert_eq!(
        pin_events_of(&record),
        [("pin_created", None)],
        "the entry past its minute hands nothing on and leaves with no expiry"
    );
}

#[tokio::test]
async fn a_reading_reported_for_a_previous_incarnation_changes_nothing() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();
    // The answers of a poll that left under the first holder and landed after the reload.
    handle
        .report(Report::Sample {
            account_id: "a".into(),
            sample: as_poll(sample_with(1_500, FIVE_HOUR, 1.0, None)),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    handle
        .report(Report::Overage {
            account_id: "a".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: None,
                reset: None,
                observed_at: 1_500,
                source: phoebis_routing::EvidenceSource::Sample,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    assert!(
        handle.samples().await.unwrap().is_empty(),
        "the old holder's sample was refused"
    );
    assert!(
        handle.overages().await.unwrap().is_empty(),
        "and so was its overage reading"
    );
    handle.shutdown().await.unwrap();
    let load = store.load_routing().unwrap();
    assert!(
        load.overage.is_empty() && load.windows.is_empty(),
        "and nothing reached the store"
    );
}

fn window_row(account: &str, window: &str, utilization: f64, reset: Option<i64>) -> WindowRow {
    WindowRow {
        account_id: account.into(),
        incarnation: 1,
        observed_at: 500,
        plan: None,
        reset,
        tier: None,
        utilization,
        window: window.into(),
    }
}

/// Prices a million input tokens at a tenth of either window, whatever the tier, the model or the instant.
fn a_tenth_per_million() -> SharePricer {
    SharePricer::new(|_, _, _, _, usage, _| Some(usage.input as f64 / 1_000_000.0 * 0.1))
}

/// The rate-limit tier `with_tier` puts an account on, which a pricer that prices one tier keys on.
const TIER: &str = "default_claude_max_5x";

fn with_tier(store: &Store, account: &str) {
    store
        .set_profile(
            account,
            "{}",
            &SubscriptionRow {
                tier: Some(TIER.into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
}

fn a_million_input(account: &str, at: i64) -> Report {
    a_million_input_served(account, at, false)
}

/// `a_million_input` whose answer reported `usage.speed` `fast` when `fast` is set.
fn a_million_input_served(account: &str, at: i64, fast: bool) -> Report {
    Report::Usage {
        account_id: account.into(),
        at,
        canonical_model: "claude-sonnet-5".into(),
        incarnation: 1,
        usage: PinUsage {
            input: 1_000_000,
            ..PinUsage::default()
        },
        fast,
        judged: None,
        request_id: "r-1".into(),
        attempt: 1,
        entrypoint: None,
    }
}

async fn read(handle: &RouterHandle, account: &str, window: &str) -> Option<f64> {
    handle
        .samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == account)
        .and_then(|(_, sample)| sample.reading(window))
        .map(|r| r.utilization)
}

fn close(read: Option<f64>, expected: f64) -> bool {
    read.is_some_and(|r| (r - expected).abs() < 1e-9)
}

#[tokio::test]
async fn a_restart_loads_the_persisted_windows_and_the_first_selection_ranks_by_them() {
    let store = seeded_oauth(&["a", "b"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.strategy = Strategy::Pace;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    let reset = Some(1_000 + 3 * HOUR);
    for (account, utilization) in [("a", 0.9), ("b", 0.1)] {
        for window in [FIVE_HOUR, SEVEN_DAY] {
            assert!(store
                .upsert_window(&window_row(account, window, utilization, reset))
                .unwrap());
        }
    }
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    for (account, utilization) in [("a", 0.9), ("b", 0.1)] {
        for window in [FIVE_HOUR, SEVEN_DAY] {
            assert_eq!(
                read(&handle, account, window).await,
                Some(utilization),
                "{account} reads its persisted {window} before any head or report"
            );
        }
    }
    match handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap()
        .result
    {
        SelectResult::Selected(selection) => assert_eq!(
            selection.account.id, "b",
            "the first selection ranks by the loaded windows"
        ),
        other => panic!("expected a selection, got {other:?}"),
    }
}

#[tokio::test]
async fn a_fable_completions_hour_reaches_the_reservation_of_the_next_starts_non_fable_pick() {
    use phoebis_routing::FactValue;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.strategy = Strategy::Pace;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    // The start of clock hour 100, with a tenth of the week and half the Fable week read, both resetting a day out.
    let at = 100 * HOUR;
    for (window, utilization) in [(SEVEN_DAY, 0.1), (SEVEN_DAY_FABLE, 0.5)] {
        assert!(store
            .upsert_window(&window_row("a", window, utilization, Some(at + 24 * HOUR)))
            .unwrap());
    }
    // Two hundredths of the Fable week per million input tokens, no other window, and a factor of a half.
    let config = || RouterConfig {
        share: SharePricer::new(|window, _, _, _, usage, _| {
            (window == SEVEN_DAY_FABLE).then(|| usage.input as f64 / 1_000_000.0 * 0.02)
        })
        .with_week_factor(|window, _| (window == SEVEN_DAY_FABLE).then_some(0.5)),
        ..RouterConfig::default()
    };
    let now = Arc::new(AtomicI64::new(at));
    let first = spawn(store.clone(), clock(&now), config()).unwrap();
    first
        .report(Report::Usage {
            account_id: "a".into(),
            at: at - HOUR / 2,
            canonical_model: "claude-fable-5-1".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 1_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-0".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    first.shutdown().await.unwrap();
    let second = spawn(store.clone(), clock(&now), config()).unwrap();
    let mut events = second.subscribe_events();
    second.select(request(&second, None, at)).await.unwrap();
    let record = next_record(&mut events).await;
    let reservation = record
        .walk
        .iter()
        .find(|step| step.step == "pick")
        .and_then(|step| {
            step.facts
                .iter()
                .find(|f| f.key == "fable_reservation" && f.account_id.as_deref() == Some("a"))
        })
        .map(|f| f.value.clone());
    // Hour 99 holds the completion's 0.02: 0.48 over the 24 hours, under the Fable room of 0.5, at a half 0.24.
    assert!(
        matches!(reservation, Some(FactValue::Number(n)) if (n - 0.24).abs() < 1e-9),
        "the second start reads the hour the first wrote: {reservation:?}"
    );
}

#[tokio::test]
async fn a_completed_requests_share_is_added_to_the_reading() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(3_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(10 * HOUR)),
        ))
        .await;
    handle.report(a_million_input("a", 2_000)).await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "the reading plus the request's share"
    );
}

#[tokio::test]
async fn inside_one_reset_a_head_keeps_the_routers_own_figure_within_its_percent_and_a_poll_lands_as_read(
) {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reset = Some(10 * HOUR);
    // Each ask lands the reports ahead of it, since a pending head and a pending poll fold in no order of their own.
    handle
        .report(observed(
            "a",
            as_poll(sample_with(1_000, FIVE_HOUR, 0.50, reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.49, reset)))
        .await;
    let figure = read(&handle, "a", FIVE_HOUR).await;
    assert!(
        close(figure, 0.50),
        "the poll's 0.50 lies inside the head's percent from 0.49, so it stands, not {figure:?}"
    );

    // 40,000 input tokens at a tenth of the window per million.
    now.store(2_500, Ordering::SeqCst);
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 2_500,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 40_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    handle.samples().await.unwrap();
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(3_000, FIVE_HOUR, 0.50, reset)))
        .await;
    let figure = read(&handle, "a", FIVE_HOUR).await;
    assert!(
        close(figure, 0.504),
        "the stored 0.50 plus the charge since lies inside the head's percent from 0.50, not {figure:?}"
    );

    now.store(4_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(4_000, FIVE_HOUR, 0.47, reset)))
        .await;
    let figure = read(&handle, "a", FIVE_HOUR).await;
    assert!(
        close(figure, 0.48),
        "the router's 0.504 lies over the head's percent from 0.47, so its nearer edge stands, not {figure:?}"
    );

    now.store(5_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(5_000, FIVE_HOUR, 0.40, reset)),
        ))
        .await;
    let figure = read(&handle, "a", FIVE_HOUR).await;
    assert!(
        close(figure, 0.40),
        "a poll under the same reset lands as read, not {figure:?}"
    );
}

#[tokio::test]
async fn a_heads_bucket_reading_lands_as_read_under_one_reset() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let reset = Some(60_000);
    handle
        .report(observed("a", sample_with(1_000, "itpm:x", 0.50, reset)))
        .await;
    handle.samples().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(2_000, "itpm:x", 0.30, reset)))
        .await;
    let figure = read(&handle, "a", "itpm:x").await;
    assert!(
        close(figure, 0.30),
        "a bucket's figure is a ratio of counts and no truncated percent, not {figure:?}"
    );
}

#[tokio::test]
async fn a_fast_completion_charges_no_window() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(3_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(10 * HOUR)),
        ))
        .await;
    handle
        .report(a_million_input_served("a", 2_000, false))
        .await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "a standard completion adds its share to the reading"
    );
    handle
        .report(a_million_input_served("a", 2_500, true))
        .await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "a fast completion draws usage credits and leaves the window where the standard one left it"
    );
}

#[tokio::test]
async fn a_classifier_completion_charges_no_window_and_releases_its_booking() {
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, _events) = priced_router(&now).await;
    // A booking under the request's id, as a selection that books leaves one: the usage releases it whatever the
    // request was.
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.52),
        "the booking's rewrite joins the reading"
    );
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 2_500,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 1_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: Some(Judged {
                charge: JudgedCharge::On,
                pin: None,
            }),
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    let reading = read(&handle, "a", FIVE_HOUR).await;
    assert!(
        close(reading, 0.5),
        "the classifier call's usage releases the booking and charges no plan window: {reading:?}"
    );
}

#[tokio::test]
async fn a_usage_report_charges_each_window_the_pricer_prices_and_the_fable_week_for_a_fable_model()
{
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(3_000));
    // A tenth of a shared window per million input tokens, and twice that of the Fable week, which is half the
    // 7-day window. A window the pricer cannot price keeps its reading while the others charge: the first
    // unpriced model has no 7-day cell and prices on the 5-hour window alone, the second prices on neither.
    let share = SharePricer::new(|window, _, _, model, usage, _| {
        let tenth = usage.input as f64 / 1_000_000.0 * 0.1;
        match (window, model) {
            (SEVEN_DAY, "claude-unpriced-1") => None,
            (FIVE_HOUR | SEVEN_DAY, "claude-unpriced-2") => None,
            (SEVEN_DAY_FABLE, _) => Some(2.0 * tenth),
            _ => Some(tenth),
        }
    });
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let readings = |incarnation: u64| {
        let reading = WindowReading {
            utilization: 0.5,
            reset: Some(10 * HOUR),
            observed_at: 1_000,
            source: WindowSource::Head,
            plan: None,
            tier: None,
        };
        Report::Sample {
            account_id: "a".into(),
            sample: sample(1_000)
                .with(FIVE_HOUR, reading.clone())
                .with(SEVEN_DAY, reading.clone())
                .with(SEVEN_DAY_FABLE, reading),
            incarnation,
            source_request_id: None,
        }
    };
    let used = |model: &str, incarnation: u64| Report::Usage {
        account_id: "a".into(),
        at: 2_000,
        canonical_model: model.into(),
        incarnation,
        usage: PinUsage {
            input: 1_000_000,
            ..PinUsage::default()
        },
        fast: false,
        judged: None,
        request_id: "r-1".into(),
        attempt: 1,
        entrypoint: None,
    };
    let windows = || async {
        let mut read_all = Vec::new();
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            read_all.push(read(&handle, "a", window).await);
        }
        read_all
    };
    let reads = |read_all: Vec<Option<f64>>, expected: [f64; 3]| {
        read_all
            .iter()
            .zip(expected)
            .all(|(read, expected)| close(*read, expected))
    };
    handle.report(readings(1)).await;

    handle.report(used("claude-fable-5", 1)).await;
    let fable = windows().await;
    assert!(
        reads(fable.clone(), [0.6, 0.6, 0.7]),
        "a Fable request charges both shared windows and its week at twice the 7-day share: {fable:?}"
    );
    handle.report(used("claude-sonnet-5", 1)).await;
    let sonnet = windows().await;
    assert!(
        reads(sonnet.clone(), [0.7, 0.7, 0.7]),
        "any other model leaves the Fable week alone: {sonnet:?}"
    );
    handle.report(used("claude-unpriced-1", 1)).await;
    let five_hour_alone = windows().await;
    assert!(
        reads(five_hour_alone.clone(), [0.8, 0.7, 0.7]),
        "a model priced on the 5-hour window alone charges that window and leaves the 7-day one: {five_hour_alone:?}"
    );
    handle.report(used("claude-unpriced-2", 1)).await;
    let neither = windows().await;
    assert!(
        reads(neither.clone(), [0.8, 0.7, 0.7]),
        "a model priced on neither window charges nothing: {neither:?}"
    );

    // The id held again under a new incarnation, with its own readings: a report from the old holder is late.
    store.delete_account("a", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    with_tier(&store, "a");
    handle.reload().await.unwrap();
    handle.report(readings(2)).await;
    assert!(
        reads(windows().await, [0.5, 0.5, 0.5]),
        "the new holder reads its own readings"
    );
    handle.report(used("claude-sonnet-5", 1)).await;
    let moved_past = windows().await;
    assert!(
        reads(moved_past.clone(), [0.5, 0.5, 0.5]),
        "a report for the incarnation the account moved past charges nothing: {moved_past:?}"
    );
}

#[tokio::test]
async fn a_usage_report_charges_no_five_hour_share_on_an_account_whose_set_lacks_it() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(3_000));
    // A tenth of a shared window per million input tokens, and twice that of the Fable week.
    let share = SharePricer::new(|window, _, _, _, usage, _| {
        let tenth = usage.input as f64 / 1_000_000.0 * 0.1;
        Some(if window == SEVEN_DAY_FABLE {
            2.0 * tenth
        } else {
            tenth
        })
    });
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reading = |observed_at: i64| WindowReading {
        utilization: 0.5,
        reset: Some(10 * HOUR),
        observed_at,
        source: WindowSource::Poll,
        plan: None,
        tier: None,
    };
    let poll = |observed_at: i64, windows: &[&str], read_at: i64| {
        observed(
            "a",
            windows.iter().fold(
                UtilizationSample {
                    poll: true,
                    ..sample(observed_at)
                },
                |sample, window| sample.with(window, reading(read_at)),
            ),
        )
    };
    handle
        .report(poll(1_000, &[SEVEN_DAY, SEVEN_DAY_FABLE], 1_000))
        .await;
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 2_000,
            canonical_model: "claude-fable-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 1_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    // Read before the next poll is reported, since a pending poll drains ahead of a control fact queued before it.
    let charged = read(&handle, "a", SEVEN_DAY).await;
    assert!(
        close(charged, 0.6),
        "the 7-day window takes its charge beside the 5-hour window the set lacks: {charged:?}"
    );
    // A later poll puts the 5-hour window back with readings taken before the usage, so each keeps any charge the
    // usage booked on its window and the prediction shows it.
    handle
        .report(poll(3_000, &[FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE], 1_500))
        .await;
    let five_hour = read(&handle, "a", FIVE_HOUR).await;
    assert!(
        close(five_hour, 0.5),
        "the account had no 5-hour window when the usage landed, so it booked no 5-hour share: {five_hour:?}"
    );
}

#[tokio::test]
async fn a_head_writes_the_windows_it_moved_and_a_restart_reads_them() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let reading = |utilization: f64| WindowReading {
        utilization,
        reset: Some(100 * HOUR),
        observed_at: 1_000,
        source: WindowSource::Head,
        plan: None,
        tier: None,
    };
    handle
        .report(observed(
            "a",
            sample(1_000)
                .with(FIVE_HOUR, reading(0.2))
                .with(SEVEN_DAY, reading(0.3))
                .with(SEVEN_DAY_FABLE, reading(0.4)),
        ))
        .await;
    let rows = wait_for(WRITTEN, || {
        let windows = store.load_routing().unwrap().windows;
        (windows.len() == 3).then_some(windows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.window.as_str(),
                r.utilization,
                r.observed_at,
                r.incarnation
            ))
            .collect::<Vec<_>>(),
        vec![
            (FIVE_HOUR, 0.2, 1_000, 1),
            (SEVEN_DAY, 0.3, 1_000, 1),
            (SEVEN_DAY_FABLE, 0.4, 1_000, 1)
        ]
    );
    handle
        .report(observed(
            "a",
            sample(900).with(
                FIVE_HOUR,
                WindowReading {
                    observed_at: 900,
                    ..reading(0.9)
                },
            ),
        ))
        .await;
    handle.shutdown().await.unwrap();
    assert_eq!(
        store.load_routing().unwrap().windows,
        rows,
        "an older head writes nothing"
    );
    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    for (window, utilization) in [(FIVE_HOUR, 0.2), (SEVEN_DAY, 0.3), (SEVEN_DAY_FABLE, 0.4)] {
        assert_eq!(
            read(&again, "a", window).await,
            Some(utilization),
            "a restart reads {window} back"
        );
    }
}

/// A clean poll's report of `a` at `observed_at` reading each of `windows` at 0.2.
fn polled(observed_at: i64, windows: &[&str]) -> Report {
    observed(
        "a",
        windows.iter().fold(
            UtilizationSample {
                poll: true,
                ..sample(observed_at)
            },
            |sample, window| {
                sample.with(
                    window,
                    WindowReading {
                        utilization: 0.2,
                        reset: None,
                        observed_at,
                        source: WindowSource::Poll,
                        plan: None,
                        tier: None,
                    },
                )
            },
        ),
    )
}

fn set_row(windows: &str, observed_at: i64) -> WindowSetRow {
    WindowSetRow {
        account_id: "a".into(),
        incarnation: 1,
        observed_at,
        windows: windows.into(),
    }
}

#[tokio::test]
async fn a_clean_poll_without_five_hour_writes_the_drop_and_the_set_and_a_restart_restores_them() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(polled(1_000, &[FIVE_HOUR, SEVEN_DAY])).await;
    wait_for(WRITTEN, || {
        (store.load_routing().unwrap().windows.len() == 2).then_some(())
    })
    .await;
    handle.report(polled(2_000, &[SEVEN_DAY])).await;
    let load = wait_for(WRITTEN, || {
        let load = store.load_routing().unwrap();
        (load.windows.len() == 1 && !load.window_sets.is_empty()).then_some(load)
    })
    .await;
    assert_eq!(
        load.windows
            .iter()
            .map(|row| (row.window.as_str(), row.observed_at))
            .collect::<Vec<_>>(),
        [(SEVEN_DAY, 2_000)],
        "the dropped 5-hour row left the store"
    );
    assert_eq!(load.window_sets, [set_row(SEVEN_DAY, 2_000)]);
    handle.shutdown().await.unwrap();

    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let (_, sample) = again
        .reported_samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == "a")
        .expect("the restart loads a's sample");
    assert_eq!(
        sample.polled,
        Some(std::collections::BTreeSet::from([SEVEN_DAY.to_string()]))
    );
    assert_eq!(
        sample.polled_at,
        Some(2_000),
        "the restart reads the set before the process's first poll"
    );
}

#[tokio::test]
async fn a_restored_seven_day_reading_at_one_outside_the_set_reads_spent_beside_a_fitting_five_hour_window(
) {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    // The 7-day row outlived the set row, which names the 5-hour window alone, so the restart restores both.
    assert!(store
        .upsert_window(&window_row("a", FIVE_HOUR, 0.25, Some(1_000 + 2 * HOUR)))
        .unwrap());
    assert!(store
        .upsert_window(&window_row("a", SEVEN_DAY, 1.0, Some(1_000 + 48 * HOUR)))
        .unwrap());
    assert!(store.upsert_window_set(&set_row(FIVE_HOUR, 500)).unwrap());
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let a = projection_of_account(&handle.projections("p").await.unwrap(), "a");
    let base = &a.regime_fits[0].windows;
    assert_eq!(
        (base.get(FIVE_HOUR).copied(), base.get(SEVEN_DAY).copied()),
        (Some("fits"), Some("spent")),
        "{a:?}"
    );
}

#[tokio::test]
async fn an_api_key_account_and_an_account_without_a_tier_are_charged_nothing() {
    let store = seeded_oauth(&["a", "n"]);
    store
        .insert_account(&AccountRow::for_tests("k", AccountKind::ApiKey))
        .unwrap();
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(3_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    for account in ["k", "n", "a"] {
        handle
            .report(observed(
                account,
                sample_with(1_000, FIVE_HOUR, 0.5, Some(10 * HOUR)),
            ))
            .await;
        handle.report(a_million_input(account, 2_000)).await;
    }
    assert!(
        close(read(&handle, "k", FIVE_HOUR).await, 0.5),
        "an api_key account"
    );
    assert!(
        close(read(&handle, "n", FIVE_HOUR).await, 0.5),
        "an account whose snapshot carries no tier"
    );
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "the pricer was asked for the account with a tier alone"
    );
}

#[tokio::test]
async fn a_windows_write_queued_for_a_deleted_holder_lands_nowhere_and_its_replacement_restarts_with_none(
) {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.7, Some(5 * HOUR)),
        ))
        .await;
    let rows = wait_for(WRITTEN, || {
        let windows = store.load_routing().unwrap().windows;
        (!windows.is_empty()).then_some(windows)
    })
    .await;
    handle.shutdown().await.unwrap();
    store.delete_account("a", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    assert_eq!(store.account("a").unwrap().unwrap().incarnation, 2);
    assert!(
        !store.upsert_window(&rows[0]).unwrap(),
        "the old holder's delayed write lands nowhere"
    );
    assert!(store.load_routing().unwrap().windows.is_empty());
    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert!(
        again
            .samples()
            .await
            .unwrap()
            .iter()
            .all(|(id, _)| id != "a"),
        "the replacement starts with no window"
    );
}

async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> phoebis_routing::RouterEvent {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("an event")
        .expect("not lagged")
}

/// The next event, which has to be a decision: its summary and its record.
async fn next_decision(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> (DecisionSummary, DecisionRecord) {
    match next_event(events).await {
        phoebis_routing::RouterEvent::Decision {
            summary, record, ..
        } => (Arc::unwrap_or_clone(summary), Arc::unwrap_or_clone(record)),
        other => panic!("the decision comes first, not {other:?}"),
    }
}

/// A decision's pin events as the task emits them after it, `(kind, reason)` in order.
fn pin_events_of(record: &DecisionRecord) -> Vec<(&'static str, Option<&str>)> {
    record
        .outcome
        .pin_events
        .iter()
        .map(|event| (event.kind, event.reason.as_deref()))
        .collect()
}

#[tokio::test]
async fn a_selection_emits_its_decision_before_its_pin_event() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(summary.kind, "placed");
    assert_eq!(pin_events_of(&record), [("pin_created", None)]);
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinCreated { pin: PinKey { session_id, .. }, account_id, .. } if session_id == "s1" && account_id == "a"),
        "the pin event the record names follows the decision"
    );
}

#[tokio::test]
async fn an_expired_idle_pin_the_selection_replaces_emits_the_decision_then_both_pin_events() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let mut events = handle.subscribe_events();
    // One millisecond past the pool's hour-long affinity TTL.
    handle
        .select(request(&handle, Some("s1"), 1_000 + HOUR + 1))
        .await
        .unwrap();
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(summary.kind, "placed");
    assert_eq!(
        record
            .pin
            .as_ref()
            .map(|pin| (pin.found, pin.released.clone())),
        Some(("expired_idle", vec!["expired"])),
        "the reasons the task read before the selection join the pin block"
    );
    assert_eq!(
        pin_events_of(&record),
        [("pin_expired", Some("expired")), ("pin_created", None)]
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { pin: PinKey { session_id, .. }, reasons, .. } if session_id == "s1" && reasons == vec!["expired"])
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinCreated { pin: PinKey { session_id, .. }, account_id, .. } if session_id == "s1" && account_id == "a")
    );
}

#[tokio::test]
async fn a_selection_s_pin_event_and_the_purge_s_expiry_name_the_pool_the_key_runs_in() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        seeded(&["a"]),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    next_decision(&mut events).await;
    let created = next_event(&mut events).await;
    assert!(
        matches!(&created, RouterEvent::PinCreated { pool_id, .. } if pool_id.as_deref() == Some("p")),
        "the selection's pin names its request's pool: {created:?}"
    );
    // One millisecond past the pool's hour-long affinity TTL while the key stands, so the purge ends the pin as idle.
    now.store(1_000 + HOUR + 1, Ordering::SeqCst);
    let expired = next_event(&mut events).await;
    assert!(
        matches!(&expired, RouterEvent::PinExpired { pool_id, reasons, .. } if pool_id.as_deref() == Some("p") && *reasons == vec!["expired"]),
        "the purge's expiry names the pool the snapshot holds for the key: {expired:?}"
    );
}

#[tokio::test]
async fn a_pin_whose_account_left_the_pool_moves_to_the_member_under_unserved() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let to = if from == "a" { "b" } else { "a" };
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: to.into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(summary.kind, "placed");
    assert_eq!(record.pin.as_ref().map(|pin| pin.found), Some("left_pool"));
    assert_eq!(
        pin_events_of(&record),
        [("pin_migrated", Some("unserved"))],
        "the live pin moves under what the selection found against its account"
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinMigrated { from_account, to_account, reason, reasons, .. } if from_account == from && to_account == to && reason == "unserved" && reasons == vec!["unserved"])
    );
}

#[tokio::test]
async fn a_pin_on_a_paused_account_moves_to_the_sibling_under_paused_and_a_replaced_pin_on_the_same_account_names_none(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    // No purge tick runs, so the row past its TTL below stays for the selection to replace.
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let to = if from == "a" { "b" } else { "a" };
    store.set_enabled(&from, false, "test").unwrap();
    handle.reload().await.unwrap();
    let mut events = handle.subscribe_events();
    let answer = handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let moved = match &answer.result {
        SelectResult::Selected(s) => s.trace.moved,
        other => panic!("{other:?}"),
    };
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(
        (
            summary.kind,
            summary.rule,
            moved,
            record.outcome.account_id.as_deref(),
            record.outcome.rule,
            pin_events_of(&record)
        ),
        (
            "placed",
            Some("paused"),
            Some("paused"),
            Some(to),
            Some("paused"),
            vec![("pin_migrated", Some("paused"))]
        ),
        "the selection moved the pin the pause left, with no expiry and no fresh pin"
    );
    assert_eq!(
        record.outcome.from_account_id.as_deref(),
        Some(from.as_str()),
        "the move names the paused account the pin left"
    );
    assert_eq!(
        record
            .candidates
            .iter()
            .find(|member| member.account_id == from)
            .map(|member| member.verdict),
        Some("source"),
        "the paused account the pin left reads source, as the outcome names it"
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinMigrated { from_account, to_account, reason, reasons, cause: None, .. } if from_account == from && to_account == to && reason == "paused" && reasons == vec!["paused"]),
        "the move carries its cause and the whole list"
    );
    let mut events = handle.subscribe_events();
    // One millisecond past the pool's hour-long affinity TTL, the paused account still out of the set.
    handle
        .select(request(&handle, Some("s1"), 1_100 + HOUR + 1))
        .await
        .unwrap();
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(
        (
            summary.kind,
            record.outcome.account_id.as_deref(),
            pin_events_of(&record)
        ),
        (
            "placed",
            Some(to),
            vec![("pin_expired", Some("expired")), ("pin_created", None)]
        ),
        "the selection replaced the idle pin on the account it lands on again"
    );
    assert_eq!(
        record.outcome.from_account_id, None,
        "a pin replaced on its own account names no source"
    );
}

#[tokio::test]
async fn a_marked_pin_on_a_paused_account_idle_past_its_ttl_stays_an_expiry_names_no_source_and_clears_its_mark_as_released(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let (from, to) = ("a", "b");
    store
        .upsert_pin(&marked(stored_pin(&store, "s1", from, 1, 0.0), AT - 60_000))
        .unwrap();
    store.set_enabled(from, false, "test").unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    // No purge tick runs, so the row past its TTL below stays for the selection to replace.
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    // One millisecond past the pool's hour-long affinity TTL from the pin's last use.
    let answer = handle
        .select(request(&handle, Some("s1"), AT - 1_000 + HOUR + 1))
        .await
        .unwrap();
    let moved = match &answer.result {
        SelectResult::Selected(s) => s.trace.moved,
        other => panic!("{other:?}"),
    };
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(
        (
            summary.kind,
            summary.rule,
            moved,
            record.outcome.account_id.as_deref(),
            record.outcome.from_account_id.as_deref(),
            pin_events_of(&record)
        ),
        (
            "placed",
            None,
            None,
            Some(to),
            None,
            vec![("pin_expired", Some("expired")), ("pin_created", None)]
        ),
        "an idle pin expires beside its account's pause, and its placement names no source"
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { account_id, reasons, .. } if account_id == from && reasons == vec!["expired", "paused"]),
        "the expiry keeps both facts"
    );
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { account_id, .. } if account_id == to
    ));
    // The expired pin left for the sibling, so a mark reason read off the two accounts would say `moved`.
    assert!(
        matches!(
            next_event(&mut events).await,
            RouterEvent::MarkCleared {
                account_id,
                pin: PinKey { session_id, .. },
                reason: "released",
                marked_at,
                marked_on,
                ..
            } if account_id == from && session_id == "s1" && marked_at == AT - 60_000 && marked_on == from
        ),
        "the expired pin's mark leaves with it as released"
    );
}

#[tokio::test]
async fn a_pin_on_an_account_paused_as_it_left_the_pool_moves_under_paused_ahead_of_unserved() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let from = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let to = if from == "a" { "b" } else { "a" };
    // One reload takes the account out of the pool and pauses it. The reload's purge keeps a pin whose account left
    // its pool, so the selection finds both facts against it.
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: to.into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    store.set_enabled(&from, false, "test").unwrap();
    handle.reload().await.unwrap();
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let (_, record) = next_decision(&mut events).await;
    assert_eq!(
        pin_events_of(&record),
        [("pin_migrated", Some("paused"))],
        "what the account is outranks what its pool says"
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinMigrated { reason, reasons, .. } if reason == "paused" && reasons == vec!["unserved", "paused"]),
        "the move keeps the list in the selection's own order"
    );
}

#[tokio::test]
async fn a_refusal_emits_a_decision_with_a_two_line_walk() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let admitted = request(&handle, Some("s1"), 1_000);
    store.revoke_api_key("k", 2, "test").unwrap();
    handle.reload().await.unwrap();
    let mut events = handle.subscribe_events();
    let answer = handle.select(admitted).await.unwrap();
    assert!(matches!(
        answer.result,
        SelectResult::Refused(Refusal::Key(KeyRefusal::Revoked))
    ));
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(
        (summary.kind, summary.by.kind, summary.by.value.as_str()),
        ("refused", "refusal", "key")
    );
    assert_eq!(
        record
            .walk
            .iter()
            .map(|step| (step.step, step.verdict))
            .collect::<Vec<_>>(),
        [("admission", "refused"), ("selection", "not_run")]
    );
    assert!(record.candidates.is_empty(), "the engine never ran");
    assert_eq!(
        (
            record.request.request_id.as_str(),
            record.request.attempt,
            record.request.seq
        ),
        ("r-1", 1, 1)
    );
    assert_eq!(
        summary.refusal.as_ref().map(|r| (r.kind, r.code)),
        Some(("key", "revoked")),
        "the refusal's structured word rides beside its display reason"
    );
}

/// A drifted client's refusal as the relay records it: the request's one decision.
fn drift_refusal() -> (RefusedRequest, RefusalFacts) {
    (
        RefusedRequest {
            request_id: "r-drift".into(),
            attempt: 1,
            seq: 1,
            at: 1_000,
            key_id: "k".into(),
            key_generation: 0,
            agent_id: "main".into(),
            parent_agent_id: None,
            session_id: Some("s1".into()),
            request_class: None,
            canonical_model: "claude-sonnet-5".into(),
            lane: Default::default(),
            flags: Default::default(),
            chain: Default::default(),
            attempted: Vec::new(),
            holds: Vec::new(),
            client: ClientClass::ClaudeCode,
            client_version: Some("2.1.274".into()),
        },
        RefusalFacts {
            kind: "client",
            code: "drift",
            reason: "hash".into(),
        },
    )
}

#[tokio::test]
async fn a_relay_refusal_recorded_through_the_handle_emits_a_client_decision() {
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let (refused, refusal) = drift_refusal();
    handle.record_refusal(refused, refusal.clone());
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(
        (
            summary.kind,
            summary.by.value.as_str(),
            summary.refusal.as_ref()
        ),
        ("refused", "client", Some(&refusal))
    );
    assert_eq!(record.outcome.refusal.as_ref(), Some(&refusal));
    assert_eq!(
        (
            record.request.request_id.as_str(),
            record.request.seq,
            record.request.attempt,
            record.request.at
        ),
        ("r-drift", 1, 1, 1_000)
    );
    assert_eq!(handle.dropped_refusals(), 0);
}

#[tokio::test]
async fn a_completed_answer_emits_the_cache_map_it_wrote() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(seeded(&["a"]), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(Report::Affinity(AffinityReport {
            message_boundaries: Arc::from(["b1".to_string(), "b2".to_string()]),
            message_ttl_ms: Some(300_000),
            cache_touched: true,
            prefix_tokens: 1_200,
            request_id: "r-7".into(),
            seq: 2,
            attempt: 3,
            ..affinity_report(&handle, "a", 1_000)
        }))
        .await;
    match next_event(&mut events).await {
        RouterEvent::CacheMapWritten {
            request_id,
            seq,
            attempt,
            entries,
            at,
        } => {
            assert_eq!(
                (request_id.as_str(), seq, attempt, at),
                ("r-7", 2, 3, 1_000)
            );
            assert_eq!(
                entries
                    .iter()
                    .map(|e| (
                        e.tier,
                        e.depth,
                        e.prefix_tokens,
                        e.ttl_ms,
                        e.account_id.as_str()
                    ))
                    .collect::<Vec<_>>(),
                [
                    ("message", 1, 1_200, 300_000, "a"),
                    ("message", 2, 1_200, 300_000, "a")
                ]
            );
        }
        other => panic!("the cache map, not {other:?}"),
    }
    let entries_of = |event: RouterEvent| match event {
        RouterEvent::CacheMapWritten { entries, .. } => entries
            .iter()
            .map(|e| (e.tier, e.depth, e.prefix_tokens, e.ttl_ms))
            .collect::<Vec<_>>(),
        other => panic!("the cache map, not {other:?}"),
    };
    // b1 and b2 refresh in place and answer no row, so the one entry sits first in the list at depth 3.
    handle
        .report(Report::Affinity(AffinityReport {
            message_boundaries: Arc::from(["b1".to_string(), "b2".to_string(), "b3".to_string()]),
            message_ttl_ms: Some(300_000),
            cache_touched: true,
            prefix_tokens: 1_500,
            request_id: "r-8".into(),
            ..affinity_report(&handle, "a", 1_000)
        }))
        .await;
    assert_eq!(
        entries_of(next_event(&mut events).await),
        [("message", 3, 1_500, 300_000)]
    );
    let with_static = |cache_touched: bool, request_id: &str| {
        Report::Affinity(AffinityReport {
            static_boundaries: Arc::from(["s1".to_string()]),
            static_sizes: vec![800],
            static_ttl_ms: Some(3_600_000),
            cache_touched,
            request_id: request_id.into(),
            ..affinity_report(&handle, "a", 1_000)
        })
    };
    handle.report(with_static(false, "r-9")).await;
    assert_eq!(
        entries_of(next_event(&mut events).await),
        [],
        "an answer that touched no cache records no static boundary"
    );
    handle.report(with_static(true, "r-10")).await;
    assert_eq!(
        entries_of(next_event(&mut events).await),
        [("static", 1, 800, 3_600_000)]
    );
    handle
        .report(Report::Affinity(AffinityReport {
            incarnation: 0,
            message_boundaries: Arc::from(["b4".to_string()]),
            request_id: "r-11".into(),
            ..affinity_report(&handle, "a", 1_000)
        }))
        .await;
    assert_eq!(
        entries_of(next_event(&mut events).await),
        [],
        "a report from a holder the account moved past writes no row"
    );
}

#[tokio::test]
async fn the_router_broadcasts_pin_cooldown_and_overage_events() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let picked = |answer: &phoebis_routing::SelectAnswer| match &answer.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let a = picked(&first);
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinCreated { pin: PinKey { session_id, .. }, account_id, at: 1_000, .. } if session_id == "s1" && account_id == a)
    );
    // A 429 on the pinned account: the cooldown lands, the pin goes, and the next select migrates the session.
    let pin = match &first.result {
        SelectResult::Selected(s) => s.pin.clone().unwrap(),
        _ => unreachable!(),
    };
    handle
        .report(Report::Cooldown {
            account_id: a.clone(),
            row: CooldownRow {
                account_id: a.clone(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: Some("req_1".into()),
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownSet { account_id, scope, until, source_request_id: Some(r), .. } if account_id == a && scope == CooldownScope::Account && until == 1_000 + HOUR && r == "req_1")
    );
    let second = handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let b = picked(&second);
    assert_ne!(a, b);
    let (moved, record) = next_decision(&mut events).await;
    assert_eq!(
        (moved.kind, moved.rule),
        ("placed", Some("cooldown")),
        "a hard failover's reason is the decision's rule"
    );
    let landing = record
        .candidates
        .iter()
        .find(|candidate| candidate.account_id == b)
        .expect("the landing's row");
    match next_event(&mut events).await {
        RouterEvent::PinMigrated {
            from_account,
            to_account,
            reason,
            fit,
            runway_ms,
            binding,
            ..
        } => {
            assert_eq!(
                (from_account.as_str(), to_account.as_str(), reason.as_str()),
                (a.as_str(), b.as_str(), "cooldown")
            );
            // The fail-over lands by a fresh placement, so its fit is the no-fit partition's read: an API key
            // member whose buckets have room fits, with no runway to bound and no window binding.
            assert_eq!(
                (fit, runway_ms, binding.clone()),
                (landing.fit, landing.runway_ms, landing.binding.clone()),
                "the event carries the landing's row"
            );
            assert_eq!((fit, runway_ms, binding), (Some("fits"), None, None));
        }
        other => panic!("{other:?}"),
    }
    // Overage moves once per edge.
    let evidence = |in_use: bool, at: i64| phoebis_routing::OverageEvidence {
        in_use,
        since: in_use.then_some(at),
        reset: None,
        observed_at: at,
        source: phoebis_routing::EvidenceSource::Head,
        incarnation: 1,
    };
    // Each reading is read back before the next is reported: readings reported back to back replace each other
    // in the pending map, and only the newest lands.
    for (in_use, at) in [(true, 1_200), (true, 1_300), (false, 1_400)] {
        handle
            .report(Report::Overage {
                account_id: b.clone(),
                evidence: evidence(in_use, at),
                incarnation: 1,
            })
            .await;
        handle.overage(&b).await.unwrap();
    }
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::OverageStarted { account_id, .. } if account_id == b)
    );
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::OverageEnded { account_id, since: Some(1_200), .. } if account_id == b)
    );
    assert_eq!(
        handle
            .overages()
            .await
            .unwrap()
            .iter()
            .map(|(id, e)| (id.as_str(), e.in_use))
            .collect::<Vec<_>>(),
        vec![(b.as_str(), false)],
        "one ask lists every account with evidence, by id"
    );
    // Clearing the cooldown and the purge each say so, and each says what released the row it took.
    assert!(handle
        .clear_cooldown(&a, CooldownScope::Account)
        .await
        .unwrap());
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { account_id, cause, .. } if account_id == a && cause == "admin")
    );
    handle
        .select(request(&handle, Some("s2"), 2_000))
        .await
        .unwrap();
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinCreated { pin: PinKey { session_id, .. }, .. } if session_id == "s2")
    );
    handle
        .report(cooled(
            &a,
            "account",
            2_000 + HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    now.store(2_000 + HOUR + 1, Ordering::SeqCst);
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { account_id, cause, .. } if account_id == a && cause == "expired"),
        "the purge tick reached the deadline the rejection announced"
    );
    // The map holds the pins in no order, so the tick's two expiries are read as a set.
    let mut expired = Vec::new();
    for _ in 0..2 {
        match next_event(&mut events).await {
            RouterEvent::PinExpired { pin, reasons, .. } => expired.push((pin.session_id, reasons)),
            other => panic!("{other:?}"),
        }
    }
    expired.sort();
    assert_eq!(
        expired,
        [
            ("s1".to_string(), vec!["expired"]),
            ("s2".to_string(), vec!["expired"])
        ],
        "and the idle pins went with it"
    );
    // Last, because a fresh reading below the limit releases the rate-limit cooldown the assertions above
    // need still standing.
    handle
        .report(Report::Sample {
            account_id: a.clone(),
            sample: sample_with(1_500, FIVE_HOUR, 0.7, Some(9 * HOUR)).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.2,
                    reset: None,
                    observed_at: 1_500,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    // The ask is handled after a drain that holds the report, so the task has applied the sample before it answers
    // this.
    let samples = handle.samples().await.unwrap();
    assert_eq!(
        samples
            .iter()
            .map(|(id, s)| (id.as_str(), s.reading(FIVE_HOUR).map(|r| r.utilization)))
            .collect::<Vec<_>>(),
        vec![(a.as_str(), Some(0.7))],
        "one ask answers every account's latest sample, by id"
    );
}

#[tokio::test]
async fn a_spent_five_hour_window_migrates_the_pin_with_reason_spent() {
    use phoebis_routing::{Report, RouterEvent};
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let a = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let b = if a == "a" { "b" } else { "a" };
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    let spent = |five_hour: f64, at: i64| {
        sample_with(at, FIVE_HOUR, five_hour, Some(at + 3_600_000)).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.1,
                reset: Some(at + 7 * 86_400_000),
                observed_at: at,
                source: WindowSource::Head,
                plan: None,
                tier: None,
            },
        )
    };
    handle
        .report(Report::Sample {
            account_id: a.clone(),
            sample: spent(1.0, 1_500),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    handle
        .report(Report::Sample {
            account_id: b.into(),
            sample: spent(0.2, 1_500),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    let second = handle
        .select(request(&handle, Some("s1"), 2_000))
        .await
        .unwrap();
    assert!(matches!(&second.result, SelectResult::Selected(s) if s.account.id == b));
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::WindowSpent { account_id, window, .. } if account_id == a && window == FIVE_HOUR),
        "the reading that spent the window lands ahead of the selection that reads it"
    );
    let (moved, _) = next_decision(&mut events).await;
    assert_eq!((moved.kind, moved.rule), ("moved", Some("spent")));
    let event = next_event(&mut events).await;
    assert!(
        matches!(event, RouterEvent::PinMigrated { from_account, to_account, reason, .. } if from_account == a && to_account == b && reason == "spent")
    );
}

#[tokio::test]
async fn a_pin_on_a_cooled_and_spent_account_with_no_sibling_allowance_moves_under_cooldown_with_both_facts(
) {
    use phoebis_routing::{Report, RouterEvent};
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let a = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    let b = if a == "a" { "b" } else { "a" };
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    let spent = |at: i64| {
        sample_with(at, FIVE_HOUR, 1.0, Some(at + 3_600_000)).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.1,
                reset: Some(at + 7 * 86_400_000),
                observed_at: at,
                source: WindowSource::Head,
                plan: None,
                tier: None,
            },
        )
    };
    handle
        .report(cooled(
            &a,
            "account",
            1_000 + HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    handle
        .report(Report::Sample {
            account_id: a.clone(),
            sample: spent(1_500),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    // Read back before the next reading lands, so the two spent windows leave in the order reported.
    handle.samples().await.unwrap();
    handle
        .report(Report::Sample {
            account_id: b.into(),
            sample: spent(1_500),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    let second = handle
        .select(request(&handle, Some("s1"), 2_000))
        .await
        .unwrap();
    assert!(matches!(&second.result, SelectResult::Selected(s) if s.account.id == b));
    for account in [a.as_str(), b] {
        assert!(
            matches!(next_event(&mut events).await, RouterEvent::WindowSpent { account_id, .. } if account_id == account),
            "{account}'s spent window lands ahead of the selection"
        );
    }
    let (placed, record) = next_decision(&mut events).await;
    assert_eq!(placed.kind, "placed");
    assert_eq!(pin_events_of(&record), [("pin_migrated", Some("cooldown"))]);
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinMigrated { from_account, to_account, reason, reasons, .. } if from_account == a && to_account == b && reason == "cooldown" && reasons == vec!["cooldown", "spent"]),
        "no sibling holds allowance, so the spent rule names no destination and the cooled pin moves under its cooldown"
    );
}

/// A rejection as the relay reports it, with the clock deciding the row's set time.
fn cooled(account: &str, scope: &str, until: i64, reason: &str, window: &str) -> Report {
    Report::Cooldown {
        account_id: account.into(),
        row: CooldownRow {
            account_id: account.into(),
            scope: CooldownScope::parse(scope).unwrap(),
            until,
            set_at: 0,
            reason: reason.into(),
            window: window.into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    }
}

/// A sample that read no window; each test fills in the ones its rule turns on.
fn sample(observed_at: i64) -> UtilizationSample {
    UtilizationSample {
        observed_at,
        ..Default::default()
    }
}

/// `sample(observed_at)` with one window's reading folded in. The flag stays clear, as on a response head's sample, so
/// the reading names `Head`.
fn sample_with(
    observed_at: i64,
    window: &str,
    utilization: f64,
    reset: Option<i64>,
) -> UtilizationSample {
    sample(observed_at).with(
        window,
        WindowReading {
            utilization,
            reset,
            observed_at,
            source: WindowSource::Head,
            plan: None,
            tier: None,
        },
    )
}

/// `sample` as `sample_from_usage` builds a clean poll's: the flag set and each reading named `Poll`.
fn as_poll(sample: UtilizationSample) -> UtilizationSample {
    UtilizationSample {
        windows: sample
            .windows
            .into_iter()
            .map(|(window, reading)| {
                (
                    window,
                    WindowReading {
                        source: WindowSource::Poll,
                        ..reading
                    },
                )
            })
            .collect(),
        poll: true,
        ..sample
    }
}

fn observed(account: &str, sample: UtilizationSample) -> Report {
    Report::Sample {
        account_id: account.into(),
        sample,
        incarnation: 1,
        source_request_id: None,
    }
}

#[tokio::test]
async fn a_sample_that_shows_the_cooled_window_below_its_limit_releases_the_account() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 5 * HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownSet { account_id, .. } if account_id == "a")
    );
    assert!(
        matches!(
            handle
                .select(request(&handle, None, 1_100))
                .await
                .unwrap()
                .result,
            SelectResult::Exhausted(Exhaustion::Timed { .. })
        ),
        "the rejection is in force"
    );
    assert_eq!(next_decision(&mut events).await.0.kind, "exhausted");
    // Anthropic moved the reset in: hours before the deadline the 429 announced, the window reads below 1.
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)))
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { account_id, scope, cause, at } if account_id == "a" && scope == CooldownScope::Account && cause == "recovered" && at == 2_000)
    );
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .cooldowns
            .is_empty()
            .then_some(())
    })
    .await;
    assert!(
        matches!(handle.select(request(&handle, None, 2_100)).await.unwrap().result, SelectResult::Selected(ref s) if s.account.id == "a"),
        "and the account is back in the selection set"
    );
}

/// Two rejections often announce one reset. The second moves no deadline, but it is newer evidence that the
/// account is still rejecting, so a sample taken before it says nothing about the state it left behind.
#[tokio::test]
async fn a_repeated_rejection_moves_the_fence_a_recovery_sample_has_to_clear() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let rejection = || cooled("a", "account", 1_000 + 5 * HOUR, "rate_limit", "five_hour");
    handle.report(rejection()).await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    now.store(3_000, Ordering::SeqCst);
    handle.report(rejection()).await;
    // The poll behind this reading read the endpoint before the second rejection and landed after it.
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 0.4, None)),
        ))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a reading older than the latest rejection is not evidence the account recovered"
    );
    handle
        .report(observed("a", sample_with(3_001, FIVE_HOUR, 0.4, None)))
        .await;
    assert!(
        handle.cooldowns().await.unwrap().is_empty(),
        "one taken after it is"
    );
}

#[tokio::test]
async fn a_window_at_its_limit_keeps_the_cooldown() {
    use phoebis_routing::SPENT_MARGIN;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 48 * HOUR,
            "rate_limit",
            "seven_day",
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle
        .report(observed("a", sample_with(2_000, SEVEN_DAY, 1.0, None)))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a window that is exactly full has nothing left to serve with"
    );
    handle
        .report(observed("a", sample_with(2_001, FIVE_HOUR, 0.1, None)))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "and the row names the window that is read, not whichever one recovered"
    );
    // Under the margin, since a reading within it after the wall still reads the wall.
    handle
        .report(observed(
            "a",
            sample_with(2_002, SEVEN_DAY, 1.0 - SPENT_MARGIN - 0.001, None),
        ))
        .await;
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_claim_that_names_neither_shared_window_waits_for_both() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // The upstream's representative claim travels into the row as it came, so the window is not always one of
    // the two names the sample reads.
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 5 * HOUR,
            "rate_limit",
            "seven_day_oauth_apps",
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 0.4, None).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 1.0,
                    reset: None,
                    observed_at: 2_000,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "one shared window still full is enough to hold the account out"
    );
    handle
        .report(observed("a", sample_with(2_001, FIVE_HOUR, 0.4, None)))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "and a window the sample never read is no evidence either"
    );
    handle
        .report(observed(
            "a",
            sample_with(2_002, FIVE_HOUR, 0.4, None).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.9,
                    reset: None,
                    observed_at: 2_002,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_row_no_usage_window_answers_for_is_left_alone() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 5 * HOUR,
            "rate_limit",
            "retry_after",
        ))
        .await;
    // A permission rejection over a window a sample can read: the reason is what keeps the row, because no
    // amount of allowance restores an entitlement.
    handle
        .report(cooled(
            "a",
            "model:claude-sonnet-5",
            1_000 + HOUR,
            "permission",
            "five_hour",
        ))
        .await;
    // A 429 on the Fable model alone whose headers carried no claim: the upstream sent a deadline and named no
    // window, so the model week's reading is a guess about which window the rejection was for.
    handle
        .report(cooled(
            "a",
            "model:claude-fable-5-1",
            1_000 + HOUR,
            "rate_limit",
            "retry_after",
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 3);
    handle
        .report(observed(
            "a",
            sample_with(2_000, SEVEN_DAY_FABLE, 0.0, None)
                .with(
                    FIVE_HOUR,
                    WindowReading {
                        utilization: 0.0,
                        reset: None,
                        observed_at: 2_000,
                        source: WindowSource::Head,
                        plan: None,
                        tier: None,
                    },
                )
                .with(
                    SEVEN_DAY,
                    WindowReading {
                        utilization: 0.0,
                        reset: None,
                        observed_at: 2_000,
                        source: WindowSource::Head,
                        plan: None,
                        tier: None,
                    },
                ),
        ))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        3,
        "an empty account releases none of them"
    );
}

#[tokio::test]
async fn a_sample_older_than_the_one_already_stored_releases_nothing() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 5 * HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    // Two pulls in flight, the newer one back first: the older one is still later than the row was set, and
    // its reading is the one the window has already moved past.
    handle
        .report(observed("a", sample_with(3_000, FIVE_HOUR, 1.0, None)))
        .await;
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "the account's newest reading is what the row is answered against"
    );
    handle
        .report(observed("a", sample_with(3_001, FIVE_HOUR, 0.4, None)))
        .await;
    assert!(
        handle.cooldowns().await.unwrap().is_empty(),
        "and the next reading past the stored one releases it"
    );
}

#[tokio::test]
async fn a_sample_that_arrives_after_the_deadline_leaves_the_row_to_the_purge_tick() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    now.store(1_000 + HOUR + 1, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            sample_with(1_000 + HOUR + 1, FIVE_HOUR, 0.4, None),
        ))
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { cause, .. } if cause == "expired"),
        "the deadline arriving is not a window that reset early, and the ledger has to tell them apart"
    );
}

#[tokio::test]
async fn a_model_scoped_row_reads_the_model_window_and_nothing_else() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "model:claude-fable-5-1",
            1_000 + 48 * HOUR,
            "rate_limit",
            "seven_day_fable",
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 0.1, None).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.1,
                    reset: None,
                    observed_at: 2_000,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "the shared windows say nothing about a model-scoped allowance"
    );
    handle
        .report(observed(
            "a",
            sample_with(2_001, SEVEN_DAY_FABLE, 1.0, None),
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle
        .report(observed(
            "a",
            sample_with(2_002, SEVEN_DAY_FABLE, 0.5, None),
        ))
        .await;
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

#[test]
fn a_key_permit_counts_every_request_and_is_released_once_by_its_drop() {
    use phoebis_routing::InFlight;
    let in_flight = InFlight::default();
    assert_eq!(in_flight.key("k"), 0);
    let first = in_flight.try_acquire_key("k", 1_000);
    let second = in_flight.try_acquire_key("k", 1_000);
    assert_eq!(
        in_flight.key("k"),
        2,
        "a key has no cap, so a second request takes its own permit"
    );
    drop(first);
    assert_eq!(in_flight.key("k"), 1);
    drop(second);
    assert_eq!(in_flight.key("k"), 0);
    let other = in_flight.try_acquire_key("other", 1_000);
    assert_eq!((in_flight.key("k"), in_flight.key("other")), (0, 1));
    drop(other);
    assert_eq!(in_flight.key("other"), 0);
}

#[test]
fn the_purge_reclaims_a_permit_and_a_ticket_whose_guard_never_dropped() {
    use phoebis_routing::{InFlight, IN_FLIGHT_RECLAIM_MS};
    let in_flight = InFlight::default();
    let taken_at = 1_000 - IN_FLIGHT_RECLAIM_MS - 1;
    let permit = in_flight.try_acquire_key("k", taken_at);
    let ticket = in_flight.dispatched("a", taken_at);
    assert_eq!((in_flight.key("k"), in_flight.account("a")), (1, 1));
    assert_eq!(
        in_flight.expire(1_000),
        2,
        "both are older than the reclaim age"
    );
    assert_eq!((in_flight.key("k"), in_flight.account("a")), (0, 0));
    drop(permit);
    drop(ticket);
    assert_eq!(
        (in_flight.key("k"), in_flight.account("a")),
        (0, 0),
        "the purge already reclaimed both rows, so the late drops find nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_completion_wakes_a_waiter_and_the_bound_ends_the_wait() {
    use phoebis_routing::{InFlight, IN_FLIGHT_RECLAIM_MS};
    use tokio::time::Instant;
    let in_flight = InFlight::default();

    // A completion: the waiter resolves with a slot as soon as the ticket drops, long before its bound.
    let ticket = in_flight.dispatched("a", 1_000);
    let waiter = tokio::spawn({
        let in_flight = in_flight.clone();
        async move {
            in_flight
                .wait_for_slot("a", 1, Instant::now() + Duration::from_secs(30))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiter.is_finished(),
        "a is at its cap, so the waiter waits"
    );
    let freed = Instant::now();
    drop(ticket);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the drop wakes the waiter")
            .unwrap(),
        "a slot opened"
    );
    assert!(freed.elapsed() < Duration::from_secs(2));

    // The purge: a reclaimed ticket frees its slot the way a drop does.
    let stale = in_flight.dispatched("a", 1_000);
    let waiter = tokio::spawn({
        let in_flight = in_flight.clone();
        async move {
            in_flight
                .wait_for_slot("a", 1, Instant::now() + Duration::from_secs(30))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    assert_eq!(in_flight.expire(1_000 + IN_FLIGHT_RECLAIM_MS + 1), 1);
    assert!(tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("the purge wakes the waiter")
        .unwrap());
    drop(stale);

    // The bound: with the slot still taken the wait ends at its deadline without one, and a release on
    // another account does not end it early.
    let held = in_flight.dispatched("a", 2_000);
    let other = in_flight.dispatched("b", 2_000);
    let started = Instant::now();
    let waiter = tokio::spawn({
        let in_flight = in_flight.clone();
        async move {
            in_flight
                .wait_for_slot("a", 1, Instant::now() + Duration::from_millis(300))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(other);
    assert!(
        !waiter.await.unwrap(),
        "the bound passed with a still at its cap"
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(in_flight.account("a"), 1);

    // Under the cap already: no wait at all.
    assert!(
        in_flight
            .wait_for_slot("a", 2, Instant::now() + Duration::from_secs(30))
            .await
    );
    drop(held);
}

#[tokio::test]
async fn two_selections_answered_together_read_the_slot_the_first_one_charged() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let cap = phoebis_store::Failover::default().in_flight_cap;
    let _busy: Vec<_> = (1..cap)
        .map(|_| handle.in_flight().dispatched("a", 1_000))
        .collect();
    assert_eq!(
        handle.in_flight().account("a"),
        cap - 1,
        "one below its cap"
    );
    let (one, two) = tokio::join!(
        handle.select(request(&handle, Some("s1"), 1_000)),
        handle.select(request(&handle, Some("s2"), 1_000)),
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    let landed = [&one, &two]
        .iter()
        .filter(|answer| matches!(answer.result, SelectResult::Selected(_)))
        .count();
    let held = [&one, &two]
        .iter()
        .filter(
            |answer| matches!(&answer.result, SelectResult::Held(hold) if hold.account_id == "a"),
        )
        .count();
    assert_eq!(
        (landed, held),
        (1, 1),
        "the second selection read the slot the first one charged: {:?} {:?}",
        one.result,
        two.result
    );
    assert_eq!(
        handle.in_flight().account("a"),
        cap,
        "the selection's ticket is the one charge"
    );
    assert!(
        [&one, &two]
            .iter()
            .all(|answer| answer.ticket.is_some()
                == matches!(answer.result, SelectResult::Selected(_))),
        "only a selection carries a ticket"
    );
    drop((one, two));
    assert_eq!(
        handle.in_flight().account("a"),
        cap - 1,
        "an answer nobody dialled releases its charge"
    );
}

#[tokio::test]
async fn a_fail_over_after_a_cooldown_names_cooldown_in_the_trace() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let a = match &first.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    let pin = match &first.result {
        SelectResult::Selected(s) => s.pin.clone().unwrap(),
        _ => unreachable!(),
    };
    handle
        .report(Report::Cooldown {
            account_id: a.clone(),
            row: CooldownRow {
                account_id: a.clone(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    let second = handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = &second.result else {
        panic!("{:?}", second.result)
    };
    assert_eq!(sel.trace.moved, Some("cooldown"));
    let (placed, record) = next_decision(&mut events).await;
    assert_eq!(
        (placed.kind, placed.rule, record.outcome.rule),
        ("placed", Some("cooldown"), Some("cooldown")),
        "the trace's move is the record's rule"
    );
    assert_eq!(
        record.outcome.from_account_id.as_deref(),
        Some(a.as_str()),
        "the record names the account the fail-over left, as the pin's move does"
    );
    let cooldown = Some(MoveCause::Cooldown {
        scope: "account".into(),
        reason: "rate_limit".into(),
        window: Some("five_hour".into()),
    });
    assert_eq!(
        record.outcome.cause, cooldown,
        "the record carries the cooldown's row as the cause"
    );
    match next_event(&mut events).await {
        RouterEvent::PinMigrated {
            reason,
            from_account,
            cause,
            ..
        } => assert_eq!(
            (reason.as_str(), from_account.as_str(), cause),
            ("cooldown", a.as_str(), cooldown),
            "the event carries the cause beside its reason"
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_fast_row_with_a_pin_invalidates_it_under_fast_lane() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let SelectResult::Selected(selection) = &first.result else {
        panic!("{:?}", first.result)
    };
    // Both members sit at one priority, so the strategy may start the session on either.
    let from = selection.account.id.clone();
    let to = if from == "a" { "b" } else { "a" };
    let pin = selection.pin.clone().unwrap();
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    handle
        .report(Report::Cooldown {
            account_id: from.clone(),
            row: CooldownRow {
                account_id: from.clone(),
                scope: CooldownScope::Fast,
                until: 1_000 + 600_000,
                set_at: 0,
                reason: FAST_RATE_LIMIT_REASON.into(),
                window: "fast".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { scope, .. } if scope == CooldownScope::Fast
    ));
    // The relay's next selection of the same request, with the account it just tried.
    let second = handle
        .select(SelectRequest {
            attempted: vec![from.clone()],
            ..request(&handle, Some("s1"), 1_100)
        })
        .await
        .unwrap();
    let SelectResult::Selected(sel) = &second.result else {
        panic!("{:?}", second.result)
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.moved),
        (to, Some("fast_lane"))
    );
    let (placed, record) = next_decision(&mut events).await;
    assert_eq!((placed.kind, placed.rule), ("placed", Some("fast_lane")));
    let fast_row = Some(MoveCause::Cooldown {
        scope: "fast".into(),
        reason: FAST_RATE_LIMIT_REASON.into(),
        window: Some("fast".into()),
    });
    assert_eq!(record.outcome.cause, fast_row);
    match next_event(&mut events).await {
        RouterEvent::PinMigrated {
            from_account,
            to_account,
            reason,
            cause,
            ..
        } => assert_eq!(
            (
                from_account.as_str(),
                to_account.as_str(),
                reason.as_str(),
                cause
            ),
            (from.as_str(), to, "fast_lane", fast_row)
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_mark_without_a_cooldown_moves_the_pin_with_the_marks_state_as_its_cause() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let first = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let SelectResult::Selected(selection) = &first.result else {
        panic!("{:?}", first.result)
    };
    // Both members sit at one priority, so the strategy may start the session on either.
    let from = selection.account.id.clone();
    let to = if from == "a" { "b" } else { "a" };
    let pin = selection.pin.clone().unwrap();
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    handle
        .report(Report::PinInvalidated {
            account_id: from.clone(),
            pin,
            mark: Some((AccountState::FundingRequired, StateCause::FundingRefused)),
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    let second = handle
        .select(SelectRequest {
            attempted: vec![from.clone()],
            ..request(&handle, Some("s1"), 1_100)
        })
        .await
        .unwrap();
    let SelectResult::Selected(sel) = &second.result else {
        panic!("{:?}", second.result)
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.moved),
        (to, Some("mark"))
    );
    let mark = Some(MoveCause::Mark {
        state: "funding_required",
        cause: "funding_refused",
    });
    let (placed, record) = next_decision(&mut events).await;
    assert_eq!(
        (placed.rule, &record.outcome.cause),
        (Some("mark"), &mark),
        "the record names the mark's state and who moved it"
    );
    match next_event(&mut events).await {
        RouterEvent::PinMigrated { reason, cause, .. } => {
            assert_eq!((reason.as_str(), cause), ("mark", mark))
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_fail_over_after_a_cooldown_lands_on_the_invalidated_pins_own_burn() {
    use phoebis_routing::RouterEvent;
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle.report(windows("a", 0.3, 0.1, AT)).await;
    handle.report(windows("b", 0.3, 0.1, AT)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: AT + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin_of(&handle).await),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert_eq!(
        landed_on(
            handle
                .select(SelectRequest {
                    attempted: vec!["a".into()],
                    ..request(&handle, Some("s1"), AT)
                })
                .await
                .unwrap()
        ),
        ("b".into(), None)
    );
    let record = loop {
        if let RouterEvent::Decision { record, .. } = next_event(&mut events).await {
            break Arc::unwrap_or_clone(record);
        }
    };
    // The pool keeps no warm pin, so a fresh session's prior burns nothing. The row the cooldown took off a burns 10
    // input tokens a second, a tenth of the 5-hour window an hour, which reaches b's 0.65 to the ceiling in 6.5 hours.
    let b = record
        .candidates
        .iter()
        .find(|c| c.account_id == "b")
        .expect("b's row");
    assert_eq!(
        (b.runway_ms, b.fit),
        (Some(23_400_000), Some("fits")),
        "the landing's fit reads the invalidated pin's burn"
    );
    let booked = record
        .outcome
        .reservation
        .as_ref()
        .expect("the landing books the pin's burn");
    assert!(
        close(booked.burn.get(FIVE_HOUR).copied(), 0.1),
        "{booked:?}"
    );
}

/// The poll's extra-usage state of account `a`, read at `fetched_at` and converted as the poll converts it.
fn credits_at(
    enabled: bool,
    reason: Option<&str>,
    utilization: Option<f64>,
    fetched_at: i64,
) -> Report {
    let snapshot = UsageSnapshot {
        account_id: "a".into(),
        identity_generation: 1,
        fetched_at,
        windows: vec![],
        fable_window: None,
        extra_usage: Some(ExtraUsage {
            is_enabled: enabled,
            monthly_limit: None,
            used_credits: None,
            utilization,
            currency: None,
            decimal_places: None,
            disabled_reason: reason.map(String::from),
        }),
        raw_keys: vec![],
        error: None,
        spend: None,
        fable_week_share: None,
    };
    Report::Credits {
        account_id: "a".into(),
        state: credits_from_usage(&snapshot, 1).expect("the snapshot carries extra usage"),
        incarnation: 1,
    }
}

/// The reasons of the fast lane's live rows, the rate limit's before the credits refusal's.
async fn fast_rows(handle: &RouterHandle) -> Vec<String> {
    handle
        .cooldowns()
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.scope == CooldownScope::Fast || row.scope == CooldownScope::FastCredits)
        .map(|row| row.reason)
        .collect()
}

#[tokio::test]
async fn an_enabled_credits_state_with_room_lifts_a_fast_credits_row_and_a_plan_sample_lifts_no_fast_row(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(cooled(
            "a",
            "fast_credits",
            1_000 + 5 * HOUR,
            FAST_CREDITS_REASON,
            "out_of_credits",
        ))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { scope, .. } if scope == CooldownScope::FastCredits
    ));
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 0.4, None).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.4,
                    reset: None,
                    observed_at: 2_000,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    handle.report(credits_at(true, None, Some(20.0), 900)).await;
    handle
        .report(credits_at(false, Some("out_of_credits"), None, 2_500))
        .await;
    handle
        .report(credits_at(true, None, Some(100.0), 2_600))
        .await;
    assert_eq!(
        fast_rows(&handle).await,
        [FAST_CREDITS_REASON],
        "a plan sample, a reading from before the rejection, a refusal and credits at their limit lift nothing"
    );
    handle
        .report(credits_at(true, None, Some(20.0), 3_000))
        .await;
    loop {
        match next_event(&mut events).await {
            RouterEvent::CooldownCleared {
                account_id,
                scope,
                cause,
                ..
            } => {
                assert_eq!(
                    (account_id.as_str(), scope, cause),
                    ("a", CooldownScope::FastCredits, "recovered")
                );
                break;
            }
            RouterEvent::CooldownSet { .. } => panic!("no row was set"),
            _ => {}
        }
    }
    assert!(fast_rows(&handle).await.is_empty());
    handle
        .report(cooled(
            "a",
            "fast",
            2_000 + 600_000,
            FAST_RATE_LIMIT_REASON,
            "fast",
        ))
        .await;
    handle
        .report(cooled(
            "a",
            "fast_credits",
            2_000 + HOUR,
            FAST_CREDITS_REASON,
            "out_of_credits",
        ))
        .await;
    assert_eq!(
        fast_rows(&handle).await,
        [FAST_RATE_LIMIT_REASON, FAST_CREDITS_REASON],
        "the two refusals keep a row each"
    );
    handle
        .report(credits_at(true, None, Some(20.0), 4_000))
        .await;
    assert_eq!(
        fast_rows(&handle).await,
        [FAST_RATE_LIMIT_REASON],
        "a credits reading lifts the credits row alone and says nothing about the fast lane's own rate limit"
    );
}

#[tokio::test]
async fn a_credits_reading_with_room_lifts_the_spent_window_rows_and_leaves_a_model_rejection() {
    use phoebis_routing::{RouterEvent, MODEL_REJECTED_REASON};
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    for (scope, reason, window) in [
        ("account", "rate_limit", FIVE_HOUR),
        ("model:claude-fable-5-1", "rate_limit", SEVEN_DAY_FABLE),
        (
            "model:claude-opus-5",
            MODEL_REJECTED_REASON,
            "model_rejection",
        ),
    ] {
        handle
            .report(cooled("a", scope, 1_000 + 5 * HOUR, reason, window))
            .await;
    }
    let scopes = || async {
        handle
            .cooldowns()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.scope.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        scopes().await,
        ["account", "model:claude-fable-5-1", "model:claude-opus-5"]
    );
    now.store(2_000, Ordering::SeqCst);
    handle.report(credits_at(true, None, Some(20.0), 900)).await;
    assert_eq!(
        scopes().await.len(),
        3,
        "a reading taken before the rejections says nothing about them"
    );
    handle
        .report(credits_at(true, None, Some(20.0), 3_000))
        .await;
    assert_eq!(
        scopes().await,
        ["model:claude-opus-5"],
        "credits with room fund the request past a spent window and say nothing about a model the upstream refused"
    );
    let mut cleared = Vec::new();
    while cleared.len() < 2 {
        if let RouterEvent::CooldownCleared { scope, cause, .. } = next_event(&mut events).await {
            cleared.push((scope, cause));
        }
    }
    cleared.sort();
    assert_eq!(
        cleared,
        [
            (CooldownScope::Account, "recovered"),
            (CooldownScope::Model("claude-fable-5-1".into()), "recovered"),
        ]
    );
    // Set after the last reading and read before the next, so the row's window alone decides the lift.
    now.store(4_000, Ordering::SeqCst);
    handle
        .report(cooled(
            "a",
            "account",
            4_000 + HOUR,
            "rate_limit",
            "retry_after",
        ))
        .await;
    handle
        .report(credits_at(true, None, Some(20.0), 5_000))
        .await;
    assert_eq!(
        scopes().await,
        ["account", "model:claude-opus-5"],
        "a reading with room keeps an account rate limit that names no plan window"
    );
}

/// A refusal of the seat `a` as a whole under `overage`, which is no plan window, so the lift of a spent plan window
/// leaves the row and the seat's own share decides it.
fn seat_refused(until: i64) -> Report {
    cooled("a", "account", until, "rate_limit", "overage")
}

/// The scopes of the rows that the router holds. A case reads them behind each refusal and between two polls, since
/// the loop drains a pending poll ahead of a queued report and two pending polls of one account fold to the newer.
async fn cooled_scopes(handle: &RouterHandle) -> Vec<String> {
    handle
        .cooldowns()
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.scope.to_string())
        .collect()
}

/// The scope and the cause of the next row that the router clears.
async fn next_cleared(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> (CooldownScope, &'static str) {
    loop {
        if let phoebis_routing::RouterEvent::CooldownCleared { scope, cause, .. } =
            next_event(events).await
        {
            return (scope, cause);
        }
    }
}

#[tokio::test]
async fn a_spend_reading_under_the_share_at_the_refusal_lifts_the_seats_row() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.report(credits_at(true, None, Some(97.0), 500)).await;
    handle.report(seat_refused(1_000 + 5 * HOUR)).await;
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(97.0), 2_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        ["account"],
        "a reading at the share held at the refusal leaves the row"
    );
    handle
        .report(credits_at(true, None, Some(96.0), 3_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        Vec::<String>::new(),
        "a reading under that share lifts the row"
    );
    assert_eq!(
        next_cleared(&mut events).await,
        (CooldownScope::Account, "recovered")
    );
}

#[tokio::test]
async fn a_seat_refused_with_no_share_held_lifts_under_one() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.report(seat_refused(1_000 + 5 * HOUR)).await;
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(100.0), 2_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        ["account"],
        "a reading at one leaves the row"
    );
    handle
        .report(credits_at(true, None, Some(99.0), 3_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        Vec::<String>::new(),
        "a refusal with no share held keeps one, so a reading under one lifts the row"
    );
    assert_eq!(
        next_cleared(&mut events).await,
        (CooldownScope::Account, "recovered")
    );
}

/// The clock moves between the two refusals, since the router dates each by its own clock and a refusal at the
/// standing instant moves nothing.
#[tokio::test]
async fn a_reading_between_the_shares_of_two_refusals_lifts_the_seats_row() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.report(credits_at(true, None, Some(90.0), 500)).await;
    handle.report(seat_refused(1_000 + 5 * HOUR)).await;
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(97.0), 1_500))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        ["account"],
        "a reading over the first refusal's share leaves the row"
    );
    now.store(2_000, Ordering::SeqCst);
    handle.report(seat_refused(1_000 + 5 * HOUR)).await;
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(95.0), 3_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        Vec::<String>::new(),
        "the second refusal brings its share under the standing deadline, and a reading under it lifts the row"
    );
    assert_eq!(
        next_cleared(&mut events).await,
        (CooldownScope::Account, "recovered")
    );
}

/// The clock moves between the two refusals, as in the case of two refusals under one deadline.
#[tokio::test]
async fn a_refusal_under_a_later_deadline_brings_its_share_to_the_seats_row() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.report(credits_at(true, None, Some(90.0), 500)).await;
    handle.report(seat_refused(1_000 + 5 * HOUR)).await;
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(97.0), 1_500))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        ["account"],
        "a reading over the first refusal's share leaves the row"
    );
    now.store(2_000, Ordering::SeqCst);
    handle.report(seat_refused(2_000 + 6 * HOUR)).await;
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(95.0), 3_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        Vec::<String>::new(),
        "the refusal that wins the deadline brings its share, and a reading under it lifts the row"
    );
    assert_eq!(
        next_cleared(&mut events).await,
        (CooldownScope::Account, "recovered")
    );
}

#[tokio::test]
async fn a_seats_row_with_no_stored_share_lifts_under_one() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    store
        .upsert_cooldown(&CooldownRow {
            account_id: "a".into(),
            scope: CooldownScope::Account,
            until: 1_000 + 5 * HOUR,
            set_at: 500,
            reason: "rate_limit".into(),
            window: "overage".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        })
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    assert_eq!(cooled_scopes(&handle).await, ["account"]);
    handle
        .report(credits_at(true, None, Some(99.0), 2_000))
        .await;
    assert_eq!(
        cooled_scopes(&handle).await,
        Vec::<String>::new(),
        "a row stored with no share reads one, so a reading under one lifts it"
    );
    assert_eq!(
        next_cleared(&mut events).await,
        (CooldownScope::Account, "recovered")
    );
}

#[tokio::test]
async fn a_restart_judges_fast_eligibility_on_the_credits_state_the_last_poll_read() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(credits_at(false, Some("out_of_credits"), None, 1_000))
        .await;
    wait_for(WRITTEN, || {
        let credits = store.load_routing().unwrap().credits;
        (!credits.is_empty()).then_some(credits)
    })
    .await;
    handle.shutdown().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let fast = SelectRequest {
        fast: true,
        lane: lane_key("claude-sonnet-5", "fast", "none", "-"),
        ..request(&again, None, 2_000)
    };
    assert_eq!(
        again.select(fast).await.unwrap().result,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "out_of_credits".into()
        }),
        "before any poll, the restored state keeps a fast request off the account"
    );
}

#[tokio::test]
async fn a_restart_restores_both_fast_rows_and_a_fast_request_waits_on_whichever_still_stands() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "fast_credits",
            1_000 + HOUR,
            FAST_CREDITS_REASON,
            "out_of_credits",
        ))
        .await;
    handle
        .report(cooled(
            "a",
            "fast",
            1_000 + 600_000,
            FAST_RATE_LIMIT_REASON,
            "fast",
        ))
        .await;
    wait_for(WRITTEN, || {
        (store.load_routing().unwrap().cooldowns.len() == 2).then_some(())
    })
    .await;
    handle.shutdown().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        again
            .cooldowns()
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.scope.to_string(), row.reason, row.until))
            .collect::<Vec<_>>(),
        [
            (
                "fast".to_string(),
                FAST_RATE_LIMIT_REASON.to_string(),
                1_000 + 600_000
            ),
            (
                "fast_credits".to_string(),
                FAST_CREDITS_REASON.to_string(),
                1_000 + HOUR
            ),
        ],
        "each row comes back with its own deadline"
    );
    let fast = || SelectRequest {
        fast: true,
        lane: lane_key("claude-sonnet-5", "fast", "none", "-"),
        ..request(&again, None, 2_000)
    };
    assert_eq!(
        again.select(fast()).await.unwrap().result,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "out_of_credits".into()
        }),
        "the credits row answers ahead of the rate limit beside it"
    );
    assert!(again
        .clear_cooldown("a", CooldownScope::FastCredits)
        .await
        .unwrap());
    assert_eq!(
        again.select(fast()).await.unwrap().result,
        SelectResult::Exhausted(Exhaustion::FastLane {
            deadline: 1_000 + 600_000
        }),
        "the rate limit alone still refuses until its own reset"
    );
}

/// A request of session "s1" under the spelling `claude-sonnet-4-5`, its lane composed from that spelling as the
/// relay composes it at admission, on the main thread so its warmth reads the lane alone.
fn spelled(handle: &RouterHandle, now: i64) -> SelectRequest {
    let s = handle.snapshot();
    SelectRequest {
        canonical_model: "claude-sonnet-4-5".into(),
        candidates: resolve(
            &s,
            s.key("k").unwrap(),
            "claude-sonnet-4-5",
            "claude-sonnet-4-5",
        )
        .unwrap(),
        request_class: Some("main".into()),
        lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
        ..request(handle, Some("s1"), now)
    }
}

#[tokio::test]
async fn a_resolution_between_admission_and_selection_keeps_the_pin_warm_on_its_relaned_lane() {
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let first = handle.select(spelled(&handle, 1_000)).await.unwrap();
    assert!(matches!(first.result, SelectResult::Selected(_)));
    // The continuation is admitted under the spelling, and the catalog resolves it before the router selects.
    let admitted = spelled(&handle, 2_000);
    handle
        .resolve("claude-sonnet-4-5", DATED, 1_500)
        .await
        .unwrap();
    let answer = handle.select(admitted).await.unwrap();
    assert!(matches!(answer.result, SelectResult::Selected(_)));
    let dated_lane = lane_key(DATED, "standard", "none", "-");
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|p| (p.model.as_str(), p.lane.as_str(), p.last_refresh_at))
            .collect::<Vec<_>>(),
        [(DATED, dated_lane.as_str(), 2_000)],
        "the turn refreshes its pin and keeps the lane the resolution gave it"
    );
}

#[tokio::test]
async fn a_resolution_between_selection_and_completion_files_the_affinity_entry_under_the_dated_id_and_lane(
) {
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let selected = handle.select(spelled(&handle, 1_000)).await.unwrap();
    assert!(matches!(selected.result, SelectResult::Selected(_)));
    handle
        .resolve("claude-sonnet-4-5", DATED, 1_500)
        .await
        .unwrap();
    // The answer completes after the resolution landed, and its report still names what admission read.
    handle
        .report(Report::Affinity(AffinityReport {
            canonical_model: "claude-sonnet-4-5".into(),
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            ..affinity_report(&handle, "a", 2_000)
        }))
        .await;
    // A query queues behind the report, so the map it reads afterwards holds the report's entries.
    handle.pins().await.unwrap();
    let entry = handle
        .affinity()
        .read()
        .unwrap()
        .entry(DATED, "b1")
        .cloned()
        .expect("the entry is filed under the dated id");
    assert_eq!(
        entry.lane,
        lane_key(DATED, "standard", "none", "-"),
        "the entry's lane names the model its key does, so the next request on the dated id reads it warm"
    );
}

#[tokio::test]
async fn a_fork_after_a_resolution_places_on_the_warm_entry_a_late_report_filed_and_not_by_the_strategy(
) {
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let selected = handle.select(spelled(&handle, 1_000)).await.unwrap();
    let SelectResult::Selected(selection) = &selected.result else {
        panic!("{:?}", selected.result)
    };
    let source = selection.account.id.clone();
    handle
        .resolve("claude-sonnet-4-5", DATED, 1_500)
        .await
        .unwrap();
    // The source's answer completes after the resolution, its message boundary confirmed warm.
    handle
        .report(Report::Affinity(AffinityReport {
            canonical_model: "claude-sonnet-4-5".into(),
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            cache_touched: true,
            message_ttl_ms: Some(300_000),
            prefix_tokens: 40_000,
            ..affinity_report(&handle, &source, 2_000)
        }))
        .await;
    // A fork under a session of its own, admitted on the dated id, continues the source's messages.
    let fork = SelectRequest {
        session_id: "s2".into(),
        canonical_model: DATED.into(),
        lane: lane_key(DATED, "standard", "none", "-"),
        boundaries: Arc::from(["b1".to_string(), "fork-1".to_string()]),
        ..spelled(&handle, 3_000)
    };
    let answer = handle.select(fork).await.unwrap();
    let SelectResult::Selected(placed) = &answer.result else {
        panic!("{:?}", answer.result)
    };
    assert_eq!(
        (
            placed.account.id.as_str(),
            placed.trace.placement.as_ref().map(|p| p.rung)
        ),
        (source.as_str(), Some("content")),
        "the fork lands on the source's warm prefix"
    );
}

#[tokio::test]
async fn a_credits_state_older_than_the_held_one_is_dropped() {
    let mut state = RoutingState::from_load(RoutingLoad::default());
    let reading = |enabled: bool, observed_at: i64| CreditsState {
        enabled,
        disabled_reason: None,
        utilization: None,
        observed_at,
        incarnation: 1,
    };
    assert!(state.record_credits("a", reading(false, 2_000)));
    assert!(
        !state.record_credits("a", reading(true, 1_500)),
        "an older reading"
    );
    assert!(
        !state.record_credits("a", reading(true, 2_000)),
        "a reading no newer than the held one"
    );
    assert_eq!(
        state.credits("a").map(|c| (c.enabled, c.observed_at)),
        Some((false, 2_000))
    );
    assert!(state.record_credits("a", reading(true, 2_500)));
    assert_eq!(state.credits("a").and_then(CreditsState::blocked), None);

    // Through the task: the dropped reading lifts nothing, the newer one does.
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "fast_credits",
            1_000 + 5 * HOUR,
            FAST_CREDITS_REASON,
            "out_of_credits",
        ))
        .await;
    handle
        .report(credits_at(false, Some("out_of_credits"), None, 3_000))
        .await;
    handle
        .report(credits_at(true, None, Some(10.0), 2_500))
        .await;
    assert_eq!(fast_rows(&handle).await, [FAST_CREDITS_REASON]);
    handle
        .report(credits_at(true, None, Some(10.0), 3_500))
        .await;
    assert!(fast_rows(&handle).await.is_empty());
}

#[tokio::test]
async fn older_overage_and_credits_readings_never_replace_newer_pending_ones() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let overage = |in_use: bool, observed_at: i64| Report::Overage {
        account_id: "a".into(),
        evidence: phoebis_routing::OverageEvidence {
            in_use,
            since: in_use.then_some(observed_at),
            reset: Some(10 * HOUR),
            observed_at,
            source: phoebis_routing::EvidenceSource::Head,
            incarnation: 1,
        },
        incarnation: 1,
    };
    let park = handle.park_loop().await;
    // Each newer reading arrives first, so only the merge rule keeps it.
    handle.report(overage(true, 3_000)).await;
    handle.report(overage(false, 2_000)).await;
    handle
        .report(credits_at(false, Some("out_of_credits"), None, 3_000))
        .await;
    handle
        .report(credits_at(true, None, Some(10.0), 2_500))
        .await;
    drop(park);
    assert_eq!(
        handle
            .overage("a")
            .await
            .unwrap()
            .map(|e| (e.in_use, e.observed_at)),
        Some((true, 3_000)),
        "the newer overage reading"
    );
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .credits
            .iter()
            .any(|row| row.account_id == "a" && !row.enabled && row.observed_at == 3_000)
            .then_some(())
    })
    .await;
}

/// Stores a profile for `account` whose rate-limit tier classes it a usage-based Enterprise seat.
fn usage_based_seat(store: &Store, account: &str) {
    store
        .set_profile(
            account,
            r#"{"organization":{"rate_limit_tier":"default_claude_zero"}}"#,
            &SubscriptionRow {
                tier: Some("default_claude_zero".into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
}

/// A head on the usage-based seat `a` carrying the overage family at `utilization` and `reset`.
fn spend_head(utilization: f64, reset: i64, observed_at: i64) -> Report {
    Report::SpendHead {
        account_id: "a".into(),
        utilization: Some(utilization),
        reset: Some(reset),
        observed_at,
        incarnation: 1,
    }
}

/// The persisted credits row of `a` once it reads `utilization`.
async fn credits_row_reads(store: &Store, utilization: f64) -> phoebis_store::CreditsRow {
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .credits
            .into_iter()
            .find(|row| row.account_id == "a" && row.utilization == Some(utilization))
    })
    .await
}

#[tokio::test]
async fn a_spend_head_moves_the_credits_share_with_no_overage_edge_and_an_older_one_changes_nothing(
) {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.report(spend_head(0.4, 30 * HOUR, 1_000)).await;
    let row = credits_row_reads(&store, 0.4).await;
    assert_eq!(
        (row.enabled, row.disabled_reason, row.observed_at),
        (true, None, 1_000),
        "a head before any poll builds the credits state, enabled and with no reason"
    );
    handle.report(spend_head(0.9, 31 * HOUR, 900)).await;
    stays_for(NOT_WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .credits
            .iter()
            .all(|row| row.utilization == Some(0.4))
    })
    .await;
    assert_eq!(
        handle.overage("a").await.unwrap(),
        None,
        "the head records no overage evidence"
    );
    // A later poll replaces the head's state by its age, as any poll does.
    handle
        .report(credits_at(true, None, Some(10.0), 2_000))
        .await;
    credits_row_reads(&store, 0.1).await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, phoebis_routing::RouterEvent::OverageStarted { .. }),
            "a spend head starts no overage episode: {event:?}"
        );
    }
}

#[test]
fn a_spend_head_keeps_its_reset_beside_the_share_and_an_older_reading_moves_neither() {
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(st.spend_head("a"), None, "no head has named a reset yet");
    assert!(st.record_spend_head("a", Some(0.4), Some(9 * HOUR), 1_000, 1));
    assert_eq!(st.spend_head("a"), Some((9 * HOUR, 1_000)));
    let credits = |st: &RoutingState| {
        st.credits_of("a", 1).map(|c| {
            (
                c.enabled,
                c.disabled_reason.clone(),
                c.utilization,
                c.observed_at,
            )
        })
    };
    assert_eq!(credits(&st), Some((true, None, Some(0.4), 1_000)));
    assert!(
        !st.record_spend_head("a", Some(0.9), Some(8 * HOUR), 900, 1),
        "an older head moves nothing"
    );
    assert_eq!(st.spend_head("a"), Some((9 * HOUR, 1_000)));
    assert_eq!(credits(&st), Some((true, None, Some(0.4), 1_000)));
    // The poll's flag and reason stand under a newer head's share.
    st.record_credits(
        "a",
        CreditsState {
            enabled: true,
            disabled_reason: Some("member_limit".into()),
            utilization: Some(0.5),
            observed_at: 2_000,
            incarnation: 1,
        },
    );
    assert!(st.record_spend_head("a", Some(0.6), None, 3_000, 1));
    assert_eq!(
        credits(&st),
        Some((true, Some("member_limit".into()), Some(0.6), 3_000))
    );
    assert_eq!(
        st.spend_head("a"),
        Some((9 * HOUR, 1_000)),
        "a head that names no reset leaves the held one"
    );
}

/// A seat's spend at its cap also blocks its usage credits, since the selector reads the one share for both, so the
/// seat stands in both lists.
#[tokio::test]
async fn probe_holds_names_a_seat_at_its_spend_cap_under_both_holds_and_a_blocked_account_under_credits_alone(
) {
    let store = seeded_oauth(&["a", "b", "c"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(spend_head(1.0, 30 * HOUR, 1_000)).await;
    handle
        .report(Report::Credits {
            account_id: "b".into(),
            state: CreditsState {
                enabled: false,
                disabled_reason: Some("org_level_disabled".into()),
                utilization: None,
                observed_at: 1_000,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    let holds = handle.probe_holds().await.unwrap();
    assert_eq!(holds.credits, ["a", "b"]);
    assert_eq!(holds.spent, ["a"]);
}

#[tokio::test]
async fn two_pending_spend_heads_of_one_account_fold_to_the_newer() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let park = handle.park_loop().await;
    // The newer reading arrives first, so only the merge rule keeps it.
    handle.report(spend_head(0.7, 30 * HOUR, 3_000)).await;
    handle.report(spend_head(0.2, 30 * HOUR, 2_000)).await;
    drop(park);
    let row = credits_row_reads(&store, 0.7).await;
    assert_eq!(row.observed_at, 3_000);
}

#[tokio::test]
async fn a_head_with_the_polls_window_names_does_not_replace_the_pending_poll() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(3_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let park = handle.park_loop().await;
    // The head is the newer, so a slot both shared would keep it and lose the set the poll settles.
    handle.report(polled(2_000, &[SEVEN_DAY])).await;
    handle
        .report(observed(
            "a",
            sample(3_000).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.4,
                    reset: None,
                    observed_at: 3_000,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    drop(park);
    let sets = wait_for(WRITTEN, || {
        let sets = store.load_routing().unwrap().window_sets;
        (!sets.is_empty()).then_some(sets)
    })
    .await;
    assert_eq!(
        sets,
        [set_row(SEVEN_DAY, 2_000)],
        "the poll drained beside the newer head and settled the set"
    );
}

#[tokio::test]
async fn a_pending_reset_only_spend_head_keeps_an_older_pending_heads_share_at_the_cap() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(credits_at(true, None, Some(50.0), 1_000))
        .await;
    credits_row_reads(&store, 0.5).await;
    let park = handle.park_loop().await;
    handle.report(spend_head(1.0, 30 * HOUR, 2_000)).await;
    handle
        .report(Report::SpendHead {
            account_id: "a".into(),
            utilization: None,
            reset: Some(30 * HOUR),
            observed_at: 3_000,
            incarnation: 1,
        })
        .await;
    drop(park);
    let row = credits_row_reads(&store, 1.0).await;
    assert_eq!(
        row.observed_at, 2_000,
        "the head with a share lands beside the newer head that names the reset alone"
    );
    assert_eq!(
        handle
            .select(request(&handle, None, 1_000))
            .await
            .unwrap()
            .result,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 30 * HOUR,
            window: "spend".into()
        }),
        "the seat the upstream reported at its cap is held out until the reset"
    );
}

/// 2026-10-10T00:00:00Z, 2026-10-15T00:00:00Z, 2026-11-01T00:00:00Z and 2026-12-01T00:00:00Z in epoch milliseconds.
const OCT_10: i64 = 1_791_590_400_000;
const OCT_15: i64 = 1_792_022_400_000;
const NOV_1: i64 = 1_793_491_200_000;
const DEC_1: i64 = 1_796_083_200_000;

/// The seat's one spend window as the handle answers it.
async fn spend_window_of(handle: &RouterHandle) -> SpendWindow {
    let windows = handle.spend_windows().await.unwrap();
    assert_eq!(
        windows
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["a"],
        "the usage-based seat alone has a spend window: {windows:?}"
    );
    windows.into_iter().next().unwrap().1
}

#[tokio::test]
async fn a_spend_windows_reset_is_the_heads_while_ahead_then_rolls_to_the_next_months_first_and_the_share_read_under_it_lapses(
) {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(OCT_10));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: None,
            share_source: None,
            reset: NOV_1,
            reset_source: SpendResetSource::Estimated,
            observed_at: None,
        },
        "before any reading the window is the estimate with no share"
    );
    // A head that names the 15th, apart from the estimate's first of the month, replaces it.
    handle.report(spend_head(0.6, OCT_15, OCT_10)).await;
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: Some(0.6),
            share_source: Some("head"),
            reset: OCT_15,
            reset_source: SpendResetSource::Head,
            observed_at: Some(OCT_10),
        },
        "the head's share and its reset while that is ahead"
    );
    now.store(OCT_15 + HOUR, Ordering::SeqCst);
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: None,
            share_source: Some("head"),
            reset: NOV_1,
            reset_source: SpendResetSource::Estimated,
            observed_at: Some(OCT_10),
        },
        "past the head's reset with no newer head the window rolls to the estimate and the share read under it \
         lapses"
    );
    handle
        .report(credits_at(true, None, Some(30.0), OCT_15 + HOUR))
        .await;
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: Some(0.3),
            share_source: Some("poll"),
            reset: NOV_1,
            reset_source: SpendResetSource::Estimated,
            observed_at: Some(OCT_15 + HOUR),
        },
        "a newer reading keeps a share, read under the estimate"
    );
}

#[tokio::test]
async fn a_polls_share_after_a_head_whose_reset_the_router_holds_lapses_at_that_reset() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(OCT_10 + HOUR));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(spend_head(0.6, OCT_15, OCT_10)).await;
    // Each read folds the readings pending before it, so the head lands before the poll whatever their keys' order.
    spend_window_of(&handle).await;
    handle
        .report(credits_at(true, None, Some(30.0), OCT_10 + HOUR))
        .await;
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: Some(0.3),
            share_source: Some("poll"),
            reset: OCT_15,
            reset_source: SpendResetSource::Head,
            observed_at: Some(OCT_10 + HOUR),
        },
        "the poll's share stands under the head's reset while that is ahead"
    );
    now.store(OCT_15 + HOUR, Ordering::SeqCst);
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: None,
            share_source: Some("poll"),
            reset: NOV_1,
            reset_source: SpendResetSource::Estimated,
            observed_at: Some(OCT_10 + HOUR),
        },
        "the poll was read under the head's reset, so its share lapses there and not at the estimate over its instant"
    );
}

#[tokio::test]
async fn a_share_only_heads_share_lapses_at_the_reset_an_earlier_head_named() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(OCT_10 + HOUR));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(spend_head(0.6, OCT_15, OCT_10)).await;
    spend_window_of(&handle).await;
    handle
        .report(Report::SpendHead {
            account_id: "a".into(),
            utilization: Some(0.8),
            reset: None,
            observed_at: OCT_10 + HOUR,
            incarnation: 1,
        })
        .await;
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: Some(0.8),
            share_source: Some("head"),
            reset: OCT_15,
            reset_source: SpendResetSource::Head,
            observed_at: Some(OCT_10 + HOUR),
        },
        "the share-only head's share stands under the earlier head's reset"
    );
    now.store(OCT_15 + HOUR, Ordering::SeqCst);
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: None,
            share_source: Some("head"),
            reset: NOV_1,
            reset_source: SpendResetSource::Estimated,
            observed_at: Some(OCT_10 + HOUR),
        },
        "the share-only head names no reset of its own and reads the held one, so its share lapses there"
    );
}

#[tokio::test]
async fn a_head_observed_after_a_poll_leaves_the_polls_share_to_the_estimate_over_its_instant() {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(OCT_10 + HOUR));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // A head carrying a share would hold a credits state newer than the slower poll's, which would then fold nothing.
    handle
        .report(Report::SpendHead {
            account_id: "a".into(),
            utilization: None,
            reset: Some(OCT_15),
            observed_at: OCT_10 + HOUR,
            incarnation: 1,
        })
        .await;
    spend_window_of(&handle).await;
    handle
        .report(credits_at(true, None, Some(30.0), OCT_10))
        .await;
    now.store(OCT_15 + HOUR, Ordering::SeqCst);
    assert_eq!(
        spend_window_of(&handle).await,
        SpendWindow {
            utilization: Some(0.3),
            share_source: Some("poll"),
            reset: NOV_1,
            reset_source: SpendResetSource::Estimated,
            observed_at: Some(OCT_10),
        },
        "the head came after the poll's reading, so its reset does not govern the poll's share"
    );
    now.store(NOV_1 + HOUR, Ordering::SeqCst);
    assert_eq!(
        spend_window_of(&handle).await.utilization,
        None,
        "the poll's share holds to the estimate over its own instant"
    );
}

/// What `handle` answers a plain request and a fast one at `at`.
async fn standard_and_fast(handle: &RouterHandle, at: i64) -> (SelectResult, SelectResult) {
    let fast = SelectRequest {
        fast: true,
        lane: lane_key("claude-sonnet-5", "fast", "none", "-"),
        ..request(handle, None, at)
    };
    (
        handle
            .select(request(handle, None, at))
            .await
            .unwrap()
            .result,
        handle.select(fast).await.unwrap().result,
    )
}

/// A seat's share of one, its poll fetched at `persisted_at`, persisted before a shutdown and read by a task spawned
/// six hours into November: the spend window, then the answers to a plain request and a fast one.
async fn a_share_at_one_after_a_restart(
    persisted_at: i64,
) -> (SpendWindow, SelectResult, SelectResult) {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(persisted_at));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(credits_at(true, None, Some(100.0), persisted_at))
        .await;
    credits_row_reads(&store, 1.0).await;
    handle.shutdown().await.unwrap();
    now.store(NOV_1 + 6 * HOUR, Ordering::SeqCst);
    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let window = spend_window_of(&again).await;
    let (standard, fast) = standard_and_fast(&again, NOV_1 + 6 * HOUR).await;
    (window, standard, fast)
}

#[tokio::test]
async fn a_share_at_one_persisted_in_october_lapses_after_a_restart_on_the_first_and_one_persisted_on_the_first_keeps_holding(
) {
    let (window, standard, fast) = a_share_at_one_after_a_restart(NOV_1 - HOUR).await;
    let answered = |result: &SelectResult| match result {
        SelectResult::Selected(selection) => format!("selected {}", selection.account.id),
        other => format!("{other:?}"),
    };
    assert_eq!(
        (
            window.utilization,
            window.share_source,
            window.reset,
            answered(&standard),
            answered(&fast)
        ),
        (None, None, DEC_1, "selected a".into(), "selected a".into()),
        "October's share lapsed at the first, read under the estimate over its own instant, so it holds nothing, \
         the fast lane's credits read included"
    );

    let (window, standard, fast) = a_share_at_one_after_a_restart(NOV_1 + 1_000).await;
    assert_eq!(
        (window.utilization, window.share_source),
        (Some(1.0), None),
        "the share persisted this month stands, with no source the row could keep"
    );
    assert_eq!(
        (standard, fast),
        (
            SelectResult::Exhausted(Exhaustion::Timed {
                deadline: DEC_1,
                window: "spend".into()
            }),
            SelectResult::Exhausted(Exhaustion::FastCredits {
                reason: "org_spend_cap_reached".into()
            })
        ),
        "it holds the seat out until the next month's first, the fast lane's credits read alike"
    );
}

/// A seat body whose `spend.percent` rounds its extra usage's utilization, so the two fields answer different shares.
const UNEQUAL_SEAT_USAGE: &[u8] = br#"{"extra_usage":{"is_enabled":true,"monthly_limit":250000,"used_credits":103125.0,"utilization":41.25,"currency":"USD","decimal_places":2},"spend":{"used":{"amount_minor":103125,"currency":"USD","exponent":2},"limit":{"amount_minor":250000,"currency":"USD","exponent":2},"percent":41}}"#;

#[tokio::test]
async fn a_seats_poll_reads_the_share_from_extra_usage_utilization_and_not_from_the_rounded_spend_percent(
) {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(OCT_10));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let phoebis_oauth::ParsedUsage {
        windows,
        fable_window,
        extra_usage,
        spend,
        raw_keys,
        fable_week_share,
        ..
    } = phoebis_oauth::parse_usage(UNEQUAL_SEAT_USAGE).unwrap();
    let snapshot = UsageSnapshot {
        account_id: "a".into(),
        identity_generation: 1,
        fetched_at: OCT_10,
        windows,
        fable_window,
        extra_usage,
        raw_keys,
        error: None,
        spend,
        fable_week_share,
    };
    handle
        .report(Report::Credits {
            account_id: "a".into(),
            state: credits_from_usage(&snapshot, 1).expect("the body carries extra usage"),
            incarnation: 1,
        })
        .await;
    let share = spend_window_of(&handle)
        .await
        .utilization
        .expect("the poll set a share");
    // `spend.percent` rounds the 41.25 percent to 41, so a reading of it would answer 0.41.
    assert!(
        (share - 0.4125).abs() < 1e-9,
        "the share is extra usage's utilization over a hundred, the one figure the view reads too: {share}"
    );
}

#[tokio::test]
async fn a_poll_at_one_after_a_lapsed_share_reads_the_credits_edge_from_no_block() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let now = Arc::new(AtomicI64::new(OCT_10));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(credits_at(true, None, Some(100.0), OCT_10))
        .await;
    assert!(
        matches!(
            next_event(&mut events).await,
            RouterEvent::FastCreditsChanged { .. }
        ),
        "the first poll at one blocks the fast lane"
    );
    // Past the first the October share has lapsed, so the selector reads no block before the next poll lands.
    now.store(NOV_1 + HOUR, Ordering::SeqCst);
    handle
        .report(credits_at(true, None, Some(100.0), NOV_1 + HOUR))
        .await;
    match next_event(&mut events).await {
        RouterEvent::FastCreditsChanged {
            blocked_before,
            blocked_after,
            ..
        } => assert_eq!(
            (blocked_before, blocked_after.as_deref()),
            (None, Some("org_spend_cap_reached")),
            "the edge runs from the lapsed share the selector read to the new cap"
        ),
        other => panic!("one credits change, not {other:?}"),
    }
}

#[tokio::test]
async fn a_usage_based_seat_starts_out_of_overage_over_a_stored_in_use_row_and_records_none_from_a_head(
) {
    let store = seeded_oauth(&["a"]);
    usage_based_seat(&store, "a");
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.overage_policy = OveragePolicy::Never;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    // The row a head left before the class existed, its reset a day out.
    store
        .upsert_overage(&OverageRow {
            account_id: "a".into(),
            in_use: true,
            since: Some(500),
            reset: Some(25 * HOUR),
            observed_at: 500,
            source: phoebis_routing::EvidenceSource::Head,
            incarnation: 1,
        })
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        handle.overage("a").await.unwrap(),
        None,
        "the start drops the seat's stored evidence"
    );
    let placed = |answer: SelectAnswer| match answer.result {
        SelectResult::Selected(selection) => (selection.account.id, selection.in_overage),
        other => panic!("the seat serves under never, not {other:?}"),
    };
    assert_eq!(
        placed(
            handle
                .select(request(&handle, Some("s1"), 1_000))
                .await
                .unwrap()
        ),
        ("a".into(), false),
        "no never hold reads the seat and its pin reads no overage"
    );
    handle.reload().await.unwrap();
    assert_eq!(
        handle.overage("a").await.unwrap(),
        None,
        "a reload keeps it so"
    );
    handle
        .report(Report::Overage {
            account_id: "a".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: Some(2_000),
                reset: Some(25 * HOUR),
                observed_at: 2_000,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    assert_eq!(
        handle.overage("a").await.unwrap(),
        None,
        "a leg that reported the overage family as evidence records nothing"
    );
    assert_eq!(
        placed(
            handle
                .select(request(&handle, Some("s1"), 3_000))
                .await
                .unwrap()
        ),
        ("a".into(), false)
    );
    assert_eq!(
        store
            .load_routing()
            .unwrap()
            .overage
            .iter()
            .map(|row| (row.account_id.as_str(), row.observed_at))
            .collect::<Vec<_>>(),
        [("a", 500)],
        "the registry's row stays as written"
    );
}

#[tokio::test]
async fn a_usage_based_seat_under_a_priced_tier_name_is_charged_no_plan_window_share() {
    let store = seeded_oauth(&["a"]);
    store
        .set_profile(
            "a",
            r#"{"organization":{"rate_limit_tier":"default_claude_max_5x","seat_tier":"enterprise_usage_based"}}"#,
            &SubscriptionRow {
                tier: Some(TIER.into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(3_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(10 * HOUR)),
        ))
        .await;
    handle.report(a_million_input("a", 2_000)).await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.5),
        "the seat's spend is billed in dollars, so its usage charges no plan window whatever tier its profile names"
    );
}

#[tokio::test]
async fn a_siblings_fresh_placement_in_the_window_never_consumes_another_agents_invalidation() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let main = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let a = match &main.result {
        SelectResult::Selected(s) => s.account.id.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(next_decision(&mut events).await.0.kind, "placed");
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    let pin = match &main.result {
        SelectResult::Selected(s) => s.pin.clone().unwrap(),
        _ => unreachable!(),
    };
    handle
        .report(Report::Cooldown {
            account_id: a.clone(),
            row: CooldownRow {
                account_id: a.clone(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    // A sibling that never held a pin places fresh inside the window the main agent's invalidation opened;
    // the two agents' entries must not share one slot in the migration-window cache.
    let sibling = handle
        .select(SelectRequest {
            agent_id: "worker".into(),
            ..request(&handle, Some("s1"), 1_050)
        })
        .await
        .unwrap();
    assert!(matches!(&sibling.result, SelectResult::Selected(_)));
    let (placed, _) = next_decision(&mut events).await;
    assert_eq!(
        (placed.kind, placed.rule),
        ("placed", None),
        "the sibling's placement takes no other agent's move"
    );
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { pin: PinKey { agent_id, .. }, .. } if agent_id == "worker"
    ));
    let main_next = handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = &main_next.result else {
        panic!("{:?}", main_next.result)
    };
    assert_eq!(sel.trace.moved, Some("cooldown"));
    let (placed, _) = next_decision(&mut events).await;
    assert_eq!((placed.kind, placed.rule), ("placed", Some("cooldown")));
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinMigrated { pin: PinKey { agent_id, .. }, reason, .. } if agent_id == "main" && reason == "cooldown"
    ));
}

#[tokio::test]
async fn a_probe_after_a_compacted_request_refreshes_warmth() {
    use phoebis_routing::{warm, Report};
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let compacted = |now: i64, probe: bool| SelectRequest {
        probe,
        request_class: Some("main".into()),
        context_compacted: true,
        static_boundaries: Arc::from(["s0".to_string()]),
        boundaries: Arc::from(["c1".to_string()]),
        ..request(&handle, Some("s1"), now)
    };
    handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    handle.select(compacted(2_000, false)).await.unwrap();
    let pin = handle.pins().await.unwrap().remove(0);
    assert_eq!(
        pin.last_refresh_at, 0,
        "the client's compacted request colds the pin"
    );
    let answer = handle.select(compacted(3_000, true)).await.unwrap();
    let SelectResult::Selected(selection) = answer.result else {
        panic!("the probe renews the pin")
    };
    assert_eq!(
        handle.pins().await.unwrap()[0].last_refresh_at,
        0,
        "the probe replays the compacted headers and chain, and selection judges none of it"
    );
    handle
        .report(Report::PinRenewed {
            pin: selection.pin.expect("the probe held the pin"),
            now: 3_500,
            usage: None,
            keepalive: true,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::NoTurn,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    let pin = handle.pins().await.unwrap().remove(0);
    assert_eq!(
        pin.last_refresh_at, 3_500,
        "the probe's renewal counts as a refresh"
    );
    assert!(warm(&pin, 3_600));
}

/// A dollar table weighing every token a quarter beside a unit share table, so a fold that priced anything
/// would read a quarter of the tokens on one side.
fn quarter_dollar_rates() -> BurnRates {
    BurnRates {
        dollars: flat_rates(0.25),
        five_hour: RegimeRates {
            base: RateTable::unit(),
            ..RegimeRates::unit()
        },
        seven_day: RegimeRates::unit(),
        fast: RateTable::unit(),
    }
}

/// Every token of every model at `rate`.
fn flat_rates(rate: f64) -> RateTable {
    let mut table = RateTable::unit();
    table.fallback = Rates::flat(rate);
    table
}

fn renewal(pin: PinRef, now: i64, usage: PinUsage, keepalive: bool, cache_ttl_ms: i64) -> Report {
    Report::PinRenewed {
        pin,
        now,
        usage: Some(usage),
        keepalive,
        cache_ttl_ms,
        fast: false,
        turn_end: if keepalive {
            RenewalEnd::NoTurn
        } else {
            RenewalEnd::Turn
        },
        thinking_tokens: None,
        input_transformations: None,
    }
}

fn selected_pin(answer: phoebis_routing::SelectAnswer) -> PinRef {
    match answer.result {
        SelectResult::Selected(s) => s.pin.expect("a session request holds a pin"),
        other => panic!("{other:?}"),
    }
}

/// Session "s1"'s main turn at `now`, standard or fast, its lane naming the speed.
fn turn(handle: &RouterHandle, fast: bool, now: i64) -> SelectRequest {
    let speed = if fast { "fast" } else { "standard" };
    SelectRequest {
        request_class: Some("main".into()),
        fast,
        lane: lane_key("claude-sonnet-5", speed, "none", "-"),
        ..request(handle, Some("s1"), now)
    }
}

#[tokio::test]
async fn a_probe_renewal_from_the_lane_the_session_left_leaves_the_switched_pins_warmth_alone() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    selected_pin(handle.select(turn(&handle, false, 1_000)).await.unwrap());
    let probe = selected_pin(
        handle
            .select(SelectRequest {
                probe: true,
                ..turn(&handle, false, 2_000)
            })
            .await
            .unwrap(),
    );
    // The session switches to fast mode while the probe is in flight: the pin reads cold on its new lane.
    selected_pin(handle.select(turn(&handle, true, 3_000)).await.unwrap());
    handle
        .report(renewal(probe, 4_000, PinUsage::default(), true, 300_000))
        .await;
    let pins = handle.pins().await.unwrap();
    assert_eq!(
        pins.iter()
            .map(|p| (p.lane.as_str(), p.last_refresh_at))
            .collect::<Vec<_>>(),
        [(lane_key("claude-sonnet-5", "fast", "none", "-").as_str(), 0)],
        "the probe refreshed the standard lane's cache, which is not the one the pin now holds"
    );
}

#[tokio::test]
async fn a_late_fast_429_from_the_lane_the_session_left_never_deletes_its_standard_pin() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let fast = selected_pin(handle.select(turn(&handle, true, 1_000)).await.unwrap());
    let account = handle.pins().await.unwrap()[0].account_id.clone();
    // The session's next turn is standard, and the fast turn's refusal lands after it.
    selected_pin(handle.select(turn(&handle, false, 2_000)).await.unwrap());
    handle
        .report(Report::Cooldown {
            account_id: account.clone(),
            row: CooldownRow {
                account_id: account.clone(),
                scope: CooldownScope::Fast,
                until: 1_000 + 600_000,
                set_at: 0,
                reason: FAST_RATE_LIMIT_REASON.into(),
                window: "fast".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(fast),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert_eq!(fast_rows(&handle).await, [FAST_RATE_LIMIT_REASON]);
    let pins = handle.pins().await.unwrap();
    assert_eq!(
        pins.iter()
            .map(|p| (p.account_id.as_str(), p.lane.as_str()))
            .collect::<Vec<_>>(),
        [(
            account.as_str(),
            lane_key("claude-sonnet-5", "standard", "none", "-").as_str()
        )],
        "the fast row holds no standard request, so the standard pin stays"
    );
}

#[tokio::test]
async fn a_cooldown_reported_with_a_bound_pins_ref_leaves_the_pin() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let pin = selected_pin(
        handle
            .select(SelectRequest {
                portability: Portability::Hold,
                ..turn(&handle, false, 1_000)
            })
            .await
            .unwrap(),
    );
    let before = handle.pins().await.unwrap();
    assert_eq!(
        before.iter().map(|p| p.account_bound).collect::<Vec<_>>(),
        [true],
        "the non-portable turn's row carries the flag"
    );
    let account = before[0].account_id.clone();
    // The relay hands the pin's ref with the refusal of a portable request that fails over.
    handle
        .report(Report::Cooldown {
            account_id: account.clone(),
            row: CooldownRow {
                account_id: account.clone(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert_eq!(
        handle
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .map(|row| (row.account_id.as_str(), row.until))
            .collect::<Vec<_>>(),
        [(account.as_str(), 1_000 + HOUR)],
        "the cooldown stands"
    );
    assert_eq!(
        handle.pins().await.unwrap(),
        before,
        "the row stays, so the session's retry finds its pin and no landing moves it"
    );
}

#[tokio::test]
async fn a_renewal_with_usage_folds_the_typed_rates_and_the_banded_copy_and_a_probes_usage_never_counts(
) {
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let config = RouterConfig {
        rates: quarter_dollar_rates(),
        ..RouterConfig::default()
    };
    let handle = spawn(store, clock(&now), config).unwrap();
    let client = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    let probe = selected_pin(
        handle
            .select(request(&handle, Some("s2"), 1_000))
            .await
            .unwrap(),
    );
    let unmetered = selected_pin(
        handle
            .select(request(&handle, Some("s3"), 1_000))
            .await
            .unwrap(),
    );
    let first = PinUsage {
        input: 100,
        cache_read: 1_000,
        write_5m: 50,
        write_1h: 30,
        output: 20,
    };
    let second = PinUsage {
        input: 500,
        ..PinUsage::default()
    };
    handle
        .report(renewal(client.clone(), 2_000, first, false, 300_000))
        .await;
    handle
        .report(renewal(client, 62_000, second, false, 300_000))
        .await;
    handle
        .report(renewal(probe, 2_000, first, true, 300_000))
        .await;
    handle
        .report(Report::PinRenewed {
            pin: unmetered,
            now: 2_000,
            usage: None,
            keepalive: false,
            cache_ttl_ms: 3_600_000,
            fast: false,
            turn_end: RenewalEnd::Turn,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    let pins = handle.pins().await.unwrap();
    // The pool's burn average is 600 s, and the second fold lands 60 s after the first: each type's first tokens over
    // 600 s keep e^-0.1 (0.904 837 418 036) of their rate, and the 500 input tokens of the second add 500 / 600.
    let expected = PinBurn {
        cache_read: 1.508_062_363_393,
        input: 0.984_139_569_673,
        output: 0.030_161_247_268,
        write_1h: 0.045_241_870_902,
        write_5m: 0.075_403_118_170,
    };
    let folded = pins.iter().find(|p| p.session_id == "s1").unwrap();
    let close = |a: PinBurn, b: PinBurn| {
        [
            a.cache_read - b.cache_read,
            a.input - b.input,
            a.output - b.output,
            a.write_1h - b.write_1h,
            a.write_5m - b.write_5m,
        ]
        .iter()
        .all(|d| d.abs() < 1e-9)
    };
    assert!(
        close(folded.burn, expected),
        "each type decays by exp(-dt/window) and adds its own tokens over the window, unpriced: {:?} against \
         {expected:?}",
        folded.burn
    );
    assert!(
        close(folded.burn_banded, expected),
        "outside the band the banded copy is the plain one: {:?}",
        folded.burn_banded
    );
    assert_eq!(
        (folded.burn_at, folded.last_input_tokens),
        (Some(62_000), 500)
    );
    let probed = pins.iter().find(|p| p.session_id == "s2").unwrap();
    assert_eq!(
        (probed.burn, probed.burn_banded, probed.burn_at),
        (PinBurn::default(), PinBurn::default(), None),
        "a probe's usage never counts"
    );
    let bare = pins.iter().find(|p| p.session_id == "s3").unwrap();
    assert_eq!(
        (bare.cache_ttl_ms, bare.last_used_at),
        (3_600_000, 2_000),
        "a client report without usage still lands the TTL the relay resolved"
    );
    assert_eq!(
        (
            bare.burn,
            bare.burn_banded,
            bare.burn_at,
            bare.last_input_tokens
        ),
        (PinBurn::default(), PinBurn::default(), None, 0),
        "and folds nothing"
    );
}

#[tokio::test]
async fn a_fast_renewal_lands_in_the_pins_fast_usage_and_lets_both_copies_of_its_row_decay() {
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    // The fast cells read twice the dollar ones, so the fast tokens price at a half beside the row's quarter.
    let rates = BurnRates {
        fast: flat_rates(0.5),
        ..quarter_dollar_rates()
    };
    let config = RouterConfig {
        rates: rates.clone(),
        ..RouterConfig::default()
    };
    let handle = spawn(store, clock(&now), config).unwrap();
    let fast = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    let standard = selected_pin(
        handle
            .select(request(&handle, Some("s2"), 1_000))
            .await
            .unwrap(),
    );
    let input = |tokens: u64| PinUsage {
        input: tokens,
        ..PinUsage::default()
    };
    handle
        .report(renewal(fast.clone(), 2_000, input(600), false, 300_000))
        .await;
    handle
        .report(Report::PinRenewed {
            pin: fast,
            now: 62_000,
            usage: Some(input(1_000)),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: true,
            turn_end: RenewalEnd::Turn,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    handle
        .report(renewal(standard, 62_000, input(1_000), false, 300_000))
        .await;
    let pins = handle.pins().await.unwrap();
    let fast_burns = handle.fast_burns().await.unwrap();
    // The pool's burn average is 600 s, and the fast fold lands 60 s after the standard one, so the row's 600 tokens
    // over 600 s keep e^-0.1 of their rate of 1.
    let window = 600.0_f64;
    let decayed = 0.904_837_418_036;
    let s1 = pins.iter().find(|p| p.session_id == "s1").unwrap();
    assert!(
        (s1.burn.input - decayed).abs() < 1e-9 && (s1.burn_banded.input - decayed).abs() < 1e-9,
        "the row's two copies only decay at a fast renewal: {:?} and {:?}",
        s1.burn,
        s1.burn_banded
    );
    let fast = fast_burns[&s1.key()];
    assert!(
        (fast.input - 1_000.0 / window).abs() < 1e-9,
        "the thousand fast tokens land in the pin's fast usage, unweighed: {fast:?}"
    );
    assert_eq!((s1.burn_at, s1.last_input_tokens), (Some(62_000), 1_000));
    let priced = rates.price_pin(BurnUnit::Usd, s1, FIVE_HOUR).unwrap();
    // 0.904 837 418 036 x 0.25 + 1 000 / 600 x 0.5.
    assert!(
        (priced - 1.059_542_687_842).abs() < 1e-9,
        "priced, the row's tokens at the quarter and the fast tokens at the half: {priced}"
    );
    let s2 = pins.iter().find(|p| p.session_id == "s2").unwrap();
    assert_eq!(
        (s2.burn.input, s2.burn_banded.input),
        (1_000.0 / window, 1_000.0 / window),
        "a standard renewal folds its tokens unweighed into both copies"
    );
    assert!(
        !fast_burns.contains_key(&s2.key()),
        "and leaves no fast usage behind"
    );
}

#[tokio::test]
async fn a_fast_renewal_is_written_with_the_pin_and_a_restart_reads_its_fast_usage_as_saved() {
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let (key, saved) = {
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        let pin = selected_pin(
            handle
                .select(request(&handle, Some("s1"), 1_000))
                .await
                .unwrap(),
        );
        handle.report(speed_renewal(pin, 2_000, true, 600)).await;
        // The renewal's `burn_at` reaches the store in the same row as its fast copy, so the wait keys on it.
        wait_for(WRITTEN, || {
            store
                .load_routing()
                .unwrap()
                .pins
                .into_iter()
                .find(|p| p.burn_at == Some(2_000))
        })
        .await;
        let (held, fast) = pin_and_fast_usage(&handle).await;
        assert!(
            (fast.input - 1.0).abs() < 1e-9,
            "the 600 fast tokens over the pool's 600 s burn average: {fast:?}"
        );
        (held.key(), fast)
    };
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        handle.fast_burns().await.unwrap().get(&key).copied(),
        Some(saved),
        "a restart reads the pin's fast usage as saved"
    );
    assert_eq!(
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .map(|p| p.burn_fast)
            .collect::<Vec<_>>(),
        vec![saved],
        "the stored row carries the fast usage the state held, never a fold behind it"
    );
}

/// Session "s1"'s classifier call at `now`, as the client sends it: an auxiliary call on Sonnet 5 under `main`.
fn classifier_call(handle: &RouterHandle, now: i64) -> SelectRequest {
    SelectRequest {
        classifier: true,
        request_class: Some("auxiliary".into()),
        ..request(handle, Some("s1"), now)
    }
}

/// The pin the classifier call's selection judged.
fn judged_pin(answer: phoebis_routing::SelectAnswer) -> PinRef {
    match answer.result {
        SelectResult::Selected(s) => s
            .judged
            .and_then(|judged| judged.pin)
            .expect("a classifier call in a session with a turn judges its pin"),
        other => panic!("{other:?}"),
    }
}

/// A Sonnet 5 classifier call's usage on `account_id` at `at`, `input` uncached tokens as its whole prompt, free of a
/// plan window under `On`, with the pin its selection judged.
fn classifier_usage(
    account_id: &str,
    at: i64,
    input: u64,
    charge: JudgedCharge,
    pin: Option<PinRef>,
) -> Report {
    Report::Usage {
        account_id: account_id.into(),
        at,
        canonical_model: "claude-sonnet-5".into(),
        incarnation: 1,
        usage: PinUsage {
            input,
            ..PinUsage::default()
        },
        fast: false,
        judged: Some(Judged { charge, pin }),
        request_id: "r-classifier".into(),
        attempt: 1,
        entrypoint: None,
    }
}

#[tokio::test]
async fn a_classifier_call_on_its_judged_pins_account_folds_into_the_pins_classifier_copy_and_one_elsewhere_folds_nothing(
) {
    let store = averaged(seeded(&["a", "b"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let rates = quarter_dollar_rates();
    let config = RouterConfig {
        rates: rates.clone(),
        ..RouterConfig::default()
    };
    let handle = spawn(store, clock(&now), config).unwrap();
    let pin = selected_pin(
        handle
            .select(SelectRequest {
                attempted: vec!["b".into()],
                ..turn(&handle, false, 1_000)
            })
            .await
            .unwrap(),
    );
    handle
        .report(speed_renewal(pin.clone(), 2_000, false, 600))
        .await;
    let judged = judged_pin(
        handle
            .select(classifier_call(&handle, 61_000))
            .await
            .unwrap(),
    );
    handle
        .report(classifier_usage(
            "a",
            62_000,
            1_000,
            JudgedCharge::On,
            Some(judged.clone()),
        ))
        .await;
    let (s1, _) = pin_and_fast_usage(&handle).await;
    // The pool's burn average is 600 s, and the classifier's fold lands 60 s after the standard one, so the row's 600
    // tokens over 600 s keep e^-0.1 of their rate of 1.
    let window = 600.0_f64;
    let decayed = 0.904_837_418_036;
    assert!(
        (s1.burn.input - decayed).abs() < 1e-9 && (s1.burn_banded.input - decayed).abs() < 1e-9,
        "the row's plain and banded copies only decay at a classifier call's fold: {:?} and {:?}",
        s1.burn,
        s1.burn_banded
    );
    assert_eq!(
        s1.burn_fast,
        PinBurn::default(),
        "and the fast copy holds nothing"
    );
    assert!(
        (s1.burn_unmetered.input - 1_000.0 / window).abs() < 1e-9,
        "the thousand classifier tokens land in the pin's classifier copy, unweighed: {:?}",
        s1.burn_unmetered
    );
    assert_eq!(
        (s1.burn_at, s1.burn_unmetered_model.as_deref()),
        (Some(62_000), Some("claude-sonnet-5")),
        "the copy names the call's model"
    );
    let share = rates
        .price_pin(BurnUnit::WindowShare, &s1, FIVE_HOUR)
        .unwrap();
    assert!(
        (share - decayed).abs() < 1e-9,
        "under the window share the row's banded copy alone, so the call adds nothing to the plan's figure: {share}"
    );
    let dollars = rates.price_pin(BurnUnit::Usd, &s1, FIVE_HOUR).unwrap();
    // (0.904 837 418 036 + 1 000 / 600) x 0.25.
    assert!(
        (dollars - 0.642_876_021_176).abs() < 1e-9,
        "in dollars the classifier tokens price at the standard quarter beside the row's: {dollars}"
    );
    // The same judged pin's call ran on b, off the pin's account: the copy stays as the call on a left it.
    handle
        .report(classifier_usage(
            "b",
            62_000,
            1_000,
            JudgedCharge::Off,
            Some(judged),
        ))
        .await;
    let (held, _) = pin_and_fast_usage(&handle).await;
    assert_eq!(
        (held.burn_unmetered, held.burn_at),
        (s1.burn_unmetered, s1.burn_at),
        "a call that ran on another account folds nothing into the judged pin"
    );
    handle.report(speed_renewal(pin, 122_000, false, 600)).await;
    let (s1, _) = pin_and_fast_usage(&handle).await;
    // The thousand classifier tokens over 600 s, 60 s later at e^-0.1 of that rate.
    assert!(
        (s1.burn_unmetered.input - 1.508_062_363_393).abs() < 1e-9,
        "a standard renewal folds nothing into the classifier copy, which decays with the row: {:?}",
        s1.burn_unmetered
    );
}

#[tokio::test]
async fn a_classifier_calls_usage_folds_into_the_pin_its_selection_judged_though_another_turn_ended_since(
) {
    let store = averaged(seeded(&["a", "b"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let opus = "claude-opus-5-5";
    handle
        .select(agent_turn(
            &handle,
            "s1",
            "main",
            "main",
            opus,
            &["b"],
            1_000,
        ))
        .await
        .unwrap();
    let x = selected_pin(
        handle
            .select(agent_turn(
                &handle,
                "s1",
                "x",
                "subagent",
                opus,
                &["a"],
                2_000,
            ))
            .await
            .unwrap(),
    );
    let y = selected_pin(
        handle
            .select(agent_turn(
                &handle,
                "s1",
                "y",
                "subagent",
                opus,
                &["a"],
                2_500,
            ))
            .await
            .unwrap(),
    );
    let usage = PinUsage {
        input: 10,
        output: 5,
        ..PinUsage::default()
    };
    // x's turn ends on b, and the classifier call that judges it is selected there.
    handle
        .report(renewal(x, 3_000, usage, false, 300_000))
        .await;
    let answer = handle
        .select(classifier_call(&handle, 3_100))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = answer.result else {
        panic!("{:?}", answer.result);
    };
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.judged
                .as_ref()
                .and_then(|judged| judged.pin.as_ref())
                .map(|p| p.agent_id.as_str())
        ),
        ("b", Some("x"))
    );
    // y's turn ends on b too while the call is upstream, then the call's usage arrives on b.
    handle
        .report(renewal(y, 3_200, usage, false, 300_000))
        .await;
    handle
        .report(classifier_usage(
            "b",
            3_300,
            600,
            JudgedCharge::On,
            sel.judged.and_then(|judged| judged.pin),
        ))
        .await;
    let pins = handle.pins().await.unwrap();
    let copy = |agent: &str| {
        pins.iter()
            .find(|p| p.agent_id == agent)
            .map(|p| (p.burn_unmetered, p.burn_unmetered_model.clone()))
            .unwrap_or_else(|| panic!("{agent}'s pin stands: {pins:?}"))
    };
    // The 600 classifier tokens over the pool's 600 s burn average, onto an empty copy.
    assert_eq!(
        copy("x"),
        (
            PinBurn {
                input: 1.0,
                ..PinBurn::default()
            },
            Some("claude-sonnet-5".into())
        ),
        "the call's tokens fold into x's pin, the one its selection judged"
    );
    assert_eq!(
        copy("y"),
        (PinBurn::default(), None),
        "y's turn ended after the call was selected, so its pin takes nothing"
    );
}

#[tokio::test]
async fn a_classifier_calls_fold_is_written_with_the_pin_and_a_restart_reads_its_copy_and_model_as_saved(
) {
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let (key, saved) = {
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        selected_pin(handle.select(turn(&handle, false, 1_000)).await.unwrap());
        let judged = judged_pin(
            handle
                .select(classifier_call(&handle, 1_500))
                .await
                .unwrap(),
        );
        handle
            .report(classifier_usage(
                "a",
                2_000,
                600,
                JudgedCharge::On,
                Some(judged),
            ))
            .await;
        // The fold's `burn_at` reaches the store in the same row as its copy and the copy's model, so the wait keys
        // on it.
        wait_for(WRITTEN, || {
            store
                .load_routing()
                .unwrap()
                .pins
                .into_iter()
                .find(|p| p.burn_at == Some(2_000))
        })
        .await;
        let (held, _) = pin_and_fast_usage(&handle).await;
        assert!(
            (held.burn_unmetered.input - 1.0).abs() < 1e-9,
            "the 600 classifier tokens over the pool's 600 s burn average: {:?}",
            held.burn_unmetered
        );
        assert_eq!(
            held.burn_unmetered_model.as_deref(),
            Some("claude-sonnet-5")
        );
        (held.key(), (held.burn_unmetered, held.burn_unmetered_model))
    };
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.key() == key)
            .map(|p| (p.burn_unmetered, p.burn_unmetered_model)),
        Some(saved.clone()),
        "a restart reads the pin's classifier copy and its model as saved"
    );
    assert_eq!(
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .map(|p| (p.burn_unmetered, p.burn_unmetered_model))
            .collect::<Vec<_>>(),
        vec![saved],
        "the stored row carries the copy and the model the state held, never a fold behind it"
    );
}

/// Every token of every model at `rate`, with Sonnet 5's own row at `rate` and its long-context rates at `long`
/// over 1,000 prompt tokens.
fn long_context_rates(rate: f64, long: f64) -> RateTable {
    let mut table = flat_rates(rate);
    table
        .rows
        .insert("claude-sonnet-5".into(), Rates::flat(rate));
    table.long_context.insert(
        "claude-sonnet-5".into(),
        LongContextRates {
            over: 1_000,
            rates: Rates::flat(long),
        },
    );
    table
}

/// Session "s1" pinned at 1,000 and renewed fast at 2,000 with a prompt one token over the threshold, under
/// `rates`: its pin and its fast usage as the task holds them after the fold.
async fn fast_renewed_over_the_threshold(rates: &BurnRates) -> (PinRow, PinBurn) {
    let (handle, pin) = pinned_under(rates).await;
    handle.report(speed_renewal(pin, 2_000, true, 1_001)).await;
    pin_and_fast_usage(&handle).await
}

#[tokio::test]
async fn a_fast_renewal_over_the_threshold_lands_in_the_fast_usage_and_prices_at_the_fast_cells() {
    // Sonnet 5 at a quarter a token, 1 over 1,000 prompt tokens and a half at the fast cells: the fast tokens land
    // in the pin's fast usage and price at the half through the fast table, the row's copy folding nothing, so
    // no later prompt of the pin reprices them.
    let rates = both_blocks_rates();
    let (s1, fast) = fast_renewed_over_the_threshold(&rates).await;
    // The pool's burn average is 600 s.
    let window = 600.0_f64;
    assert_eq!(
        s1.burn,
        PinBurn::default(),
        "the row's plain copy folds nothing at a fast renewal"
    );
    assert!(
        (fast.input - 1_001.0 / window).abs() < 1e-9,
        "the fast tokens land in the fast usage, unweighed: {fast:?}"
    );
    assert_eq!(s1.last_input_tokens, 1_001);
    let priced = rates.price_pin(BurnUnit::Usd, &s1, FIVE_HOUR).unwrap();
    assert!(
        (priced - 1_001.0 * 0.5 / window).abs() < 1e-9,
        "priced through the fast table, the burn reads the fast dollars whatever the pin's last prompt: {priced}"
    );
}

#[tokio::test]
async fn a_fast_renewal_over_the_threshold_on_a_row_without_fast_cells_prices_at_the_long_context_dollars(
) {
    // A row without fast cells reads its own standard and long-context rates in the fast table, as the ledger
    // builds it, so its fast usage over the threshold prices at the long-context rates, as a fast answer on that
    // row prices in the ledger.
    let dollars = long_context_rates(0.25, 1.0);
    let rates = BurnRates {
        fast: dollars.clone(),
        dollars,
        five_hour: RegimeRates {
            base: RateTable::unit(),
            ..RegimeRates::unit()
        },
        seven_day: RegimeRates::unit(),
    };
    let (s1, fast) = fast_renewed_over_the_threshold(&rates).await;
    let window = 600.0_f64;
    assert!(
        (fast.input - 1_001.0 / window).abs() < 1e-9,
        "the fast tokens land in the fast usage: {fast:?}"
    );
    let priced = rates.price_pin(BurnUnit::Usd, &s1, FIVE_HOUR).unwrap();
    assert!(
        (priced - 1_001.0 / window).abs() < 1e-9,
        "priced at the long-context rates the pin's last prompt selects: {priced}"
    );
}

/// Both blocks on Sonnet 5: a quarter a token at the standard cells, 1 over 1,000 prompt tokens, and a half at the
/// fast cells whatever the prompt.
fn both_blocks_rates() -> BurnRates {
    BurnRates {
        dollars: long_context_rates(0.25, 1.0),
        five_hour: RegimeRates {
            base: RateTable::unit(),
            ..RegimeRates::unit()
        },
        seven_day: RegimeRates::unit(),
        fast: flat_rates(0.5),
    }
}

/// A client renewal of `pin` at `now`, standard or fast, with `input` uncached tokens as its whole prompt.
fn speed_renewal(pin: PinRef, now: i64, fast: bool, input: u64) -> Report {
    Report::PinRenewed {
        pin,
        now,
        usage: Some(PinUsage {
            input,
            ..PinUsage::default()
        }),
        keepalive: false,
        cache_ttl_ms: 300_000,
        fast,
        turn_end: RenewalEnd::Turn,
        thinking_tokens: None,
        input_transformations: None,
    }
}

/// Session "s1" pinned at 1,000 under `rates`: the handle and the pin's reference for the renewals that follow.
async fn pinned_under(rates: &BurnRates) -> (RouterHandle, PinRef) {
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let config = RouterConfig {
        rates: rates.clone(),
        ..RouterConfig::default()
    };
    let handle = spawn(store, clock(&now), config).unwrap();
    let pin = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    (handle, pin)
}

/// Session "s1"'s pin as the task holds it after the last fold.
async fn s1_pin(handle: &RouterHandle) -> PinRow {
    handle
        .pins()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.session_id == "s1")
        .expect("the session holds a pin")
}

/// Session "s1"'s pin and its fast usage as the task holds them after the last fold.
async fn pin_and_fast_usage(handle: &RouterHandle) -> (PinRow, PinBurn) {
    let pin = s1_pin(handle).await;
    let fast = handle
        .fast_burns()
        .await
        .unwrap()
        .get(&pin.key())
        .copied()
        .unwrap_or_default();
    (pin, fast)
}

/// One second of session "s1"'s dollar burn as the readers price it: the pin's row at the set its last prompt
/// selects plus its fast usage through the fast table.
async fn dollar_burn(handle: &RouterHandle, rates: &BurnRates) -> f64 {
    rates
        .price_pin(BurnUnit::Usd, &s1_pin(handle).await, FIVE_HOUR)
        .unwrap()
}

#[tokio::test]
async fn two_fast_renewals_around_the_threshold_price_at_the_fast_cells_whichever_side_each_prompt_fell(
) {
    let rates = both_blocks_rates();
    let (handle, pin) = pinned_under(&rates).await;
    handle
        .report(speed_renewal(pin.clone(), 2_000, true, 1_000))
        .await;
    handle.report(speed_renewal(pin, 3_000, true, 1_001)).await;
    // Over the pool's 600 s burn average, (1 000 / 600 x e^(-1/600) + 1 001 / 600) x 0.5: the first turn's tokens a
    // second later beside the second's, at the fast cells' half.
    let priced = dollar_burn(&handle, &rates).await;
    assert!(
        (priced - 1.666_112_267_876).abs() < 1e-9,
        "the fast cells never move with the prompt, so the dollar burn is a half over every fast token folded and \
         the second prompt's crossing of the threshold reprices nothing: {priced}"
    );
}

#[tokio::test]
async fn a_standard_renewal_crossing_the_threshold_each_way_reprices_the_ordinary_burn_alone() {
    let rates = both_blocks_rates();
    let (handle, pin) = pinned_under(&rates).await;
    // A fast turn at the threshold, then a standard turn one token over it: the ordinary tokens price at the
    // long-context rate and the fast tokens stay at the fast cells.
    handle
        .report(speed_renewal(pin.clone(), 2_000, true, 1_000))
        .await;
    handle
        .report(speed_renewal(pin.clone(), 3_000, false, 1_001))
        .await;
    // Over the pool's 600 s burn average: 1 001 / 600 x 1 + 1 000 / 600 x e^(-1/600) x 0.5.
    let crossed_up = dollar_burn(&handle, &rates).await;
    assert!(
        (crossed_up - 2.500_278_934_542).abs() < 1e-9,
        "crossing up, the ordinary tokens read at the long-context rate and the fast tokens at the fast cells: \
         {crossed_up}"
    );
    // A standard turn back at the threshold: the whole ordinary history reprices at the standard rate, as the
    // pin's last prompt rules, and the fast tokens still read at the fast cells.
    handle.report(speed_renewal(pin, 4_000, false, 1_000)).await;
    // (1 001 / 600 x e^(-1/600) + 1 000 / 600) x 0.25 + 1 000 / 600 x e^(-2/600) x 0.5.
    let crossed_back = dollar_burn(&handle, &rates).await;
    assert!(
        (crossed_back - 1.663_615_620_117).abs() < 1e-9,
        "crossing back, the ordinary tokens read at the standard rate and the fast tokens at the fast cells: \
         {crossed_back}"
    );
}

#[tokio::test]
async fn mixed_speed_renewals_across_the_threshold_sum_the_ordinary_burn_at_the_last_prompts_set_and_the_fast_burn_at_the_fast_cells(
) {
    let rates = both_blocks_rates();
    let (handle, pin) = pinned_under(&rates).await;
    // Standard under, fast over, standard over, fast at the threshold: the ordinary and the fast tokens each fold
    // on their own copy, every copy decaying from the same last fold.
    let turns = [
        (2_000, false, 900),
        (3_000, true, 1_001),
        (4_000, false, 1_001),
        (5_000, true, 1_000),
    ];
    for (now, is_fast, input) in turns {
        handle
            .report(speed_renewal(pin.clone(), now, is_fast, input))
            .await;
    }
    // The last prompt, 1,000 tokens, is at the threshold, so the ordinary tokens price at the standard rate. Over the
    // pool's 600 s burn average the ordinary copy holds 900 / 600 three seconds old and 1 001 / 600 one second old,
    // the fast copy 1 001 / 600 two seconds old and 1 000 / 600: 0.25 x (900 / 600 x e^(-3/600) + 1 001 / 600 x
    // e^(-1/600)) + 0.5 x (1 001 / 600 x e^(-2/600) + 1 000 / 600).
    let priced = dollar_burn(&handle, &rates).await;
    assert!(
        (priced - 2.454_242_526_661).abs() < 1e-9,
        "the ordinary tokens at the set the last prompt selects plus the fast tokens at the fast cells: {priced}"
    );
}

#[tokio::test]
async fn the_burn_lands_in_the_store_with_the_input_size_and_the_ttl() {
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let config = RouterConfig {
        rates: quarter_dollar_rates(),
        ..RouterConfig::default()
    };
    let handle = spawn(store.clone(), clock(&now), config).unwrap();
    let pin = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    let usage = PinUsage {
        input: 60,
        cache_read: 540,
        write_5m: 0,
        write_1h: 30,
        output: 12,
    };
    handle
        .report(renewal(pin, 2_000, usage, false, 3_600_000))
        .await;
    let row = wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .find(|p| p.burn_at.is_some())
    })
    .await;
    let per_second = |tokens: f64| tokens / 600.0;
    let expected = PinBurn {
        cache_read: per_second(540.0),
        input: per_second(60.0),
        output: per_second(12.0),
        write_1h: per_second(30.0),
        write_5m: 0.0,
    };
    assert_eq!(
        (row.burn, row.burn_banded),
        (expected, expected),
        "the ten columns hold each type's tokens over the window, the banded copy unweighed outside the band"
    );
    assert_eq!(
        (row.last_input_tokens, row.cache_ttl_ms),
        (630, 3_600_000),
        "the input size counts every prompt token and the TTL is the one the relay resolved"
    );
    handle.shutdown().await.unwrap();
    let again = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let reloaded = again.pins().await.unwrap().remove(0);
    assert_eq!(
        (reloaded.burn, reloaded.burn_banded),
        (row.burn, row.burn_banded),
        "a restart reads both copies back"
    );
}

#[tokio::test]
async fn a_renewal_inside_the_band_folds_a_heavier_banded_copy_and_the_same_plain_rates() {
    // Monday 2026-09-21 at 14:00 and at 20:00 UTC.
    let (inside, outside) = (1_789_999_200_000_i64, 1_790_020_800_000_i64);
    let band = BandWeight::new(|window: &str, _, at: i64| {
        let hour = at.rem_euclid(86_400_000) / 3_600_000;
        if window == FIVE_HOUR && (12..18).contains(&hour) {
            1.45
        } else {
            1.0
        }
    });
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let config = RouterConfig {
        band,
        rates: quarter_dollar_rates(),
        ..RouterConfig::default()
    };
    let handle = spawn(store, clock(&now), config).unwrap();
    let afternoon = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    let evening = selected_pin(
        handle
            .select(request(&handle, Some("s2"), 1_000))
            .await
            .unwrap(),
    );
    let usage = PinUsage {
        input: 600,
        ..PinUsage::default()
    };
    handle
        .report(renewal(afternoon, inside, usage, false, 300_000))
        .await;
    handle
        .report(renewal(evening, outside, usage, false, 300_000))
        .await;
    let pins = handle.pins().await.unwrap();
    let pin = |session: &str| pins.iter().find(|p| p.session_id == session).unwrap();
    let (afternoon, evening) = (pin("s1").burn_banded, pin("s2").burn_banded);
    assert!(
        (afternoon.input - 1.45 * evening.input).abs() < 1e-9 && evening.input > 0.0,
        "the afternoon renewal folds a banded copy 1.45 times the evening one: {} against {}",
        afternoon.input,
        evening.input
    );
    assert_eq!(
        pin("s1").burn,
        pin("s2").burn,
        "the plain rates are not weighted by the band"
    );
}

#[tokio::test]
async fn a_renewal_weighs_its_banded_copy_by_the_band_of_the_entrypoint_its_own_request_named() {
    let band = BandWeight::new(
        |_, entrypoint: Option<&str>, _| {
            if entrypoint == Some("desk") {
                1.5
            } else {
                1.0
            }
        },
    );
    let store = averaged(seeded(&["a"]));
    let now = Arc::new(AtomicI64::new(1_000));
    let config = RouterConfig {
        band,
        ..RouterConfig::default()
    };
    let handle = spawn(store, clock(&now), config).unwrap();
    let desk = selected_pin(
        handle
            .select(SelectRequest {
                entrypoint: Some("desk".into()),
                ..request(&handle, Some("s1"), 1_000)
            })
            .await
            .unwrap(),
    );
    // The session's next request, from no entrypoint, renews the pin before the first request's renewal lands.
    selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_500))
            .await
            .unwrap(),
    );
    let usage = PinUsage {
        input: 600,
        ..PinUsage::default()
    };
    handle
        .report(renewal(desk, 2_000, usage, false, 300_000))
        .await;
    let pin = handle.pins().await.unwrap().remove(0);
    // 600 input tokens over the pool's 600 s burn average fold a rate of 1, weighed 1.5 in the banded copy.
    assert_eq!((pin.burn.input, pin.burn_banded.input), (1.0, 1.5));
}

#[tokio::test]
async fn a_reports_affinity_rows_reach_the_store_and_a_restart_reloads_them() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    {
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        let report = AffinityReport {
            message_boundaries: Arc::from(["b1".to_string(), "b2".to_string()]),
            message_id: Some("msg_01".into()),
            ..affinity_report(&handle, "a", 1_000)
        };
        handle.report(Report::Affinity(report)).await;
        wait_for(WRITTEN, || {
            (!store.load_routing().unwrap().affinity.is_empty()).then_some(())
        })
        .await;
    }
    let rows = store.load_routing().unwrap().affinity;
    assert_eq!(rows.len(), 2, "the two message boundaries");
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let entry = handle
        .affinity()
        .read()
        .unwrap()
        .entry("claude-sonnet-5", "b1")
        .cloned();
    assert_eq!(
        entry
            .as_ref()
            .map(|e| (e.account_id.as_str(), e.written_at, e.depth)),
        Some(("a", 1_000, 1)),
        "the restart reloaded the map from the store"
    );
    let tail = handle
        .affinity()
        .read()
        .unwrap()
        .entry("claude-sonnet-5", "b2")
        .cloned();
    assert_eq!(
        tail.as_ref().map(|e| e.depth),
        Some(2),
        "the reloaded tail keeps the depth its row names"
    );
    assert_eq!(
        tail.as_ref().and_then(|e| e.message_id.as_deref()),
        Some("msg_01"),
        "the reloaded tail keeps the message id its row names"
    );
}

#[tokio::test]
async fn a_shutdown_persists_a_refreshed_entrys_time() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(Report::Affinity(affinity_report(&handle, "a", 1_000)))
        .await;
    wait_for(WRITTEN, || {
        (!store.load_routing().unwrap().affinity.is_empty()).then_some(())
    })
    .await;
    handle
        .report(Report::Affinity(affinity_report(&handle, "a", 2_000)))
        .await;
    // The refresh is marked dirty, not written through: the store's row keeps the first report's time until a
    // drain, and the default purge tick is a minute out, so here the shutdown is the one that drains it.
    stays_for(NOT_WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .affinity
            .into_iter()
            .find(|r| r.hash == "b1")
            .is_some_and(|r| r.written_at == 1_000)
    })
    .await;
    handle.shutdown().await.unwrap();
    let row = store
        .load_routing()
        .unwrap()
        .affinity
        .into_iter()
        .find(|r| r.hash == "b1")
        .expect("the row survived the shutdown");
    assert_eq!(
        row.written_at, 2_000,
        "the shutdown drained the refreshed row"
    );
}

#[tokio::test]
async fn an_affinity_report_takes_a_sequence_at_enqueue_and_the_wait_sees_it_applied() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        handle
            .report(Report::Affinity(affinity_report(&handle, "a", 1_000)))
            .await,
        Some(1)
    );
    assert_eq!(
        handle
            .report(Report::Affinity(affinity_report(&handle, "a", 1_000)))
            .await,
        Some(2)
    );
    assert_eq!(
        handle.report(a_million_input("a", 1_000)).await,
        None,
        "only an affinity report takes a number"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    assert!(handle.wait_for_report(2, deadline).await);
    assert_eq!(handle.applied_report(), 2);

    let hold = handle.hold_reports().await;
    assert_eq!(
        handle
            .report(Report::Affinity(affinity_report(&handle, "a", 2_000)))
            .await,
        Some(3)
    );
    // A query queues behind the report, so its answer proves the task reached the report and parked it.
    handle.pins().await.unwrap();
    assert_eq!(
        handle.applied_report(),
        2,
        "the held report stays unapplied while the task serves the commands behind it"
    );
    hold.release().await;
    assert_eq!(handle.applied_report(), 3);

    // The bound reads tokio's clock, paused from here so the wait ends at its deadline whatever the host's load.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    assert!(
        !handle
            .wait_for_report(9, started + Duration::from_millis(5))
            .await,
        "a report never enqueued is never applied"
    );
    let waited = started.elapsed();
    assert!(
        (Duration::from_millis(5)..Duration::from_secs(1)).contains(&waited),
        "the wait ends at its bound: {waited:?}"
    );
}

#[tokio::test]
async fn the_purge_tick_writes_a_refreshed_entrys_time_so_a_restart_without_a_shutdown_keeps_it() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    {
        let handle = spawn(
            store.clone(),
            clock(&now),
            RouterConfig {
                purge_interval: Duration::from_millis(50),
                ..RouterConfig::default()
            },
        )
        .unwrap();
        handle
            .report(Report::Affinity(affinity_report(&handle, "a", 1_000)))
            .await;
        wait_for(WRITTEN, || {
            (!store.load_routing().unwrap().affinity.is_empty()).then_some(())
        })
        .await;
        now.store(2_000, Ordering::SeqCst);
        handle
            .report(Report::Affinity(affinity_report(&handle, "a", 2_000)))
            .await;
        wait_for(WRITTEN, || {
            store
                .load_routing()
                .unwrap()
                .affinity
                .into_iter()
                .find(|r| r.hash == "b1" && r.written_at == 2_000)
        })
        .await;
        // Dropped without a shutdown, the way a crash leaves the store: no drain runs after this point.
    }
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let written_at = handle
        .affinity()
        .read()
        .unwrap()
        .entry("claude-sonnet-5", "b1")
        .map(|e| e.written_at);
    assert_eq!(
        written_at,
        Some(2_000),
        "the reload reads the refresh the purge tick wrote"
    );
}

#[tokio::test]
async fn the_purge_tick_deletes_an_aged_entry_from_the_store() {
    let store = seeded(&["a"]);
    // Short enough for the entry to age inside the test's own window, rather than the hour-scale default.
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.affinity_ttl_seconds = 1;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(Report::Affinity(affinity_report(&handle, "a", 1_000)))
        .await;
    wait_for(WRITTEN, || {
        (!store.load_routing().unwrap().affinity.is_empty()).then_some(())
    })
    .await;
    now.store(1_000 + 2_000, Ordering::SeqCst);
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .affinity
            .is_empty()
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn an_account_recreated_under_its_id_drops_its_affinity_entries_and_a_late_report_from_the_old_holder_lands_nowhere(
) {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let report = affinity_report(&handle, "a", 1_000);
    handle.report(Report::Affinity(report.clone())).await;
    wait_for(WRITTEN, || {
        (!store.load_routing().unwrap().affinity.is_empty()).then_some(())
    })
    .await;

    store.delete_account("a", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::ApiKey))
        .unwrap();
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    // A single reload sees the account already recreated under a fresh incarnation, so the purge drops the
    // old holder's entry as one under another incarnation, not as one whose account is gone.
    handle.reload().await.unwrap();
    assert!(
        handle.affinity().read().unwrap().is_empty(),
        "the reload's purge drops the old holder's entry from the shared map at once"
    );
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .affinity
            .is_empty()
            .then_some(())
    })
    .await;

    handle.report(Report::Affinity(report)).await;
    stays_for(NOT_WRITTEN, || handle.affinity().read().unwrap().is_empty()).await;
    assert!(
        store.load_routing().unwrap().affinity.is_empty(),
        "a report under the old incarnation writes nothing"
    );
}

#[tokio::test]
async fn the_handles_predecessor_reads_the_snapshots_incarnation() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    // Held, so the purge cannot drop the entry under the other incarnation before the walk is asked to step over
    // it.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let seq = handle
        .report(Report::Affinity(AffinityReport {
            root: "b1".into(),
            message_id: Some("msg_01".into()),
            ..affinity_report(&handle, "a", 1_000)
        }))
        .await
        .expect("an affinity report takes a number");
    assert!(
        handle
            .wait_for_report(seq, tokio::time::Instant::now() + WRITTEN)
            .await
    );
    let key_generation = handle.snapshot().key("k").unwrap().created_at;
    let query = |hash: &str| {
        let message_boundaries = [hash.to_string()];
        handle.predecessor(&PredecessorQuery {
            canonical_model: "claude-sonnet-5",
            account_id: "a",
            key_id: "k",
            key_generation,
            roots: &[hash],
            message_boundaries: &message_boundaries,
        })
    };
    assert_eq!(
        query("b1"),
        Some("msg_01".to_string()),
        "the entry sits under the account's current incarnation"
    );

    handle.affinity().write().unwrap().record(&AffinityReport {
        incarnation: 2,
        root: "c1".into(),
        message_boundaries: Arc::from(["c1".to_string()]),
        message_id: Some("msg_c1".into()),
        ..affinity_report(&handle, "a", 2_000)
    });
    assert_eq!(
        query("c1"),
        None,
        "an entry under an incarnation the snapshot does not hold is stepped over"
    );
}

/// The ledger's standing is a replacing reading: a degraded report keeps a budgeted member out of the set whatever
/// its spend, and the next report puts it back.
#[tokio::test]
async fn the_accounting_report_flips_the_degraded_state_both_ways() {
    let store = seeded(&["a"]);
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(10.0),
            }],
            "test",
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    assert!(
        !handle.accounting_degraded().await.unwrap(),
        "a router starts with the ledger current"
    );

    handle.report(Report::Accounting { degraded: true }).await;
    assert!(handle.accounting_degraded().await.unwrap());
    let answer = handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    assert!(
        matches!(answer.result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "the pool's one member carries a budget, so nothing is placed until the ledger catches up: {:?}",
        answer.result
    );

    handle.report(Report::Accounting { degraded: false }).await;
    assert!(!handle.accounting_degraded().await.unwrap());
    let answer = handle
        .select(request(&handle, Some("s1"), 1_001))
        .await
        .unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(_)),
        "the member serves again once the ledger reads current: {:?}",
        answer.result
    );
}

#[tokio::test]
async fn a_spend_report_under_another_period_than_the_members_is_discarded() {
    let store = seeded(&["a"]);
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: Some(10.0),
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let snapshot = handle.snapshot();
    // The task's own state is not reachable from the handle; the same read and write the report handler uses
    // are exercised directly against a state built the way the task's does.
    let mut state = RoutingState::from_load(RoutingLoad::default());
    let month_row = MemberSpend {
        pool_id: "p".into(),
        account_id: "a".into(),
        period: phoebis_store::BudgetPeriod::Month,
        period_start: 0,
        period_end: 1,
        spent_usd: 5.0,
        observed_at: 1_000,
    };
    state.record_member_spend(&snapshot, vec![month_row]);
    assert!(
        state.member_spends_of("p", "a").next().is_none(),
        "a Month row is no obstacle for a member configured on Day"
    );
    let day_row = MemberSpend {
        pool_id: "p".into(),
        account_id: "a".into(),
        period: phoebis_store::BudgetPeriod::Day,
        period_start: 0,
        period_end: 1,
        spent_usd: 5.0,
        observed_at: 1_000,
    };
    state.record_member_spend(&snapshot, vec![day_row.clone()]);
    assert_eq!(
        state.member_spends_of("p", "a").collect::<Vec<_>>(),
        vec![&day_row]
    );
}

#[tokio::test]
async fn the_handle_answers_no_row_for_a_member_whose_period_the_report_predates() {
    let store = seeded(&["a"]);
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(10.0),
            }],
            "test",
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(Report::MemberSpend(vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: phoebis_store::BudgetPeriod::Month,
            period_start: 0,
            period_end: 100_000,
            spent_usd: 5.0,
            observed_at: 1_000,
        }]))
        .await;
    wait_for_async(WRITTEN, || async {
        handle
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "p" && r.account_id == "a")
    })
    .await;

    // The member's period changes with no report following: the analytics surface must never show the old
    // period's spend once the edit has taken effect.
    store
        .set_pool_members(
            "p",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: Some(10.0),
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    let rows = handle.member_spends().await.unwrap();
    assert!(
        rows.iter()
            .all(|r| !(r.pool_id == "p" && r.account_id == "a")),
        "a stale Month row is not answered for a member now configured on Day: {rows:?}"
    );
}

/// Proves nothing further reaches the broadcast for 200 ms: every purge tick and report in flight has had its turn.
async fn no_further_event(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) {
    stays_for(Duration::from_millis(200), || {
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        )
    })
    .await;
}

/// A head's reading of the 5-hour window, reported with the request whose head carried it.
fn head_reading(at: i64, utilization: f64, reset: Option<i64>, request_id: &str) -> Report {
    Report::Sample {
        account_id: "a".into(),
        sample: sample(at).with(
            FIVE_HOUR,
            WindowReading {
                utilization,
                reset,
                observed_at: at,
                source: WindowSource::Head,
                plan: None,
                tier: None,
            },
        ),
        incarnation: 1,
        source_request_id: Some(request_id.into()),
    }
}

#[tokio::test]
async fn a_shedding_round_emits_its_pins_and_marks_once() {
    use phoebis_routing::{PinKeyFacts, RouterEvent};
    let store = averaged(shedding_store(&["a", "b"]));
    with_tier(&store, "a");
    with_tier(&store, "b");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // Two sessions on `a`, `s1` burning four times what `s2` burns: 4 000 and 1 000 tokens folded over the 600 s
    // average read as 0.067 and 0.017 of the 5-hour window an hour through the harness pricer.
    let s1 = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    let s2 = selected_pin(
        handle
            .select(request(&handle, Some("s2"), 1_000))
            .await
            .unwrap(),
    );
    for (pin, input) in [(s1, 4_000), (s2, 1_000)] {
        handle
            .report(renewal(
                pin,
                2_000,
                PinUsage {
                    input,
                    ..PinUsage::default()
                },
                false,
                300_000,
            ))
            .await;
    }
    // A 5-hour reading at 0.9 with no reset projects five hours of the pins' burn, 0.42 more, over the ceiling. `b`
    // reads both windows low, so it fits a marked pin to its horizon.
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.9, None)))
        .await;
    handle.report(windows("b", 0.1, 0.1, 2_000)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    // Ten minutes on, `s2`'s cache has lapsed. `s1`'s is the one this request refreshes, so it reads warm.
    let at = 1_000 + 600_000;
    now.store(at, Ordering::SeqCst);
    past_the_wait(&handle, &store, "a", at).await;
    handle
        .select(request(&handle, Some("s1"), at))
        .await
        .unwrap();
    let (summary, _) = next_decision(&mut events).await;
    assert_eq!(
        summary.kind, "moved",
        "the round marks s1, the request's own pin, which moves in the same selection"
    );
    let key = |session: &str| PinKeyFacts {
        key_id: "k".into(),
        session_id: session.into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
    };
    match next_event(&mut events).await {
        RouterEvent::PinMigrated {
            pin: PinKey { session_id, .. },
            from_account,
            to_account,
            reason,
            ..
        } => assert_eq!(
            (
                session_id.as_str(),
                from_account.as_str(),
                to_account.as_str(),
                reason.as_str()
            ),
            ("s1", "a", "b", "shed")
        ),
        other => panic!("the move's pin event follows its decision, not {other:?}"),
    }
    match next_event(&mut events).await {
        RouterEvent::MarkCleared {
            account_id,
            pin: PinKey { session_id, .. },
            reason,
            marked_at,
            marked_on,
            marked_window,
            projection,
            stretch_ms,
            threshold_ms,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                session_id.as_str(),
                reason,
                marked_at,
                marked_on.as_str(),
                marked_window.as_str(),
                projection,
                stretch_ms,
                threshold_ms
            ),
            ("a", "s1", "moved", at, "a", "five_hour", None, None, None)
        ),
        other => panic!("the move clears the mark beside its pin event, not {other:?}"),
    }
    match next_event(&mut events).await {
        RouterEvent::SheddingRound {
            account_id,
            pool_id,
            request_id,
            pin:
                PinKey {
                    key_id,
                    session_id,
                    agent_id,
                    model,
                },
            round,
            at: emitted,
        } => {
            assert_eq!(
                (
                    account_id.as_str(),
                    pool_id.as_str(),
                    request_id.as_str(),
                    key_id.as_str(),
                    session_id.as_str(),
                    agent_id.as_str(),
                    model.as_str(),
                    emitted
                ),
                ("a", "p", "r-1", "k", "s1", "main", "claude-sonnet-5", at)
            );
            assert_eq!(
                (round.window.as_str(), round.unit, round.warm_sessions),
                ("five_hour", "window_share", "when_needed")
            );
            assert_eq!(
                (
                    round.trigger,
                    round.stretch_ms,
                    round.threshold_ms,
                    round.factor
                ),
                ("projection", None, None, None),
                "the pool runs no slow-down, so the projection fired the round"
            );
            assert!((round.utilization - 0.9).abs() < 1e-9);
            assert!((round.ceiling - 0.95).abs() < 1e-9);
            assert_eq!(round.horizon, at + 5 * HOUR, "no reset, so five hours out");
            assert!(
                round.projection > round.ceiling,
                "the projection at the horizon fired the round: {}",
                round.projection
            );
            assert_eq!(
                round
                    .pins
                    .iter()
                    .map(|p| (p.session_id.as_str(), p.cold, p.marked, p.skip, p.note))
                    .collect::<Vec<_>>(),
                [
                    ("s2", true, false, None, None),
                    ("s1", false, true, None, Some("cheapest"))
                ],
                "the round's order: the cold pin first, its burn under the target, and the warm current one, whose \
                 burn covers the target, marked as the cheapest sufficient pin"
            );
            assert!(round.pins.iter().all(|p| p.burn.is_some()));
            // The pins' 0.067 and 0.017 an hour, 599 s after their fold over the 600 s average: 5 000 / 60 000 x
            // e^(-599/600). Five hours of it over 0.9 put the excess at 0.0207 an hour, over the cap's half of the
            // burn.
            assert!((round.burn_sum - 0.030_707_757_067).abs() < 1e-9);
            assert!(
                (round.target - 0.015_353_878_533).abs() < 1e-9,
                "the target is the lesser of the excess and the cap's share of the burn"
            );
            assert!(round.pins[0].burn.unwrap() < round.target);
            assert!((round.removed - round.pins[1].burn.unwrap()).abs() < 1e-9);
            assert_eq!(round.pins[1].planned.as_deref(), Some("b"));
            assert_eq!(round.marks, vec![key("s1")]);
            assert_eq!((round.outcome.as_str(), round.reason), ("marked 1", None));
            assert_eq!(
                (round.dwell_seconds, round.dwell_until),
                (900, at + 900_000)
            );
        }
        other => panic!("the round follows its decision, not {other:?}"),
    }
    // Inside the dwell no round runs: the unmarked `s2` renews where it is.
    handle
        .select(request(&handle, Some("s2"), at + 1_000))
        .await
        .unwrap();
    assert_eq!(next_decision(&mut events).await.0.kind, "renewed");
    no_further_event(&mut events).await;
}

/// The round the events name, the first one among them.
fn round_among(events: &[phoebis_routing::RouterEvent]) -> phoebis_routing::ShedRoundFacts {
    events
        .iter()
        .find_map(|event| match event {
            phoebis_routing::RouterEvent::SheddingRound { round, .. } => Some((**round).clone()),
            _ => None,
        })
        .expect("a round ran")
}

#[tokio::test]
async fn a_round_leaves_out_a_pin_whose_agent_ended_until_the_agent_sends_again() {
    use phoebis_routing::{burn_average_ms, PinKeyFacts};
    let store = averaged(shedding_store(&["a", "b"]));
    with_tier(&store, "a");
    with_tier(&store, "b");
    let pool = store.pool("p").unwrap().expect("the pool");
    // Two burn averages, so each round's stamp stands past the trigger's wait.
    let waited = 2 * burn_average_ms(&pool);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    let subagent = |at: i64| SelectRequest {
        agent_id: "sub".into(),
        ..request(&handle, Some("s1"), at)
    };
    // Three pins on `a`: s1's main, the requester, s1's subagent, whose answer ended its turn, and s2's main, a live
    // sibling. The subagent burns the most and reads cold at the round, so a walk that priced it would mark it first.
    let requester = selected_pin(
        handle
            .select(request(&handle, Some("s1"), AT))
            .await
            .unwrap(),
    );
    let sibling = selected_pin(
        handle
            .select(request(&handle, Some("s2"), AT))
            .await
            .unwrap(),
    );
    let ended = selected_pin(handle.select(subagent(AT)).await.unwrap());
    for (pin, input) in [(requester, 1_000), (sibling.clone(), 2_000)] {
        handle
            .report(renewal(
                pin,
                AT + 1_000,
                PinUsage {
                    input,
                    ..PinUsage::default()
                },
                false,
                300_000,
            ))
            .await;
    }
    handle
        .report(Report::PinRenewed {
            pin: ended,
            now: AT + 1_000,
            usage: Some(PinUsage {
                input: 4_000,
                ..PinUsage::default()
            }),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::AgentEnded,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    handle
        .report(observed("a", sample_with(AT + 1_000, FIVE_HOUR, 0.9, None)))
        .await;
    handle.report(windows("b", 0.1, 0.1, AT + 1_000)).await;
    handle.samples().await.unwrap();
    let key = |session: &str, agent: &str| PinKeyFacts {
        key_id: "k".into(),
        session_id: session.into(),
        agent_id: agent.into(),
        model: "claude-sonnet-5".into(),
    };
    let rows = |round: &phoebis_routing::ShedRoundFacts| {
        round
            .pins
            .iter()
            .map(|p| {
                (
                    p.session_id.clone(),
                    p.agent_id.clone(),
                    p.marked,
                    p.skip,
                    p.burn.is_some(),
                )
            })
            .collect::<Vec<_>>()
    };
    let row = |session: &str, agent: &str, marked: bool, skip: Option<&'static str>, burn: bool| {
        (session.to_string(), agent.to_string(), marked, skip, burn)
    };

    // Ten minutes on, every cache but the requester's has lapsed.
    let at = AT + 600_000;
    now.store(at, Ordering::SeqCst);
    handle
        .stamp_projection_over("p", "a", FIVE_HOUR, at - waited, None)
        .await;
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), at))
        .await
        .unwrap();
    let round = round_among(&drained(&mut events).await);
    assert_eq!(
        rows(&round),
        [
            row("s2", "main", true, None, true),
            row("s1", "main", false, None, true),
            row("s1", "sub", false, Some("ended"), false),
        ],
        "the ended pin leaves the walk before the order with no burn, and the walk marks the live sibling"
    );
    assert_eq!(round.marks, vec![key("s2", "main")]);
    assert!(
        (round.removed - round.pins[0].burn.unwrap()).abs() < 1e-12,
        "the mark relieves the sibling's own burn: {round:?}"
    );

    // The next round past the dwell, on the requester's request. A reading over the ceiling keeps it firing whatever
    // the decayed burns read by then.
    let dwell = shed_dwell_ms(&pool);
    let round_after = |since: i64| {
        let handle = handle.clone();
        let now = now.clone();
        async move {
            let at = since + dwell + 1_000;
            now.store(at, Ordering::SeqCst);
            handle
                .report(observed(
                    "a",
                    sample_with(at - 1_000, FIVE_HOUR, 0.96, None),
                ))
                .await;
            handle.samples().await.unwrap();
            handle
                .stamp_projection_over("p", "a", FIVE_HOUR, at - waited, None)
                .await;
            (at, handle.select(request(&handle, Some("s1"), at)).await)
        }
    };

    // Inside the dwell a probe replays the subagent's request: it is not the agent's own request, so the next round
    // still leaves the subagent out.
    handle
        .select(SelectRequest {
            probe: true,
            ..subagent(at + 500)
        })
        .await
        .unwrap();
    drained(&mut events).await;
    let (middle, answer) = round_after(at).await;
    answer.unwrap();
    let round = round_among(&drained(&mut events).await);
    let skip_of = |round: &phoebis_routing::ShedRoundFacts, session: &str, agent: &str| {
        round
            .pins
            .iter()
            .find(|p| p.session_id == session && p.agent_id == agent)
            .unwrap_or_else(|| panic!("{session}'s {agent} pin is on the account: {round:?}"))
            .skip
    };
    assert_eq!(
        skip_of(&round, "s1", "sub"),
        Some("ended"),
        "a probe's selection leaves the mark: {round:?}"
    );

    // The subagent sends again inside the dwell, so the next round past it walks its pin.
    handle.select(subagent(middle + 1_000)).await.unwrap();
    drained(&mut events).await;
    round_after(middle).await.1.unwrap();
    let round = round_among(&drained(&mut events).await);
    let returned = round
        .pins
        .iter()
        .find(|p| p.session_id == "s1" && p.agent_id == "sub")
        .expect("the subagent's pin is on the account");
    assert!(
        returned.skip != Some("ended") && returned.burn.is_some(),
        "the subagent's request cleared the mark, so its pin is walked and priced again: {round:?}"
    );
}

#[tokio::test]
async fn a_projection_leaves_out_the_burn_of_a_pin_whose_agent_ended_until_the_agent_sends_again() {
    let store = averaged(shedding_store(&["a", "b"]));
    with_tier(&store, "a");
    with_tier(&store, "b");
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    let subagent = |at: i64| SelectRequest {
        agent_id: "sub".into(),
        ..request(&handle, Some("s1"), at)
    };
    // Two pins on `a`: s2's main, a live sibling, and s1's subagent, whose answer ended its turn. Over the 600 s
    // average the sibling's 1 200 input tokens burn 0.02 of the 5-hour window an hour and the subagent's 3 600 burn
    // 0.06.
    let sibling = selected_pin(
        handle
            .select(request(&handle, Some("s2"), AT))
            .await
            .unwrap(),
    );
    let ended = selected_pin(handle.select(subagent(AT)).await.unwrap());
    handle
        .report(renewal(
            sibling,
            AT + 1_000,
            PinUsage {
                input: 1_200,
                ..PinUsage::default()
            },
            false,
            300_000,
        ))
        .await;
    handle
        .report(Report::PinRenewed {
            pin: ended,
            now: AT + 1_000,
            usage: Some(PinUsage {
                input: 3_600,
                ..PinUsage::default()
            }),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::AgentEnded,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    handle
        .report(observed("a", sample_with(AT + 1_000, FIVE_HOUR, 0.6, None)))
        .await;
    handle.report(windows("b", 0.1, 0.1, AT + 1_000)).await;
    handle.samples().await.unwrap();
    now.store(AT + 1_000, Ordering::SeqCst);
    let on_a = |record: &DecisionRecord| {
        let a = record
            .candidates
            .iter()
            .find(|c| c.account_id == "a")
            .expect("a's row");
        (
            a.burn.get(FIVE_HOUR).copied(),
            a.windows.get(FIVE_HOUR).and_then(|w| w.projection),
        )
    };

    // The sibling's request: five hours of its burn over 0.6 read under the ceiling, where the subagent's burn would
    // carry the window to 1.0.
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s2"), AT + 1_000))
        .await
        .unwrap();
    let (burn, projection) = on_a(&next_decision(&mut events).await.1);
    assert!(
        close(burn, 0.02),
        "the burn leaves the ended pin out: {burn:?}"
    );
    assert!(
        close(projection, 0.7),
        "the projection at the horizon leaves the ended pin out: {projection:?}"
    );

    // The subagent's own request clears the mark, so its record counts the pin in both sums again.
    let mut events = handle.subscribe_events();
    handle.select(subagent(AT + 1_000)).await.unwrap();
    let (burn, projection) = on_a(&next_decision(&mut events).await.1);
    assert!(
        close(burn, 0.08),
        "the burn counts the returned pin: {burn:?}"
    );
    assert!(
        close(projection, 1.0),
        "the projection counts the returned pin: {projection:?}"
    );
}

#[tokio::test]
async fn the_slow_down_hold_divides_the_factor_among_the_live_pins_and_leaves_out_a_pin_whose_agent_ended(
) {
    let now = Arc::new(AtomicI64::new(AT));
    let store = shedding_store(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.slow_down.slow_down = true;
    pool.slow_down.slow_down_stretch_seconds = 1800;
    pool.slow_down.slow_down_hold_seconds = 600;
    pool.shedding.burn_average_seconds = (SLOWED_AVERAGE_MS / 1_000) as u64;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    with_tier(&store, "a");
    // `slowed_router`'s s1, a tenth of the 5-hour window an hour on `a` at 0.9 with 40 minutes left, beside s2, whose
    // agent burned nine times that and ended.
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    store
        .upsert_pin(&stored_pin(&store, "s2", "a", 2, 90.0))
        .unwrap();
    let handle = spawn(store, clock(&now), priced_config()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.9, Some(SLOWED_RESET)),
        ))
        .await;
    handle
        .report(Report::AgentEnded {
            key_id: "k".into(),
            session_id: "s2".into(),
            agent_id: "main".into(),
        })
        .await;
    handle.samples().await.unwrap();

    // The stamp one burn average back puts the ramp at 1, s1's first main turn starts its pace, and its second, 30 s
    // on, gives it its cycle and waits.
    handle
        .select(SelectRequest {
            request_class: Some("quota".into()),
            ..slowed_turn(&handle, "r-0", AT - SLOWED_AVERAGE_MS)
        })
        .await
        .unwrap();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    let t2 = AT + 30_000;
    now.store(t2, Ordering::SeqCst);
    let mut events = handle.subscribe_events();
    let hold = match handle
        .select(slowed_turn(&handle, "r-2", t2))
        .await
        .unwrap()
        .result
    {
        SelectResult::Held(hold) => hold,
        other => panic!("the pin with a cycle waits: {other:?}"),
    };
    let outcome = next_decision(&mut events)
        .await
        .1
        .outcome
        .hold
        .expect("a held outcome");
    let factor = outcome.factor.expect("the read's factor");

    // s1 is the one live pin, so it takes the whole reduction: it waits its cycle over the factor, under the cap, and
    // the account realizes the factor itself.
    let held = (hold.until.expect("a deadline") - t2) as f64;
    let wanted = 30_000.0 * (1.0 / factor - 1.0);
    assert!(
        (held / wanted - 1.0).abs() < 1e-3,
        "s1 waits its cycle over the factor, not the cap: {held} against {wanted} at {factor}"
    );
    let speed = outcome.speed.expect("the realized speed");
    assert!(
        (speed / factor - 1.0).abs() < 1e-9,
        "the ended pin adds no full-speed burn to the realized speed: {speed} against {factor}"
    );
    // The entry folds that speed into the full speed r-1's read stored, 30 s of the 30-minute average before.
    let w = (-1.0_f64 / 60.0).exp();
    let stored = projection_of_account(&handle.projections("p").await.unwrap(), "a")
        .slow_down
        .expect("the pinned reads wrote a's entry")
        .speed;
    assert!(
        (stored / (w + (1.0 - w) * factor) - 1.0).abs() < 1e-9,
        "the stored speed folds the live pins' speed alone: {stored}"
    );
}

/// A clock past the cache TTL, so a pin last refreshed an hour before it reads cold.
const AT: i64 = 10 * HOUR;

/// `request` under the `quota` class, which leaves the pin's warmth as it was, so a pin last refreshed an hour ago
/// reads cold on it.
fn quota_turn(handle: &RouterHandle, session: Option<&str>, now: i64) -> SelectRequest {
    SelectRequest {
        request_class: Some("quota".into()),
        ..request(handle, session, now)
    }
}

/// Session `session`'s pin under the store's key `k` on `account`, burning `burn` input tokens a second in both
/// copies, placed an hour before `AT`, used a second before it and last refreshed half an hour before it, so it is
/// live and cold, its row unflagged.
fn stored_pin(store: &Store, session: &str, account: &str, epoch: u64, burn: f64) -> PinRow {
    let rates = PinBurn {
        input: burn,
        ..PinBurn::default()
    };
    PinRow {
        key_id: "k".into(),
        session_id: session.into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        account_id: account.into(),
        epoch,
        created_at: AT - HOUR,
        last_used_at: AT - 1_000,
        // The key's creation time, so the purge reads the pin as its key's rather than a replaced key's.
        key_generation: store.api_key("k").unwrap().unwrap().created_at,
        incarnation: 1,
        burn: rates,
        burn_at: Some(AT),
        burn_banded: rates,
        burn_fast: PinBurn::default(),
        burn_unmetered: PinBurn::default(),
        burn_unmetered_model: None,
        last_input_tokens: 1_000,
        last_refresh_at: AT - HOUR / 2,
        cache_ttl_ms: 300_000,
        shed: None,
        static_hash: None,
        tail_hash: None,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        entrypoint: None,
        mark: None,
        auxiliary_only: false,
        effort: None,
        ended: false,
        thinking_sums: Default::default(),
        account_bound: false,
    }
}

/// `pin` as a round at `at` on the 5-hour window left it: marked on the account it sits on.
fn marked(pin: PinRow, at: i64) -> PinRow {
    PinRow {
        mark: Some(PinMark {
            at,
            on: pin.account_id.clone(),
            window: FIVE_HOUR.into(),
            source: None,
        }),
        ..pin
    }
}

/// `RouterConfig::default()` with a pricer on `TIER`: one cache-write token a ten-millionth of the 5-hour window and a
/// sixty-millionth of the 7-day window, and a pin burning `x` input tokens a second a hundredth of `x` of the 5-hour
/// window an hour, a sixth of that on the 7-day window. Nothing on any other tier.
fn priced_config() -> RouterConfig {
    fn per_token(window: &str) -> Option<f64> {
        match window {
            FIVE_HOUR => Some(10_000_000.0),
            SEVEN_DAY => Some(60_000_000.0),
            SEVEN_DAY_FABLE => Some(30_000_000.0),
            _ => None,
        }
    }
    let share = SharePricer::new(|window, _, tier, _, usage, _| {
        (tier == TIER)
            .then(|| per_token(window))
            .flatten()
            .map(|per| (usage.write_5m + usage.write_1h) as f64 / per)
    })
    .with_rate(|window, _, tier, _, burn, _| {
        if tier != TIER {
            return None;
        }
        per_token(window).map(|per| burn.input / per * 100_000.0 / 3_600.0)
    });
    RouterConfig {
        share,
        ..RouterConfig::default()
    }
}

/// The pool `p` over OAuth accounts `ids` in that priority order.
fn shedding_store(ids: &[&str]) -> Arc<Store> {
    let store = seeded_oauth(ids);
    store
        .set_pool_members(
            "p",
            &ids.iter()
                .enumerate()
                .map(|(priority, id)| MemberWrite {
                    account_id: id.to_string(),
                    priority: priority as u32,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                })
                .collect::<Vec<_>>(),
            "test",
        )
        .unwrap();
    store
}

/// A second pool `q` on its own dwell, holding no member: the pool `a` left for `p`, since an account belongs to one
/// pool, whose rounds on `a` stay on file.
fn second_pool(store: &Store, dwell_seconds: u64) {
    store
        .insert_pool(
            &PoolRow {
                shedding: Shedding {
                    shed_dwell_seconds: dwell_seconds,
                    ..Shedding::default()
                },
                ..PoolRow::for_tests("q")
            },
            "test",
        )
        .unwrap();
}

/// Both shared windows at `five_hour` and `seven_day` with no reset, so each one's horizon sits five hours out.
fn windows(account: &str, five_hour: f64, seven_day: f64, at: i64) -> Report {
    observed(
        account,
        sample_with(at, FIVE_HOUR, five_hour, None).with(
            SEVEN_DAY,
            WindowReading {
                utilization: seven_day,
                reset: None,
                observed_at: at,
                source: WindowSource::Head,
                plan: None,
                tier: None,
            },
        ),
    )
}

/// Where the selection landed and the rule that moved it there.
fn landed_on(
    answer: phoebis_routing::SelectAnswer,
) -> (String, Option<phoebis_routing::MigrateReason>) {
    match answer.result {
        SelectResult::Selected(sel) => (sel.account.id, sel.migrated),
        other => panic!("{other:?}"),
    }
}

/// The `round` fact of the decision's shed line: `ran`, or `within_dwell` while the pool's last round on the
/// account gates the next.
fn shed_round_fact(record: &DecisionRecord) -> Option<phoebis_routing::FactValue> {
    record
        .walk
        .iter()
        .find(|step| step.step == "move:shed")?
        .facts
        .iter()
        .find(|fact| fact.key == "round")
        .map(|fact| fact.value.clone())
}

/// Every event the broadcast holds until it stays quiet for 200 ms.
async fn drained(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> Vec<phoebis_routing::RouterEvent> {
    let mut seen = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await
    {
        seen.push(event);
    }
    seen
}

fn no_round(events: &[phoebis_routing::RouterEvent]) -> bool {
    !events
        .iter()
        .any(|event| matches!(event, phoebis_routing::RouterEvent::SheddingRound { .. }))
}

/// Stamps the 5-hour window of `account` on the pool `p` as over the ceiling since two burn averages of the pool before
/// `now`, the wait's cap, so the selection at `now` finds it past its wait whatever its wall and a round case reads its
/// round at its first selection over the ceiling.
async fn past_the_wait(handle: &RouterHandle, store: &Store, account: &str, now: i64) {
    let cap = 2 * phoebis_routing::burn_average_ms(&store.pool("p").unwrap().expect("the pool"));
    handle
        .stamp_projection_over("p", account, FIVE_HOUR, now - cap, None)
        .await;
}

#[tokio::test]
async fn the_marks_and_the_dwell_gate_survive_a_restart() {
    use phoebis_routing::MigrateReason;
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    // Neither pin burns, so a round run now would mark nothing: s1 moves on the stored mark alone, which the
    // previous run's round on a made a minute before the restart, leaving s2.
    for pin in [
        marked(stored_pin(&store, "s1", "a", 1, 0.0), AT - 60_000),
        stored_pin(&store, "s2", "a", 2, 0.0),
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    store.replace_shed_round("p", "a", 1, AT - 60_000).unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // a reads over the ceiling with nothing burning and its window past its wait, so the trigger fires and the loaded
    // round's dwell holds it.
    past_the_wait(&handle, &store, "a", AT).await;
    handle.report(windows("a", 0.96, 0.3, AT)).await;
    handle.report(windows("b", 0.1, 0.1, AT)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT))
                .await
                .unwrap()
        ),
        ("b".into(), Some(MigrateReason::Shed)),
        "the loaded mark moves s1 at its next request"
    );
    let (_, record) = next_decision(&mut events).await;
    assert!(record.pin.as_ref().is_some_and(|pin| pin.marked));
    assert_eq!(
        shed_round_fact(&record),
        Some(phoebis_routing::FactValue::Text("within_dwell".into()))
    );
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s2"), AT + 1_000))
                .await
                .unwrap()
        ),
        ("a".into(), None)
    );
    let later = drained(&mut events).await;
    assert!(
        no_round(&later),
        "inside the loaded dwell no round runs on the hot account: {later:?}"
    );
    assert!(
        later.iter().any(|event| matches!(
            event,
            phoebis_routing::RouterEvent::MarkCleared {
                pin: PinKey { session_id, .. },
                reason: "moved",
                marked_at,
                ..
            } if session_id == "s1" && *marked_at == AT - 60_000
        )),
        "the move clears the loaded mark: {later:?}"
    );
}

/// Spawns a router again over `store`, as a restart does, beside its read of session `s1`'s `sub` pin: the account the
/// pin sits on and its ended mark.
async fn sub_after_a_restart(
    store: &Arc<Store>,
    now: &Arc<AtomicI64>,
) -> (RouterHandle, (String, bool)) {
    let handle = spawn(store.clone(), clock(now), RouterConfig::default()).unwrap();
    let pin = handle
        .pins()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.session_id == "s1" && p.agent_id == "sub")
        .expect("the restart loads the subagent's pin");
    (handle, (pin.account_id, pin.ended))
}

#[tokio::test]
async fn every_set_of_an_ended_agents_mark_and_its_clear_by_an_exhausted_request_survive_a_restart()
{
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let subagent = |handle: &RouterHandle, request_id: &str, at: i64| SelectRequest {
        request_id: request_id.into(),
        agent_id: "sub".into(),
        ..request(handle, Some("s1"), at)
    };

    // The companion's report ends the agent.
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .select(subagent(&handle, "r-1", 1_000))
        .await
        .unwrap();
    handle
        .report(Report::AgentEnded {
            key_id: "k".into(),
            session_id: "s1".into(),
            agent_id: "sub".into(),
        })
        .await;
    handle.shutdown().await.unwrap();
    let (handle, (home, ended)) = sub_after_a_restart(&store, &now).await;
    assert!(ended, "the companion's report is written with the pin");
    let other = if home == "a" { "b" } else { "a" };

    // The agent sends again. Its first attempt failed on the pin's account and the request is bound to its pin, so the
    // selection is exhausted and writes nothing for the pin: the clear writes the row of its own.
    now.store(2_000, Ordering::SeqCst);
    let answer = handle
        .select(SelectRequest {
            portability: Portability::Hold,
            attempt: 2,
            attempted: vec![home.clone()],
            ..subagent(&handle, "r-2", 2_000)
        })
        .await
        .unwrap();
    assert!(
        matches!(answer.result, SelectResult::Exhausted(_)),
        "the bound request finds its pin's account tried: {:?}",
        answer.result
    );
    handle.shutdown().await.unwrap();
    let (handle, (account, ended)) = sub_after_a_restart(&store, &now).await;
    assert_eq!(
        (account.as_str(), ended),
        (home.as_str(), false),
        "the agent's request clears the stored mark"
    );

    // A request of the agent is served on its pin, and the next one, its first attempt failed there, moves the pin to
    // the other account before the first one's stream ends the agent's turn: the renewal names the pin's old epoch and
    // writes no renewal of its own, so the mark reaches the store through the marked row alone.
    now.store(3_000, Ordering::SeqCst);
    let served = selected_pin(
        handle
            .select(subagent(&handle, "r-3", 3_000))
            .await
            .unwrap(),
    );
    let moved = handle
        .select(SelectRequest {
            attempt: 2,
            attempted: vec![home.clone()],
            ..subagent(&handle, "r-4", 3_000)
        })
        .await
        .unwrap();
    assert_eq!(
        landed_on(moved).0,
        other,
        "the retry places the pin on the other account"
    );
    handle
        .report(Report::PinRenewed {
            pin: served,
            now: 4_000,
            usage: Some(PinUsage {
                input: 100,
                ..PinUsage::default()
            }),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::AgentEnded,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    handle.shutdown().await.unwrap();
    let (handle, (account, ended)) = sub_after_a_restart(&store, &now).await;
    assert_eq!(
        (account.as_str(), ended),
        (other, true),
        "the renewal's mark is written with the moved pin"
    );

    // The agent sends again on its pin, which clears the mark, and that request's stream ends its turn: the renewal
    // names the pin's own epoch, so the mark reaches the store on the renewal's own write of the row.
    now.store(5_000, Ordering::SeqCst);
    let served = selected_pin(
        handle
            .select(subagent(&handle, "r-5", 5_000))
            .await
            .unwrap(),
    );
    handle
        .report(Report::PinRenewed {
            pin: served,
            now: 6_000,
            usage: Some(PinUsage {
                input: 100,
                ..PinUsage::default()
            }),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::AgentEnded,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    handle.shutdown().await.unwrap();
    let (_, (account, ended)) = sub_after_a_restart(&store, &now).await;
    assert_eq!(
        (account.as_str(), ended),
        (other, true),
        "the renewal's own row carries the mark"
    );
}

#[tokio::test]
async fn a_pin_that_moved_away_and_returned_inside_the_dwell_is_not_shed_again_after_a_restart() {
    use phoebis_routing::MigrateReason;
    let store = shedding_store(&["a", "b", "c"]);
    for id in ["a", "b", "c"] {
        with_tier(&store, id);
    }
    for pin in [
        stored_pin(&store, "s1", "a", 1, 4.0),
        stored_pin(&store, "s0", "a", 2, 1.0),
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // a at 0.9 with its pins burning 0.05 of the window an hour projects to 1.15, so its round runs and marks s1. b
    // fits. c reads 0.92, so the spent move below finds the rewrite fitting on a and c alone and takes a, whose runway
    // is the longer.
    handle.report(windows("a", 0.9, 0.3, AT)).await;
    handle.report(windows("b", 0.1, 0.1, AT)).await;
    handle.report(windows("c", 0.92, 0.1, AT)).await;
    handle.samples().await.unwrap();
    past_the_wait(&handle, &store, "a", AT).await;
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT))
                .await
                .unwrap()
        ),
        ("b".into(), Some(MigrateReason::Shed)),
        "a's round marks s1 and it moves"
    );
    handle.report(windows("b", 1.0, 0.1, AT + 500)).await;
    handle.samples().await.unwrap();
    now.store(AT + 1_000, Ordering::SeqCst);
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT + 1_000))
                .await
                .unwrap()
        ),
        ("a".into(), Some(MigrateReason::Spent)),
        "b spends its window and the spent move takes s1 back to a, inside a's dwell"
    );
    handle.shutdown().await.unwrap();

    now.store(AT + 2_000, Ordering::SeqCst);
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle.report(windows("a", 0.9, 0.3, AT + 2_000)).await;
    handle.report(windows("b", 1.0, 0.1, AT + 2_000)).await;
    // c now fits, so a mark that outlived the move would find it a destination.
    handle.report(windows("c", 0.1, 0.1, AT + 2_000)).await;
    handle.samples().await.unwrap();
    // The stamps live in memory, so the restart starts a's wait afresh.
    past_the_wait(&handle, &store, "a", AT + 2_000).await;
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT + 2_000))
                .await
                .unwrap()
        ),
        ("a".into(), None),
        "the move consumed s1's mark before the restart, so it renews in place rather than going to c"
    );
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(summary.kind, "renewed");
    assert!(record.pin.as_ref().is_some_and(|pin| !pin.marked));
    assert_eq!(
        shed_round_fact(&record),
        Some(phoebis_routing::FactValue::Text("within_dwell".into())),
        "the round before the restart still gates a"
    );
    let later = drained(&mut events).await;
    assert!(no_round(&later), "{later:?}");
}

#[tokio::test]
async fn a_released_and_recreated_pin_s_mark_does_not_survive_a_restart() {
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    for pin in [
        stored_pin(&store, "s1", "a", 1, 4.0),
        stored_pin(&store, "s0", "a", 2, 1.0),
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle.report(windows("a", 0.9, 0.3, AT)).await;
    handle.report(windows("b", 0.1, 0.1, AT)).await;
    handle.samples().await.unwrap();
    past_the_wait(&handle, &store, "a", AT).await;
    let mut events = handle.subscribe_events();
    // s0's request runs a's round, which marks s1, the heavier, and leaves s0 where it is.
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s0"), AT))
                .await
                .unwrap()
        ),
        ("a".into(), None)
    );
    next_decision(&mut events).await;
    match next_event(&mut events).await {
        phoebis_routing::RouterEvent::SheddingRound { round, .. } => assert_eq!(
            round
                .marks
                .iter()
                .map(|key| key.session_id.as_str())
                .collect::<Vec<_>>(),
            ["s1"]
        ),
        other => panic!("the round follows its decision, not {other:?}"),
    }
    // The relay's release of the stored pin, which no selection renewed since the load.
    handle
        .report(Report::PinReleased {
            pin: PinRef {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: "claude-sonnet-5".into(),
                epoch: 1,
                lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
                entrypoint: None,
                effort: None,
            },
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        phoebis_routing::RouterEvent::PinExpired { reasons, .. } if reasons == vec!["client_error"]
    ));
    match next_event(&mut events).await {
        phoebis_routing::RouterEvent::MarkCleared {
            pin: PinKey { session_id, .. },
            reason,
            marked_at,
            marked_on,
            ..
        } => assert_eq!(
            (session_id.as_str(), reason, marked_at, marked_on.as_str()),
            ("s1", "released", AT, "a"),
            "the release clears the mark beside its pin event"
        ),
        other => panic!("{other:?}"),
    }
    now.store(AT + 1_000, Ordering::SeqCst);
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT + 1_000))
                .await
                .unwrap()
        ),
        ("a".into(), None),
        "a fresh pin under the key lands on a under a new epoch"
    );
    handle.shutdown().await.unwrap();

    now.store(AT + 2_000, Ordering::SeqCst);
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // With s1's burn gone, 0.93 keeps a's projection over the ceiling, so the dwell alone holds a second round off.
    handle.report(windows("a", 0.93, 0.3, AT + 2_000)).await;
    handle.report(windows("b", 0.1, 0.1, AT + 2_000)).await;
    handle.samples().await.unwrap();
    past_the_wait(&handle, &store, "a", AT + 2_000).await;
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT + 2_000))
                .await
                .unwrap()
        ),
        ("a".into(), None),
        "the released pin's mark left with it, so the fresh pin stays rather than moving to b"
    );
    let (_, record) = next_decision(&mut events).await;
    assert!(record.pin.as_ref().is_some_and(|pin| !pin.marked));
    assert_eq!(
        shed_round_fact(&record),
        Some(phoebis_routing::FactValue::Text("within_dwell".into())),
        "the round's dwell still gates a"
    );
    let later = drained(&mut events).await;
    assert!(no_round(&later), "{later:?}");
}

#[tokio::test]
async fn a_marked_pin_moved_off_its_cooled_account_inside_a_decision_clears_its_mark_as_moved() {
    use phoebis_routing::RouterEvent;
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    for pin in [
        marked(stored_pin(&store, "s1", "a", 1, 0.0), AT - 60_000),
        stored_pin(&store, "s0", "a", 2, 0.0),
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    // The model is cooled on a, so step 5 releases s1's pin and the placement on b moves the session.
    store
        .upsert_cooldown(&CooldownRow {
            account_id: "a".into(),
            scope: CooldownScope::Model("claude-sonnet-5".into()),
            until: AT + 600_000,
            set_at: AT - 1_000,
            reason: "rate_limit".into(),
            window: "retry_after".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        })
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // a reads over the ceiling with nothing burning, so the heal check leaves the mark standing.
    handle.report(windows("a", 0.96, 0.3, AT)).await;
    handle.report(windows("b", 0.1, 0.1, AT)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT))
                .await
                .unwrap()
        ),
        ("b".into(), None),
        "no rule moves the cooled account's pin, and the selection replaces it on b"
    );
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(summary.kind, "placed");
    assert!(record.pin.as_ref().is_some_and(|pin| pin.marked));
    assert_eq!(
        record.outcome.mark_cleared, None,
        "no heal and no rule's move cleared it inside the selection"
    );
    let later = drained(&mut events).await;
    assert!(
        later
            .iter()
            .any(|event| matches!(event, RouterEvent::PinMigrated { reason, reasons, .. } if reason == "cooldown" && reasons.contains(&"cooldown"))),
        "{later:?}"
    );
    assert!(
        later.iter().any(|event| matches!(
            event,
            RouterEvent::MarkCleared {
                account_id,
                pin: PinKey { session_id, .. },
                reason: "moved",
                marked_at,
                marked_on,
                marked_window,
                projection: None,
                ..
            } if account_id == "a"
                && session_id == "s1"
                && *marked_at == AT - 60_000
                && marked_on == "a"
                && marked_window == "five_hour"
        )),
        "the move inside the decision clears the mark: {later:?}"
    );
}

#[tokio::test]
async fn a_heals_event_names_the_wall_at_the_windows_reset_and_the_pools_ceiling_at_the_five_hour_bound(
) {
    use phoebis_routing::RouterEvent;
    let store = shedding_store(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.five_hour_ceiling = 0.8;
    pool.weekly_ceiling = 0.8;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    for pin in [
        marked(stored_pin(&store, "s1", "a", 1, 0.0), AT - 60_000),
        PinRow {
            mark: Some(PinMark {
                at: AT - 60_000,
                on: "a".into(),
                window: SEVEN_DAY.into(),
                source: None,
            }),
            ..stored_pin(&store, "s2", "a", 2, 0.0)
        },
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // The 5-hour reading names its reset an hour out, so its horizon is the reset and its line the wall, which 0.9
    // reads under while over the ceiling. The week names none, so its horizon is the five-hour bound and its line the
    // ceiling, which 0.5 reads under. Nothing burns, so each projection is its reading.
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.9, Some(AT + HOUR)).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.5,
                    reset: None,
                    observed_at: AT,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    for session in ["s1", "s2"] {
        handle
            .select(quota_turn(&handle, Some(session), AT))
            .await
            .unwrap();
    }
    let cleared: Vec<_> = drained(&mut events)
        .await
        .into_iter()
        .filter_map(|event| match event {
            RouterEvent::MarkCleared {
                pin,
                reason,
                marked_window,
                limit,
                ..
            } => Some((pin.session_id, reason, marked_window, limit)),
            _ => None,
        })
        .collect();
    assert_eq!(
        cleared,
        [
            (
                "s1".to_string(),
                "healed",
                "five_hour".to_string(),
                Some(1.0)
            ),
            (
                "s2".to_string(),
                "healed",
                "seven_day".to_string(),
                Some(0.8)
            ),
        ]
    );
}

#[tokio::test]
async fn a_cooldown_report_naming_a_marked_pin_clears_its_mark_as_released() {
    use phoebis_routing::RouterEvent;
    let store = shedding_store(&["a", "b"]);
    store
        .upsert_pin(&marked(stored_pin(&store, "s1", "a", 1, 0.0), AT - 60_000))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    // A 429 on the pin's account: the relay's report names the pin, and the invalidation takes the row.
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: AT + 600_000,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(PinRef {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: "claude-sonnet-5".into(),
                epoch: 1,
                lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
                entrypoint: None,
                effort: None,
            }),
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    handle.cooldowns().await.unwrap();
    assert!(
        handle.pins().await.unwrap().is_empty(),
        "the report invalidated the pin"
    );
    let seen = drained(&mut events).await;
    assert!(
        seen.iter()
            .any(|event| matches!(event, RouterEvent::CooldownSet { account_id, .. } if account_id == "a")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|event| matches!(
            event,
            RouterEvent::MarkCleared {
                account_id,
                pin: PinKey { session_id, .. },
                reason: "released",
                marked_at,
                marked_on,
                marked_window,
                projection: None,
                ..
            } if account_id == "a"
                && session_id == "s1"
                && *marked_at == AT - 60_000
                && marked_on == "a"
                && marked_window == "five_hour"
        )),
        "the invalidation clears the mark with its row: {seen:?}"
    );
}

/// Session `s1`'s pin of `model` on `account`, last used `used_at`, its lane composed from `model`.
fn spelled_pin(store: &Store, model: &str, account: &str, epoch: u64, used_at: i64) -> PinRow {
    PinRow {
        model: model.into(),
        lane: lane_key(model, "standard", "none", "-"),
        last_used_at: used_at,
        ..stored_pin(store, "s1", account, epoch, 0.0)
    }
}

/// The `released` clears in `events`, as (account, model, marked_on).
fn released_clears(events: &[phoebis_routing::RouterEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            phoebis_routing::RouterEvent::MarkCleared {
                account_id,
                pin: PinKey { model, .. },
                reason: "released",
                marked_on,
                ..
            } => Some((account_id.clone(), model.clone(), marked_on.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_marked_pin_the_rekey_merges_into_a_newer_dated_row_clears_its_mark_as_released() {
    const SPELLING: &str = "claude-sonnet-4-5";
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = shedding_store(&["a", "b"]);
    store
        .upsert_pin(&marked(
            spelled_pin(&store, SPELLING, "a", 1, AT - 5_000),
            AT - 60_000,
        ))
        .unwrap();
    store
        .upsert_pin(&spelled_pin(&store, DATED, "b", 2, AT - 1_000))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.resolve(SPELLING, DATED, AT).await.unwrap();
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|p| (p.model.as_str(), p.account_id.as_str()))
            .collect::<Vec<_>>(),
        [(DATED, "b")],
        "the newer dated row wins the merge"
    );
    assert_eq!(
        released_clears(&drained(&mut events).await),
        [("a".to_string(), SPELLING.to_string(), "a".to_string())],
        "the spelled row leaves with its mark, so the mark clears as a release does"
    );
}

#[tokio::test]
async fn a_marked_dated_row_the_rekey_replaces_with_a_newer_spelled_row_clears_its_mark_as_released(
) {
    const SPELLING: &str = "claude-sonnet-4-5";
    const DATED: &str = "claude-sonnet-4-5-20250929";
    let store = shedding_store(&["a", "b"]);
    store
        .upsert_pin(&spelled_pin(&store, SPELLING, "a", 1, AT - 1_000))
        .unwrap();
    store
        .upsert_pin(&marked(
            spelled_pin(&store, DATED, "b", 2, AT - 5_000),
            AT - 60_000,
        ))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.resolve(SPELLING, DATED, AT).await.unwrap();
    assert_eq!(
        handle
            .pins()
            .await
            .unwrap()
            .iter()
            .map(|p| (p.model.as_str(), p.account_id.as_str()))
            .collect::<Vec<_>>(),
        [(DATED, "a")],
        "the newer spelled row wins the merge"
    );
    assert_eq!(
        released_clears(&drained(&mut events).await),
        [("b".to_string(), DATED.to_string(), "b".to_string())],
        "the replaced dated row leaves with its mark, so the mark clears as a release does"
    );
}

#[tokio::test]
async fn a_mark_a_round_makes_on_the_requests_own_pin_clears_as_moved_when_the_same_selection_moves_the_pin(
) {
    use phoebis_routing::RouterEvent;
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    for pin in [
        stored_pin(&store, "s1", "a", 1, 4.0),
        stored_pin(&store, "s0", "a", 2, 1.0),
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    // The model is cooled on a: the round still runs and marks s1, the heavier pin, with b as its destination, and
    // step 5 then releases the pin for the cooldown and the placement on b moves the session.
    store
        .upsert_cooldown(&CooldownRow {
            account_id: "a".into(),
            scope: CooldownScope::Model("claude-sonnet-5".into()),
            until: AT + 600_000,
            set_at: AT - 1_000,
            reason: "rate_limit".into(),
            window: "retry_after".into(),
            source_request_id: None,
            incarnation: 1,
            spend_share: None,
        })
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle.report(windows("a", 0.9, 0.3, AT)).await;
    handle.report(windows("b", 0.1, 0.1, AT)).await;
    handle.samples().await.unwrap();
    past_the_wait(&handle, &store, "a", AT).await;
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT))
                .await
                .unwrap()
        ),
        ("b".into(), None),
        "no rule moves the cooled account's pin, and the selection replaces it on b"
    );
    let (summary, record) = next_decision(&mut events).await;
    assert_eq!(summary.kind, "placed");
    assert_eq!(
        record.outcome.shed_marks.as_ref().map(|marks| marks
            .iter()
            .map(|m| m.session_id.as_str())
            .collect::<Vec<_>>()),
        Some(vec!["s1"]),
        "the round marked the request's own pin"
    );
    assert_eq!(
        record.outcome.mark_cleared, None,
        "no heal and no rule's move cleared it inside the selection"
    );
    let later = drained(&mut events).await;
    assert!(
        later
            .iter()
            .any(|event| matches!(event, RouterEvent::PinMigrated { reason, reasons, .. } if reason == "cooldown" && reasons.contains(&"cooldown"))),
        "{later:?}"
    );
    assert!(
        later.iter().any(|event| matches!(
            event,
            RouterEvent::MarkCleared {
                account_id,
                pin: PinKey { session_id, .. },
                reason: "moved",
                marked_at,
                marked_on,
                marked_window,
                projection: None,
                ..
            } if account_id == "a"
                && session_id == "s1"
                && *marked_at == AT
                && marked_on == "a"
                && marked_window == "five_hour"
        )),
        "the mark made and moved off in one selection is cleared as moved: {later:?}"
    );
}

#[tokio::test]
async fn the_purge_tick_writes_the_round_cut_past_the_longest_dwell() {
    let store = seeded(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.shedding.shed_dwell_seconds = 600;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    second_pool(&store, 1_800);
    // p's round is past p's own dwell and inside q's, the longest of the snapshot's pools, which sets the cut, and q's
    // round is past q's dwell.
    store.replace_shed_round("p", "a", 1, AT - 900_000).unwrap();
    store
        .replace_shed_round("q", "a", 1, AT - 2_000_000)
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let _handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let kept = wait_for_async(WRITTEN, || async {
        let rounds = store.load_routing().unwrap().shed_rounds;
        (rounds.len() == 1).then(|| rounds[0].pool_id.clone())
    })
    .await;
    assert_eq!(
        kept, "p",
        "the cut is now less the longest dwell of the snapshot's pools"
    );
}

#[tokio::test]
async fn after_a_restart_a_spent_pin_moves_to_a_sibling_whatever_its_former_pools_round_read() {
    use phoebis_routing::{FactValue, MigrateReason};
    let store = seeded(&["x", "a"]);
    store
        .set_pool_members(
            "p",
            &[("x", 0), ("a", 1)]
                .into_iter()
                .map(|(id, priority)| MemberWrite {
                    account_id: id.into(),
                    priority,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                })
                .collect::<Vec<_>>(),
            "test",
        )
        .unwrap();
    second_pool(&store, 900);
    store
        .upsert_pin(&stored_pin(&store, "s1", "x", 1, 0.0))
        .unwrap();
    // q's round on a a minute before the restart, run while a was q's member, and p never ran one there.
    store.replace_shed_round("q", "a", 1, AT - 60_000).unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle.report(windows("x", 1.0, 0.1, AT)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT))
                .await
                .unwrap()
        ),
        ("a".into(), Some(MigrateReason::Spent)),
        "the sibling fits by its bucket, and a former pool's round on it holds the move no more"
    );
    let (_, record) = next_decision(&mut events).await;
    let fit = record
        .walk
        .iter()
        .find(|step| step.step == "fit")
        .expect("the spent rule's fit table");
    let about_a = |key: &str| {
        fit.facts
            .iter()
            .find(|fact| fact.key == key && fact.account_id.as_deref() == Some("a"))
            .map(|fact| fact.value.clone())
    };
    assert_eq!(
        (fit.verdict, about_a("verdict"), about_a("runway_ms")),
        ("fits", Some(FactValue::Text("fits".into())), None),
        "an api_key sibling with no bucket reading fits with no runway"
    );
}

#[tokio::test]
async fn the_load_counts_name_the_standing_marks_and_the_stale_round_rows() {
    let store = seeded(&["a"]);
    second_pool(&store, 600);
    // s1's mark was made on a, where it sits, so it stands. s2 carries a mark made on another account, which no
    // longer stands. s3 is unmarked.
    for pin in [
        marked(stored_pin(&store, "s1", "a", 1, 0.0), AT - 60_000),
        PinRow {
            mark: Some(PinMark {
                at: AT - 60_000,
                on: "z".into(),
                window: FIVE_HOUR.into(),
                source: None,
            }),
            ..stored_pin(&store, "s2", "a", 2, 0.0)
        },
        stored_pin(&store, "s3", "a", 3, 0.0),
    ] {
        store.upsert_pin(&pin).unwrap();
    }
    // p's round a minute ago is inside its 900-second dwell. q's round is past its 600-second dwell, so its row is
    // stale.
    store.replace_shed_round("p", "a", 1, AT - 60_000).unwrap();
    store.replace_shed_round("q", "a", 1, AT - 700_000).unwrap();
    let handle = spawn(store, Arc::new(|| AT), RouterConfig::default()).unwrap();
    let counts = handle.load_counts();
    assert_eq!(
        (counts.pins, counts.shed_marks, counts.shed_rounds_stale),
        (3, 1, 1)
    );
}

#[tokio::test]
async fn a_credits_reading_that_flips_blocked_emits_one_change() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(credits_at(true, None, Some(0.2), 1_000))
        .await;
    handle.cooldowns().await.unwrap();
    handle
        .report(credits_at(false, Some("org_level_disabled"), None, 2_000))
        .await;
    match next_event(&mut events).await {
        RouterEvent::FastCreditsChanged {
            account_id,
            before,
            after,
            blocked_before,
            blocked_after,
            ..
        } => {
            assert_eq!(account_id, "a");
            assert_eq!(
                before.map(|b| (b.enabled, b.observed_at)),
                Some((true, 1_000))
            );
            assert_eq!((after.enabled, after.observed_at), (false, 2_000));
            assert_eq!(
                (blocked_before, blocked_after.as_deref()),
                (None, Some("org_level_disabled"))
            );
        }
        other => panic!("one credits change, not {other:?}"),
    }
    // The same refusal read again moves no edge.
    handle
        .report(credits_at(false, Some("org_level_disabled"), None, 3_000))
        .await;
    handle.cooldowns().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_sample_that_spends_a_window_emits_window_spent_once() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = Some(10 * HOUR);
    for (at, utilization, request_id) in [(1_000, 0.9, "r-0"), (2_000, 1.0, "r-1")] {
        handle
            .report(head_reading(at, utilization, reset, request_id))
            .await;
        handle.samples().await.unwrap();
    }
    match next_event(&mut events).await {
        RouterEvent::WindowSpent {
            account_id,
            window,
            utilization,
            reset: spent_reset,
            source,
            source_request_id,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                window.as_str(),
                utilization,
                spent_reset,
                source,
                source_request_id.as_deref()
            ),
            ("a", FIVE_HOUR, 1.0, reset, WindowSource::Head, Some("r-1"))
        ),
        other => panic!("one window spent, not {other:?}"),
    }
    // A reading that stays spent crosses nothing.
    handle.report(head_reading(3_000, 1.0, reset, "r-2")).await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_sample_with_a_later_reset_emits_window_reset_by_sample() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.6, Some(5 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    handle
        .report(observed(
            "a",
            sample_with(5 * HOUR + 1_000, FIVE_HOUR, 0.1, Some(10 * HOUR)),
        ))
        .await;
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            account_id,
            window,
            utilization_before,
            reset_at,
            next_reset,
            observed,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                window.as_str(),
                utilization_before,
                reset_at,
                next_reset,
                observed
            ),
            ("a", FIVE_HOUR, 0.6, 5 * HOUR, Some(10 * HOUR), "sample")
        ),
        other => panic!("one window reset, not {other:?}"),
    }
    // A newer reading of the same window names the same reset.
    handle
        .report(observed(
            "a",
            sample_with(5 * HOUR + 2_000, FIVE_HOUR, 0.2, Some(10 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

/// An idle account's 5-hour reset slides later with every poll, so a later reset passes the stored one only where the
/// reading was taken within `RESET_SLACK_MS` of it or past it.
#[tokio::test]
async fn a_later_reset_reads_the_stored_reset_passed_only_within_the_slack_of_the_reading_s_instant(
) {
    use phoebis_routing::{RouterEvent, RESET_SLACK_MS};
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            as_poll(sample_with(
                1_000,
                FIVE_HOUR,
                0.6,
                Some(2_000 + RESET_SLACK_MS + 1),
            )),
        ))
        .await;
    handle.samples().await.unwrap();
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 0.5, Some(5 * HOUR)),
        ))
        .await;
    let slid = handle
        .reported_samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == "a")
        .and_then(|(_, sample)| sample.reading(FIVE_HOUR))
        .expect("a stored reading");
    assert_eq!(
        slid.reset,
        Some(5 * HOUR),
        "a stored reset a millisecond past the slack still lies ahead, so the later one replaces it"
    );
    no_further_event(&mut events).await;
    handle
        .report(observed(
            "a",
            sample_with(5 * HOUR - RESET_SLACK_MS, FIVE_HOUR, 0.1, Some(10 * HOUR)),
        ))
        .await;
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            observed,
            ..
        } => assert_eq!(
            (reset_at, next_reset, observed),
            (5 * HOUR, Some(10 * HOUR), "sample")
        ),
        other => panic!("a stored reset at the slack's edge reads passed, not {other:?}"),
    }
}

/// The pair `with_tier` stores, as the snapshot resolves it.
const WITH_TIER_PAIR: (Option<&str>, Option<&str>) = (Some("max_5x"), Some(TIER));

/// The pair after `upgraded`, as the snapshot resolves it.
const UPGRADED_PAIR: (Option<&str>, Option<&str>) =
    (Some("max_20x"), Some("default_claude_max_20x"));

/// A profile read that found `account` on the Ultra tier (`default_claude_max_20x`), stored as the quota poll
/// stores it.
fn upgraded(store: &Store, account: &str) {
    store
        .set_profile(
            account,
            "{}",
            &SubscriptionRow {
                tier: Some("default_claude_max_20x".into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
}

/// The plan and the tier of the reading the router stores for `account`'s `window`.
async fn stored_pair(
    handle: &RouterHandle,
    account: &str,
    window: &str,
) -> (Option<String>, Option<String>) {
    let reading = handle
        .reported_samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == account)
        .and_then(|(_, sample)| sample.reading(window))
        .expect("a stored reading");
    (reading.plan, reading.tier)
}

/// A reset's pair as the event names it.
fn reset_pair(event: &phoebis_routing::RouterEvent) -> (Option<&str>, Option<&str>, &'static str) {
    match event {
        phoebis_routing::RouterEvent::WindowReset {
            plan,
            tier,
            observed,
            ..
        } => (plan.as_deref(), tier.as_deref(), *observed),
        other => panic!("a window reset, not {other:?}"),
    }
}

#[tokio::test]
async fn two_heads_readings_carry_the_plan_and_the_tier_the_account_held_when_each_was_taken() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, SEVEN_DAY, 0.3, Some(100 * HOUR)),
        ))
        .await;
    handle
        .report(head_reading(2_000, 0.4, Some(5 * HOUR), "r-1"))
        .await;
    for window in [SEVEN_DAY, FIVE_HOUR] {
        let (plan, tier) = stored_pair(&handle, "a", window).await;
        assert_eq!(
            (plan.as_deref(), tier.as_deref()),
            WITH_TIER_PAIR,
            "the {window} reading carries the account's pair"
        );
    }
    upgraded(&store, "a");
    handle.reload().await.unwrap();
    handle
        .report(head_reading(3_000, 0.5, Some(5 * HOUR), "r-2"))
        .await;
    let (plan, tier) = stored_pair(&handle, "a", FIVE_HOUR).await;
    assert_eq!(
        (plan.as_deref(), tier.as_deref()),
        UPGRADED_PAIR,
        "a reading taken after the profile read carries the new pair"
    );
    let (plan, tier) = stored_pair(&handle, "a", SEVEN_DAY).await;
    assert_eq!(
        (plan.as_deref(), tier.as_deref()),
        WITH_TIER_PAIR,
        "a reading taken before it keeps the pair it was taken under"
    );
}

#[tokio::test]
async fn a_profile_read_between_a_reading_and_the_sample_past_its_reset_leaves_the_reading_s_pair_on_the_reset(
) {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.6, Some(5 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    upgraded(&store, "a");
    handle.reload().await.unwrap();
    handle
        .report(observed(
            "a",
            sample_with(5 * HOUR + 1_000, FIVE_HOUR, 0.1, Some(10 * HOUR)),
        ))
        .await;
    let event = next_window_edge(&mut events).await;
    let (plan, tier, observed) = reset_pair(&event);
    assert_eq!(
        ((plan, tier), observed),
        (WITH_TIER_PAIR, "sample"),
        "the reset names the pair of the reading it replaces, never the account's at the emission"
    );
}

#[tokio::test]
async fn a_profile_read_between_a_reading_and_the_clock_passing_its_reset_leaves_the_reading_s_pair_on_the_reset(
) {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.7, Some(2_000)),
        ))
        .await;
    handle.samples().await.unwrap();
    upgraded(&store, "a");
    handle.reload().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    let event = next_window_edge(&mut events).await;
    let (plan, tier, observed) = reset_pair(&event);
    assert_eq!(
        ((plan, tier), observed),
        (WITH_TIER_PAIR, "clock"),
        "the clock walk names the pair of the reading whose reset passed, never the account's at the emission"
    );
}

#[tokio::test]
async fn a_reading_s_pair_is_written_with_its_row_and_names_the_reset_a_restart_restores() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.7, Some(2_000)),
        ))
        .await;
    let rows = wait_for(WRITTEN, || {
        let windows = store.load_routing().unwrap().windows;
        (windows.len() == 1).then_some(windows)
    })
    .await;
    assert_eq!(
        (rows[0].plan.as_deref(), rows[0].tier.as_deref()),
        WITH_TIER_PAIR,
        "the row carries the reading's pair"
    );
    handle.shutdown().await.unwrap();
    // The account moved while the router was down, and its reset passed.
    upgraded(&store, "a");
    now.store(3_000, Ordering::SeqCst);
    let again = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = again.subscribe_events();
    let event = next_window_edge(&mut events).await;
    assert!(
        matches!(
            event,
            phoebis_routing::RouterEvent::WindowReset { restored: true, .. }
        ),
        "{event:?}"
    );
    let (plan, tier, observed) = reset_pair(&event);
    assert_eq!(
        ((plan, tier), observed),
        (WITH_TIER_PAIR, "clock"),
        "the restored reading names the pair its row carried"
    );
}

/// The pair comes from the reading alone: a reading restored from a row that carries none names none on its reset,
/// whatever pair the account holds at the start.
#[tokio::test]
async fn a_row_loaded_without_the_pair_names_no_pair_on_its_reset() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    assert!(store
        .upsert_window(&window_row("a", FIVE_HOUR, 0.7, Some(2_000)))
        .unwrap());
    let now = Arc::new(AtomicI64::new(3_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let event = next_window_edge(&mut events).await;
    let (plan, tier, observed) = reset_pair(&event);
    assert_eq!(((plan, tier), observed), ((None, None), "clock"));
}

#[tokio::test]
async fn a_poll_naming_a_head_s_reset_to_the_millisecond_names_the_same_reset() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    // A head names the reset in whole seconds. The poll names the same reset to the millisecond.
    let reset = 5 * HOUR;
    handle
        .report(head_reading(1_000, 0.4, Some(reset), "r-1"))
        .await;
    handle.samples().await.unwrap();
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 0.4, Some(reset + 400))),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn the_purge_tick_emits_a_clock_reset_once_per_window() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.7, Some(2_000)),
        ))
        .await;
    handle.samples().await.unwrap();
    // The ticks before the reset find it ahead and announce nothing.
    no_further_event(&mut events).await;
    now.store(2_000, Ordering::SeqCst);
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            account_id,
            window,
            utilization_before,
            reset_at,
            next_reset,
            observed,
            restored,
            at,
            plan,
            tier,
        } => assert_eq!(
            (
                account_id.as_str(),
                window.as_str(),
                utilization_before,
                reset_at,
                next_reset,
                observed,
                restored,
                at,
                plan,
                tier
            ),
            // The account holds no stored profile, so its reading carries no pair.
            ("a", FIVE_HOUR, 0.7, 2_000, None, "clock", false, 2_000, None, None)
        ),
        other => panic!("the tick's reset, not {other:?}"),
    }
    // The stored reading still names the passed reset at every later tick.
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_clock_reset_then_a_new_sample_names_one_row() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.7, Some(2_000)),
        ))
        .await;
    handle.samples().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowReset {
            reset_at: 2_000,
            observed: "clock",
            ..
        }
    ));
    // The poll's next reading names the window after it: the reset it passed was announced already.
    handle
        .report(observed(
            "a",
            as_poll(sample_with(3_000, FIVE_HOUR, 0.1, Some(2_000 + 5 * HOUR))),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_clock_reset_a_later_poll_names_to_the_millisecond_is_announced_once() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.7, Some(2_000)),
        ))
        .await;
    handle.samples().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowReset {
            reset_at: 2_000,
            observed: "clock",
            ..
        }
    ));
    // The head named the reset in whole seconds and the poll names it to the millisecond, 400 ms later.
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_100, FIVE_HOUR, 0.1, Some(2_400))),
        ))
        .await;
    handle.samples().await.unwrap();
    // The ticks past the poll's reset walk a reading whose reset passed, which names the one announced already.
    now.store(2_400, Ordering::SeqCst);
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_reading_with_no_reset_after_the_stored_reset_passed_announces_it_by_sample() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.6, Some(2_000)),
        ))
        .await;
    handle.samples().await.unwrap();
    // Parked, so no purge tick runs between the clock passing the reset and the reading that replaces it.
    let park = handle.park_loop().await;
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(3_000, FIVE_HOUR, 0.0, None)))
        .await;
    drop(park);
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            account_id,
            window,
            utilization_before,
            reset_at,
            next_reset,
            observed,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                window.as_str(),
                utilization_before,
                reset_at,
                next_reset,
                observed
            ),
            ("a", FIVE_HOUR, 0.6, 2_000, None, "sample")
        ),
        other => panic!("the passed reset, by the sample, not {other:?}"),
    }
    // A reload runs the purge, whose clock walk finds nothing left to announce.
    handle.reload().await.unwrap();
    no_further_event(&mut events).await;
}

/// The next window edge on `events`, a spent, a cleared or a reset window, every other event passed over.
async fn next_window_edge(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> phoebis_routing::RouterEvent {
    use phoebis_routing::RouterEvent;
    loop {
        let event = next_event(events).await;
        if matches!(
            event,
            RouterEvent::WindowSpent { .. }
                | RouterEvent::WindowCleared { .. }
                | RouterEvent::WindowReset { .. }
        ) {
            return event;
        }
    }
}

/// A cleared window's edge as `(account, window, before, after, reset, source, request)`.
type Cleared<'a> = (
    &'a str,
    &'a str,
    f64,
    f64,
    Option<i64>,
    WindowSource,
    Option<&'a str>,
);

fn cleared_of(event: &phoebis_routing::RouterEvent) -> Option<Cleared<'_>> {
    match event {
        phoebis_routing::RouterEvent::WindowCleared {
            account_id,
            window,
            utilization_before,
            utilization,
            reset,
            source,
            source_request_id,
            ..
        } => Some((
            account_id.as_str(),
            window.as_str(),
            *utilization_before,
            *utilization,
            *reset,
            *source,
            source_request_id.as_deref(),
        )),
        _ => None,
    }
}

#[tokio::test]
async fn a_sample_whose_every_window_falls_under_its_limit_under_the_same_reset_emits_window_cleared_for_each(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let resets = [
        (FIVE_HOUR, 5 * HOUR),
        (SEVEN_DAY, 7 * 24 * HOUR),
        (SEVEN_DAY_FABLE, 7 * 24 * HOUR),
    ];
    let read = |at: i64, utilization: f64| {
        resets.iter().fold(sample(at), |sample, (window, reset)| {
            sample.with(
                window,
                WindowReading {
                    utilization,
                    reset: Some(*reset),
                    observed_at: at,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            )
        })
    };
    handle.report(observed("a", read(1_000, 1.0))).await;
    handle.samples().await.unwrap();
    for (window, _) in resets {
        assert!(matches!(
            next_event(&mut events).await,
            RouterEvent::WindowSpent { window: spent, .. } if spent == window
        ));
    }
    // Every window of the answer falls together under its unchanged reset, as an account's one-time reset grant reads.
    // The head's percent from 0.0 bounds each stored 1.0 at 0.01.
    handle.report(observed("a", read(2_000, 0.0))).await;
    handle.samples().await.unwrap();
    for (window, reset) in resets {
        let event = next_event(&mut events).await;
        assert_eq!(
            cleared_of(&event),
            Some((
                "a",
                window,
                1.0,
                0.01,
                Some(reset),
                WindowSource::Head,
                None
            )),
            "{event:?}"
        );
        assert!(matches!(
            event,
            RouterEvent::WindowCleared { at: 4_000, .. }
        ));
    }
    // A clear announces no reset.
    no_further_event(&mut events).await;
}

/// A dropped window's edge as `(account, window, before, after, reset)`.
fn dropped_of(event: &phoebis_routing::RouterEvent) -> Option<(&str, &str, f64, f64, i64)> {
    match event {
        phoebis_routing::RouterEvent::WindowDropped {
            account_id,
            window,
            utilization_before,
            utilization,
            reset,
            ..
        } => Some((
            account_id.as_str(),
            window.as_str(),
            *utilization_before,
            *utilization,
            *reset,
        )),
        _ => None,
    }
}

/// A clean poll's sample of the given weekly windows, each read under `reset`.
fn weekly_poll(at: i64, reset: i64, readings: &[(&str, f64)]) -> UtilizationSample {
    let sample = readings
        .iter()
        .fold(sample(at), |sample, (window, utilization)| {
            sample.with(
                window,
                WindowReading {
                    utilization: *utilization,
                    reset: Some(reset),
                    observed_at: at,
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            )
        });
    UtilizationSample {
        poll: true,
        ..sample
    }
}

#[tokio::test]
async fn a_poll_more_than_five_points_under_the_stored_weekly_reading_under_one_reset_writes_window_dropped(
) {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 7 * 24 * HOUR;
    handle
        .report(observed(
            "a",
            weekly_poll(
                1_000,
                reset,
                &[
                    (SEVEN_DAY, 0.50),
                    (SEVEN_DAY_FABLE, 0.50),
                    ("seven_day_opus", 0.55),
                ],
            ),
        ))
        .await;
    handle.samples().await.unwrap();
    // Six points under the stored figure on one window and five on the other two, under the reset each stored reading
    // names. The two figures of 0.55 to 0.50, subtracted as floats, read more than 0.05. The poll behind it reads the
    // same figures and writes the event.
    for at in [2_000, 3_000] {
        handle
            .report(observed(
                "a",
                weekly_poll(
                    at,
                    reset,
                    &[
                        (SEVEN_DAY, 0.44),
                        (SEVEN_DAY_FABLE, 0.45),
                        ("seven_day_opus", 0.50),
                    ],
                ),
            ))
            .await;
        handle.samples().await.unwrap();
    }
    let event = next_event(&mut events).await;
    assert_eq!(
        dropped_of(&event),
        Some(("a", SEVEN_DAY, 0.50, 0.44, reset)),
        "{event:?}"
    );
    assert!(
        matches!(
            &event,
            phoebis_routing::RouterEvent::WindowDropped {
                plan: None,
                tier: None,
                pair_changed: false,
                ..
            }
        ),
        "an unchanged pair: {event:?}"
    );
    // A fall of five points is no drop.
    no_further_event(&mut events).await;
}

/// The restarts the task sent since the last read, as `(account, incarnation)`. Read once the task answered a command
/// behind the report, since the task sends a report's restart in that report's turn.
fn restarts_of(
    restarts: &mut tokio::sync::broadcast::Receiver<WindowRestart>,
) -> Vec<(String, u64)> {
    std::iter::from_fn(|| restarts.try_recv().ok())
        .map(|restart| (restart.account_id, restart.incarnation))
        .collect()
}

#[tokio::test]
async fn a_later_reset_ahead_of_the_stored_one_s_turn_signals_a_restart_where_the_figure_fell() {
    let head_at = 2_000;
    let later = head_at + 5 * HOUR;
    let one = || vec![("a".to_string(), 1)];
    // The stored reset, the stored figure and the head's, whether the head reads the reset in turn, and the restarts:
    // a fall of one point over a reset two hours ahead, the same fall over a reset at the last instant the head reads
    // in turn, and an idle 5-hour window's slide at 0.
    for (row, stored_reset, before, after, in_turn, restarts_expected) in [
        (
            "two hours ahead",
            head_at + 2 * HOUR,
            0.40,
            0.39,
            false,
            one(),
        ),
        (
            "in turn",
            head_at + RESET_SLACK_MS,
            0.40,
            0.39,
            true,
            vec![],
        ),
        ("idle", head_at + 2 * HOUR, 0.0, 0.0, false, vec![]),
    ] {
        let store = seeded_oauth(&["a"]);
        let now = Arc::new(AtomicI64::new(4_000));
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        let mut events = handle.subscribe_events();
        let mut restarts = handle.subscribe_restarts();
        handle
            .report(observed(
                "a",
                as_poll(sample_with(1_000, FIVE_HOUR, before, Some(stored_reset))),
            ))
            .await;
        handle.samples().await.unwrap();
        handle
            .report(observed(
                "a",
                sample_with(head_at, FIVE_HOUR, after, Some(later)),
            ))
            .await;
        handle.samples().await.unwrap();
        let reset = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, phoebis_routing::RouterEvent::WindowReset { .. }));
        assert_eq!(reset, in_turn, "{row}: the reset's event");
        assert_eq!(restarts_of(&mut restarts), restarts_expected, "{row}");
    }
}

#[tokio::test]
async fn a_weekly_fall_under_its_standing_reset_signals_a_restart_at_its_first_reading_alone() {
    let store = seeded_oauth(&["a", "b", "c"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let mut restarts = handle.subscribe_restarts();
    let none = Vec::<(String, u64)>::new;
    let reset = 7 * 24 * HOUR;
    for (account, stored) in [
        ("a", weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.60)])),
        ("b", weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.60)])),
        (
            "c",
            weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.60), (SEVEN_DAY_FABLE, 0.60)]),
        ),
    ] {
        handle.report(observed(account, stored)).await;
        handle.samples().await.unwrap();
    }
    assert_eq!(
        restarts_of(&mut restarts),
        none(),
        "a first reading shows no fall"
    );

    // The head that opens the pending drop shows the restart, and the later head and the poll read the same one.
    handle
        .report(observed("a", seven_day_head(2_000, reset, 0.10)))
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        restarts_of(&mut restarts),
        vec![("a".to_string(), 1)],
        "the first head"
    );
    handle
        .report(observed("a", seven_day_head(2_500, reset, 0.10)))
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        restarts_of(&mut restarts),
        none(),
        "a later head under the pending drop"
    );
    handle
        .report(observed(
            "a",
            weekly_poll(3_000, reset, &[(SEVEN_DAY, 0.10)]),
        ))
        .await;
    handle.samples().await.unwrap();
    let event = next_event(&mut events).await;
    assert_eq!(
        dropped_of(&event),
        Some(("a", SEVEN_DAY, 0.60, 0.10, reset)),
        "{event:?}"
    );
    assert_eq!(
        restarts_of(&mut restarts),
        none(),
        "the poll that dates the drop"
    );

    // With no head before it, the poll is the first reading to show the fall.
    handle
        .report(observed(
            "b",
            weekly_poll(3_000, reset, &[(SEVEN_DAY, 0.10)]),
        ))
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        restarts_of(&mut restarts),
        vec![("b".to_string(), 1)],
        "the poll's own fall"
    );

    // Two weekly windows of one head fall together, and the report signals once.
    handle
        .report(observed(
            "c",
            seven_day_head(2_000, reset, 0.10).with(
                SEVEN_DAY_FABLE,
                WindowReading {
                    utilization: 0.10,
                    reset: Some(reset),
                    observed_at: 2_000,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        restarts_of(&mut restarts),
        vec![("c".to_string(), 1)],
        "one report with two falls"
    );
}

/// A head's sample of the 7-day window under `reset`.
fn seven_day_head(at: i64, reset: i64, utilization: f64) -> UtilizationSample {
    sample(at).with(
        SEVEN_DAY,
        WindowReading {
            utilization,
            reset: Some(reset),
            observed_at: at,
            source: WindowSource::Head,
            plan: None,
            tier: None,
        },
    )
}

#[tokio::test]
async fn a_poll_after_a_head_s_fall_compares_the_figure_the_head_found_and_not_the_one_it_stored() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 7 * 24 * HOUR;
    for report in [
        weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.61)]),
        // The fold stores the head's 0.40 as 0.41, the top of the head's percent.
        seven_day_head(2_000, reset, 0.40),
        weekly_poll(3_000, reset, &[(SEVEN_DAY, 0.30)]),
    ] {
        handle.report(observed("a", report)).await;
        handle.samples().await.unwrap();
    }
    let event = next_event(&mut events).await;
    assert_eq!(
        dropped_of(&event),
        Some(("a", SEVEN_DAY, 0.61, 0.30, reset)),
        "{event:?}"
    );
    // The stored 0.41 stands more than five points over the poll too, and writes no second event.
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_poll_that_writes_no_drop_clears_the_figure_a_head_found() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 7 * 24 * HOUR;
    for report in [
        weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.61)]),
        seven_day_head(2_000, reset, 0.0),
        // Three points under the 0.61 the head found: no drop.
        weekly_poll(3_000, reset, &[(SEVEN_DAY, 0.58)]),
        weekly_poll(3_500, reset, &[(SEVEN_DAY, 0.40)]),
        // The poll behind the first reading of the fall writes the event.
        weekly_poll(4_000, reset, &[(SEVEN_DAY, 0.40)]),
    ] {
        handle.report(observed("a", report)).await;
        handle.samples().await.unwrap();
    }
    let event = next_event(&mut events).await;
    assert_eq!(
        dropped_of(&event),
        Some(("a", SEVEN_DAY, 0.58, 0.40, reset)),
        "the poll at 0.40 compares the figure the poll before it stored: {event:?}"
    );
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_fall_that_a_poll_reads_first_writes_its_event_at_the_next_poll_with_the_pairs_change() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 7 * 24 * HOUR;
    let mut drops = || {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                RouterEvent::WindowDropped {
                    utilization_before,
                    utilization,
                    plan,
                    tier,
                    pair_changed,
                    ..
                } => Some((utilization_before, utilization, plan, tier, pair_changed)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for report in [
        weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.60)]),
        weekly_poll(2_000, reset, &[(SEVEN_DAY, 0.10)]),
    ] {
        handle.report(observed("a", report)).await;
        handle.samples().await.unwrap();
    }
    let first = drops();
    assert!(
        first.is_empty(),
        "the poll that first reads the fall: {first:?}"
    );
    // The profile read that the fall asked for found the account upgraded.
    upgraded(&store, "a");
    handle.reload().await.unwrap();
    handle
        .report(observed(
            "a",
            weekly_poll(3_000, reset, &[(SEVEN_DAY, 0.12)]),
        ))
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        drops(),
        vec![(
            0.60,
            0.12,
            Some("max_5x".to_string()),
            Some(TIER.to_string()),
            true
        )],
        "the poll behind it writes the event with the pair before the fall and its change"
    );
}

#[tokio::test]
async fn a_fall_under_a_reading_that_names_no_pair_writes_no_pair_change() {
    use phoebis_routing::RouterEvent;
    // No profile read stands for the account, so its first readings name no plan and no tier.
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 7 * 24 * HOUR;
    for report in [
        weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.60)]),
        weekly_poll(2_000, reset, &[(SEVEN_DAY, 0.10)]),
    ] {
        handle.report(observed("a", report)).await;
        handle.samples().await.unwrap();
    }
    upgraded(&store, "a");
    handle.reload().await.unwrap();
    handle
        .report(observed(
            "a",
            weekly_poll(3_000, reset, &[(SEVEN_DAY, 0.12)]),
        ))
        .await;
    handle.samples().await.unwrap();
    let drops: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            RouterEvent::WindowDropped {
                plan,
                tier,
                pair_changed,
                ..
            } => Some((plan, tier, pair_changed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        drops,
        vec![(None::<String>, None::<String>, false)],
        "the account's first profile read names a pair where the reading before the fall named none"
    );
}

#[tokio::test]
async fn a_passed_reset_clears_the_figure_a_head_found_so_the_next_week_s_poll_writes_no_drop() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 3_000;
    for report in [
        weekly_poll(1_000, reset, &[(SEVEN_DAY, 0.61)]),
        seven_day_head(2_000, reset, 0.0),
        // Past the stored reset, naming the next week's.
        weekly_poll(3_500, reset + 7 * 24 * HOUR, &[(SEVEN_DAY, 0.02)]),
    ] {
        handle.report(observed("a", report)).await;
        handle.samples().await.unwrap();
    }
    let event = next_event(&mut events).await;
    assert!(
        matches!(&event, RouterEvent::WindowReset { window, reset_at: 3_000, .. } if window == SEVEN_DAY),
        "{event:?}"
    );
    // The head's 0.61 belongs to the week that reset, so the new week's 0.02 writes no drop.
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_head_whose_one_window_falls_under_its_limit_emits_window_cleared_for_that_window_alone()
{
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let (five_hour, week) = (Some(5 * HOUR), Some(7 * 24 * HOUR));
    let head = |at: i64, five_hour_utilization: f64, request_id: &str| Report::Sample {
        account_id: "a".into(),
        sample: sample(at)
            .with(
                FIVE_HOUR,
                WindowReading {
                    utilization: five_hour_utilization,
                    reset: five_hour,
                    observed_at: at,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            )
            .with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 1.0,
                    reset: week,
                    observed_at: at,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        incarnation: 1,
        source_request_id: Some(request_id.into()),
    };
    handle.report(head(1_000, 1.0, "r-1")).await;
    handle.samples().await.unwrap();
    for window in [FIVE_HOUR, SEVEN_DAY] {
        assert!(matches!(
            next_event(&mut events).await,
            RouterEvent::WindowSpent { window: spent, .. } if spent == window
        ));
    }
    // The head's percent from 0.0 bounds the stored 1.0 at 0.01.
    handle.report(head(2_000, 0.0, "r-2")).await;
    handle.samples().await.unwrap();
    let event = next_event(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.01,
            five_hour,
            WindowSource::Head,
            Some("r-2")
        )),
        "{event:?}"
    );
    // The week still reads its limit under its reset, which crosses nothing.
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_reading_under_the_limit_with_a_reset_past_the_slack_emits_window_reset_and_no_clear() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 1.0, Some(5 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowSpent { .. }
    ));
    handle
        .report(observed(
            "a",
            sample_with(5 * HOUR + 1_000, FIVE_HOUR, 0.1, Some(10 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            utilization_before,
            reset_at,
            next_reset,
            observed,
            ..
        } => assert_eq!(
            (utilization_before, reset_at, next_reset, observed),
            (1.0, 5 * HOUR, Some(10 * HOUR), "sample")
        ),
        other => panic!("the passed reset, not {other:?}"),
    }
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_reading_at_the_limit_after_a_clear_emits_window_spent_again_under_the_reset_the_prediction_announced(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.99, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    // The 200 000 token body books 0.02, which takes the 0.99 reading past the wall before any usage lands.
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(matches!(
        next_window_edge(&mut events).await,
        RouterEvent::WindowSpent {
            source: WindowSource::Predicted,
            ..
        }
    ));
    // The head that confirms the wall names the reset the prediction announced, and announces nothing. The next head's
    // percent from 0.0 bounds the stored 1.0 at 0.01.
    for (at, utilization, request_id) in [(3_000, 1.0, "r-2"), (4_000, 0.0, "r-3")] {
        now.store(at, Ordering::SeqCst);
        handle
            .report(head_reading(at, utilization, Some(reset), request_id))
            .await;
        handle.samples().await.unwrap();
    }
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.01,
            Some(reset),
            WindowSource::Head,
            Some("r-3")
        )),
        "{event:?}"
    );
    now.store(5_000, Ordering::SeqCst);
    handle
        .report(head_reading(5_000, 1.0, Some(reset), "r-4"))
        .await;
    handle.samples().await.unwrap();
    match next_window_edge(&mut events).await {
        RouterEvent::WindowSpent {
            utilization,
            reset: spent_reset,
            source,
            source_request_id,
            ..
        } => assert_eq!(
            (
                utilization,
                spent_reset,
                source,
                source_request_id.as_deref()
            ),
            (1.0, Some(reset), WindowSource::Head, Some("r-4"))
        ),
        other => panic!("the window spent again, not {other:?}"),
    }
}

/// A wall the prediction announced over a stored reading under it: a later head that reads the window under its limit
/// under the same reset clears it as a stored reading's fall does, from the prediction's figure, and takes the
/// announcement with it, so the next head at the limit announces the wall anew.
#[tokio::test]
async fn a_head_under_the_limit_after_a_predicted_spent_edge_emits_window_cleared_from_the_predictions_figure(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.99, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    // The 200 000 token body books 0.02, which takes the 0.99 reading past the wall before any usage lands.
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(matches!(
        next_window_edge(&mut events).await,
        RouterEvent::WindowSpent {
            source: WindowSource::Predicted,
            ..
        }
    ));
    let predicted = predicted_reading(&handle, FIVE_HOUR).await.utilization;
    assert!(
        predicted >= 1.0,
        "the booking holds the prediction at {predicted}"
    );
    // The booking counts in the prediction alone, so the router's own figure is the stored 0.99, which the head's
    // percent from 0.5 bounds at 0.51.
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(head_reading(3_000, 0.5, Some(reset), "r-2"))
        .await;
    handle.samples().await.unwrap();
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            predicted,
            0.51,
            Some(reset),
            WindowSource::Head,
            Some("r-2")
        )),
        "{event:?}"
    );
    now.store(4_000, Ordering::SeqCst);
    handle
        .report(head_reading(4_000, 1.0, Some(reset), "r-3"))
        .await;
    handle.samples().await.unwrap();
    match next_window_edge(&mut events).await {
        RouterEvent::WindowSpent {
            utilization,
            reset: spent_reset,
            source,
            source_request_id,
            ..
        } => assert_eq!(
            (
                utilization,
                spent_reset,
                source,
                source_request_id.as_deref()
            ),
            (1.0, Some(reset), WindowSource::Head, Some("r-3"))
        ),
        other => panic!("the window spent anew, not {other:?}"),
    }
}

/// A charge that takes the prediction to the wall, then a head a point under it under the same reset: the router's own
/// figure lies inside the head's percent, so the stored figure stays at the wall, and the recovery and the edges read
/// that figure rather than the head's cut.
#[tokio::test]
async fn a_head_whose_percent_holds_the_predicted_wall_clears_neither_the_wall_nor_the_cooldown() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.995, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    now.store(1_500, Ordering::SeqCst);
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 5 * HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    // 50 000 input tokens charge 0.005, which takes the 0.995 reading to the wall.
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 2_000,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 50_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    assert!(matches!(
        next_window_edge(&mut events).await,
        RouterEvent::WindowSpent {
            source: WindowSource::Predicted,
            ..
        }
    ));
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(head_reading(3_000, 0.99, Some(reset), "r-2"))
        .await;
    handle.samples().await.unwrap();
    let after = drained(&mut events).await;
    assert!(
        after.is_empty(),
        "the stored figure still reads the wall, so the head clears nothing: {after:?}"
    );
    assert!(
        handle
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|row| row.account_id == "a" && row.scope == CooldownScope::Account),
        "the window still reads its limit, so the cooldown stands"
    );
}

/// A head inside the margin over a stored wall is held at the wall's figure. Its own percent from 0.99 reaches the wall
/// and no further, so the charge since the wall lifts nothing, the stored figure keeps the wall's and the charge
/// leaves.
#[tokio::test]
async fn a_head_the_hold_raises_keeps_the_walls_figure_over_the_charges_since() {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    // 50 000 input tokens charge 0.005 over the wall.
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 2_000,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 50_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(head_reading(3_000, 0.99, Some(reset), "r-2"))
        .await;
    let stored = handle
        .reported_samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == "a")
        .and_then(|(_, sample)| sample.reading(FIVE_HOUR))
        .expect("a stored reading");
    assert_eq!(
        stored.utilization, 1.0,
        "the held head keeps the wall's figure"
    );
    let after = drained(&mut events).await;
    assert!(
        after.is_empty(),
        "the held head reads the wall, so it emits no edge: {after:?}"
    );
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        1.0,
        "the stored figure holds the charge, so the charge leaves with the fold"
    );
}

/// A reading below the wall that reads the announced reset passed leaves that cycle's announcement standing, and a
/// later head of the next cycle under the limit clears no wall of the cycle that already reset.
#[tokio::test]
async fn a_head_under_the_limit_clears_no_wall_whose_announcement_is_of_a_cycle_that_already_reset()
{
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset ahead of the reading that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 2 * HOUR;
    let next_reset = reset + 5 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    let passed = reset + HOUR;
    now.store(passed, Ordering::SeqCst);
    handle
        .report(head_reading(passed, 0.5, Some(next_reset), "r-1"))
        .await;
    assert!(matches!(
        next_window_edge(&mut events).await,
        RouterEvent::WindowReset { reset_at, .. } if reset_at == reset
    ));
    let later = passed + HOUR;
    now.store(later, Ordering::SeqCst);
    handle
        .report(head_reading(later, 0.4, Some(next_reset), "r-2"))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_clear_whose_reading_names_no_reset_carries_the_stored_reset() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(4_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = 5 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowSpent { .. }
    ));
    // Read before the stored reset, so no reset passed and the window stands under the reset it read.
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)))
        .await;
    handle.samples().await.unwrap();
    let event = next_event(&mut events).await;
    let (account, window, before, after, reset_named, source, request) =
        cleared_of(&event).unwrap_or_else(|| panic!("a clear, not {event:?}"));
    assert_eq!(
        (account, window, before, reset_named, source, request),
        ("a", FIVE_HOUR, 1.0, Some(reset), WindowSource::Head, None),
        "{event:?}"
    );
    // Under the stored reset the head's percent from 0.4 bounds the stored 1.0 at 0.41.
    assert!(close(Some(after), 0.41), "{event:?}");
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn the_clock_walk_announces_the_stored_reset_once_it_passes_after_a_clear() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    for (at, utilization) in [(1_000, 1.0), (1_500, 0.0)] {
        handle
            .report(observed(
                "a",
                sample_with(at, FIVE_HOUR, utilization, Some(2_000)),
            ))
            .await;
        handle.samples().await.unwrap();
    }
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowSpent { .. }
    ));
    // The second head's percent from 0.0 bounds the stored 1.0 at 0.01.
    let event = next_event(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.01,
            Some(2_000),
            WindowSource::Head,
            None
        )),
        "{event:?}"
    );
    now.store(2_000, Ordering::SeqCst);
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            utilization_before,
            reset_at,
            next_reset,
            observed,
            at,
            ..
        } => assert_eq!(
            (utilization_before, reset_at, next_reset, observed, at),
            (0.01, 2_000, None, "clock", 2_000)
        ),
        other => panic!("the tick's reset, not {other:?}"),
    }
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_member_entering_its_wind_down_emits_once() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    // Forty minutes out, ten more than the pool's thirty-minute wind-down.
    let end = 1_000 + 40 * 60_000;
    store
        .set_canceled("a", Cancellation::On { ends_at: Some(end) })
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    // The ticks before the end comes inside the window find the member outside it.
    no_further_event(&mut events).await;
    let inside = end - 20 * 60_000;
    now.store(inside, Ordering::SeqCst);
    match next_event(&mut events).await {
        RouterEvent::WindDownStarted {
            account_id,
            pool_id,
            ends_at,
            wind_down_minutes,
            end_source,
            restored,
            at,
        } => assert_eq!(
            (
                account_id.as_str(),
                pool_id.as_str(),
                ends_at,
                wind_down_minutes,
                end_source,
                restored,
                at
            ),
            ("a", "p", end, 30, "canceled", false, inside)
        ),
        other => panic!("the wind-down's start, not {other:?}"),
    }
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_member_entering_its_wind_down_before_its_trial_ends_names_the_trial_as_the_end() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    // Forty minutes out, ten more than the pool's thirty-minute wind-down, with no flag set.
    let end = 1_000 + 40 * 60_000;
    store
        .set_profile(
            "a",
            "{}",
            &SubscriptionRow {
                tier: Some("default_claude_max_5x".into()),
                since: Some(1),
                trial_ends_at: Some(end),
            },
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    no_further_event(&mut events).await;
    now.store(end - 20 * 60_000, Ordering::SeqCst);
    match next_event(&mut events).await {
        RouterEvent::WindDownStarted {
            account_id,
            ends_at,
            end_source,
            ..
        } => assert_eq!(
            (account_id.as_str(), ends_at, end_source),
            ("a", end, "trial")
        ),
        other => panic!("the wind-down's start, not {other:?}"),
    }
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_decision_records_a_canceled_members_term_under_the_canceled_key() {
    let store = seeded(&["a"]);
    // Ten minutes before the pool's thirty-minute wind-down opens, so the member stays the pick.
    let end = 1_000 + 40 * 60_000;
    store
        .set_canceled("a", Cancellation::On { ends_at: Some(end) })
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .select(request(&handle, Some("s1"), 1_000))
        .await
        .unwrap();
    let (_, record) = next_decision(&mut events).await;
    let record = serde_json::to_value(&record).unwrap();
    let candidate = &record["candidates"][0];
    assert_eq!(
        (
            &candidate["account_id"],
            &candidate["term"]["canceled"],
            &candidate["term"]["ends_at"]
        ),
        (
            &serde_json::json!("a"),
            &serde_json::json!(true),
            &serde_json::json!(end)
        ),
        "the record names the operator's flag as the account view does: {candidate}"
    );
}

#[tokio::test]
async fn the_first_purge_after_the_start_marks_the_wind_downs_it_finds_as_restored() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    // Ten minutes out under the pool's thirty-minute wind-down: inside the window before the router starts.
    let end = 1_000 + 10 * 60_000;
    store
        .set_canceled("a", Cancellation::On { ends_at: Some(end) })
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    match next_event(&mut events).await {
        RouterEvent::WindDownStarted {
            account_id,
            ends_at,
            restored,
            ..
        } => assert_eq!((account_id.as_str(), ends_at, restored), ("a", end, true)),
        other => panic!("the restored wind-down, not {other:?}"),
    }
    // A reload's purge is a later one, and the member is still inside the window it already announced.
    handle.reload().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_member_past_its_canceled_end_emits_no_wind_down() {
    let store = seeded(&["a"]);
    // The subscription ended an hour before the router starts: past its end, not entering a wind-down.
    store
        .set_canceled(
            "a",
            Cancellation::On {
                ends_at: Some(1_000 - HOUR),
            },
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn the_purge_groups_expired_entries_per_root() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.affinity_ttl_seconds = 1;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    for (root, chain) in [("s1", &["x1", "x2", "x3"][..]), ("s2", &["y1"][..])] {
        handle
            .report(Report::Affinity(AffinityReport {
                root: root.into(),
                message_boundaries: Arc::from(
                    chain.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
                ),
                ..affinity_report(&handle, "a", 1_000)
            }))
            .await;
        assert!(matches!(
            next_event(&mut events).await,
            RouterEvent::CacheMapWritten { .. }
        ));
    }
    // Past the pool's one-second TTL for every entry.
    now.store(2_001, Ordering::SeqCst);
    let mut expired = Vec::new();
    for _ in 0..2 {
        match next_event(&mut events).await {
            RouterEvent::WarmPrefixExpired {
                account_id,
                key_id,
                root,
                model,
                entries,
                cause,
                at,
                ..
            } => expired.push((account_id, key_id, root, model, entries, cause, at)),
            other => panic!("an expiry per root, not {other:?}"),
        }
    }
    let row = |root: &str, entries: u32| {
        (
            "a".to_string(),
            "k".to_string(),
            root.to_string(),
            "claude-sonnet-5".to_string(),
            entries,
            "ttl",
            2_001,
        )
    };
    assert_eq!(expired, [row("s1", 3), row("s2", 1)]);
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn an_expiry_reports_the_depth_the_entry_was_written_at_when_its_chain_is_partly_purged() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.affinity_ttl_seconds = 1;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let chain = |hashes: &[&str]| -> Arc<[String]> {
        Arc::from(hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>())
    };
    handle
        .report(Report::Affinity(AffinityReport {
            root: "b1".into(),
            message_boundaries: chain(&["b1", "b2", "b3"]),
            ..affinity_report(&handle, "a", 1_000)
        }))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CacheMapWritten { .. }
    ));
    // The middle boundary alone is refreshed in place, so its chain's first and last entries age out without it.
    now.store(1_600, Ordering::SeqCst);
    handle
        .report(Report::Affinity(AffinityReport {
            root: "b1".into(),
            message_boundaries: chain(&["b2"]),
            ..affinity_report(&handle, "a", 1_600)
        }))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CacheMapWritten { .. }
    ));
    now.store(2_100, Ordering::SeqCst);
    match next_event(&mut events).await {
        RouterEvent::WarmPrefixExpired {
            root,
            entries,
            deepest,
            cause,
            ..
        } => assert_eq!(
            (root.as_str(), entries, deepest.depth, deepest.tier, cause),
            ("b1", 2, 3, "message", "ttl")
        ),
        other => panic!("the root's expiry, not {other:?}"),
    }
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_moved_account_state_emits_the_relay_s_cause() {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a"]);
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let before = store.account("a").unwrap().unwrap().revision;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let verdict = || Report::AccountState {
        account_id: "a".into(),
        state: AccountState::ReauthRequired,
        incarnation: 1,
        lifecycle_generation: 1,
        credential_generation: 1,
        cause: StateCause::AuthFailed,
        detail: Some("invalid x-api-key".into()),
        dispatched_ordinal: None,
    };
    handle.report(verdict()).await;
    match next_event(&mut events).await {
        RouterEvent::AccountStateChanged {
            account_id,
            to,
            incarnation,
            revision,
            cause,
            detail,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                to,
                incarnation,
                revision,
                cause,
                detail.as_deref()
            ),
            (
                "a",
                AccountState::ReauthRequired,
                1,
                before + 1,
                StateCause::AuthFailed,
                Some("invalid x-api-key")
            )
        ),
        other => panic!("the moved row's event, not {other:?}"),
    }
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(
        (row.state, row.state_cause),
        (AccountState::ReauthRequired, StateCause::AuthFailed),
        "the fenced write stored the word the event names"
    );
    // The same verdict again lands under its fences and leaves the state where it was.
    handle.report(verdict()).await;
    wait_for(WRITTEN, || {
        (store.account("a").unwrap().unwrap().revision == before + 2).then_some(())
    })
    .await;
    no_further_event(&mut events).await;
}

/// Prices a million tokens of input or cache write at a tenth of either window on `TIER`, whatever the model or the
/// instant, and nothing on any other tier: a 200 000 token body books 0.02 and a million input tokens charge 0.1. The
/// entrypoint `desk`, the tests' own regime, prices half of that on the 5-hour window, and no entrypoint or any other
/// one the base.
fn a_tenth_per_million_written() -> SharePricer {
    SharePricer::new(|window, entrypoint, tier, _, usage, _| {
        let regime = if window == FIVE_HOUR && entrypoint == Some("desk") {
            0.5
        } else {
            1.0
        };
        (tier == TIER).then(|| (usage.input + usage.write_5m) as f64 / 1_000_000.0 * 0.1 * regime)
    })
}

/// A million input tokens served on `a` for `request_id`'s wire attempt `attempt`.
fn usage_of(request_id: &str, attempt: u32, at: i64) -> Report {
    Report::Usage {
        account_id: "a".into(),
        at,
        canonical_model: "claude-sonnet-5".into(),
        incarnation: 1,
        usage: PinUsage {
            input: 1_000_000,
            ..PinUsage::default()
        },
        fast: false,
        judged: None,
        request_id: request_id.into(),
        attempt,
        entrypoint: None,
    }
}

/// A million input tokens served on `a` for `request_id`'s first wire attempt, from a request whose user agent named
/// `entrypoint`.
fn usage_under(request_id: &str, at: i64, entrypoint: Option<&str>) -> Report {
    Report::Usage {
        account_id: "a".into(),
        at,
        canonical_model: "claude-sonnet-5".into(),
        incarnation: 1,
        usage: PinUsage {
            input: 1_000_000,
            ..PinUsage::default()
        },
        fast: false,
        judged: None,
        request_id: request_id.into(),
        attempt: 1,
        entrypoint: entrypoint.map(String::from),
    }
}

fn ended(request_id: &str, attempt: u32, at: i64, cause: &'static str) -> Report {
    Report::Ended {
        request_id: request_id.into(),
        attempt,
        at,
        cause,
    }
}

/// `request` for `session` under `request_id`, carrying a 200 000 token body.
fn placement(handle: &RouterHandle, request_id: &str, session: &str, now: i64) -> SelectRequest {
    SelectRequest {
        request_id: request_id.into(),
        body_tokens: 200_000,
        ..request(handle, Some(session), now)
    }
}

/// A router over the tiered OAuth account `a`, priced by `a_tenth_per_million_written` with the purge every 50 ms,
/// once `a` reads 0.5 on its 5-hour window, and its events from before that reading.
async fn priced_router(
    now: &Arc<AtomicI64>,
) -> (
    RouterHandle,
    tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) {
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let handle = spawn(
        store,
        clock(now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(10 * HOUR)),
        ))
        .await;
    (handle, events)
}

/// The next `ReservationExpired` on `events`, every other event passed over.
async fn next_expiry(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> phoebis_routing::RouterEvent {
    loop {
        let event = next_event(events).await;
        if matches!(
            event,
            phoebis_routing::RouterEvent::ReservationExpired { .. }
        ) {
            return event;
        }
    }
}

/// Whether an expiry is among the events already on the broadcast.
fn an_expiry_among(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> bool {
    std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        matches!(
            event,
            phoebis_routing::RouterEvent::ReservationExpired { .. }
        )
    })
}

#[tokio::test]
async fn a_selection_books_a_reservation_the_usage_report_releases_before_its_charge() {
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, _events) = priced_router(&now).await;
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.52),
        "the booking's rewrite joins the reading"
    );
    handle.report(usage_of("r-1", 1, 2_500)).await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "the usage's charge alone once the report lands"
    );
}

#[tokio::test]
async fn a_reservation_on_a_destination_whose_set_lacks_the_five_hour_window_books_no_rewrite_or_burn_on_it(
) {
    use rand::SeedableRng;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    // A warm pin burning on the account, so a fresh session's booking carries a burn as well as its rewrite.
    let warm = PinRow {
        last_refresh_at: AT - 1_000,
        ..stored_pin(&store, "s1", "a", 1, 4.0)
    };
    let mut state = RoutingState::from_load(RoutingLoad {
        pins: vec![warm],
        ..RoutingLoad::default()
    });
    state.set_share(priced_config().share);
    state.record_sample(
        "a",
        as_poll(sample_with(AT - 500, SEVEN_DAY, 0.2, Some(AT + 72 * HOUR))),
        AT - 500,
    );
    let decided = decide(
        &mut state,
        &handle.snapshot(),
        &placement(&handle, "r-2", "s2", AT),
        &mut rand::rngs::StdRng::seed_from_u64(7),
    );
    let reservation = decided
        .record
        .outcome
        .reservation
        .as_ref()
        .expect("the placement booked on a");
    let windows = |shares: &std::collections::BTreeMap<String, f64>| {
        shares.keys().cloned().collect::<Vec<_>>()
    };
    assert_eq!(windows(&reservation.rewrite), [SEVEN_DAY]);
    assert_eq!(
        windows(&reservation.burn),
        [SEVEN_DAY],
        "the booking names the windows of the destination's set alone"
    );
    let facts = decided
        .record
        .candidates
        .iter()
        .find(|candidate| candidate.account_id == "a")
        .expect("a is a candidate");
    assert_eq!(
        windows(&facts.burn),
        [SEVEN_DAY],
        "the candidate's burn lists no 5-hour share on an account without the window"
    );
}

#[tokio::test]
async fn a_request_that_ends_without_usage_releases_its_reservation_and_the_task_emits_reservation_expired(
) {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, mut events) = priced_router(&now).await;
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(close(read(&handle, "a", FIVE_HOUR).await, 0.52));
    handle.report(ended("r-1", 1, 3_000, "canceled")).await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.5),
        "the end releases the booking and charges nothing"
    );
    match next_expiry(&mut events).await {
        RouterEvent::ReservationExpired {
            account_id,
            request_id,
            attempt,
            cause,
            shares,
            age_ms,
            at,
        } => {
            assert_eq!(
                (
                    account_id.as_str(),
                    request_id.as_str(),
                    attempt,
                    cause,
                    age_ms,
                    at
                ),
                ("a", "r-1", 1, "canceled", 1_000, 3_000)
            );
            assert!(close(shares.get(FIVE_HOUR).copied(), 0.02), "{shares:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_purge_expires_a_reservation_past_the_reclaim_bound_with_the_cause_timeout() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, mut events) = priced_router(&now).await;
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(close(read(&handle, "a", FIVE_HOUR).await, 0.52));
    now.store(2_000 + IN_FLIGHT_RECLAIM_MS + 1, Ordering::SeqCst);
    match next_expiry(&mut events).await {
        RouterEvent::ReservationExpired {
            request_id,
            attempt,
            cause,
            age_ms,
            ..
        } => assert_eq!(
            (request_id.as_str(), attempt, cause, age_ms),
            ("r-1", 1, "timeout", IN_FLIGHT_RECLAIM_MS + 1)
        ),
        other => panic!("{other:?}"),
    }
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.5),
        "the expired rewrite left the prediction"
    );
}

#[tokio::test]
async fn a_renewals_usage_charges_its_account_with_no_booking() {
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, _events) = priced_router(&now).await;
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.52),
        "the placement books"
    );
    handle.report(usage_of("r-1", 1, 2_500)).await;
    assert!(close(read(&handle, "a", FIVE_HOUR).await, 0.6));
    let renewed = handle
        .select(placement(&handle, "r-2", "s1", 3_000))
        .await
        .unwrap()
        .result;
    assert!(
        matches!(&renewed, SelectResult::Selected(s) if s.trace.renewed),
        "{renewed:?}"
    );
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "a renewal books nothing"
    );
    handle.report(usage_of("r-2", 1, 3_500)).await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.7),
        "its usage charges the account all the same"
    );
}

#[tokio::test]
async fn an_interactive_second_turn_on_an_existing_pin_charges_under_its_regime_with_no_booking() {
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, _events) = priced_router(&now).await;
    let desk_turn = |request_id: &str, at: i64| SelectRequest {
        entrypoint: Some("desk".into()),
        ..placement(&handle, request_id, "s1", at)
    };
    handle.select(desk_turn("r-1", 2_000)).await.unwrap();
    handle.report(usage_under("r-1", 2_500, Some("desk"))).await;
    let placed = read(&handle, "a", FIVE_HOUR).await.unwrap();
    let renewed = handle.select(desk_turn("r-2", 3_000)).await.unwrap().result;
    assert!(
        matches!(&renewed, SelectResult::Selected(s) if s.trace.renewed),
        "{renewed:?}"
    );
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, placed),
        "a renewal books nothing"
    );
    handle.report(usage_under("r-2", 3_500, Some("desk"))).await;
    let interactive = read(&handle, "a", FIVE_HOUR).await.unwrap();
    handle.report(usage_under("r-3", 4_000, None)).await;
    let base = read(&handle, "a", FIVE_HOUR).await.unwrap();
    let (interactive_rise, base_rise) = (interactive - placed, base - interactive);
    assert!(
        base_rise > 0.0 && (interactive_rise / base_rise - 0.5).abs() < 1e-9,
        "the turn's charge prices under the regime its report carries, half the base one's: {interactive_rise} \
         against {base_rise}"
    );
}

#[tokio::test]
async fn a_usage_after_its_bookings_expiry_charges_once() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, mut events) = priced_router(&now).await;
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    now.store(2_000 + IN_FLIGHT_RECLAIM_MS + 1, Ordering::SeqCst);
    assert!(matches!(
        next_expiry(&mut events).await,
        RouterEvent::ReservationExpired {
            cause: "timeout",
            ..
        }
    ));
    handle
        .report(usage_of("r-1", 1, 2_000 + IN_FLIGHT_RECLAIM_MS + 2))
        .await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "the reading plus the usage's share once, the expired rewrite gone"
    );
}

#[tokio::test]
async fn a_retry_in_place_keeps_the_requests_booking_until_the_usage_of_its_second_attempt() {
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, mut events) = priced_router(&now).await;
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.52),
        "the booking stands while attempt 2 is in flight, no report between the attempts"
    );
    handle.report(usage_of("r-1", 2, 4_000)).await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.6),
        "attempt 2's usage releases the request's booking and charges once"
    );
    assert!(
        !an_expiry_among(&mut events),
        "no expiry between the attempts"
    );
}

#[tokio::test]
async fn a_request_dropped_before_its_selection_answer_is_read_releases_the_booking_on_its_ended_report(
) {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(2_000));
    let (handle, mut events) = priced_router(&now).await;
    let park = handle.park_loop().await;
    let asked = tokio::spawn({
        let handle = handle.clone();
        let request = placement(&handle, "r-7", "s7", 2_000);
        async move { handle.select(request).await }
    });
    wait_for(WRITTEN, || (handle.queued() == 1).then_some(())).await;
    // The answer's receiver drops unread, and the end report queues behind the selection.
    asked.abort();
    assert!(matches!(asked.await, Err(error) if error.is_cancelled()));
    handle.report(ended("r-7", 1, 2_500, "canceled")).await;
    drop(park);
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.5),
        "the selection booked and the end behind it released"
    );
    assert!(matches!(
        next_expiry(&mut events).await,
        RouterEvent::ReservationExpired { cause: "canceled", ref request_id, .. } if request_id == "r-7"
    ));
    // An end for a request that holds no booking changes nothing and says nothing.
    handle.report(ended("r-none", 1, 2_600, "canceled")).await;
    assert!(close(read(&handle, "a", FIVE_HOUR).await, 0.5));
    assert!(!an_expiry_among(&mut events));
}

/// The next `WindowSpent` on `events`, every other event passed over.
async fn next_spent(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> phoebis_routing::RouterEvent {
    loop {
        let event = next_event(events).await;
        if matches!(event, phoebis_routing::RouterEvent::WindowSpent { .. }) {
            return event;
        }
    }
}

#[tokio::test]
async fn a_charge_or_a_reservation_that_predicts_a_spent_window_emits_window_spent_with_the_source_predicted_once_per_reset(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.99, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    // The 200 000 token body books 0.02, which takes the 0.99 reading past the wall before any usage lands.
    handle
        .select(placement(&handle, "r-1", "s1", 2_000))
        .await
        .unwrap();
    match next_spent(&mut events).await {
        RouterEvent::WindowSpent {
            account_id,
            window,
            utilization,
            reset: spent_reset,
            source,
            source_request_id,
            at,
        } => {
            assert_eq!(
                (
                    account_id.as_str(),
                    window.as_str(),
                    spent_reset,
                    source,
                    source_request_id.as_deref(),
                    at
                ),
                (
                    "a",
                    FIVE_HOUR,
                    Some(reset),
                    WindowSource::Predicted,
                    Some("r-1"),
                    2_000
                )
            );
            assert!(close(Some(utilization), 1.01), "{utilization}");
        }
        other => panic!("{other:?}"),
    }
    // The usage replaces the booking and its charge crosses the wall again inside the same cycle, and the head that
    // reads the window spent names the same reset a half second off, as a head's whole seconds do.
    handle.report(usage_of("r-1", 1, 2_500)).await;
    handle
        .report(head_reading(3_000, 1.0, Some(reset + 500), "r-2"))
        .await;
    handle.samples().await.unwrap();
    let quiet = drained(&mut events).await;
    assert!(
        !quiet
            .iter()
            .any(|event| matches!(event, RouterEvent::WindowSpent { .. })),
        "one announcement per reset: {quiet:?}"
    );
    // Past the reset the window opens with the router's own charges, and a charge that spends it past the reset's
    // minute of slack is a new cycle: a reading that names no reset inside the slack is of the announced one.
    let opened = reset + 2 * 60_000;
    now.store(opened, Ordering::SeqCst);
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: opened,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 10_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-3".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    match next_spent(&mut events).await {
        RouterEvent::WindowSpent {
            utilization,
            reset: spent_reset,
            source,
            source_request_id,
            ..
        } => {
            assert_eq!(
                (spent_reset, source, source_request_id.as_deref()),
                (None, WindowSource::Predicted, Some("r-3")),
                "the open window names no reset until the next reading does"
            );
            assert!(close(Some(utilization), 1.0), "{utilization}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_pin_migrated_event_carries_the_destinations_fit_runway_and_binding() {
    use phoebis_routing::{FactValue, MigrateReason, RouterEvent};
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // a is spent with no reset to wait for, so the spent rule moves s1. On b at 0.5 the pin's 0.1 of the window an
    // hour reaches the ceiling in about four and a half hours, short of the five-hour horizon: the rewrite lands and
    // the runway falls short, so b reads `rewrite_only` with the 5-hour window binding.
    handle.report(windows("a", 1.0, 0.3, AT)).await;
    handle.report(windows("b", 0.5, 0.1, AT)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(quota_turn(&handle, Some("s1"), AT))
                .await
                .unwrap()
        ),
        ("b".into(), Some(MigrateReason::Spent))
    );
    let (_, record) = next_decision(&mut events).await;
    let row: Vec<&phoebis_routing::Fact> = record
        .walk
        .iter()
        .find(|step| step.step == "fit")
        .expect("the spent rule's fit table")
        .facts
        .iter()
        .filter(|fact| fact.account_id.as_deref() == Some("b"))
        .collect();
    let of = |key: &str| {
        row.iter()
            .find(|fact| fact.key == key)
            .map(|fact| fact.value.clone())
    };
    let migrated = loop {
        if let event @ RouterEvent::PinMigrated { .. } = next_event(&mut events).await {
            break event;
        }
    };
    let RouterEvent::PinMigrated {
        to_account,
        fit,
        runway_ms,
        binding,
        cause,
        ..
    } = migrated
    else {
        unreachable!()
    };
    assert_eq!(to_account, "b");
    assert_eq!(
        (cause, &record.outcome.cause),
        (None, &None),
        "a rule's move carries no cause"
    );
    assert_eq!(
        (fit, binding.as_deref()),
        (Some("rewrite_only"), Some("five_hour"))
    );
    assert!(
        runway_ms.is_some_and(|ms| ms > 0 && ms < 5 * HOUR),
        "{runway_ms:?}"
    );
    assert_eq!(
        (
            fit.map(|fit| FactValue::Text(fit.into())),
            runway_ms.map(FactValue::Int),
            binding.map(FactValue::Text)
        ),
        (of("verdict"), of("runway_ms"), of("binding")),
        "the event names the destination's row of the move's fit table"
    );
}

#[tokio::test]
async fn a_standing_runway_that_reaches_past_its_windows_reset_reads_unbounded() {
    let store = shedding_store(&["a"]);
    with_tier(&store, "a");
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // The pin's 0.1 of the window an hour fills the 0.2 left under the ceiling in two hours, and the window resets in
    // one, so the ceiling is never reached before the reset. a holds no weekly reading, so no other window bounds it.
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.75, Some(AT + HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    let standing = handle.pins_standing().await.unwrap();
    let s1 = standing
        .iter()
        .find(|p| p.pin.session_id == "s1")
        .unwrap_or_else(|| panic!("no pin of s1: {standing:?}"));
    assert_eq!(
        (s1.runway_ms, s1.runway_bounded, s1.binding.as_deref()),
        (None, Some(false), None),
        "a runway past its window's reset reads unbounded"
    );
}

#[tokio::test]
async fn a_standing_runway_that_reaches_its_accounts_end_ahead_of_its_windows_reset_reads_unbounded(
) {
    let store = shedding_store(&["a"]);
    with_tier(&store, "a");
    store
        .set_canceled(
            "a",
            Cancellation::On {
                ends_at: Some(AT + HOUR),
            },
        )
        .unwrap();
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // The pin's 0.1 of the window an hour fills the 0.2 left under the ceiling in two hours. The window resets in
    // three, but the account ends in one, so the ceiling is never reached before the end.
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.75, Some(AT + 3 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    let standing = handle.pins_standing().await.unwrap();
    let s1 = standing
        .iter()
        .find(|p| p.pin.session_id == "s1")
        .unwrap_or_else(|| panic!("no pin of s1: {standing:?}"));
    assert_eq!(
        (s1.runway_ms, s1.runway_bounded, s1.binding.as_deref()),
        (None, Some(false), None),
        "a runway past its account's end reads unbounded"
    );
}

#[tokio::test]
async fn the_window_inside_its_reset_binds_the_standing_runway_where_a_shorter_one_passes_its_reset(
) {
    let store = shedding_store(&["a"]);
    with_tier(&store, "a");
    // A weekly reserve at the 5-hour figure, so the week's line stands at 0.95.
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.weekly_ceiling = 0.95;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    // The 5-hour window's two hours to the ceiling pass its reset an hour out, while the week's 0.05 left under the
    // ceiling, at a sixth of the burn, lasts three hours under no reset: the week binds though its runway is longer.
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.75, Some(AT + HOUR)).with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.9,
                    reset: None,
                    observed_at: AT,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
        ))
        .await;
    handle.samples().await.unwrap();
    let standing = handle.pins_standing().await.unwrap();
    let s1 = standing
        .iter()
        .find(|p| p.pin.session_id == "s1")
        .unwrap_or_else(|| panic!("no pin of s1: {standing:?}"));
    assert_eq!(
        (s1.runway_ms, s1.runway_bounded, s1.binding.as_deref()),
        (Some(3 * HOUR), Some(true), Some(SEVEN_DAY)),
        "the week binds at its three hours, the 5-hour window's two passing its reset"
    );
}

#[tokio::test]
async fn a_predicted_wall_with_no_reset_holds_back_a_sample_whose_reset_lands_within_a_minute_past_its_length(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let reset = 2 * HOUR;
    let now = Arc::new(AtomicI64::new(reset + 1_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    let announced = reset + 2_000;
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: announced,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 10_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent {
            reset: None,
            source: WindowSource::Predicted,
            ..
        }
    ));
    // The request that opened the window reached the upstream after the announcement, so the reset lands half a
    // minute past five hours from it, inside the slack two named resets are compared within.
    handle
        .report(head_reading(
            reset + 3_000,
            1.0,
            Some(announced + 5 * HOUR + 30_000),
            "r-2",
        ))
        .await;
    handle.samples().await.unwrap();
    let quiet = drained(&mut events).await;
    assert!(
        !quiet
            .iter()
            .any(|event| matches!(event, RouterEvent::WindowSpent { .. })),
        "the open window's wall is announced once: {quiet:?}"
    );
}

#[tokio::test]
async fn a_predicted_wall_with_no_reset_holds_back_the_samples_of_its_own_window_and_not_of_a_later_one(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let reset = 2 * HOUR;
    let now = Arc::new(AtomicI64::new(reset + 1_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    // Past the stored reset the window opens with the router's own charges, so the wall the charge reaches names no
    // reset yet.
    let announced = reset + 2_000;
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: announced,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 10_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent {
            reset: None,
            source: WindowSource::Predicted,
            ..
        }
    ));
    // The head that confirms the wall names that window's reset, within five hours of the announcement.
    handle
        .report(head_reading(
            reset + 3_000,
            1.0,
            Some(announced + 4 * HOUR),
            "r-2",
        ))
        .await;
    handle.samples().await.unwrap();
    let quiet = drained(&mut events).await;
    assert!(
        !quiet
            .iter()
            .any(|event| matches!(event, RouterEvent::WindowSpent { .. })),
        "the open window's wall is announced once: {quiet:?}"
    );
    // A window whose reset falls past the open window's length is a later one, and its wall is announced. Its heads
    // are taken past the open window's reset, which passes with no clear, so the wall's announcement stands and the
    // cycle test alone tells the later window apart.
    handle
        .report(head_reading(
            announced + 4 * HOUR + 1_000,
            0.5,
            Some(announced + 7 * HOUR),
            "r-3",
        ))
        .await;
    // Read back before the next, which would otherwise replace it unread.
    handle.samples().await.unwrap();
    handle
        .report(head_reading(
            announced + 4 * HOUR + 2_000,
            1.0,
            Some(announced + 7 * HOUR),
            "r-4",
        ))
        .await;
    match next_spent(&mut events).await {
        RouterEvent::WindowSpent {
            reset: spent_reset,
            source,
            source_request_id,
            ..
        } => assert_eq!(
            (spent_reset, source, source_request_id.as_deref()),
            (Some(announced + 7 * HOUR), WindowSource::Head, Some("r-4"))
        ),
        other => panic!("{other:?}"),
    }
}

/// A head's reading of `window` on `a`, reported with the request whose head carried it, as `head_reading` reports
/// the 5-hour one.
fn head_of(
    window: &str,
    at: i64,
    utilization: f64,
    reset: Option<i64>,
    request_id: &str,
) -> Report {
    Report::Sample {
        account_id: "a".into(),
        sample: sample(at).with(
            window,
            WindowReading {
                utilization,
                reset,
                observed_at: at,
                source: WindowSource::Head,
                plan: None,
                tier: None,
            },
        ),
        incarnation: 1,
        source_request_id: Some(request_id.into()),
    }
}

/// `a`'s prediction of `window` as the handle answers it, once every report sent before the ask has landed. With no
/// charge or booking on the account, and its reset ahead of the clock, it reads as the stored reading.
async fn predicted_reading(handle: &RouterHandle, window: &str) -> WindowReading {
    handle
        .samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == "a")
        .and_then(|(_, sample)| sample.reading(window))
        .unwrap_or_else(|| panic!("a's {window} reading"))
}

/// A poll at the wall on `window`, a head at 0.99 `step` later and a second poll at the wall `step` after that, each
/// naming `reset`, with the clock at each reading's instant: one wall, and the head lands raised to it with its own
/// instant, reset and source, in the state and in the window's row.
async fn a_head_within_the_margin_holds_the_wall(window: &str, reset: Option<i64>, step: i64) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let polled = 2_000;
    let now = Arc::new(AtomicI64::new(polled));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            as_poll(sample_with(polled, window, 1.0, reset)),
        ))
        .await;
    match next_event(&mut events).await {
        RouterEvent::WindowSpent {
            account_id,
            window: spent,
            utilization,
            reset: spent_reset,
            source,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                spent.as_str(),
                utilization,
                spent_reset,
                source
            ),
            ("a", window, 1.0, reset, WindowSource::Poll)
        ),
        other => panic!("one window spent, not {other:?}"),
    }
    // The head reads the utilization's float, a point under the poll's whole percent at the same wall.
    let headed = polled + step;
    now.store(headed, Ordering::SeqCst);
    handle
        .report(head_of(window, headed, 0.99, reset, "r-1"))
        .await;
    let held = predicted_reading(&handle, window).await;
    assert_eq!(
        (held.utilization, held.observed_at, held.reset, held.source),
        (1.0, headed, reset, WindowSource::Head),
        "the head lands at the wall with its own instant, reset and source"
    );
    let row = wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .windows
            .into_iter()
            .find(|row| row.account_id == "a" && row.window == window && row.observed_at == headed)
    })
    .await;
    assert_eq!(row.utilization, 1.0, "the window's row reads the wall too");
    let polled_again = headed + step;
    now.store(polled_again, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(polled_again, window, 1.0, reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_head_within_the_margin_after_a_wall_neither_clears_it_nor_announces_it_again() {
    a_head_within_the_margin_holds_the_wall(FIVE_HOUR, Some(10 * HOUR), 1_000).await;
}

#[tokio::test]
async fn a_head_within_the_margin_after_a_wall_of_the_fable_week_neither_clears_it_nor_announces_it_again(
) {
    // Neither reading names the week's reset, so the week's length from the wall bounds its cycle: a head six hours
    // on, past a 5-hour window's length, still reads the wall's cycle.
    a_head_within_the_margin_holds_the_wall(SEVEN_DAY_FABLE, None, 6 * HOUR).await;
}

#[tokio::test]
async fn a_wall_of_the_next_reset_cycle_is_announced_again() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(10 * HOUR)),
        ))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowSpent { reset: Some(reset), .. } if reset == 10 * HOUR
    ));
    // Parked, so no purge tick announces the passed reset by the clock ahead of the poll that names the next one.
    let park = handle.park_loop().await;
    let next_cycle = 10 * HOUR + 2_000;
    now.store(next_cycle, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(next_cycle, FIVE_HOUR, 0.99, Some(20 * HOUR))),
        ))
        .await;
    drop(park);
    match next_event(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            observed,
            ..
        } => assert_eq!(
            (reset_at, next_reset, observed),
            (10 * HOUR, Some(20 * HOUR), "sample")
        ),
        other => panic!("the passed reset, by the sample, not {other:?}"),
    }
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        0.99,
        "a reading of the next cycle within the margin lands as read"
    );
    now.store(next_cycle + 1_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(
                next_cycle + 1_000,
                FIVE_HOUR,
                1.0,
                Some(20 * HOUR),
            )),
        ))
        .await;
    match next_event(&mut events).await {
        RouterEvent::WindowSpent { reset, source, .. } => {
            assert_eq!((reset, source), (Some(20 * HOUR), WindowSource::Poll))
        }
        other => panic!("the next cycle's wall, not {other:?}"),
    }
}

#[tokio::test]
async fn the_margin_is_two_points_and_a_thousandth_under_it_is_a_clear() {
    use phoebis_routing::{RouterEvent, SPENT_MARGIN};
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = Some(10 * HOUR);
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 1.0, reset)))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowSpent { .. }
    ));
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(head_reading(3_000, 1.0 - SPENT_MARGIN, reset, "r-1"))
        .await;
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        1.0,
        "a head at the margin's bound reads the wall"
    );
    now.store(4_000, Ordering::SeqCst);
    let cleared = 1.0 - SPENT_MARGIN - 0.001;
    handle
        .report(head_reading(4_000, cleared, reset, "r-2"))
        .await;
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        0.989,
        "a thousandth under the bound lands under the wall, the head's percent from 0.979 bounding the stored 1.0"
    );
    now.store(5_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(5_000, FIVE_HOUR, 1.0, reset)))
        .await;
    match next_spent(&mut events).await {
        RouterEvent::WindowSpent {
            reset: spent_reset,
            source,
            at,
            ..
        } => assert_eq!(
            (spent_reset, source, at),
            (reset, WindowSource::Head, 5_000),
            "the clear lets the next crossing announce a wall"
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_spent_state_reads_the_raised_reading_through_a_head_within_the_margin() {
    use phoebis_routing::{MigrateReason, RouterEvent};
    let store = shedding_store(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    assert_eq!(
        landed_on(
            handle
                .select(request(&handle, Some("s1"), 1_000))
                .await
                .unwrap()
        ),
        ("a".into(), None)
    );
    // The spent rule moves only onto an account whose shared windows it has read.
    handle.report(windows("b", 0.2, 0.1, 1_500)).await;
    now.store(2_000, Ordering::SeqCst);
    let reset = Some(10 * HOUR);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { account_id, .. } if account_id == "a"
    ));
    now.store(3_000, Ordering::SeqCst);
    handle.report(head_reading(3_000, 0.99, reset, "r-1")).await;
    handle.samples().await.unwrap();
    now.store(3_100, Ordering::SeqCst);
    assert_eq!(
        landed_on(
            handle
                .select(request(&handle, Some("s1"), 3_100))
                .await
                .unwrap()
        ),
        ("b".into(), Some(MigrateReason::Spent)),
        "the head within the margin leaves a spent"
    );
    let (moved, _) = next_decision(&mut events).await;
    assert_eq!((moved.kind, moved.rule), ("moved", Some("spent")));
    let move_events = drained(&mut events).await;
    assert!(
        move_events.iter().any(|event| matches!(
            event,
            RouterEvent::PinMigrated { from_account, to_account, reason, .. }
                if from_account == "a" && to_account == "b" && reason == "spent"
        )),
        "{move_events:?}"
    );
    now.store(4_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(4_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
    now.store(4_100, Ordering::SeqCst);
    assert_eq!(
        landed_on(
            handle
                .select(request(&handle, Some("s1"), 4_100))
                .await
                .unwrap()
        ),
        ("b".into(), None),
        "the second poll at the wall moves nothing more"
    );
}

#[tokio::test]
async fn a_head_under_the_margin_after_a_wall_lets_the_next_poll_announce_it_again() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = Some(10 * HOUR);
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 1.0, reset)))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    now.store(3_000, Ordering::SeqCst);
    handle.report(head_reading(3_000, 0.90, reset, "r-1")).await;
    handle.samples().await.unwrap();
    // The head's percent from 0.90 bounds the stored 1.0 at 0.91.
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.91,
            reset,
            WindowSource::Head,
            Some("r-1")
        )),
        "a head under the margin reads the window cleared: {event:?}"
    );
    now.store(4_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(4_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    // The next edge is the wall itself, so the head cleared the window once.
    match next_window_edge(&mut events).await {
        RouterEvent::WindowSpent {
            reset: spent_reset,
            source,
            at,
            ..
        } => assert_eq!(
            (spent_reset, source, at),
            (reset, WindowSource::Poll, 4_000),
            "a head under the margin is a real clear, so the next wall is announced"
        ),
        other => panic!("{other:?}"),
    }
    now.store(5_000, Ordering::SeqCst);
    handle.report(head_reading(5_000, 0.99, reset, "r-2")).await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_cooldown_set_at_the_wall_stands_through_a_head_within_the_margin() {
    use phoebis_routing::{RouterEvent, SPENT_MARGIN};
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = Some(10 * HOUR);
    handle
        .report(observed("a", sample_with(1_000, FIVE_HOUR, 1.0, reset)))
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::WindowSpent { .. }
    ));
    handle
        .report(cooled(
            "a",
            "account",
            1_000 + 5 * HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownSet { account_id, .. } if account_id == "a")
    );
    now.store(2_000, Ordering::SeqCst);
    handle.report(head_reading(2_000, 0.99, reset, "r-1")).await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
    assert!(
        matches!(
            handle
                .select(request(&handle, None, 2_100))
                .await
                .unwrap()
                .result,
            SelectResult::Exhausted(Exhaustion::Timed { .. })
        ),
        "the rejection stands through the head"
    );
    assert_eq!(next_decision(&mut events).await.0.kind, "exhausted");
    let held = predicted_reading(&handle, FIVE_HOUR).await;
    assert_eq!((held.utilization, held.source), (1.0, WindowSource::Head));
    now.store(3_000, Ordering::SeqCst);
    handle
        .report(head_reading(
            3_000,
            1.0 - SPENT_MARGIN - 0.001,
            reset,
            "r-2",
        ))
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { account_id, scope, cause, at } if account_id == "a" && scope == CooldownScope::Account && cause == "recovered" && at == 3_000)
    );
    // The head's percent from 0.979 bounds the stored 1.0 at 0.989.
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.989,
            reset,
            WindowSource::Head,
            Some("r-2")
        )),
        "the head that releases the row reads the window cleared: {event:?}"
    );
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .cooldowns
            .is_empty()
            .then_some(())
    })
    .await;
    assert!(
        matches!(handle.select(request(&handle, None, 3_100)).await.unwrap().result, SelectResult::Selected(ref s) if s.account.id == "a"),
        "a head under the margin releases the row"
    );
}

#[tokio::test]
async fn a_wall_announced_by_a_sample_is_not_announced_again_under_its_reset() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset = Some(10 * HOUR);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    // A head that names no reset, read before the stored reset passed, stands under that reset, so within the margin
    // of the wall it is raised to it and clears nothing.
    now.store(3_000, Ordering::SeqCst);
    handle.report(head_reading(3_000, 0.99, None, "r-1")).await;
    assert_eq!(predicted_reading(&handle, FIVE_HOUR).await.utilization, 1.0);
    // The poll names the wall's reset again, the cycle the first poll's announcement was recorded for.
    now.store(4_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(4_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

/// A reset-less head the hold raises to the wall stands under the stored reset and takes it, so the stored reading
/// after it still names that reset, and a poll taken past it naming a later one reads it passed.
#[tokio::test]
async fn a_head_raised_to_the_wall_keeps_the_stored_reset_so_a_later_reset_reads_it_passed() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset between the move past it and the sample that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    now.store(3_000, Ordering::SeqCst);
    handle.report(head_reading(3_000, 0.99, None, "r-1")).await;
    let raised = predicted_reading(&handle, FIVE_HOUR).await;
    assert_eq!(
        (raised.utilization, raised.reset),
        (1.0, Some(reset)),
        "the raised head stands under the stored reset"
    );
    now.store(reset + 1_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(reset + 1_000, FIVE_HOUR, 0.1, Some(15 * HOUR))),
        ))
        .await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            account_id,
            window,
            utilization_before,
            reset_at,
            next_reset,
            observed,
            at,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                window.as_str(),
                utilization_before,
                reset_at,
                next_reset,
                observed,
                at
            ),
            (
                "a",
                FIVE_HOUR,
                1.0,
                reset,
                Some(15 * HOUR),
                "sample",
                reset + 1_000
            )
        ),
        other => panic!("the stored reset reads passed, not {other:?}"),
    }
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

/// A reset-less clear read before the stored reset stands under it and is stored with it, so a poll taken past it
/// naming a later reset reads the stored one passed.
#[tokio::test]
async fn a_clear_that_names_no_reset_is_stored_under_the_stored_reset_so_a_later_reset_reads_it_passed(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset between the move past it and the sample that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    now.store(3_000, Ordering::SeqCst);
    handle.report(head_reading(3_000, 0.4, None, "r-1")).await;
    // Under the stored reset the head's percent from 0.4 bounds the stored 1.0 at 0.41.
    let event = next_window_edge(&mut events).await;
    let (account, window, before, after, reset_named, source, request) =
        cleared_of(&event).unwrap_or_else(|| panic!("a clear, not {event:?}"));
    assert_eq!(
        (account, window, before, reset_named, source, request),
        (
            "a",
            FIVE_HOUR,
            1.0,
            Some(reset),
            WindowSource::Head,
            Some("r-1")
        ),
        "{event:?}"
    );
    assert!(close(Some(after), 0.41), "{event:?}");
    let cleared = predicted_reading(&handle, FIVE_HOUR).await;
    assert_eq!(
        cleared.reset,
        Some(reset),
        "the clear is stored under the stored reset"
    );
    assert!(
        close(Some(cleared.utilization), 0.41),
        "{}",
        cleared.utilization
    );
    now.store(reset + 1_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(reset + 1_000, FIVE_HOUR, 0.1, Some(15 * HOUR))),
        ))
        .await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            utilization_before,
            ..
        } => {
            assert_eq!((reset_at, next_reset), (reset, Some(15 * HOUR)));
            assert!(
                close(Some(utilization_before), 0.41),
                "{utilization_before}"
            );
        }
        other => panic!("the stored reset reads passed, not {other:?}"),
    }
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

/// A reset-less head at the wall over a stored reading under it announces the wall under the stored reset and is stored
/// with it, so a poll taken past that reset naming a later one reads it passed rather than cleared.
#[tokio::test]
async fn a_wall_read_by_a_head_that_names_no_reset_is_stored_under_the_stored_reset_and_resets_past_it(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    // Held, so no clock walk announces the passed reset between the move past it and the sample that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 10 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    now.store(2_000, Ordering::SeqCst);
    handle.report(head_reading(2_000, 1.0, None, "r-1")).await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowSpent {
            at,
            reset: announced,
            ..
        } => assert_eq!(
            (at, announced),
            (2_000, Some(reset)),
            "the wall is announced under the stored reset"
        ),
        other => panic!("the head at the wall reads spent, not {other:?}"),
    }
    let spent = predicted_reading(&handle, FIVE_HOUR).await;
    assert_eq!(
        (spent.utilization, spent.reset),
        (1.0, Some(reset)),
        "the wall is stored under the stored reset"
    );
    now.store(reset + 1_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(reset + 1_000, FIVE_HOUR, 0.1, Some(15 * HOUR))),
        ))
        .await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            ..
        } => assert_eq!((reset_at, next_reset), (reset, Some(15 * HOUR))),
        other => panic!("the stored reset reads passed, not {other:?}"),
    }
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

#[tokio::test]
async fn a_late_reading_under_the_margin_that_the_fold_drops_leaves_the_wall_announced() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    let reset_at = 10 * HOUR;
    let reset = Some(reset_at);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    // A head read before the poll arrives after it: the fold keeps the newer poll, so the head is no clear.
    now.store(2_500, Ordering::SeqCst);
    handle.report(head_reading(1_500, 0.90, reset, "r-1")).await;
    let kept = predicted_reading(&handle, FIVE_HOUR).await;
    assert_eq!(
        (kept.utilization, kept.observed_at, kept.source),
        (1.0, 2_000, WindowSource::Poll)
    );
    // A newer reading of a later cycle, taken within the slack of the old reset, reads the reset passed and clears
    // nothing, so the old cycle's announcement stands, and a poll naming the old reset at the wall again announces
    // nothing. Had the late reading forgotten the announcement, that poll would announce the wall a second time. The
    // steps after it stay short of the old reset, which the head that names none stands under.
    now.store(reset_at - 50_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            sample_with(reset_at - 50_000, FIVE_HOUR, 0.10, Some(15 * HOUR)),
        ))
        .await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            account_id,
            window,
            utilization_before,
            reset_at,
            next_reset,
            observed,
            at,
            ..
        } => assert_eq!(
            (
                account_id.as_str(),
                window.as_str(),
                utilization_before,
                Some(reset_at),
                next_reset,
                observed,
                at
            ),
            (
                "a",
                FIVE_HOUR,
                1.0,
                reset,
                Some(15 * HOUR),
                "sample",
                reset_at - 50_000
            )
        ),
        other => panic!("{other:?}"),
    }
    now.store(reset_at - 40_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(reset_at - 40_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
    // A head that names no reset, read before the stored reset passed, stands under that reset, so within the margin
    // of the wall it is raised to it: no clear, and the poll at the wall after it announces nothing.
    now.store(reset_at - 30_000, Ordering::SeqCst);
    handle
        .report(head_reading(reset_at - 30_000, 0.99, None, "r-2"))
        .await;
    assert_eq!(predicted_reading(&handle, FIVE_HOUR).await.utilization, 1.0);
    now.store(reset_at - 20_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(reset_at - 20_000, FIVE_HOUR, 1.0, reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    no_further_event(&mut events).await;
}

/// A wall the prediction announced names the stored reading's reset, and a head that names none, read before that reset
/// passed, stands under it: the head's own reading at the wall is the same crossing and announces nothing.
#[tokio::test]
async fn a_predicted_wall_is_not_announced_again_by_a_head_that_names_no_reset_before_its_reset() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let reset = 2 * HOUR;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    now.store(1_500, Ordering::SeqCst);
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 1_500,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 10_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent {
            reset: Some(at),
            source: WindowSource::Predicted,
            ..
        } if at == reset
    ));
    now.store(2_000, Ordering::SeqCst);
    handle.report(head_reading(2_000, 1.0, None, "r-2")).await;
    handle.samples().await.unwrap();
    let quiet = drained(&mut events).await;
    assert!(
        !quiet
            .iter()
            .any(|event| matches!(event, RouterEvent::WindowSpent { .. })),
        "the predicted wall is announced once: {quiet:?}"
    );
}

/// A head that names no reset, taken past the announced reset and its minute of slack, is of another cycle than the
/// wall's: within the margin it lands as read and reads the reset passed, with no clear.
#[tokio::test]
async fn a_head_naming_no_reset_past_the_announced_reset_and_its_slack_lands_as_read() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset ahead of the reading that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 2 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    let past = reset + 2 * 60_000;
    now.store(past, Ordering::SeqCst);
    handle.report(head_reading(past, 0.99, None, "r-1")).await;
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        0.99,
        "the head lands as read"
    );
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            observed,
            at,
            ..
        } => assert_eq!(
            (reset_at, next_reset, observed, at),
            (reset, None, "sample", past)
        ),
        other => panic!("the announced reset reads passed, not {other:?}"),
    }
    no_further_event(&mut events).await;
}

/// A wall announced with no reset bounds its cycle by the window's length and a minute of slack from the
/// announcement, which a head held at the wall does not move: hourly heads within the margin are held for five hours,
/// and the first past them lands as read and clears the wall.
#[tokio::test]
async fn a_wall_with_no_reset_holds_heads_within_the_margin_for_the_windows_length_from_its_announcement(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let announced = 2_000;
    let now = Arc::new(AtomicI64::new(announced));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle
        .report(observed("a", sample_with(announced, FIVE_HOUR, 1.0, None)))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { reset: None, at, .. } if at == announced
    ));
    for hour in 1..=5 {
        let at = announced + hour * HOUR;
        now.store(at, Ordering::SeqCst);
        handle
            .report(head_reading(at, 0.99, None, &format!("r-{hour}")))
            .await;
        assert_eq!(
            predicted_reading(&handle, FIVE_HOUR).await.utilization,
            1.0,
            "a head {hour} hours after the announcement is held at the wall"
        );
    }
    no_further_event(&mut events).await;
    let past = announced + 6 * HOUR;
    now.store(past, Ordering::SeqCst);
    handle.report(head_reading(past, 0.99, None, "r-6")).await;
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        0.99,
        "a head past the window's length from the announcement lands as read"
    );
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.99,
            None,
            WindowSource::Head,
            Some("r-6")
        )),
        "{event:?}"
    );
    no_further_event(&mut events).await;
}

/// A wall a poll announced with its reset that continues into the window open past that reset: the head at the wall
/// that reads the reset passed announces nothing and anchors the wall's cycle at its own instant, so hourly heads
/// within the margin are held for the window's length from the reset's passing, the first past it clears the wall
/// once, and the next head at the wall announces it anew.
#[tokio::test]
async fn a_wall_that_outlives_its_reset_is_held_for_the_windows_length_from_the_resets_passing_and_announced_once(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset ahead of the reading that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 2 * HOUR;
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 1.0, Some(reset))),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    let passed = reset + 1_000;
    now.store(passed, Ordering::SeqCst);
    handle.report(head_reading(passed, 1.0, None, "r-0")).await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            observed,
            at,
            ..
        } => assert_eq!(
            (reset_at, next_reset, observed, at),
            (reset, None, "sample", passed)
        ),
        other => panic!("the announced reset reads passed, not {other:?}"),
    }
    for hour in 1..=5 {
        let at = passed + hour * HOUR;
        now.store(at, Ordering::SeqCst);
        handle
            .report(head_reading(at, 0.99, None, &format!("r-{hour}")))
            .await;
        assert_eq!(
            predicted_reading(&handle, FIVE_HOUR).await.utilization,
            1.0,
            "a head {hour} hours after the reset's passing is held at the wall"
        );
    }
    // Neither the head at the wall past the reset nor a held head announced or cleared the wall.
    no_further_event(&mut events).await;
    let past = passed + 6 * HOUR;
    now.store(past, Ordering::SeqCst);
    handle.report(head_reading(past, 0.99, None, "r-6")).await;
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        0.99,
        "a head past the window's length from the reset's passing lands as read"
    );
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.99,
            None,
            WindowSource::Head,
            Some("r-6")
        )),
        "{event:?}"
    );
    no_further_event(&mut events).await;
    let again = past + 1_000;
    now.store(again, Ordering::SeqCst);
    handle.report(head_reading(again, 1.0, None, "r-7")).await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowSpent {
            reset, source, at, ..
        } => assert_eq!((reset, source, at), (None, WindowSource::Head, again)),
        other => panic!("the next wall is announced anew, not {other:?}"),
    }
}

/// A wall with no reset that a restart restores from its row keeps the anchor the row's instant gives it: hourly heads
/// within the margin after the restart are held for the window's length from that instant, and the first past it lands
/// as read and clears the wall once.
#[tokio::test]
async fn a_restored_wall_keeps_its_anchor_across_a_restart_so_heads_within_the_margin_are_held_for_the_windows_length(
) {
    let store = seeded_oauth(&["a"]);
    let t0 = 2_000;
    let now = Arc::new(AtomicI64::new(t0));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(observed("a", sample_with(t0, FIVE_HOUR, 1.0, None)))
        .await;
    let rows = wait_for(WRITTEN, || {
        let windows = store.load_routing().unwrap().windows;
        (!windows.is_empty()).then_some(windows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .map(|r| (r.window.as_str(), r.utilization, r.reset, r.observed_at))
            .collect::<Vec<_>>(),
        vec![(FIVE_HOUR, 1.0, None, t0)],
        "the wall's row is on disk"
    );
    handle.shutdown().await.unwrap();
    let again = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut events = again.subscribe_events();
    for hour in 1..=5 {
        let at = t0 + hour * HOUR;
        now.store(at, Ordering::SeqCst);
        again
            .report(head_reading(at, 0.99, None, &format!("r-{hour}")))
            .await;
        assert_eq!(
            predicted_reading(&again, FIVE_HOUR).await.utilization,
            1.0,
            "a head {hour} hours after the restored wall's instant is held at the wall"
        );
    }
    no_further_event(&mut events).await;
    let past = t0 + 6 * HOUR;
    now.store(past, Ordering::SeqCst);
    again.report(head_reading(past, 0.99, None, "r-6")).await;
    assert_eq!(
        predicted_reading(&again, FIVE_HOUR).await.utilization,
        0.99,
        "a head past the window's length from the restored wall's instant lands as read"
    );
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.99,
            None,
            WindowSource::Head,
            Some("r-6")
        )),
        "{event:?}"
    );
    no_further_event(&mut events).await;
}

/// A head within the margin of a wall a poll announced with its reset, taken inside the slack past that reset and
/// naming none: it is held at the wall, reads the reset passed, and re-bases the wall's announcement on its own reset
/// and instant, so later heads within the margin are held for the window's length from that instant and the first past
/// it clears the wall once.
#[tokio::test]
async fn a_head_within_the_margin_inside_the_slack_past_the_announced_reset_re_bases_the_wall_on_its_own_instant(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset ahead of the reading that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 2 * HOUR;
    handle
        .report(observed(
            "a",
            as_poll(sample_with(2_000, FIVE_HOUR, 1.0, Some(reset))),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    let headed = reset + 1_000;
    now.store(headed, Ordering::SeqCst);
    handle.report(head_reading(headed, 0.99, None, "r-0")).await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset,
            observed,
            at,
            ..
        } => assert_eq!(
            (reset_at, next_reset, observed, at),
            (reset, None, "sample", headed)
        ),
        other => panic!("the announced reset reads passed, not {other:?}"),
    }
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        1.0,
        "the head within the margin inside the slack is held at the wall"
    );
    for hour in 1..=5 {
        let at = headed + hour * HOUR;
        now.store(at, Ordering::SeqCst);
        handle
            .report(head_reading(at, 0.99, None, &format!("r-{hour}")))
            .await;
        assert_eq!(
            predicted_reading(&handle, FIVE_HOUR).await.utilization,
            1.0,
            "a head {hour} hours after the re-basing head is held at the wall"
        );
    }
    no_further_event(&mut events).await;
    let past = headed + 6 * HOUR;
    now.store(past, Ordering::SeqCst);
    handle.report(head_reading(past, 0.99, None, "r-6")).await;
    let event = next_window_edge(&mut events).await;
    assert_eq!(
        cleared_of(&event),
        Some((
            "a",
            FIVE_HOUR,
            1.0,
            0.99,
            None,
            WindowSource::Head,
            Some("r-6")
        )),
        "a head past the window's length from the re-basing head lands as read and clears the wall: {event:?}"
    );
    no_further_event(&mut events).await;
}

/// A wall the prediction announced ends at its reset: a poll past that reset that names the next one at the wall is the
/// next cycle's own crossing and announces it.
#[tokio::test]
async fn a_poll_at_the_next_cycles_wall_past_a_predicted_walls_reset_announces_that_wall() {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let reset = 2 * HOUR;
    let now = Arc::new(AtomicI64::new(1_000));
    // Held, so no clock walk announces the passed reset ahead of the reading that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share: a_tenth_per_million_written(),
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle
        .report(observed(
            "a",
            sample_with(1_000, FIVE_HOUR, 0.5, Some(reset)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    now.store(1_500, Ordering::SeqCst);
    handle
        .report(Report::Usage {
            account_id: "a".into(),
            at: 1_500,
            canonical_model: "claude-sonnet-5".into(),
            incarnation: 1,
            usage: PinUsage {
                input: 10_000_000,
                ..PinUsage::default()
            },
            fast: false,
            judged: None,
            request_id: "r-1".into(),
            attempt: 1,
            entrypoint: None,
        })
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent {
            reset: Some(at),
            source: WindowSource::Predicted,
            ..
        } if at == reset
    ));
    let past = reset + 1_000;
    let next_reset = reset + 5 * HOUR;
    now.store(past, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(past, FIVE_HOUR, 1.0, Some(next_reset))),
        ))
        .await;
    match next_spent(&mut events).await {
        RouterEvent::WindowSpent {
            reset: spent,
            source,
            at,
            ..
        } => assert_eq!(
            (spent, source, at),
            (Some(next_reset), WindowSource::Poll, past)
        ),
        other => panic!("the next cycle's wall, not {other:?}"),
    }
}

/// A poll at the wall past the announced reset that names the next one is a new wall, over a stored reading at the wall
/// too: it announces that cycle's wall, and a head within the margin naming the same reset is held at it.
#[tokio::test]
async fn a_poll_at_the_wall_naming_the_next_reset_past_an_announced_walls_reset_announces_the_new_wall(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    // Held, so no clock walk announces the passed reset ahead of the reading that reads it passed.
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            hold_first_purge: true,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    let reset = 2 * HOUR;
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(reset)),
        ))
        .await;
    assert!(matches!(
        next_spent(&mut events).await,
        RouterEvent::WindowSpent { at: 2_000, .. }
    ));
    let passed = reset + 1_000;
    let next_reset = reset + 5 * HOUR;
    now.store(passed, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            as_poll(sample_with(passed, FIVE_HOUR, 1.0, Some(next_reset))),
        ))
        .await;
    match next_window_edge(&mut events).await {
        RouterEvent::WindowReset {
            reset_at,
            next_reset: next,
            ..
        } => assert_eq!((reset_at, next), (reset, Some(next_reset))),
        other => panic!("the announced reset reads passed, not {other:?}"),
    }
    match next_window_edge(&mut events).await {
        RouterEvent::WindowSpent {
            reset: spent,
            source,
            at,
            ..
        } => assert_eq!(
            (spent, source, at),
            (Some(next_reset), WindowSource::Poll, passed)
        ),
        other => panic!("the next cycle's wall, not {other:?}"),
    }
    let headed = passed + HOUR;
    now.store(headed, Ordering::SeqCst);
    handle
        .report(head_reading(headed, 0.99, Some(next_reset), "r-1"))
        .await;
    assert_eq!(
        predicted_reading(&handle, FIVE_HOUR).await.utilization,
        1.0,
        "a head within the margin of the new wall is held at it"
    );
    no_further_event(&mut events).await;
}

/// A main turn of `session` selected against `state` over the task's snapshot: the task's own state is out of the
/// handle's reach, so a case on the slow-down's memory selects and purges against a state built as the task builds it.
fn main_turn_on(
    state: &mut RoutingState,
    handle: &RouterHandle,
    session: &str,
    at: i64,
) -> phoebis_routing::Decided {
    use rand::SeedableRng;
    let req = SelectRequest {
        request_class: Some("main".into()),
        ..request(handle, Some(session), at)
    };
    decide(
        state,
        &handle.snapshot(),
        &req,
        &mut rand::rngs::StdRng::seed_from_u64(7),
    )
}

#[tokio::test]
async fn a_quiet_accounts_slow_down_entry_and_a_gone_pins_pace_leave_at_the_purge() {
    let store = shedding_store(&["a", "b"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.slow_down.slow_down = true;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let snapshot = handle.snapshot();
    let s1 = stored_pin(&store, "s1", "a", 1, 0.0);
    let s2 = stored_pin(&store, "s2", "a", 2, 0.0);
    let mut state = RoutingState::from_load(RoutingLoad {
        pins: vec![s1.clone(), s2.clone()],
        ..RoutingLoad::default()
    });
    state.record_sample(
        "a",
        sample_with(AT - 500, FIVE_HOUR, 0.5, Some(AT + HOUR)),
        AT - 500,
    );
    main_turn_on(&mut state, &handle, "s1", AT);
    main_turn_on(&mut state, &handle, "s2", AT);
    let entry = state
        .slow_down_state("p", "a")
        .cloned()
        .expect("the read wrote a's entry");
    assert!(state.pin_pace(&s1.key()).is_some() && state.pin_pace(&s2.key()).is_some());
    assert!(
        state.purge(&snapshot, AT + 1_000).slow_downs.is_empty(),
        "the pool's pins still stand on a"
    );
    assert!(state.pin_pace(&s1.key()).is_some());

    // The failover's invalidation takes the row alone, since the landing writes the fresh one under the same key, so
    // the pace waits for the purge.
    for pin in [&s1, &s2] {
        state
            .invalidate_pin(
                &PinRef {
                    key_id: "k".into(),
                    session_id: pin.session_id.clone(),
                    agent_id: "main".into(),
                    model: "claude-sonnet-5".into(),
                    epoch: pin.epoch,
                    lane: pin.lane.clone(),
                    entrypoint: None,
                    effort: None,
                },
                "a",
            )
            .expect("the row");
    }
    assert!(state.pin_pace(&s1.key()).is_some() && state.pin_pace(&s2.key()).is_some());
    assert_eq!(state.slow_down_state("p", "a"), Some(&entry));

    let purged = state.purge(&snapshot, AT + 2_000);
    assert_eq!(
        purged.slow_downs,
        vec![(("p".to_string(), "a".to_string()), entry)],
        "a holds no pin of the pool, so its entry leaves whole"
    );
    assert!(state.slow_down_state("p", "a").is_none());
    assert!(
        state.pin_pace(&s1.key()).is_none() && state.pin_pace(&s2.key()).is_none(),
        "no pin holds either key"
    );
}

/// Pool `p` slowing its pins over the tiered account `a` alone, its session `s1` pinned there burning ten input tokens
/// a second, a tenth of the 5-hour window an hour through `priced_config`, and the window at 0.9 with its reset 40
/// minutes after `AT`: the room under the ceiling lasts half an hour at that burn, so the sessions would pass it ten
/// minutes before the reset, a factor of about 0.75 and a stretch under the pool's half-hour threshold. The pool's burn
/// average is `SLOWED_AVERAGE_MS`. The purge ticks every 50 ms.
async fn slowed_router(now: &Arc<AtomicI64>) -> RouterHandle {
    slowed_router_under(
        now,
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..priced_config()
        },
    )
    .await
}

/// `slowed_router`'s pool, pin and reading under `config`.
async fn slowed_router_under(now: &Arc<AtomicI64>, config: RouterConfig) -> RouterHandle {
    let store = shedding_store(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.slow_down.slow_down = true;
    pool.slow_down.slow_down_stretch_seconds = 1800;
    pool.shedding.burn_average_seconds = (SLOWED_AVERAGE_MS / 1_000) as u64;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    with_tier(&store, "a");
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let handle = spawn(store, clock(now), config).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.9, Some(SLOWED_RESET)),
        ))
        .await;
    handle.samples().await.unwrap();
    handle
}

/// The 5-hour reset `slowed_router` seeds on `a`.
const SLOWED_RESET: i64 = AT + 40 * 60_000;

/// The burn average that `slowed_router` and the projections case's router set on their pools. Their stamp turns are
/// dated one of it before the first main turn, so the held turn after it reads the ramp at 1.
const SLOWED_AVERAGE_MS: i64 = 30 * 60_000;

/// Session `s1`'s main turn at `at` under its own request id.
fn slowed_turn(handle: &RouterHandle, request_id: &str, at: i64) -> SelectRequest {
    SelectRequest {
        request_id: request_id.into(),
        request_class: Some("main".into()),
        ..request(handle, Some("s1"), at)
    }
}

/// The value of `key` on the decision's shed line, where the slow-down read states its figures.
fn shed_fact(record: &DecisionRecord, key: &str) -> phoebis_routing::FactValue {
    record
        .walk
        .iter()
        .find(|step| step.step == "move:shed")
        .and_then(|step| step.facts.iter().find(|fact| fact.key == key))
        .map(|fact| fact.value.clone())
        .unwrap_or_else(|| panic!("no {key} on the shed line"))
}

/// The decisions among `events`, by request id, with their records.
fn decisions_of(events: &[phoebis_routing::RouterEvent]) -> Vec<(String, &DecisionRecord)> {
    events
        .iter()
        .filter_map(|event| match event {
            phoebis_routing::RouterEvent::Decision { record, .. } => {
                Some((record.request.request_id.clone(), record.as_ref()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_factor_under_one_emits_slow_down_started_once_and_a_return_to_one_ends_it_after_the_wait(
) {
    use phoebis_routing::{FactValue, RouterEvent, SLOW_DOWN_WAIT_MS};
    let now = Arc::new(AtomicI64::new(AT));
    let handle = slowed_router(&now).await;
    let mut events = handle.subscribe_events();
    // An auxiliary turn one burn average before the first main turn reads the factor under 1, opens the span and
    // stamps the ramp, so the hold below applies the read's whole factor. The first main turn starts the pin's pace,
    // and the second, 30 s on, gives the pin its cycle and waits its part of the slow-down.
    let stamped = AT - SLOWED_AVERAGE_MS;
    handle
        .select(SelectRequest {
            request_class: Some("quota".into()),
            ..slowed_turn(&handle, "r-0", stamped)
        })
        .await
        .unwrap();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    let t2 = AT + 30_000;
    now.store(t2, Ordering::SeqCst);
    let hold = match handle
        .select(slowed_turn(&handle, "r-2", t2))
        .await
        .unwrap()
        .result
    {
        SelectResult::Held(hold) => hold,
        other => panic!("the pin with a cycle waits: {other:?}"),
    };
    assert_eq!(hold.reason, HoldReason::SlowDown);
    let until = hold
        .until
        .expect("a slow-down hold names the pin's deadline");
    let seen = drained(&mut events).await;
    let started: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, RouterEvent::SlowDownStarted { .. }))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        started.len(),
        1,
        "the second read under 1 continues the span: {seen:?}"
    );
    let first = match &seen[started[0] - 1] {
        RouterEvent::Decision { record, .. } => record,
        other => panic!("the start follows the decision whose read opened the span, not {other:?}"),
    };
    assert_eq!(first.request.request_id, "r-0");
    match &seen[started[0]] {
        RouterEvent::SlowDownStarted {
            account_id,
            pool_id,
            factor,
            stretch_ms,
            threshold_ms,
            demand,
            reset,
            at,
        } => {
            assert_eq!(
                (account_id.as_str(), pool_id.as_str(), *reset, *at),
                ("a", "p", Some(SLOWED_RESET), stamped)
            );
            assert!(*factor > 0.0 && *factor < 1.0, "{factor}");
            assert_eq!(
                (
                    FactValue::Number(*factor),
                    FactValue::Int(*stretch_ms),
                    FactValue::Int(*threshold_ms),
                    FactValue::Number(*demand)
                ),
                (
                    shed_fact(first, "factor"),
                    shed_fact(first, "stretch_ms"),
                    shed_fact(first, "threshold_ms"),
                    shed_fact(first, "demand_five_hour")
                ),
                "the start carries the read's figures, the demand in share per hour"
            );
            assert_eq!(*threshold_ms, 1_800_000);
        }
        _ => unreachable!(),
    }
    // The window reads far under the ceiling a minute after the hold's deadline: the factor returns to 1, and the span
    // stays open through the wait.
    let t3 = until + 60_000;
    now.store(t3, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            sample_with(t3, FIVE_HOUR, 0.1, Some(SLOWED_RESET)),
        ))
        .await;
    handle.samples().await.unwrap();
    assert!(matches!(
        handle
            .select(slowed_turn(&handle, "r-3", t3))
            .await
            .unwrap()
            .result,
        SelectResult::Selected(_)
    ));
    let seen = drained(&mut events).await;
    let (_, record) = decisions_of(&seen)
        .into_iter()
        .find(|(id, _)| id == "r-3")
        .expect("the third turn's decision");
    assert_eq!(shed_fact(record, "factor"), FactValue::Number(1.0));
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            RouterEvent::SlowDownStarted { .. } | RouterEvent::SlowDownEnded { .. }
        )),
        "a return shorter than the wait closes no span: {seen:?}"
    );

    // Once the factor has read 1 for the wait, the purge tick closes the span at the return.
    now.store(t3 + SLOW_DOWN_WAIT_MS, Ordering::SeqCst);
    let ended = loop {
        if let ended @ RouterEvent::SlowDownEnded { .. } = next_event(&mut events).await {
            break ended;
        }
    };
    let RouterEvent::SlowDownEnded {
        account_id,
        pool_id,
        since,
        duration_ms,
        factor_min,
        held_ms,
        holds: _,
        at,
    } = ended
    else {
        unreachable!()
    };
    assert_eq!(
        (
            account_id.as_str(),
            pool_id.as_str(),
            since,
            duration_ms,
            held_ms,
            at
        ),
        (
            "a",
            "p",
            stamped,
            t3 - stamped,
            until - t2,
            t3 + SLOW_DOWN_WAIT_MS
        ),
        "the span runs from the first read under 1 to the return, with the hold it answered"
    );
    // The lowest factor is the stamp turn's: the 0.05 of room under the ceiling over the 70 minutes to the reset at a
    // tenth of the window an hour, 0.05 / (0.1 x 70 / 60) = 3 / 7. The main turns, nearer the reset, read about 0.75.
    assert!(
        (factor_min - 0.428_571_428_571).abs() < 1e-9,
        "the span's lowest factor: {factor_min}"
    );
    assert!(
        !drained(&mut events)
            .await
            .iter()
            .any(|event| matches!(event, RouterEvent::SlowDownEnded { .. })),
        "the span ends once"
    );
}

/// Two live pins on `a`, each with 1,000 tokens of prompt: `s-ended`'s row carries the mark of an agent that ended,
/// `s-live`'s none. Each session's own request is the agent's return, which clears the mark.
#[tokio::test]
async fn a_row_that_carries_ended_hands_on_the_release() {
    let now = Arc::new(AtomicI64::new(AT));
    let store = seeded(&["a"]);
    store
        .upsert_pin(&PinRow {
            ended: true,
            ..stored_pin(&store, "s-ended", "a", 1, 0.0)
        })
        .unwrap();
    store
        .upsert_pin(&stored_pin(&store, "s-live", "a", 2, 0.0))
        .unwrap();
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let mut released = Vec::new();
    for session in ["s-ended", "s-live"] {
        match handle
            .select(request(&handle, Some(session), AT))
            .await
            .unwrap()
            .result
        {
            SelectResult::Selected(sel) => released.push((sel.account.id, sel.agent_released)),
            other => panic!("the pin renews: {other:?}"),
        }
    }
    assert_eq!(
        released,
        [
            ("a".to_string(), Some(true)),
            ("a".to_string(), Some(false))
        ]
    );
}

/// `slowed_router`'s session takes its two turns, then the companion releases its agent, and the turn after the release
/// waits its part of the slow-down: the routing task cleared the mark ahead of that first pass, so the second pass,
/// which carries the hold as the relay does, finds the row unmarked.
#[tokio::test]
async fn a_release_rides_a_hold_to_the_landing_behind_it() {
    let now = Arc::new(AtomicI64::new(AT));
    let handle = slowed_router(&now).await;
    handle
        .select(SelectRequest {
            request_class: Some("quota".into()),
            ..slowed_turn(&handle, "r-0", AT - SLOWED_AVERAGE_MS)
        })
        .await
        .unwrap();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    handle
        .report(Report::AgentEnded {
            key_id: "k".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
        })
        .await;
    let t2 = AT + 30_000;
    now.store(t2, Ordering::SeqCst);
    let req = slowed_turn(&handle, "r-2", t2);
    let hold = match handle.select(req.clone()).await.unwrap().result {
        SelectResult::Held(hold) => hold,
        other => panic!("the pin with a cycle waits: {other:?}"),
    };
    assert_eq!(hold.reason, HoldReason::SlowDown);
    let until = hold
        .until
        .expect("a slow-down hold names the pin's deadline");
    now.store(until, Ordering::SeqCst);
    let carried = SelectRequest {
        seq: 2,
        now: until,
        holds: vec![HoldFacts {
            held_ms: (until - t2) as u64,
            ..hold
        }],
        ..req
    };
    match handle.select(carried).await.unwrap().result {
        SelectResult::Selected(sel) => assert_eq!(
            (sel.account.id.as_str(), sel.agent_released),
            ("a", Some(true))
        ),
        other => panic!("the request that waited renews: {other:?}"),
    }
}

#[tokio::test]
async fn a_purge_that_drops_a_slowed_entry_ends_its_span() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(AT));
    let handle = slowed_router(&now).await;
    // A reading whose reset lies two hours out, past the purge below: `SLOWED_RESET` passes forty minutes in, and the
    // entry read before it would leave at that reset with its span ending there. The drop this case reads is the quiet
    // account's, whose span ends at the entry's last read.
    handle
        .report(observed(
            "a",
            sample_with(AT + 1, FIVE_HOUR, 0.9, Some(AT + 2 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    let factor = drained(&mut events)
        .await
        .into_iter()
        .find_map(|event| match event {
            RouterEvent::SlowDownStarted { factor, .. } => Some(factor),
            _ => None,
        })
        .expect("the read under 1 opened a span");
    // The session idles past the pool's hour of affinity and the account goes quiet: the purge expires the pin and
    // drops the entry, and the span, whose factor never returned to 1, ends at the entry's last read.
    now.store(AT + IN_FLIGHT_RECLAIM_MS + 1_000, Ordering::SeqCst);
    let ended = loop {
        if let RouterEvent::SlowDownEnded {
            account_id,
            pool_id,
            since,
            duration_ms,
            factor_min,
            held_ms,
            ..
        } = next_event(&mut events).await
        {
            break (account_id, pool_id, since, duration_ms, factor_min, held_ms);
        }
    };
    assert_eq!(ended, ("a".into(), "p".into(), AT, 0, factor, 0));
    assert!(
        !drained(&mut events)
            .await
            .iter()
            .any(|event| matches!(event, RouterEvent::SlowDownEnded { .. })),
        "the dropped entry's span ends once"
    );
}

#[tokio::test]
async fn a_purge_after_the_reset_drops_the_slowed_entry_and_ends_its_span_at_the_reset() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(AT));
    let handle = slowed_router(&now).await;
    let mut events = handle.subscribe_events();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    let factor = drained(&mut events)
        .await
        .into_iter()
        .find_map(|event| match event {
            RouterEvent::SlowDownStarted { factor, .. } => Some(factor),
            _ => None,
        })
        .expect("the read under 1 opened a span");
    // The window's reset passes with the session still pinned and no selection after it: the purge tick drops the
    // entry of the old window, and its span ends at the reset.
    now.store(SLOWED_RESET + 60_000, Ordering::SeqCst);
    // Asked before the purge's end is awaited, so the answer holds whether or not the tick has dropped the entry yet.
    let a = projection_of_account(&handle.projections("p").await.unwrap(), "a");
    assert_eq!(a.slow_down, None, "no entry answers after the reset");
    let five_hour = a.windows[FIVE_HOUR];
    assert_eq!(
        five_hour.demand, five_hour.burn,
        "the demand is the achieved burn: {five_hour:?}"
    );
    let ended = loop {
        if let RouterEvent::SlowDownEnded {
            account_id,
            pool_id,
            since,
            duration_ms,
            factor_min,
            held_ms,
            ..
        } = next_event(&mut events).await
        {
            break (account_id, pool_id, since, duration_ms, factor_min, held_ms);
        }
    };
    assert_eq!(
        ended,
        ("a".into(), "p".into(), AT, SLOWED_RESET - AT, factor, 0)
    );
    assert!(
        !drained(&mut events)
            .await
            .iter()
            .any(|event| matches!(event, RouterEvent::SlowDownEnded { .. })),
        "the dropped entry's span ends once"
    );
}

#[tokio::test]
async fn a_clean_poll_that_drops_the_five_hour_window_ends_the_account_s_slow_down_at_the_drop() {
    use phoebis_routing::RouterEvent;
    let now = Arc::new(AtomicI64::new(AT));
    // Held, so no purge tick emits the span's end and the drop is the one pass that can.
    let handle = slowed_router_under(
        &now,
        RouterConfig {
            hold_first_purge: true,
            ..priced_config()
        },
    )
    .await;
    let mut events = handle.subscribe_events();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    assert!(
        drained(&mut events)
            .await
            .iter()
            .any(|event| matches!(event, RouterEvent::SlowDownStarted { .. })),
        "the read under 1 opened a span"
    );
    let dropped_at = AT + 1_000;
    now.store(dropped_at, Ordering::SeqCst);
    handle.report(polled(dropped_at, &[SEVEN_DAY])).await;
    match next_event(&mut events).await {
        RouterEvent::SlowDownEnded {
            account_id,
            pool_id,
            since,
            at,
            ..
        } => assert_eq!(
            (account_id.as_str(), pool_id.as_str(), since, at),
            ("a", "p", AT, dropped_at)
        ),
        other => panic!("the drop ends the span before any selection, not {other:?}"),
    }
}

/// `account`'s answer among `projections`.
fn projection_of_account(
    projections: &[phoebis_routing::AccountProjection],
    account: &str,
) -> phoebis_routing::AccountProjection {
    projections
        .iter()
        .find(|p| p.account_id == account)
        .cloned()
        .unwrap_or_else(|| panic!("no projection of {account}: {projections:?}"))
}

#[tokio::test]
async fn a_members_projection_answers_its_account_wide_cooldown_and_a_clean_member_none() {
    use phoebis_routing::Obstacle;
    let store = seeded(&["a", "b"]);
    store.upsert_cooldown(&cooldown_row("a")).unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let projections = handle.projections("p").await.unwrap();
    assert_eq!(
        (
            projection_of_account(&projections, "a").obstacle,
            projection_of_account(&projections, "b").obstacle
        ),
        (
            Some(Obstacle::Cooldown {
                reason: "rate_limit".into(),
                until: 1_000 + 48 * HOUR,
                window: "seven_day".into(),
                scope: CooldownScope::Account,
            }),
            None
        ),
    );
}

#[tokio::test]
async fn a_regime_that_a_later_revision_names_joins_the_fits_at_its_instant() {
    // The instant from which the pricer names the regime `desk`.
    const T: i64 = AT;
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    // The pool's one warm pin stands on b, so a's own burn is zero and a fresh session's prior on a is that pin's burn
    // of one input token a second.
    store
        .upsert_pin(&PinRow {
            burn_at: Some(T - 1),
            last_refresh_at: T - 1_000,
            ..stored_pin(&store, "s1", "b", 1, 1.0)
        })
        .unwrap();
    // A token a second burns a hundredth of the 5-hour window an hour, ten times that under `desk`, and a thousandth
    // of the 7-day window an hour.
    let share = SharePricer::default()
        .with_rate(|window, entrypoint, _, _, burn, _| {
            let per_hour = match window {
                FIVE_HOUR if entrypoint == Some("desk") => 0.1,
                FIVE_HOUR => 0.01,
                SEVEN_DAY => 0.001,
                _ => return None,
            };
            Some(burn.input * per_hour / 3_600.0)
        })
        .with_regimes(|at| {
            if at >= T {
                vec![("desk".to_string(), vec!["desk".to_string()])]
            } else {
                Vec::new()
            }
        });
    let now = Arc::new(AtomicI64::new(T - 1));
    let handle = spawn(
        store,
        clock(&now),
        RouterConfig {
            share,
            ..RouterConfig::default()
        },
    )
    .unwrap();
    handle.report(windows("a", 0.5, 0.1, T - 1)).await;
    let fits = |projections: Vec<phoebis_routing::AccountProjection>| {
        projection_of_account(&projections, "a")
            .regime_fits
            .into_iter()
            .map(|entry| (entry.regime, entry.fit))
            .collect::<Vec<_>>()
    };
    // a's 5-hour window has 0.45 of room to its ceiling of 0.95 over the five hours to its horizon. The prior reaches
    // the ceiling in 45 hours under the base regime and in 4.5 hours under `desk`.
    assert_eq!(
        fits(handle.projections("p").await.unwrap()),
        [(None, "fits")]
    );
    now.store(T, Ordering::SeqCst);
    assert_eq!(
        fits(handle.projections("p").await.unwrap()),
        [(None, "fits"), (Some("desk".to_string()), "rewrite_only")]
    );
}

#[tokio::test]
async fn the_projections_ask_answers_the_slow_down_read_and_the_demand_beside_the_projection() {
    let now = Arc::new(AtomicI64::new(AT));
    let store = shedding_store(&["a"]);
    // An api_key member beside the slowed account: it has no 5-hour window to slow.
    store
        .insert_account(&AccountRow::for_tests("d", AccountKind::ApiKey))
        .unwrap();
    store
        .set_pool_members(
            "p",
            &[("a", 0), ("d", 1)].map(|(id, priority)| MemberWrite {
                account_id: id.into(),
                priority,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }),
            "test",
        )
        .unwrap();
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.slow_down.slow_down = true;
    pool.slow_down.slow_down_stretch_seconds = 900;
    pool.slow_down.slow_down_hold_seconds = 90;
    pool.shedding.burn_average_seconds = (SLOWED_AVERAGE_MS / 1_000) as u64;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    with_tier(&store, "a");
    // `slowed_router`'s figures: s1 burns a tenth of the 5-hour window an hour on `a`, at 0.9 with 40 minutes left.
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.9, Some(SLOWED_RESET)),
        ))
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        projection_of_account(&handle.projections("p").await.unwrap(), "a").slow_down,
        None,
        "no pinned selection has read the account yet"
    );

    // An auxiliary turn one burn average before the first main turn reads the factor under 1, opens the span and
    // stamps the ramp, so the hold below applies the read's whole factor. The first main turn reads it under 1 again.
    // No pin has a cycle yet, so none is held, the realized speed is 1 and the demand is the achieved burn.
    let stamped = AT - SLOWED_AVERAGE_MS;
    handle
        .select(SelectRequest {
            request_class: Some("quota".into()),
            ..slowed_turn(&handle, "r-0", stamped)
        })
        .await
        .unwrap();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    let first = handle.projections("p").await.unwrap();
    let a = projection_of_account(&first, "a");
    let entry = a.slow_down.expect("the pinned read wrote a's entry");
    assert!(entry.factor > 0.0 && entry.factor < 1.0, "{entry:?}");
    assert!(entry.stretch_ms > 0, "{entry:?}");
    assert_eq!(
        (
            entry.speed,
            entry.span.map(|s| s.since),
            entry.span.map(|s| s.ramp),
            entry.threshold_ms,
            entry.hold_ms
        ),
        (1.0, Some(stamped), Some(1.0), 900_000, 90_000),
        "the entry's own speed, its span's start, the whole ramp one burn average after it, and the pool's threshold \
         and hold cap"
    );
    let five_hour = a.windows[FIVE_HOUR];
    assert!(
        five_hour.burn > 0.0 && five_hour.demand >= five_hour.burn,
        "{five_hour:?}"
    );
    assert!(
        five_hour.projection > five_hour.predicted,
        "the same answer projects the achieved burn to the horizon: {five_hour:?}"
    );
    assert_eq!(
        projection_of_account(&first, "d").slow_down,
        None,
        "an api_key account has no 5-hour window to slow"
    );

    // The second turn plants s1's cycle and waits its part of the slow-down: the speed the plan realizes is under 1,
    // and the demand divides the pin's burn by it.
    now.store(AT + 30_000, Ordering::SeqCst);
    handle
        .select(slowed_turn(&handle, "r-2", AT + 30_000))
        .await
        .unwrap();
    let a = projection_of_account(&handle.projections("p").await.unwrap(), "a");
    let entry = a.slow_down.expect("the second read rewrote a's entry");
    assert!(entry.speed < 1.0, "{entry:?}");
    let five_hour = a.windows[FIVE_HOUR];
    assert!(five_hour.demand > five_hour.burn, "{five_hour:?}");

    // The pool's switch off: the entry still stands until the purge tick, and none is in force at once.
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.slow_down.slow_down = false;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    handle.reload().await.unwrap();
    assert_eq!(
        projection_of_account(&handle.projections("p").await.unwrap(), "a").slow_down,
        None,
        "a pool with the slow-down off answers no entry"
    );
}

#[tokio::test]
async fn a_slowed_account_answers_the_figure_that_the_stretch_test_compares() {
    let now = Arc::new(AtomicI64::new(AT));
    let store = shedding_store(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.five_hour_ceiling = 0.95;
    pool.slow_down.slow_down = true;
    pool.slow_down.slow_down_stretch_seconds = 240;
    pool.shedding.burn_average_seconds = (SLOWED_AVERAGE_MS / 1_000) as u64;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    with_tier(&store, "a");
    // s1 burns a tenth of the 5-hour window an hour on `a`, at 0.9 with 40 minutes left: the demand passes the ceiling
    // half an hour in, before the reset less the threshold of four minutes.
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let handle = spawn(store, clock(&now), priced_config()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.9, Some(SLOWED_RESET)),
        ))
        .await;
    handle.samples().await.unwrap();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();

    let a = projection_of_account(&handle.projections("p").await.unwrap(), "a");
    let entry = a.slow_down.expect("the pinned read wrote a's entry");
    assert!(
        close(entry.stretch_projection, 0.96),
        "0.9 and a tenth an hour over the 36 minutes to the reset less the threshold: {entry:?}"
    );
    let five_hour = a.windows[FIVE_HOUR];
    assert!(
        !close(Some(five_hour.projection), 0.96),
        "the window's projection runs on to the reset itself: {five_hour:?}"
    );
}

#[tokio::test]
async fn a_slowed_account_read_again_answers_the_figure_of_its_newest_read() {
    let now = Arc::new(AtomicI64::new(AT));
    let store = shedding_store(&["a"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.five_hour_ceiling = 0.95;
    pool.slow_down.slow_down = true;
    pool.slow_down.slow_down_stretch_seconds = 240;
    pool.shedding.burn_average_seconds = (SLOWED_AVERAGE_MS / 1_000) as u64;
    store.update_pool(&pool, pool.updated_at, "test").unwrap();
    with_tier(&store, "a");
    // s1's burn was never folded, so both reads take its tenth of the 5-hour window an hour undecayed. The first read
    // at `AT` writes 0.96: 0.9 and a tenth an hour over the 36 minutes to the reset less the threshold.
    store
        .upsert_pin(&PinRow {
            burn_at: None,
            ..stored_pin(&store, "s1", "a", 1, 10.0)
        })
        .unwrap();
    let handle = spawn(store, clock(&now), priced_config()).unwrap();
    handle
        .report(observed(
            "a",
            sample_with(AT, FIVE_HOUR, 0.9, Some(SLOWED_RESET)),
        ))
        .await;
    handle.samples().await.unwrap();
    handle
        .select(slowed_turn(&handle, "r-1", AT))
        .await
        .unwrap();
    // Twelve minutes on, inside the same window, the second read changes the standing entry.
    let second = AT + 12 * 60_000;
    now.store(second, Ordering::SeqCst);
    handle
        .select(slowed_turn(&handle, "r-2", second))
        .await
        .unwrap();

    let entry = projection_of_account(&handle.projections("p").await.unwrap(), "a")
        .slow_down
        .expect("the pinned reads wrote a's entry");
    assert!(
        close(entry.stretch_projection, 0.94),
        "0.9 and a tenth an hour over the 24 minutes from the second read to the reset less the threshold: {entry:?}"
    );
}

#[tokio::test]
async fn pins_standing_answers_the_speed_and_the_cycle_of_a_pin() {
    use phoebis_routing::FactValue;
    let now = Arc::new(AtomicI64::new(AT));
    let handle = slowed_router(&now).await;
    let mut events = handle.subscribe_events();
    let turn = |session: &str, request_id: &str, at: i64| SelectRequest {
        request_id: request_id.into(),
        request_class: Some("main".into()),
        ..request(&handle, Some(session), at)
    };
    // s2 and s3 are placed by their first turns, which fold nothing. Each one's next turn starts its pace, and s2's
    // third plants its cycle from the gap between its second and third.
    for (session, request_id, at) in [
        ("s2", "r-1", AT),
        ("s3", "r-2", AT + 1_000),
        ("s2", "r-3", AT + 10_000),
        ("s3", "r-4", AT + 11_000),
        ("s2", "r-5", AT + 22_000),
    ] {
        now.store(at, Ordering::SeqCst);
        handle.select(turn(session, request_id, at)).await.unwrap();
    }
    // The entry holds the factor the last pinned read wrote.
    let seen = drained(&mut events).await;
    let (_, last) = decisions_of(&seen)
        .into_iter()
        .find(|(id, _)| id == "r-5")
        .expect("the last turn's decision");
    let FactValue::Number(factor) = shed_fact(last, "factor") else {
        panic!("the last read states its factor");
    };
    assert!(factor < 1.0, "{factor}");
    let speed = projection_of_account(&handle.projections("p").await.unwrap(), "a")
        .slow_down
        .expect("the pinned reads wrote a's entry")
        .speed;
    let standing = handle.pins_standing().await.unwrap();
    let of = |session: &str| {
        standing
            .iter()
            .find(|p| p.pin.session_id == session)
            .map(|p| {
                (
                    p.slow_down.map(|s| s.factor),
                    p.slow_down.map(|s| s.speed),
                    p.cycle_ms,
                )
            })
            .unwrap_or_else(|| panic!("no pin of {session}: {standing:?}"))
    };
    assert_eq!(
        (of("s2"), of("s3")),
        (
            (Some(factor), Some(speed), Some(12_000.0)),
            (Some(factor), Some(speed), None)
        ),
        "each pin reads its account's factor and realized speed, and the cycle stands from a pin's third request on"
    );
}

#[tokio::test]
async fn a_restarted_router_reads_each_pins_flag_and_effort_from_its_row_and_a_refreshing_turn_persists_the_clear(
) {
    let store = seeded(&["a"]);
    // Both last refreshed at their placement: a conversation pin no second refreshing turn renewed, and a pin only
    // auxiliary requests used.
    let conversation = PinRow {
        last_refresh_at: AT - HOUR,
        effort: Some("high".into()),
        ..stored_pin(&store, "s1", "a", 1, 0.0)
    };
    let auxiliary = PinRow {
        last_refresh_at: AT - HOUR,
        auxiliary_only: true,
        ..stored_pin(&store, "s2", "a", 2, 0.0)
    };
    for pin in [&conversation, &auxiliary] {
        store.upsert_pin(pin).unwrap();
    }
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let standing = handle.pins_standing().await.unwrap();
    let of = |session: &str| {
        standing
            .iter()
            .find(|p| p.pin.session_id == session)
            .map(|p| (p.pin.auxiliary_only, p.pin.effort.clone()))
            .unwrap_or_else(|| panic!("no pin of {session}: {standing:?}"))
    };
    assert_eq!(
        of("s1"),
        (false, Some("high".to_string())),
        "the conversation pin reads as one after the restart, with its stored effort"
    );
    assert_eq!(
        of("s2"),
        (true, None),
        "the flagged row reads flagged, and a row with no effort answers none"
    );
    handle
        .select(SelectRequest {
            request_class: Some("main".into()),
            ..request(&handle, Some("s2"), AT)
        })
        .await
        .unwrap();
    wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .find(|p| p.session_id == "s2" && !p.auxiliary_only)
    })
    .await;
    assert!(
        !handle
            .pins_standing()
            .await
            .unwrap()
            .iter()
            .any(|p| p.pin.auxiliary_only),
        "the main turn's refresh clears the flag in the state and in the store"
    );
}

/// Session `session`'s request of `agent` on `model` at `now`, under a request id of its own.
fn turn_of(
    handle: &RouterHandle,
    session: &str,
    agent: &str,
    model: &str,
    now: i64,
) -> SelectRequest {
    let s = handle.snapshot();
    SelectRequest {
        request_id: format!("r-{session}-{agent}-{now}"),
        agent_id: agent.into(),
        canonical_model: model.into(),
        folded_model: model.into(),
        candidates: resolve(&s, s.key("k").unwrap(), model, model).unwrap(),
        lane: lane_key(model, "standard", "none", "-"),
        parent_agent_id: (agent != "main").then(|| "main".into()),
        ..request(handle, Some(session), now)
    }
}

#[tokio::test]
async fn a_client_renewal_writes_the_effort_its_pin_names_to_the_row_and_a_probes_renewal_keeps_the_stored_one(
) {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let placed = selected_pin(
        handle
            .select(SelectRequest {
                effort: Some("high".into()),
                ..request(&handle, Some("s1"), 1_000)
            })
            .await
            .unwrap(),
    );
    let renewed = |effort: Option<&str>, keepalive: bool, at: i64| Report::PinRenewed {
        pin: PinRef {
            effort: effort.map(String::from),
            ..placed.clone()
        },
        now: at,
        usage: None,
        keepalive,
        cache_ttl_ms: 300_000,
        fast: false,
        turn_end: if keepalive {
            RenewalEnd::NoTurn
        } else {
            RenewalEnd::Turn
        },
        thinking_tokens: None,
        input_transformations: None,
    };
    let stored = || async { handle.pins().await.unwrap().remove(0).effort };
    assert_eq!(
        stored().await.as_deref(),
        Some("high"),
        "the placement stamped the word"
    );
    handle.report(renewed(Some("low"), false, 2_000)).await;
    assert_eq!(
        stored().await.as_deref(),
        Some("low"),
        "a client's renewal writes the word its pin names"
    );
    handle.report(renewed(Some("max"), true, 3_000)).await;
    assert_eq!(
        stored().await.as_deref(),
        Some("low"),
        "a probe replays another request's body, so its renewal keeps the stored word"
    );
    handle.report(renewed(None, false, 4_000)).await;
    assert_eq!(
        stored().await,
        None,
        "a client's renewal whose pin names none writes none"
    );
}

#[tokio::test]
async fn a_token_count_behind_a_turn_keeps_the_effort_the_turn_stamped() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let placed = selected_pin(
        handle
            .select(SelectRequest {
                effort: Some("xhigh".into()),
                ..turn_of(&handle, "s1", "main", "claude-sonnet-5", 1_000)
            })
            .await
            .unwrap(),
    );
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(Report::PinRenewed {
            pin: placed,
            now: 2_000,
            usage: None,
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::Turn,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    now.store(3_000, Ordering::SeqCst);
    // The relay sends no renewal for a count's answer, so the count's selection is the last write the row takes.
    selected_pin(
        handle
            .select(SelectRequest {
                count: true,
                ..turn_of(&handle, "s1", "main", "claude-sonnet-5", 3_000)
            })
            .await
            .unwrap(),
    );
    assert_eq!(
        handle.pins().await.unwrap().remove(0).effort.as_deref(),
        Some("xhigh"),
        "a token count leaves the effort of the pin's last turn"
    );
}

#[tokio::test]
async fn a_fail_over_whose_landing_was_held_names_the_account_left_on_the_next_request() {
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    store
        .upsert_pin(&stored_pin(&store, "s1", "a", 1, 10.0))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle.report(windows("a", 0.3, 0.1, AT)).await;
    handle.report(windows("b", 0.3, 0.1, AT)).await;
    handle.samples().await.unwrap();
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: AT + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin_of(&handle).await),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    let cap = phoebis_store::Failover::default().in_flight_cap;
    let busy: Vec<_> = (0..cap)
        .map(|_| handle.in_flight().dispatched("b", AT))
        .collect();
    let held = handle
        .select(SelectRequest {
            attempted: vec!["a".into()],
            ..request(&handle, Some("s1"), AT)
        })
        .await
        .unwrap();
    assert!(
        matches!(&held.result, SelectResult::Held(hold) if hold.account_id == "b"),
        "the failover's landing waits for b's slot: {:?}",
        held.result
    );
    drop(held);
    drop(busy);
    let next = handle
        .select(request(&handle, Some("s1"), AT + 1_000))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = &next.result else {
        panic!("{:?}", next.result)
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.left.as_deref()),
        ("b", Some("a")),
        "the session's next request lands the carried failover and names the account it left"
    );
}

/// `s1` moved off `a`'s spent window to `b` by its request `r-1` at 3_100 on `shedding_store(ids)`, as
/// `the_spent_state_reads_the_raised_reading_through_a_head_within_the_margin` stages it, `b`'s windows read before
/// the move.
async fn moved_off_a(ids: &[&str]) -> (RouterHandle, Arc<AtomicI64>) {
    use phoebis_routing::MigrateReason;
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(shedding_store(ids), clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        landed_on(
            handle
                .select(request(&handle, Some("s1"), 1_000))
                .await
                .unwrap()
        ),
        ("a".into(), None)
    );
    handle.report(windows("b", 0.2, 0.1, 1_500)).await;
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(observed(
            "a",
            sample_with(2_000, FIVE_HOUR, 1.0, Some(10 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    now.store(3_100, Ordering::SeqCst);
    assert_eq!(
        landed_on(
            handle
                .select(request(&handle, Some("s1"), 3_100))
                .await
                .unwrap()
        ),
        ("b".into(), Some(MigrateReason::Spent)),
        "r-1 moves the pin off a's spent window"
    );
    (handle, now)
}

/// The session's request `request_id` at `now`.
fn request_of(handle: &RouterHandle, request_id: &str, now: i64) -> SelectRequest {
    SelectRequest {
        request_id: request_id.into(),
        ..request(handle, Some("s1"), now)
    }
}

/// The rewrite hold a selection answered, naming `r-1`.
fn rewrite_hold(answer: phoebis_routing::SelectAnswer) -> HoldFacts {
    match answer.result {
        SelectResult::Held(hold) => {
            assert_eq!(
                (hold.reason, hold.waits_for.as_deref()),
                (HoldReason::Rewrite, Some("r-1"))
            );
            hold
        }
        other => panic!("expected the rewrite's hold, got {other:?}"),
    }
}

#[tokio::test]
async fn a_head_report_clears_the_mark_and_the_pins_next_request_renews_unheld() {
    let (handle, now) = moved_off_a(&["a", "b"]).await;
    now.store(3_300, Ordering::SeqCst);
    let req = request_of(&handle, "r-2", 3_300);
    let hold = rewrite_hold(handle.select(req.clone()).await.unwrap());
    handle
        .report(Report::Head {
            request_id: "r-1".into(),
            attempt: 1,
            at: 3_400,
        })
        .await;
    now.store(3_500, Ordering::SeqCst);
    let carried = SelectRequest {
        seq: 2,
        now: 3_500,
        holds: vec![hold],
        ..req
    };
    assert_eq!(
        landed_on(handle.select(carried).await.unwrap()),
        ("b".into(), None),
        "the request that waited renews"
    );
    assert_eq!(
        landed_on(
            handle
                .select(request_of(&handle, "r-3", 3_600))
                .await
                .unwrap()
        ),
        ("b".into(), None),
        "with the mark gone a request carrying no hold renews too"
    );
}

#[tokio::test]
async fn an_end_report_clears_the_mark() {
    let (handle, now) = moved_off_a(&["a", "b"]).await;
    now.store(3_300, Ordering::SeqCst);
    rewrite_hold(
        handle
            .select(request_of(&handle, "r-2", 3_300))
            .await
            .unwrap(),
    );
    handle
        .report(Report::Ended {
            request_id: "r-1".into(),
            attempt: 1,
            at: 3_400,
            cause: "transport_error",
        })
        .await;
    assert_eq!(
        landed_on(
            handle
                .select(request_of(&handle, "r-3", 3_500))
                .await
                .unwrap()
        ),
        ("b".into(), None)
    );
}

#[tokio::test]
async fn a_pin_invalidated_report_drops_the_mark_with_the_row() {
    let (handle, now) = moved_off_a(&["a", "b"]).await;
    now.store(3_300, Ordering::SeqCst);
    rewrite_hold(
        handle
            .select(request_of(&handle, "r-2", 3_300))
            .await
            .unwrap(),
    );
    handle
        .report(Report::PinInvalidated {
            account_id: "b".into(),
            pin: pin_of(&handle).await,
            mark: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    let answer = handle
        .select(request_of(&handle, "r-3", 3_500))
        .await
        .unwrap();
    assert!(
        matches!(&answer.result, SelectResult::Selected(sel) if !sel.trace.renewed),
        "the session's next request is placed afresh: {:?}",
        answer.result
    );
}

#[tokio::test]
async fn a_head_of_an_earlier_leg_leaves_the_mark_a_later_landing_set() {
    use phoebis_routing::MigrateReason;
    let (handle, now) = moved_off_a(&["a", "b", "c"]).await;
    handle.report(windows("c", 0.2, 0.1, 3_200)).await;
    handle
        .report(observed(
            "b",
            sample_with(3_200, FIVE_HOUR, 1.0, Some(10 * HOUR)),
        ))
        .await;
    handle.samples().await.unwrap();
    now.store(3_300, Ordering::SeqCst);
    // The failover's re-selection of the same request lands it again under the next attempt.
    let again = SelectRequest {
        attempt: 2,
        ..request_of(&handle, "r-1", 3_300)
    };
    assert_eq!(
        landed_on(handle.select(again).await.unwrap()),
        ("c".into(), Some(MigrateReason::Spent))
    );
    handle
        .report(Report::Head {
            request_id: "r-1".into(),
            attempt: 1,
            at: 3_400,
        })
        .await;
    now.store(3_500, Ordering::SeqCst);
    rewrite_hold(
        handle
            .select(request_of(&handle, "r-2", 3_500))
            .await
            .unwrap(),
    );
    handle
        .report(Report::Head {
            request_id: "r-1".into(),
            attempt: 2,
            at: 3_600,
        })
        .await;
    assert_eq!(
        landed_on(
            handle
                .select(request_of(&handle, "r-3", 3_700))
                .await
                .unwrap()
        ),
        ("c".into(), None),
        "the later leg's report clears its own landing's mark"
    );
}

/// A turn of `session` under `agent` and `class` on `model`, with `attempted` so the case steers where it lands.
fn agent_turn(
    handle: &RouterHandle,
    session: &str,
    agent: &str,
    class: &str,
    model: &str,
    attempted: &[&str],
    now: i64,
) -> SelectRequest {
    let s = handle.snapshot();
    SelectRequest {
        agent_id: agent.into(),
        canonical_model: model.into(),
        folded_model: model.into(),
        candidates: resolve(&s, s.key("k").unwrap(), model, model).unwrap(),
        attempted: attempted.iter().map(|a| a.to_string()).collect(),
        request_class: Some(class.into()),
        lane: lane_key(model, "standard", "none", "-"),
        ..request(handle, Some(session), now)
    }
}

#[tokio::test]
async fn a_classifier_call_follows_the_turn_that_ended_last_rather_than_the_request_that_arrived_last(
) {
    let store = seeded(&["a", "b", "c"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let opus = "claude-opus-5-5";
    handle
        .select(agent_turn(
            &handle,
            "s1",
            "main",
            "main",
            opus,
            &["b", "c"],
            1_000,
        ))
        .await
        .unwrap();
    let x = handle
        .select(agent_turn(
            &handle,
            "s1",
            "x",
            "subagent",
            opus,
            &["a", "c"],
            2_000,
        ))
        .await
        .unwrap();
    assert!(matches!(x.result, SelectResult::Selected(ref s) if s.account.id == "b"));
    // x's stream ends on b, then y's request arrives on c ahead of x's classifier call.
    let usage = PinUsage {
        input: 10,
        cache_read: 0,
        write_5m: 0,
        write_1h: 0,
        output: 5,
    };
    handle
        .report(renewal(selected_pin(x), 3_000, usage, false, 300_000))
        .await;
    let y = handle
        .select(agent_turn(
            &handle,
            "s1",
            "y",
            "subagent",
            opus,
            &["a", "b"],
            3_010,
        ))
        .await
        .unwrap();
    assert!(matches!(y.result, SelectResult::Selected(ref s) if s.account.id == "c"));
    let call = SelectRequest {
        classifier: true,
        request_class: Some("auxiliary".into()),
        ..request(&handle, Some("s1"), 3_020)
    };
    let answer = handle.select(call).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "b"),
        "the call follows x's turn, whose stream ended last, not y's arrival: {:?}",
        answer.result
    );
}

#[tokio::test]
async fn a_classifier_call_holds_no_pin_and_its_completion_leaves_the_judged_pins_prompt_last_use_and_epoch(
) {
    use phoebis_routing::RouterEvent;
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    // s1's conversation runs on Opus, so no pin of it stands under the call's own key on Sonnet 5.
    let turn = handle
        .select(agent_turn(
            &handle,
            "s1",
            "main",
            "main",
            "claude-opus-5-5",
            &["b"],
            1_000,
        ))
        .await
        .unwrap();
    let usage = PinUsage {
        input: 10,
        output: 5,
        ..PinUsage::default()
    };
    handle
        .report(renewal(selected_pin(turn), 2_000, usage, false, 300_000))
        .await;
    let rows = |pins: Vec<PinRow>| {
        pins.into_iter()
            .map(|p| {
                (
                    p.agent_id,
                    p.model,
                    p.account_id,
                    p.epoch,
                    p.last_input_tokens,
                    p.last_used_at,
                )
            })
            .collect::<Vec<_>>()
    };
    let before = rows(handle.pins().await.unwrap());
    assert_eq!(
        before
            .iter()
            .map(|(_, model, account, _, prompt, used)| (
                model.as_str(),
                account.as_str(),
                *prompt,
                *used
            ))
            .collect::<Vec<_>>(),
        [("claude-opus-5-5", "a", 10, 2_000)]
    );
    let mut events = handle.subscribe_events();
    let answer = handle
        .select(classifier_call(&handle, 3_000))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = answer.result else {
        panic!("{:?}", answer.result);
    };
    assert_eq!(sel.pin, None);
    handle
        .report(classifier_usage(
            &sel.account.id,
            3_500,
            600,
            JudgedCharge::On,
            sel.judged.as_ref().and_then(|judged| judged.pin.clone()),
        ))
        .await;
    assert_eq!(
        rows(handle.pins().await.unwrap()),
        before,
        "the session holds the conversation's pin alone, its prompt, last use and epoch as its turn left them"
    );
    let seen = drained(&mut events).await;
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, RouterEvent::PinCreated { .. })),
        "the call creates no pin: {seen:?}"
    );
}

/// The decision of the next selection among `events`, whatever the reports before it emitted.
async fn next_record(
    events: &mut tokio::sync::broadcast::Receiver<phoebis_routing::RouterEvent>,
) -> DecisionRecord {
    loop {
        if let phoebis_routing::RouterEvent::Decision { record, .. } = next_event(events).await {
            return Arc::unwrap_or_clone(record);
        }
    }
}

/// The value of `key` on the decision's `pin` step, `None` where the step carries no such fact.
fn pin_step_fact(record: &DecisionRecord, key: &str) -> Option<phoebis_routing::FactValue> {
    record
        .walk
        .iter()
        .find(|step| step.step == "pin")
        .and_then(|step| step.facts.iter().find(|fact| fact.key == key))
        .map(|fact| fact.value.clone())
}

#[tokio::test]
async fn a_classifier_call_goes_to_the_account_its_judged_turn_ended_on_past_a_moved_pin_and_else_to_the_pins_account(
) {
    use phoebis_routing::{FactValue, JudgedCharge, MigrateReason};
    let store = shedding_store(&["a", "b"]);
    with_tier(&store, "a");
    with_tier(&store, "b");
    // s1's pin on a carries the mark of a round on a a minute ago, inside the pool's dwell, so the conversation's
    // next request moves it on that mark alone.
    store
        .upsert_pin(&marked(stored_pin(&store, "s1", "a", 1, 0.0), AT - 60_000))
        .unwrap();
    store.replace_shed_round("p", "a", 1, AT - 60_000).unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    past_the_wait(&handle, &store, "a", AT).await;
    handle.report(windows("a", 0.96, 0.3, AT)).await;
    handle.report(windows("b", 0.1, 0.1, AT)).await;
    handle.samples().await.unwrap();
    // s1's turn ends on a.
    let pin = PinRef {
        key_id: "k".into(),
        session_id: "s1".into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        epoch: 1,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        entrypoint: None,
        effort: None,
    };
    let usage = PinUsage {
        input: 10,
        cache_read: 0,
        write_5m: 0,
        write_1h: 0,
        output: 5,
    };
    handle.report(renewal(pin, AT, usage, false, 300_000)).await;
    assert_eq!(
        landed_on(
            handle
                .select(turn(&handle, false, AT + 1_000))
                .await
                .unwrap()
        ),
        ("b".into(), Some(MigrateReason::Shed)),
        "the conversation's next turn moves the pin on its mark, its stream still running"
    );
    let pins = |rows: Vec<PinRow>| {
        rows.into_iter()
            .map(|p| (p.agent_id, p.model, p.account_id, p.epoch))
            .collect::<Vec<_>>()
    };
    let moved = pins(handle.pins().await.unwrap());
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0].2, "b");
    let mut events = handle.subscribe_events();
    let answer = handle
        .select(classifier_call(&handle, AT + 2_000))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = answer.result else {
        panic!("{:?}", answer.result);
    };
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.judged.map(|judged| judged.charge)
        ),
        ("a", Some(JudgedCharge::On)),
        "the call goes to a, where the judged turn's stream ended, past the pin's move to b"
    );
    assert_eq!(
        pin_step_fact(&next_record(&mut events).await, "direction"),
        Some(FactValue::Text("turn_account".into()))
    );
    assert_eq!(
        pins(handle.pins().await.unwrap()),
        moved,
        "the call takes no pin along: the conversation's pin stays on b under its epoch"
    );
    // a's 5-hour window reads spent: the account the turn ended on holds no allowance, so the call goes to the
    // account the turn's pin stands on now, where the upstream charges it.
    handle.report(windows("a", 1.0, 0.3, AT + 2_500)).await;
    handle.samples().await.unwrap();
    let answer = handle
        .select(classifier_call(&handle, AT + 3_000))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = answer.result else {
        panic!("{:?}", answer.result);
    };
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.judged.map(|judged| judged.charge)
        ),
        ("b", Some(JudgedCharge::Off))
    );
    assert_eq!(
        pin_step_fact(&next_record(&mut events).await, "direction"),
        Some(FactValue::Text("pin_account".into()))
    );
}

#[tokio::test]
async fn a_classifier_call_inside_a_failover_window_reads_no_failover_and_leaves_the_landing_to_the_conversation(
) {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    // main's conversation runs on the classifier's model on a, and a cooldown on a fails its pin over.
    let pin = selected_pin(
        handle
            .select(SelectRequest {
                attempted: vec!["b".into()],
                ..turn(&handle, false, 1_000)
            })
            .await
            .unwrap(),
    );
    let mut events = handle.subscribe_events();
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    let answer = handle
        .select(classifier_call(&handle, 1_100))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = answer.result else {
        panic!("{:?}", answer.result);
    };
    let record = next_record(&mut events).await;
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.left.as_deref(),
            record.outcome.from_account_id.as_deref(),
            record.outcome.cause.as_ref(),
        ),
        ("b", None, None, None),
        "the call lands no session, so it names no failover's source"
    );
    let landing = handle.select(turn(&handle, false, 1_200)).await.unwrap();
    let SelectResult::Selected(sel) = landing.result else {
        panic!("{:?}", landing.result);
    };
    let record = next_record(&mut events).await;
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.trace.moved,
            sel.left.as_deref(),
            record.outcome.from_account_id.as_deref(),
            record.outcome.cause,
        ),
        (
            "b",
            Some("cooldown"),
            Some("a"),
            Some("a"),
            Some(MoveCause::Cooldown {
                scope: "account".into(),
                reason: "rate_limit".into(),
                window: Some("five_hour".into()),
            }),
        ),
        "the conversation's next request lands as the failover's landing, with its cause"
    );
}

#[tokio::test]
async fn an_auxiliary_calls_end_on_another_agents_turn_pin_does_not_redirect_the_classifier_call() {
    let store = seeded(&["a", "b", "c"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, clock(&now), RouterConfig::default()).unwrap();
    let opus = "claude-opus-5-5";
    handle
        .select(agent_turn(
            &handle,
            "s1",
            "main",
            "main",
            opus,
            &["b", "c"],
            1_000,
        ))
        .await
        .unwrap();
    let x = handle
        .select(agent_turn(
            &handle,
            "s1",
            "x",
            "subagent",
            opus,
            &["a", "c"],
            2_000,
        ))
        .await
        .unwrap();
    assert!(matches!(x.result, SelectResult::Selected(ref s) if s.account.id == "b"));
    let y = handle
        .select(agent_turn(
            &handle,
            "s1",
            "y",
            "subagent",
            opus,
            &["a", "b"],
            2_500,
        ))
        .await
        .unwrap();
    assert!(matches!(y.result, SelectResult::Selected(ref s) if s.account.id == "c"));
    let usage = || PinUsage {
        input: 10,
        cache_read: 0,
        write_5m: 0,
        write_1h: 0,
        output: 5,
    };
    // x's turn ends on b, then y's summary fork, an auxiliary call on y's own turn pin, ends on c.
    handle
        .report(renewal(selected_pin(x), 3_000, usage(), false, 300_000))
        .await;
    handle
        .report(Report::PinRenewed {
            pin: selected_pin(y),
            now: 3_010,
            usage: Some(usage()),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::NoTurn,
            thinking_tokens: None,
            input_transformations: None,
        })
        .await;
    let call = SelectRequest {
        classifier: true,
        request_class: Some("auxiliary".into()),
        ..request(&handle, Some("s1"), 3_020)
    };
    let answer = handle.select(call).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "b"),
        "the call follows x's turn, not the fork that ended on y's pin: {:?}",
        answer.result
    );
}

const SONNET: &str = "claude-sonnet-5";

/// The member key of a block dropped for the account the session left.
const DROPPED_FOR_ACCOUNT: &str = "thinking_dropped.end_user_binding_mismatch";

/// The member key of the client's standing prefix entry, which drops nothing the session's move caused.
const DROPPED_PREFIX: &str = "thinking_dropped.prefix_binding_mismatch";

/// The pool `p` over `accounts` in that priority order, each on `TIER`, with the facts an earlier task stored for
/// `claude-sonnet-5`, each as `(account_a, account_b, bound)` with the smaller id first.
fn thinking_store(accounts: &[&str], facts: &[(&str, &str, bool)]) -> Arc<Store> {
    let store = shedding_store(accounts);
    for account in accounts {
        with_tier(&store, account);
    }
    for (account_a, account_b, bound) in facts {
        store
            .write_routing(|tx| {
                tx.replace_thinking_binding(&phoebis_store::ThinkingBindingRow {
                    model: SONNET.into(),
                    account_a: account_a.to_string(),
                    incarnation_a: 1,
                    account_b: account_b.to_string(),
                    incarnation_b: 1,
                    bound: *bound,
                    observed_at: AT - HOUR,
                })
            })
            .unwrap();
    }
    store
}

/// A router on `store` under `priced_config` whose `pins` each answered two turns of a 100,000-token prompt with
/// 20,000 thinking tokens each, so every pin holds 40,000 for its account.
async fn thinking_router(
    store: &Arc<Store>,
    pins: &[PinRow],
    now: &Arc<AtomicI64>,
) -> RouterHandle {
    for pin in pins {
        store.upsert_pin(pin).unwrap();
    }
    let handle = spawn(store.clone(), clock(now), priced_config()).unwrap();
    for pin in pins {
        for _ in 0..2 {
            handle
                .report(answered(
                    session_pin(&handle, &pin.session_id).await,
                    100_000,
                    Some(20_000),
                    None,
                ))
                .await;
        }
    }
    handle
}

/// The pin `session` holds, as the relay names it in a report.
async fn session_pin(handle: &RouterHandle, session: &str) -> PinRef {
    let row = handle
        .pins()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.session_id == session)
        .expect("the session's pin");
    PinRef {
        key_id: row.key_id,
        session_id: row.session_id,
        agent_id: row.agent_id,
        model: row.model,
        epoch: row.epoch,
        lane: row.lane,
        entrypoint: None,
        effort: None,
    }
}

/// A client turn's renewal of `pin` at `AT` whose answer read a prompt of `prompt` input tokens, with the thinking
/// tokens it reported and the one key its `input_transformations` member listed.
fn answered(pin: PinRef, prompt: u64, thinking: Option<u64>, dropped: Option<&str>) -> Report {
    Report::PinRenewed {
        pin,
        now: AT,
        usage: Some(PinUsage {
            input: prompt,
            ..PinUsage::default()
        }),
        keepalive: false,
        cache_ttl_ms: 300_000,
        fast: false,
        turn_end: RenewalEnd::Turn,
        thinking_tokens: thinking,
        input_transformations: dropped.map(|key| {
            std::collections::BTreeMap::from([(
                key.to_string(),
                phoebis_routing::DroppedBlocks {
                    blocks: 1,
                    first_message: 1,
                },
            )])
        }),
    }
}

/// The spent rule's move of `request`'s session: the account it landed on and the 5-hour share its decision booked
/// for the rewrite, a ten-millionth per token under `priced_config`.
async fn spent_move(handle: &RouterHandle, request: SelectRequest) -> (String, Option<f64>) {
    let mut events = handle.subscribe_events();
    let (account, rule) = landed_on(handle.select(request).await.unwrap());
    assert_eq!(rule, Some(phoebis_routing::MigrateReason::Spent));
    let record = next_record(&mut events).await;
    let booked = record
        .outcome
        .reservation
        .as_ref()
        .and_then(|booked| booked.rewrite.get(FIVE_HOUR).copied());
    (account, booked)
}

/// Reads `a` spent with no reset to wait for and every other account of `accounts` low.
async fn a_spent(handle: &RouterHandle, accounts: &[&str], at: i64) {
    handle.report(windows("a", 1.0, 0.3, at)).await;
    for account in accounts.iter().filter(|account| **account != "a") {
        handle.report(windows(account, 0.1, 0.1, at)).await;
    }
    handle.samples().await.unwrap();
}

#[tokio::test]
async fn a_return_after_a_failover_prices_the_last_prompt_less_the_thinking_it_leaves_plus_the_thinking_it_returns_to(
) {
    let store = thinking_store(&["a", "b"], &[("a", "b", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s1", "a", 1, 10.0)], &now).await;
    handle.report(windows("a", 0.3, 0.1, AT)).await;
    handle.report(windows("b", 0.3, 0.1, AT)).await;
    handle.samples().await.unwrap();
    handle
        .report(Report::PinInvalidated {
            account_id: "a".into(),
            pin: session_pin(&handle, "s1").await,
            mark: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    let landing = SelectRequest {
        attempted: vec!["a".into()],
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    assert_eq!(
        landed_on(handle.select(landing).await.unwrap()).0,
        "b",
        "the failover lands the session on b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            120_000,
            Some(10_000),
            None,
        ))
        .await;
    let later = AT + 60_000;
    now.store(later, Ordering::SeqCst);
    handle.report(windows("a", 0.1, 0.1, later)).await;
    handle.report(windows("b", 1.0, 0.3, later)).await;
    handle.samples().await.unwrap();
    let (account, booked) =
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, later)).await;
    // 120,000 less b's 10,000 plus a's 40,000.
    assert_eq!(account, "a");
    assert!(close(booked, 0.015), "{booked:?}");
}

#[tokio::test]
async fn a_compacted_move_opens_no_observation_so_its_answer_leaves_its_pair_unlearned() {
    let store = thinking_store(&["a", "b", "c"], &[("a", "c", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b", "c"], AT).await;
    let compacted = SelectRequest {
        context_compacted: true,
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    assert_eq!(spent_move(&handle, compacted).await.0, "b");
    // A drop the compaction made needs no binding, so this answer must not teach the pair kept.
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            30_000,
            None,
            Some(DROPPED_PREFIX),
        ))
        .await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    // The pair holds no fact, and a and c bound make it the model's first drop: 100,000 less 40,000.
    assert_eq!(account, "b");
    assert!(close(booked, 0.006), "{booked:?}");
}

#[tokio::test]
async fn a_probe_of_a_compacted_request_keeps_the_thinking_its_answer_added() {
    let store = thinking_store(&["a", "b"], &[("a", "b", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s1", "a", 1, 10.0)], &now).await;
    let compacted = |request_id: &str, probe: bool| SelectRequest {
        request_id: request_id.into(),
        probe,
        context_compacted: true,
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    assert_eq!(
        landed_on(
            handle
                .select(compacted("r-compacted", false))
                .await
                .unwrap()
        ),
        ("a".into(), None)
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            60_000,
            Some(20_000),
            None,
        ))
        .await;
    // The keepalive replays the compacted request, its header among the rest.
    assert_eq!(
        landed_on(handle.select(compacted("r-probe", true)).await.unwrap()),
        ("a".into(), None)
    );
    a_spent(&handle, &["a", "b"], AT).await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT)).await;
    // 60,000 less the 20,000 the compacted request's answer produced on a.
    assert_eq!(account, "b");
    assert!(close(booked, 0.004), "{booked:?}");
}

#[tokio::test]
async fn a_compacted_request_after_a_landing_without_usage_teaches_the_pair_nothing() {
    let store = thinking_store(&["a", "b", "c"], &[("a", "c", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b", "c"], AT).await;
    let landing = turn_of(&handle, "s1", "main", SONNET, AT);
    let landing_id = landing.request_id.clone();
    assert_eq!(spent_move(&handle, landing).await.0, "b");
    // The landing's answer brought its head and no usage, so the session's next request renews the landed row.
    handle
        .report(Report::Head {
            request_id: landing_id,
            attempt: 1,
            at: AT,
        })
        .await;
    let compacted = SelectRequest {
        request_id: "r-compacted".into(),
        context_compacted: true,
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    assert_eq!(
        landed_on(handle.select(compacted).await.unwrap()),
        ("b".into(), None)
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            30_000,
            None,
            Some(DROPPED_PREFIX),
        ))
        .await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    // a and b hold no fact, and a and c bound make it the model's first drop: 100,000 less 40,000.
    assert_eq!(account, "b");
    assert!(close(booked, 0.006), "{booked:?}");
}

#[tokio::test]
async fn a_placement_that_releases_the_pin_off_a_cooled_account_hands_its_sums_on() {
    let store = thinking_store(&["a", "b"], &[("a", "b", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s1", "a", 1, 10.0)], &now).await;
    handle.report(windows("a", 0.3, 0.1, AT)).await;
    handle.report(windows("b", 0.3, 0.1, AT)).await;
    handle.samples().await.unwrap();
    // Another session's answer cools a, so s1's next request releases its pin there and lands on b.
    handle
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: AT + 30_000,
                set_at: AT,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    assert_eq!(
        landed_on(
            handle
                .select(turn_of(&handle, "s1", "main", SONNET, AT))
                .await
                .unwrap()
        )
        .0,
        "b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            120_000,
            Some(10_000),
            None,
        ))
        .await;
    let later = AT + 60_000;
    now.store(later, Ordering::SeqCst);
    handle.report(windows("a", 0.1, 0.1, later)).await;
    handle.report(windows("b", 1.0, 0.3, later)).await;
    handle.samples().await.unwrap();
    let (account, booked) =
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, later)).await;
    // 120,000 less b's 10,000 plus a's 40,000.
    assert_eq!(account, "a");
    assert!(close(booked, 0.015), "{booked:?}");
}

#[tokio::test]
async fn a_start_drops_a_loaded_pins_sum_of_an_account_the_registry_no_longer_holds() {
    let store = thinking_store(&["a", "b", "c"], &[]);
    let sum = |tokens| ThinkingSum {
        incarnation: 1,
        tokens,
    };
    store
        .upsert_pin(&PinRow {
            thinking_sums: std::collections::BTreeMap::from([
                ("a".to_string(), sum(40_000)),
                ("c".to_string(), sum(5_000)),
            ]),
            ..stored_pin(&store, "s1", "a", 1, 10.0)
        })
        .unwrap();
    store.delete_account("c", "test").unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    let pin = handle.pins().await.unwrap().pop().expect("s1's pin");
    assert_eq!(
        pin.thinking_sums,
        std::collections::BTreeMap::from([("a".to_string(), sum(40_000))])
    );
}

#[tokio::test]
async fn a_later_observation_replaces_the_fact_its_pair_held() {
    let store = thinking_store(&["a", "b"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
        stored_pin(&store, "s3", "a", 3, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b"], AT).await;
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            100_000,
            None,
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s2").await,
            100_000,
            None,
            Some(DROPPED_PREFIX),
        ))
        .await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s3", "main", SONNET, AT)).await;
    // s1's answer taught a and b bound and s2's taught them kept, so the move prices the whole 100,000.
    assert_eq!(account, "b");
    assert!(close(booked, 0.01), "{booked:?}");
}

#[tokio::test]
async fn an_observation_opened_under_the_spelling_settles_under_the_dated_id() {
    const DATED: &str = "claude-sonnet-5-20260929";
    let store = thinking_store(&["a", "b"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b"], AT).await;
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    handle.resolve(SONNET, DATED, AT).await.unwrap();
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            100_000,
            None,
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    // s1's answer taught a and b bound under the dated id, so the move prices 100,000 less a's 40,000.
    assert_eq!(account, "b");
    assert!(close(booked, 0.006), "{booked:?}");
}

/// The 5-hour share `s2`'s move from `a` to `b` books once `s1`'s move over the same pair was answered with a prompt of
/// `prompt` tokens and `dropped` as its member's one key.
async fn second_move_after(prompt: u64, dropped: Option<&str>) -> Option<f64> {
    let store = thinking_store(&["a", "b"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b"], AT).await;
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            prompt,
            None,
            dropped,
        ))
        .await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    assert_eq!(account, "b");
    booked
}

#[tokio::test]
async fn the_first_answer_after_a_move_teaches_its_pair_bound_by_an_account_reason_and_kept_by_the_prefix_entry_alone(
) {
    // Each prompt points the other way, so a read of the size in place of the member fails both halves.
    let taught = second_move_after(130_000, Some(DROPPED_FOR_ACCOUNT)).await;
    assert!(close(taught, 0.006), "100,000 less a's 40,000: {taught:?}");
    let kept = second_move_after(70_000, Some(DROPPED_PREFIX)).await;
    assert!(close(kept, 0.01), "the whole 100,000: {kept:?}");
}

#[tokio::test]
async fn an_answer_without_the_member_teaches_its_pair_bound_where_its_prompt_shrank_and_nothing_where_it_grew(
) {
    let shrank = second_move_after(70_000, None).await;
    assert!(close(shrank, 0.006), "100,000 less a's 40,000: {shrank:?}");

    // The prompt grows: the next move over the pair prices the whole prompt, and once a and c read bound, a pair the
    // grown answer left unlearned takes the model's first drop, where a kept fact would hold the whole prompt.
    let store = thinking_store(&["a", "b", "c"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
        stored_pin(&store, "s3", "a", 3, 10.0),
        stored_pin(&store, "s4", "a", 4, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b", "c"], AT).await;
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            130_000,
            None,
            None,
        ))
        .await;
    let grew = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    assert_eq!(grew.0, "b");
    assert!(close(grew.1, 0.01), "the whole 100,000: {grew:?}");
    let to_c = SelectRequest {
        attempted: vec!["b".into()],
        ..turn_of(&handle, "s3", "main", SONNET, AT)
    };
    assert_eq!(spent_move(&handle, to_c).await.0, "c");
    handle
        .report(answered(
            session_pin(&handle, "s3").await,
            100_000,
            None,
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s4", "main", SONNET, AT)).await;
    assert_eq!(account, "b");
    assert!(close(booked, 0.006), "100,000 less a's 40,000: {booked:?}");
}

#[tokio::test]
async fn a_pair_reads_its_own_fact_else_its_models_first_drop_and_never_another_models() {
    let store = thinking_store(&["a", "b", "c", "d"], &[("a", "c", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let opus = "claude-opus-5";
    let pins = [
        stored_pin(&store, "s0", "a", 4, 10.0),
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
        PinRow {
            model: opus.into(),
            lane: lane_key(opus, "standard", "none", "-"),
            ..stored_pin(&store, "s3", "a", 3, 10.0)
        },
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    a_spent(&handle, &["a", "b", "c", "d"], AT).await;
    // s0's answer on b lists the client's prefix entry alone, which teaches a and b kept beside a and c bound.
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s0", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s0").await,
            70_000,
            None,
            Some(DROPPED_PREFIX),
        ))
        .await;
    let kept = spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT)).await;
    assert_eq!(kept.0, "b");
    assert!(
        close(kept.1, 0.01),
        "a and b kept: the whole 100,000: {kept:?}"
    );
    let to_d = |session: &str, model: &str| SelectRequest {
        attempted: vec!["b".into(), "c".into()],
        ..turn_of(&handle, session, "main", model, AT)
    };
    let unobserved = spent_move(&handle, to_d("s2", SONNET)).await;
    assert_eq!(unobserved.0, "d");
    assert!(
        close(unobserved.1, 0.006),
        "a and c bound make a and d the model's first drop: 100,000 less 40,000: {unobserved:?}"
    );
    let other_model = spent_move(&handle, to_d("s3", opus)).await;
    assert_eq!(other_model.0, "d");
    assert!(
        close(other_model.1, 0.01),
        "Opus has no fact: the whole 100,000: {other_model:?}"
    );
}

#[tokio::test]
async fn a_second_task_prices_a_moves_drop_from_the_sums_and_the_fact_the_first_one_stored() {
    let store = thinking_store(&["a", "b"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "a", 1, 10.0),
        stored_pin(&store, "s2", "a", 2, 10.0),
    ];
    let first = thinking_router(&store, &pins, &now).await;
    a_spent(&first, &["a", "b"], AT).await;
    assert_eq!(
        spent_move(&first, turn_of(&first, "s2", "main", SONNET, AT))
            .await
            .0,
        "b"
    );
    first
        .report(answered(
            session_pin(&first, "s2").await,
            130_000,
            None,
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    first.shutdown().await.unwrap();
    let second = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    a_spent(&second, &["a", "b"], AT).await;
    let (account, booked) = spent_move(&second, turn_of(&second, "s1", "main", SONNET, AT)).await;
    assert_eq!(account, "b");
    assert!(close(booked, 0.006), "100,000 less a's 40,000: {booked:?}");
}

#[tokio::test]
async fn a_pair_learned_by_a_move_from_the_higher_id_to_the_lower_stands_through_the_next_purge() {
    // a deleted and created again under its id holds the id's second incarnation and b its first, and the move runs
    // from b to a, so the pair names its holders in the reverse of the move's order.
    let store = thinking_store(&["a", "b"], &[]);
    store.delete_account("a", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    with_tier(&store, "a");
    store
        .set_pool_members(
            "p",
            &[
                MemberWrite {
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
                MemberWrite {
                    account_id: "b".into(),
                    priority: 1,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
            ],
            "test",
        )
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let pins = [
        stored_pin(&store, "s1", "b", 1, 10.0),
        stored_pin(&store, "s2", "b", 2, 10.0),
    ];
    let handle = thinking_router(&store, &pins, &now).await;
    handle.report(windows("b", 1.0, 0.3, AT)).await;
    let Report::Sample {
        account_id,
        sample,
        source_request_id,
        ..
    } = windows("a", 0.1, 0.1, AT)
    else {
        unreachable!("a usage sample")
    };
    handle
        .report(Report::Sample {
            account_id,
            sample,
            incarnation: 2,
            source_request_id,
        })
        .await;
    handle.samples().await.unwrap();
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT))
            .await
            .0,
        "a"
    );
    // A purge between a move and its answer keeps the observation while both of its holders stand.
    handle.reload().await.unwrap();
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            100_000,
            None,
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    // A reload of an unchanged registry purges under the holders the move ran on.
    handle.reload().await.unwrap();
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    assert_eq!(account, "a");
    assert!(close(booked, 0.006), "100,000 less b's 40,000: {booked:?}");
}

#[tokio::test]
async fn an_answer_behind_the_swap_that_gave_the_left_id_a_new_holder_teaches_the_new_holder_nothing(
) {
    // b first in the pool's order, so a fresh session lands on b's new holder once the old one is gone.
    let store = thinking_store(&["b", "a"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s1", "b", 1, 10.0)], &now).await;
    handle.report(windows("b", 1.0, 0.3, AT)).await;
    handle.report(windows("a", 0.1, 0.1, AT)).await;
    handle.samples().await.unwrap();
    assert_eq!(
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT))
            .await
            .0,
        "a"
    );
    // b deleted and created again while s1's move waits for its answer.
    store.delete_account("b", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("b", AccountKind::Oauth))
        .unwrap();
    with_tier(&store, "b");
    store
        .set_pool_members(
            "p",
            &[
                MemberWrite {
                    account_id: "b".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
                MemberWrite {
                    account_id: "a".into(),
                    priority: 1,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
            ],
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            100_000,
            None,
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    let first_turn = SelectRequest {
        request_id: "r-s2-first".into(),
        ..turn_of(&handle, "s2", "main", SONNET, AT)
    };
    assert_eq!(landed_on(handle.select(first_turn).await.unwrap()).0, "b");
    handle
        .report(answered(
            session_pin(&handle, "s2").await,
            100_000,
            Some(40_000),
            None,
        ))
        .await;
    let Report::Sample {
        account_id,
        sample,
        source_request_id,
        ..
    } = windows("b", 1.0, 0.3, AT)
    else {
        unreachable!("a usage sample")
    };
    handle
        .report(Report::Sample {
            account_id,
            sample,
            incarnation: 2,
            source_request_id,
        })
        .await;
    handle.samples().await.unwrap();
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    assert_eq!(account, "a");
    assert!(
        close(booked, 0.01),
        "b's new holder has no pair with a: the whole 100,000: {booked:?}"
    );
}

#[tokio::test]
async fn a_move_onto_an_id_deleted_and_created_again_prices_none_of_the_thinking_its_previous_holder_produced(
) {
    // s1 holds 40,000 tokens of thinking from b's first holder and answers once on a. The bound pair of a and c makes
    // the pair of a and b read bound, and c spent leaves b's new holder the move's one destination.
    let store = thinking_store(&["a", "b", "c"], &[("a", "c", true)]);
    store
        .upsert_pin(&PinRow {
            thinking_sums: std::collections::BTreeMap::from([(
                "b".to_string(),
                ThinkingSum {
                    incarnation: 1,
                    tokens: 40_000,
                },
            )]),
            ..stored_pin(&store, "s1", "a", 1, 10.0)
        })
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = spawn(store.clone(), clock(&now), priced_config()).unwrap();
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            100_000,
            Some(20_000),
            None,
        ))
        .await;
    store.delete_account("b", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("b", AccountKind::Oauth))
        .unwrap();
    with_tier(&store, "b");
    store
        .set_pool_members(
            "p",
            &["a", "b", "c"]
                .iter()
                .enumerate()
                .map(|(priority, id)| MemberWrite {
                    account_id: id.to_string(),
                    priority: priority as u32,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                })
                .collect::<Vec<_>>(),
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    handle.report(windows("a", 1.0, 0.3, AT)).await;
    handle.report(windows("c", 1.0, 0.3, AT)).await;
    let Report::Sample {
        account_id,
        sample,
        source_request_id,
        ..
    } = windows("b", 0.1, 0.1, AT)
    else {
        unreachable!("a usage sample")
    };
    handle
        .report(Report::Sample {
            account_id,
            sample,
            incarnation: 2,
            source_request_id,
        })
        .await;
    handle.samples().await.unwrap();
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, AT)).await;
    assert_eq!(account, "b");
    assert!(
        close(booked, 0.008),
        "100,000 less a's 20,000, and nothing for b's first holder: {booked:?}"
    );
}

#[tokio::test]
async fn a_failovers_landing_hands_on_no_thinking_of_a_holder_that_left_while_its_row_waited() {
    // s1 holds 40,000 tokens of a's thinking when a failover takes its pin off a, and a is deleted and created again
    // before the landing, so no purge reads the waiting row. The bound pair of b and c makes the pair of a and b read
    // bound, and c spent keeps both selections off c.
    let store = thinking_store(&["a", "b", "c"], &[("b", "c", true)]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s1", "a", 1, 10.0)], &now).await;
    handle.report(windows("a", 0.3, 0.1, AT)).await;
    handle.report(windows("b", 0.3, 0.1, AT)).await;
    handle.report(windows("c", 1.0, 0.3, AT)).await;
    handle.samples().await.unwrap();
    handle
        .report(Report::PinInvalidated {
            account_id: "a".into(),
            pin: session_pin(&handle, "s1").await,
            mark: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    store.delete_account("a", "test").unwrap();
    store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    with_tier(&store, "a");
    store
        .set_pool_members(
            "p",
            &["a", "b", "c"]
                .iter()
                .enumerate()
                .map(|(priority, id)| MemberWrite {
                    account_id: id.to_string(),
                    priority: priority as u32,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                })
                .collect::<Vec<_>>(),
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    let landing = SelectRequest {
        attempted: vec!["a".into()],
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    assert_eq!(
        landed_on(handle.select(landing).await.unwrap()).0,
        "b",
        "the failover lands the session on b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            120_000,
            Some(10_000),
            None,
        ))
        .await;
    let later = AT + 60_000;
    now.store(later, Ordering::SeqCst);
    let Report::Sample {
        account_id,
        sample,
        source_request_id,
        ..
    } = windows("a", 0.1, 0.1, later)
    else {
        unreachable!("a usage sample")
    };
    handle
        .report(Report::Sample {
            account_id,
            sample,
            incarnation: 2,
            source_request_id,
        })
        .await;
    handle.report(windows("b", 1.0, 0.3, later)).await;
    handle.samples().await.unwrap();
    let (account, booked) =
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, later)).await;
    assert_eq!(account, "a");
    assert!(
        close(booked, 0.011),
        "120,000 less b's 10,000, and nothing for a's first holder: {booked:?}"
    );
}

#[tokio::test]
async fn a_failovers_landing_teaches_no_pair_under_a_holder_that_left_while_its_row_waited() {
    // s1 holds 40,000 tokens of a's thinking when a failover takes its pin off a, and a leaves the registry before
    // the landing on b. The landing's answer lists a block dropped for the account, so an observation under a's holder
    // would teach the model its one bound pair, which the move from b to c reads through the model's first drop.
    let store = thinking_store(&["a", "b", "c"], &[]);
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s1", "a", 1, 10.0)], &now).await;
    handle.report(windows("a", 0.3, 0.1, AT)).await;
    handle.report(windows("b", 0.3, 0.1, AT)).await;
    handle.report(windows("c", 0.3, 0.1, AT)).await;
    handle.samples().await.unwrap();
    handle
        .report(Report::PinInvalidated {
            account_id: "a".into(),
            pin: session_pin(&handle, "s1").await,
            mark: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    store.delete_account("a", "test").unwrap();
    handle.reload().await.unwrap();
    let landing = SelectRequest {
        attempted: vec!["a".into()],
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    assert_eq!(
        landed_on(handle.select(landing).await.unwrap()).0,
        "b",
        "the failover lands the session on b"
    );
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            120_000,
            Some(10_000),
            Some(DROPPED_FOR_ACCOUNT),
        ))
        .await;
    let later = AT + 60_000;
    now.store(later, Ordering::SeqCst);
    handle.report(windows("b", 1.0, 0.3, later)).await;
    handle.samples().await.unwrap();
    let (account, booked) =
        spent_move(&handle, turn_of(&handle, "s1", "main", SONNET, later)).await;
    assert_eq!(account, "c");
    assert!(
        close(booked, 0.012),
        "the model holds no pair, so the whole 120,000: {booked:?}"
    );
}

/// Where `s1`'s return lands when its request already tried `attempted`, and the 5-hour share `s2`'s move from `a` to
/// `b` books behind it. `s1` returns under its lapsed agent's record of a 100,000-token prompt on `a` with 40,000
/// tokens of a's thinking, and the return's first answer reads a prompt of 70,000.
async fn a_move_behind_a_return(attempted: &[&str]) -> (String, Option<f64>) {
    let store = thinking_store(&["a", "b"], &[]);
    // Written ahead of the router's start, which loads it.
    let record = LapsedAgentRow {
        key_id: "k".into(),
        session_id: "s1".into(),
        agent_id: "main".into(),
        model: SONNET.into(),
        key_generation: store.api_key("k").unwrap().unwrap().created_at,
        account_id: "a".into(),
        incarnation: 1,
        last_used_at: AT - HOUR,
        last_input_tokens: 100_000,
        burn: PinBurn::default(),
        burn_banded: PinBurn::default(),
        burn_fast: PinBurn::default(),
        burn_unmetered: PinBurn::default(),
        burn_unmetered_model: None,
        speed: 1.0,
        thinking_sums: std::collections::BTreeMap::from([(
            "a".to_string(),
            ThinkingSum {
                incarnation: 1,
                tokens: 40_000,
            },
        )]),
        account_bound: false,
    };
    store
        .write_routing(|tx| tx.upsert_lapsed_agent(&record))
        .unwrap();
    let now = Arc::new(AtomicI64::new(AT));
    let handle = thinking_router(&store, &[stored_pin(&store, "s2", "a", 2, 10.0)], &now).await;
    let returned = SelectRequest {
        attempted: attempted
            .iter()
            .map(|account| account.to_string())
            .collect(),
        ..turn_of(&handle, "s1", "main", SONNET, AT)
    };
    let landed = landed_on(handle.select(returned).await.unwrap()).0;
    handle
        .report(answered(
            session_pin(&handle, "s1").await,
            70_000,
            None,
            None,
        ))
        .await;
    a_spent(&handle, &["a", "b"], AT).await;
    let (account, booked) = spent_move(&handle, turn_of(&handle, "s2", "main", SONNET, AT)).await;
    assert_eq!(account, "b");
    (landed, booked)
}

#[tokio::test]
async fn a_return_onto_another_account_opens_an_observation_and_one_onto_the_same_opens_none() {
    let (landed, booked) = a_move_behind_a_return(&["a"]).await;
    assert_eq!(landed, "b");
    assert!(
        close(booked, 0.006),
        "the return onto b taught a and b bound: 100,000 less a's 40,000: {booked:?}"
    );
    let (landed, booked) = a_move_behind_a_return(&[]).await;
    assert_eq!(landed, "a");
    assert!(
        close(booked, 0.01),
        "the return onto a taught the pair nothing: the whole 100,000: {booked:?}"
    );
}

#[tokio::test]
async fn a_lapsed_agents_record_reaches_the_store_at_the_purge_and_leaves_it_at_the_return_or_past_seven_days(
) {
    let store = seeded(&["a"]);
    let start = 8 * 24 * HOUR;
    let key_generation = store.api_key("k").unwrap().unwrap().created_at;
    let record = |session: &str, last_used_at: i64| LapsedAgentRow {
        key_id: "k".into(),
        session_id: session.into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        key_generation,
        account_id: "a".into(),
        incarnation: 1,
        last_used_at,
        last_input_tokens: 0,
        burn: PinBurn::default(),
        burn_banded: PinBurn::default(),
        burn_fast: PinBurn::default(),
        burn_unmetered: PinBurn::default(),
        burn_unmetered_model: None,
        speed: 1.0,
        thinking_sums: Default::default(),
        account_bound: false,
    };
    // Ahead of the start: a record last used a minute more than 7 days back, one last used an hour back, and a pin
    // under a key the registry does not hold. The start's purge deletes that pin as unserved, which writes no record,
    // in the batch that ends the aged record, so the pin's delete says that batch landed.
    let unserved = PinRow {
        key_id: "gone".into(),
        ..stored_pin(&store, "s9", "a", 9, 10.0)
    };
    store
        .write_routing(|tx| {
            tx.upsert_lapsed_agent(&record("aged", start - 7 * 24 * HOUR - 60_000))?;
            tx.upsert_lapsed_agent(&record("kept", start - HOUR))?;
            tx.upsert_pin(&unserved)
        })
        .unwrap();
    let now = Arc::new(AtomicI64::new(start));
    let handle = spawn(
        store.clone(),
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let records = || -> Vec<String> {
        store
            .load_routing()
            .unwrap()
            .lapsed_agents
            .into_iter()
            .map(|record| record.session_id)
            .collect()
    };
    let pinned = |session: &str| {
        store
            .load_routing()
            .unwrap()
            .pins
            .iter()
            .any(|pin| pin.session_id == session)
    };
    wait_for(WRITTEN, || (!pinned("s9")).then_some(())).await;
    assert_eq!(
        records(),
        ["kept"],
        "the start's purge ends the record past its 7 days in the store"
    );
    handle
        .select(request(&handle, Some("s1"), start))
        .await
        .unwrap();
    wait_for(WRITTEN, || pinned("s1").then_some(())).await;
    // One millisecond past the pool's hour-long affinity TTL, so the purge takes s1's pin as idle, and the pin's delete
    // rides the batch that holds its record.
    now.store(start + HOUR + 1, Ordering::SeqCst);
    wait_for(WRITTEN, || (!pinned("s1")).then_some(())).await;
    assert_eq!(
        records(),
        ["kept", "s1"],
        "the purge that takes s1's pin stores its record"
    );
    // The return's pin rides the batch that takes its record.
    handle
        .select(request(&handle, Some("s1"), start + HOUR + 1))
        .await
        .unwrap();
    wait_for(WRITTEN, || pinned("s1").then_some(())).await;
    assert_eq!(
        records(),
        ["kept"],
        "the return takes s1's record off the store, and the record inside its 7 days stands"
    );
}

#[tokio::test]
async fn a_rounds_record_states_the_rewrite_of_a_pin_whose_pair_reads_bound_at_the_smaller_size() {
    let store = averaged(thinking_store(&["a", "b"], &[("a", "b", true)]));
    // The record's figure reads the pricer's Pro-week equivalent: a ten-millionth per cache-write token.
    let config = RouterConfig {
        share: priced_config().share.with_equivalent(|_, _, _, usage, _| {
            Some((usage.write_5m + usage.write_1h) as f64 / 10_000_000.0)
        }),
        ..priced_config()
    };
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), config).unwrap();
    // As in the round case above: s1 burns four times what s2 burns, so the round marks s1 for b.
    let s1 = selected_pin(
        handle
            .select(request(&handle, Some("s1"), 1_000))
            .await
            .unwrap(),
    );
    let s2 = selected_pin(
        handle
            .select(request(&handle, Some("s2"), 1_000))
            .await
            .unwrap(),
    );
    handle
        .report(Report::PinRenewed {
            pin: s1,
            now: 2_000,
            usage: Some(PinUsage {
                input: 4_000,
                ..PinUsage::default()
            }),
            keepalive: false,
            cache_ttl_ms: 300_000,
            fast: false,
            turn_end: RenewalEnd::Turn,
            thinking_tokens: Some(1_000),
            input_transformations: None,
        })
        .await;
    handle
        .report(renewal(
            s2,
            2_000,
            PinUsage {
                input: 1_000,
                ..PinUsage::default()
            },
            false,
            300_000,
        ))
        .await;
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.9, None)))
        .await;
    handle.report(windows("b", 0.1, 0.1, 2_000)).await;
    handle.samples().await.unwrap();
    let mut events = handle.subscribe_events();
    let at = 1_000 + 600_000;
    now.store(at, Ordering::SeqCst);
    past_the_wait(&handle, &store, "a", at).await;
    handle
        .select(request(&handle, Some("s1"), at))
        .await
        .unwrap();
    let round = round_among(&drained(&mut events).await);
    let s1 = round
        .pins
        .iter()
        .find(|pin| pin.session_id == "s1")
        .expect("s1 in the round");
    assert_eq!(s1.planned.as_deref(), Some("b"));
    // 4,000 less a's 1,000.
    assert!(close(s1.rewrite, 0.000_3), "{:?}", s1.rewrite);
}
