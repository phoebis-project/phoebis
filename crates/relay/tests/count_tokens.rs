//! A token count's own cache markers take the cache TTL rewrite and the restamp an inference request's do,
//! and nothing else about its body changes.
mod common;
use common::*;

use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{CACHE_DIAGNOSIS_BETA, EXTENDED_TTL_BETA};
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA0001";
const OA: &str = "sk-ant-oat01-AAAA0001";

const COUNT_OK: &[u8] = br#"{"input_tokens":7}"#;

const REFUSED_TTL_BETA: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `extended-cache-ttl-2025-04-11` for the `anthropic-beta` header."}}"#;

/// Ahead of any `/v1/messages` rule in a test that serves both, since the count path matches that prefix too.
fn count_rule() -> Rule {
    Rule {
        path_prefix: "/v1/messages/count_tokens".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: COUNT_OK.to_vec(),
        },
    }
}

fn json_400(message: &str) -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: message.as_bytes().to_vec(),
    }
}

fn policy(json: &'static str) -> PoolSpec {
    PoolSpec {
        cache_ttl: json,
        ..Default::default()
    }
}

/// Every `cache_control` value's raw text, in body order.
fn markers(body: &[u8]) -> Vec<String> {
    phoebis_cch::cache_control_spans(body)
        .unwrap()
        .into_iter()
        .map(|(start, end)| String::from_utf8(body[start..end].to_vec()).unwrap())
        .collect()
}

fn beta_list(r: &fake_upstream::RecordedRequest) -> Vec<String> {
    r.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.split(',').map(str::trim).map(String::from).collect())
        .unwrap_or_default()
}

/// The recorded count with its one message turned into a marked text block: the shape the client would send if
/// it ever cached a count. The recorded bodies carry no marker at all, so nothing else can prove the rewrite.
fn marked_count(fixture: BodyFixture) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(body(fixture)).unwrap();
    let message = &mut value["messages"][0];
    let text = message["content"]
        .as_str()
        .expect("the recorded count holds one text message")
        .to_string();
    message["content"] = serde_json::json!([
        {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}
    ]);
    serde_json::to_vec(&value).unwrap()
}

/// The same body with its five-minute marker read as the hour, which is the whole edit the policy owes a count.
fn hour_marked(body: &[u8]) -> Vec<u8> {
    String::from_utf8(body.to_vec())
        .unwrap()
        .replace(
            r#"{"type":"ephemeral"}"#,
            r#"{"type":"ephemeral","ttl":"1h"}"#,
        )
        .into_bytes()
}

/// The recorded count as the client sent it, under the router's key.
fn count_request(kind: FixtureKind, fixture: BodyFixture) -> axum::http::Request<axum::body::Body> {
    request_from_fixture(inference_head(kind), body(fixture), KEY)
}

fn count_request_with(kind: FixtureKind, body: &[u8]) -> axum::http::Request<axum::body::Body> {
    request_from_fixture(inference_head(kind), body, KEY)
}

#[tokio::test]
async fn under_the_defaults_the_recorded_count_reaches_the_upstream_as_the_client_sent_it() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(&up, &[api_key("acct", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(count_request(
            FixtureKind::CountApiKey,
            BodyFixture::CountApiKey,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        sent.body == body(BodyFixture::CountApiKey),
        "no edit under the defaults"
    );
    assert!(sent.line.starts_with("POST /v1/messages/count_tokens"));
    assert!(
        beta_list(sent)
            .iter()
            .any(|b| b == "token-counting-2024-11-01"),
        "the client's own beta travels"
    );
    assert!(!beta_list(sent).iter().any(|b| b == CACHE_DIAGNOSIS_BETA));
    up.shutdown().await;
}

#[tokio::test]
async fn under_the_policy_the_recorded_count_travels_as_sent_since_it_carries_no_marker() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"main":"1h"}"#),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(count_request(
            FixtureKind::CountApiKey,
            BodyFixture::CountApiKey,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        markers(body(BodyFixture::CountApiKey)).is_empty(),
        "the client sends a count with no marker of its own"
    );
    assert!(
        sent.body == body(BodyFixture::CountApiKey),
        "the count endpoint gets no top-level member the client never sent"
    );
    assert!(
        !beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "no beta for an hour the body does not carry"
    );
    assert!(
        phoebis_cch::object_member_span(&sent.body, &["diagnostics"]).is_none(),
        "nothing to diagnose on a count"
    );
    assert!(
        phoebis_cch::string_member_span(&sent.body, &["metadata", "user_id"]).is_none(),
        "no identity slot on a count"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_marked_counts_marker_takes_the_ttl_and_the_beta_follows_as_on_an_inference_request() {
    let up = FakeUpstream::start(vec![
        count_rule(),
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
    ])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        // `in_overage` pinned off: an api_key account's default would otherwise take over `main`.
        policy(r#"{"enabled":true,"main":"1h","in_overage":null}"#),
    )
    .await;
    let marked = marked_count(BodyFixture::CountApiKey);
    let resp = h
        .app
        .clone()
        .oneshot(count_request_with(FixtureKind::CountApiKey, &marked))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
        "the marker the count carries reads the policy's hour"
    );
    assert_eq!(
        beta_list(sent).last().map(String::as_str),
        Some(EXTENDED_TTL_BETA)
    );
    assert!(
        sent.body == hour_marked(&marked),
        "the marker is the whole edit: a count carries no cch to restamp"
    );
    assert!(
        phoebis_cch::object_member_span(&sent.body, &["diagnostics"]).is_none(),
        "nothing to diagnose on a count"
    );
    assert!(
        phoebis_cch::string_member_span(&sent.body, &["metadata", "user_id"]).is_none(),
        "no identity slot on a count"
    );

    // The inference request under the same policy reads the same TTL, so the two hashes stay in step.
    let resp = h
        .app
        .clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::ApiKey),
            body(BodyFixture::ApiKey),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let inference = &up.requests()[1];
    assert!(!markers(&inference.body).is_empty());
    assert!(markers(&inference.body)
        .iter()
        .all(|m| m.contains(r#""ttl":"1h""#)));
    up.shutdown().await;
}

#[tokio::test]
async fn a_marked_count_under_an_oauth_key_takes_the_same_rule_and_no_restoration() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(
        &up,
        &[oauth("acct", OA, 0)],
        policy(r#"{"enabled":true,"main":"1h"}"#),
    )
    .await;
    for (kind, fixture) in [
        (
            FixtureKind::CountOauthNonFirstParty,
            BodyFixture::CountOauth,
        ),
        (
            FixtureKind::CountOauthFirstParty,
            BodyFixture::CountOauthFirstParty,
        ),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(count_request_with(kind, &marked_count(fixture)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    assert_eq!(up.requests().len(), 2);
    for sent in up.requests().iter() {
        assert_eq!(
            markers(&sent.body),
            vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
            "the hour reaches an oauth count as it reaches an api-key one"
        );
        assert_eq!(
            beta_list(sent).last().map(String::as_str),
            Some(EXTENDED_TTL_BETA)
        );
        assert!(
            phoebis_cch::string_member_span(&sent.body, &["system", "0", "text"]).is_none(),
            "no billing line was inserted on a count"
        );
        assert!(phoebis_cch::object_member_span(&sent.body, &["diagnostics"]).is_none());
        assert!(
            phoebis_cch::object_member_span(&sent.body, &["metadata"]).is_none(),
            "no identity slot on a count"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_400_on_a_count_strips_the_beta_for_that_leg_and_leaves_the_account_unlatched() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", json_400(REFUSED_TTL_BETA));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        // `in_overage` pinned off: an api_key account's default would otherwise take over `main`.
        policy(r#"{"enabled":true,"main":"1h","in_overage":null}"#),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(count_request_with(
            FixtureKind::CountApiKey,
            &marked_count(BodyFixture::CountApiKey),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        up.requests().len(),
        2,
        "the strip retry corrects the count's own edit on the same leg"
    );
    assert!(beta_list(&up.requests()[0])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral"}"#.to_string()],
        "the retry rewrites the count's marker to five minutes"
    );
    assert!(!beta_list(&up.requests()[1])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));

    // What the count endpoint refused says nothing about the inference requests the account serves.
    up.push_credential_rule(A, "/v1/messages", json_200());
    let resp = h
        .app
        .clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::ApiKey),
            body(BodyFixture::ApiKey),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.requests().len(), 3);
    let inference = &up.requests()[2];
    assert!(
        markers(&inference.body)
            .iter()
            .all(|m| m.contains(r#""ttl":"1h""#)),
        "the account was never latched, so the hour still reaches an inference body"
    );
    assert!(
        beta_list(inference).iter().any(|b| b == EXTENDED_TTL_BETA),
        "and its beta with it"
    );
    up.shutdown().await;
}
