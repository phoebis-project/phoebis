//! The catalog cache: the account-scoped fill and its eviction fence, and the derivation that folds an undated
//! spelling to the dated id the listings carry, off the request path.
mod common;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_relay::{Warmth, ERROR_HOLD, MAX_BODY};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, NewBundle, RekeyWrite,
    ResolutionRow, RulesMode, Secret, StateCause, StorageKey, Store,
};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";

fn model_row(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "display_name": "",
        "created_at": "",
        "max_input_tokens": 1_000_000,
        "max_tokens": null,
    })
}

fn json_list(ids: &[&str]) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: serde_json::json!({ "data": ids.iter().map(|id| model_row(id)).collect::<Vec<_>>() })
            .to_string()
            .into_bytes(),
    }
}

/// Steps the runtime's clock past the error hold, so a retry sleeping it wakes at once: the retry's task registers
/// its sleep against the paused clock on its next poll, which the yields give it before the jump.
async fn past_the_error_hold() {
    tokio::time::pause();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(ERROR_HOLD + Duration::from_secs(1)).await;
    tokio::time::resume();
}

fn error_code(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.headers()
        .get("x-phoebis-error-code")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn an_old_slot_answers_fetch_and_the_models_listing_until_a_refresh_reads_live() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[api_key("a", "sk-ant-api03-A", 0)],
        PoolSpec::default(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    let first = h.catalog.fetch(&a).await;
    assert_eq!(first.models.len(), 2);
    assert!(first.error.is_none());
    assert_eq!(
        listings_for(&h.catalog_up),
        1,
        "the startup fetch filled the slot"
    );
    // A day past the read, far longer than a failed read is held.
    h.clock.store(NOW + 86_400_000, Ordering::SeqCst);
    let later = h.catalog.fetch(&a).await;
    assert_eq!(later, first, "the slot answers whatever its age");
    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        listings_for(&h.catalog_up),
        1,
        "neither the fetch nor the models listing read the upstream"
    );
    let refreshed = h.catalog.refresh(&a).await;
    assert_eq!(
        (refreshed.models.len(), refreshed.fetched_at),
        (2, NOW + 86_400_000)
    );
    assert_eq!(listings_for(&h.catalog_up), 2, "refresh reads live");
}

#[tokio::test]
async fn a_failed_fetch_holds_its_error_for_the_error_hold_and_reads_again_after_it() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[api_key("a", "sk-ant-api03-A", 0)],
        PoolSpec::default(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    let first = h.catalog.fetch(&a).await;
    assert!(first.models.is_empty());
    assert_eq!(
        first.error.as_deref(),
        Some("the models endpoint answered 503")
    );
    let hold = ERROR_HOLD.as_millis() as i64;
    h.clock.store(NOW + hold - 1, Ordering::SeqCst);
    let second = h.catalog.fetch(&a).await;
    assert_eq!(second, first);
    assert_eq!(
        h.catalog_up.requests().len(),
        1,
        "inside the hold the error answers with no request"
    );
    h.clock.store(NOW + hold, Ordering::SeqCst);
    let third = h.catalog.fetch(&a).await;
    assert_eq!(third.fetched_at, NOW + hold);
    assert_eq!(
        h.catalog_up.requests().len(),
        2,
        "at the hold's end the slot is a miss and the account is read again"
    );
}

#[tokio::test]
async fn a_body_over_the_bound_fails_the_fetch_and_caches_no_rows() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: vec![b'a'; MAX_BODY + 1],
        },
    }])
    .await;
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[api_key("a", "sk-ant-api03-A", 0)],
        PoolSpec::default(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    let fetched = h.catalog.fetch(&a).await;
    assert!(fetched.models.is_empty());
    assert_eq!(
        fetched.error.as_deref(),
        Some(
            format!("the models response is malformed: response body exceeds {MAX_BODY} bytes")
                .as_str()
        )
    );
}

#[tokio::test]
async fn a_disconnected_accounts_fetch_answers_no_credential_without_reaching_the_catalog_fake() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&["claude-sonnet-4-5-20250929"]),
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[], PoolSpec::default()).await;
    let disconnected = test_account_row(&api_key("d", "unused", 0));
    h.store.insert_account(&disconnected).unwrap();
    let fetched = h.catalog.fetch(&disconnected).await;
    assert!(fetched.models.is_empty());
    assert_eq!(
        fetched.error.as_deref(),
        Some("the account has no credential")
    );
    assert_eq!(
        h.catalog_up.requests().len(),
        0,
        "no credential means no request ever reaches the catalog fake"
    );
}

#[tokio::test]
async fn two_fetches_during_one_fill_share_it() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(json_list(&["claude-opus-5"]))),
    }])
    .await;
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[api_key("a", "sk-ant-api03-A", 0)],
        PoolSpec::default(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    let c1 = h.catalog.clone();
    let a1 = a.clone();
    let fut1 = tokio::spawn(async move { c1.fetch(&a1).await });
    let c2 = h.catalog.clone();
    let a2 = a.clone();
    let fut2 = tokio::spawn(async move { c2.fetch(&a2).await });
    h.catalog_up.release();
    let (r1, r2) = tokio::join!(fut1, fut2);
    let (r1, r2) = (r1.unwrap(), r2.unwrap());
    assert_eq!(r1, r2);
    assert_eq!(r1.models.len(), 1);
    assert_eq!(h.catalog_up.requests().len(), 1, "one fetch for both");
}

#[tokio::test]
async fn an_eviction_during_a_slow_fetch_wins() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(json_list(&["claude-opus-5"]))),
    }])
    .await;
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[api_key("a", "sk-ant-api03-A", 0)],
        PoolSpec::default(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    let c = h.catalog.clone();
    let a1 = a.clone();
    let fut = tokio::spawn(async move { c.fetch(&a1).await });
    wait_for(Duration::from_secs(2), || {
        (h.catalog_up.requests().len() == 1).then_some(())
    })
    .await;
    h.catalog.evict("a");
    h.catalog_up.release();
    let answered = fut.await.unwrap();
    assert_eq!(
        answered.models.len(),
        1,
        "the waiting fetch still gets the rows once"
    );
    let again = h.catalog.fetch(&a).await;
    assert_eq!(again.models.len(), 1);
    assert_eq!(
        h.catalog_up.requests().len(),
        2,
        "the eviction won the commit-time fence, so the rows land through a second fetch"
    );
}

#[tokio::test]
async fn a_dropped_fetch_future_lowers_the_in_flight_count_so_a_later_miss_does_not_wait() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(json_list(&["claude-opus-5"]))),
    }])
    .await;
    // Kept out of the harness's own accounts, so the caller below is the first and only fetch for it: the
    // startup prefetch must not already hold the raise this test means to drop.
    let h = harness_with_catalog(&up, catalog_up, &[], PoolSpec::default()).await;
    h.store
        .insert_account(&test_account_row(&api_key("a", "sk-ant-api03-A", 0)))
        .unwrap();
    h.store
        .set_api_key_secret("a", &Secret::new("sk-ant-api03-A"))
        .unwrap();
    let a = h.store.account("a").unwrap().unwrap();
    let c = h.catalog.clone();
    let a1 = a.clone();
    let fut = tokio::spawn(async move { c.fetch(&a1).await });
    wait_for(Duration::from_secs(2), || {
        (h.catalog_up.requests().len() == 1).then_some(())
    })
    .await;
    // The client went away mid-fill, as the catalog popup and `GET /v1/models` both can.
    fut.abort();
    let _ = fut.await;
    h.catalog_up.release();
    let start = std::time::Instant::now();
    let canonical = h.catalog.canonical_for("claude-nope").await;
    assert_eq!(canonical, "claude-nope");
    assert!(
        start.elapsed() < STARTUP_BOUND,
        "the guard lowered the count on the drop, so this miss did not wait the bound"
    );
    assert_ne!(
        h.catalog.warmth(),
        Warmth::Filling,
        "the count returned to zero rather than sticking above it"
    );
}

#[tokio::test]
async fn a_spelling_is_named_by_its_dated_id_from_the_first_request_with_no_request_beyond_the_startup_fetch(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = drain_until_complete(&mut sub).await;
    let observed = events
        .iter()
        .find_map(|e| match e {
            Event::RequestObserved(o) => Some(o.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(observed.canonical_model, "claude-sonnet-4-5-20250929");
    assert_eq!(observed.wire_model, "claude-sonnet-4-5");
    assert!(
        String::from_utf8_lossy(&up.requests()[0].body).contains(r#""model":"claude-sonnet-4-5""#)
    );
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));
    assert_eq!(listings_for(&h.catalog_up), 1);
    assert!(
        h.catalog_up
            .requests()
            .iter()
            .all(|r| !r.line.starts_with("GET /v1/models/")),
        "no per-spelling request is ever sent"
    );
}

#[tokio::test]
async fn a_dateless_id_the_listing_holds_and_a_spelling_it_lacks_both_resolve_to_themselves() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    for model in ["claude-opus-5", "claude-nope"] {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, model, "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let events = drain_until_complete(&mut sub).await;
        let observed = events
            .iter()
            .find_map(|e| match e {
                Event::RequestObserved(o) => Some(o.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(observed.canonical_model, model);
    }
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 1
    })
    .await;
}

#[tokio::test]
async fn the_newest_dated_entry_wins_end_to_end() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&["claude-sonnet-4-5-20250601", "claude-sonnet-4-5-20250929"]),
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929")
    );
}

#[tokio::test]
async fn the_first_request_after_a_restart_carries_the_dated_id_from_the_persisted_mapping_when_the_fake_is_down(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .rekey_resolution(&RekeyWrite {
            resolution: ResolutionRow {
                spelling: "claude-sonnet-4-5".into(),
                dated_id: "claude-sonnet-4-5-20250929".into(),
                resolved_at: NOW,
            },
            delete_pins: Vec::new(),
            upsert_pins: Vec::new(),
            delete_cooldowns: Vec::new(),
            upsert_cooldowns: Vec::new(),
        })
        .unwrap();
    let h = harness_with_store_and_catalog(
        store,
        &up,
        catalog_up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(h.catalog.warmth(), Warmth::Cold);
    let start = std::time::Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "the persisted mapping answers with no wait"
    );
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));
}

#[tokio::test]
async fn a_cold_services_request_waits_the_bound_then_falls_back_to_the_spelling() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(json_list(&["claude-sonnet-4-5-20250929"]))),
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let start = std::time::Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let elapsed = start.elapsed();
    assert!(
        elapsed >= STARTUP_BOUND,
        "{elapsed:?} did not reach the bound"
    );
    assert!(
        elapsed < STARTUP_BOUND * 2,
        "{elapsed:?} waited past twice the bound"
    );
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .all(|p| p.model != "claude-sonnet-4-5-20250929"),
        "the fallback named the spelling, so nothing pinned the dated id yet"
    );
    h.catalog_up.release();
    wait_for(Duration::from_secs(2), || {
        (h.catalog.warmth() == Warmth::Warm).then_some(())
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));
}

#[tokio::test]
async fn a_failed_startup_fetch_answers_at_once_and_is_retried_in_the_background() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        json_list(&["claude-sonnet-4-5-20250929"]),
    );
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let start = std::time::Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(start.elapsed() < Duration::from_millis(200));
    past_the_error_hold().await;
    wait_for(Duration::from_secs(2), || {
        (h.catalog.warmth() == Warmth::Warm).then_some(())
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));
    assert_eq!(listings_for(&h.catalog_up), 2);
}

#[tokio::test]
async fn a_startup_fetch_still_pending_at_the_bound_is_retried_once_it_lands_an_error() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        })),
    }])
    .await;
    catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        json_list(&["claude-sonnet-4-5-20250929"]),
    );
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(
        h.catalog.warmth(),
        Warmth::Filling,
        "still gated past the startup bound"
    );
    h.catalog_up.release();
    // The gated answer's error lands first; the retry it starts then sleeps the hold.
    wait_for(Duration::from_secs(2), || {
        (h.catalog.warmth() == Warmth::Cold).then_some(())
    })
    .await;
    past_the_error_hold().await;
    wait_for(Duration::from_secs(2), || {
        (h.catalog.warmth() == Warmth::Warm).then_some(())
    })
    .await;
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929"),
        "the retry landed once the gated answer's error was seen"
    );
    assert_eq!(
        listings_for(&h.catalog_up),
        2,
        "the gated answer, then one retry, with no request in between"
    );
}

#[tokio::test]
async fn a_listing_that_drops_the_dated_entry_keeps_the_dated_id_and_its_denial_and_cooldown_still_apply(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["NOT claude-sonnet-4-5-20250929"]),
            ..PoolSpec::default()
        },
    )
    .await;
    h.store
        .insert_api_key(
            &ApiKeyRow {
                id: "open".into(),
                label: "open".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: Vec::new(),
                model_rules_mode: RulesMode::Replace,
                long_context_twins: true,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: h.store.api_key_digest("phoebis-open").to_vec(),
                prefix: phoebis_store::key_prefix("phoebis-open"),
                created_at: NOW,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();

    let resp = h
        .app
        .clone()
        .oneshot(request_for("phoebis-open", "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the dated id resolved before this request"
    );

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(
        error_code(&resp),
        "model_not_allowed",
        "the exact dated-id denial holds"
    );

    // A listing that no longer carries the dated entry never takes the resolution back.
    h.catalog_up
        .set_rule_after(1, "/v1/models?limit=1000", json_list(&[]));
    let a = h.store.account("a").unwrap().unwrap();
    h.catalog.refresh(&a).await;
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929"),
        "an empty listing keeps the dated id"
    );
    // Nor does a refresh that fails outright.
    h.catalog_up.set_rule_after(
        2,
        "/v1/models?limit=1000",
        Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    h.catalog.refresh(&a).await;
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929"),
        "a failed refresh keeps the dated id too"
    );

    let resp = h
        .app
        .clone()
        .oneshot(request_for("phoebis-open", "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the dated id still routes; the resolution held"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(
        error_code(&resp),
        "model_not_allowed",
        "still denied under the kept dated id"
    );

    let now = h.clock.load(Ordering::SeqCst);
    h.router
        .report(phoebis_routing::Report::Cooldown {
            account_id: "a".into(),
            row: phoebis_store::CooldownRow {
                account_id: "a".into(),
                scope: "model:claude-sonnet-4-5-20250929".into(),
                until: now + 3_600_000,
                set_at: now,
                reason: phoebis_routing::MODEL_REJECTED_REASON.into(),
                window: "model_rejection".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    let before = up.requests().len();
    let resp = h
        .app
        .clone()
        .oneshot(request_for("phoebis-open", "claude-sonnet-4-5", "s3"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(error_code(&resp), "model_rejected");
    assert_eq!(up.requests().len(), before, "no upstream call");
}

#[tokio::test]
async fn a_move_the_router_never_acknowledged_stays_unpublished() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&[]),
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    h.catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        json_list(&["claude-sonnet-4-5-20250929"]),
    );
    h.router.shutdown().await.unwrap();
    let a = h.store.account("a").unwrap().unwrap();
    h.catalog.refresh(&a).await;
    assert_eq!(h.catalog.lookup("claude-sonnet-4-5"), None);
    assert!(h.store.resolutions().unwrap().is_empty());
}

#[tokio::test]
async fn a_router_already_past_the_catalogs_own_bookkeeping_is_never_walked_back() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&["claude-sonnet-4-5-20250601"]),
    }])
    .await;
    // Kept out of the harness's own accounts, so nothing publishes for this spelling before the router's own
    // mapping is set directly below: the catalog's `published` map starts this test with no entry to compare
    // against, standing in for a publication whose acknowledgement never reached it.
    let h = harness_with_catalog(&up, catalog_up, &[], PoolSpec::default()).await;
    h.store
        .insert_account(&test_account_row(&api_key("a", "sk-ant-api03-A", 0)))
        .unwrap();
    h.store
        .set_api_key_secret("a", &Secret::new("sk-ant-api03-A"))
        .unwrap();
    let a = h.store.account("a").unwrap().unwrap();
    h.router
        .resolve("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", NOW)
        .await
        .unwrap();
    h.catalog.fetch(&a).await;
    assert_eq!(
        h.router
            .canonicals(vec!["claude-sonnet-4-5".into()])
            .await
            .unwrap(),
        vec!["claude-sonnet-4-5-20250929".to_string()],
        "an older listing derived without the router's own advance in view moves nothing"
    );
}

#[tokio::test]
async fn two_fills_landing_together_end_on_the_newer_snapshot_whichever_is_acknowledged_first() {
    for gated in [A, B] {
        let up = FakeUpstream::start(vec![]).await;
        let catalog_up = FakeUpstream::start(vec![]).await;
        let dates = [
            (A, "claude-sonnet-4-5-20250601"),
            (B, "claude-sonnet-4-5-20250929"),
        ];
        for (credential, dated) in dates {
            let respond = json_list(&[dated]);
            let respond = if credential == gated {
                Respond::Gated(Box::new(respond))
            } else {
                respond
            };
            catalog_up.push_credential_rule(credential, "/v1/models?limit=1000", respond);
        }
        let h = harness_with_catalog(
            &up,
            catalog_up,
            &[oauth("a", A, 0), oauth("b", B, 1)],
            PoolSpec::default(),
        )
        .await;
        let immediate_dated = if gated == A {
            "claude-sonnet-4-5-20250929"
        } else {
            "claude-sonnet-4-5-20250601"
        };
        assert_eq!(
            h.catalog.lookup("claude-sonnet-4-5").as_deref(),
            Some(immediate_dated),
            "the account that was not gated landed alone first"
        );
        h.catalog_up.release();
        // `listings_for` alone only proves the gated request was sent, not that its answer landed and
        // published; the lookup itself is the signal that the newer snapshot has actually moved.
        wait_for(Duration::from_secs(2), || {
            (h.catalog.lookup("claude-sonnet-4-5").as_deref() == Some("claude-sonnet-4-5-20250929"))
                .then_some(())
        })
        .await;
        assert_eq!(
            listings_for(&h.catalog_up),
            2,
            "both accounts' listings landed: {gated}"
        );
        let resolutions = h.store.resolutions().unwrap();
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].dated_id, "claude-sonnet-4-5-20250929");
        assert_eq!(
            h.router
                .canonicals(vec!["claude-sonnet-4-5".into()])
                .await
                .unwrap(),
            vec!["claude-sonnet-4-5-20250929".to_string()]
        );
    }
}

#[tokio::test]
async fn a_persisted_newer_mapping_is_not_moved_back_by_an_older_startup_listing() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&["claude-sonnet-4-5-20250929"]),
    }])
    .await;
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .rekey_resolution(&RekeyWrite {
            resolution: ResolutionRow {
                spelling: "claude-sonnet-4-5".into(),
                dated_id: "claude-sonnet-4-5-20251101".into(),
                resolved_at: NOW,
            },
            delete_pins: Vec::new(),
            upsert_pins: Vec::new(),
            delete_cooldowns: Vec::new(),
            upsert_cooldowns: Vec::new(),
        })
        .unwrap();
    let h = harness_with_store_and_catalog(
        store,
        &up,
        catalog_up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(h.catalog.warmth(), Warmth::Warm);
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20251101")
    );
    let resolutions = h.store.resolutions().unwrap();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(resolutions[0].dated_id, "claude-sonnet-4-5-20251101");
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20251101"));
}

#[tokio::test]
async fn a_request_waiting_on_a_gated_listing_is_named_by_the_dated_id_once_the_listing_lands() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(json_list(&["claude-sonnet-4-5-20250929"]))),
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let app = h.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    stays_for(Duration::from_millis(100), || !task.is_finished()).await;
    h.catalog_up.release();
    let resp = task.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        String::from_utf8_lossy(&up.requests()[0].body).contains(r#""model":"claude-sonnet-4-5""#)
    );
    // The pin sits under the dated id only because the in-flight count is lowered after the acknowledged
    // publication; the listing gate stands in for holding the router's own acknowledgement.
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));
}

#[tokio::test]
async fn a_fast_unrelated_listing_does_not_release_a_request_the_gated_account_would_resolve() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![]).await;
    catalog_up.push_credential_rule(B, "/v1/models?limit=1000", json_list(&["claude-opus-5"]));
    catalog_up.push_credential_rule(
        A,
        "/v1/models?limit=1000",
        Respond::Gated(Box::new(json_list(&["claude-sonnet-4-5-20250929"]))),
    );
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(h.catalog.warmth(), Warmth::Warm, "b's listing landed");
    let app = h.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    stays_for(Duration::from_millis(100), || !task.is_finished()).await;
    h.catalog_up.release();
    let resp = task.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));
}

#[tokio::test]
async fn a_dated_id_and_an_exactly_listed_spelling_skip_the_miss_wait_but_an_unlisted_one_still_waits(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![]).await;
    catalog_up.push_credential_rule(
        B,
        "/v1/models?limit=1000",
        json_list(&["claude-opus-5", "claude-sonnet-4-5-20250929"]),
    );
    catalog_up.push_credential_rule(
        A,
        "/v1/models?limit=1000",
        Respond::Gated(Box::new(json_list(&["claude-sonnet-4-5-20251101"]))),
    );
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(h.catalog.warmth(), Warmth::Warm, "b's listing landed");
    for spelling in ["claude-opus-5", "claude-sonnet-4-5-20250929"] {
        let start = std::time::Instant::now();
        let canonical = h.catalog.canonical_for(spelling).await;
        assert_eq!(canonical, spelling);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "{spelling} is already the union's own entry and does not wait on a's gated fill"
        );
    }
    let start = std::time::Instant::now();
    let canonical = h.catalog.canonical_for("claude-nope").await;
    assert_eq!(canonical, "claude-nope");
    assert!(
        start.elapsed() >= STARTUP_BOUND,
        "a spelling the union has never listed still waits the bound"
    );
    h.catalog_up.release();
}

#[tokio::test]
async fn a_retry_sleeping_its_backoff_ends_at_shutdown() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), h.catalog.shutdown())
            .await
            .is_ok(),
        "the retry, sleeping a minute-long backoff, ended at shutdown rather than outliving it"
    );
    stays_for(Duration::from_millis(300), || {
        listings_for(&h.catalog_up) == 1
    })
    .await;
}

#[tokio::test]
async fn a_failed_fetch_is_retried_first_after_the_error_hold() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        json_list(&["claude-sonnet-4-5-20250929"]),
    );
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5"),
        None,
        "the first fetch failed"
    );
    tokio::time::pause();
    // Gives the retry's own task its first poll (registering its sleep) against the now-frozen clock before
    // the jump below, since nothing has awaited anything since it was spawned.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(ERROR_HOLD - Duration::from_secs(1)).await;
    tokio::time::resume();
    stays_for(Duration::from_millis(300), || {
        listings_for(&h.catalog_up) == 1
    })
    .await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    wait_for(Duration::from_secs(2), || {
        (h.catalog.warmth() == Warmth::Warm).then_some(())
    })
    .await;
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929"),
        "the retry landed once the hold had passed"
    );
    assert_eq!(listings_for(&h.catalog_up), 2);
}

#[tokio::test]
async fn a_catalog_fetch_and_an_inference_request_never_share_a_connection() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    assert_eq!(
        up.requests().len(),
        0,
        "the fetch never rode the inference fake"
    );

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.requests().len(), 1);
    assert_eq!(
        listings_for(&h.catalog_up),
        1,
        "the inference request never rode the fetch's connection"
    );

    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    assert_eq!(up.requests().len(), 2);
    assert_eq!(listings_for(&h.catalog_up), 1);
}

#[tokio::test]
async fn a_replacement_account_under_the_same_id_reads_its_own_catalog() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let a = h.store.account("a").unwrap().unwrap();
    let first = h.catalog.fetch(&a).await;
    assert_eq!(first.models.len(), 2);

    h.store.delete_account("a", "test").unwrap();
    h.store
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
            state_cause: StateCause::Operator,
        })
        .unwrap();
    h.store.set_api_key_secret("a", &Secret::new("k2")).unwrap();
    h.catalog_up
        .add_credential_rule("k2", "/v1/models", json_list(&["claude-vendor-1"]));
    h.router.reload().await.unwrap();

    let new_row = h.store.account("a").unwrap().unwrap();
    let second = h.catalog.fetch(&new_row).await;
    assert_eq!(second.models.len(), 1);
    assert_eq!(second.models[0].id, "claude-vendor-1");
    assert_ne!(
        first.models, second.models,
        "no row of the old holder answers the new one"
    );
}

#[tokio::test]
async fn a_gated_fill_whose_account_was_replaced_commits_nothing() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![]).await;
    catalog_up.add_credential_rule(
        A,
        "/v1/models",
        Respond::Gated(Box::new(json_list(&[
            "claude-sonnet-4-5-20250929",
            "claude-opus-5",
        ]))),
    );
    let h = harness_with_catalog(&up, catalog_up, &[], PoolSpec::default()).await;
    // Inserted after the build: over zero accounts, the startup fetch touches nothing, so the fetch this test
    // drives by hand is the only one racing the account's replacement.
    h.store
        .insert_account(&test_account_row(&oauth("a", A, 0)))
        .unwrap();
    h.store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new(A),
                refresh_token: Some(Secret::new(&format!("{A}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    let old_row = h.store.account("a").unwrap().unwrap();
    let c = h.catalog.clone();
    let old = old_row.clone();
    let fut = tokio::spawn(async move { c.fetch(&old).await });
    wait_for(Duration::from_secs(2), || {
        (h.catalog_up.requests().len() == 1).then_some(())
    })
    .await;

    h.store.delete_account("a", "test").unwrap();
    h.store
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
            state_cause: StateCause::Operator,
        })
        .unwrap();
    h.store.set_api_key_secret("a", &Secret::new("k2")).unwrap();
    h.catalog_up
        .add_credential_rule("k2", "/v1/models", json_list(&["claude-vendor-1"]));
    h.catalog_up.release();

    let answered = fut.await.unwrap();
    assert_eq!(
        answered.models.len(),
        2,
        "the spawned fetch answers the old rows once"
    );

    let new_row = h.store.account("a").unwrap().unwrap();
    let second = h.catalog.fetch(&new_row).await;
    assert_eq!(second.models.len(), 1);
    assert_eq!(second.models[0].id, "claude-vendor-1");
    assert_eq!(
        listings_for(&h.catalog_up),
        2,
        "the replacement's fetch adds a second listing line"
    );
}

#[tokio::test]
async fn a_failed_fill_slower_than_the_error_hold_is_shared_by_every_caller_waiting_on_it() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        })),
    }])
    .await;
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[api_key("a", "sk-ant-api03-A", 0)],
        PoolSpec::default(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    let callers: Vec<_> = (0..3)
        .map(|_| {
            let (catalog, account) = (h.catalog.clone(), a.clone());
            tokio::spawn(async move { catalog.fetch(&account).await })
        })
        .collect();
    wait_for(Duration::from_secs(2), || {
        (h.catalog_up.requests().len() == 1).then_some(())
    })
    .await;
    // The discovery answers a second past the hold after it left.
    h.clock
        .fetch_add(ERROR_HOLD.as_millis() as i64 + 1_000, Ordering::SeqCst);
    h.catalog_up.release();
    let mut answers = Vec::new();
    for caller in callers {
        answers.push(caller.await.unwrap());
    }
    assert_eq!(
        h.catalog_up.requests().len(),
        1,
        "the callers behind the fill lock read the fill rather than discovering again"
    );
    assert!(
        answers
            .iter()
            .all(|answer| answer == &answers[0] && answer.error.is_some()),
        "all three share the one answer: {answers:?}"
    );
}

#[tokio::test]
async fn the_periodic_refresh_fires_on_the_stepped_clock() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 1
    })
    .await;
    h.clock.store(NOW + 21_601_000, Ordering::SeqCst);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 2).then_some(())
    })
    .await;
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 2
    })
    .await;
}

#[tokio::test]
async fn the_scheduled_refresh_replaces_the_slot_and_fetch_answers_the_replacement() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    h.clock.store(NOW + 21_601_000, Ordering::SeqCst);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 2).then_some(())
    })
    .await;
    // A minute and more past the scheduled read, and far short of the next one.
    h.clock.store(NOW + 21_662_000, Ordering::SeqCst);
    let a = h.store.account("a").unwrap().unwrap();
    let held = h.catalog.fetch(&a).await;
    assert_eq!(
        held.fetched_at,
        NOW + 21_601_000,
        "the slot the scheduled refresh left answers"
    );
    assert_eq!(
        listings_for(&h.catalog_up),
        2,
        "no read past the scheduled one"
    );
}

#[tokio::test]
async fn a_refresh_that_moves_a_spelling_to_a_newer_snapshot_rekeys_through_the_acknowledged_command(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20250929"));

    h.catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        json_list(&["claude-sonnet-4-5-20250929", "claude-sonnet-4-5-20251101"]),
    );
    let a = h.store.account("a").unwrap().unwrap();
    h.catalog.refresh(&a).await;

    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20251101")
    );
    // The acknowledgement moves the lookup at once; the persisted row lands behind it on the router's writer.
    wait_for(Duration::from_secs(2), || {
        let rows = h.store.resolutions().unwrap();
        (rows.len() == 1 && rows[0].dated_id == "claude-sonnet-4-5-20251101").then_some(())
    })
    .await;
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.model == "claude-sonnet-4-5-20250929"),
        "s1's pin stays under the snapshot it was made on; a newer one is a different model"
    );

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h
        .router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.model == "claude-sonnet-4-5-20251101"));
}

#[tokio::test]
async fn a_response_naming_another_snapshot_refreshes_the_account_once_per_minute() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-5-20251101","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
        },
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 2).then_some(())
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 2
    })
    .await;

    // The one refresh already proved the listing never carries the served id, so the remembered mismatch
    // skips a second refresh even once the minute has passed.
    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 2
    })
    .await;

    // The same through a streamed answer whose `message_start` names the other snapshot.
    let stream_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: vec![
                SseEvent {
                    event: "message_start".into(),
                    data: r#"{"type":"message_start","message":{"id":"msg_2","model":"claude-sonnet-4-5-20251101","usage":{"input_tokens":1}}}"#.into(),
                },
                SseEvent {
                    event: "message_stop".into(),
                    data: r#"{"type":"message_stop"}"#.into(),
                },
            ],
            delay_between: Duration::from_millis(0),
        },
    }])
    .await;
    let h2 = harness(&stream_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h2.catalog_up), 1, "the startup fetch alone");
    let resp = h2.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h2.catalog_up) == 2).then_some(())
    })
    .await;
}

#[tokio::test]
async fn three_mismatches_while_the_first_refresh_is_gated_cost_one_request_then_one_more_past_the_minute(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-5-20251101","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
        },
    }])
    .await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&["claude-sonnet-4-5-20250929"]),
    }])
    .await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    h.catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        Respond::Gated(Box::new(json_list(&[
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20251101",
        ]))),
    );

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 2).then_some(())
    })
    .await;

    // Two more mismatching answers land while that refresh is still gated: the reservation the first one
    // took out must still stand, so neither starts a request of its own.
    for session in ["s2", "s3"] {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", session))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        stays_for(Duration::from_millis(100), || {
            listings_for(&h.catalog_up) == 2
        })
        .await;
    }

    h.catalog_up.release();
    wait_for(Duration::from_secs(2), || {
        (h.catalog.lookup("claude-sonnet-4-5").as_deref() == Some("claude-sonnet-4-5-20251101"))
            .then_some(())
    })
    .await;

    // Past the minute, a fresh mismatch (the alias having just moved again) costs one more request.
    h.clock.fetch_add(61_000, Ordering::SeqCst);
    up.set_rule_after(
        3,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_2","type":"message","model":"claude-sonnet-4-5-20251201","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
        },
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s4"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 3).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_live_alias_move_resolves_after_one_refresh_and_further_answers_naming_it_trigger_nothing(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-5-20251101","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
        },
    }])
    .await;
    let catalog_up = FakeUpstream::start(catalog_rules()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    // The triggered refresh fails first: nothing about a failed refresh is remembered, so the mismatch is
    // still live once the interval opens again.
    h.catalog_up.set_rule_after(
        1,
        "/v1/models?limit=1000",
        Respond::Json {
            status: 500,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 2).then_some(())
    })
    .await;
    assert_eq!(
        h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929"),
        "the failed refresh moved nothing"
    );

    // The account's listing catches up to the served snapshot by the time the next answer past the minute
    // triggers a second refresh, since the failed first one left nothing remembered to skip it.
    h.catalog_up.set_rule_after(
        2,
        "/v1/models?limit=1000",
        json_list(&[
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20251101",
            "claude-opus-5",
        ]),
    );
    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (h.catalog.lookup("claude-sonnet-4-5").as_deref() == Some("claude-sonnet-4-5-20251101"))
            .then_some(())
    })
    .await;
    assert_eq!(
        listings_for(&h.catalog_up),
        3,
        "the second refresh landed the moved alias"
    );

    // The alias is resolved now, so a further answer naming the same snapshot is the ordinary match case.
    for _ in 0..2 {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 3
    })
    .await;
}

#[tokio::test]
async fn two_spellings_one_account_can_never_resolve_cost_one_refresh_a_minute_then_none() {
    let served_a = "claude-sonnet-4-5-20260101";
    let served_b = "claude-opus-5-20260101";
    let body_naming = |msg: &str, model: &str| -> Vec<u8> {
        format!(
            r#"{{"id":"{msg}","type":"message","model":"{model}","usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        )
        .into_bytes()
    };
    let up = FakeUpstream::start(vec![]).await;
    // Four requests, queued in the order the test sends them below: A, B, then A and B once each is
    // remembered, so a request reusing a session still names the pair it started with.
    for (msg, model) in [
        ("msg_1", served_a),
        ("msg_2", served_b),
        ("msg_3", served_a),
        ("msg_4", served_b),
    ] {
        up.push_credential_rule(
            A,
            "/v1/messages",
            Respond::Json {
                status: 200,
                headers: vec![],
                body: body_naming(msg, model),
            },
        );
    }
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");

    // The first minute: A's mismatch costs the account's one refresh, which cannot resolve it, since the
    // default listing never carries `served_a`.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 2).then_some(())
    })
    .await;

    // The second minute: B's mismatch, seen for the first time, still costs only the one refresh the
    // interval allows, and that refresh re-judges A's carried triple alongside B's rather than losing it.
    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (listings_for(&h.catalog_up) == 3).then_some(())
    })
    .await;
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 3
    })
    .await;

    // The third minute: both triples are already remembered, so neither mismatch costs a fetch.
    h.clock.store(NOW + 122_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 3
    })
    .await;
}

#[tokio::test]
async fn a_response_naming_the_resolved_id_triggers_no_refresh() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-5-20250929","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
        },
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 1
    })
    .await;
}

#[tokio::test]
async fn a_cancelled_stream_with_a_message_start_naming_another_snapshot_triggers_no_refresh() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::SseGated {
            status: 200,
            headers: vec![],
            events: vec![
                SseEvent {
                    event: "message_start".into(),
                    data: r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5-20251101","usage":{"input_tokens":1}}}"#.into(),
                },
                SseEvent {
                    event: "message_stop".into(),
                    data: r#"{"type":"message_stop"}"#.into(),
                },
            ],
        },
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    assert_eq!(listings_for(&h.catalog_up), 1, "the startup fetch alone");
    // The response is held until the first SSE event, so the head commits as soon as the fake's gated write
    // delivers `message_start`, well before the account is deleted below.
    let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the head and the first event were already committed"
    );
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    let _ = resp.into_body().collect().await;
    stays_for(Duration::from_millis(100), || {
        listings_for(&h.catalog_up) == 1
    })
    .await;
    up.release();
}

#[tokio::test]
async fn a_listing_that_changed_calls_the_hook_with_what_came_and_went() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: json_list(&["claude-opus-5", "claude-sonnet-4-5-20250929"]),
    }])
    .await;
    // No account at the start, so no listing lands before the hook is set.
    let h = harness_with_catalog(&up, catalog_up, &[], PoolSpec::default()).await;
    let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
    h.catalog.set_on_change(Arc::new({
        let changes = changes.clone();
        move |change| changes.lock().unwrap().push(change)
    }));
    h.store
        .insert_account(&test_account_row(&api_key("a", "sk-ant-api03-A", 0)))
        .unwrap();
    h.store
        .set_api_key_secret("a", &Secret::new("sk-ant-api03-A"))
        .unwrap();
    let a = h.store.account("a").unwrap().unwrap();
    assert_eq!(h.catalog.fetch(&a).await.models.len(), 2);
    assert!(
        changes.lock().unwrap().is_empty(),
        "the first listing has none before it to differ from"
    );

    h.clock.store(NOW + 1_000, Ordering::SeqCst);
    h.catalog_up.set_rule_after(
        h.catalog_up.requests().len(),
        "/v1/models?limit=1000",
        json_list(&["claude-haiku-5", "claude-opus-5"]),
    );
    h.catalog.refresh(&a).await;
    let expected = vec![phoebis_relay::CatalogChange {
        account_id: "a".into(),
        added: vec!["claude-haiku-5".into()],
        removed: vec!["claude-sonnet-4-5-20250929".into()],
        count: 2,
        at: NOW + 1_000,
    }];
    assert_eq!(*changes.lock().unwrap(), expected);
    h.catalog.refresh(&a).await;
    assert_eq!(
        *changes.lock().unwrap(),
        expected,
        "the same listing again changes nothing"
    );
}
