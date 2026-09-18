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

/// A session holds a pin per model. The row shows one of them, so it shows the pin the caller used last
/// rather than whichever model happens to sort last.
#[tokio::test]
async fn a_session_with_pins_on_two_models_shows_the_one_it_used_last() {
    let (_first, _up, store, _router) = common::admin_parts().await;
    for (model, account, epoch, last_used_at) in [
        ("claude-opus-5", "direct", 4, common::NOW - 5),
        ("claude-sonnet-4-5", "business", 3, common::NOW - 5_000),
    ] {
        store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "k".into(),
                session_id: "s1".into(),
                model: model.into(),
                account_id: account.into(),
                epoch,
                created_at: common::NOW - 10_000,
                last_used_at,
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
                strategy: phoebis_store::Strategy::PriorityRandom,
                affinity_ttl_seconds: 3_600,
                overage_policy: phoebis_store::OveragePolicy::Allow,
                migrate_above_utilization: None,
                select_below_utilization: None,
                default_allowed_models: None,
                keepalive_json: "{}".into(),
                body_capture_hours: 0,
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
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: store.api_key_digest(secret).to_vec(),
                prefix: phoebis_store::key_prefix(secret),
                created_at: common::NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        )
        .unwrap();
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

#[tokio::test]
async fn keys_and_pools_carry_their_ledger_totals_status_and_traffic() {
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    for (id, at) in [("r1", now - 60_000), ("r2", now - 2 * 86_400_000)] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 100),
                key_id: Some("desktop".into()),
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
    parts
        .store
        .set_state("direct", phoebis_store::AccountState::Refreshing)
        .unwrap();
    assert_eq!(
        json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0]["status"],
        "active",
        "a member mid-refresh still serves, as the selector admits it"
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
