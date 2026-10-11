mod common;
use std::time::Duration;

use common::*;

use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::EXTENDED_TTL_BETA;
use phoebis_routing::{DecisionRecord, DecisionSummary, RouterEvent};
use phoebis_store::{AffinityMode, OveragePolicy};
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA0001";
const B: &str = "sk-ant-api03-BBBB0001";
const OA: &str = "sk-ant-oat01-AAAA0001";
const OB: &str = "sk-ant-oat01-BBBB0001";

const REFUSED_TTL_BETA: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `extended-cache-ttl-2025-04-11` for the `anthropic-beta` header."}}"#;

fn policy(json: &'static str) -> PoolSpec {
    PoolSpec {
        cache_ttl: json,
        ..Default::default()
    }
}

fn json_400(message: &str) -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: message.as_bytes().to_vec(),
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

/// The router's events up to and including the first one `last` accepts, under a five-second bound.
async fn router_events_until(
    events: &mut broadcast::Receiver<RouterEvent>,
    last: impl Fn(&RouterEvent) -> bool,
) -> Vec<RouterEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the router's event arrives")
            .expect("not lagged");
        let done = last(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

/// The decisions among `events` for `request_id`, in the order the router made them.
fn decisions_of(
    events: &[RouterEvent],
    request_id: &str,
) -> Vec<(DecisionSummary, DecisionRecord)> {
    events
        .iter()
        .filter_map(|event| match event {
            RouterEvent::Decision {
                summary, record, ..
            } if record.request.request_id == request_id => {
                Some(((**summary).clone(), (**record).clone()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_marker_inside_a_tool_results_own_content_is_rewritten_like_every_other() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        // An api_key account always pays, so its writes take the metered set's own `main` here.
        policy(r#"{"enabled":true,"metered":{"main":"1h"}}"#),
    )
    .await;
    let native = br#"{"model":"claude-sonnet-4-5","max_tokens":8,"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}]}]}],"stream":true}"#;
    let mut req = request_from_fixture(inference_head(FixtureKind::ApiKey), native, KEY);
    set_header(&mut req, "x-claude-code-session-id", "s1");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 2],
        "the system marker and the one inside the tool result both carry the hour"
    );
    assert!(
        beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "the edit stood, so the hour's beta travels with it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn every_marker_is_rewritten_to_the_policys_ttl_and_the_beta_follows_the_body() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        // An api_key account always pays, so its writes take the metered set's own turns here.
        policy(r#"{"enabled":true,"metered":{"main":"1h","subagent":"5m"}}"#),
    )
    .await;

    // Main: every five-minute marker the api-key fixture carries is rewritten to an hour, and the beta the
    // fixture never sends is appended once.
    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    let betas = beta_list(sent);
    assert_eq!(
        betas
            .iter()
            .filter(|b| b.as_str() == EXTENDED_TTL_BETA)
            .count(),
        1
    );
    assert_eq!(betas.last().map(String::as_str), Some(EXTENDED_TTL_BETA));

    // A subagent turn on the same policy stays at five minutes, the fixture's own shape, and asks for no beta.
    let mut sub = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    set_header(&mut sub, "x-claude-code-agent-id", "sub-1");
    let resp = h.app.clone().oneshot(sub).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[1];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3]
    );
    assert!(!beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA));

    // The first-party fixture under an oauth account, subagent five minutes: the fixture already carries the
    // extended beta and three one-hour markers (one with `scope: global`), so the subagent turn rewrites every
    // marker down and drops the beta the caller sent.
    let oh = harness(
        &up,
        &[oauth("o", OA, 0)],
        policy(r#"{"enabled":true,"plan":{"main":"1h","subagent":"5m"}}"#),
    )
    .await;
    let mut sub_oauth = request_from_fixture(
        inference_head(FixtureKind::OauthFirstParty),
        body(BodyFixture::FirstParty9b244),
        KEY,
    );
    set_header(&mut sub_oauth, "x-claude-code-agent-id", "sub-1");
    let resp = oh.app.clone().oneshot(sub_oauth).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[2];
    assert_eq!(
        markers(&sent.body),
        vec![
            r#"{"type":"ephemeral"}"#.to_string(),
            r#"{"type":"ephemeral","scope":"global"}"#.to_string(),
            r#"{"type":"ephemeral"}"#.to_string(),
        ],
        "three markers stand, none added or removed"
    );
    assert!(!beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        phoebis_cch::sign(&sent.body).unwrap(),
        sent.body,
        "the restamp covers the rewrite"
    );

    up.shutdown().await;
}

#[tokio::test]
async fn with_the_toggle_off_a_marker_less_body_goes_as_sent_and_asks_for_no_beta() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // An api_key account has no identity to splice, and a body carrying no attribution line and no `system`
    // gives the restoration and diagnostics edits nothing to touch either, so a marker-less body's only
    // possible edit is the one this test is proving does not run.
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"metered":{"main":"1h"}}"#),
    )
    .await;

    let native: &[u8] =
        br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
    let req = request_from_fixture(inference_head(FixtureKind::ApiKey), native, KEY);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        sent.body == native,
        "no marker anywhere and the toggle off: the body reaches the upstream byte-identical"
    );
    assert!(
        !beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "no beta for a TTL the body never claims"
    );
    assert_eq!(
        outcome(&resp).cache_ttl_ms,
        None,
        "the attempt reports no TTL"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_marker_the_rewrite_cannot_reach_is_refused_rather_than_sent_unenforced() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"metered":{"main":"5m"}}"#),
    )
    .await;
    let native = body(BodyFixture::Oauth);
    assert_eq!(
        markers(native),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    let escaped = String::from_utf8(native.to_vec())
        .unwrap()
        .replacen(r#""cache_control""#, r#""cache_contr\u006fl""#, 1)
        .into_bytes();
    assert!(
        escaped.windows(6).any(|w| w == br"\u006f"),
        "the escape is in the raw bytes"
    );
    assert_eq!(
        markers(&escaped).len(),
        2,
        "the walk no longer reaches the first marker, which a decoder still reads as an hour"
    );

    let req = request_from_fixture(inference_head(FixtureKind::ApiKey), &escaped, KEY);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.headers()
            .get("x-phoebis-error-code")
            .and_then(|v| v.to_str().ok()),
        Some("body_shape")
    );
    let answer: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        answer["error"]["message"],
        "the request body carries a cache_control marker the relay cannot place, so the pool's cache TTL policy cannot be applied to it"
    );
    assert!(
        up.requests().is_empty(),
        "no hour the policy could not enforce reached the upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_marker_the_rewrite_cannot_read_is_refused_rather_than_left_at_its_own_ttl() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"metered":{"main":"5m"}}"#),
    )
    .await;
    let unreadable = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(
            r#"{"type":"ephemeral","ttl":"1h"}"#,
            r#"{"type":1e400,"type":"ephemeral","ttl":"1h"}"#,
            1,
        )
        .into_bytes();
    assert_eq!(
        markers(&unreadable).len(),
        3,
        "the walk reaches every marker, the unreadable one first"
    );

    let req = request_from_fixture(inference_head(FixtureKind::ApiKey), &unreadable, KEY);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.headers()
            .get("x-phoebis-error-code")
            .and_then(|v| v.to_str().ok()),
        Some("body_shape")
    );
    assert!(
        up.requests().is_empty(),
        "no body with one marker left at its own hour reached the upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn with_the_toggle_on_a_marker_less_body_takes_the_member() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("o", OA, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"plan":{"main":"1h"}}"#),
    )
    .await;

    let stripped = without_markers(body(BodyFixture::Oauth));
    let req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &stripped,
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
        "the wire carries exactly the top-level member"
    );
    let text = String::from_utf8_lossy(&sent.body);
    let cache_at = text
        .find("\"cache_control\"")
        .expect("the member is present");
    let stream_at = text.find("\"stream\"").expect("the fixture body streams");
    assert!(cache_at < stream_at, "the top-level member precedes stream");
    assert!(
        beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "the hour's beta follows the injected member"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn with_the_toggle_on_a_marker_less_auxiliary_body_goes_as_sent() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // An api_key account has no identity to splice, and a body carrying no attribution line and no `system`
    // gives the restoration and diagnostics edits nothing to touch either, so the policy's member is the one
    // edit the relay could still make.
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"metered":{"main":"1h"}}"#),
    )
    .await;

    let native: &[u8] =
        br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
    let mut req = request_from_fixture(inference_head(FixtureKind::ApiKey), native, KEY);
    set_header(&mut req, "x-claude-code-request-class", "auxiliary");
    let client_betas: Vec<String> = req
        .headers()
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .expect("the recorded head names its betas")
        .split(',')
        .map(str::trim)
        .map(String::from)
        .collect();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    let wire: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert!(
        wire.get("cache_control").is_none(),
        "a one-off helper's write is one nothing reads, so the policy adds no top-level member"
    );
    assert!(
        sent.body == native,
        "the auxiliary body reaches the upstream byte-identical"
    );
    assert_eq!(
        beta_list(sent),
        client_betas,
        "no beta for a TTL the body never claims, and none of the client's dropped"
    );
    assert_eq!(
        outcome(&resp).cache_ttl_ms,
        None,
        "the attempt reports no TTL"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn with_the_toggle_on_a_marker_less_main_body_takes_the_member() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("o", OA, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"plan":{"main":"1h"}}"#),
    )
    .await;

    let stripped = without_markers(body(BodyFixture::Oauth));
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &stripped,
        KEY,
    );
    set_header(&mut req, "x-claude-code-request-class", "main");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
        "a conversation class is not auxiliary, so the wire carries exactly the top-level member"
    );
    let text = String::from_utf8_lossy(&sent.body);
    let cache_at = text
        .find("\"cache_control\"")
        .expect("the member is present");
    let stream_at = text.find("\"stream\"").expect("the fixture body streams");
    assert!(cache_at < stream_at, "the top-level member precedes stream");
    assert!(
        beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "the hour's beta follows the injected member"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_auxiliary_body_carrying_a_marker_is_rewritten_to_the_policys_ttl() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // Five minutes against the fixture's three one-hour markers, so a leg that skipped the rewrite for an
    // auxiliary class would leave the hour on the wire rather than pass on a no-op edit.
    let h = harness(
        &up,
        &[oauth("o", OA, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"plan":{"main":"5m"}}"#),
    )
    .await;
    let mut events = h.router.subscribe_events();

    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    set_header(&mut req, "x-claude-code-request-class", "auxiliary");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "the client's own markers take the policy's TTL whatever the request's class"
    );
    let id = outcome(&resp).request_id;
    assert_eq!(outcome(&resp).cache_ttl_ms, Some(300_000));
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == id)
    })
    .await;
    assert_eq!(
        decisions_of(&seen, &id)
            .iter()
            .map(|(_, record)| record.request.flags.marker)
            .collect::<Vec<_>>(),
        vec![true],
        "a body carrying the client's marker carries one whatever its class"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_decisions_marker_reads_false_for_the_auxiliary_marker_less_body_and_true_for_the_class_less_one(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("o", OA, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"plan":{"main":"1h"}}"#),
    )
    .await;
    let mut events = h.router.subscribe_events();
    let stripped = without_markers(body(BodyFixture::Oauth));

    let mut auxiliary = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &stripped,
        KEY,
    );
    set_header(&mut auxiliary, "x-claude-code-request-class", "auxiliary");
    let resp = h.app.clone().oneshot(auxiliary).await.unwrap();
    assert_eq!(resp.status(), 200);
    let auxiliary_id = outcome(&resp).request_id;

    let class_less = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &stripped,
        KEY,
    );
    let resp = h.app.clone().oneshot(class_less).await.unwrap();
    assert_eq!(resp.status(), 200);
    let class_less_id = outcome(&resp).request_id;

    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == class_less_id)
    })
    .await;
    let marker = |id: &str| {
        decisions_of(&seen, id)
            .iter()
            .map(|(_, record)| record.request.flags.marker)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        marker(&auxiliary_id),
        vec![false],
        "the policy leaves an auxiliary helper's marker-less body alone, so it neither carries nor receives a marker"
    );
    assert_eq!(
        marker(&class_less_id),
        vec![true],
        "a class-less body receives the policy's member"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_count_never_takes_the_member() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages/count_tokens".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"input_tokens":7}"#.to_vec(),
        },
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"metered":{"main":"1h"}}"#),
    )
    .await;
    let count = request_from_fixture(
        inference_head(FixtureKind::CountApiKey),
        body(BodyFixture::CountApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(count).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        sent.body == body(BodyFixture::CountApiKey),
        "the toggle never reaches a count, which carries no marker of its own and travels as sent"
    );
    assert!(
        markers(&sent.body).is_empty(),
        "no top-level member was added"
    );
    assert!(
        !beta_list(sent).iter().any(|b| b == EXTENDED_TTL_BETA),
        "no beta for a count that took no member"
    );
    up.shutdown().await;
}

async fn set_affinity_mode(h: &Harness, mode: AffinityMode) {
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.affinity.mode = mode;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
}

#[tokio::test]
async fn a_marker_less_count_under_when_cached_with_the_toggle_on_creates_no_pin() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages/count_tokens".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"input_tokens":7}"#.to_vec(),
        },
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("acct", A, 0)],
        policy(r#"{"enabled":true,"inject_when_absent":true,"metered":{"main":"1h"}}"#),
    )
    .await;
    set_affinity_mode(&h, AffinityMode::WhenCached).await;
    let count = request_from_fixture(
        inference_head(FixtureKind::CountApiKey),
        body(BodyFixture::CountApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(count).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await;
    assert!(
        h.router.pins().await.unwrap().is_empty(),
        "a marker-less count carries no marker of its own, so it creates no pin under when_cached even with the toggle on"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn under_when_cached_a_cache_control_object_in_a_tool_schema_pins_nothing_and_a_block_marker_pins(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[api_key("acct", A, 0)], PoolSpec::default()).await;
    set_affinity_mode(&h, AffinityMode::WhenCached).await;
    let send = |native: &'static [u8], session: &'static str| {
        let app = h.app.clone();
        async move {
            let mut req = request_from_fixture(inference_head(FixtureKind::ApiKey), native, KEY);
            set_header(&mut req, "x-claude-code-session-id", session);
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 200);
            let _ = resp.into_body().collect().await;
        }
    };

    send(
        br#"{"model":"claude-sonnet-4-5","max_tokens":8,"tools":[{"name":"t","input_schema":{"type":"object","properties":{"cache_control":{"type":"object"}}}}],"messages":[{"role":"user","content":"hi"}]}"#,
        "schema-only",
    )
    .await;
    assert!(
        h.router.pins().await.unwrap().is_empty(),
        "a tool schema naming a property cache_control writes no cache, so the request pins nothing"
    );

    send(
        br#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#,
        "marked",
    )
    .await;
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        pins.iter()
            .map(|p| p.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["marked"],
        "a content block's marker pins"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_metered_write_takes_the_metered_set_and_a_plan_write_the_plan_set() {
    // Inverted against each fixture's own markers, so a leg that read the wrong set would rewrite to the
    // wrong TTL rather than pass on a no-op edit.
    const POLICY: &str = r#"{"enabled":true,"plan":{"main":"5m"},"metered":{"main":"1h"}}"#;
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[api_key("acct", A, 0)], policy(POLICY)).await;
    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3],
        "an api_key write pays, so the metered set's hour rewrites the fixture's bare markers"
    );
    assert_eq!(outcome(&resp).cache_ttl_ms, Some(3_600_000));

    let oh = harness(&up, &[oauth("o", OA, 0)], policy(POLICY)).await;
    let req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    let resp = oh.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "an oauth account on plan quota reads the plan set's five minutes, stripping the fixture's hour"
    );
    assert_eq!(outcome(&resp).cache_ttl_ms, Some(300_000));
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_writes_with_its_agents_branch_ttl() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", OA, 0)],
        policy(r#"{"enabled":true,"plan":{"main":"5m","subagent":"1h"}}"#),
    )
    .await;
    let probe = || phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
    };
    let resp = h
        .relay
        .probe(probe(), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "the probe's replay reads its agent's branch, main's five minutes, not the hour the subagent branch holds"
    );

    let mut sub = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut sub, "x-claude-code-agent-id", "sub-1");
    let resp = h.relay.probe(probe(), sub).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3],
        "a subagent's probe reads the subagent branch's hour"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_per_model_override_and_the_metered_value_apply_and_overage_is_read_per_attempt() {
    const HAIKU: &str = "claude-haiku-4-5-20251001";
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        OA,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "allowed".into(),
                ),
                (
                    "anthropic-ratelimit-unified-overage-in-use".into(),
                    "true".into(),
                ),
            ],
            body: b"{}".to_vec(),
        },
    );
    up.add_credential_rule(OB, "/v1/messages", json_200());
    let h = harness_with(
        &up,
        &[oauth("a", OA, 0), oauth("b", OB, 1)],
        PoolSpec {
            cache_ttl: r#"{"enabled":true,"metered":{"main":"5m"},"plan":{"main":"1h","models":[{"model":"claude-haiku-4-5-20251001","main":"5m","subagent":"5m"}]}}"#,
            overage_policy: OveragePolicy::Allow,
            ..Default::default()
        },
        default_limits(),
    )
    .await;

    // First request: `a` has no overage evidence yet, so the policy's main rule (an hour) applies.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "a");
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );

    // Second request, same session: `allow` keeps the pin on `a`, whose first response reported overage, so
    // this attempt reads the metered value instead.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "a");
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "a is in overage"
    );

    // A 429 on `a` fails the session over to `b`: the failed attempt still priced against a's overage state,
    // and b's own attempt reads its own (unexhausted) state.
    up.push_credential_rule(OA, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        markers(&up.requests()[2].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "a's failed attempt still priced against its own overage"
    );
    assert_eq!(
        markers(&up.requests()[3].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3],
        "b is not in overage"
    );

    // A haiku request in the same session: a's cooldown from the 429 excludes it, and the per-model override
    // on b (not in overage) reads five minutes regardless.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, HAIKU, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        markers(&up.requests()[4].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3]
    );

    up.shutdown().await;
}

#[tokio::test]
async fn a_refused_extended_ttl_beta_reverts_the_body_to_five_minutes_and_stays_reverted_on_that_account(
) {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", json_400(REFUSED_TTL_BETA));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("a", A, 0), api_key("b", B, 1)],
        // An api_key account always pays, so its writes take the metered set's own `main` here.
        policy(r#"{"enabled":true,"metered":{"main":"1h"}}"#),
    )
    .await;

    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        up.requests().len(),
        2,
        "the strip retry is a second upstream request on this leg"
    );
    assert_eq!(
        up.requests()[0].credential(),
        up.requests()[1].credential(),
        "both attempts went out on the same account"
    );
    assert_eq!(
        markers(&up.requests()[0].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    assert!(beta_list(&up.requests()[0])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        markers(&up.requests()[1].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3],
        "the retry rewrites every marker to five minutes"
    );
    assert!(!beta_list(&up.requests()[1])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));
    assert_eq!(
        phoebis_cch::find_cch(&up.requests()[1].body)
            .map(|l| up.requests()[1].body[l.digits_start..l.digits_start + 5].to_vec()),
        Some(b"00000".to_vec()),
        "the rewrite leaves the placeholder unsigned"
    );
    assert_eq!(outcome(&resp).cache_ttl_ms, Some(300_000));

    // The account is latched against the beta: the next request on it skips straight to five minutes.
    up.push_credential_rule(A, "/v1/messages", json_200());
    let req = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.requests().len(), 3);
    assert_eq!(
        markers(&up.requests()[2].body),
        vec![r#"{"type":"ephemeral"}"#.to_string(); 3]
    );
    assert!(!beta_list(&up.requests()[2])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));

    // A fresh account never latched serves another session at the full policy, undisturbed by a's history.
    h.store.set_enabled("a", false).unwrap();
    h.router.reload().await.unwrap();
    up.push_credential_rule(B, "/v1/messages", json_200());
    let mut other_session = request_from_fixture(
        inference_head(FixtureKind::ApiKey),
        body(BodyFixture::ApiKey),
        KEY,
    );
    set_header(&mut other_session, "x-claude-code-session-id", "s2");
    let resp = h.app.clone().oneshot(other_session).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        markers(&up.requests()[3].body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string(); 3]
    );
    assert!(beta_list(&up.requests()[3])
        .iter()
        .any(|b| b == EXTENDED_TTL_BETA));

    up.shutdown().await;
}

/// The tool-search shape (the beta line, a `tool_reference` block) named no fence any pool can trip any more:
/// every account dials the one measured host, so the request carrying it reaches the upstream as sent.
#[tokio::test]
async fn a_request_carrying_the_tool_search_betas_reaches_the_upstream_unchanged() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", OA, 0), api_key("k", A, 1)],
        PoolSpec::default(),
    )
    .await;
    let reference = r#"{"type":"tool_reference","tool_name":"lookup"}"#;
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(
            r#""messages":[{"role":"user","content":["#,
            &format!(r#""messages":[{{"role":"user","content":[{reference},"#),
            1,
        );
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        KEY,
    );
    set_header(
        &mut req,
        "anthropic-beta",
        "claude-code-20250219,oauth-2025-04-20,advanced-tool-use-2025-11-20",
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        beta_list(sent)
            .iter()
            .any(|b| b == "advanced-tool-use-2025-11-20"),
        "the beta line travels as sent"
    );
    assert!(
        String::from_utf8_lossy(&sent.body).contains(reference),
        "the tool_reference block travels as sent"
    );
    up.shutdown().await;
}
