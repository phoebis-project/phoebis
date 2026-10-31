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
use phoebis_routing::{RefusalFacts, RouterEvent};
use phoebis_store::GuardrailRow;
use tokio::sync::broadcast;
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

fn header_condition(operator: Operator, name: &str, value: Option<&str>) -> Condition {
    Condition {
        subject: Subject::Header,
        operator,
        value: value.map(Into::into),
        name: Some(name.into()),
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

/// The kinds of the request's decisions up to its placement, in the order the router made them.
async fn decision_kinds_until_placed(
    decisions: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> Vec<&'static str> {
    let events = router_events_until(decisions, |event| {
        matches!(
            event,
            RouterEvent::Decision { summary, record, .. }
                if record.request.request_id == request_id && summary.kind == "placed"
        )
    })
    .await;
    decisions_of(&events, request_id)
        .iter()
        .map(|(summary, _)| summary.kind)
        .collect()
}

#[tokio::test]
async fn an_enforcing_pool_guardrail_refuses_the_fork_before_the_upstream_sees_it() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Enforce);
    guarded(&h, &row, &["main"], &[]).await;
    let mut decisions = h.router.subscribe_events();
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
            403,
            Some("guardrail_blocked"),
            Some("false"),
            "permission_error",
            r#"blocked by guardrail "Subagent status summary""#
        )
    );
    // The answer names the guardrail by its name alone, since the request row records the id.
    for (name, value) in &answer.headers {
        assert!(
            !String::from_utf8_lossy(value.as_bytes()).contains(&row.id),
            "{name} names the guardrail's id"
        );
    }
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
        (Outcome::Rejected, 403, Some("guardrail_blocked"), 0)
    );
    assert_eq!(
        recorded_refusal(&mut decisions, &complete.request_id).await,
        Some(RefusalFacts {
            kind: "guardrail",
            code: "guardrail_blocked",
            reason: SUMMARY.into(),
        })
    );
    up.shutdown().await;
}

/// The block reads the request's class and kind before it answers, so its record carries both, where a drift's
/// carries neither.
#[tokio::test]
async fn a_blocked_count_tokens_requests_record_carries_its_class_and_the_count_flag() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    guarded(&h, &summary(GuardrailMode::Enforce), &["main"], &[]).await;
    let (_, body) = with_bytes(fork()).await;
    let mut req = request_from_fixture(
        inference_head(FixtureKind::CountOauthNonFirstParty),
        &body,
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", "s1");
    set_header(&mut req, "x-claude-code-request-class", "auxiliary");
    let mut decisions = h.router.subscribe_events();
    let answer = send(&h, &mut sub, req).await;
    assert_eq!(answer.status, 403);
    let request_id = answer.observed().request_id.clone();
    let events = router_events_until(&mut decisions, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == request_id)
    })
    .await;
    let recorded = decisions_of(&events, &request_id);
    let [(summary, record)] = recorded.as_slice() else {
        panic!("{recorded:?}")
    };
    assert_eq!(
        (
            summary.kind,
            summary.refusal.as_ref().map(|refusal| refusal.kind),
            record.request.request_class.as_deref(),
            record.request.flags.count,
            record.request.flags.probe
        ),
        ("refused", Some("guardrail"), Some("auxiliary"), true, false)
    );
    up.shutdown().await;
}

/// The routes refuse a name that carries one of the client's phrases, but a row stored before that rule can hold
/// one. "model:" or "OAuth token has been revoked" in a 403's message would start the client's own handling, so the
/// block answers by the id for a stored name that carries any phrase of the list.
async fn answered_by_the_id(id: &str, name: &str) {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    // The store writes a name the routes would refuse, as a row stored before the rule holds one.
    let row = guardrail(id, name, GuardrailMode::Enforce, fork_conditions());
    guarded(&h, &row, &["main"], &[]).await;
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(
        (
            answer.status,
            answer.header("x-phoebis-error-code"),
            answer.error_type.as_str(),
            answer.message.clone()
        ),
        (
            403,
            Some("guardrail_blocked"),
            "permission_error",
            format!("blocked by guardrail \"{id}\"")
        )
    );
    let message = answer.message.to_lowercase();
    for phrase in [
        "model:",
        "oauth token has been revoked",
        "prompt is too long",
    ] {
        assert!(!message.contains(phrase), "{message} carries {phrase}");
    }
    assert!(
        up.requests().is_empty(),
        "the block stays enforced, so nothing went upstream"
    );
    let [Event::RequestObserved(_), Event::RequestComplete(complete)] = answer.events.as_slice()
    else {
        panic!("{:?}", answer.events)
    };
    assert_eq!(
        (
            complete.outcome,
            complete.status,
            complete.error_code.as_deref(),
            complete.attempt
        ),
        (Outcome::Rejected, 403, Some("guardrail_blocked"), 0)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_stored_name_carrying_the_model_access_phrase_is_answered_by_the_guardrails_id() {
    answered_by_the_id("model-phrase", "Model: Opus Only").await;
}

#[tokio::test]
async fn an_observing_guardrail_lets_the_fork_through_and_records_its_match() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let row = summary(GuardrailMode::Observe);
    guarded(&h, &row, &["main"], &[]).await;
    let mut decisions = h.router.subscribe_events();
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
    assert_eq!(
        decision_kinds_until_placed(&mut decisions, &observed.request_id).await,
        ["placed"],
        "the selection records the request, and the match records no refusal"
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
        (403, Some("guardrail_blocked")),
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
    let mut decisions = h.router.subscribe_events();
    let answer = send(&h, &mut sub, fork()).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.observed().guardrail_id, None);
    assert!(answer.observed().guardrails.is_empty());
    assert_eq!(
        decision_kinds_until_placed(&mut decisions, &answer.observed().request_id).await,
        ["placed"],
        "the selection records the request, and the disabled guardrail records no refusal"
    );
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
        (403, Some("guardrail_blocked"))
    );
    assert!(up.requests().is_empty());
    let observed = answer.observed();
    assert_eq!(
        (observed.kind, observed.guardrail_id.as_deref()),
        (Kind::CountTokens, Some("summary"))
    );
    up.shutdown().await;
}

/// The fork and its body with a `deep` member last, `arrays` nested arrays around one number, so every member the
/// relay reads comes before it.
async fn fork_nested(arrays: usize) -> (Request<Body>, String) {
    let (req, body) = with_bytes(fork()).await;
    let body = String::from_utf8(body.to_vec()).expect("the fork's body is text");
    let close = body.rfind('}').expect("the fork's body is an object");
    let nested = format!(
        r#"{},"deep":{}1{}}}"#,
        &body[..close],
        "[".repeat(arrays),
        "]".repeat(arrays)
    );
    let (parts, _) = req.into_parts();
    (
        Request::from_parts(parts, Body::from(nested.clone())),
        nested,
    )
}

/// serde's recursion limit of 128 counts the root, so 127 arrays under it are the shallowest nesting the
/// guardrails' parse refuses, and a `body` condition would read such a body as no body at all. The shallower body
/// meets an enforcing guardrail, whose 403 shows the parse read the tools: `tools present` is false on a body the
/// parse cannot read.
#[tokio::test]
async fn a_body_at_serdes_limit_is_refused_invalid_body_under_a_tools_present_guardrail_and_one_level_shallower_is_evaluated(
) {
    const REFUSED: usize = 127;
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let tools_present = Condition {
        subject: Subject::Body,
        operator: Operator::Present,
        value: None,
        name: None,
        path: Some("tools".into()),
        selector: None,
    };
    let row = guardrail(
        "tools",
        "Tools present",
        GuardrailMode::Enforce,
        vec![tools_present],
    );
    guarded(&h, &row, &["main"], &[]).await;

    let (deep, body) = fork_nested(REFUSED).await;
    assert!(
        serde_json::from_slice::<serde_json::Value>(body.as_bytes()).is_err(),
        "serde refuses {REFUSED} arrays under the root"
    );
    let answer = send(&h, &mut sub, deep).await;
    assert_eq!(
        (
            answer.status,
            answer.header("x-phoebis-error-code"),
            answer.error_type.as_str(),
            answer.message.as_str()
        ),
        (
            400,
            Some("invalid_body"),
            "invalid_request_error",
            "the request body nests deeper than the relay reads"
        )
    );
    assert!(
        up.requests().is_empty(),
        "admission refused the body, so nothing went upstream"
    );

    let (shallow, body) = fork_nested(REFUSED - 1).await;
    assert!(
        serde_json::from_slice::<serde_json::Value>(body.as_bytes()).is_ok(),
        "serde reads one level shallower"
    );
    let answer = send(&h, &mut sub, shallow).await;
    assert_eq!(
        (answer.status, answer.header("x-phoebis-error-code")),
        (403, Some("guardrail_blocked"))
    );
    assert_eq!(answer.observed().guardrail_id.as_deref(), Some("tools"));
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_header_condition_reads_a_sent_headers_value_and_a_header_the_request_does_not_send_as_absent(
) {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let unsent = guardrail(
        "unsent",
        "No marker header",
        GuardrailMode::Observe,
        vec![header_condition(Operator::Absent, "x-marker", None)],
    );
    guarded(&h, &unsent, &["main"], &[]).await;
    let team = guardrail(
        "team",
        "Research team",
        GuardrailMode::Enforce,
        vec![header_condition(
            Operator::Equals,
            "x-team",
            Some("research"),
        )],
    );
    guarded(&h, &team, &["main"], &[]).await;
    let mut req = fork();
    set_header(&mut req, "x-team", "research");
    let answer = send(&h, &mut sub, req).await;
    assert_eq!(
        (
            answer.status,
            answer.header("x-phoebis-error-code"),
            answer.message.as_str()
        ),
        (
            403,
            Some("guardrail_blocked"),
            r#"blocked by guardrail "Research team""#
        )
    );
    assert!(up.requests().is_empty());
    assert_eq!(
        answer.observed().guardrails,
        [
            GuardrailMatch {
                id: "unsent".into(),
                position: 0,
                blocked: false
            },
            GuardrailMatch {
                id: "team".into(),
                position: 1,
                blocked: true
            },
        ]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_user_agent_condition_reads_the_header_cut_to_256_bytes_as_the_row_keeps_it() {
    let up = upstream().await;
    let (h, mut sub) = harness(&up, PoolSpec::default()).await;
    let mut req = fork();
    let agent = req.headers()["user-agent"].to_str().unwrap();
    let sent = format!("{agent} {}", "x".repeat(300));
    set_header(&mut req, "user-agent", &sent);
    let row = guardrail(
        "long-agent",
        "Long user agent",
        GuardrailMode::Enforce,
        vec![condition(
            Subject::UserAgent,
            Operator::Equals,
            &sent[..256],
        )],
    );
    guarded(&h, &row, &["main"], &[]).await;
    let answer = send(&h, &mut sub, req).await;
    assert_eq!(
        (answer.status, answer.header("x-phoebis-error-code")),
        (403, Some("guardrail_blocked")),
        "the condition compares the first 256 bytes of the header"
    );
    assert_eq!(
        answer.observed().guardrail_id.as_deref(),
        Some("long-agent")
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
            403,
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
    assert_eq!(answer.status, 403);
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
    assert_eq!(send(&h, &mut sub, fork()).await.status, 403);
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
