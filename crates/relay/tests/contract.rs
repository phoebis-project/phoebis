//! What the relay reads off a Claude Code request's headers at admission: the client's own auto-mode classifier
//! call, marked by the class header `auxiliary` with the classifier beta and nothing of the body.
mod common;

use axum::body::Body;
use axum::http::{HeaderValue, Request};
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Classifier, Delivery, Event, RequestObserved, Subscriber};
use phoebis_profile::fixtures::{inference_head, FixtureKind};
use phoebis_relay::CLASSIFIER_BETA;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";

/// The classifier's own shape: the unstamped line, the monitor's prompt, the two user turns, the stop sequence and
/// the short answer. The marker is header-only, so the body is here for the shape alone.
fn classifier_body() -> Vec<u8> {
    br#"{"model":"claude-sonnet-5","max_tokens":64,"stop_sequences":["</block>"],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cc_turn_origin=human;"},{"type":"text","text":"You are a security monitor for autonomous AI coding agents. Decide whether the command below may run."}],"messages":[{"role":"user","content":"The command: ls -la"},{"role":"user","content":"The context: a listing of the working directory"}],"metadata":{"user_id":"{\"account_uuid\":\"\",\"device_id\":\"d-1\",\"session_id\":\"s-1\"}"}}"#.to_vec()
}

/// A request of `body` under `head`, the class header set when `class` names one and the beta appended on a line
/// of its own when `beta` is set, so the fixture's own beta line stays as sent.
fn marked(head: FixtureKind, class: Option<&str>, beta: bool) -> Request<Body> {
    let mut req = request_from_fixture(inference_head(head), &classifier_body(), KEY);
    if let Some(class) = class {
        set_header(&mut req, "x-claude-code-request-class", class);
    }
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
    /// The request's events in publish order, the response chunks left out.
    events: Vec<Event>,
}

impl Answer {
    async fn of(resp: axum::http::Response<Body>, sub: &mut Subscriber) -> Answer {
        let status = resp.status().as_u16();
        resp.into_body().collect().await.unwrap();
        let events = drain_until_complete(sub)
            .await
            .into_iter()
            .filter(|event| !matches!(event, Event::ResponseChunk(_)))
            .collect();
        Answer { status, events }
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
}

async fn send(h: &Harness, sub: &mut Subscriber, req: Request<Body>) -> Answer {
    Answer::of(h.app.clone().oneshot(req).await.unwrap(), sub).await
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
    assert_eq!(
        (answer.status, answer.observed().client.classifier),
        (200, None)
    );
    up.shutdown().await;
}
