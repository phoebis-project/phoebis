use axum::body::Body;
use axum::http::{Request, StatusCode};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::RelayOutcome;
use phoebis_store::AccountKind;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;
use common::*;

#[tokio::test]
async fn oauth_first_party_request_is_byte_identical_except_the_two_edits() {
    let up = FakeUpstream::start(vec![Rule { path_prefix: "/v1/messages".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec() } }]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let head = inference_head(FixtureKind::OauthFirstParty);
    let native = body(BodyFixture::FirstParty9b244);
    let resp = app
        .oneshot(request_from_fixture(head, native, "phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = up.requests();
    let got = wire_of(&recorded[0]);
    // The expected body is built without the relay's own editor: a plain splice of the uuid into the escaped
    // metadata string, then a signature. An oracle that called edit_body would only restate the implementation.
    let expected_body = phoebis_cch::sign(
        String::from_utf8(native.to_vec())
            .unwrap()
            .replacen(
                r#"\"account_uuid\":\"\""#,
                &format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#),
                1,
            )
            .as_bytes(),
    )
    .unwrap();
    let expected_head = String::from_utf8(head.to_vec()).unwrap().replace(
        &format!("Content-Length: {}", native.len()),
        &format!("Content-Length: {}", expected_body.len()),
    );
    let mut expected = expected_head.into_bytes();
    expected.extend_from_slice(&expected_body);
    assert_eq!(got, expected);
    assert_ne!(
        phoebis_cch::find_cch(&expected_body)
            .map(|l| expected_body[l.digits_start..l.digits_start + 5].to_vec()),
        Some(b"9b244".to_vec()),
        "the uuid edit changes the hash"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn oauth_non_first_party_request_passes_through_with_only_the_uuid_edit() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let native = body(BodyFixture::Oauth);
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthNonFirstParty),
            native,
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = &up.requests()[0];
    assert!(got
        .headers
        .iter()
        .any(|(n, v)| n == "Authorization" && v == "Bearer sk-ant-oat01-PLACEHOLDER"));
    assert!(std::str::from_utf8(&got.body)
        .unwrap()
        .contains(&format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#)));
    up.shutdown().await;
}

#[tokio::test]
async fn api_key_request_emits_the_measured_api_key_shape() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    let head = inference_head(FixtureKind::ApiKey);
    let native = body(BodyFixture::ApiKey);
    let resp = app
        .oneshot(request_from_fixture(head, native, "phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = wire_of(&up.requests()[0]);
    assert_eq!(
        got,
        [head, native].concat(),
        "an API-key fixture round-trips byte-identical: no uuid, no cch, key in its slot"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_oauth_client_served_by_an_api_key_account_loses_the_oauth_beta() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthNonFirstParty),
            body(BodyFixture::Oauth),
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = &up.requests()[0];
    let beta = got
        .headers
        .iter()
        .find(|(n, _)| n == "anthropic-beta")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(!beta.contains("oauth-2025-04-20"));
    assert!(!got.headers.iter().any(|(n, _)| n == "Authorization"));
    let names: Vec<&str> = got.headers.iter().map(|(n, _)| n.as_str()).collect();
    let av = names
        .iter()
        .position(|n| *n == "anthropic-version")
        .unwrap();
    assert_eq!(names[av + 1], "x-api-key");
    assert_eq!(names[av + 2], "x-app");
    up.shutdown().await;
}

#[tokio::test]
async fn model_suffix_is_stripped_without_changing_the_cch() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let native = body(BodyFixture::FirstParty9b244);
    let suffixed = String::from_utf8(native.to_vec()).unwrap().replacen(
        r#""model":"claude-sonnet-4-5""#,
        r#""model":"claude-sonnet-4-5[1m]""#,
        1,
    );
    // Same splice-and-sign oracle as the byte-equality test: the suffix must leave no trace in the body.
    let expected = phoebis_cch::sign(
        String::from_utf8(native.to_vec())
            .unwrap()
            .replacen(
                r#"\"account_uuid\":\"\""#,
                &format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#),
                1,
            )
            .as_bytes(),
    )
    .unwrap();
    let head = String::from_utf8(inference_head(FixtureKind::OauthFirstParty).to_vec())
        .unwrap()
        .replace(
            &format!("Content-Length: {}", native.len()),
            &format!("Content-Length: {}", suffixed.len()),
        );
    let resp = app
        .oneshot(request_from_fixture(
            head.as_bytes(),
            suffixed.as_bytes(),
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = up.requests();
    assert_eq!(
        recorded[0].body, expected,
        "the suffix is gone and the cch equals the unsuffixed edit"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn local_rejections_are_anthropic_shaped() {
    let up = FakeUpstream::start(vec![]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let bad = Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("Authorization", "Bearer wrong")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(bad).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "authentication_error");
    let both = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", "Bearer phoebis-testkey")
        .header("x-api-key", "phoebis-testkey")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(both).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let hello = Request::builder()
        .method("HEAD")
        .uri("/api/hello")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(hello).await.unwrap().status(),
        StatusCode::OK
    );
    let models = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("Authorization", "Bearer phoebis-testkey")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(models).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_with_a_drifted_hash_is_refused() {
    a_drifted_stamp_is_refused("cch=00000").await;
}

#[tokio::test]
async fn an_uppercase_cch_counts_as_drift() {
    a_drifted_stamp_is_refused("cch=9B244").await;
}

/// A first-party body whose billing hash this build cannot reproduce never reaches the upstream: the client
/// moved past the pinned profile, the caller gets a 400 that names the drift, and the outcome carries it for
/// the ledger.
async fn a_drifted_stamp_is_refused(stamp: &str) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let drifted = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen("cch=9b244", stamp, 1);
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            drifted.as_bytes(),
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let outcome = resp
        .extensions()
        .get::<RelayOutcome>()
        .expect("a refused request carries its outcome")
        .clone();
    assert_eq!(outcome.client_drift.as_deref(), Some("hash"));
    assert_eq!(outcome.attempt, 0);
    assert_eq!(
        resp.headers()
            .get("x-phoebis-error-code")
            .and_then(|v| v.to_str().ok()),
        Some("client_drift")
    );
    let refusal: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(refusal["error"]["type"], "invalid_request_error");
    assert!(
        up.requests().is_empty(),
        "a drifted body is not sent upstream in any form"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_wrong_key_is_refused_before_the_body_is_read() {
    let up = FakeUpstream::start(vec![]).await;
    let app = relay_with_limit(AccountKind::Oauth, &up, 1 << 10).await;
    let oversized = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", "Bearer wrong")
        .body(Body::from(vec![b'x'; 8 << 10]))
        .unwrap();
    let resp = app.oneshot(oversized).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the key is checked before the body is buffered, so this is never a 413"
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_nested_past_the_scanner_bound_is_refused() {
    // Authenticated, so it gets past the key check and reaches the body scanners; the process has to survive
    // it and answer, rather than recursing until the stack ends.
    let up = FakeUpstream::start(vec![]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let body = format!(
        r#"{{"deep":{}1{},"model":"claude-sonnet-4-5"}}"#,
        "[".repeat(10_000),
        "]".repeat(10_000)
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", "Bearer phoebis-testkey")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        up.requests().is_empty(),
        "a body the relay cannot read never reaches the upstream"
    );
    up.shutdown().await;
}
