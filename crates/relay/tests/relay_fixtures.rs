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
async fn oauth_non_first_party_request_passes_through_with_the_uuid_edit_and_the_restoration() {
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
    let head = inference_head(FixtureKind::OauthNonFirstParty);
    let native = body(BodyFixture::Oauth);
    let resp = app
        .oneshot(request_from_fixture(head, native, "phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // The recorded head already carries the account's own token, so the credential slot reads as recorded. The
    // body takes the account's uuid and the placeholder stamp the reduced shape lacks, and the head takes the
    // client request id after `x-app`, read off the relayed request since it is fresh.
    let got = wire_of(&up.requests()[0]);
    let client_request_id = up.requests()[0]
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("x-client-request-id"))
        .map(|(_, v)| v.clone())
        .expect("the restoration sends a client request id");
    let edited = String::from_utf8(native.to_vec())
        .unwrap()
        .replacen(
            r#"\"account_uuid\":\"\""#,
            &format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#),
            1,
        )
        .replacen(
            "cc_entrypoint=sdk-cli;",
            "cc_entrypoint=sdk-cli; cch=00000;",
            1,
        );
    let mut expected_head = String::from_utf8(head.to_vec())
        .unwrap()
        .replace(
            &format!("Content-Length: {}", native.len()),
            &format!("Content-Length: {}", edited.len()),
        )
        .replace(
            "x-app: cli\r\n",
            &format!("x-app: cli\r\nx-client-request-id: {client_request_id}\r\n"),
        );
    // The request was built from the client's head, which the harness ends with the class header the recorded
    // head predates.
    let end = expected_head.rfind("\r\n\r\n").unwrap();
    expected_head.insert_str(end + 2, "x-claude-code-request-class: main\r\n");
    let mut expected = expected_head.into_bytes();
    expected.extend_from_slice(edited.as_bytes());
    assert_eq!(
        got, expected,
        "an OAuth fixture round-trips byte-identical but for the account's uuid and what the restoration adds"
    );
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
    // The fixture's own Claude Code head with its session header, so the relay restores what the client's reduced
    // shape lacks: the placeholder stamp in the attribution line and the client request id, a header it writes after
    // `x-app`. The expected wire carries both where the relayed request carries them, the id read off that request
    // since it is fresh.
    let req = request_from_fixture(head, native, "phoebis-testkey");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = wire_of(&up.requests()[0]);
    let client_request_id = up.requests()[0]
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("x-client-request-id"))
        .map(|(_, v)| v.clone())
        .expect("the restoration sends a client request id");
    let restored = String::from_utf8(native.to_vec()).unwrap().replacen(
        "cc_entrypoint=sdk-cli;",
        "cc_entrypoint=sdk-cli; cch=00000;",
        1,
    );
    let mut expected_head = String::from_utf8(head.to_vec())
        .unwrap()
        .replace(
            &format!("Content-Length: {}", native.len()),
            &format!("Content-Length: {}", restored.len()),
        )
        .replace(
            "x-app: cli\r\n",
            &format!("x-app: cli\r\nx-client-request-id: {client_request_id}\r\n"),
        );
    // The request was built from the client's head, which the harness ends with the class header the recorded
    // head predates.
    let end = expected_head.rfind("\r\n\r\n").unwrap();
    expected_head.insert_str(end + 2, "x-claude-code-request-class: main\r\n");
    let mut expected = expected_head.into_bytes();
    expected.extend_from_slice(restored.as_bytes());
    assert_eq!(
        got, expected,
        "an API-key fixture round-trips byte-identical but for what the restoration adds: no uuid, key in its slot"
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
    let native = body(BodyFixture::FirstParty);
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

/// The second first-party body, since its stamp carries a hex letter: the uppercase spelling of the right digits.
#[tokio::test]
async fn an_uppercase_cch_counts_as_drift() {
    let drifted = String::from_utf8(body(BodyFixture::FirstPartyAgain).to_vec())
        .unwrap()
        .replacen("cch=2c366", "cch=2C366", 1)
        .into_bytes();
    assert_ne!(
        drifted,
        body(BodyFixture::FirstPartyAgain),
        "the edit reached the fixture's stamp"
    );
    a_drifted_stamp_is_refused(drifted, "hash").await;
}

/// A readable hash with digits this build does not compute over the body is what a client hashing by newer
/// rules sends; the uppercase case never reaches the comparison, so this is the one that drives it.
#[tokio::test]
async fn a_lowercase_hash_this_build_does_not_compute_counts_as_drift() {
    let drifted = String::from_utf8(body(BodyFixture::FirstParty).to_vec())
        .unwrap()
        .replacen("cch=91984", "cch=91985", 1)
        .into_bytes();
    a_drifted_stamp_is_refused(drifted, "hash").await;
}

/// The edited fixture, re-signed so the hash check passes and the suffix check is what decides.
fn resigned(from: &str, to: &str) -> Vec<u8> {
    let edited = String::from_utf8(body(BodyFixture::FirstParty).to_vec())
        .unwrap()
        .replacen(from, to, 1);
    phoebis_cch::sign(edited.as_bytes()).unwrap()
}

/// The fixture's line carries `cc_version=2.1.310.077`, the suffix of a prompt of three zeros under 2.1.310. A
/// suffix the client computed by another rule is refused before any account is tried, as a drifted hash is;
/// the body carries a valid hash, so the refusal is the suffix check's own.
#[tokio::test]
async fn a_version_suffix_this_build_cannot_reproduce_is_refused() {
    a_drifted_stamp_is_refused(
        resigned("cc_version=2.1.310.077", "cc_version=2.1.310.abc"),
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
    let bare = resigned("cc_version=2.1.310.077", "cc_version=2.1.310");
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
    // "59cf53e54c78" + "cfs" + "2.1.310", first three hex characters, is "7cf".
    let spliced = String::from_utf8(body(BodyFixture::FirstParty).to_vec())
        .unwrap()
        .replacen(
            "\"text\":\"hi\"",
            "\"text\":\"a\u{1D11E}bcdefghijklmnopqrstuvwxyz\"",
            1,
        )
        .replacen("cc_version=2.1.310.077", "cc_version=2.1.310.7cf", 1);
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
/// moved past the pinned profile, the caller gets a 403 that names which check refused it, and the outcome
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
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
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
    assert_eq!(refusal["error"]["type"], "permission_error");
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
        .header("user-agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", fresh_session())
        .header("x-claude-code-request-class", "main")
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
