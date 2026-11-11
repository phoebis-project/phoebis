//! What the relay reads off a Claude Code request's headers at admission: the client's own auto-mode classifier
//! call, marked by the class header `auxiliary` with the classifier beta and nothing of the body, and the contract,
//! the items a Claude Code request carries, two of them on a token count and a probe, or is refused for before any
//! account is chosen.
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request};
use common::*;
use fake_upstream::{wait_for_async, FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{
    Bus, Classifier, Delivery, Event, Outcome, RequestComplete, RequestObserved, Subscriber,
};
use phoebis_profile::fixtures::{inference_head, FixtureKind};
use phoebis_relay::{Probe, CLASSIFIER_BETA};
use phoebis_routing::{
    RefusalFacts, Report, RouterConfig, RouterEvent, SharePricer, UtilizationSample, WindowReading,
    WindowSource, FIVE_HOUR,
};
use phoebis_store::SubscriptionRow;
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
/// semicolon, the item's word and no spent attempt on the completion, and nothing sent upstream.
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
    assert_eq!(answer.complete().attempt, 0, "{item}: no attempt was spent");
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

/// The classifier body of `session` on the first-party head under the class header `class`, the beta appended when
/// `beta` is set, the session named alike in its header and its metadata.
fn in_session(session: &str, class: &str, beta: bool) -> Request<Body> {
    let body = classifier_body_with(|v| v["metadata"]["user_id"] = user_id_for(session).into());
    let mut req = shaped(FixtureKind::OauthFirstParty, &body, Some(class));
    set_header(&mut req, "x-claude-code-session-id", session);
    if beta {
        req.headers_mut()
            .append("anthropic-beta", HeaderValue::from_static(CLASSIFIER_BETA));
    }
    req
}

/// The tier the charge case names on its account, the one tier its pricer prices.
const CHARGED_TIER: &str = "default_claude_max_20x";

#[tokio::test]
async fn a_classifier_call_off_its_judged_turns_account_is_reported_as_any_call_and_completes_client_off(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_1"),
    }])
    .await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    // s-1's turn ends on a, so its classifier call runs on the judged turn's account. s-2 holds no turn to judge.
    send(&h, &mut sub, in_session("s-1", "main", false)).await;
    let on = send(&h, &mut sub, in_session("s-1", "auxiliary", true)).await;
    let off = send(&h, &mut sub, in_session("s-2", "auxiliary", true)).await;
    assert_eq!(
        (on.complete().classifier, off.complete().classifier),
        (None, Some(Classifier::ClientOff)),
        "the call off the judged turn completes with the second word, the one on it with the observation's"
    );
    // The relay hands the judged pin back on the free call's usage report, so the call's tokens enter the classifier
    // copy of s-1's pin at the call's model.
    let pinned: Vec<(String, Option<String>)> = h
        .router
        .pins()
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.session_id, p.burn_unmetered_model))
        .collect();
    assert_eq!(
        pinned,
        [("s-1".to_string(), Some("claude-sonnet-5".to_string()))],
        "neither call holds a pin, so the conversation's turn holds the one pin, its copy folded by the free call"
    );
    up.shutdown().await;

    // Every token a thousandth of the 5-hour window on the tier, so the usage report of a call charged as any call
    // moves the account's prediction off its reading of a half, taken a second before the call.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_1"),
    }])
    .await;
    let charging = SharePricer::new(|window, _, tier, _, usage, _| {
        (window == FIVE_HOUR && tier == CHARGED_TIER)
            .then(|| (usage.input + usage.output) as f64 / 1_000.0)
    });
    let h = harness_with_router_config(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        RouterConfig {
            share: charging,
            ..RouterConfig::default()
        },
    )
    .await;
    h.store
        .set_profile(
            "a",
            "{}",
            &SubscriptionRow {
                tier: Some(CHARGED_TIER.into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    h.router
        .report(Report::Sample {
            account_id: "a".into(),
            sample: UtilizationSample::default().with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.5,
                    reset: Some(NOW + 3_600_000),
                    observed_at: NOW - 1_000,
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    let resp = h
        .app
        .clone()
        .oneshot(in_session("s-2", "auxiliary", true))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await;
    // The answer's two tokens charge 0.002 of the window.
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .samples()
            .await
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == "a")
            .and_then(|(_, sample)| sample.reading(FIVE_HOUR).map(|r| r.utilization))
            .filter(|utilization| (utilization - 0.502).abs() < 1e-9)
    })
    .await;
    up.shutdown().await;
}

/// The completion of the one request `sub` carries once the client's future was aborted.
async fn completion_after_abort(
    call: tokio::task::JoinHandle<Result<axum::http::Response<Body>, std::convert::Infallible>>,
    sub: &mut Subscriber,
) -> RequestComplete {
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    drain_until_complete(sub)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .expect("the guard completed the request")
}

#[tokio::test]
async fn a_classifier_call_the_client_leaves_while_its_leg_waits_for_its_head_completes_with_no_off_word(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Gated(Box::new(ok_with_id("msg_1"))),
    }])
    .await;
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    // s-2 holds no turn to judge, so the router answers its leg off, and the upstream holds that leg's head.
    let app = h.app.clone();
    let call = tokio::spawn(async move { app.oneshot(in_session("s-2", "auxiliary", true)).await });
    wait_for(Duration::from_secs(5), || {
        (credentials_seen(&up).len() == 1).then_some(())
    })
    .await;
    let complete = completion_after_abort(call, &mut sub).await;
    assert_eq!(
        (complete.outcome, complete.classifier),
        (Outcome::Canceled, None),
        "a leg that never answered the client leaves the observation's word"
    );
    up.release();
    up.shutdown().await;
}

fn json_429_retry_after(seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), seconds.into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_vec(),
    }
}

#[tokio::test]
async fn a_classifier_call_the_client_leaves_while_its_leg_waits_to_retry_in_place_completes_with_no_off_word(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_1"),
    }])
    .await;
    // Far past any test's run, so the client leaves inside the wait the first head starts.
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("30"));
    let (h, mut sub) = observing(&up, &[oauth("a", A, 0)]).await;
    let app = h.app.clone();
    let call = tokio::spawn(async move { app.oneshot(in_session("s-2", "auxiliary", true)).await });
    wait_for(Duration::from_secs(5), || {
        std::iter::from_fn(|| sub.events.try_recv().ok())
            .any(|event| matches!(event, Event::ResponseHead(head) if head.status == 429))
            .then_some(())
    })
    .await;
    let complete = completion_after_abort(call, &mut sub).await;
    assert_eq!(
        (complete.outcome, complete.classifier),
        (Outcome::Canceled, None),
        "the throttled head the relay retries never answered the client"
    );
    assert_eq!(credentials_seen(&up).len(), 1, "no retry went out");
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
