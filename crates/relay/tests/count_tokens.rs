//! A token count takes the cache TTL rewrite and the restamp as an inference request does, and nothing else.
mod common;
use common::*;

use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{CACHE_DIAGNOSIS_BETA, EXTENDED_TTL_BETA};
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA0001";
const OA: &str = "sk-ant-oat01-AAAA0001";

const COUNT_OK: &[u8] = br#"{"input_tokens":7}"#;

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

/// The recorded count as the client sent it, under the router's key.
fn count_request(kind: FixtureKind, fixture: BodyFixture) -> axum::http::Request<axum::body::Body> {
    request_from_fixture(inference_head(kind), body(fixture), KEY)
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
    assert_eq!(
        sent.body,
        body(BodyFixture::CountApiKey),
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
async fn under_the_policy_the_counts_markers_take_the_ttl_and_the_beta_follows_as_on_an_inference_request(
) {
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
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
        "a count with no marker gets the top-level one the policy gives an inference body with none"
    );
    assert_eq!(
        beta_list(sent).last().map(String::as_str),
        Some(EXTENDED_TTL_BETA)
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
async fn a_count_under_an_oauth_key_takes_the_same_rule_and_no_restoration() {
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
            .oneshot(count_request(kind, fixture))
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
