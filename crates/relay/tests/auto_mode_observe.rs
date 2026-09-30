//! What the bus learns about auto mode: admission names a request that asked for the server-side check, one the
//! key's setting stripped it from, and the client's own classifier requests at either stage; the response side
//! reads the server's `safeguard_results` and a classifier answer without changing a byte the caller receives.
mod common;
use common::*;

use std::time::Duration;

use phoebis_bus::{AutoMode, Bus, Event, RequestComplete, RequestObserved};
use phoebis_profile::fixtures::{
    body, inference_head, with_dangerous_tool_use_beta, with_safeguards, BodyFixture, FixtureKind,
};
use fake_upstream::{FakeUpstream, RecordedRequest, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const OA: &str = "sk-ant-oat01-AAAA0001";

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1}}}"#;
const TOOL_USE_START: &str = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"Bash","input":{}}}"#;
const DELTA_NOT_FLAGGED: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{"toolu_01":{"type":"evaluated","outcome":"not_flagged"}}}}]},"usage":{"output_tokens":52}}"#;
const DELTA_UNJUDGED: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{}}}]},"usage":{"output_tokens":52}}"#;
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

fn json_answer(answer: Value) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: serde_json::to_vec(&answer).unwrap(),
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

/// The client's own classifier request, built from the recorded session body's billing line and metadata: the
/// monitor instruction after the line, the session's context, one user turn, no `stream` and no `tools`.
fn classifier_body(max_tokens: u64, stops: Option<&[&str]>) -> Vec<u8> {
    let session: Value = serde_json::from_slice(body(BodyFixture::Oauth)).unwrap();
    let mut classifier = json!({
        "model": session["model"],
        "max_tokens": max_tokens,
        "system": [
            session["system"][0],
            {
                "type": "text",
                "text": "You are a security monitor for autonomous AI coding agents.\n\n## Context\n\n<instructions>",
                "cache_control": { "type": "ephemeral", "ttl": "1h" },
            },
            { "type": "text", "text": "\n\n## Session Context\n\n<context>" },
        ],
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "<transcript>" }] }],
        "metadata": session["metadata"],
    });
    if let Some(stops) = stops {
        classifier["stop_sequences"] = json!(stops);
    }
    serde_json::to_vec(&classifier).unwrap()
}

struct Observed {
    answer: Vec<u8>,
    observed: RequestObserved,
    complete: RequestComplete,
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
    let complete = events
        .iter()
        .find_map(|e| match e {
            Event::RequestComplete(c) => Some(c.clone()),
            _ => None,
        })
        .expect("the request completed");
    let sent = up.requests();
    up.shutdown().await;
    Observed {
        answer,
        observed,
        complete,
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
async fn a_server_check_is_observed_with_its_verdict_and_the_stream_passes_through_unchanged() {
    let events = tool_call_stream(DELTA_NOT_FLAGGED);
    let r = observe(
        PoolSpec::default(),
        sse(events.clone()),
        &server_check_head(),
        &server_check_body(),
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::Server));
    let checked = r
        .complete
        .response
        .safeguards
        .expect("the delta's safeguard_results is read");
    assert_eq!(checked.verdict, "not_flagged");
    assert_eq!(
        checked.tool_uses.keys().collect::<Vec<_>>(),
        vec!["toolu_01"]
    );
    assert_eq!(checked.tool_uses["toolu_01"].outcome, "not_flagged");
    assert_eq!(
        r.answer,
        wire(&events),
        "the caller receives the upstream's events byte for byte"
    );
}

#[tokio::test]
async fn a_streamed_tool_call_with_no_arguments_the_check_did_not_judge_reads_none() {
    let r = observe(
        PoolSpec::default(),
        sse(tool_call_stream(DELTA_UNJUDGED)),
        &server_check_head(),
        &server_check_body(),
    )
    .await;
    let checked = r.complete.response.safeguards.unwrap();
    assert_eq!(
        (checked.verdict.as_str(), checked.tool_uses.len()),
        ("none", 0),
        "the tool_use block's start alone names the call"
    );
}

#[tokio::test]
async fn a_server_check_under_an_off_key_is_observed_stripped_and_answers_no_verdict() {
    let r = observe(
        key_off(),
        sse(tool_call_stream(DELTA_WITHOUT_MEMBER)),
        &server_check_head(),
        &server_check_body(),
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::Stripped));
    assert_eq!(
        r.complete.response.safeguards, None,
        "the upstream was asked for no check"
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

#[tokio::test]
async fn a_stage_one_classifier_request_is_observed_with_its_block_answer() {
    let answer = json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5-20250929",
        "content": [{ "type": "text", "text": "<block>yes</block>" }],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": { "input_tokens": 900, "output_tokens": 6 },
    });
    let r = observe(
        PoolSpec::default(),
        json_answer(answer.clone()),
        inference_head(FixtureKind::OauthNonFirstParty),
        &classifier_body(2112, Some(&["</block>"])),
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::ClientStage1));
    let classifier = r
        .complete
        .response
        .classifier
        .expect("a whole JSON answer is read");
    assert_eq!(classifier.verdict, "flagged");
    assert_eq!(r.answer, serde_json::to_vec(&answer).unwrap());
}

#[tokio::test]
async fn a_refused_stage_two_classifier_request_is_observed_with_its_refusal() {
    let answer = json!({
        "id": "msg_02", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5-20250929",
        "content": [], "stop_reason": "refusal", "stop_sequence": null,
        "stop_details": { "category": "reasoning_extraction" },
        "usage": { "input_tokens": 900, "output_tokens": 0 },
    });
    let r = observe(
        PoolSpec::default(),
        json_answer(answer),
        inference_head(FixtureKind::OauthNonFirstParty),
        &classifier_body(10240, None),
    )
    .await;
    assert_eq!(r.observed.auto_mode, Some(AutoMode::ClientStage2));
    let response = &r.complete.response;
    assert_eq!(
        (
            response.stop_reason.as_deref(),
            response.refusal_category.as_deref()
        ),
        (Some("refusal"), Some("reasoning_extraction"))
    );
    assert_eq!(
        response.classifier.as_ref().map(|c| c.verdict.as_str()),
        Some("unreadable")
    );
}
