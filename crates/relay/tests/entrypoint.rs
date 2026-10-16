//! The user agent's entrypoint labels the client whatever the billing line's own `cc_entrypoint` names: the line is
//! the client's attribution, so a disagreement is logged and never refused.

mod common;

use axum::http::StatusCode;
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use phoebis_bus::{Bus, Delivery, Event, Outcome};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
/// The third-party desktop's agent as the ledger recorded it: a third token follows the entrypoint's position.
const DESKTOP_AGENT: &str = "claude-cli/2.1.295 (external, claude-desktop-3p, agent-sdk/0.3.295)";

#[tokio::test]
async fn a_billing_line_naming_another_entrypoint_than_the_user_agent_is_relayed_and_the_user_agents_token_labels_the_client(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    // The fixture carries `metadata.user_id`, so the OAuth member splices its identity, and the placeholder line
    // names `sdk-py` against the agent's `claude-desktop-3p`.
    let mut with_line: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).unwrap();
    with_line["system"][0]["text"] = serde_json::Value::String(PLACEHOLDER_LINE.into());
    let native = serde_json::to_vec(&with_line).unwrap();
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &native,
        KEY,
    );
    set_header(&mut req, "user-agent", DESKTOP_AGENT);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-phoebis-error-code").is_none());
    let events = drain_until_complete(&mut sub).await;
    let Some(Event::RequestObserved(observed)) = events
        .iter()
        .find(|e| matches!(e, Event::RequestObserved(_)))
    else {
        panic!("{events:?}")
    };
    assert_eq!(observed.client.class, "claude-code/claude-desktop-3p");
    let Some(Event::RequestComplete(complete)) = events
        .iter()
        .find(|e| matches!(e, Event::RequestComplete(_)))
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (Outcome::Succeeded, None)
    );
    assert_eq!(credentials_seen(&up), vec![A.to_string()]);
    up.shutdown().await;
}
