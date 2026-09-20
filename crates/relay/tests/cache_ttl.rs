mod common;
use common::*;

use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::EXTENDED_TTL_BETA;
use phoebis_store::OveragePolicy;
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA0001";
const B: &str = "sk-ant-api03-BBBB0001";
const OA: &str = "sk-ant-oat01-AAAA0001";
const OB: &str = "sk-ant-oat01-BBBB0001";

const REFUSED_TTL_BETA: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `extended-cache-ttl-2025-04-11` for the `anthropic-beta` header."}}"#;

fn policy(json: &'static str) -> PoolSpec {
    PoolSpec {
        cache_ttl: json,
        ..Default::default()
    }
}

fn json_400(message: &str) -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: message.as_bytes().to_vec(),
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

/// Every `cache_control` member removed, so a policy sees a body with no marker anywhere.
fn without_markers(native: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(native).unwrap();
    let strip = |v: &mut serde_json::Value| {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("cache_control");
        }
    };
    for field in ["system", "tools"] {
        if let Some(items) = value.get_mut(field).and_then(|v| v.as_array_mut()) {
            for item in items {
                strip(item);
            }
        }
    }
    if let Some(messages) = value.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for message in messages {
            if let Some(content) = message.get_mut("content").and_then(|v| v.as_array_mut()) {
                for block in content {
                    strip(block);
                }
            }
        }
    }
    serde_json::to_vec(&value).unwrap()
}

#[tokio::test]
async fn every_marker_is_rewritten_to_the_policys_ttl_and_the_beta_follows_the_body() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"main":"1h","subagent":"5m"}"#),
    )
    .await;

    // Main: every five-minute marker the api-key fixture carries is rewritten to an hour, and the beta the
    // fixture never sends is appended once.
    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    let betas = beta_list(sent);
    assert_eq!(
        betas
            .iter()
            .filter(|b| b.as_str() == EXTENDED_TTL_BETA)
            .count(),
        1
    );
    assert_eq!(betas.last().map(String::as_str), Some(EXTENDED_TTL_BETA));

    // A subagent turn on the same policy stays at five minutes, the fixture's own shape, and asks for no beta.
    let mut sub = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    set_header(&mut sub, "x-claude-code-agent-id", "sub-1");
    let resp = h.app.clone().oneshot(sub).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[1];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3]
    );
    assert!(!beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA));

    // The first-party fixture under an oauth account, subagent five minutes: the fixture already carries the
    // extended beta and three one-hour markers (one with `scope: global`), so the subagent turn rewrites every
    // marker down and drops the beta the caller sent.
    let oh = harness(
        &up,
        &[oauth("o", OA, 0)],
        policy(r#"{"enabled":true,"main":"1h","subagent":"5m"}"#),
    )
    .await;
    let mut sub_oauth = request_from_fixture(
        inference_head(FixtureKind::OauthFirstParty),
        body(BodyFixture::FirstParty9b244),
        KEY,
    );
    set_header(&mut sub_oauth, "x-claude-code-agent-id", "sub-1");
    let resp = oh.app.clone().oneshot(sub_oauth).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[2];
    assert_eq!(
        markers(&sent.body),
        vec![
            r#"{"type":"ephemeral"}"#.to_string(),
            r#"{"type":"ephemeral","scope":"global"}"#.to_string(),
            r#"{"type":"ephemeral"}"#.to_string(),
        ],
        "three markers stand, none added or removed"
    );
    assert!(!beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        phoebis_cch::sign(&sent.body).unwrap(),
        sent.body,
        "the restamp covers the rewrite"
    );

    up.shutdown().await;
}

#[tokio::test]
async fn a_body_with_no_marker_gets_the_top_level_member_and_a_body_with_any_marker_keeps_its_count(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], policy(r#"{"enabled":true}"#)).await;

    let stripped = without_markers(body(BodyFixture::Oauth));
    let req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &stripped,
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
        "the wire carries exactly the top-level member"
    );
    let text = String::from_utf8_lossy(&sent.body);
    let cache_at = text
        .find("\"cache_control\"")
        .expect("the member is present");
    let stream_at = text.find("\"stream\"").expect("the fixture body streams");
    assert!(cache_at < stream_at, "the top-level member precedes stream");

    let req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[1].body).len(),
        3,
        "a body with markers keeps its count"
    );

    up.shutdown().await;
}

#[tokio::test]
async fn the_per_model_override_and_the_in_overage_value_apply_and_overage_is_read_per_attempt() {
    const HAIKU: &str = "claude-haiku-4-5-20251001";
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        OA,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "allowed".into(),
                ),
                (
                    "anthropic-ratelimit-unified-overage-in-use".into(),
                    "true".into(),
                ),
            ],
            body: b"{}".to_vec(),
        },
    );
    up.add_credential_rule(OB, "/v1/messages", json_200());
    let h = harness_with(
        &up,
        &[oauth("a", OA, 0), oauth("b", OB, 1)],
        PoolSpec {
            cache_ttl: r#"{"enabled":true,"main":"1h","in_overage":"5m","models":[{"model":"claude-haiku-4-5-20251001","main":"5m","subagent":"5m"}]}"#,
            overage_policy: OveragePolicy::Allow,
            ..Default::default()
        },
        &["claude-sonnet-4-5", "claude-opus-5", HAIKU],
        default_limits(),
    )
    .await;

    // First request: `a` has no overage evidence yet, so the policy's main rule (an hour) applies.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "a");
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );

    // Second request, same session: `allow` keeps the pin on `a`, whose first response reported overage, so
    // this attempt reads the in-overage value instead.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "a");
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "a is in overage"
    );

    // A 429 on `a` fails the session over to `b`: the failed attempt still priced against a's overage state,
    // and b's own attempt reads its own (unexhausted) state.
    up.push_credential_rule(OA, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        markers(&up.requests()[2].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "a's failed attempt still priced against its own overage"
    );
    assert_eq!(
        markers(&up.requests()[3].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3],
        "b is not in overage"
    );

    // A haiku request in the same session: a's cooldown from the 429 excludes it, and the per-model override
    // on b (not in overage) reads five minutes regardless.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, HAIKU, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        markers(&up.requests()[4].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3]
    );

    up.shutdown().await;
}

#[tokio::test]
async fn a_refused_extended_ttl_beta_reverts_the_body_to_five_minutes_and_stays_reverted_on_that_account(
) {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", json_400(REFUSED_TTL_BETA));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("a", A, 0), api_key("b", B, 1)],
        policy(r#"{"enabled":true,"main":"1h"}"#),
    )
    .await;

    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        up.requests().len(),
        2,
        "the strip retry is a second upstream request on this leg"
    );
    assert_eq!(
        up.requests()[0].credential(),
        up.requests()[1].credential(),
        "both attempts went out on the same account"
    );
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    assert!(beta_list(&up.requests()[0])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "the retry rewrites every marker to five minutes"
    );
    assert!(!beta_list(&up.requests()[1])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        phoebis_cch::sign(&up.requests()[1].body).unwrap(),
        up.requests()[1].body,
        "the retry restamps"
    );
    assert_eq!(outcome(&resp).cache_ttl_ms, Some(300_000));

    // The account is latched against the beta: the next request on it skips straight to five minutes.
    up.push_credential_rule(A, "/v1/messages", json_200());
    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.requests().len(), 3);
    assert_eq!(
        markers(&up.requests()[2].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3]
    );
    assert!(!beta_list(&up.requests()[2])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));

    // A fresh account never latched serves another session at the full policy, undisturbed by a's history.
    h.store.set_enabled("a", false).unwrap();
    h.router.reload().await.unwrap();
    up.push_credential_rule(B, "/v1/messages", json_200());
    let mut other_session = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    set_header(&mut other_session, "x-claude-code-session-id", "s2");
    let resp = h.app.clone().oneshot(other_session).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        markers(&up.requests()[3].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    assert!(beta_list(&up.requests()[3])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));

    up.shutdown().await;
}

#[tokio::test]
async fn the_policy_is_off_by_default_and_never_touches_a_count_tokens_body() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // An enabled policy, not the default: a count_tokens body has to be exempted from a policy that would
    // otherwise rewrite it, not merely left alone because nothing was on.
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"main":"1h"}"#),
    )
    .await;
    let mut count_tokens = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    *count_tokens.uri_mut() = "/v1/messages/count_tokens".parse().unwrap();
    let resp = h.app.clone().oneshot(count_tokens).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "count_tokens is left as sent even with the policy enabled elsewhere"
    );
    assert!(
        !beta_list(&up.requests()[0])
            .iter()
            .any(|b| b == EXTENDED_TTL_BETA),
        "count_tokens's public schema has no slot for the beta either"
    );
    up.shutdown().await;
}
