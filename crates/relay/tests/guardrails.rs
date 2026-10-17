//! The guardrails a key's pool and the key attach, evaluated at admission: what a block answers and publishes, and
//! what the evaluation leaves as it was.
mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Request};
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{
    Bus, Delivery, Event, GuardrailMatch, Kind, Outcome, RequestObserved, Subscriber,
};
use phoebis_guardrails::{Condition, GuardrailAction, GuardrailMode, Operator, Selector, Subject};
use phoebis_profile::fixtures::{inference_head, FixtureKind};
use phoebis_store::GuardrailRow;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const FORK: &[u8] = include_bytes!("../../guardrails/tests/fixtures/subagent_summary_fork.json");
const CLASSIFIER: &[u8] =
    include_bytes!("../../guardrails/tests/fixtures/auto_mode_classifier.json");
const SUMMARY: &str = "Subagent status summary";

fn messages_of(fixture: &[u8]) -> Vec<serde_json::Value> {
    let value: serde_json::Value =
        serde_json::from_slice(fixture).expect("the fixture is valid JSON");
    value["messages"]
        .as_array()
        .expect("the fixture carries messages")
        .clone()
}

/// A conversation under the class the client sends its forks and its classifier calls with.
fn auxiliary(messages: &[serde_json::Value]) -> Request<Body> {
    let mut req = conversation_request(KEY, "claude-sonnet-4-5", "s1", messages);
    set_header(&mut req, "x-claude-code-request-class", "auxiliary");
    req
}

fn fork() -> Request<Body> {
    auxiliary(&messages_of(FORK))
}

/// Auxiliary with tools like the fork, but its last user message is no status question.
fn classifier() -> Request<Body> {
    auxiliary(&messages_of(CLASSIFIER))
}

/// The request with its body read out, so a case can compare what was sent with what was published.
async fn with_bytes(req: Request<Body>) -> (Request<Body>, bytes::Bytes) {
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (Request::from_parts(parts, Body::from(bytes.clone())), bytes)
}

fn condition(subject: Subject, operator: Operator, value: &str) -> Condition {
    Condition {
        subject,
        operator,
        value: Some(value.into()),
        name: None,
        path: None,
        selector: None,
    }
}

/// The preset's three conditions written out, so a case asserts its own guardrail rather than the shipped table.
fn fork_conditions() -> Vec<Condition> {
    vec![
        condition(Subject::RequestClass, Operator::Equals, "auxiliary"),
        Condition {
            subject: Subject::Body,
            operator: Operator::Present,
            value: None,
            name: None,
            path: Some("tools".into()),
            selector: None,
        },
        Condition {
            selector: Some(Selector::LastUser),
            ..condition(
                Subject::Message,
                Operator::Matches,
                "^Describe your most recent action in 3-5 words",
            )
        },
    ]
}

fn guardrail(
    id: &str,
    name: &str,
    mode: GuardrailMode,
    conditions: Vec<Condition>,
) -> GuardrailRow {
    GuardrailRow {
        id: id.into(),
        name: name.into(),
        description: String::new(),
        enabled: true,
        mode,
        action: GuardrailAction::Block,
        conditions,
        preset_id: None,
        preset_version: None,
        incarnation: 1,
        created_at: NOW,
        updated_at: NOW,
    }
}

/// The fork's guardrail under the test's own id and name.
fn summary(mode: GuardrailMode) -> GuardrailRow {
    guardrail("summary", SUMMARY, mode, fork_conditions())
}

/// Writes the guardrail with its attachments and reloads, so the next request's snapshot holds it.
async fn guarded(h: &Harness, row: &GuardrailRow, pools: &[&str], keys: &[&str]) {
    let owned = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    h.store
        .insert_guardrail(row, &owned(pools), &owned(keys), "test")
        .unwrap();
    h.router.reload().await.unwrap();
}

async fn upstream() -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await
}

async fn harness(up: &FakeUpstream, pool: PoolSpec) -> (Harness, Subscriber) {
    let bus = Bus::new();
    let sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(up, &[oauth("a", A, 0)], pool, bus).await;
    (h, sub)
}

struct Answer {
    status: u16,
    headers: HeaderMap,
    /// `error.type` and `error.message` of a local refusal, empty for an answer that carries none.
    error_type: String,
    message: String,
    /// The request's events in publish order, the response chunks left out.
    events: Vec<Event>,
}

impl Answer {
    async fn of(resp: axum::http::Response<Body>, sub: &mut Subscriber) -> Answer {
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let error = serde_json::from_slice::<serde_json::Value>(&body)
            .map(|v| v["error"].clone())
            .unwrap_or_default();
        let text = |member: &str| error[member].as_str().unwrap_or_default().to_string();
        let (error_type, message) = (text("type"), text("message"));
        let events = drain_until_complete(sub)
            .await
            .into_iter()
            .filter(|event| !matches!(event, Event::ResponseChunk(_)))
            .collect();
        Answer {
            status,
            headers,
            error_type,
            message,
            events,
        }
    }

    fn observed(&self) -> &RequestObserved {
        self.events
            .iter()
            .find_map(|event| match event {
                Event::RequestObserved(observed) => Some(observed),
                _ => None,
            })
            .expect("the request was observed")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

async fn send(h: &Harness, sub: &mut Subscriber, req: Request<Body>) -> Answer {
    Answer::of(h.app.clone().oneshot(req).await.unwrap(), sub).await
}

#[tokio::test]
async fn an_enforcing_pool_guardrail_refuses_the_fork_before_the_upstream_sees_it() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(
        (
            answer.status,
            answer.header("x-phoebis-error-code"),
            answer.header("x-should-retry"),
            answer.error_type.as_str(),
            answer.message.as_str()
        ),
        (
            400,
            Some("guardrail_blocked"),
            Some("false"),
            "invalid_request_error",
            r#"blocked by guardrail "Subagent status summary""#
        )
    );
    assert!(
        up.requests().is_empty(),
        "no account was selected, so nothing went upstream"
    );
    let [Event::RequestObserved(observed), Event::RequestComplete(complete)] =
        answer.events.as_slice()
    else {
        panic!("{:?}", answer.events)
    };
    assert_eq!(observed.guardrail_id.as_deref(), Some("summary"));
    assert_eq!(
        observed.guardrails,
        [GuardrailMatch {
            id: "summary".into(),
            position: 0,
            blocked: true
        }]
    );
    assert_eq!(
        (
            complete.outcome,
            complete.status,
            complete.error_code.as_deref(),
            complete.attempt
        ),
        (Outcome::Rejected, 400, Some("guardrail_blocked"), 0)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_observing_guardrail_lets_the_fork_through_and_records_its_match() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Observe);
    guarded(&h, &row, &["main"], &[]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(answer.status, 200);
    assert_eq!(up.requests().len(), 1);
    let observed = answer.observed();
    assert_eq!(observed.guardrail_id, None);
    assert_eq!(
        observed.guardrails,
        [GuardrailMatch {
            id: "summary".into(),
            position: 0,
            blocked: false
        }]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_block_wins_over_the_keys_model_rules() {
    let up = upstream().await;
    let (h, mut sub) = harness(
        &up,
        PoolSpec {
            model_rules: rules(&["claude-opus-5*", "NOT *"]),
            ..PoolSpec::default()
        },
    )
    .await;
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(
        (answer.status, answer.header("x-phoebis-error-code")),
        (400, Some("guardrail_blocked")),
        "the model rules alone answer model_not_allowed for this model"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_disabled_guardrail_records_nothing() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = GuardrailRow {
        enabled: false,
        ..summary(GuardrailMode::Enforce)
    };
    guarded(&h, &row, &["main"], &[]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.observed().guardrail_id, None);
    assert!(answer.observed().guardrails.is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_is_not_evaluated() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    // The engine holds the key's id and replays the request without a credential or a recorded head.
    let mut req = fork();
    req.headers_mut().remove("authorization");
    req.extensions_mut().remove::<phoebis_relay::OriginalHead>();
    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
            },
            req,
        )
        .await;
    let answer = Answer::of(resp, &mut sub).await;
    assert_eq!(answer.status, 200);
    let observed = answer.observed();
    assert_eq!(observed.kind, Kind::Probe);
    assert_eq!(observed.guardrail_id, None);
    assert!(observed.guardrails.is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_count_tokens_request_is_evaluated() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    let (_, body) = with_bytes(fork()).await;
    let mut req = request_from_fixture(
        inference_head(FixtureKind::CountOauthNonFirstParty),
        &body,
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", "s1");
    set_header(&mut req, "x-claude-code-request-class", "auxiliary");
    let answer = send(&h, &mut sub, req).await;
    assert_eq!(
        (answer.status, answer.header("x-phoebis-error-code")),
        (400, Some("guardrail_blocked"))
    );
    assert!(up.requests().is_empty());
    let observed = answer.observed();
    assert_eq!(
        (observed.kind, observed.guardrail_id.as_deref()),
        (Kind::CountTokens, Some("summary"))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_keys_own_guardrail_runs_after_the_pools_and_one_on_both_is_recorded_once_at_the_pools_position(
) {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    // The key's own list reads g2 then g1, so only a list that puts the pool's first answers g1 ahead of g2.
    let g2 = guardrail("g2", "Second", GuardrailMode::Observe, fork_conditions());
    guarded(&h, &g2, &[], &["desktop"]).await;
    let g1 = guardrail("g1", "First", GuardrailMode::Observe, fork_conditions());
    guarded(&h, &g1, &["main"], &["desktop"]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.observed().guardrails,
        [
            GuardrailMatch {
                id: "g1".into(),
                position: 0,
                blocked: false
            },
            GuardrailMatch {
                id: "g2".into(),
                position: 1,
                blocked: false
            },
        ]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_unmatched_guardrail_ahead_of_the_enforcing_one_does_not_move_the_block() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let main_only = guardrail(
        "main-only",
        "Main turns",
        GuardrailMode::Enforce,
        vec![condition(Subject::RequestClass, Operator::Equals, "main")],
    );
    guarded(&h, &main_only, &["main"], &[]).await;
    let summary = summary(GuardrailMode::Enforce);
    guarded(&h, &summary, &["main"], &[]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(
        (
            answer.status,
            answer.header("x-phoebis-error-code"),
            answer.message.as_str()
        ),
        (
            400,
            Some("guardrail_blocked"),
            r#"blocked by guardrail "Subagent status summary""#
        )
    );
    let observed = answer.observed();
    assert_eq!(observed.guardrail_id.as_deref(), Some("summary"));
    assert_eq!(
        observed.guardrails,
        [GuardrailMatch {
            id: "summary".into(),
            position: 1,
            blocked: true
        }]
    );
    up.shutdown().await;
}

/// The relay's harness holds no ledger, so the case stops at the observation the ledger captures the body from.
#[tokio::test]
async fn a_blocked_requests_observed_event_carries_the_body_and_the_keys_capture_hours() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let mut key = h.store.api_key("desktop").unwrap().unwrap();
    key.body_capture_hours = Some(6);
    h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    let (req, sent) = with_bytes(fork()).await;
    let answer = send(&h, &mut sub, req).await;
    assert_eq!(answer.status, 400);
    let observed = answer.observed();
    assert_eq!(observed.guardrail_id.as_deref(), Some("summary"));
    assert_eq!(
        observed.guardrails,
        [GuardrailMatch {
            id: "summary".into(),
            position: 0,
            blocked: true
        }]
    );
    assert_eq!(observed.body, sent, "the body as received");
    assert_eq!(observed.body_capture_hours, 6, "the key's own window");
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_request_on_the_key_is_admitted_after_a_block() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    assert_eq!(send(&h, &mut sub, fork()).await.status, 400);
    assert_eq!(
        h.router.in_flight().key("desktop"),
        0,
        "the block released the key's permit when it answered"
    );
    let answer = send(&h, &mut sub, classifier()).await;
    assert_eq!(
        answer.status, 200,
        "the key admits the next request, which this guardrail does not match"
    );
    assert!(answer.observed().guardrails.is_empty());
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_key_without_guardrails_answers_as_before() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(answer.status, 200);
    assert_eq!(up.requests().len(), 1);
    assert_eq!(answer.observed().guardrail_id, None);
    assert!(answer.observed().guardrails.is_empty());
    up.shutdown().await;
}
