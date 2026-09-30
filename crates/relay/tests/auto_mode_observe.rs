//! What the bus learns about auto mode at admission: a request that asked for the server-side check and had it
//! passed through, and one the key's setting stripped it from; the answer reaches the caller byte for byte.
mod common;
use common::*;

use std::time::Duration;

use phoebis_bus::{AutoMode, Bus, Event, RequestObserved};
use phoebis_profile::fixtures::{
    body, inference_head, with_dangerous_tool_use_beta, with_safeguards, BodyFixture, FixtureKind,
};
use fake_upstream::{FakeUpstream, RecordedRequest, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const OA: &str = "sk-ant-oat01-AAAA0001";

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1}}}"#;
const TOOL_USE_START: &str = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"Bash","input":{}}}"#;
const DELTA_NOT_FLAGGED: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{"toolu_01":{"type":"evaluated","outcome":"not_flagged"}}}}]},"usage":{"output_tokens":52}}"#;
const DELTA_WITHOUT_MEMBER: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":52}}"#;
const MESSAGE_STOP: &str = r#"{"type":"message_stop"}"#;

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

/// A tool call streamed with no `input_json_delta`, closed by `delta`.
fn tool_call_stream(delta: &str) -> Vec<SseEvent> {
    vec![
        ev("message_start", MESSAGE_START),
        ev("content_block_start", TOOL_USE_START),
        ev("message_delta", delta),
        ev("message_stop", MESSAGE_STOP),
    ]
}

/// The bytes the fake upstream writes for `events`, which the caller must receive unchanged.
fn wire(events: &[SseEvent]) -> Vec<u8> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e.event, e.data))
        .collect::<String>()
        .into_bytes()
}

fn sse(events: Vec<SseEvent>) -> Respond {
    Respond::Sse {
        status: 200,
        headers: vec![],
        events,
        delay_between: Duration::from_millis(2),
    }
}

fn account() -> TestAccount {
    TestAccount {
        uuid: Some(ACCOUNT_UUID),
        ..oauth("acct", OA, 0)
    }
}

fn key_off() -> PoolSpec {
    PoolSpec {
        key_auto_mode_server: Some(false),
        ..PoolSpec::default()
    }
}

struct Observed {
    answer: Vec<u8>,
    observed: RequestObserved,
    sent: Vec<RecordedRequest>,
}

/// One request through a harness with a bus attached, answered by `respond`.
async fn observe(pool: PoolSpec, respond: Respond, head: &[u8], sent_body: &[u8]) -> Observed {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(&up, &[account()], pool, bus).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_from_fixture(head, sent_body, KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let answer = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    let events = drain_until_complete(&mut sub).await;
    let observed = events
        .iter()
        .find_map(|e| match e {
            Event::RequestObserved(o) => Some(o.clone()),
            _ => None,
        })
        .expect("the request was observed");
    let sent = up.requests();
    up.shutdown().await;
    Observed {
        answer,
        observed,
        sent,
    }
}

fn server_check_head() -> Vec<u8> {
    with_dangerous_tool_use_beta(inference_head(FixtureKind::OauthNonFirstParty))
}

fn server_check_body() -> Vec<u8> {
    with_safeguards(body(BodyFixture::Oauth))
}

#[tokio::test]
async fn a_server_check_is_observed_as_server_and_its_stream_passes_through_unchanged() {
    let events = tool_call_stream(DELTA_NOT_FLAGGED);
    let r = observe(
        PoolSpec::default(),
        sse(events.clone()),
        &server_check_head(),
        &server_check_body(),
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::Server));
    assert_eq!(
        r.answer,
        wire(&events),
        "the caller receives the upstream's events byte for byte"
    );
}

#[tokio::test]
async fn a_server_check_under_an_off_key_is_observed_stripped() {
    let events = tool_call_stream(DELTA_WITHOUT_MEMBER);
    let r = observe(
        key_off(),
        sse(events.clone()),
        &server_check_head(),
        &server_check_body(),
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::Stripped));
    let received = String::from_utf8_lossy(&r.sent[0].body).into_owned();
    assert!(
        !received.contains("safeguards"),
        "the member the row calls dropped did not reach the upstream"
    );
    assert_eq!(
        r.answer,
        wire(&events),
        "the off setting edits the request alone: the caller receives the upstream's events byte for byte"
    );
}

#[tokio::test]
async fn a_body_a_strict_parser_refuses_is_observed_stripped_under_an_off_key() {
    // A lone surrogate escape, which a client emits for a string cut mid-pair: the cut walks such a body, so
    // the row must say the member was dropped rather than that the request asked for nothing.
    let sent_body = String::from_utf8(server_check_body())
        .unwrap()
        .replacen(r#""text":""#, r#""text":"\ud800"#, 1)
        .into_bytes();
    assert!(serde_json::from_slice::<Value>(&sent_body).is_err());
    let r = observe(
        key_off(),
        sse(tool_call_stream(DELTA_WITHOUT_MEMBER)),
        &server_check_head(),
        &sent_body,
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::Stripped));
    let received = String::from_utf8_lossy(&r.sent[0].body).into_owned();
    assert!(
        !received.contains("safeguards"),
        "the member the row calls dropped did not reach the upstream"
    );
}
