use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fake_upstream::{stays_for, wait_for, wait_for_async};
use phoebis_oauth::{Clock, ExtraUsage, UsageSnapshot, UsageWindow};
use phoebis_routing::{
    credits_from_usage, lane_key, resolve, sample_from_usage, spawn, AffinityReport, BandWeight,
    BurnRates, ClientClass, CreditsState, Exhaustion, KeyRefusal, MemberSpend, PinRef,
    PinUsage, Presented, RateTable, Rates, Refusal, ReloadError, Report, ResolveError,
    RouterConfig, RouterHandle, RoutingState, SelectRequest, SelectResult, SharePricer,
    UtilizationSample, WindowReading, WindowSource, WriterStatus, COMMANDS, FAST_CREDITS_REASON,
    FAST_RATE_LIMIT_REASON, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE, WRITER_DEPTH,
};
use phoebis_store::{
    key_prefix, AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow,
    MemberWrite, ModelRule, PinBurn, PoolRow, RoutingLoad, RulesMode, StorageKey, Store, Strategy,
    SubscriptionRow, WindowRow,
};

const HOUR: i64 = 3_600_000;

fn seeded(ids: &[&str]) -> Arc<Store> {
    seeded_kind(AccountKind::ApiKey, ids)
}

/// The same pool over OAuth accounts, which are the only ones a usage sample is ever pulled for.
fn seeded_oauth(ids: &[&str]) -> Arc<Store> {
    seeded_kind(AccountKind::Oauth, ids)
}

fn seeded_kind(kind: AccountKind, ids: &[&str]) -> Arc<Store> {
    seed(
        Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap()),
        kind,
        ids,
    )
}

/// The pool `p` over `ids` and the key `k`, written into a store the caller opened.
fn seed(store: Arc<Store>, kind: AccountKind, ids: &[&str]) -> Arc<Store> {
    for id in ids {
        store
            .insert_account(&AccountRow {
                id: (*id).into(),
                kind,
                label: (*id).into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
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
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-secret").to_vec(),
                prefix: "secret00".into(),
                created_at: 0,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
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
        session_id: session.map(String::from),
        canonical_model: "claude-sonnet-5".into(),
        folded_model: "claude-sonnet-5".into(),
        candidates: resolve(
            &s,
            s.key("k").unwrap(),
            "claude-sonnet-5",
            "claude-sonnet-5",
        )
        .unwrap(),
        portable: true,
        attempted: vec![],
        now,
        fallback_ms: 60_000,
        revision: s.revision,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        static_hash: None,
        boundaries: Arc::from([]),
        client: ClientClass::ClaudeCode,
        static_boundaries: Arc::from([]),
        marker: false,
        parent_agent_id: None,
        holds: Vec::new(),
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        fast: false,
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
        root: None,
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
        .insert_account(&AccountRow {
            id: "b".into(),
            kind: AccountKind::ApiKey,
            label: "b".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
        handle
            .snapshot()
            .authenticate("phoebis-secret", 1_002)
            .is_err(),
        "a revoked key answers 401 at once"
    );
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
        handle
            .snapshot()
            .authenticate("phoebis-secret", 1_004)
            .is_err(),
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
        Arc::new(Store::open(&path, &key).unwrap()),
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
    let reopened = wait_for(DRAINED, || Store::open(&path, &key).ok()).await;
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
                scope: "account".into(),
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
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
            .filter(|c| c.account_id == "a" && c.scope == "model:claude-sonnet-5")
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
    assert_eq!(store.bump_lifecycle("a").unwrap(), 2);
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
    };
    let reports = [
        Report::AccountState {
            account_id: "a".into(),
            state: AccountState::ReauthRequired,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        },
        Report::Accounting { degraded: true },
        Report::Affinity(affinity_report(&handle, "a", 1_000)),
        model_cooldown("a", "model:claude-sonnet-5", 1_000),
        credits_at(true, None, None, 1_000),
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
        renewal(pin, 1_000, PinUsage::default(), "client", 0),
        observed("a", sample(1_000)),
        a_million_input("a", 1_000),
    ];
    for report in &reports {
        // No wildcard, so a new variant fails to compile here until it is named; it also needs a report above and a
        // name in `KINDS`.
        match report {
            Report::AccountState { .. }
            | Report::Accounting { .. }
            | Report::Affinity(_)
            | Report::Cooldown { .. }
            | Report::Credits { .. }
            | Report::MemberSpend(_)
            | Report::Overage { .. }
            | Report::PinInvalidated { .. }
            | Report::PinReleased { .. }
            | Report::PinRenewed { .. }
            | Report::Sample { .. }
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
    let mut picks = std::collections::HashSet::new();
    for _ in 0..5 {
        let session = format!("s{}", rand::random::<u32>());
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
        picks.insert(x);
    }
    let _ = picks;
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
                    scope: "account".into(),
                    until: 1_000 + 2 * HOUR,
                    set_at: 0,
                    reason: "rate_limit".into(),
                    window: "five_hour".into(),
                    source_request_id: None,
                    incarnation: 1,
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
                .find(|c| c.scope == "account")
        })
        .await;
    }
    store
        .upsert_cooldown(&CooldownRow {
            account_id: "a".into(),
            scope: "model:claude-opus-5".into(),
            until: 500,
            set_at: 0,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
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
    assert!(handle.clear_cooldown("a", "account").await.unwrap());
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
            session_id: Some("s1".into()),
            canonical_model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            candidates: resolve(
                &s,
                s.key("k").unwrap(),
                "claude-sonnet-4-5",
                "claude-sonnet-4-5",
            )
            .unwrap(),
            portable: true,
            attempted: vec![],
            now,
            fallback_ms: 60_000,
            revision: s.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            holds: Vec::new(),
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            fast: false,
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
                scope: "model:claude-sonnet-4-5".into(),
                until: 2_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
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
            .find(|c| c.scope == "model:claude-sonnet-4-5-20250929")
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
            scope: scope.into(),
            until: now + HOUR,
            set_at: now,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
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
            session_id: Some("s1".into()),
            canonical_model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            candidates: resolve(
                &s,
                s.key("k").unwrap(),
                "claude-sonnet-4-5",
                "claude-sonnet-4-5",
            )
            .unwrap(),
            portable: true,
            attempted: vec![],
            now: 1_000,
            fallback_ms: 60_000,
            revision: s.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            holds: Vec::new(),
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            fast: false,
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
            .map(|c| c.scope.as_str())
            .collect::<Vec<_>>(),
        ["model:claude-sonnet-4-5-20250929"]
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
            .any(|c| c.account_id == "b" && c.scope == scope)
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
        load.cooldowns
            .iter()
            .any(|c| c.account_id == "a" && c.scope == "model:claude-sonnet-4-5-20250929"),
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
                    .any(|c| c.account_id == "b" && &c.scope == scope)
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
) -> (RouterHandle, phoebis_store::RoutingWriteHold) {
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
                "client",
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
        "the placement waits in the backlog"
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
        "the last filler and the three placements wait in the backlog"
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
async fn a_refused_rekey_behind_a_hold(
) -> (Arc<Store>, RouterHandle, phoebis_store::RoutingWriteHold) {
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
    store.set_enabled(&sel.account.id, false).unwrap();
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
    store.bump_lifecycle(&other.account.id).unwrap();
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
        handle.release_session("k", "s1").await.unwrap(),
        1,
        "the pin stays on the disconnected account until it is released or expires"
    );
    store.set_enabled(&sel.account.id, true).unwrap();
    store.delete_account(&other.account.id, "test").unwrap();
    handle.reload().await.unwrap();
    let fourth = handle
        .select(request(&handle, Some("s2"), 1_003))
        .await
        .unwrap();
    assert!(
        matches!(fourth.result, SelectResult::Selected(ref s) if s.account.id == sel.account.id)
    );
    assert_eq!(handle.release_session("k", "s2").await.unwrap(), 1);
    assert!(handle.pins().await.unwrap().is_empty());
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
            disabled_reason: None,
        }),
        raw_keys: vec![],
        error: None,
    };
    let (sample, overage) = sample_from_usage(&snapshot(100.0, 40.0, true), 1);
    assert_eq!(
        (
            sample.reading(FIVE_HOUR).map(|r| r.utilization),
            sample.reading(SEVEN_DAY).map(|r| r.utilization),
            sample.observed_at,
        ),
        (Some(1.0), Some(0.4), 5_000)
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
                scope: "account".into(),
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "permission".into(),
                window: "none".into(),
                source_request_id: None,
                incarnation: 1,
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
                scope: "account".into(),
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "permission".into(),
                window: "none".into(),
                source_request_id: None,
                incarnation: 1,
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
                scope: "account".into(),
                until: 1_000 + 48 * HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "seven_day".into(),
                source_request_id: None,
                incarnation: 1,
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-reissued").to_vec(),
                prefix: key_prefix("phoebis-reissued"),
                created_at: 0,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
            },
            "test",
        )
        .unwrap();
}

fn cooldown_row(account: &str) -> CooldownRow {
    CooldownRow {
        account_id: account.into(),
        scope: "account".into(),
        until: 1_000 + 48 * HOUR,
        set_at: 0,
        reason: "rate_limit".into(),
        window: "seven_day".into(),
        source_request_id: None,
        incarnation: 1,
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

    // A key delete and a key create under the same id, and an account delete and recreate under the same id,
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
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { session_id, reasons, .. } if session_id == "s" && reasons == vec!["deleted"])
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
            sample: sample_with(1_500, FIVE_HOUR, 1.0, None),
            incarnation: 1,
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
        reset,
        utilization,
        window: window.into(),
    }
}

/// Prices a million input tokens at a tenth of either window, whatever the tier, the model or the instant.
fn a_tenth_per_million() -> SharePricer {
    SharePricer::new(|_, _, _, usage, _| Some(usage.input as f64 / 1_000_000.0 * 0.1))
}

fn with_tier(store: &Store, account: &str) {
    store
        .set_profile(
            account,
            "{}",
            &SubscriptionRow {
                tier: Some("default_claude_max_5x".into()),
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
async fn a_completed_requests_share_is_added_to_the_reading_until_the_next_reading_drops_it() {
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
    handle
        .report(observed(
            "a",
            sample_with(3_000, FIVE_HOUR, 0.55, Some(10 * HOUR)),
        ))
        .await;
    assert!(
        close(read(&handle, "a", FIVE_HOUR).await, 0.55),
        "a reading observed after the request carries it, so the share is dropped"
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
async fn a_usage_report_charges_each_window_the_pricer_prices_and_the_fable_week_for_a_fable_model()
{
    let store = seeded_oauth(&["a"]);
    with_tier(&store, "a");
    let now = Arc::new(AtomicI64::new(3_000));
    // A tenth of a shared window per million input tokens, and twice that of the Fable week, which is half the
    // 7-day window. A window the pricer cannot price keeps its reading while the others charge: the first
    // unpriced model has no 7-day cell and prices on the 5-hour window alone, the second prices on neither.
    let share = SharePricer::new(|window, _, model, usage, _| {
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
            source: WindowSource::Poll,
        };
        Report::Sample {
            account_id: "a".into(),
            sample: sample(1_000)
                .with(FIVE_HOUR, reading)
                .with(SEVEN_DAY, reading)
                .with(SEVEN_DAY_FABLE, reading),
            incarnation,
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
async fn a_head_writes_the_windows_it_moved_and_a_restart_reads_them() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    let reading = |utilization: f64| WindowReading {
        utilization,
        reset: Some(100 * HOUR),
        observed_at: 1_000,
        source: WindowSource::Head,
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
            sample_with(900, FIVE_HOUR, 0.9, Some(100 * HOUR)),
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

#[tokio::test]
async fn an_api_key_account_and_an_account_without_a_tier_are_charged_nothing() {
    let store = seeded_oauth(&["a", "n"]);
    store
        .insert_account(&AccountRow {
            id: "k".into(),
            kind: AccountKind::ApiKey,
            label: "k".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinCreated { session_id, account_id, at: 1_000, .. } if session_id == "s1" && account_id == a)
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
                scope: "account".into(),
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: Some("req_1".into()),
                incarnation: 1,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownSet { account_id, scope, until, source_request_id: Some(r), .. } if account_id == a && scope == "account" && until == 1_000 + HOUR && r == "req_1")
    );
    let second = handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let b = picked(&second);
    assert_ne!(a, b);
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinMigrated { from_account, to_account, reason, .. } if from_account == a && to_account == b && reason == "cooldown")
    );
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
    // Clearing the cooldown, releasing the session, and the purge each say so, and each says what released
    // the row it took.
    assert!(handle.clear_cooldown(&a, "account").await.unwrap());
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { account_id, cause, .. } if account_id == a && cause == "admin")
    );
    assert_eq!(handle.release_session("k", "s1").await.unwrap(), 1);
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinReleased { session_id, account_id, .. } if session_id == "s1" && account_id == b)
    );
    handle
        .select(request(&handle, Some("s2"), 2_000))
        .await
        .unwrap();
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinCreated { session_id, .. } if session_id == "s2")
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
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { session_id, reasons, .. } if session_id == "s2" && reasons == vec!["expired"]),
        "and the idle pin went with it"
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
                    source: WindowSource::Poll,
                },
            ),
            incarnation: 1,
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
                source: WindowSource::Poll,
            },
        )
    };
    handle
        .report(Report::Sample {
            account_id: a.clone(),
            sample: spent(1.0, 1_500),
            incarnation: 1,
        })
        .await;
    handle
        .report(Report::Sample {
            account_id: b.into(),
            sample: spent(0.2, 1_500),
            incarnation: 1,
        })
        .await;
    let second = handle
        .select(request(&handle, Some("s1"), 2_000))
        .await
        .unwrap();
    assert!(matches!(&second.result, SelectResult::Selected(s) if s.account.id == b));
    let event = next_event(&mut events).await;
    assert!(
        matches!(event, RouterEvent::PinMigrated { from_account, to_account, reason, .. } if from_account == a && to_account == b && reason == "spent")
    );
}

#[tokio::test]
async fn a_pin_on_a_cooled_and_spent_account_with_no_sibling_allowance_expires_with_both_facts() {
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
                source: WindowSource::Poll,
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
        })
        .await;
    handle
        .report(Report::Sample {
            account_id: b.into(),
            sample: spent(1_500),
            incarnation: 1,
        })
        .await;
    let second = handle
        .select(request(&handle, Some("s1"), 2_000))
        .await
        .unwrap();
    assert!(matches!(&second.result, SelectResult::Selected(s) if s.account.id == b));
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { account_id, reasons, .. } if account_id == a && reasons == vec!["cooldown", "spent"]),
        "no sibling holds allowance, so the spent rule names no destination and the cooled pin expires"
    );
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
}

/// A rejection as the relay reports it, with the clock deciding the row's set time.
fn cooled(account: &str, scope: &str, until: i64, reason: &str, window: &str) -> Report {
    Report::Cooldown {
        account_id: account.into(),
        row: CooldownRow {
            account_id: account.into(),
            scope: scope.into(),
            until,
            set_at: 0,
            reason: reason.into(),
            window: window.into(),
            source_request_id: None,
            incarnation: 1,
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

/// `sample(observed_at)` with one window's reading folded in.
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
            source: WindowSource::Poll,
        },
    )
}

fn observed(account: &str, sample: UtilizationSample) -> Report {
    Report::Sample {
        account_id: account.into(),
        sample,
        incarnation: 1,
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
    // Anthropic moved the reset in: hours before the deadline the 429 announced, the window reads below 1.
    now.store(2_000, Ordering::SeqCst);
    handle
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)))
        .await;
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { account_id, scope, cause, at } if account_id == "a" && scope == "account" && cause == "recovered" && at == 2_000)
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
        .report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)))
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
async fn a_sample_the_cooled_account_was_holding_when_it_was_rejected_releases_nothing() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
    handle
        .report(cooled(
            "a",
            "account",
            2_000 + 5 * HOUR,
            "rate_limit",
            "five_hour",
        ))
        .await;
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    // The usage endpoint serves a cached view for a minute, so the snapshot that arrives first can be older
    // than the rejection.
    handle
        .report(observed("a", sample_with(1_999, FIVE_HOUR, 0.4, None)))
        .await;
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a reading from before the row was set is not evidence about it"
    );
    handle
        .report(observed("a", sample_with(2_001, FIVE_HOUR, 0.4, None)))
        .await;
    assert!(
        handle.cooldowns().await.unwrap().is_empty(),
        "the same reading taken after it is"
    );
}

#[tokio::test]
async fn a_window_at_its_limit_keeps_the_cooldown() {
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
    handle
        .report(observed("a", sample_with(2_002, SEVEN_DAY, 0.99, None)))
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
                    source: WindowSource::Poll,
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
                    source: WindowSource::Poll,
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
                        source: WindowSource::Poll,
                    },
                )
                .with(
                    SEVEN_DAY,
                    WindowReading {
                        utilization: 0.0,
                        reset: None,
                        observed_at: 2_000,
                        source: WindowSource::Poll,
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
                    source: WindowSource::Poll,
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

#[tokio::test]
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
        .filter(|answer| matches!(&answer.result, SelectResult::Held { account_id, .. } if account_id == "a"))
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
                scope: "account".into(),
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
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
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinMigrated { reason, .. } if reason == "cooldown"
    ));
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
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { .. }
    ));
    handle
        .report(Report::Cooldown {
            account_id: from.clone(),
            row: CooldownRow {
                account_id: from.clone(),
                scope: "fast".into(),
                until: 1_000 + 600_000,
                set_at: 0,
                reason: FAST_RATE_LIMIT_REASON.into(),
                window: "fast".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: Some(pin),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { scope, .. } if scope == "fast"
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
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinMigrated { from_account, to_account, reason, .. }
            if from_account == from && to_account == to && reason == "fast_lane"
    ));
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
            disabled_reason: reason.map(String::from),
        }),
        raw_keys: vec![],
        error: None,
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
        .filter(|row| row.scope == "fast" || row.scope == "fast_credits")
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
        RouterEvent::CooldownSet { scope, .. } if scope == "fast_credits"
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
                    source: WindowSource::Poll,
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
                    (account_id.as_str(), scope.as_str(), cause),
                    ("a", "fast_credits", "recovered")
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
            .map(|row| (row.scope, row.reason, row.until))
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
    assert!(again.clear_cooldown("a", "fast_credits").await.unwrap());
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
        session_id: Some("s2".into()),
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
                scope: "account".into(),
                until: 1_000 + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
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
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinCreated { agent_id, .. } if agent_id == "worker"
    ));
    let main_next = handle
        .select(request(&handle, Some("s1"), 1_100))
        .await
        .unwrap();
    let SelectResult::Selected(sel) = &main_next.result else {
        panic!("{:?}", main_next.result)
    };
    assert_eq!(sel.trace.moved, Some("cooldown"));
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::PinMigrated { agent_id, reason, .. } if agent_id == "main" && reason == "cooldown"
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
        static_hash: Some("s0".into()),
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
            origin: "keepalive",
            cache_ttl_ms: 300_000,
            fast: false,
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
        five_hour: RateTable::unit(),
        fast: RateTable::unit(),
    }
}

/// Every token of every model at `rate`.
fn flat_rates(rate: f64) -> RateTable {
    let mut table = RateTable::unit();
    table.fallback = Rates {
        input: Some(rate),
        cache_read: Some(rate),
        write_5m: Some(rate),
        write_1h: Some(rate),
        output: Some(rate),
    };
    table
}

fn renewal(
    pin: PinRef,
    now: i64,
    usage: PinUsage,
    origin: &'static str,
    cache_ttl_ms: i64,
) -> Report {
    Report::PinRenewed {
        pin,
        now,
        usage: Some(usage),
        origin,
        cache_ttl_ms,
        fast: false,
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
        .report(renewal(
            probe,
            4_000,
            PinUsage::default(),
            "keepalive",
            300_000,
        ))
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
                scope: "fast".into(),
                until: 1_000 + 600_000,
                set_at: 0,
                reason: FAST_RATE_LIMIT_REASON.into(),
                window: "fast".into(),
                source_request_id: None,
                incarnation: 1,
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
async fn a_renewal_with_usage_folds_the_typed_rates_and_the_banded_copy_and_a_probes_usage_never_counts(
) {
    let store = seeded(&["a"]);
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
        .report(renewal(client.clone(), 2_000, first, "client", 300_000))
        .await;
    handle
        .report(renewal(client, 62_000, second, "client", 300_000))
        .await;
    handle
        .report(renewal(probe, 2_000, first, "keepalive", 300_000))
        .await;
    handle
        .report(Report::PinRenewed {
            pin: unmetered,
            now: 2_000,
            usage: None,
            origin: "client",
            cache_ttl_ms: 3_600_000,
            fast: false,
        })
        .await;
    let pins = handle.pins().await.unwrap();
    // The pool's burn window is the default 600 s, and the second fold lands 60 s after the first.
    let window = 600.0_f64;
    let decay = (-60.0_f64 / window).exp();
    let expected = PinBurn {
        cache_read: 1_000.0 / window * decay,
        input: 100.0 / window * decay + 500.0 / window,
        output: 20.0 / window * decay,
        write_1h: 30.0 / window * decay,
        write_5m: 50.0 / window * decay,
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
async fn a_fast_renewal_weighs_the_plain_copy_by_the_fast_cells_and_lets_the_banded_copy_decay() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    // The fast cells read twice the dollar ones, so a fast token weighs two standard tokens in the plain copy.
    let config = RouterConfig {
        rates: BurnRates {
            fast: flat_rates(0.5),
            ..quarter_dollar_rates()
        },
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
        .report(renewal(fast.clone(), 2_000, input(600), "client", 300_000))
        .await;
    handle
        .report(Report::PinRenewed {
            pin: fast,
            now: 62_000,
            usage: Some(input(1_000)),
            origin: "client",
            cache_ttl_ms: 300_000,
            fast: true,
        })
        .await;
    handle
        .report(renewal(standard, 62_000, input(1_000), "client", 300_000))
        .await;
    let pins = handle.pins().await.unwrap();
    // The pool's burn window is the default 600 s, and the fast fold lands 60 s after the standard one.
    let window = 600.0_f64;
    let decayed = 600.0 / window * (-60.0_f64 / window).exp();
    let s1 = pins.iter().find(|p| p.session_id == "s1").unwrap();
    assert!(
        (s1.burn.input - (decayed + 2_000.0 / window)).abs() < 1e-9,
        "the plain copy folds the thousand fast tokens as two thousand standard ones: {:?}",
        s1.burn
    );
    assert!(
        (s1.burn_banded.input - decayed).abs() < 1e-9,
        "the banded copy, the plan share, only decays: {:?}",
        s1.burn_banded
    );
    assert_eq!((s1.burn_at, s1.last_input_tokens), (Some(62_000), 1_000));
    let s2 = pins.iter().find(|p| p.session_id == "s2").unwrap();
    assert_eq!(
        (s2.burn.input, s2.burn_banded.input),
        (1_000.0 / window, 1_000.0 / window),
        "a standard renewal folds its tokens unweighed into both copies"
    );
}

#[tokio::test]
async fn the_burn_lands_in_the_store_with_the_input_size_and_the_ttl() {
    let store = seeded(&["a"]);
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
        .report(renewal(pin, 2_000, usage, "client", 3_600_000))
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
    let band = BandWeight::new(|window: &str, at: i64| {
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
        .report(renewal(afternoon, inside, usage, "client", 300_000))
        .await;
    handle
        .report(renewal(evening, outside, usage, "client", 300_000))
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
async fn a_reports_affinity_rows_reach_the_store_and_a_restart_reloads_them() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    {
        let handle = spawn(store.clone(), clock(&now), RouterConfig::default()).unwrap();
        let report = AffinityReport {
            message_boundaries: Arc::from(["b1".to_string(), "b2".to_string()]),
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
    assert!(
        entry.is_some(),
        "the restart reloaded the map from the store"
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
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
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
async fn a_members_spend_read_after_a_period_edit_answers_none_until_the_next_report() {
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
    let month_snapshot = handle.snapshot();
    let mut state = RoutingState::from_load(RoutingLoad::default());
    let month_row = MemberSpend {
        pool_id: "p".into(),
        account_id: "a".into(),
        period: phoebis_store::BudgetPeriod::Month,
        period_start: 0,
        period_end: 100_000,
        spent_usd: 5.0,
        observed_at: 1_000,
    };
    state.record_member_spend(&month_snapshot, vec![month_row.clone()]);
    assert_eq!(
        state.member_spends_live(&month_snapshot, "p", "a"),
        vec![&month_row],
        "the row matches the member's period at write time"
    );

    // The member's period changes with no report following yet: the raw accessor still holds the old row, but
    // a read against the new snapshot must not answer it as if it were current.
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
    let day_snapshot = handle.snapshot();
    assert_eq!(
        state.member_spends_of("p", "a").collect::<Vec<_>>(),
        vec![&month_row],
        "nothing has cleared the stale row yet"
    );
    assert!(
        state.member_spends_live(&day_snapshot, "p", "a").is_empty(),
        "the read applies the current period, not the one the stored row was summed under"
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
