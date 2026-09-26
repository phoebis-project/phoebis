use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::{Clock, CredentialManager, ExtraUsage, UsageService, UsageSnapshot, UsageWindow};
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{control_response, ControlFixture},
};
use phoebis_routing::{
    resolve, sample_from_usage, spawn, BandWeight, BurnRates, Exhaustion, PinRef, PinUsage,
    Presented, RateTable, Rates, ReloadError, Report, ResolveError, RouterConfig, SelectRequest,
    SelectResult, UtilizationSample, WindowReading, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{
    key_prefix, AccountKind, AccountRow, AccountState, Affinity, AffinitySource, ApiKeyRow,
    CooldownRow, Failover, FitRow, MemberWrite, ModelRule, NewBundle, OveragePolicy,
    PoolRow, QuotaHeaders, RulesMode, Secret, Shedding, StorageKey, Store, Strategy,
};
use phoebis_transport::{
    connector_builder,
    control::{ControlClient, ResolveFuture, Resolver},
    trust_extra_ca, Plane,
};
use fake_upstream::{stays_for, wait_for, wait_for_async, FakeUpstream, Respond, Rule};

const HOUR: i64 = 3_600_000;

fn seeded(ids: &[&str]) -> Arc<Store> {
    seeded_kind(AccountKind::ApiKey, ids)
}

/// The same pool over OAuth accounts, which are the only ones a usage sample is ever pulled for.
fn seeded_oauth(ids: &[&str]) -> Arc<Store> {
    seeded_kind(AccountKind::Oauth, ids)
}

fn seeded_kind(kind: AccountKind, ids: &[&str]) -> Arc<Store> {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
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
                id: "p".into(),
                label: "p".into(),
                enabled: true,
                strategy: Strategy::Priority,
                affinity_ttl_seconds: 3600,
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
                cache_diagnostics: None,
                affinity: Affinity::default(),
                quota_headers: QuotaHeaders::Pass,
                created_at: 0,
                updated_at: 0,
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
                    budget_usd: None,
                    budget_period: phoebis_store::BudgetPeriod::Month,
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
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                in_flight_cap: None,
                affinity_source: AffinitySource::Auto,
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

fn request(handle: &phoebis_routing::RouterHandle, session: Option<&str>, now: i64) -> SelectRequest {
    let s = handle.snapshot();
    SelectRequest {
        key_id: "k".into(),
        key_generation: s.key("k").unwrap().created_at,
        agent_id: "main".into(),
        session_id: session.map(String::from),
        canonical_model: "claude-sonnet-5".into(),
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
        boundaries: Vec::new(),
    }
}

/// How long a store write may take to reach disk before the test calls it lost. Only ever waited out in
/// full when the write never lands.
const WRITTEN: Duration = Duration::from_secs(2);

/// The window a write that must never land is given to prove it. A bounded negative check has to wait.
const NOT_WRITTEN: Duration = Duration::from_millis(50);

/// The same request, admitted under a snapshot the registry has since moved past.
fn stale(mut req: SelectRequest) -> SelectRequest {
    req.revision = req.revision.saturating_sub(1);
    req
}

#[tokio::test]
async fn a_selection_is_written_through_and_a_registry_write_shows_up_after_reload() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
                budget_usd: None,
                budget_period: phoebis_store::BudgetPeriod::Month,
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
        handle.snapshot().authenticate("phoebis-secret", 1_002).is_err(),
        "a revoked key answers 401 at once"
    );
}

#[tokio::test]
async fn a_request_admitted_under_an_older_registry_is_re_resolved_against_the_current_one() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
    store.update_pool(&pool, "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted.clone())).await.unwrap();
    assert!(matches!(
        answer.result,
        SelectResult::Refused(ResolveError::ModelNotAllowed)
    ));
    store.revoke_api_key("k", 2, "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted)).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "a key revoked after admission selects nothing"
    );
}

#[tokio::test]
async fn a_request_admitted_under_a_key_that_was_replaced_under_its_id_is_refused() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
        matches!(answer.result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "refused as a refused id is, never resolved against the replacement: {:?}",
        answer.result
    );
    assert!(
        handle.pins().await.unwrap().is_empty(),
        "no pin was written under the replacement key"
    );
}

#[tokio::test]
async fn a_failed_reload_keeps_the_old_snapshot_and_says_so() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    store.revoke_api_key("k", 2, "test").unwrap();
    store.debug_fail_next_load();
    assert!(matches!(handle.reload().await, Err(ReloadError::Load(_))));
    assert!(
        handle.snapshot().authenticate("phoebis-secret", 1_003).is_ok(),
        "the old snapshot still authenticates the revoked key"
    );
    handle.reload().await.unwrap();
    assert!(
        handle.snapshot().authenticate("phoebis-secret", 1_004).is_err(),
        "the next successful reload publishes the revocation"
    );
}

#[tokio::test]
async fn shutdown_lands_every_acknowledged_write_before_it_returns() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    for i in 0..200 {
        let answer = handle
            .select(request(&handle, Some(&format!("s{i}")), 1_000 + i))
            .await
            .unwrap();
        assert!(matches!(answer.result, SelectResult::Selected(_)));
    }
    handle.report(Report::Cooldown {
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
    });
    handle.report(Report::AccountState {
        account_id: "a".into(),
        state: AccountState::ReauthRequired,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
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
async fn a_state_verdict_about_a_replaced_credential_changes_nothing() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k1"))
        .unwrap();
    let (_, generation) = store.api_key_credential("a").unwrap().unwrap();
    store
        .set_api_key_secret("a", &phoebis_store::Secret::new("k2"))
        .unwrap();
    handle.report(Report::AccountState {
        account_id: "a".into(),
        state: AccountState::ReauthRequired,
        lifecycle_generation: 1,
        credential_generation: generation,
        incarnation: 1,
    });
    // The verdict named the key the store has since replaced, so the writer fences it out.
    stays_for(NOT_WRITTEN, || {
        store.account("a").unwrap().unwrap().state == AccountState::Ready
    })
    .await;
    handle.report(Report::AccountState {
        account_id: "a".into(),
        state: AccountState::ReauthRequired,
        lifecycle_generation: 1,
        credential_generation: generation + 1,
        incarnation: 1,
    });
    wait_for(WRITTEN, || {
        (store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
}

#[tokio::test]
async fn two_concurrent_first_requests_of_one_session_land_on_one_account() {
    let store = seeded(&["a", "b", "c"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
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
        let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
        handle.report(Report::Cooldown {
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
        });
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
            key_id: "k".into(),
            key_generation: s.key("k").unwrap().created_at,
            agent_id: "main".into(),
            session_id: Some("s1".into()),
            canonical_model: "claude-sonnet-4-5".into(),
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
            boundaries: Vec::new(),
        }
    };
    let epoch;
    {
        let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    let answer = handle.select(req(&handle, 2_000)).await.unwrap();
    let SelectResult::Selected(selection) = answer.result else {
        panic!("expected a selection")
    };
    assert_eq!(
        (selection.account.id.as_str(), selection.pin.unwrap().epoch),
        ("a", epoch),
        "the pin renews under the dated id rather than starting over"
    );
    handle.report(Report::Cooldown {
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
    });
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    let s = handle.snapshot();
    let answer = handle
        .select(SelectRequest {
            key_id: "k".into(),
            key_generation: s.key("k").unwrap().created_at,
            agent_id: "main".into(),
            session_id: Some("s1".into()),
            canonical_model: "claude-sonnet-4-5".into(),
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
            boundaries: Vec::new(),
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
    handle.report(model_cooldown(other, "model:claude-sonnet-4-5", 1_000));
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(model_cooldown("a", "model:claude-sonnet-4-5", 1_000));
    wait_for(WRITTEN, || {
        (!store.load_routing().unwrap().cooldowns.is_empty()).then_some(())
    })
    .await;
    store.delete_account("a", "test").unwrap();
    handle
        .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 1_500)
        .await
        .unwrap();
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
    handle.report(model_cooldown("b", "model:claude-opus-5", 1_000));
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
    handle.report(model_cooldown("b", "model:claude-opus-5", 1_000));
    landed(&store, "model:claude-opus-5").await;
    restore_a(&store);
    handle.report(model_cooldown("b", "model:claude-haiku-4-5", 1_000));
    landed(&store, "model:claude-haiku-4-5").await;
    assert!(
        store.load_routing().unwrap().resolutions.is_empty(),
        "a rekey that failed its one retry is dropped, not retried at every later write"
    );
}

#[tokio::test]
async fn disconnecting_an_account_cancels_its_in_flight_relays_and_a_pause_does_not() {
    let store = seeded(&["a", "b"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.touch_key("k", Presented::Live, None);
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)).then_some(())
    })
    .await;
    now.store(11_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Live, None);
    // Ten seconds on from the last touch, which is inside the minute, so nothing is written.
    stays_for(NOT_WRITTEN, || {
        store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)
    })
    .await;
    now.store(62_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Live, None);
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.touch_key("k", Presented::Live, rotated_at);
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)).then_some(())
    })
    .await;
    now.store(11_000, Ordering::SeqCst);
    handle.touch_key("k", Presented::Retired, rotated_at);
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
    handle.touch_key("k", Presented::Retired, rotated_at);
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
    handle.touch_key("k", Presented::Retired, Some(30_000));
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
    handle.touch_key("k", Presented::Retired, rotated_at);
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
    handle.report(Report::Cooldown {
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
    });
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
    handle.report(Report::Cooldown {
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
    });
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
    }
}

#[tokio::test]
async fn an_account_recreated_under_a_used_id_inherits_none_of_its_routing_state() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(Report::Cooldown {
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
    });
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
                budget_usd: None,
                budget_period: phoebis_store::BudgetPeriod::Month,
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
                budget_usd: None,
                budget_period: phoebis_store::BudgetPeriod::Month,
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
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                in_flight_cap: None,
                affinity_source: AffinitySource::Auto,
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();
    handle.report(Report::AccountState {
        account_id: "a".into(),
        state: AccountState::FundingRequired,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    handle.report(Report::Cooldown {
        account_id: "a".into(),
        row: cooldown_row("a"),
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    // The report and the ask share one sender, so both reports are applied before this answers.
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
    handle.report(mark(1));
    assert_eq!(
        handle.pins().await.unwrap().len(),
        1,
        "the mark was about the holder the id had before"
    );
    stays_for(NOT_WRITTEN, || {
        !store.load_routing().unwrap().pins.is_empty()
    })
    .await;
    handle.report(mark(2));
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
    handle.report(cooled(
        "a",
        "account",
        1_000 + HOUR,
        "rate_limit",
        "five_hour",
    ));
    handle.report(Report::Overage {
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
    });
    handle.report(observed("a", sample_with(1_100, FIVE_HOUR, 1.0, None)));
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
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
async fn a_fetch_started_under_a_previous_incarnation_changes_nothing() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    store.delete_account("a", "test").unwrap();
    insert_account_a(&store);
    handle.reload().await.unwrap();
    // The answers of a usage fetch that left under the first holder and landed after the reload.
    handle.report(Report::Sample {
        account_id: "a".into(),
        sample: sample_with(1_500, FIVE_HOUR, 1.0, None),
        incarnation: 1,
    });
    handle.report(Report::Overage {
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
    });
    assert!(
        handle.samples().await.unwrap().is_empty(),
        "the old holder's sample was refused"
    );
    assert!(
        handle.overages().await.unwrap().is_empty(),
        "and so was its overage reading"
    );
    handle.shutdown().await.unwrap();
    assert!(
        store.load_routing().unwrap().overage.is_empty(),
        "and nothing reached the store"
    );
}

/// A usage service over account `a` of `store` whose fetches reach `up`, a fake control plane on a loopback
/// port. The token is far from expiry, so no fetch refreshes first, and the cache holds nothing, so every fetch
/// the router starts is one request the fake records.
fn usage_against(up: &FakeUpstream, store: &Arc<Store>, clock: Clock) -> Arc<UsageService> {
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let profile = claude_code_2_1_269_linux_x64();
    let mut builder = connector_builder(profile, Plane::Control).unwrap();
    trust_extra_ca(&mut builder, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let control = Arc::new(ControlClient::new(profile, builder.build(), resolver));
    let manager = Arc::new(CredentialManager::new(
        store.clone(),
        control.clone(),
        profile,
        clock.clone(),
    ));
    Arc::new(UsageService::new(
        manager,
        control,
        profile,
        Duration::ZERO,
        4,
        clock,
    ))
}

#[tokio::test]
async fn a_head_sample_within_the_ttl_defers_the_usage_poll_and_one_past_it_does_not() {
    let up = usage_endpoint().await;
    let fetches = || usage_fetches(&up);
    // Under `prefer_plan` every selection needs a sample, so only the sampler's own clock holds a fetch back.
    let store = seeded_oauth(&["a"]);
    let start = 1_000_000;
    let now = Arc::new(AtomicI64::new(start));
    let config = RouterConfig::default();
    let ttl = config.sample_ttl.as_millis() as i64;
    let handle = spawn(
        store.clone(),
        Some(usage_against(&up, &store, clock(&now))),
        clock(&now),
        config,
    )
    .unwrap();
    handle.report(observed("a", shared_windows(start)));
    let inside = start + ttl - 1_000;
    now.store(inside, Ordering::SeqCst);
    handle
        .select(request(&handle, Some("s1"), inside))
        .await
        .unwrap();
    stays_for(Duration::from_millis(200), || fetches() == 0).await;

    let past = start + ttl;
    now.store(past, Ordering::SeqCst);
    handle
        .select(request(&handle, Some("s1"), past))
        .await
        .unwrap();
    wait_for(WRITTEN, || (fetches() == 1).then_some(())).await;
    stays_for(Duration::from_millis(100), || fetches() == 1).await;
    up.shutdown().await;
}

/// A fake control plane whose usage endpoint answers the recorded document.
async fn usage_endpoint() -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Usage).to_vec(),
        },
    }])
    .await
}

fn usage_fetches(up: &FakeUpstream) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.contains("/api/oauth/usage"))
        .count()
}

/// The two shared windows as a head reads them, at `observed_at`.
fn shared_windows(observed_at: i64) -> UtilizationSample {
    sample_with(observed_at, FIVE_HOUR, 0.2, None).with(
        SEVEN_DAY,
        WindowReading {
            utilization: 0.1,
            reset: None,
            observed_at,
        },
    )
}

#[tokio::test]
async fn a_head_that_leaves_a_stored_window_unread_leaves_the_usage_poll_due() {
    let up = usage_endpoint().await;
    let store = seeded_oauth(&["a"]);
    let start = 1_000_000;
    let now = Arc::new(AtomicI64::new(start));
    let config = RouterConfig::default();
    let ttl = config.sample_ttl.as_millis() as i64;
    let handle = spawn(
        store.clone(),
        Some(usage_against(&up, &store, clock(&now))),
        clock(&now),
        config,
    )
    .unwrap();
    // A TTL ago the account's sample read the Fable window too; the heads since read the shared windows alone.
    let earlier = start - ttl;
    handle.report(observed(
        "a",
        shared_windows(earlier).with(
            SEVEN_DAY_FABLE,
            WindowReading {
                utilization: 0.3,
                reset: None,
                observed_at: earlier,
            },
        ),
    ));
    handle.report(observed("a", shared_windows(start)));
    let inside = start + 1_000;
    now.store(inside, Ordering::SeqCst);
    handle
        .select(request(&handle, Some("s1"), inside))
        .await
        .unwrap();
    wait_for(WRITTEN, || (usage_fetches(&up) == 1).then_some(())).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_head_that_reads_one_shared_window_leaves_the_usage_poll_due() {
    let up = usage_endpoint().await;
    let store = seeded_oauth(&["a"]);
    let start = 1_000_000;
    let now = Arc::new(AtomicI64::new(start));
    let handle = spawn(
        store.clone(),
        Some(usage_against(&up, &store, clock(&now))),
        clock(&now),
        RouterConfig::default(),
    )
    .unwrap();
    handle.report(observed("a", sample_with(start, FIVE_HOUR, 0.2, None)));
    let inside = start + 1_000;
    now.store(inside, Ordering::SeqCst);
    handle
        .select(request(&handle, Some("s1"), inside))
        .await
        .unwrap();
    wait_for(WRITTEN, || (usage_fetches(&up) == 1).then_some(())).await;
    up.shutdown().await;
}

/// Pace ranks subscription accounts by their readings, so it is a reason on its own to read a quiet sibling: with
/// the kind order, the spent move, the thresholds and the overage policy all left without a reason to sample, a due
/// sibling is still fetched, and its reading is what the next placement ranks it by.
#[tokio::test]
async fn a_pace_pool_with_its_other_sampling_reasons_off_still_reads_a_due_sibling() {
    let up = usage_endpoint().await;
    let store = seeded_oauth(&["a", "b"]);
    let mut pool = store.pool("p").unwrap().unwrap();
    pool.strategy = Strategy::Pace;
    pool.prefer_subscription = false;
    pool.overage_policy = OveragePolicy::Allow;
    pool.shedding.migrate_when_spent = false;
    pool.migrate_above_utilization = None;
    pool.select_below_utilization = None;
    store.update_pool(&pool, "test").unwrap();
    let start = 1_000_000;
    let now = Arc::new(AtomicI64::new(start));
    let usage = usage_against(&up, &store, clock(&now));
    store
        .commit_credentials(
            "b",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-PLACEHOLDER-B"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let handle = spawn(
        store.clone(),
        Some(usage),
        clock(&now),
        RouterConfig::default(),
    )
    .unwrap();
    // `a` answers from its own heads, which holds its fetch off; `b` has no reading and reads as untouched.
    handle.report(observed("a", shared_windows(start)));
    let inside = start + 1_000;
    now.store(inside, Ordering::SeqCst);
    handle
        .select(request(&handle, Some("s1"), inside))
        .await
        .unwrap();
    wait_for(WRITTEN, || (usage_fetches(&up) == 1).then_some(())).await;
    let b = wait_for_async(WRITTEN, || async {
        handle
            .samples()
            .await
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == "b")
            .map(|(_, sample)| sample)
    })
    .await;
    assert_eq!(
        b.reading(SEVEN_DAY).map(|r| r.utilization),
        Some(0.4),
        "the fetched document's weekly reading"
    );
    // The weekly reading leaves `b` far less allowance per hour to its reset than `a` has, so the next session
    // is placed on `a`, which only a selection reading `b`'s sample can decide.
    let next = start + 2_000;
    now.store(next, Ordering::SeqCst);
    let answer = handle
        .select(request(&handle, Some("s2"), next))
        .await
        .unwrap();
    match answer.result {
        SelectResult::Selected(selection) => assert_eq!(selection.account.id, "a"),
        other => panic!("expected a selection, got {other:?}"),
    }
    stays_for(Duration::from_millis(100), || usage_fetches(&up) == 1).await;
    up.shutdown().await;
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
        None,
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
    handle.report(Report::Cooldown {
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
    });
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
    handle.report(Report::Overage {
        account_id: b.clone(),
        evidence: evidence(true, 1_200),
        incarnation: 1,
    });
    handle.report(Report::Overage {
        account_id: b.clone(),
        evidence: evidence(true, 1_300),
        incarnation: 1,
    });
    handle.report(Report::Overage {
        account_id: b.clone(),
        evidence: evidence(false, 1_400),
        incarnation: 1,
    });
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
    handle.report(cooled(
        &a,
        "account",
        2_000 + HOUR,
        "rate_limit",
        "five_hour",
    ));
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
    handle.report(Report::Sample {
        account_id: a.clone(),
        sample: sample_with(1_500, FIVE_HOUR, 0.7, Some(9_000)).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.2,
                reset: None,
                observed_at: 1_500,
            },
        ),
        incarnation: 1,
    });
    // The report and the ask share one sender, so the task has applied the sample before it answers this.
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
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
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
            },
        )
    };
    handle.report(Report::Sample {
        account_id: a.clone(),
        sample: spent(1.0, 1_500),
        incarnation: 1,
    });
    handle.report(Report::Sample {
        account_id: b.into(),
        sample: spent(0.2, 1_500),
        incarnation: 1,
    });
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
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
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
            },
        )
    };
    handle.report(cooled(
        &a,
        "account",
        1_000 + HOUR,
        "rate_limit",
        "five_hour",
    ));
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    handle.report(Report::Sample {
        account_id: a.clone(),
        sample: spent(1_500),
        incarnation: 1,
    });
    handle.report(Report::Sample {
        account_id: b.into(),
        sample: spent(1_500),
        incarnation: 1,
    });
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    let mut events = handle.subscribe_events();
    handle.report(cooled(
        "a",
        "account",
        1_000 + 5 * HOUR,
        "rate_limit",
        "five_hour",
    ));
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
    handle.report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)));
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    let rejection = || cooled("a", "account", 1_000 + 5 * HOUR, "rate_limit", "five_hour");
    handle.report(rejection());
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    now.store(3_000, Ordering::SeqCst);
    handle.report(rejection());
    // The usage fetch behind this reading left before the second rejection and landed after it.
    handle.report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a reading older than the latest rejection is not evidence the account recovered"
    );
    handle.report(observed("a", sample_with(3_001, FIVE_HOUR, 0.4, None)));
    assert!(
        handle.cooldowns().await.unwrap().is_empty(),
        "one taken after it is"
    );
}

#[tokio::test]
async fn a_sample_the_cooled_account_was_holding_when_it_was_rejected_releases_nothing() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(2_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(cooled(
        "a",
        "account",
        2_000 + 5 * HOUR,
        "rate_limit",
        "five_hour",
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    // The usage endpoint serves a cached view for a minute, so the snapshot that arrives first can be older
    // than the rejection.
    handle.report(observed("a", sample_with(1_999, FIVE_HOUR, 0.4, None)));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a reading from before the row was set is not evidence about it"
    );
    handle.report(observed("a", sample_with(2_001, FIVE_HOUR, 0.4, None)));
    assert!(
        handle.cooldowns().await.unwrap().is_empty(),
        "the same reading taken after it is"
    );
}

#[tokio::test]
async fn a_window_at_its_limit_keeps_the_cooldown() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(cooled(
        "a",
        "account",
        1_000 + 48 * HOUR,
        "rate_limit",
        "seven_day",
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle.report(observed("a", sample_with(2_000, SEVEN_DAY, 1.0, None)));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a window that is exactly full has nothing left to serve with"
    );
    handle.report(observed("a", sample_with(2_001, FIVE_HOUR, 0.1, None)));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "and the row names the window that is read, not whichever one recovered"
    );
    handle.report(observed("a", sample_with(2_002, SEVEN_DAY, 0.99, None)));
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_claim_that_names_neither_shared_window_waits_for_both() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    // The upstream's representative claim travels into the row as it came, so the window is not always one of
    // the two names the sample reads.
    handle.report(cooled(
        "a",
        "account",
        1_000 + 5 * HOUR,
        "rate_limit",
        "seven_day_oauth_apps",
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle.report(observed(
        "a",
        sample_with(2_000, FIVE_HOUR, 0.4, None).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 1.0,
                reset: None,
                observed_at: 2_000,
            },
        ),
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "one shared window still full is enough to hold the account out"
    );
    handle.report(observed("a", sample_with(2_001, FIVE_HOUR, 0.4, None)));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "and a window the sample never read is no evidence either"
    );
    handle.report(observed(
        "a",
        sample_with(2_002, FIVE_HOUR, 0.4, None).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.9,
                reset: None,
                observed_at: 2_002,
            },
        ),
    ));
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_row_no_usage_window_answers_for_is_left_alone() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(cooled(
        "a",
        "account",
        1_000 + 5 * HOUR,
        "rate_limit",
        "retry_after",
    ));
    // A permission rejection over a window a sample can read: the reason is what keeps the row, because no
    // amount of allowance restores an entitlement.
    handle.report(cooled(
        "a",
        "model:claude-sonnet-5",
        1_000 + HOUR,
        "permission",
        "five_hour",
    ));
    // A fable-only 429 whose headers carried no claim: the upstream sent a deadline and named no window, so
    // the fable reading is a guess about which window the rejection was for.
    handle.report(cooled(
        "a",
        "model:claude-fable-5-1",
        1_000 + HOUR,
        "rate_limit",
        "retry_after",
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 3);
    handle.report(observed(
        "a",
        sample_with(2_000, SEVEN_DAY_FABLE, 0.0, None)
            .with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.0,
                    reset: None,
                    observed_at: 2_000,
                },
            )
            .with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.0,
                    reset: None,
                    observed_at: 2_000,
                },
            ),
    ));
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
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(cooled(
        "a",
        "account",
        1_000 + 5 * HOUR,
        "rate_limit",
        "five_hour",
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    // Two pulls in flight, the newer one back first: the older one is still later than the row was set, and
    // its reading is the one the window has already moved past.
    handle.report(observed("a", sample_with(3_000, FIVE_HOUR, 1.0, None)));
    handle.report(observed("a", sample_with(2_000, FIVE_HOUR, 0.4, None)));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "the account's newest reading is what the row is answered against"
    );
    handle.report(observed("a", sample_with(3_001, FIVE_HOUR, 0.4, None)));
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
        None,
        clock(&now),
        RouterConfig {
            purge_interval: Duration::from_millis(50),
            ..RouterConfig::default()
        },
    )
    .unwrap();
    let mut events = handle.subscribe_events();
    handle.report(cooled(
        "a",
        "account",
        1_000 + HOUR,
        "rate_limit",
        "five_hour",
    ));
    assert!(matches!(
        next_event(&mut events).await,
        RouterEvent::CooldownSet { .. }
    ));
    now.store(1_000 + HOUR + 1, Ordering::SeqCst);
    handle.report(observed(
        "a",
        sample_with(1_000 + HOUR + 1, FIVE_HOUR, 0.4, None),
    ));
    assert!(
        matches!(next_event(&mut events).await, RouterEvent::CooldownCleared { cause, .. } if cause == "expired"),
        "the deadline arriving is not a window that reset early, and the ledger has to tell them apart"
    );
}

#[tokio::test]
async fn a_model_scoped_row_reads_the_model_window_and_nothing_else() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
    handle.report(cooled(
        "a",
        "model:claude-sonnet-5",
        1_000 + 48 * HOUR,
        "rate_limit",
        "seven_day_fable",
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle.report(observed(
        "a",
        sample_with(2_000, FIVE_HOUR, 0.1, None).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.1,
                reset: None,
                observed_at: 2_000,
            },
        ),
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "the shared windows say nothing about a model-scoped allowance"
    );
    handle.report(observed(
        "a",
        sample_with(2_001, SEVEN_DAY_FABLE, 1.0, None),
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle.report(observed(
        "a",
        sample_with(2_002, SEVEN_DAY_FABLE, 0.5, None),
    ));
    assert!(handle.cooldowns().await.unwrap().is_empty());
}

#[test]
fn a_key_permit_is_reserved_atomically_and_released_once_by_its_drop() {
    use phoebis_routing::InFlight;
    let in_flight = InFlight::default();
    assert_eq!(in_flight.key("k"), 0);
    let first = in_flight.try_acquire_key("k", Some(1), 1_000);
    assert!(first.is_some());
    assert_eq!(in_flight.key("k"), 1);
    let second = in_flight.try_acquire_key("k", Some(1), 1_000);
    assert!(second.is_none(), "the cap is reached");
    assert_eq!(in_flight.key("k"), 1);
    drop(first);
    assert_eq!(in_flight.key("k"), 0);
    let third = in_flight.try_acquire_key("k", Some(1), 1_000);
    assert!(third.is_some());
    assert_eq!(in_flight.key("k"), 1);

    // No cap always answers a permit and still counts.
    let uncapped = in_flight.try_acquire_key("other", None, 1_000);
    assert!(uncapped.is_some());
    assert_eq!(in_flight.key("other"), 1);
}

#[test]
fn the_purge_reclaims_a_permit_and_a_ticket_whose_guard_never_dropped() {
    use phoebis_routing::{InFlight, IN_FLIGHT_RECLAIM_MS};
    let in_flight = InFlight::default();
    let taken_at = 1_000 - IN_FLIGHT_RECLAIM_MS - 1;
    let permit = in_flight.try_acquire_key("k", None, taken_at).unwrap();
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
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
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
        .filter(|answer| matches!(&answer.result, SelectResult::Held { account_id } if account_id == "a"))
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
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
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
    handle.report(Report::Cooldown {
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
    });
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
async fn a_probe_after_a_compacted_request_refreshes_warmth() {
    use phoebis_routing::{warm, Report};
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
    let compacted = |now: i64, probe: bool| SelectRequest {
        probe,
        request_class: Some("main".into()),
        context_compacted: true,
        static_hash: Some("s0".into()),
        boundaries: vec!["c1".into()],
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
    handle.report(Report::PinRenewed {
        pin: selection.pin.expect("the probe held the pin"),
        now: 3_500,
        usage: None,
        origin: "keepalive",
        canonical_model: "claude-sonnet-5".into(),
        cache_ttl_ms: 300_000,
    });
    let pin = handle.pins().await.unwrap().remove(0);
    assert_eq!(
        pin.last_refresh_at, 3_500,
        "the probe's renewal counts as a refresh"
    );
    assert!(warm(&pin, 3_600));
}

/// A meter table weighing every token 1 and a dollar table weighing every token a quarter, so a pin's two
/// averages differ by exactly that ratio.
fn quarter_dollar_rates() -> BurnRates {
    let mut dollars = RateTable::unit();
    dollars.fallback = Rates {
        input: 0.25,
        cache_read: 0.25,
        write_5m: 0.25,
        write_1h: 0.25,
        output: 0.25,
    };
    BurnRates {
        meter: RateTable::unit(),
        dollars,
    }
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
        canonical_model: "claude-sonnet-5".into(),
        cache_ttl_ms,
    }
}

fn selected_pin(answer: phoebis_routing::SelectAnswer) -> PinRef {
    match answer.result {
        SelectResult::Selected(s) => s.pin.expect("a session request holds a pin"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_renewal_with_usage_folds_both_averages_and_a_probes_usage_never_counts() {
    let store = seeded(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let config = RouterConfig {
        rates: quarter_dollar_rates(),
        ..RouterConfig::default()
    };
    let handle = spawn(store, None, clock(&now), config).unwrap();
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
    handle.report(renewal(client.clone(), 2_000, first, "client", 300_000));
    handle.report(renewal(client, 62_000, second, "client", 300_000));
    handle.report(renewal(probe, 2_000, first, "keepalive", 300_000));
    handle.report(Report::PinRenewed {
        pin: unmetered,
        now: 2_000,
        usage: None,
        origin: "client",
        canonical_model: "claude-sonnet-5".into(),
        cache_ttl_ms: 3_600_000,
    });
    let pins = handle.pins().await.unwrap();
    // The pool's burn window is the default 600 s, and the second fold lands 60 s after the first.
    let window = 600.0_f64;
    let meter = 1_200.0 / window * (-60.0_f64 / window).exp() + 500.0 / window;
    let folded = pins.iter().find(|p| p.session_id == "s1").unwrap();
    assert!(
        (folded.burn - meter).abs() < 1e-9,
        "the second fold decays the first by exp(-dt/window) and adds its own p/window: {} against {meter}",
        folded.burn
    );
    assert!(
        (folded.burn_usd - 0.25 * meter).abs() < 1e-9,
        "the dollar average folds the same usage through its own table: {} against {}",
        folded.burn_usd,
        0.25 * meter
    );
    assert_eq!(
        (folded.burn_at, folded.last_input_tokens),
        (Some(62_000), 500)
    );
    let probed = pins.iter().find(|p| p.session_id == "s2").unwrap();
    assert_eq!(
        (probed.burn, probed.burn_usd, probed.burn_at),
        (0.0, 0.0, None),
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
            bare.burn_usd,
            bare.burn_at,
            bare.last_input_tokens
        ),
        (0.0, 0.0, None, 0),
        "and folds nothing"
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
    let handle = spawn(store.clone(), None, clock(&now), config).unwrap();
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
    handle.report(renewal(pin, 2_000, usage, "client", 3_600_000));
    let row = wait_for(WRITTEN, || {
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .find(|p| p.burn_at.is_some())
    })
    .await;
    let priced = 642.0 / 600.0;
    assert!((row.burn - priced).abs() < 1e-9, "{}", row.burn);
    assert!(
        (row.burn_usd - 0.25 * priced).abs() < 1e-9,
        "{}",
        row.burn_usd
    );
    assert_eq!(
        (row.last_input_tokens, row.cache_ttl_ms),
        (630, 3_600_000),
        "the input size counts every prompt token and the TTL is the one the relay resolved"
    );
    handle.shutdown().await.unwrap();
    let again = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
    let reloaded = again.pins().await.unwrap().remove(0);
    assert_eq!(
        (reloaded.burn, reloaded.burn_usd),
        (row.burn, row.burn_usd),
        "a restart reads both averages back"
    );
}

#[tokio::test]
async fn a_renewal_inside_the_band_folds_a_heavier_meter_burn_and_the_same_dollars() {
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
    let handle = spawn(store, None, clock(&now), config).unwrap();
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
    handle.report(renewal(afternoon, inside, usage, "client", 300_000));
    handle.report(renewal(evening, outside, usage, "client", 300_000));
    let pins = handle.pins().await.unwrap();
    let pin = |session: &str| pins.iter().find(|p| p.session_id == session).unwrap();
    assert!(
        (pin("s1").burn - 1.45 * pin("s2").burn).abs() < 1e-9 && pin("s2").burn > 0.0,
        "the afternoon renewal folds 1.45 times the evening one: {} against {}",
        pin("s1").burn,
        pin("s2").burn
    );
    assert_eq!(
        pin("s1").burn_usd,
        pin("s2").burn_usd,
        "the dollar average is not weighted by the band"
    );
}

#[tokio::test]
async fn a_fit_report_lands_in_the_store_and_reloads() {
    let store = seeded_oauth(&["a"]);
    let now = Arc::new(AtomicI64::new(1_000));
    let row = FitRow {
        account_id: "a".into(),
        window: FIVE_HOUR.into(),
        scale: 11_890_000.0,
        confidence: 0.6,
        intervals: 3,
        corrections_json: "{}".into(),
        fitted_at: 2_000,
        incarnation: 1,
        band_factor: Some(1.45),
        carry_weighted: 100_000.0,
        carry_outside: 100_000.0,
        carry_delta: 0.01,
        carry_since: Some(2_000),
        carry_utilization: Some(0.26),
        ..FitRow::default()
    };
    {
        let handle = spawn(store.clone(), None, clock(&now), RouterConfig::default()).unwrap();
        handle.report(Report::Fit {
            row: row.clone(),
            incarnation: 1,
        });
        handle.report(Report::Fit {
            row: FitRow {
                scale: 1.0,
                fitted_at: 1_500,
                ..row.clone()
            },
            incarnation: 1,
        });
        handle.report(Report::Fit {
            row: FitRow {
                window: SEVEN_DAY.into(),
                ..row.clone()
            },
            incarnation: 2,
        });
        assert_eq!(
            handle.fits().await.unwrap(),
            vec![row.clone()],
            "an older fold loses, and a report for a holder the id moved past lands nowhere"
        );
        wait_for(WRITTEN, || {
            (store.fits().unwrap() == vec![row.clone()]).then_some(())
        })
        .await;
        handle.shutdown().await.unwrap();
    }
    let again = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
    assert_eq!(
        again.fits().await.unwrap(),
        vec![row],
        "a restart reads the fit back"
    );
}

#[tokio::test]
async fn a_stored_fit_the_fitter_would_refuse_is_not_served() {
    let store = seeded_oauth(&["a"]);
    let refused = FitRow {
        account_id: "a".into(),
        window: FIVE_HOUR.into(),
        scale: 0.0,
        confidence: 0.6,
        intervals: 3,
        corrections_json: "{}".into(),
        fitted_at: 2_000,
        incarnation: 1,
        ..FitRow::default()
    };
    assert!(!refused.sound());
    store.upsert_fit(&refused).unwrap();
    let now = Arc::new(AtomicI64::new(1_000));
    let handle = spawn(store, None, clock(&now), RouterConfig::default()).unwrap();
    assert!(
        handle.fits().await.unwrap().is_empty(),
        "a zero scale is no window size to serve"
    );
}
