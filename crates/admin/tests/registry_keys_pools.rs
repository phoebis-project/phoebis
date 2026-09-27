mod common;
use phoebis_store::{Affinity, AffinitySource, QuotaHeaders};
use common::{delete, get, json, login, patch, post, put};

#[tokio::test]
async fn pools_are_created_edited_and_deleted_with_their_members_and_validation() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(&app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":1},{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert_eq!(
        (
            body["affinity_ttl_seconds"].as_u64(),
            body["overage_policy"].as_str(),
            body["body_capture_hours"].as_u64()
        ),
        (Some(3600), Some("prefer_plan"), Some(0))
    );
    assert_eq!(
        body["members"][0]["account_id"], "business",
        "members sorted by priority"
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"main","label":"x","strategy":"priority"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"rr","label":"x","strategy":"round_robin"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        201
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"p2","label":"x","strategy":"pace","affinity_ttl_seconds":30}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "affinity is 0 or at least 60"
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"p2","label":"x","strategy":"pace","migrate_above_utilization":1.5}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"p2","label":"x","strategy":"pace","select_below_utilization":1.5}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"p2","label":"x","strategy":"pace","select_below_utilization":-0.1}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    let refreshing = post(
        &app,
        "/admin/pools",
        r#"{"id":"p3","label":"x","strategy":"pace","select_below_utilization":1}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refreshing.status(),
        201,
        "the bounds are closed as they are for the migrate threshold"
    );
    let body = json(refreshing).await;
    assert_eq!(
        (
            body["strategy"].as_str(),
            body["select_below_utilization"].as_f64()
        ),
        (Some("pace"), Some(1.0))
    );
    let lowered = patch(
        &app,
        "/admin/pools/p3",
        r#"{"select_below_utilization":0}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(lowered.status(), 200);
    assert_eq!(
        json(lowered).await["select_below_utilization"].as_f64(),
        Some(0.0)
    );
    let cleared = patch(
        &app,
        "/admin/pools/p3",
        r#"{"select_below_utilization":null}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(cleared.status(), 200);
    assert!(
        json(cleared).await["select_below_utilization"].is_null(),
        "and null turns the filter off again"
    );
    assert_eq!(
        delete(&app, "/admin/pools/p3", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"p2","label":"x","strategy":"pace","body_capture_hours":100}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "capped by body_capture_max_hours"
    );
    let patched = patch(&app, "/admin/pools/main", r#"{"affinity_ttl_seconds":0,"overage_policy":"never","model_rules":[{"pattern":"claude-opus-5"},{"pattern":"*","not":true}],"body_capture_hours":24}"#, Some(&cookie)).await;
    assert_eq!(patched.status(), 200);
    let body = json(patched).await;
    assert_eq!(
        (
            body["affinity_ttl_seconds"].as_u64(),
            body["overage_policy"].as_str(),
            body["model_rules"][0]["pattern"].as_str(),
            body["model_rules"][0]["not"].as_bool(),
            body["model_rules"][1]["pattern"].as_str(),
            body["model_rules"][1]["not"].as_bool(),
            body["body_capture_hours"].as_u64()
        ),
        (
            Some(0),
            Some("never"),
            Some("claude-opus-5"),
            Some(false),
            Some("*"),
            Some(true),
            Some(24)
        )
    );
    let empty_pattern = patch(
        &app,
        "/admin/pools/main",
        r#"{"model_rules":[{"pattern":""}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(empty_pattern.status(), 400);
    assert_eq!(
        json(empty_pattern).await["error"],
        "model_rules[0]: pattern is empty"
    );
    let triple_star = patch(
        &app,
        "/admin/pools/main",
        r#"{"model_rules":[{"pattern":"a***"}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(triple_star.status(), 400);
    assert_eq!(
        json(triple_star).await["error"],
        "model_rules[0]: pattern holds three stars in a row"
    );
    let members = put(
        &app,
        "/admin/pools/main/members",
        r#"{"members":[{"account_id":"direct","priority":0}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(members.status(), 200);
    assert_eq!(json(members).await["members"].as_array().unwrap().len(), 1);
    assert_eq!(
        put(
            &app,
            "/admin/pools/main/members",
            r#"{"members":[{"account_id":"ghost","priority":0}]}"#,
            Some(&cookie)
        )
        .await
        .status(),
        404
    );
    let key = post(
        &app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(key.status(), 201);
    assert_eq!(
        delete(&app, "/admin/pools/main", Some(&cookie))
            .await
            .status(),
        409,
        "a pool with keys stays"
    );
    assert_eq!(
        delete(&app, "/admin/keys/desktop", Some(&cookie))
            .await
            .status(),
        204
    );
    let recreated = post(
        &app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(recreated.status(), 201);
    assert_eq!(
        delete(&app, "/admin/pools/main", Some(&cookie))
            .await
            .status(),
        409,
        "pool still has keys"
    );
    assert_eq!(
        delete(&app, "/admin/keys/desktop", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        delete(&app, "/admin/pools/main", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(get(&app, "/admin/pools", Some(&cookie)).await.status(), 200);
    let viewer = login(&app, "viewer-secret").await;
    assert_eq!(get(&app, "/admin/pools", Some(&viewer)).await.status(), 200);
    assert_eq!(
        get(&app, "/admin/sessions", Some(&viewer)).await.status(),
        200
    );
}

#[tokio::test]
async fn keys_are_issued_once_rotated_revoked_and_never_shown_again() {
    let (app, _up, _store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","body_capture_hours":12}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        post(
            &app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"nope"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        404
    );
    let created = post(
        &app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main","notes":"laptop in the study"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    assert_eq!(created.headers().get("cache-control").unwrap(), "no-store");
    let body = json(created).await;
    let secret = body["secret"].as_str().unwrap().to_string();
    assert!(
        secret.starts_with("phoebis-") && secret.len() == 4 + 43,
        "phoebis- plus 32 bytes of base64url without padding"
    );
    assert_eq!(
        body["prefix"].as_str().unwrap(),
        phoebis_store::key_prefix(&secret)
    );
    assert_eq!(
        (
            body["body_capture_hours"].is_null(),
            body["effective_body_capture_hours"].as_u64()
        ),
        (true, Some(12)),
        "null inherits the pool"
    );
    let listed = json(get(&app, "/admin/keys", Some(&cookie)).await).await;
    assert!(
        listed["keys"][0].get("secret").is_none()
            && listed["keys"][0].get("secret_digest").is_none()
    );
    assert!(!listed.to_string().contains(&secret));
    let patched = json(
        patch(
            &app,
            "/admin/keys/desktop",
            r#"{"body_capture_hours":0,"model_rules":[{"pattern":"claude-sonnet-5"}],"model_rules_mode":"replace","long_context_twins":false}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            patched["body_capture_hours"].as_u64(),
            patched["effective_body_capture_hours"].as_u64(),
            patched["model_rules"][0]["pattern"].as_str(),
            patched["model_rules_mode"].as_str(),
            patched["long_context_twins"].as_bool(),
        ),
        (
            Some(0),
            Some(0),
            Some("claude-sonnet-5"),
            Some("replace"),
            Some(false)
        )
    );
    let bad_mode = patch(
        &app,
        "/admin/keys/desktop",
        r#"{"model_rules_mode":"other"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(bad_mode.status(), 400);
    assert_eq!(
        json(bad_mode).await["error"],
        "model_rules_mode is extend or replace"
    );
    assert_eq!(
        patch(
            &app,
            "/admin/keys/desktop",
            r#"{"body_capture_hours":500}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    let rotated = json(post(&app, "/admin/keys/desktop/rotate", "", Some(&cookie)).await).await;
    let second = rotated["secret"].as_str().unwrap().to_string();
    assert_ne!(second, secret);
    assert!(rotated["rotated_at"].is_number());
    assert!(
        router.snapshot().authenticate(&secret, common::NOW).is_ok(),
        "the rotation dialog promises the old secret a grace window"
    );
    assert!(router
        .snapshot()
        .authenticate(&secret, common::NOW + phoebis_store::ROTATION_GRACE_MS)
        .is_err());
    assert_eq!(
        post(&app, "/admin/keys/desktop/revoke", "", Some(&cookie))
            .await
            .status(),
        204
    );
    assert!(router
        .snapshot()
        .authenticate(&secret, common::NOW)
        .is_err());
    assert!(router
        .snapshot()
        .authenticate(&secret, common::NOW + phoebis_store::ROTATION_GRACE_MS)
        .is_err());
    assert_eq!(
        post(&app, "/admin/keys/desktop/rotate", "", Some(&cookie))
            .await
            .status(),
        409,
        "a revoked key cannot be rotated"
    );
    assert_eq!(
        patch(
            &app,
            "/admin/keys/desktop",
            r#"{"enabled":true}"#,
            Some(&cookie)
        )
        .await
        .status(),
        409,
        "or re-enabled"
    );
    let viewer = login(&app, "viewer-secret").await;
    assert_eq!(get(&app, "/admin/keys", Some(&viewer)).await.status(), 200);
    assert_eq!(
        post(
            &app,
            "/admin/keys",
            r#"{"id":"k2","label":"x","pool_id":"main"}"#,
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        post(&app, "/admin/keys/desktop/rotate", "", Some(&viewer))
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn the_key_list_counts_live_sessions_still_on_the_secret_a_rotation_retired() {
    use phoebis_ledger::{Record, RequestRecord};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    let created = json(
        post(
            &parts.app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    let old_secret = created["secret"].as_str().unwrap().to_string();
    let generation = common::generation(&parts.store, "desktop");

    let rotated =
        json(post(&parts.app, "/admin/keys/desktop/rotate", "", Some(&cookie)).await).await;
    let r1 = rotated["rotated_at"].as_i64().unwrap();

    for (id, session, retired) in [("req1", "s1", true), ("req2", "s2", false)] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: common::NOW,
                key_id: Some("desktop".into()),
                key_generation: generation,
                session_id: Some(session.into()),
                model: "claude-sonnet-5".into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                presented_retired: retired,
                rotation_at: Some(r1),
                ..Default::default()
            },
            attempts: vec![],
            bodies: None,
        });
    }
    let keys = common::wait_json(&parts.app, "/admin/keys", &cookie, |v| {
        v["keys"][0]["sessions_on_retired_secret"] == 1
    })
    .await;
    assert_eq!(keys["keys"][0]["sessions_on_retired_secret"], 1);
    assert!(keys["keys"][0]["retired_secret_last_used_at"].is_null());

    // Relay a request through the proxy listener with the secret the rotation just retired, the same
    // authenticate-and-touch path a real client's request takes.
    use tower::ServiceExt;
    let proxy = phoebis_relay::router(parts.relay.clone());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &old_secret)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();
    let _ = proxy.oneshot(request).await.unwrap();
    fake_upstream::wait_for(std::time::Duration::from_secs(2), || {
        parts
            .store
            .api_key("desktop")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at
            .is_some()
            .then_some(())
    })
    .await;
    assert!(
        json(get(&parts.app, "/admin/keys", Some(&cookie)).await).await["keys"][0]
            ["retired_secret_last_used_at"]
            .is_number()
    );

    // Still inside the first grace, a second rotation clears the count and the stamp: the one session's
    // newest request presented the secret the first rotation retired, which the second no longer accepts.
    parts
        .clock_cell
        .fetch_add(1_000, std::sync::atomic::Ordering::SeqCst);
    let rotated2 =
        json(post(&parts.app, "/admin/keys/desktop/rotate", "", Some(&cookie)).await).await;
    let r2 = rotated2["rotated_at"].as_i64().unwrap();
    assert!(r2 > r1 && r2 < r1 + phoebis_store::ROTATION_GRACE_MS);
    let keys = json(get(&parts.app, "/admin/keys", Some(&cookie)).await).await;
    assert_eq!(keys["keys"][0]["sessions_on_retired_secret"], 0);
    assert!(keys["keys"][0]["retired_secret_last_used_at"].is_null());

    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "req3".into(),
            received_at: common::NOW,
            key_id: Some("desktop".into()),
            key_generation: generation,
            session_id: Some("s3".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            presented_retired: true,
            rotation_at: Some(r2),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    });
    let keys = common::wait_json(&parts.app, "/admin/keys", &cookie, |v| {
        v["keys"][0]["sessions_on_retired_secret"] == 1
    })
    .await;
    assert_eq!(keys["keys"][0]["sessions_on_retired_secret"], 1);
}

#[tokio::test]
async fn a_revoke_whose_reload_fails_answers_503_and_the_old_secret_is_refused_only_once_the_reload_succeeds(
) {
    let (app, _up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    let created = json(
        post(
            &app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    let secret = created["secret"].as_str().unwrap().to_string();
    assert!(router.snapshot().authenticate(&secret, common::NOW).is_ok());
    store.debug_fail_next_load();
    let revoked = post(&app, "/admin/keys/desktop/revoke", "", Some(&cookie)).await;
    assert_eq!(revoked.status(), 503);
    assert_eq!(
        revoked.headers().get("x-phoebis-error-code").unwrap(),
        "registry_unavailable"
    );
    assert!(
        store
            .api_key("desktop")
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some(),
        "the row was written"
    );
    assert!(
        router.snapshot().authenticate(&secret, common::NOW).is_ok(),
        "the old snapshot still authenticates the revoked secret, which is what the 503 says"
    );
    router.reload().await.unwrap();
    assert!(
        router
            .snapshot()
            .authenticate(&secret, common::NOW)
            .is_err(),
        "the next successful reload publishes the revocation"
    );
}

/// A session holds a pin per model. The row shows one of them, so it shows the pin the caller used last
/// rather than whichever model happens to sort last.
#[tokio::test]
async fn a_session_with_pins_on_two_models_shows_the_one_it_used_last() {
    let (first, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&first, "admin-secret").await;
    post(
        &first,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    // A pin belongs to the live key of its id, so the key exists before the pins do.
    post(
        &first,
        "/admin/keys",
        r#"{"id":"k","label":"k","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    for (model, account, epoch, last_used_at) in [
        ("claude-opus-5", "direct", 4, common::NOW - 5),
        ("claude-sonnet-4-5", "business", 3, common::NOW - 5_000),
    ] {
        store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: model.into(),
                account_id: account.into(),
                epoch,
                created_at: common::NOW - 10_000,
                last_used_at,
                key_generation: common::generation(&store, "k"),
                incarnation: 1,
                burn: 0.0,
                burn_at: None,
                burn_usd: 0.0,
                last_input_tokens: 0,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
            })
            .unwrap();
    }
    let (app, _up2, _store, _router2) = common::admin_parts_on(store.clone()).await;
    let cookie = login(&app, "admin-secret").await;
    let listed = json(get(&app, "/admin/sessions", Some(&cookie)).await).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "one row for the session");
    assert_eq!(
        (
            sessions[0]["model"].as_str(),
            sessions[0]["account_id"].as_str(),
            sessions[0]["epoch"].as_u64()
        ),
        (Some("claude-opus-5"), Some("direct"), Some(4)),
        "the row is the most recently used pin, not the alphabetically last model"
    );
}

/// The main agent's two pins are considered in the four-part key's sort order, model last; whichever pin the
/// caller used most recently still wins even when it is the one that sorts first.
#[tokio::test]
async fn a_main_agents_two_pins_show_whichever_it_used_last_even_when_the_older_pin_sorts_first() {
    let (first, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&first, "admin-secret").await;
    post(
        &first,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &first,
        "/admin/keys",
        r#"{"id":"k","label":"k","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    for (model, account, epoch, last_used_at) in [
        ("claude-opus-5", "direct", 4, common::NOW - 5_000),
        ("claude-sonnet-4-5", "business", 3, common::NOW - 5),
    ] {
        store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: model.into(),
                account_id: account.into(),
                epoch,
                created_at: common::NOW - 10_000,
                last_used_at,
                key_generation: common::generation(&store, "k"),
                incarnation: 1,
                burn: 0.0,
                burn_at: None,
                burn_usd: 0.0,
                last_input_tokens: 0,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
            })
            .unwrap();
    }
    let (app, _up2, _store, _router2) = common::admin_parts_on(store.clone()).await;
    let cookie = login(&app, "admin-secret").await;
    let listed = json(get(&app, "/admin/sessions", Some(&cookie)).await).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "one row for the session");
    assert_eq!(
        (
            sessions[0]["model"].as_str(),
            sessions[0]["account_id"].as_str(),
            sessions[0]["epoch"].as_u64()
        ),
        (Some("claude-sonnet-4-5"), Some("business"), Some(3)),
        "the row is the most recently used pin, even though its model sorts after the older pin's"
    );
}

/// A subagent's pin never displaces the main agent's, however recently the subagent's was used; both still
/// count toward the row's agent total.
#[tokio::test]
async fn a_main_pin_beside_a_newer_subagent_pin_still_shows_the_main_pin_and_counts_both_agents() {
    let (first, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&first, "admin-secret").await;
    post(
        &first,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &first,
        "/admin/keys",
        r#"{"id":"k","label":"k","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    for (agent_id, model, account, epoch, last_used_at) in [
        ("main", "claude-opus-5", "direct", 4, common::NOW - 5_000),
        (
            "worker",
            "claude-sonnet-4-5",
            "business",
            3,
            common::NOW - 5,
        ),
    ] {
        store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: agent_id.into(),
                model: model.into(),
                account_id: account.into(),
                epoch,
                created_at: common::NOW - 10_000,
                last_used_at,
                key_generation: common::generation(&store, "k"),
                incarnation: 1,
                burn: 0.0,
                burn_at: None,
                burn_usd: 0.0,
                last_input_tokens: 0,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
            })
            .unwrap();
    }
    let (app, _up2, _store, _router2) = common::admin_parts_on(store.clone()).await;
    let cookie = login(&app, "admin-secret").await;
    let listed = json(get(&app, "/admin/sessions", Some(&cookie)).await).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "one row for the session");
    assert_eq!(
        (
            sessions[0]["model"].as_str(),
            sessions[0]["account_id"].as_str(),
            sessions[0]["agents"].as_u64()
        ),
        (Some("claude-opus-5"), Some("direct"), Some(2)),
        "the main agent's own pin shows, and both agents count toward the row"
    );
}

#[tokio::test]
async fn sessions_list_live_pins_and_a_release_expires_them() {
    let (_first, _up, store, _router) = common::admin_parts().await;
    // The pins' key and its pool are registered first: `RoutingState::purge` deletes a pin whose key it
    // cannot resolve, so without them the router's first tick races the release and wins about as often as
    // the runtime parks between the two.
    let secret = "phoebis-testkey0000000000000000000000000000000000";
    store
        .insert_pool(
            &phoebis_store::PoolRow {
                id: "main".into(),
                label: "Main".into(),
                enabled: true,
                strategy: phoebis_store::Strategy::Priority,
                affinity_ttl_seconds: 3_600,
                overage_policy: phoebis_store::OveragePolicy::Allow,
                migrate_above_utilization: None,
                select_below_utilization: None,
                model_rules: Vec::new(),
                keepalive_json: "{}".into(),
                body_capture_hours: 0,
                prefer_subscription: false,
                shedding: phoebis_store::Shedding::default(),
                failover: phoebis_store::Failover::default(),
                cache_ttl_json: "{}".into(),
                cache_diagnostics: None,
                affinity: Affinity::default(),
                quota_headers: QuotaHeaders::Pass,
                created_at: common::NOW,
                updated_at: common::NOW,
            },
            "test",
        )
        .unwrap();
    store
        .insert_api_key(
            &phoebis_store::ApiKeyRow {
                id: "k".into(),
                label: "k".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: Vec::new(),
                model_rules_mode: phoebis_store::RulesMode::Extend,
                long_context_twins: true,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: store.api_key_digest(secret).to_vec(),
                prefix: phoebis_store::key_prefix(secret),
                created_at: common::NOW,
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
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "k".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-4-5".into(),
            account_id: "direct".into(),
            epoch: 3,
            created_at: common::NOW - 10,
            last_used_at: common::NOW - 5,
            key_generation: common::generation(&store, "k"),
            incarnation: 1,
            burn: 0.0,
            burn_at: None,
            burn_usd: 0.0,
            last_input_tokens: 0,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
        })
        .unwrap();
    store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "k".into(),
            session_id: "old".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-4-5".into(),
            account_id: "direct".into(),
            epoch: 2,
            created_at: 0,
            last_used_at: common::NOW - 90_000_000,
            key_generation: common::generation(&store, "k"),
            incarnation: 1,
            burn: 0.0,
            burn_at: None,
            burn_usd: 0.0,
            last_input_tokens: 0,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
        })
        .unwrap();
    // A second app over the same store: its router task loads the pins the first one never saw.
    let (app, _up2, _store, _router2) = common::admin_parts_on(store.clone()).await;
    let cookie = login(&app, "admin-secret").await;
    let listed = json(get(&app, "/admin/sessions", Some(&cookie)).await).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "a pin older than sessions.expire_after_seconds is not listed"
    );
    assert_eq!(
        (
            sessions[0]["session_id"].as_str(),
            sessions[0]["epoch"].as_u64()
        ),
        (Some("s1"), Some(3))
    );
    let released = post(&app, "/admin/sessions/k/s1/release", "", Some(&cookie)).await;
    assert_eq!(released.status(), 204);
    assert_eq!(released.headers().get("x-phoebis-released").unwrap(), "1");
    assert_eq!(
        post(&app, "/admin/sessions/k/s1/release", "", Some(&cookie))
            .await
            .status(),
        404
    );
    assert!(
        json(get(&app, "/admin/sessions", Some(&cookie)).await).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn keys_and_pools_carry_their_ledger_totals_status_and_traffic() {
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    // The store stamps the key from the wall clock, so the live generation is read back rather than assumed.
    let key_generation = parts.store.api_key("desktop").unwrap().unwrap().created_at;
    for (id, at) in [("r1", now - 60_000), ("r2", now - 2 * 86_400_000)] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 100),
                key_id: Some("desktop".into()),
                key_generation,
                pool_id: Some("main".into()),
                model: "claude-sonnet-5".into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                status: Some(200),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            // The ledger reprices every attempt from the shipped table on write (see `land` in
            // `crates/ledger/src/task.rs`), so a manually set `estimated_usd` would not survive; the model is
            // priced instead and the assertions below use the table's own rate.
            attempts: vec![AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "direct".into(),
                account_kind: "api_key".into(),
                started_at: at,
                outcome: "succeeded".into(),
                input_tokens: Some(100),
                output_tokens: Some(50),
                ..Default::default()
            }],
            bodies: None,
        });
    }
    let keys = common::wait_json(&parts.app, "/admin/keys", &cookie, |v| {
        v["keys"][0]["lifetime"]["requests"].as_u64() == Some(2)
    })
    .await;
    let key = keys["keys"][0].clone();
    // claude-sonnet-5 prices at 2.0/input and 10.0/output per million tokens: one request of 100 input and 50
    // output tokens costs 0.0002 + 0.0005 = 0.0007, and two of them cost 0.0014.
    assert_eq!(
        (
            key["usage_7d"]["requests"].as_u64(),
            key["usage_7d"]["tokens"].as_u64()
        ),
        (Some(2), Some(300))
    );
    assert!((key["lifetime"]["cost_usd"].as_f64().unwrap() - 0.0014).abs() < 1e-9);
    let pool = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0].clone();
    assert_eq!(
        (
            pool["status"].as_str(),
            pool["requests_24h"].as_u64(),
            pool["active_pins"].as_u64()
        ),
        (Some("active"), Some(1), Some(0))
    );
    // Only r1 falls in the last 24h; r2 is two days old. One request costs 0.0007, per the note above.
    assert!((pool["cost_24h"].as_f64().unwrap() - 0.0007).abs() < 1e-9);
    let traffic = pool["traffic_24h"].as_array().unwrap();
    assert_eq!(traffic.len(), 24);
    assert_eq!(
        traffic
            .iter()
            .map(|b| b["requests"].as_u64().unwrap())
            .sum::<u64>(),
        1
    );
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts/direct/pause",
            "",
            Some(&cookie)
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0]["status"],
        "exhausted",
        "every member paused"
    );
    assert_eq!(
        patch(
            &parts.app,
            "/admin/pools/main",
            r#"{"enabled":false}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0]["status"],
        "disabled"
    );
}

/// The selector skips an account whose saved subscription end has passed, so the pool status does too: its only
/// such member reads unavailable and the pool exhausted, as a paused one leaves it.
#[tokio::test]
async fn a_member_past_its_saved_subscription_end_is_unavailable_and_its_pool_exhausted() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .store
        .set_state("business", phoebis_store::AccountState::Ready)
        .unwrap();
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","overage_policy":"allow","members":[{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
    let pool = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0].clone();
    assert_eq!(
        (
            pool["status"].as_str(),
            pool["members"][0]["eligible"].as_str()
        ),
        (Some("active"), Some("eligible")),
        "a ready member before any end: {pool}"
    );
    assert_eq!(
        patch(
            &parts.app,
            "/admin/accounts/business",
            &format!(
                r#"{{"cancelled":true,"subscription_ends_at":{}}}"#,
                common::NOW - 1
            ),
            Some(&cookie),
        )
        .await
        .status(),
        200
    );
    let pool = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0].clone();
    assert_eq!(
        (
            pool["status"].as_str(),
            pool["members"][0]["eligible"].as_str()
        ),
        (Some("exhausted"), Some("unavailable")),
        "past its saved end the member is the selector's obstacle: {pool}"
    );
}

#[tokio::test]
async fn a_pool_under_never_reads_exhausted_when_its_only_member_is_in_overage() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","overage_policy":"never","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
    parts.router.report(phoebis_routing::Report::Overage {
        account_id: "direct".into(),
        evidence: phoebis_routing::OverageEvidence {
            in_use: true,
            since: Some(common::NOW),
            reset: None,
            observed_at: common::NOW,
            source: phoebis_routing::EvidenceSource::Head,
            incarnation: 1,
        },
        incarnation: 1,
    });
    let pools = common::wait_json(&parts.app, "/admin/pools", &cookie, |v| {
        v["pools"][0]["status"] == "exhausted"
    })
    .await;
    assert_eq!(pools["pools"][0]["members"][0]["eligible"], "overage");
    assert_eq!(
        patch(
            &parts.app,
            "/admin/pools/main",
            r#"{"overage_policy":"allow"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0]["status"],
        "active",
        "the same overage is not an obstacle once the policy allows it"
    );
}

#[tokio::test]
async fn an_overage_whose_reset_has_passed_no_longer_marks_the_pool() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","overage_policy":"never","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
    parts.router.report(phoebis_routing::Report::Overage {
        account_id: "direct".into(),
        evidence: phoebis_routing::OverageEvidence {
            in_use: true,
            since: Some(common::NOW - 3_600_000),
            reset: Some(common::NOW - 1),
            observed_at: common::NOW - 3_600_000,
            source: phoebis_routing::EvidenceSource::Head,
            incarnation: 1,
        },
        incarnation: 1,
    });
    // Synchronized on the account view, which reads the raw flag, before reading the pool, which converts it.
    common::wait_json(&parts.app, "/admin/accounts", &cookie, |v| {
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "direct" && a["overage"]["in_use"] == true)
    })
    .await;
    let pools = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await;
    assert_eq!(
        (
            pools["pools"][0]["status"].as_str(),
            pools["pools"][0]["members"][0]["eligible"].as_str()
        ),
        (Some("active"), Some("eligible")),
        "a reset that has passed no longer counts as overage evidence for eligibility"
    );
}

#[tokio::test]
async fn a_key_carries_an_in_flight_cap_that_null_clears_and_zero_is_refused() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    let created = json(
        post(
            &app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main","in_flight_cap":4}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(created["in_flight_cap"].as_u64(), Some(4));
    let refused = patch(
        &app,
        "/admin/keys/desktop",
        r#"{"in_flight_cap":0}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert_eq!(
        json(refused).await["error"].as_str(),
        Some("in_flight_cap is at least 1")
    );
    let cleared = json(
        patch(
            &app,
            "/admin/keys/desktop",
            r#"{"in_flight_cap":null}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        cleared["in_flight_cap"].is_null(),
        "an explicit null clears the stored cap"
    );
}

#[tokio::test]
async fn the_account_list_answers_from_one_router_ask_for_the_whole_list() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.router.report(phoebis_routing::Report::Overage {
        account_id: "business".into(),
        evidence: phoebis_routing::OverageEvidence {
            in_use: true,
            since: Some(common::NOW),
            reset: None,
            observed_at: common::NOW,
            source: phoebis_routing::EvidenceSource::Head,
            incarnation: 1,
        },
        incarnation: 1,
    });
    let accounts = common::wait_json(&parts.app, "/admin/accounts", &cookie, |v| {
        v["accounts"][0]["overage"]["in_use"] == true
    })
    .await;
    assert_eq!(accounts["accounts"][0]["overage"]["in_use"], true);
}

#[tokio::test]
async fn a_pool_defaults_to_pace_with_the_kind_order_on_and_the_flag_is_the_operators_alone() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"dflt","label":"Default"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert_eq!(
        (
            body["strategy"].as_str(),
            body["prefer_subscription"].as_bool()
        ),
        (Some("pace"), Some(true)),
        "a fresh pool starts on pace with the kind order already on"
    );
    let off = json(
        patch(
            &app,
            "/admin/pools/dflt",
            r#"{"prefer_subscription":false}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(off["prefer_subscription"].as_bool(), Some(false));
    let edited = json(
        patch(
            &app,
            "/admin/pools/dflt",
            r#"{"strategy":"round_robin"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            edited["strategy"].as_str(),
            edited["prefer_subscription"].as_bool()
        ),
        (Some("round_robin"), Some(false)),
        "the strategy patch leaves the operator's flag standing"
    );
    assert_eq!(
        post(&app, "/admin/pools", r#"{"id":"nolabel"}"#, Some(&cookie))
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn a_pool_refuses_an_unknown_strategy_name_and_an_unknown_field() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let refused = post(
        &app,
        "/admin/pools",
        r#"{"id":"legacy","label":"x","strategy":"foo"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert_eq!(
        json(refused).await["error"].as_str(),
        Some("strategy must be pace, priority, or round_robin")
    );
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main2","label":"Main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main2",
            r#"{"unknown_field":1}"#,
            Some(&cookie),
        )
        .await
        .status(),
        400,
        "an unknown field is refused"
    );
    let listed = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let pool = listed["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main2")
        .unwrap();
    let carries_a_removed_field = pool
        .as_object()
        .unwrap()
        .keys()
        .any(|k| k.contains("sub_strategy") || k.contains("_effective"));
    assert!(
        !carries_a_removed_field,
        "a GET carries neither removed field"
    );
}

#[tokio::test]
async fn a_round_robin_pool_created_with_prefer_subscription_answers_the_flag() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let flags = |v: &serde_json::Value| {
        (
            v["strategy"].as_str().map(str::to_owned),
            v["prefer_subscription"].as_bool(),
        )
    };
    let preferred = post(&app, "/admin/pools", r#"{"id":"rotating","label":"Rotating","strategy":"round_robin","prefer_subscription":true,"members":[{"account_id":"direct","priority":0},{"account_id":"business","priority":1}]}"#, Some(&cookie)).await;
    assert_eq!(preferred.status(), 201);
    let rr = Some("round_robin".to_owned());
    assert_eq!(
        flags(&json(preferred).await),
        (rr.clone(), Some(true)),
        "the operator's flag is answered as stored"
    );
    let plain = post(
        &app,
        "/admin/pools",
        r#"{"id":"plain","label":"Plain","strategy":"round_robin","prefer_subscription":false}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        flags(&json(plain).await),
        (rr.clone(), Some(false)),
        "the flag is explicit either way"
    );
    let listed = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let stored = listed["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "rotating")
        .cloned()
        .unwrap();
    assert_eq!(
        flags(&stored),
        (rr, Some(true)),
        "the create stored the flag rather than answering it once"
    );
}

#[tokio::test]
async fn a_pool_carries_a_cache_ttl_policy_and_a_diagnostics_override() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = json(post(&app, "/admin/pools", r#"{"id":"cached","label":"Cached","cache_ttl":{"enabled":true,"plan":{"main":"1h","subagent":"5m","models":[{"model":"claude-haiku-4-5-20251001","main":"5m","subagent":"5m"}]},"metered":{"main":"5m"}},"cache_diagnostics":true}"#, Some(&cookie)).await).await;
    assert_eq!(
        created["cache_ttl"]["plan"]["models"][0]["model"],
        "claude-haiku-4-5-20251001"
    );
    assert_eq!(created["cache_diagnostics"], true);
    let plain = json(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"plain","label":"Plain"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            plain["cache_ttl"]["enabled"].as_bool(),
            plain["cache_ttl"]["plan"]["main"].as_str(),
            plain["cache_ttl"]["metered"]["main"].as_str(),
            plain["cache_diagnostics"].is_null()
        ),
        (Some(false), Some("1h"), Some("5m"), true),
        "defaults are answered in full; null inherits the bootstrap option"
    );
    let patched = json(
        patch(
            &app,
            "/admin/pools/cached",
            r#"{"cache_ttl":{"plan":{"subagent":"1h"}},"cache_diagnostics":null}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            patched["cache_ttl"]["plan"]["subagent"].as_str(),
            patched["cache_ttl"]["metered"]["main"].as_str(),
            patched["cache_diagnostics"].is_null()
        ),
        (Some("1h"), Some("5m"), true),
        "a partial policy merges over the stored one, one set at a time"
    );
    let repatched = json(
        patch(
            &app,
            "/admin/pools/cached",
            r#"{"cache_ttl":{"metered":{"models":[{"model":"Claude-Sonnet-4-5[1m]","main":"5m","subagent":"5m"}]}}}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            repatched["cache_ttl"]["metered"]["models"][0]["model"].as_str(),
            repatched["cache_ttl"]["metered"]["main"].as_str(),
            repatched["cache_ttl"]["plan"]["subagent"].as_str(),
        ),
        (Some("claude-sonnet-4-5"), Some("5m"), Some("1h")),
        "an undated id under metered.models reads back canonical, and the untouched fields of both sets merge one level deep"
    );
    for (body, message) in [
        (
            r#"{"cache_ttl":{"plan":{"main":"2h"}}}"#,
            "cache_ttl is not a policy object",
        ),
        (
            r#"{"cache_ttl":{"unknown_field":"5m"}}"#,
            "cache_ttl is not a policy object",
        ),
        (
            r#"{"cache_ttl":{"plan":{"models":[{"model":"","main":"5m","subagent":"5m"}]}}}"#,
            "cache_ttl.plan.models[].model must be a model id",
        ),
        (
            r#"{"cache_ttl":{"plan":{"models":[{"model":"m","main":"5m","subagent":"5m"},{"model":"m","main":"1h","subagent":"1h"}]}}}"#,
            "cache_ttl.plan.models lists m twice",
        ),
        (r#"{"cache_ttl":[]}"#, "cache_ttl is not a policy object"),
    ] {
        let resp = patch(&app, "/admin/pools/cached", body, Some(&cookie)).await;
        assert_eq!(resp.status(), 400, "{body}");
        assert!(
            json(resp).await["error"]
                .as_str()
                .unwrap()
                .starts_with(message),
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_key_created_again_under_a_used_id_answers_the_stamped_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = std::sync::Arc::new(
        phoebis_store::Store::open(&path, &phoebis_store::StorageKey::generate()).unwrap(),
    );
    let (app, _up, store, _router) = common::admin_parts_on(store).await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    let first = post(
        &app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(first.status(), 201);
    assert_eq!(
        delete(&app, "/admin/keys/desktop", Some(&cookie))
            .await
            .status(),
        204
    );
    // The first holder's create is moved a minute past the wall clock: the state the second create sees when
    // it lands inside the first's millisecond, pinned rather than raced for.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE registry_changes SET at = at + 60000 WHERE target = 'desktop' AND kind = 'key_created'",
        [],
    )
    .unwrap();
    let previous: i64 = conn
        .query_row(
            "SELECT MAX(at) FROM registry_changes WHERE target = 'desktop' AND kind = 'key_created'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    let second = json(
        post(
            &app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            second["created_at"].as_i64(),
            store.api_key("desktop").unwrap().unwrap().created_at
        ),
        (Some(previous + 1), previous + 1),
        "the answer carries the generation the store stamped, not the route's clock"
    );
}

#[tokio::test]
async fn a_pool_patch_carrying_members_applies_both_or_refuses_before_the_policy_lands() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":1}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"affinity_ttl_seconds":120,"members":[{"account_id":"business","priority":0},{"account_id":"ghost","priority":1}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 404);
    assert_eq!(json(refused).await["error"], "account ghost not found");
    let kept = json(get(&app, "/admin/pools", Some(&cookie)).await).await["pools"][0].clone();
    assert_eq!(
        (
            kept["affinity_ttl_seconds"].as_u64(),
            kept["members"].as_array().map(Vec::len),
            kept["members"][0]["account_id"].as_str()
        ),
        (Some(3600), Some(1), Some("direct")),
        "a member list naming an unknown account refuses before the policy is written"
    );
    let both = patch(
        &app,
        "/admin/pools/main",
        r#"{"affinity_ttl_seconds":120,"members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":2}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(both.status(), 200);
    let body = json(both).await;
    let members: Vec<(&str, u64)> = body["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["account_id"].as_str().unwrap(),
                m["priority"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        (body["affinity_ttl_seconds"].as_u64(), members),
        (Some(120), vec![("business", 0), ("direct", 2)]),
        "one PATCH changes the policy and the members together"
    );
    let reread = json(get(&app, "/admin/pools", Some(&cookie)).await).await["pools"][0].clone();
    assert_eq!(
        (&reread["affinity_ttl_seconds"], &reread["members"]),
        (&body["affinity_ttl_seconds"], &body["members"])
    );
}

#[tokio::test]
async fn a_pool_accepts_the_shedding_and_failover_fields_and_refuses_each_out_of_range_value() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = json(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"eng","label":"Engine","shed_share":0.3,"burn_window_seconds":120,
                "source_dwell_seconds":90,"migrate_when_spent":false,"warm_sessions":"always",
                "retry_in_place_seconds":5,"in_flight_cap":4,"pace_floor":0.1,
                "overage_move_grace_minutes":10,"wind_down_minutes":15}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            created["shed_share"].as_f64(),
            created["burn_window_seconds"].as_u64(),
            created["source_dwell_seconds"].as_u64(),
            created["migrate_when_spent"].as_bool(),
            created["warm_sessions"].as_str(),
            created["retry_in_place_seconds"].as_u64(),
            created["in_flight_cap"].as_u64(),
            created["pace_floor"].as_f64(),
            created["overage_move_grace_minutes"].as_u64(),
            created["wind_down_minutes"].as_u64(),
        ),
        (
            Some(0.3),
            Some(120),
            Some(90),
            Some(false),
            Some("always"),
            Some(5),
            Some(4),
            Some(0.1),
            Some(10),
            Some(15),
        ),
        "every shedding and failover field is answered as stored"
    );
    assert!(
        created["moves_per_hour"].is_object(),
        "the pool card's figure is always an object, empty when nothing moved"
    );
    for (body, message) in [
        (r#"{"shed_share":1.5}"#, "shed_share is between 0 and 1"),
        (r#"{"shed_share":-0.1}"#, "shed_share is between 0 and 1"),
        (
            r#"{"burn_window_seconds":59}"#,
            "burn_window_seconds is at least 60",
        ),
        (
            r#"{"source_dwell_seconds":59}"#,
            "source_dwell_seconds is at least 60",
        ),
        (
            r#"{"warm_sessions":"sometimes"}"#,
            "warm_sessions must be always, never, or when_needed",
        ),
        (
            r#"{"retry_in_place_seconds":301}"#,
            "retry_in_place_seconds is 0 to 300",
        ),
        (r#"{"in_flight_cap":0}"#, "in_flight_cap is at least 1"),
        (r#"{"pace_floor":-0.1}"#, "pace_floor is between 0 and 1"),
        (r#"{"pace_floor":1.5}"#, "pace_floor is between 0 and 1"),
        (
            r#"{"overage_move_grace_minutes":301}"#,
            "overage_move_grace_minutes is 0 to 300",
        ),
        (
            r#"{"wind_down_minutes":10081}"#,
            "wind_down_minutes is at most 10080 (one week)",
        ),
    ] {
        let refused = patch(&app, "/admin/pools/eng", body, Some(&cookie)).await;
        assert_eq!(refused.status(), 400, "{body}");
        assert_eq!(json(refused).await["error"].as_str(), Some(message));
    }
    // Every field's lower inclusive edge, accepted in one patch.
    let low = json(
        patch(
            &app,
            "/admin/pools/eng",
            r#"{"shed_share":0,"pace_floor":0,"burn_window_seconds":60,"source_dwell_seconds":60,
                "retry_in_place_seconds":0,"in_flight_cap":1,"overage_move_grace_minutes":0,
                "wind_down_minutes":0}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            low["shed_share"].as_f64(),
            low["pace_floor"].as_f64(),
            low["burn_window_seconds"].as_u64(),
            low["source_dwell_seconds"].as_u64(),
            low["retry_in_place_seconds"].as_u64(),
            low["in_flight_cap"].as_u64(),
            low["overage_move_grace_minutes"].as_u64(),
            low["wind_down_minutes"].as_u64(),
        ),
        (
            Some(0.0),
            Some(0.0),
            Some(60),
            Some(60),
            Some(0),
            Some(1),
            Some(0),
            Some(0),
        ),
        "every field's lower inclusive edge is accepted"
    );
    // The three fields with an upper edge, accepted in a second patch.
    let high = json(
        patch(
            &app,
            "/admin/pools/eng",
            r#"{"shed_share":1,"pace_floor":1,"retry_in_place_seconds":300,
                "overage_move_grace_minutes":300,"wind_down_minutes":10080}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            high["shed_share"].as_f64(),
            high["pace_floor"].as_f64(),
            high["retry_in_place_seconds"].as_u64(),
            high["overage_move_grace_minutes"].as_u64(),
            high["wind_down_minutes"].as_u64(),
        ),
        (Some(1.0), Some(1.0), Some(300), Some(300), Some(10_080)),
        "every field's upper inclusive edge is accepted"
    );
}

#[tokio::test]
async fn a_patch_leaves_an_absent_engine_field_at_its_stored_value() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"eng","label":"Engine","shed_share":0.3,"burn_window_seconds":120,
            "source_dwell_seconds":90,"migrate_when_spent":false,"warm_sessions":"always",
            "retry_in_place_seconds":5,"in_flight_cap":4,"pace_floor":0.1,
            "overage_move_grace_minutes":10,"wind_down_minutes":15}"#,
        Some(&cookie),
    )
    .await;
    let edited = json(
        patch(
            &app,
            "/admin/pools/eng",
            r#"{"label":"Renamed"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            edited["label"].as_str(),
            edited["shed_share"].as_f64(),
            edited["burn_window_seconds"].as_u64(),
            edited["source_dwell_seconds"].as_u64(),
            edited["migrate_when_spent"].as_bool(),
            edited["warm_sessions"].as_str(),
            edited["retry_in_place_seconds"].as_u64(),
            edited["in_flight_cap"].as_u64(),
            edited["pace_floor"].as_f64(),
            edited["overage_move_grace_minutes"].as_u64(),
            edited["wind_down_minutes"].as_u64(),
        ),
        (
            Some("Renamed"),
            Some(0.3),
            Some(120),
            Some(90),
            Some(false),
            Some("always"),
            Some(5),
            Some(4),
            Some(0.1),
            Some(10),
            Some(15),
        ),
        "an unrelated patch leaves every engine field at what it was stored with"
    );
}
