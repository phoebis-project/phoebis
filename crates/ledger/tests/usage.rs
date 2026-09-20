//! `key_usage` sums one key's rollups alone, probes left out, and the route it backs answers only the caller's
//! own key, a deleted key's usage staying with the retired id.
mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use phoebis_ledger::{key_usage, AttemptRecord, KeyUsageWindow, LedgerConfig, LedgerDb, RequestRecord};
use phoebis_store::{ApiKeyRow, StoreError};
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn used(id: &str) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        started_at: 1_000,
        outcome: "succeeded".into(),
        input_tokens: Some(100),
        output_tokens: Some(10),
        cache_read_tokens: Some(50),
        cache_write_5m_tokens: Some(20),
        estimated_usd: Some(0.5),
        ..Default::default()
    }
}

fn unused(id: &str) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        started_at: 1_000,
        outcome: "failed".into(),
        ..Default::default()
    }
}

fn request(
    id: &str,
    key_id: &str,
    model: &str,
    kind: &str,
    outcome: &str,
    status: u16,
) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: 1_000,
        key_id: Some(key_id.into()),
        pool_id: Some("p".into()),
        model: model.into(),
        canonical_model: model.into(),
        kind: kind.into(),
        outcome: outcome.into(),
        status: Some(status),
        attempts: 1,
        ..Default::default()
    }
}

#[test]
fn key_usage_sums_one_keys_rollups_and_leaves_probes_and_other_keys_out() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(
        &request("r1", "k1", "claude-sonnet-5", "inference", "succeeded", 200),
        &[used("r1")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r2", "k1", "claude-sonnet-5", "inference", "succeeded", 200),
        &[used("r2")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r3", "k1", "claude-sonnet-5", "inference", "failed", 500),
        &[unused("r3")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r4", "k1", "claude-opus-5", "inference", "succeeded", 200),
        &[used("r4")],
        None,
    )
    .unwrap();
    // A probe under the same key is folded into its own rollup row and must not reach the key's own totals,
    // the way `request_totals` already leaves probes out of the key list.
    db.insert_request(
        &request("r5", "k1", "claude-sonnet-5", "probe", "succeeded", 200),
        &[used("r5")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r6", "k2", "claude-sonnet-5", "inference", "succeeded", 200),
        &[used("r6")],
        None,
    )
    .unwrap();

    let k1 = db.read(|c| key_usage(c, "k1", 0, 10_000)).unwrap();
    assert_eq!((k1.requests, k1.succeeded, k1.failed), (4, 3, 1));
    assert_eq!(k1.tokens.input, 300);
    assert_eq!(k1.tokens.cache_write, 60);
    assert!((k1.cost_usd - 1.5).abs() < 1e-9);
    assert_eq!(
        k1.models
            .iter()
            .map(|m| m.model.as_str())
            .collect::<Vec<_>>(),
        ["claude-opus-5", "claude-sonnet-5"]
    );

    assert_eq!(
        db.read(|c| key_usage(c, "k2", 0, 10_000)).unwrap().requests,
        1
    );

    let k3 = db.read(|c| key_usage(c, "k3", 0, 10_000)).unwrap();
    assert_eq!(
        k3,
        KeyUsageWindow {
            from: 0,
            to: 10_000,
            ..KeyUsageWindow::default()
        }
    );
}

async fn get(
    app: &axum::Router,
    key: Option<&str>,
) -> (u16, Vec<(String, String)>, serde_json::Value) {
    let mut req = Request::builder().method("GET").uri("/v1/usage");
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Polls the route itself rather than the ledger underneath it, so the assertion covers the same read path a
/// caller does.
async fn wait_for_route(app: &axum::Router, key: &str) -> serde_json::Value {
    let give_up = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (_, _, body) = get(app, Some(key)).await;
        if body["windows"]["24h"]["requests"].as_u64() == Some(1) {
            return body;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the route never saw the relayed request: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// No key named `account_id`, and no string value naming the fixture's own account id or a secret's shape:
/// the self-service answer names the caller's key and nothing of the registry beyond it.
fn assert_nothing_leaks(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                assert_ne!(k, "account_id", "the body names an account: {value}");
                assert_nothing_leaks(v);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(assert_nothing_leaks),
        serde_json::Value::String(s) => {
            assert!(
                s != "a" && !s.starts_with("sk-ant-"),
                "an account id or a secret leaked: {s}"
            );
        }
        _ => {}
    }
}

#[tokio::test]
async fn the_usage_route_answers_the_callers_own_key_and_refuses_the_rest() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    let (status, headers, body) = get(&app, None).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));

    assert_eq!(get(&app, Some("phoebis-nobody")).await.0, 401);

    let (status, _, body) = get(&app, Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["key"],
        serde_json::json!({ "id": "desktop", "label": "desktop", "pool_id": "main" })
    );
    assert_eq!(body["windows"]["24h"]["requests"], 0);

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();

    let body = wait_for_route(&app, KEY).await;
    assert_eq!(
        body["windows"]["24h"]["models"][0]["model"],
        "claude-sonnet-4-5"
    );
    assert_eq!(body["windows"]["7d"]["requests"], 1);
    assert_nothing_leaks(&body);
}

#[tokio::test]
async fn a_second_key_sees_its_own_usage_and_not_the_firsts() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));
    const LAPTOP: &str = "phoebis-laptop";
    h.store
        .insert_api_key(
            &ApiKeyRow {
                id: "laptop".into(),
                label: "laptop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: h.store.api_key_digest(LAPTOP).to_vec(),
                prefix: phoebis_store::key_prefix(LAPTOP),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();

    for (key, model) in [(KEY, "claude-sonnet-4-5"), (LAPTOP, "claude-opus-5")] {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(key, model, "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.into_body().collect().await.unwrap();
    }

    let desktop = wait_for_route(&app, KEY).await;
    assert_eq!(desktop["key"]["id"], "desktop");
    assert_eq!(desktop["windows"]["24h"]["requests"], 1);
    let laptop = wait_for_route(&app, LAPTOP).await;
    assert_eq!(laptop["key"]["id"], "laptop");
    assert_eq!(laptop["windows"]["24h"]["requests"], 1);
}

#[tokio::test]
async fn a_deleted_keys_usage_stays_with_it() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let _ = wait_for_route(&app, KEY).await;

    h.store.delete_api_key("desktop", "test").unwrap();
    h.router.reload().await.unwrap();

    assert_eq!(
        get(&app, Some(KEY)).await.0,
        401,
        "the key is gone from the snapshot the route authenticates against"
    );

    assert!(matches!(
        h.store.insert_api_key(
            &ApiKeyRow {
                id: "desktop".into(),
                label: "desktop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: h.store.api_key_digest("phoebis-desktop-reissued").to_vec(),
                prefix: phoebis_store::key_prefix("phoebis-desktop-reissued"),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        ),
        Err(StoreError::KeyIdRetired(id)) if id == "desktop"
    ));

    let now = h.clock.load(Ordering::SeqCst);
    assert_eq!(
        h.db.read(|c| key_usage(c, "desktop", 0, now + 1))
            .unwrap()
            .requests,
        1,
        "the history stays under an id no key can present"
    );
}

async fn get_with_accept(
    app: &axum::Router,
    key: &str,
    accept: Option<&str>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut req = Request::builder()
        .method("GET")
        .uri("/v1/usage")
        .header("authorization", format!("Bearer {key}"));
    if let Some(accept) = accept {
        req = req.header("accept", accept);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

#[tokio::test]
async fn the_usage_page_is_html_when_accept_prefers_it_and_escapes_the_label() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    let row = h.store.api_key("desktop").unwrap().unwrap();
    h.store
        .update_api_key(
            &ApiKeyRow {
                label: "<b>Desk</b> & \"Co\"".into(),
                ..row
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();

    let (status, headers, body) = get_with_accept(
        &app,
        KEY,
        Some("text/html,application/xhtml+xml,application/json;q=0.9"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(header(&headers, "content-type")
        .unwrap()
        .starts_with("text/html"));
    assert_eq!(
        header(&headers, "content-security-policy"),
        Some("default-src 'none'; style-src 'unsafe-inline'")
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("&lt;b&gt;Desk&lt;/b&gt; &amp; &quot;Co&quot;"));
    assert!(!body.contains("<b>Desk"));
    assert!(body.contains("Last 24 hours"));
    assert!(body.contains("Last 7 days"));
    assert!(!body.contains("<script"));
    assert!(!body.contains(KEY));

    let (status, headers, _) =
        get_with_accept(&app, KEY, Some("application/json, text/html")).await;
    assert_eq!(status, 200);
    assert!(header(&headers, "content-type")
        .unwrap()
        .starts_with("application/json"));

    let (status, headers, _) = get_with_accept(&app, KEY, None).await;
    assert_eq!(status, 200);
    assert!(header(&headers, "content-type")
        .unwrap()
        .starts_with("application/json"));
}
