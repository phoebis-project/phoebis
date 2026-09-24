//! The catalog cache: the account-scoped fill and its eviction fence, and the resolution map's probes off the
//! request path.
mod common;
use std::sync::atomic::Ordering;
use std::time::Duration;

use phoebis_bus::{Bus, Event};
use phoebis_store::{AccountKind, AccountRow, AccountState, ApiKeyRow, RulesMode, Secret};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";

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

fn json_row(id: &str) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: model_row(id).to_string().into_bytes(),
    }
}

fn error_code(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.headers()
        .get("x-phoebis-error-code")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn probes_for(up: &FakeUpstream, prefix: &str) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with(&format!("GET {prefix}")))
        .count()
}

#[tokio::test]
async fn a_fetch_is_answered_from_the_cache_inside_the_window_and_refetched_after_it() {
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
    let second = h.catalog.fetch(&a).await;
    assert_eq!(second, first, "the cache answers inside the window");
    assert_eq!(
        probes_for(&h.catalog_up, "/v1/models?limit=1000"),
        1,
        "one probe for both calls"
    );
    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    let third = h.catalog.fetch(&a).await;
    assert_eq!(third.models.len(), 2);
    assert_eq!(
        probes_for(&h.catalog_up, "/v1/models?limit=1000"),
        2,
        "past the window, a third call probes again"
    );
    let refreshed = h.catalog.refresh(&a).await;
    assert_eq!(refreshed.models.len(), 2);
    assert_eq!(
        probes_for(&h.catalog_up, "/v1/models?limit=1000"),
        3,
        "refresh probes at once"
    );
}

#[tokio::test]
async fn a_failed_fetch_stores_the_error_and_no_rows_for_the_window() {
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
    let second = h.catalog.fetch(&a).await;
    assert_eq!(second, first);
    assert_eq!(
        h.catalog_up.requests().len(),
        1,
        "a second call inside the window probes nothing"
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
    assert_eq!(h.catalog_up.requests().len(), 1, "one probe for both");
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
        "a further fetch probes again since the eviction won the commit-time fence"
    );
}

#[tokio::test]
async fn a_known_spelling_is_resolved_to_the_dated_id_off_the_request_path() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let resp1 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp1.status(), 200);
    let events1 = drain_until_complete(&mut sub).await;
    let observed1 = events1
        .iter()
        .find_map(|e| match e {
            Event::RequestObserved(o) => Some(o.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(observed1.canonical_model, "claude-sonnet-4-5");
    assert_eq!(observed1.wire_model, "claude-sonnet-4-5");
    assert!(
        String::from_utf8_lossy(&up.requests()[0].body).contains(r#""model":"claude-sonnet-4-5""#)
    );

    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-sonnet-4-5") == 1).then_some(())
    })
    .await;
    wait_for_async(Duration::from_secs(2), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.model == "claude-sonnet-4-5-20250929")
            .then_some(())
    })
    .await;

    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let events2 = drain_until_complete(&mut sub).await;
    let observed2 = events2
        .iter()
        .find_map(|e| match e {
            Event::RequestObserved(o) => Some(o.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(observed2.canonical_model, "claude-sonnet-4-5-20250929");
    assert_eq!(observed2.wire_model, "claude-sonnet-4-5");
    // Each turn injects its own prompt id and billing hash, so the bodies differ there like any two
    // requests would; the model field is the one part resolution must never touch.
    assert!(
        String::from_utf8_lossy(&up.requests()[1].body).contains(r#""model":"claude-sonnet-4-5""#)
    );
    let pins = h.router.pins().await.unwrap();
    let pin = pins
        .iter()
        .find(|p| p.model == "claude-sonnet-4-5-20250929")
        .unwrap();
    assert_eq!(
        pin.account_id, "a",
        "the same pin renews on the same account"
    );
}

#[tokio::test]
async fn a_spelling_the_api_lacks_is_asked_once_per_window() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    for _ in 0..3 {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-nope", "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-nope") == 1).then_some(())
    })
    .await;
    stays_for(Duration::from_millis(100), || {
        probes_for(&h.catalog_up, "/v1/models/claude-nope") == 1
    })
    .await;
    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-nope", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-nope") == 2).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_refused_request_enqueues_no_resolution() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["NOT claude-sonnet-4-5"]),
            ..PoolSpec::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    stays_for(Duration::from_millis(100), || {
        h.catalog_up.requests().is_empty()
    })
    .await;
}

#[tokio::test]
async fn a_refresh_answering_404_keeps_the_dated_id_and_its_denial_and_cooldown_still_apply() {
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
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
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
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-sonnet-4-5") == 1).then_some(())
    })
    .await;

    // The dated-id resolution has landed: the desktop key's pool-inherited denial now bites.
    let denied = wait_for_async(Duration::from_secs(2), || async {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
            .await
            .unwrap();
        (resp.status() == 404).then_some(error_code(&resp))
    })
    .await;
    assert_eq!(
        denied, "model_not_allowed",
        "the exact dated-id denial holds"
    );

    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    h.catalog_up
        .add_credential_rule(A, "/v1/models/claude-sonnet-4-5", json_404_plain());
    let resp = h
        .app
        .clone()
        .oneshot(request_for("phoebis-open", "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the dated id still routes; the refresh landed"
    );
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-sonnet-4-5") == 2).then_some(())
    })
    .await;
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
    h.router.report(phoebis_routing::Report::Cooldown {
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
    });
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
async fn at_most_four_probes_are_in_flight_and_a_failed_probe_waits_its_window() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let mut catalog_rules_vec = vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{\"data\":[]}".to_vec(),
        },
    }];
    for n in 1..=6 {
        catalog_rules_vec.push(Rule {
            path_prefix: format!("/v1/models/claude-x{n}"),
            respond: Respond::Gated(Box::new(json_row(&format!("claude-x{n}-20260101")))),
        });
    }
    catalog_rules_vec.push(Rule {
        path_prefix: "/v1/models/claude-nope".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    });
    let catalog_up = FakeUpstream::start(catalog_rules_vec).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    for n in 1..=6 {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, &format!("claude-x{n}"), &format!("s{n}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-x") == 4).then_some(())
    })
    .await;
    stays_for(Duration::from_millis(100), || {
        probes_for(&h.catalog_up, "/v1/models/claude-x") == 4
    })
    .await;
    h.catalog_up.release();
    // Retried until the four gated probes clear `resolving`: each poll both proves the inference request
    // still succeeds and gives a fresh admission attempt for x5 a chance to land.
    wait_for_async(Duration::from_secs(2), || async {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-x5", "s5b"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        (probes_for(&h.catalog_up, "/v1/models/claude-x5") == 1).then_some(())
    })
    .await;

    // A 503 on claude-nope, to prove the doubling backoff after a failure.
    for n in 0..3 {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-nope", &format!("t{n}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-nope") == 1).then_some(())
    })
    .await;
    stays_for(Duration::from_millis(100), || {
        probes_for(&h.catalog_up, "/v1/models/claude-nope") == 1
    })
    .await;
    h.clock.store(NOW + 61_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-nope", "u1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-nope") == 2).then_some(())
    })
    .await;
    stays_for(Duration::from_millis(100), || {
        probes_for(&h.catalog_up, "/v1/models/claude-nope") == 2
    })
    .await;
    // The window has doubled to 120s after one failure; 90s past the second probe is still inside it.
    h.clock.store(NOW + 61_000 + 90_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-nope", "u2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    stays_for(Duration::from_millis(100), || {
        probes_for(&h.catalog_up, "/v1/models/claude-nope") == 2
    })
    .await;
    h.clock.store(NOW + 61_000 + 121_000, Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-nope", "u3"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for(Duration::from_secs(2), || {
        (probes_for(&h.catalog_up, "/v1/models/claude-nope") == 3).then_some(())
    })
    .await;
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
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
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
        probes_for(&h.catalog_up, "/v1/models"),
        2,
        "the replacement's fetch adds a second probe line"
    );
}

#[tokio::test]
async fn a_catalog_probe_and_an_inference_request_never_share_a_connection() {
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
    assert_eq!(up.requests().len(), 1);

    // The request admitted above also enqueues its own background resolution, on the same fake and the
    // same account credential a plain list fetch uses; the list line is counted on its own path so that
    // probe's traffic never masks what this test is about.
    let a = h.store.account("a").unwrap().unwrap();
    let fetched = h.catalog.fetch(&a).await;
    assert_eq!(fetched.models.len(), 2);
    assert_eq!(
        probes_for(&h.catalog_up, "/v1/models?limit=1000"),
        1,
        "the list line lands on the catalog fake"
    );
    assert_eq!(
        up.requests().len(),
        1,
        "the probe never rode the inference connection"
    );

    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    assert_eq!(up.requests().len(), 2);
    assert_eq!(
        probes_for(&h.catalog_up, "/v1/models?limit=1000"),
        1,
        "the inference request never rode the probe's connection"
    );
}
