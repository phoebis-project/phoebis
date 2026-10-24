//! A token count travels to the upstream exactly as the client sent it: its own cache markers are never
//! rewritten, and nothing else about its body changes. A count writes no cache, so a pin it lands on keeps its
//! warmth.
mod common;
use common::*;

use std::sync::atomic::Ordering;

use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{CACHE_DIAGNOSIS_BETA, EXTENDED_TTL_BETA};
use phoebis_routing::{warm, RefusalFacts, RouterEvent};
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA0001";
const OA: &str = "sk-ant-oat01-AAAA0001";

const COUNT_OK: &[u8] = br#"{"input_tokens":7}"#;

/// Ahead of any `/v1/messages` rule in a test that serves both, since the count path matches that prefix too.
fn count_rule() -> Rule {
    Rule {
        path_prefix: "/v1/messages/count_tokens".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: COUNT_OK.to_vec(),
        },
    }
}

fn policy(json: &'static str) -> PoolSpec {
    PoolSpec {
        cache_ttl: json,
        ..Default::default()
    }
}

/// Every `cache_control` value's raw text, in body order.
fn markers(body: &[u8]) -> Vec<String> {
    phoebis_cch::cache_control_spans(body)
        .unwrap()
        .into_iter()
        .map(|(start, end)| String::from_utf8(body[start..end].to_vec()).unwrap())
        .collect()
}

/// The recorded count with its one message turned into a marked text block: the shape the client would send if
/// it ever cached a count. The recorded bodies carry no marker at all, so nothing else can prove the rewrite.
fn marked_count(fixture: BodyFixture) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(body(fixture)).unwrap();
    let message = &mut value["messages"][0];
    let text = message["content"]
        .as_str()
        .expect("the recorded count holds one text message")
        .to_string();
    message["content"] = serde_json::json!([
        {"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}
    ]);
    serde_json::to_vec(&value).unwrap()
}

/// The recorded count as the client sent it, under the router's key.
fn count_request(kind: FixtureKind, fixture: BodyFixture) -> axum::http::Request<axum::body::Body> {
    request_from_fixture(inference_head(kind), body(fixture), KEY)
}

fn count_request_with(kind: FixtureKind, body: &[u8]) -> axum::http::Request<axum::body::Body> {
    request_from_fixture(inference_head(kind), body, KEY)
}

#[tokio::test]
async fn under_the_defaults_the_recorded_count_reaches_the_upstream_as_the_client_sent_it() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(&up, &[api_key("acct", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(count_request(
            FixtureKind::CountApiKey,
            BodyFixture::CountApiKey,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        sent.body == body(BodyFixture::CountApiKey),
        "no edit under the defaults"
    );
    assert!(sent.line.starts_with("POST /v1/messages/count_tokens"));
    assert!(
        beta_list(sent)
            .iter()
            .any(|b| b == "token-counting-2024-11-01"),
        "the client's own beta travels"
    );
    assert!(!beta_list(sent).iter().any(|b| b == CACHE_DIAGNOSIS_BETA));
    up.shutdown().await;
}

#[tokio::test]
async fn under_the_policy_the_recorded_count_travels_as_sent() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"metered":{"main":"1h"}}"#),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(count_request(
            FixtureKind::CountApiKey,
            BodyFixture::CountApiKey,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        markers(body(BodyFixture::CountApiKey)).is_empty(),
        "the client sends a count with no marker of its own"
    );
    assert!(
        sent.body == body(BodyFixture::CountApiKey),
        "the count endpoint gets no top-level member the client never sent"
    );
    assert!(
        !beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "no beta for an hour the body does not carry"
    );
    assert!(
        phoebis_cch::object_member_span(&sent.body, &["diagnostics"]).is_none(),
        "nothing to diagnose on a count"
    );
    assert!(
        phoebis_cch::string_member_span(&sent.body, &["metadata", "user_id"]).is_none(),
        "no identity slot on a count"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_marked_count_travels_as_sent_and_no_beta_follows_under_the_policy() {
    let up = FakeUpstream::start(vec![
        count_rule(),
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
    ])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        // An api_key account always pays, so its writes take the metered set's own `main` here.
        policy(r#"{"enabled":true,"metered":{"main":"1h"}}"#),
    )
    .await;
    let marked = marked_count(BodyFixture::CountApiKey);
    let resp = h
        .app
        .clone()
        .oneshot(count_request_with(FixtureKind::CountApiKey, &marked))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        sent.body == marked,
        "a count is not the request the policy prices, so its own marker travels untouched"
    );
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral"}"#.to_string()],
        "the client's own marker keeps the TTL it was sent with"
    );
    assert!(
        !beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "no beta for an edit that never ran"
    );
    assert!(
        phoebis_cch::object_member_span(&sent.body, &["diagnostics"]).is_none(),
        "nothing to diagnose on a count"
    );
    assert!(
        phoebis_cch::string_member_span(&sent.body, &["metadata", "user_id"]).is_none(),
        "no identity slot on a count"
    );

    // The inference request under the same policy still reads the hour, unaffected by the count.
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
    assert_eq!(resp.status(), 200);
    let inference = &up.requests()[1];
    assert!(!markers(&inference.body).is_empty());
    assert!(markers(&inference.body)
        .iter()
        .all(|m| m.contains(r#""ttl":"1h""#)));
    up.shutdown().await;
}

#[tokio::test]
async fn a_marked_count_under_an_oauth_key_travels_as_sent_and_gets_no_restoration() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(
        &up,
        &[oauth("acct", OA, 0)],
        policy(r#"{"enabled":true,"plan":{"main":"1h"}}"#),
    )
    .await;
    let marked: Vec<Vec<u8>> = vec![
        marked_count(BodyFixture::CountOauth),
        marked_count(BodyFixture::CountOauthFirstParty),
    ];
    for (kind, body) in [
        (FixtureKind::CountOauthNonFirstParty, &marked[0]),
        (FixtureKind::CountOauthFirstParty, &marked[1]),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(count_request_with(kind, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    assert_eq!(up.requests().len(), 2);
    for (sent, marked) in up.requests().iter().zip(marked.iter()) {
        assert!(
            sent.body == *marked,
            "an oauth count travels as sent under the policy, as an api-key one does"
        );
        assert!(
            !beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
            "no beta for an edit that never ran"
        );
        assert!(
            phoebis_cch::string_member_span(&sent.body, &["system", "0", "text"]).is_none(),
            "no billing line was inserted on a count"
        );
        assert!(phoebis_cch::object_member_span(&sent.body, &["diagnostics"]).is_none());
        assert!(
            phoebis_cch::object_member_span(&sent.body, &["metadata"]).is_none(),
            "no identity slot on a count"
        );
    }
    up.shutdown().await;
}

/// `value` with every `cache_control` member removed, at any depth.
fn unmarked(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            map.values_mut().for_each(unmarked);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(unmarked),
        _ => {}
    }
}

#[tokio::test]
async fn a_count_on_a_pinned_session_leaves_the_pins_refresh_its_ttl_and_its_warmth_as_they_were() {
    let up = FakeUpstream::start(vec![
        count_rule(),
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
    ])
    .await;
    let h = harness(&up, &[oauth("acct", OA, 0)], PoolSpec::default()).await;
    let turn = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(turn.status(), 200);
    turn.into_body().collect().await.unwrap();
    let pin_of = || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.session_id == "s1")
            .expect("the turn pinned its session")
    };
    let before = pin_of().await;
    assert_eq!(
        before.cache_ttl_ms, 3_600_000,
        "the turn's one-hour markers"
    );

    // The turn's own conversation counted: its messages hold the pin's tail, so it reads as a continuation,
    // and without a marker the TTL a renewal would write is the five-minute default.
    let mut conversation: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).unwrap();
    unmarked(&mut conversation);
    let counted = serde_json::json!({
        "messages": conversation["messages"],
        "model": conversation["model"],
        "system": conversation["system"],
        "tools": conversation["tools"],
    });
    let mut count = count_request_with(
        FixtureKind::CountOauthNonFirstParty,
        &serde_json::to_vec(&counted).unwrap(),
    );
    set_header(&mut count, "x-claude-code-session-id", "s1");
    let counted_at = NOW + 60_000;
    h.clock.store(counted_at, Ordering::SeqCst);
    let resp = h.app.clone().oneshot(count).await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    assert!(up.requests()[1]
        .line
        .starts_with("POST /v1/messages/count_tokens"));

    let after = pin_of().await;
    assert_eq!(
        (after.epoch, after.account_id.as_str()),
        (before.epoch, before.account_id.as_str()),
        "the count served on the pin"
    );
    assert_eq!(
        (after.last_refresh_at, after.cache_ttl_ms),
        (before.last_refresh_at, before.cache_ttl_ms),
        "a count writes no cache, so it neither refreshes the pin nor names its TTL"
    );
    assert!(
        warm(&after, false, counted_at + 600_000),
        "the turn's hour still stands ten minutes after the count"
    );
    up.shutdown().await;
}

/// The contract's answer to a count for `item`: a `400` whose message names `name` with no semicolon, nothing sent
/// upstream, and the router's record of a count refused for the item.
async fn assert_count_refused_for(
    resp: axum::http::Response<axum::body::Body>,
    up: &FakeUpstream,
    decisions: &mut broadcast::Receiver<RouterEvent>,
    item: &str,
    name: &str,
) {
    assert_eq!(resp.status(), 400, "{item}");
    assert_eq!(
        resp.headers()["x-phoebis-error-code"],
        "client_contract",
        "{item}"
    );
    let request_id = outcome(&resp).request_id;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let refusal: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let message = refusal["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(name) && !message.contains(';'),
        "{item}: {message}"
    );
    assert!(
        up.requests().is_empty(),
        "{item}: nothing was sent upstream"
    );
    let events = router_events_until(decisions, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == request_id)
    })
    .await;
    let (summary, record) = decisions_of(&events, &request_id)
        .pop()
        .expect("the refusal's decision");
    assert!(
        record.request.flags.count,
        "{item}: the record names a count"
    );
    assert_eq!(
        summary.refusal,
        Some(RefusalFacts {
            kind: "client",
            code: "contract",
            reason: item.into(),
        }),
        "{item}"
    );
}

/// The client sends its session header on every call, a token count among them, so a count without it is refused
/// for the contract.
#[tokio::test]
async fn a_count_without_the_session_header_is_refused_for_the_contract() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(&up, &[api_key("acct", A, 0)], PoolSpec::default()).await;
    let mut decisions = h.router.subscribe_events();
    let mut req = count_request(FixtureKind::CountApiKey, BodyFixture::CountApiKey);
    remove_header(&mut req, "x-claude-code-session-id");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_count_refused_for(
        resp,
        &up,
        &mut decisions,
        "session_header",
        "x-claude-code-session-id",
    )
    .await;
    up.shutdown().await;
}

/// The version is judged on a count as on any request, so a `claude-cli/` version outside the client's spelling is
/// refused for the contract on a count too.
#[tokio::test]
async fn a_count_with_a_version_outside_the_clients_spelling_is_refused_for_the_contract() {
    let up = FakeUpstream::start(vec![count_rule()]).await;
    let h = harness(&up, &[api_key("acct", A, 0)], PoolSpec::default()).await;
    let mut decisions = h.router.subscribe_events();
    let mut req = count_request(FixtureKind::CountApiKey, BodyFixture::CountApiKey);
    set_header(
        &mut req,
        "user-agent",
        "claude-cli/2.1.278;x=y (external, cli)",
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_count_refused_for(resp, &up, &mut decisions, "client_version", "User-Agent").await;
    up.shutdown().await;
}
