use axum::body::Body;
use axum::http::{Request, StatusCode};
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::RelayOutcome;
use phoebis_store::AccountKind;
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
    let mut req = request_from_fixture(head, native, "phoebis-testkey");
    // An SDK caller, so restoration never applies: this test is about the measured API-key shape, not about
    // the fields a `claude-cli/` agent gets restored, which the restoration suite covers on its own.
    set_header(&mut req, "user-agent", "anthropic-sdk-python/0.40.0");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = wire_of(&up.requests()[0]);
    let expected_head = String::from_utf8(head.to_vec()).unwrap().replace(
        "User-Agent: claude-cli/2.1.269 (external, sdk-cli)",
        "User-Agent: anthropic-sdk-python/0.40.0",
    );
    let mut expected = expected_head.into_bytes();
    expected.extend_from_slice(native);
    assert_eq!(
        got, expected,
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
    assert_eq!(
        names[..2],
        ["Accept", "x-api-key"],
        "the key takes the slot the caller's own credential header held"
    );
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
    let resp = app.oneshot(models).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v["data"].is_array(),
        "the harness's default catalog fake feeds the listing"
    );
    assert!(
        up.requests().is_empty(),
        "the catalog fetches go to catalog_up"
    );
    up.shutdown().await;
}

async fn ask_ready(app: &axum::Router, method: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri("/api/ready")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

#[tokio::test]
async fn ready_answers_503_naming_what_the_readiness_source_reads_dead() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let (status, body) = ask_ready(&h.app, "GET").await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        ),
        (StatusCode::OK, serde_json::json!({ "ready": true })),
        "a relay built without a readiness source answers ready"
    );
    let Harness { app, relay, .. } = h;
    drop(app);
    let relay = std::sync::Arc::try_unwrap(relay)
        .ok()
        .expect("the dropped app held the relay's only other handle");
    let app = phoebis_relay::router(std::sync::Arc::new(
        relay.with_readiness(std::sync::Arc::new(|| vec!["router"])),
    ));
    let (status, body) = ask_ready(&app, "GET").await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "ready": false, "dead": ["router"] })
        )
    );
    assert_eq!(
        ask_ready(&app, "HEAD").await,
        (StatusCode::SERVICE_UNAVAILABLE, Vec::new()),
        "HEAD carries the status alone"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_with_a_drifted_hash_is_refused() {
    let drifted = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen("cch=9b244", "cch=00000", 1)
        .into_bytes();
    a_drifted_stamp_is_refused(drifted, "hash").await;
}

#[tokio::test]
async fn an_uppercase_cch_counts_as_drift() {
    let drifted = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen("cch=9b244", "cch=9B244", 1)
        .into_bytes();
    a_drifted_stamp_is_refused(drifted, "hash").await;
}

/// The edited fixture, re-signed so the hash check passes and the suffix check is what decides.
fn resigned(from: &str, to: &str) -> Vec<u8> {
    let edited = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen(from, to, 1);
    phoebis_cch::sign(edited.as_bytes()).unwrap()
}

/// The fixture's line carries `cc_version=2.1.269.517`, the suffix of a prompt of three zeros under 2.1.269. A
/// suffix the client computed by another rule is refused before any account is tried, as a drifted hash is;
/// the body carries a valid hash, so the refusal is the suffix check's own.
#[tokio::test]
async fn a_version_suffix_this_build_cannot_reproduce_is_refused() {
    a_drifted_stamp_is_refused(
        resigned("cc_version=2.1.269.517", "cc_version=2.1.269.abc"),
        "version_suffix",
    )
    .await;
}

/// A bare version with no suffix has nothing to check and relays.
#[tokio::test]
async fn a_bare_version_with_no_suffix_is_not_drift() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let bare = resigned("cc_version=2.1.269.517", "cc_version=2.1.269");
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            &bare,
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(outcome(&resp).client_drift, None);
    assert_eq!(up.requests().len(), 1, "the body reached the upstream");
    up.shutdown().await;
}

/// The guard reaches the comparison on a stamped body: the same edit with the right suffix relays, so the
/// refusal above is the suffix's and not a `None` from a span the guard could not read.
#[tokio::test]
async fn a_stamped_body_with_the_right_suffix_relays() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let same = resigned("cc_version=2.1.269.517", "cc_version=2.1.269.517");
    assert_eq!(
        same,
        body(BodyFixture::FirstParty9b244),
        "re-signing the unchanged fixture reproduces its own stamp"
    );
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            &same,
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

/// A supplementary-plane character ahead of a sampled position shifts the client's own UTF-16 indexing, not
/// the relay's: a suffix computed the client's way for such a prompt still relays rather than being refused as
/// drift. The fixture's own prompt is `hi`, too short to sample anything but padding, so this splices a longer
/// one in to exercise the shift.
#[tokio::test]
async fn a_suffix_over_a_supplementary_character_prompt_relays() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    // "a\u{1D11E}bcdefghijklmnopqrstuvwxyz": UTF-16 units 4, 7, and 20 are 'c', 'f', 's'; sha256 of
    // "59cf53e54c78" + "cfs" + "2.1.269", first three hex characters, is "596".
    let spliced = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen(
            "\"text\":\"hi\"",
            "\"text\":\"a\u{1D11E}bcdefghijklmnopqrstuvwxyz\"",
            1,
        )
        .replacen("cc_version=2.1.269.517", "cc_version=2.1.269.596", 1);
    let signed = phoebis_cch::sign(spliced.as_bytes()).unwrap();
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            &signed,
            "phoebis-testkey",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(outcome(&resp).client_drift, None);
    assert_eq!(up.requests().len(), 1, "the body reached the upstream");
    up.shutdown().await;
}

/// A first-party body whose billing stamp this build cannot reproduce never reaches the upstream: the client
/// moved past the pinned profile, the caller gets a 400 that names which check refused it, and the outcome
/// carries it for the ledger.
async fn a_drifted_stamp_is_refused(drifted: Vec<u8>, kind: &str) {
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
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            &drifted,
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
    assert_eq!(outcome.client_drift.as_deref(), Some(kind));
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

/// A body the account identity cannot be written into would be billed against whatever the caller wrote, so
/// it is refused before any account is tried rather than relayed as it stands.
#[tokio::test]
async fn a_body_with_no_account_uuid_target_is_refused_with_body_shape() {
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
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", "Bearer phoebis-testkey")
        .header("User-Agent", CLAUDE_CLI_USER_AGENT)
        .body(Body::from(r#"{"model":"claude-sonnet-4-5","messages":[]}"#))
        .unwrap();
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.headers()
            .get("x-phoebis-error-code")
            .and_then(|v| v.to_str().ok()),
        Some("body_shape")
    );
    let outcome = resp
        .extensions()
        .get::<RelayOutcome>()
        .expect("a refused request carries its outcome")
        .clone();
    assert_eq!(
        (outcome.error_code.as_deref(), outcome.attempt),
        (Some("body_shape"), 0),
        "the ledger row names the refusal and no attempt was spent"
    );
    let refusal: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(refusal["error"]["type"], "invalid_request_error");
    let message = refusal["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("metadata.user_id") && !message.contains("claude-sonnet-4-5"),
        "the refusal names the member it could not find and nothing of the body: {message}"
    );
    assert!(
        up.requests().is_empty(),
        "a body the relay cannot bind is not sent upstream in any form"
    );
    up.shutdown().await;
}

/// The public `count_tokens` schema has no `metadata` member and the upstream refuses unknown top-level
/// members, so the account identity has no slot here and its absence is not a malformed body.
#[tokio::test]
async fn an_oauth_count_tokens_body_without_metadata_reaches_the_upstream() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages/count_tokens".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"input_tokens":7}"#.to_vec(),
        },
    }])
    .await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("Authorization", "Bearer phoebis-testkey")
        .header("User-Agent", CLAUDE_CLI_USER_AGENT)
        .body(Body::from(
            r#"{"model":"claude-sonnet-4-5[1m]","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = String::from_utf8(up.requests()[0].body.clone()).unwrap();
    assert_eq!(
        sent, r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}"#,
        "the model the registry resolved is spliced in and no metadata member is invented"
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
