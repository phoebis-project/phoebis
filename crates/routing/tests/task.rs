use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::{Clock, ExtraUsage, UsageSnapshot, UsageWindow};
use phoebis_routing::{
    resolve, sample_from_usage, spawn, Exhaustion, ReloadError, Report, ResolveError, RouterConfig,
    SelectRequest, SelectResult, UtilizationSample,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, OveragePolicy, PoolRow,
    ServedModelWrite, StorageKey, Store, Strategy,
};
use fake_upstream::{stays_for, wait_for};

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
        store
            .apply_catalog_sync(id, &[("claude-sonnet-5".into(), false)], None, 1, "test")
            .unwrap();
    }
    store
        .insert_pool(
            &PoolRow {
                id: "p".into(),
                label: "p".into(),
                enabled: true,
                strategy: Strategy::PriorityRandom,
                affinity_ttl_seconds: 3600,
                overage_policy: OveragePolicy::PreferPlan,
                migrate_above_utilization: None,
                select_below_utilization: None,
                default_allowed_models: None,
                keepalive_json: "{}".into(),
                body_capture_hours: 0,
                created_at: 0,
                updated_at: 0,
            },
            "test",
        )
        .unwrap();
    store
        .set_pool_members(
            "p",
            &ids.iter().map(|id| (id.to_string(), 0)).collect::<Vec<_>>(),
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
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-secret").to_vec(),
                prefix: "secret00".into(),
                created_at: 0,
                rotated_at: None,
                last_used_at: None,
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
        session_id: session.map(String::from),
        canonical_model: "claude-sonnet-5".into(),
        candidates: resolve(&s, s.key("k").unwrap(), "claude-sonnet-5").unwrap(),
        portable: true,
        attempted: vec![],
        now,
        fallback_ms: 60_000,
        revision: s.revision,
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
        .apply_catalog_sync("b", &[("claude-sonnet-5".into(), false)], None, 1, "test")
        .unwrap();
    store
        .set_pool_members("p", &[("b".into(), 0)], "test")
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
    pool.default_allowed_models = Some(vec!["claude-opus-5".into()]);
    store.update_pool(&pool, "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted.clone())).await.unwrap();
    assert!(matches!(
        answer.result,
        SelectResult::Refused(ResolveError::ModelNotAllowed)
    ));
    pool.default_allowed_models = None;
    store.update_pool(&pool, "test").unwrap();
    store
        .put_served_models(
            "b",
            &[ServedModelWrite {
                upstream_id: "claude-sonnet-5".into(),
                selected: false,
                long_context: false,
            }],
            "test",
        )
        .unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted.clone())).await.unwrap();
    assert!(
        matches!(
            answer.result,
            SelectResult::Refused(ResolveError::ModelUnserved)
        ),
        "a model deselected after admission is unserved"
    );
    store.revoke_api_key("k", 2, "test").unwrap();
    handle.reload().await.unwrap();
    let answer = handle.select(stale(admitted)).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "a key revoked after admission selects nothing"
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
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
    });
    handle.report(Report::AccountState {
        account_id: "a".into(),
        state: AccountState::ReauthRequired,
        lifecycle_generation: 1,
        credential_generation: 1,
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
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
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
    handle.touch_key("k");
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)).then_some(())
    })
    .await;
    now.store(11_000, Ordering::SeqCst);
    handle.touch_key("k");
    // Ten seconds on from the last touch, which is inside the minute, so nothing is written.
    stays_for(NOT_WRITTEN, || {
        store.api_key("k").unwrap().unwrap().last_used_at == Some(1_000)
    })
    .await;
    now.store(62_000, Ordering::SeqCst);
    handle.touch_key("k");
    wait_for(WRITTEN, || {
        (store.api_key("k").unwrap().unwrap().last_used_at == Some(62_000)).then_some(())
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
    let (sample, overage) = sample_from_usage(&snapshot(100.0, 40.0, true));
    assert_eq!(
        (sample.five_hour, sample.seven_day, sample.observed_at),
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
        sample_from_usage(&both).1.unwrap().reset,
        Some(90_000),
        "the reset is the latest among the exhausted windows, not the earliest of all"
    );
    let (distinct, _) = sample_from_usage(&both);
    assert_eq!(
        (distinct.five_hour_reset, distinct.seven_day_reset),
        (Some(9_000), Some(90_000)),
        "each window's reset travels with its own utilization"
    );
    assert_eq!(
        (sample.fable, sample.fable_reset),
        (None, None),
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
    let (fable, _) = sample_from_usage(&fable);
    assert_eq!(
        (fable.fable, fable.fable_reset),
        (Some(0.75), Some(80_000)),
        "and the fable window is a fraction like the shared ones, with its own reset"
    );
    assert!(
        sample_from_usage(&snapshot(100.0, 40.0, false))
            .1
            .is_some_and(|o| !o.in_use),
        "without extra usage a full window is not overage"
    );
    assert!(sample_from_usage(&snapshot(12.5, 40.0, true))
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
        },
        pin: Some(pin_of(&handle).await),
        lifecycle_generation: 1,
        credential_generation: 1,
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
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 2,
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
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
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
        .apply_catalog_sync("a", &[("claude-sonnet-5".into(), false)], None, 1, "test")
        .unwrap();
    store
        .set_pool_members("p", &[("a".to_string(), 0)], "test")
        .unwrap();
    handle.reload().await.unwrap();

    let answer = handle.select(request(&handle, None, 1_000)).await.unwrap();
    assert!(
        matches!(answer.result, SelectResult::Selected(ref s) if s.account.id == "a"),
        "the id was reused, and the days of cooldown its last holder earned did not come with it"
    );
    assert!(handle.cooldowns().await.unwrap().is_empty());
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
        },
        pin: Some(pin),
        lifecycle_generation: 1,
        credential_generation: 1,
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
    };
    handle.report(Report::Overage {
        account_id: b.clone(),
        evidence: evidence(true, 1_200),
    });
    handle.report(Report::Overage {
        account_id: b.clone(),
        evidence: evidence(true, 1_300),
    });
    handle.report(Report::Overage {
        account_id: b.clone(),
        evidence: evidence(false, 1_400),
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
        matches!(next_event(&mut events).await, RouterEvent::PinExpired { session_id, .. } if session_id == "s2"),
        "and the idle pin went with it"
    );
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
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
    }
}

/// A sample that read no window; each test fills in the ones its rule turns on.
fn sample(observed_at: i64) -> UtilizationSample {
    UtilizationSample {
        fable: None,
        fable_reset: None,
        five_hour: None,
        five_hour_reset: None,
        seven_day: None,
        seven_day_reset: None,
        observed_at,
    }
}

fn observed(account: &str, sample: UtilizationSample) -> Report {
    Report::Sample {
        account_id: account.into(),
        sample,
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
    handle.report(observed(
        "a",
        UtilizationSample {
            five_hour: Some(0.4),
            ..sample(2_000)
        },
    ));
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
    handle.report(observed(
        "a",
        UtilizationSample {
            five_hour: Some(0.4),
            ..sample(1_999)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a reading from before the row was set is not evidence about it"
    );
    handle.report(observed(
        "a",
        UtilizationSample {
            five_hour: Some(0.4),
            ..sample(2_001)
        },
    ));
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
    handle.report(observed(
        "a",
        UtilizationSample {
            seven_day: Some(1.0),
            ..sample(2_000)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "a window that is exactly full has nothing left to serve with"
    );
    handle.report(observed(
        "a",
        UtilizationSample {
            five_hour: Some(0.1),
            ..sample(2_001)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "and the row names the window that is read, not whichever one recovered"
    );
    handle.report(observed(
        "a",
        UtilizationSample {
            seven_day: Some(0.99),
            ..sample(2_002)
        },
    ));
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
        UtilizationSample {
            five_hour: Some(0.4),
            seven_day: Some(1.0),
            ..sample(2_000)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "one shared window still full is enough to hold the account out"
    );
    handle.report(observed(
        "a",
        UtilizationSample {
            five_hour: Some(0.4),
            ..sample(2_001)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "and a window the sample never read is no evidence either"
    );
    handle.report(observed(
        "a",
        UtilizationSample {
            five_hour: Some(0.4),
            seven_day: Some(0.9),
            ..sample(2_002)
        },
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
    assert_eq!(handle.cooldowns().await.unwrap().len(), 2);
    handle.report(observed(
        "a",
        UtilizationSample {
            fable: Some(0.0),
            five_hour: Some(0.0),
            seven_day: Some(0.0),
            ..sample(2_000)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        2,
        "an empty account releases neither"
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
        UtilizationSample {
            five_hour: Some(0.1),
            seven_day: Some(0.1),
            ..sample(2_000)
        },
    ));
    assert_eq!(
        handle.cooldowns().await.unwrap().len(),
        1,
        "the shared windows say nothing about a model-scoped allowance"
    );
    handle.report(observed(
        "a",
        UtilizationSample {
            fable: Some(1.0),
            ..sample(2_001)
        },
    ));
    assert_eq!(handle.cooldowns().await.unwrap().len(), 1);
    handle.report(observed(
        "a",
        UtilizationSample {
            fable: Some(0.5),
            ..sample(2_002)
        },
    ));
    assert!(handle.cooldowns().await.unwrap().is_empty());
}
