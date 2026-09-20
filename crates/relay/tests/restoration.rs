mod common;
use common::*;

use axum::body::Body;
use axum::http::Request;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::MemoryKey;
use phoebis_store::AccountKind;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use tower::ServiceExt;

const UUID_RE_LEN: usize = 36;

fn billing_line(wire: &[u8]) -> String {
    let (start, end) = phoebis_cch::string_member_span(wire, &["system", "0", "text"])
        .expect("the recorded body carries a billing line at system[0]");
    serde_json::from_slice(&wire[start..end]).expect("the span is a JSON string")
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    let at = line.find(&needle)?;
    let rest = &line[at + needle.len()..];
    Some(&rest[..rest.find(';').unwrap_or(rest.len())])
}

fn header<'a>(r: &'a fake_upstream::RecordedRequest, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn position(r: &fake_upstream::RecordedRequest, name: &str) -> usize {
    r.headers
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case(name))
        .unwrap()
}

fn ok_with_request_id(id: &str) -> Rule {
    Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), id.into())],
            body: br#"{"id":"msg_01","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }
}

fn cli_request(session: &str, agent: Option<&str>, parent: Option<&str>) -> Request<Body> {
    let mut req = request_for(KEY, "claude-sonnet-4-5", session);
    if let Some(agent) = agent {
        set_header(&mut req, "x-claude-code-agent-id", agent);
    }
    if let Some(parent) = parent {
        set_header(&mut req, "x-claude-code-parent-agent-id", parent);
    }
    req
}

/// A tool loop shape built directly rather than sliced out of the fixture: the fixture's `messages` array is not
/// the last top-level member, so a byte-level splice would have to track nesting the tests elsewhere never need.
fn tool_loop_request(session: &str) -> Request<Body> {
    let native = br#"{"model":"claude-sonnet-4-5","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"}],"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"},"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#;
    let mut req =
        request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), native, KEY);
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// The opt-out shape: `CLAUDE_CODE_ATTRIBUTION_HEADER=false` drops the billing line but leaves the rest of
/// `system` in place, so `system[0]` becomes whatever followed it.
fn without_billing_line(session: &str) -> Request<Body> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    value["system"]
        .as_array_mut()
        .expect("the fixture carries a system array")
        .remove(0);
    let native = serde_json::to_vec(&value).expect("a Value always re-encodes");
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &native,
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

#[tokio::test]
async fn absent_fields_are_restored_in_native_order_and_present_ones_are_left_alone() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    let sent = &up.requests()[0];
    let line = billing_line(&sent.body);
    let names: Vec<&str> = line
        .trim_start_matches("x-anthropic-billing-header: ")
        .split("; ")
        .map(|f| f.split('=').next().unwrap())
        .collect();
    assert_eq!(
        names,
        ["cc_version", "cc_entrypoint", "cch", "cc_prompt_id"],
        "cch and cc_prompt_id join the two the client sent, in the client's order; cc_prev_req is absent on a first turn"
    );
    assert_eq!(
        field(&line, "cc_version"),
        Some("2.1.269.517"),
        "the client's own value is untouched"
    );
    assert_eq!(
        field(&line, "cc_prompt_id").map(str::len),
        Some(UUID_RE_LEN)
    );
    let signed = phoebis_cch::sign(&sent.body).unwrap();
    assert_eq!(
        signed, sent.body,
        "the restored cch is the signature of the body as sent"
    );
    assert_eq!(
        position(sent, "x-client-request-id"),
        position(sent, "x-app") + 1,
        "after x-app, where the client puts it"
    );
    assert_eq!(
        header(sent, "x-client-request-id").map(str::len),
        Some(UUID_RE_LEN)
    );
    // The first-party head already carries one: it travels once, as sent, and nothing is generated.
    app.clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            body(BodyFixture::FirstParty9b244),
            KEY,
        ))
        .await
        .unwrap();
    let fp = &up.requests()[1];
    assert_eq!(
        fp.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("x-client-request-id"))
            .count(),
        1
    );
    assert_eq!(
        header(fp, "x-client-request-id"),
        Some("33333333-3333-4333-8333-333333333333")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn cc_prev_req_names_the_previous_request_id_on_the_same_session_agent_and_account() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        },
    );
    app.clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    let second = billing_line(&up.requests()[1].body);
    assert_eq!(field(&second, "cc_prev_req"), Some("req_up_0001"));
    let names: Vec<&str> = second
        .trim_start_matches("x-anthropic-billing-header: ")
        .split("; ")
        .map(|f| f.split('=').next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "cc_version",
            "cc_entrypoint",
            "cch",
            "cc_prev_req",
            "cc_prompt_id"
        ]
    );
    app.clone()
        .oneshot(cli_request("s-1", Some("sub-1"), None))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[2].body), "cc_prev_req"),
        None,
        "another agent of the session has no previous request"
    );
    app.clone()
        .oneshot(cli_request("s-2", None, None))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[3].body), "cc_prev_req"),
        None,
        "another session neither"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_prompt_id_is_reused_across_a_tool_loop_minted_on_a_fresh_turn_and_inherited_by_a_subagent(
) {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    let first = field(&billing_line(&up.requests()[0].body), "cc_prompt_id")
        .unwrap()
        .to_string();
    // A tool loop: the last message is a user turn carrying a tool_result.
    app.clone().oneshot(tool_loop_request("s-1")).await.unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[1].body), "cc_prompt_id"),
        Some(first.as_str()),
        "a tool loop keeps the turn's id"
    );
    app.clone()
        .oneshot(cli_request("s-1", Some("sub-1"), Some("main")))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[2].body), "cc_prompt_id"),
        Some(first.as_str()),
        "a subagent's first request inherits its parent's"
    );
    app.clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    assert_ne!(
        field(&billing_line(&up.requests()[3].body), "cc_prompt_id"),
        Some(first.as_str()),
        "a fresh user turn mints a new id"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_first_party_turns_own_prompt_id_is_what_its_subagent_inherits() {
    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const SENT: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            body(BodyFixture::FirstParty9b244),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[0].body), "cc_prompt_id"),
        Some(SENT),
        "a line that carries the field is relayed with it"
    );
    app.clone()
        .oneshot(cli_request(SESSION, Some("sub-1"), Some("main")))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[1].body), "cc_prompt_id"),
        Some(SENT),
        "a reduced-shape subagent inherits the id its parent put on the wire, not one minted beside it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_absent_line_is_inserted_whole_with_the_version_and_entrypoint_from_the_user_agent() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(without_billing_line("s-1"))
        .await
        .unwrap();
    let line = billing_line(&up.requests()[0].body);
    assert!(
        line.starts_with(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cch="
        ),
        "{line}"
    );
    assert!(field(&line, "cc_prompt_id").is_some());
    assert_eq!(
        phoebis_cch::sign(&up.requests()[0].body).unwrap(),
        up.requests()[0].body
    );
    up.shutdown().await;
}

/// A version half that is not digits and dots is a forged User-Agent: the whole-line insert would sign a
/// line carrying whatever `;`-separated field it named.
#[tokio::test]
async fn a_user_agent_whose_version_carries_a_field_of_its_own_restores_nothing() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let mut req = without_billing_line("s-1");
    set_header(
        &mut req,
        "user-agent",
        "claude-cli/2.1.269;cc_prev_req=forged (external, cli)",
    );
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
    let sent = &up.requests()[0];
    let line = billing_line(&sent.body);
    assert!(
        !line.starts_with("x-anthropic-billing-header:"),
        "no line was inserted: {line}"
    );
    assert_eq!(
        header(sent, "x-client-request-id"),
        None,
        "and nothing else of the first-party shape is presented either"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn nothing_is_restored_for_a_probe_turn() {
    // A probe replays what the client sent: the reduced line stays reduced and no header is generated.
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let probe = phoebis_relay::Probe {
        key_id: "desktop".into(),
        allow_oauth: true,
    };
    let resp = h.relay.probe(probe, cli_request("s-p", None, None)).await;
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(header(sent, "x-client-request-id"), None);
    let line = billing_line(&sent.body);
    assert_eq!(
        (
            field(&line, "cch"),
            field(&line, "cc_prev_req"),
            field(&line, "cc_prompt_id")
        ),
        (None, None, None)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_api_key_account_toward_api_anthropic_com_is_restored_too() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    app.clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::ApiKey),
            body(BodyFixture::ApiKey),
            KEY,
        ))
        .await
        .unwrap();
    let sent = &up.requests()[0];
    assert!(field(&billing_line(&sent.body), "cch").is_some());
    assert_eq!(
        position(sent, "x-client-request-id"),
        position(sent, "x-app") + 1
    );
    up.shutdown().await;
}

/// Closes the open item from the response-id capture's review: the `message_start` id path only ever ran
/// through a unit test of the SSE parser, never through a real relayed stream. A streamed commit and a
/// buffered JSON commit both have to leave the upstream's `request-id` behind for the next request's
/// `cc_prev_req`, which is the only wire-visible trace either commit leaves in this build (the message id
/// itself has no restored field yet).
#[tokio::test]
async fn a_streamed_commits_request_id_and_a_json_commits_request_id_both_reach_the_next_requests_cc_prev_req(
) {
    let sse = Respond::Sse {
        status: 200,
        headers: vec![("request-id".into(), "req_stream_0001".into())],
        events: vec![
            SseEvent {
                event: "message_start".into(),
                data: r#"{"type":"message_start","message":{"id":"msg_stream_01","usage":{"input_tokens":1,"output_tokens":0}}}"#.into(),
            },
            SseEvent {
                event: "message_stop".into(),
                data: r#"{"type":"message_stop"}"#.into(),
            },
        ],
        delay_between: std::time::Duration::from_millis(0),
    };
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: sse,
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let app = h.app.clone();
    let key = MemoryKey {
        key_id: "desktop".into(),
        session_id: "s-1".into(),
        agent_id: "main".into(),
    };
    let resp = app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Drained rather than dropped: the message id is captured as the body streams to the caller, by the same
    // task that does the streaming, so it is not there to read back until that task has run to completion.
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .message_id
            .as_deref(),
        Some("msg_stream_01"),
        "the streamed commit's message_start id reaches the memory a unit test alone never proved"
    );
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_json_0002".into())],
            body: br#"{"id":"msg_json_02","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    );
    let resp = app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        h.relay.previous(&key, "acct", "claude-sonnet-4-5").message_id.as_deref(),
        Some("msg_json_02"),
        "a json commit's own id overwrites the streamed one under the same key, session, agent, and account"
    );
    let second = billing_line(&up.requests()[1].body);
    assert_eq!(
        field(&second, "cc_prev_req"),
        Some("req_stream_0001"),
        "a streamed commit's request id reaches the next request just as a json commit's does"
    );
    let resp = app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let third = billing_line(&up.requests()[2].body);
    assert_eq!(
        field(&third, "cc_prev_req"),
        Some("req_json_0002"),
        "the most recently committed attempt's request id is the one carried forward"
    );
    up.shutdown().await;
}
