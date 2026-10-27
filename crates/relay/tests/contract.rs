//! What the relay reads off a Claude Code request's headers at admission: the client's own auto-mode classifier
//! call, marked by the class header `auxiliary` with the classifier beta and nothing of the body, and the contract,
//! the items a Claude Code request carries, two of them on a token count and a probe, or is refused for before any
//! account is chosen.
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request};
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{
    Bus, Classifier, Delivery, Event, Outcome, RequestComplete, RequestObserved, Subscriber,
};
use phoebis_profile::fixtures::{inference_head, FixtureKind};
use phoebis_relay::{Probe, CLASSIFIER_BETA};
use phoebis_routing::{RefusalFacts, RouterEvent};
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const K: &str = "sk-ant-api03-KKKK";

/// The classifier's own shape: the unstamped line, the monitor's prompt, the two user turns, the stop sequence and
/// the short answer. The marker is header-only, so the body is here for the shape alone.
fn classifier_body() -> Vec<u8> {
    br#"{"model":"claude-sonnet-5","max_tokens":64,"stop_sequences":["</block>"],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cc_turn_origin=human;"},{"type":"text","text":"You are a security monitor for autonomous AI coding agents. Decide whether the command below may run."}],"messages":[{"role":"user","content":"The command: ls -la"},{"role":"user","content":"The context: a listing of the working directory"}],"metadata":{"user_id":"{\"account_uuid\":\"\",\"device_id\":\"d-1\",\"session_id\":\"s-1\"}"}}"#.to_vec()
}

/// The classifier body with `edit` applied to its parse.
fn classifier_body_with(edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(&classifier_body()).unwrap();
    edit(&mut value);
    serde_json::to_vec(&value).unwrap()
}

/// `body` under `head`, the class header set when `class` names one and removed when it does not (the fixture
/// harness sets `main` where the head carries none).
fn shaped(head: FixtureKind, body: &[u8], class: Option<&str>) -> Request<Body> {
    let mut req = request_from_fixture(inference_head(head), body, KEY);
    match class {
        Some(class) => set_header(&mut req, "x-claude-code-request-class", class),
        None => remove_header(&mut req, "x-claude-code-request-class"),
    }
    req
}

/// A request of the classifier body under `head`, the class header set when `class` names one and removed when it
/// does not (the fixture harness sets `main` where the head carries none), and the beta appended on a line of its
/// own when `beta` is set, so the fixture's own beta line stays as sent.
fn marked(head: FixtureKind, class: Option<&str>, beta: bool) -> Request<Body> {
    let mut req = shaped(head, &classifier_body(), class);
    if beta {
        req.headers_mut()
            .append("anthropic-beta", HeaderValue::from_static(CLASSIFIER_BETA));
    }
    req
}

async fn upstream() -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await
}

async fn observing(up: &FakeUpstream, accounts: &[TestAccount]) -> (Harness, Subscriber) {
    let bus = Bus::new();
    let sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(up, accounts, PoolSpec::default(), bus).await;
    (h, sub)
}

struct Answer {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
    /// The request's events in publish order, the response chunks left out.
    events: Vec<Event>,
}

impl Answer {
    async fn of(resp: axum::http::Response<Body>, sub: &mut Subscriber) -> Answer {
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        let events = drain_until_complete(sub)
            .await
            .into_iter()
            .filter(|event| !matches!(event, Event::ResponseChunk(_)))
            .collect();
        Answer {
            status,
            headers,
            body,
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

    fn complete(&self) -> &RequestComplete {
        self.events
            .iter()
            .find_map(|event| match event {
                Event::RequestComplete(complete) => Some(complete),
                _ => None,
            })
            .expect("the request completed")
    }

    fn message(&self) -> String {
        let body: serde_json::Value = serde_json::from_slice(&self.body).unwrap();
        body["error"]["message"].as_str().unwrap().to_string()
    }
}

async fn send(h: &Harness, sub: &mut Subscriber, req: Request<Body>) -> Answer {
    Answer::of(h.app.clone().oneshot(req).await.unwrap(), sub).await
}

fn probe_of(h: &Harness) -> Probe {
    Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
    }
}

/// The contract's answer for `item`: a `403` the client is told not to retry, its message naming `name` with no
/// semicolon, the item's word on the completion, and nothing sent upstream.
fn assert_refused_for(answer: &Answer, up: &FakeUpstream, item: &str, name: &str) {
    assert_eq!(answer.status, 403, "{item}");
    assert_eq!(
        answer.headers["x-phoebis-error-code"], "client_contract",
        "{item}"
    );
    assert_eq!(answer.headers["x-should-retry"], "false", "{item}");
    assert!(answer.headers.get("retry-after").is_none(), "{item}");
    let body: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(body["error"]["type"], "permission_error", "{item}");
    let message = answer.message();
    assert!(
        message.contains(name) && !message.contains(';'),
        "{item}: {message}"
    );
    assert_eq!(
        answer.complete().client_contract.as_deref(),
        Some(item),
        "{item}"
    );
    assert!(
        up.requests().is_empty(),
        "{item}: nothing was sent upstream"
    );
}

/// The request class the router recorded with the request's decision, under a five-second bound.
async fn recorded_request_class(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> Option<String> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the request's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision { record, .. } = event {
            if record.request.request_id == request_id {
                return record.request.request_class.clone();
            }
        }
    }
}

#[tokio::test]
async fn a_claude_code_request_with_the_auxiliary_class_and_the_beta_is_observed_as_the_clients_classifier_call(
) {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, Some("auxiliary"), true),
    )
    .await;
    assert_eq!(answer.status, 200);
    assert_eq!(credentials_seen(&up).len(), 1, "the upstream saw the call");
    assert_eq!(
        answer.observed().client.classifier,
        Some(Classifier::Client)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_class_without_the_beta_and_the_beta_without_the_auxiliary_class_mark_nothing() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    for (class, beta) in [("auxiliary", false), ("main", true), ("side_reply", true)] {
        let answer = send(
            &h,
            &mut sub,
            marked(FixtureKind::OauthFirstParty, Some(class), beta),
        )
        .await;
        let client = &answer.observed().client;
        assert_eq!(
            (client.request_class.as_deref(), client.classifier),
            (Some(class), None),
            "the class {class} with the beta {beta}"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn the_pair_without_a_class_header_marks_nothing() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, None, true),
    )
    .await;
    let client = &answer.observed().client;
    assert_eq!(
        (
            answer.status,
            client.request_class.as_deref(),
            client.classifier
        ),
        (403, None, None)
    );
    assert_refused_for(&answer, &up, "class_header", "x-claude-code-request-class");
    up.shutdown().await;
}

#[tokio::test]
async fn a_claude_code_request_with_every_item_is_served_and_the_upstream_sees_one_request() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, Some("main"), false),
    )
    .await;
    assert_eq!(answer.status, 200);
    assert_eq!(up.requests().len(), 1);
    assert_eq!(answer.complete().client_contract, None);
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_without_the_class_header_is_refused_for_the_contract_before_any_account_is_chosen(
) {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let mut decisions = h.router.subscribe_events();
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, None, false),
    )
    .await;
    assert_refused_for(&answer, &up, "class_header", "x-claude-code-request-class");
    let complete = answer.complete();
    assert_eq!(
        (
            complete.error_code.as_deref(),
            complete.outcome,
            complete.client_drift.as_deref(),
            complete.account_id.as_deref()
        ),
        (Some("client_contract"), Outcome::Rejected, None, None)
    );
    let client = &answer.observed().client;
    assert_eq!(
        (client.class.as_str(), client.request_class.as_deref()),
        ("claude-code/sdk-cli", None),
        "the row carries the request's class facts as a drift refusal's does"
    );
    assert_eq!(
        recorded_refusal(&mut decisions, &complete.request_id).await,
        Some(RefusalFacts {
            kind: "client",
            code: "contract",
            reason: "class_header".into(),
        })
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_without_the_session_header_is_refused_for_the_contract() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let mut req = marked(FixtureKind::OauthFirstParty, Some("main"), false);
    remove_header(&mut req, "x-claude-code-session-id");
    let answer = send(&h, &mut sub, req).await;
    assert_refused_for(&answer, &up, "session_header", "x-claude-code-session-id");
    up.shutdown().await;
}

/// A keepalive probe is judged on the session header as a request is, and its refusal's record carries the probe's
/// flag and not the count's.
#[tokio::test]
async fn a_probe_without_the_session_header_is_refused_for_the_contract_and_recorded_as_a_probe() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let mut decisions = h.router.subscribe_events();
    let mut req = marked(FixtureKind::OauthFirstParty, Some("main"), false);
    remove_header(&mut req, "x-claude-code-session-id");
    let answer = Answer::of(h.relay.probe(probe_of(&h), req).await, &mut sub).await;
    assert_refused_for(&answer, &up, "session_header", "x-claude-code-session-id");
    let request_id = answer.complete().request_id.clone();
    let events = router_events_until(&mut decisions, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == request_id)
    })
    .await;
    let (_, record) = decisions_of(&events, &request_id)
        .pop()
        .expect("the refusal's decision");
    assert_eq!(
        (record.request.flags.probe, record.request.flags.count),
        (true, false),
        "the record names a probe"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_without_metadata_user_id_is_refused_for_the_contract() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let body = classifier_body_with(|value| {
        value.as_object_mut().unwrap().remove("metadata");
    });
    let answer = send(
        &h,
        &mut sub,
        shaped(FixtureKind::OauthFirstParty, &body, Some("main")),
    )
    .await;
    assert_refused_for(&answer, &up, "metadata_user_id", "metadata.user_id");
    up.shutdown().await;
}

#[tokio::test]
async fn a_subagent_request_without_the_agent_id_header_is_refused_for_the_contract() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let mut decisions = h.router.subscribe_events();
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, Some("subagent"), false),
    )
    .await;
    assert_refused_for(&answer, &up, "agent_id_header", "x-claude-code-agent-id");
    // The refusal's record keeps the class the request named, where a drift's records none.
    assert_eq!(
        recorded_request_class(&mut decisions, &answer.complete().request_id)
            .await
            .as_deref(),
        Some("subagent")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_compaction_request_without_the_compaction_header_is_refused_for_the_contract() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, Some("compaction"), false),
    )
    .await;
    assert_refused_for(
        &answer,
        &up,
        "compaction_header",
        "x-claude-code-compaction",
    );
    up.shutdown().await;
}

/// The splice binds the account into the `account_uuid` member of `metadata.user_id`, so a string without that
/// member is refused for the contract before any account is chosen, on an OAuth leg and an API-key leg alike.
#[tokio::test]
async fn a_request_whose_user_id_lacks_the_account_uuid_member_is_refused_for_the_contract() {
    let body = classifier_body_with(|value| {
        value["metadata"]["user_id"] = serde_json::json!("{\"session_id\":\"s-1\"}");
    });
    for account in [api_key("k", K, 0), oauth("a", A, 0)] {
        let up = upstream().await;
        let kind = account.kind;
        let (h, mut sub) = observing(&up, &[account]).await;
        let answer = send(
            &h,
            &mut sub,
            shaped(FixtureKind::OauthFirstParty, &body, Some("main")),
        )
        .await;
        assert_eq!(answer.status, 403, "{kind:?}");
        assert_refused_for(&answer, &up, "metadata_user_id", "account_uuid");
        up.shutdown().await;
    }
}

/// The client writes an empty `account_uuid` when it holds no OAuth account, as it does behind the router, so the
/// member with an empty value satisfies the item.
#[tokio::test]
async fn an_empty_account_uuid_in_metadata_user_id_is_admitted() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let answer = send(
        &h,
        &mut sub,
        shaped(
            FixtureKind::OauthFirstParty,
            &classifier_body(),
            Some("main"),
        ),
    )
    .await;
    assert_eq!(answer.status, 200);
    assert_eq!(up.requests().len(), 1);
    assert_eq!(answer.complete().client_contract, None);
    up.shutdown().await;
}

/// The client's attribution opt-out drops the line and leaves its own prompt at `system[0]`.
#[tokio::test]
async fn the_attribution_opt_out_shape_is_refused_for_the_line() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let body = classifier_body_with(|value| {
        value["system"].as_array_mut().unwrap().remove(0);
    });
    let answer = send(
        &h,
        &mut sub,
        shaped(FixtureKind::OauthFirstParty, &body, Some("main")),
    )
    .await;
    assert_refused_for(&answer, &up, "attribution_line", "system[0]");
    up.shutdown().await;
}

/// The public `count_tokens` schema has no slot for the line or `metadata`, so a token count is judged on the
/// version and the session header alone, and this count sends both.
#[tokio::test]
async fn a_token_count_without_a_line_and_without_metadata_is_admitted_as_today() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let body = br#"{"model":"claude-sonnet-5","system":[{"type":"text","text":"You are Claude Code."}],"messages":[{"role":"user","content":"hi"}]}"#;
    let req = request_from_fixture(inference_head(FixtureKind::CountOauthFirstParty), body, KEY);
    let answer = send(&h, &mut sub, req).await;
    assert_eq!(answer.status, 200);
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

/// The client's quota probe sends no `system`, so it carries no slot for the line.
#[tokio::test]
async fn the_quota_probes_shape_without_a_system_member_is_admitted() {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let body = classifier_body_with(|value| {
        value.as_object_mut().unwrap().remove("system");
    });
    let answer = send(
        &h,
        &mut sub,
        shaped(FixtureKind::OauthFirstParty, &body, Some("auxiliary")),
    )
    .await;
    assert_eq!(answer.status, 200);
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

/// A key's model policy is judged before the request's shape, so a refused model answers before the contract.
#[tokio::test]
async fn a_request_without_the_class_header_under_a_key_whose_model_policy_refuses_the_model_answers_model_not_allowed(
) {
    let up = upstream().await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["claude-opus-5", "NOT *"]),
            ..PoolSpec::default()
        },
        bus,
    )
    .await;
    let answer = send(
        &h,
        &mut sub,
        marked(FixtureKind::OauthFirstParty, None, false),
    )
    .await;
    assert_eq!(
        (
            answer.status,
            answer.headers["x-phoebis-error-code"].to_str().unwrap()
        ),
        (404, "model_not_allowed")
    );
    assert_eq!(answer.complete().client_contract, None);
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

/// The drift checks read the line the contract requires, so a request that fails both is refused for the contract.
#[tokio::test]
async fn a_drifted_stamp_without_the_class_header_is_refused_for_the_contract_and_never_for_the_drift(
) {
    let up = upstream().await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let body = classifier_body_with(|value| {
        value["system"][0]["text"] = serde_json::json!(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cch=12345;"
        );
    });
    let answer = send(
        &h,
        &mut sub,
        shaped(FixtureKind::OauthFirstParty, &body, None),
    )
    .await;
    assert_refused_for(&answer, &up, "class_header", "x-claude-code-request-class");
    assert_eq!(answer.complete().client_drift, None);
    up.shutdown().await;
}
