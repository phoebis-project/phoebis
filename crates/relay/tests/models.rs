//! `GET /v1/models` and `GET /v1/models/{*id}`: the caller's key answered with what its pool may route to.
mod common;
use std::sync::atomic::Ordering;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_store::{ApiKeyRow, CooldownRow, RulesMode};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAAAAAAAAAAAAAA";
const B: &str = "sk-ant-oat01-BBBBBBBBBBBBBBBB";

fn model_row(id: &str, display_name: &str, max_input_tokens: u64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "display_name": display_name,
        "created_at": "2025-09-29T00:00:00Z",
        "max_input_tokens": max_input_tokens,
        "max_tokens": 64_000,
    })
}

/// `claude-sonnet-4-5-20250929` and `claude-opus-5` above the long-context threshold, `claude-haiku-4-5-20251001`
/// and `vendor/model-a` at it exactly, so the twin rule's boundary and a slash-carrying id are both exercised.
fn catalog_rows() -> Vec<Rule> {
    vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::json!({
                "data": [
                    model_row("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5", 1_000_000),
                    model_row("claude-opus-5", "Claude Opus 5", 1_000_000),
                    model_row("claude-haiku-4-5-20251001", "Claude Haiku 4.5", 200_000),
                    model_row("vendor/model-a", "Vendor Model A", 200_000),
                ],
            })
            .to_string()
            .into_bytes(),
        },
    }]
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn ids_of(v: &serde_json::Value) -> Vec<&str> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect()
}

fn error_code(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.headers()
        .get("x-phoebis-error-code")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn should_retry(resp: &axum::http::Response<axum::body::Body>) -> Option<&str> {
    resp.headers()
        .get("x-should-retry")
        .map(|value| value.to_str().unwrap())
}

#[tokio::test]
async fn the_listing_answers_the_callers_key_with_each_list_level_applied() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["NOT claude-opus*"]),
            ..PoolSpec::default()
        },
    )
    .await;
    let mut desktop = h.store.api_key("desktop").unwrap().unwrap();
    desktop.model_rules = rules(&["NOT claude-haiku*"]);
    desktop.model_rules_mode = RulesMode::Extend;
    h.store
        .update_api_key(&desktop, desktop.updated_at, "test")
        .unwrap();
    h.router.reload().await.unwrap();

    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        ids_of(&body),
        vec![
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929[1m]",
            "vendor/model-a",
        ],
        "the pool's opus denial and the key's own haiku denial both applied"
    );

    h.store
        .insert_api_key(
            &ApiKeyRow {
                id: "wide".into(),
                label: "wide".into(),
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
                secret_digest: h.store.api_key_digest("phoebis-wide").to_vec(),
                prefix: phoebis_store::key_prefix("phoebis-wide"),
                incarnation: 1,
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
        .oneshot(models_request("phoebis-wide", "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        ids_of(&body),
        vec![
            "claude-haiku-4-5-20251001",
            "claude-opus-5",
            "claude-opus-5[1m]",
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929[1m]",
            "vendor/model-a",
        ],
        "replace drops the pool's list, so opus and haiku both answer"
    );
}

#[tokio::test]
async fn the_twins_are_switched_off_per_key() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut desktop = h.store.api_key("desktop").unwrap().unwrap();
    desktop.long_context_twins = false;
    h.store
        .update_api_key(&desktop, desktop.updated_at, "test")
        .unwrap();
    h.router.reload().await.unwrap();

    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        ids_of(&body).iter().all(|id| !id.ends_with("[1m]")),
        "no twin id is listed"
    );
}

#[tokio::test]
async fn a_member_whose_fetch_fails_contributes_nothing_and_a_cooled_model_still_shows() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    catalog_up.add_credential_rule(
        A,
        "/v1/models?limit=1000",
        Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let h = harness_with_catalog(
        &up,
        catalog_up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;

    let now = h.clock.load(Ordering::SeqCst);
    h.router
        .report(phoebis_routing::Report::Cooldown {
            account_id: "b".into(),
            row: CooldownRow {
                account_id: "b".into(),
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

    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body_text = String::from_utf8(bytes.to_vec()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(
        ids_of(&v),
        vec![
            "claude-haiku-4-5-20251001",
            "claude-opus-5",
            "claude-opus-5[1m]",
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929[1m]",
            "vendor/model-a",
        ],
        "b's rows answer in full; a's failed fetch added nothing and the cooldown filtered nothing, and no
         member id names an account in the body"
    );
}

#[tokio::test]
async fn the_single_route_takes_a_slash_and_a_percent_encoded_id_and_404s_the_rest() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;

    for path in ["/v1/models/vendor/model-a", "/v1/models/vendor%2Fmodel-a"] {
        let resp = h
            .app
            .clone()
            .oneshot(models_request(KEY, path))
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK, "{path}");
        let v = body_json(resp).await;
        assert_eq!(v["id"], "vendor/model-a");
        assert_eq!(v["type"], "model");
    }

    let resp = h
        .app
        .clone()
        .oneshot(models_request(
            KEY,
            "/v1/models/claude-sonnet-4-5-20250929%5B1m%5D",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["id"], "claude-sonnet-4-5-20250929[1m]");

    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models/claude-nope"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(error_code(&resp), "model_not_found");
}

#[tokio::test]
async fn the_page_is_anthropics_shape() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;

    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models?limit=1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["has_more"], true);
    let first_id = v["first_id"].as_str().unwrap().to_string();
    assert_eq!(v["last_id"].as_str().unwrap(), first_id);

    let resp = h
        .app
        .clone()
        .oneshot(models_request(
            KEY,
            &format!("/v1/models?limit=1&after_id={first_id}"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let next = body_json(resp).await;
    assert!(
        next["data"][0]["id"].as_str().unwrap() > first_id.as_str(),
        "the second page starts past the first row"
    );

    for bad in ["/v1/models?limit=0", "/v1/models?limit=1001"] {
        let resp = h
            .app
            .clone()
            .oneshot(models_request(KEY, bad))
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(error_code(&resp), "invalid_limit");
    }
}

#[tokio::test]
async fn a_missing_or_wrong_key_is_refused_as_messages_refuses_it() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;

    let no_key = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/models")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(no_key).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&resp), "auth_missing");
    assert_eq!(should_retry(&resp), Some("false"));

    h.store.revoke_api_key("desktop", NOW, "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(models_request(KEY, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&resp), "auth_unknown");
    assert_eq!(should_retry(&resp), Some("false"));
}

#[tokio::test]
async fn a_bad_limit_with_no_key_answers_the_authentication_refusal_first() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;

    let no_key = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/models?limit=bad")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(no_key).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&resp), "auth_missing");
    assert_eq!(should_retry(&resp), Some("false"));
}

/// The listing reads the key alone, so a caller outside Claude Code that holds a key reads the list the client
/// reads.
#[tokio::test]
async fn the_listing_answers_a_curl_user_agent_on_any_key() {
    let up = FakeUpstream::start(vec![]).await;
    let catalog_up = FakeUpstream::start(catalog_rows()).await;
    let h = harness_with_catalog(&up, catalog_up, &[oauth("a", A, 0)], PoolSpec::default()).await;

    let mut curl = models_request(KEY, "/v1/models");
    set_header(&mut curl, "user-agent", "curl/8.0.1");
    let resp = h.app.clone().oneshot(curl).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let listed = body_json(resp).await;
    let mut native = models_request(KEY, "/v1/models");
    set_header(&mut native, "user-agent", CLAUDE_CLI_USER_AGENT);
    let resp = h.app.clone().oneshot(native).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let native = body_json(resp).await;
    assert!(!ids_of(&listed).is_empty(), "{listed}");
    assert_eq!(ids_of(&listed), ids_of(&native), "the key's list");
}
