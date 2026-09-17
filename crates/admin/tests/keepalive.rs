//! The keepalive policy on a pool: typed on the way in, whole on the way out, and gated by affinity.
mod common;
use common::{get, json, login, patch, post};

#[tokio::test]
async fn a_pool_policy_is_typed_gated_by_affinity_and_rendered_whole() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority_random"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert_eq!(body["keepalive"]["enabled"], false);
    assert_eq!(
        (
            body["keepalive"]["lead_seconds_5m"].as_u64(),
            body["keepalive"]["retry_max_seconds"].as_u64(),
            body["keepalive"]["probe_cost_cap_usd_per_day"].is_null()
        ),
        (Some(60), Some(300), true),
        "a pool created without a policy renders the defaults"
    );
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"enabled":true,"probe_enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("keepalive is not a policy object"));
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"cold_probe_limit":0}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("cold_probe_limit must be between 1 and 20"));
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"keepalive":"on"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"affinity_ttl_seconds":120,"keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refused.status(),
        400,
        "enabling with affinity below 300 s in the same request"
    );
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("affinity_ttl_seconds of at least 300"));
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":120}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"keepalive":{"enabled":true}}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "enabling on a pool whose affinity is already short"
    );
    let enabled = patch(&app, "/admin/pools/main", r#"{"affinity_ttl_seconds":600,"keepalive":{"enabled":true,"lead_seconds_5m":90,"probe_cost_cap_usd_per_day":2.5}}"#, Some(&cookie)).await;
    assert_eq!(enabled.status(), 200);
    let body = json(enabled).await;
    assert_eq!(
        (
            body["keepalive"]["enabled"].as_bool(),
            body["keepalive"]["lead_seconds_5m"].as_u64(),
            body["keepalive"]["probe_cost_cap_usd_per_day"].as_f64(),
            body["keepalive"]["jitter_seconds_1h"].as_u64()
        ),
        (Some(true), Some(90), Some(2.5), Some(30))
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":299}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "lowering affinity under an enabled policy"
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":0}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "turning affinity off under an enabled policy"
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":0,"keepalive":{"enabled":false}}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200,
        "both in one request is consistent"
    );
    let listed = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let main = listed["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main")
        .unwrap();
    assert_eq!(
        (
            main["keepalive"]["enabled"].as_bool(),
            main["keepalive"]["lead_seconds_5m"].as_u64()
        ),
        (Some(false), Some(90)),
        "the tuning survives the toggle"
    );
    let with_policy = post(
        &app,
        "/admin/pools",
        r#"{"id":"warm","label":"Warm","strategy":"priority_random","keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        with_policy.status(),
        201,
        "the default affinity of 3,600 admits an enabled policy at creation"
    );
}
