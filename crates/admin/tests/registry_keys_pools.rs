mod common;
use common::{delete, get, json, login, patch, post, put};

#[tokio::test]
async fn pools_are_created_edited_and_deleted_with_their_members_and_validation() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(&app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"direct","priority":1},{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
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
            r#"{"id":"main","label":"x","strategy":"priority_random"}"#,
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
            r#"{"id":"p2","label":"x","strategy":"round_robin"}"#,
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
            r#"{"id":"p2","label":"x","strategy":"least_utilized","affinity_ttl_seconds":30}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "affinity is 0 or at least 60"
    );
    assert_eq!(post(&app, "/admin/pools", r#"{"id":"p2","label":"x","strategy":"least_utilized","migrate_above_utilization":1.5}"#, Some(&cookie)).await.status(), 400);
    assert_eq!(
        post(
            &app,
            "/admin/pools",
            r#"{"id":"p2","label":"x","strategy":"least_utilized","select_below_utilization":1.5}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    assert_eq!(post(&app, "/admin/pools", r#"{"id":"p2","label":"x","strategy":"least_utilized","select_below_utilization":-0.1}"#, Some(&cookie)).await.status(), 400);
    let refreshing = post(
        &app,
        "/admin/pools",
        r#"{"id":"p3","label":"x","strategy":"earliest_refresh","select_below_utilization":1}"#,
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
        (Some("earliest_refresh"), Some(1.0))
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
            r#"{"id":"p2","label":"x","strategy":"least_utilized","body_capture_hours":100}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "capped by body_capture_max_hours"
    );
    let patched = patch(&app, "/admin/pools/main", r#"{"affinity_ttl_seconds":0,"overage_policy":"never","default_allowed_models":["Claude-Opus-5"],"body_capture_hours":24}"#, Some(&cookie)).await;
    assert_eq!(patched.status(), 200);
    let body = json(patched).await;
    assert_eq!(
        (
            body["affinity_ttl_seconds"].as_u64(),
            body["overage_policy"].as_str(),
            body["default_allowed_models"][0].as_str(),
            body["body_capture_hours"].as_u64()
        ),
        (Some(0), Some("never"), Some("claude-opus-5"), Some(24))
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
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority_random","body_capture_hours":12}"#,
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
            r#"{"body_capture_hours":0,"allowed_models":["Claude-Sonnet-5"]}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            patched["body_capture_hours"].as_u64(),
            patched["effective_body_capture_hours"].as_u64(),
            patched["allowed_models"][0].as_str()
        ),
        (Some(0), Some(0), Some("claude-sonnet-5"))
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
    assert_eq!(
        post(&app, "/admin/keys/desktop/revoke", "", Some(&cookie))
            .await
            .status(),
        204
    );
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
async fn a_revoke_whose_reload_fails_answers_503_and_the_old_secret_is_refused_only_once_the_reload_succeeds(
) {
    let (app, _up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority_random"}"#,
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

#[tokio::test]
async fn sessions_list_live_pins_and_a_release_expires_them() {
    let (_first, _up, store, _router) = common::admin_parts().await;
    store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "k".into(),
            session_id: "s1".into(),
            model: "claude-sonnet-4-5".into(),
            account_id: "direct".into(),
            epoch: 3,
            created_at: common::NOW - 10,
            last_used_at: common::NOW - 5,
        })
        .unwrap();
    store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "k".into(),
            session_id: "old".into(),
            model: "claude-sonnet-4-5".into(),
            account_id: "direct".into(),
            epoch: 2,
            created_at: 0,
            last_used_at: common::NOW - 90_000_000,
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
