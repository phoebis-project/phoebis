//! The router admits Claude Code alone and refuses any other client before the body is read, under every key, so
//! an admitted request may land on any member of a mixed pool.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use phoebis_bus::{Bus, Delivery, Event, Outcome, RequestShape, OTHER_CLIENT};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_routing::RouterEvent;
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

/// The API-key fixture's body without its `metadata` member, as a raw Messages caller sends it.
fn identity_less_body() -> Vec<u8> {
    let mut native = body(BodyFixture::ApiKey).to_vec();
    phoebis_cch::remove_top_level_member(&mut native, "metadata")
        .expect("the recording carries the member");
    native
}

/// A request under the foreign user agent `agent` that also carries the Stainless header, so the gate case shows
/// the header does not change the label.
fn identity_less_request(agent: &str) -> Request<Body> {
    let mut req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        &identity_less_body(),
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

/// The gate: a user agent outside `claude-cli/` is refused before the body is read, under a key that admits
/// Claude Code as every key does, the request row carries the code, the label and the user agent, no decision
/// is recorded for the request, and the warn line names the key and the user agent.
#[tokio::test]
async fn a_client_outside_claude_code_is_refused_before_the_body_is_read() {
    let captured = Captured::default();
    let _guard = captured.install();
    let up = FakeUpstream::start(vec![ok()]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[
            oauth("oauth-member", "sk-ant-oat01-MIXEDPOOL", 0),
            api_key("apikey-member", "sk-ant-api03-MIXEDPOOL", 1),
        ],
        PoolSpec::default(),
        bus,
    )
    .await;
    let mut decisions = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(identity_less_request(SDK_USER_AGENT))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        resp.headers()["x-phoebis-error-code"].to_str().unwrap(),
        "client_not_allowed"
    );
    assert_eq!(resp.headers()["x-should-retry"].to_str().unwrap(), "false");
    assert!(up.requests().is_empty(), "the body was never read");
    let events = drain_until_complete(&mut sub).await;
    let Some(Event::RequestObserved(observed)) = events
        .iter()
        .find(|e| matches!(e, Event::RequestObserved(_)))
    else {
        panic!("{events:?}")
    };
    assert!(
        observed.body.is_empty(),
        "the refusal is published before the body is read"
    );
    assert_eq!(observed.shape, RequestShape::default());
    assert_eq!(observed.client.class, OTHER_CLIENT);
    assert!(observed
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("user-agent") && value == SDK_USER_AGENT));
    let Some(Event::RequestComplete(complete)) = events
        .iter()
        .find(|e| matches!(e, Event::RequestComplete(_)))
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        (
            complete.outcome,
            complete.status,
            complete.error_code.as_deref()
        ),
        (Outcome::Rejected, 403, Some("client_not_allowed"))
    );
    // The request row as the ledger assembles it from the same events.
    let mut assembler = phoebis_ledger::Assembler::new(std::sync::Arc::new(|| 0));
    let row = events
        .iter()
        .cloned()
        .flat_map(|e| assembler.observe(e))
        .find_map(|record| match record {
            phoebis_ledger::Record::Request { request, .. } => Some(request),
            _ => None,
        })
        .expect("the refusal's request row");
    assert_eq!(
        (
            row.error_code.as_deref(),
            row.client.as_deref(),
            row.user_agent.as_deref()
        ),
        (
            Some("client_not_allowed"),
            Some(OTHER_CLIENT),
            Some(SDK_USER_AGENT)
        )
    );
    // A refusal's decision is a command the relay queues on the router task before it answers, so once a pins read
    // queued after the completion comes back, the task has emitted any decision: none names the request.
    let _ = h.router.pins().await;
    while let Ok(event) = decisions.try_recv() {
        assert!(
            !matches!(&event, RouterEvent::Decision { record, .. } if record.request.request_id == row.id),
            "{event:?}: no decision is recorded for a request refused before its body"
        );
    }
    let key_id = observed.key_id.as_deref().expect("the row names the key");
    let log = captured.text();
    assert!(
        log.lines().any(|line| line.contains("WARN")
            && line.contains(&format!("key={key_id}"))
            && line.contains(SDK_USER_AGENT)),
        "{log}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_claude_code_agent_is_admitted() {
    let up = FakeUpstream::start(vec![ok()]).await;
    let h = harness(
        &up,
        &[api_key("apikey-only", "sk-ant-api03-CCADMIT", 0)],
        PoolSpec::default(),
    )
    .await;
    // The recorded Claude Code request, since the router requires its `metadata.user_id` of that client.
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
    assert_eq!(resp.status(), StatusCode::OK);
    up.shutdown().await;
}
