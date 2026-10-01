mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use tower::ServiceExt;

fn ok() -> Rule {
    Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }
}

fn identity_less_request(agent: &str) -> Request<Body> {
    let mut req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    set_header(&mut req, "user-agent", agent);
    set_header(&mut req, "x-stainless-lang", "python");
    req
}

#[tokio::test]
async fn a_claude_cli_user_agent_on_the_mixed_pool_may_land_on_either_member() {
    let up = FakeUpstream::start(vec![ok()]).await;
    let h = harness(
        &up,
        &[
            oauth("oauth-member", "sk-ant-oat01-EITHER", 0),
            api_key("apikey-member", "sk-ant-api03-EITHER", 1),
        ],
        PoolSpec::default(),
    )
    .await;
    // The body carries `metadata.user_id`, so landing on the OAuth member succeeds too: the point here is the
    // class, not the identity splice.
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    set_header(&mut req, "user-agent", CLAUDE_CLI_USER_AGENT);
    let resp = h.app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = credentials_seen(&up);
    assert!(
        seen == vec!["sk-ant-oat01-EITHER".to_string()]
            || seen == vec!["sk-ant-api03-EITHER".to_string()],
        "either member may serve a Claude Code request: {seen:?}"
    );
    up.shutdown().await;
}
