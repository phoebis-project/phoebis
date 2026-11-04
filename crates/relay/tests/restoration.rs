mod common;
use common::*;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Request};
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{MemoryKey, CLASSIFIER_BETA};
use phoebis_routing::RefusalFacts;
use phoebis_store::{AccountKind, ApiKeyRow, MemoryStore, RulesMode, StorageKey, Store};
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

/// The five digits after `cch=` on the billing line `find_cch` reads, `None` for a body that carries none.
fn cch_digits(wire: &[u8]) -> Option<&[u8]> {
    phoebis_cch::find_cch(wire).map(|l| &wire[l.digits_start..l.digits_start + 5])
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

/// `cli_request` from a client that sends its own `diagnostics` member, so the session memory remembers the
/// answer's id with the pool's switch off. The fixture carries no billing hash, so nothing is re-signed.
fn cli_request_with_member(session: &str) -> Request<Body> {
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(
            r#"{"model""#,
            r#"{"diagnostics":{"previous_message_id":null},"model""#,
            1,
        );
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
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

/// A reduced-shape turn whose billing line already names `id`. Unstamped, as the reduced shape is, so the
/// body carries no hash to reproduce.
fn carrying_prompt_id(session: &str, id: &str) -> Request<Body> {
    let native = format!(
        r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cc_prompt_id={id};"}}],"metadata":{{"user_id":"{{\"account_uuid\":\"\",\"session_id\":\"s-1\"}}"}},"messages":[{{"role":"user","content":"hi"}}]}}"#
    );
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// `req` with the turn's prompt id in the client's header, as its conversation sites send it.
fn with_prompt_id(mut req: Request<Body>, id: &str) -> Request<Body> {
    set_header(&mut req, "x-claude-code-prompt-id", id);
    req
}

/// The opt-out shape: `CLAUDE_CODE_ATTRIBUTION_HEADER=false` drops the billing line but leaves the rest of
/// `system` in place, so `system[0]` becomes whatever followed it.
fn bytes_without_billing_line() -> Vec<u8> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    value["system"]
        .as_array_mut()
        .expect("the fixture carries a system array")
        .remove(0);
    serde_json::to_vec(&value).expect("a Value always re-encodes")
}

fn without_billing_line(session: &str) -> Request<Body> {
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &bytes_without_billing_line(),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

#[tokio::test]
async fn absent_fields_are_restored_in_native_order_and_present_ones_are_left_alone() {
    const PROMPT_ID: &str = "c3d1a4e2-5b6f-4a7c-8d9e-0f1a2b3c4d5e";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(with_prompt_id(cli_request("s-1", None, None), PROMPT_ID))
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
        Some("2.1.310.077"),
        "the client's own value is untouched"
    );
    assert_eq!(
        field(&line, "cc_prompt_id"),
        Some(PROMPT_ID),
        "the id the client's header names"
    );
    assert_eq!(
        cch_digits(&sent.body),
        Some(&b"00000"[..]),
        "the restored cch is the client's placeholder, unsigned"
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
            body(BodyFixture::FirstParty),
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
        Some("22222222-2222-4222-8222-222222222222")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_restored_placeholder_is_signed_when_the_switch_is_on() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let h = harness_with_signed_placeholder(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(field(&billing_line(&sent.body), "cch").is_some());
    assert_eq!(
        phoebis_cch::sign(&sent.body).unwrap(),
        sent.body,
        "the restored cch is the signature of the body as sent"
    );
    assert_ne!(cch_digits(&sent.body), Some(&b"00000"[..]));
    up.shutdown().await;
}

/// The client's unpatched paths send the placeholder on the full first-party line, and the server accepts it:
/// it is no hash to check and none to sign, so it travels as the client sent it, whether or not the switch is on.
#[tokio::test]
async fn a_client_sent_placeholder_on_a_full_shape_line_is_relayed_intact() {
    let placeholder = String::from_utf8(body(BodyFixture::FirstParty).to_vec())
        .unwrap()
        .replacen("cch=91984", "cch=00000", 1)
        .into_bytes();
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let off = relay_with(AccountKind::Oauth, &up).await;
    let on = harness_with_signed_placeholder(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await
    .app;
    for (switch, app) in [("off", off), ("on", on)] {
        let resp = app
            .oneshot(request_from_fixture(
                inference_head(FixtureKind::OauthFirstParty),
                &placeholder,
                KEY,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "switch {switch}");
        assert_eq!(outcome(&resp).client_drift, None, "switch {switch}");
        let sent = up.requests().last().unwrap().body.clone();
        assert_eq!(
            cch_digits(&sent),
            Some(&b"00000"[..]),
            "switch {switch}: the wire carries the client's placeholder"
        );
    }
    assert_eq!(up.requests().len(), 2);
    up.shutdown().await;
}

#[tokio::test]
async fn a_turn_that_ends_with_a_control_entry_gets_the_headers_prompt_id_and_none_without_it() {
    const PROMPT_ID: &str = "5f0c2a9e-7d41-4b8e-9c3a-1e2d4f6a8b0c";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let mut next: serde_json::Value = serde_json::from_slice(body(BodyFixture::Oauth)).unwrap();
    next["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"role": "user", "content": "say hi again"}));
    let next = with_control_entry(&serde_json::to_vec(&next).unwrap(), "high");
    let turn = || {
        let mut req =
            request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), &next, KEY);
        set_header(&mut req, "x-claude-code-session-id", "s-1");
        req
    };
    app.clone()
        .oneshot(with_prompt_id(turn(), PROMPT_ID))
        .await
        .unwrap();
    app.clone().oneshot(turn()).await.unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[0].body), "cc_prompt_id"),
        Some(PROMPT_ID),
        "the header names the turn whatever entry closes the messages"
    );
    assert_eq!(
        field(&billing_line(&up.requests()[1].body), "cc_prompt_id"),
        None,
        "without the header the line carries none, though the memory holds the session's id"
    );
    let sent: serde_json::Value = serde_json::from_slice(&up.requests()[0].body).unwrap();
    let asked: serde_json::Value = serde_json::from_slice(&next).unwrap();
    assert_eq!(
        sent["messages"], asked["messages"],
        "the restoration fills the line alone: the messages reach the upstream as sent, the control entry included"
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
        ["cc_version", "cc_entrypoint", "cch", "cc_prev_req"],
        "a turn that sends no prompt-id header gets no cc_prompt_id"
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
async fn a_request_without_the_prompt_id_header_gets_no_cc_prompt_id_on_a_tool_loop_a_fresh_turn_and_a_subagents_first_turn(
) {
    const PROMPT_ID: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    // The first turn names its id, so the memory holds one for the session from here on.
    app.clone()
        .oneshot(with_prompt_id(cli_request("s-1", None, None), PROMPT_ID))
        .await
        .unwrap();
    // A tool loop: the last message is a user turn carrying a tool_result.
    app.clone().oneshot(tool_loop_request("s-1")).await.unwrap();
    app.clone()
        .oneshot(cli_request("s-1", Some("sub-1"), Some("main")))
        .await
        .unwrap();
    app.clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    for (at, request) in [
        (1, "a tool loop"),
        (2, "a subagent's first request"),
        (3, "a fresh user turn"),
    ] {
        assert_eq!(
            field(&billing_line(&up.requests()[at].body), "cc_prompt_id"),
            None,
            "{request} sends no header, so its line carries no id"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_carried_prompt_id_of_any_length_travels_as_sent_and_a_later_turn_without_the_header_gets_none(
) {
    const SESSION: &str = "8f0d1f6e-9f7a-4f63-9e2d-2f8b1c5a7d31";
    let huge = "a".repeat(100_000);
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(carrying_prompt_id(SESSION, &huge))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[0].body), "cc_prompt_id"),
        Some(huge.as_str()),
        "the caller's own value is relayed as sent, whatever its length"
    );
    app.clone()
        .oneshot(cli_request(SESSION, Some("sub-1"), Some("main")))
        .await
        .unwrap();
    let line = billing_line(&up.requests()[1].body);
    assert_eq!(
        field(&line, "cc_prompt_id"),
        None,
        "the subagent's turn sends no header, so its line carries no id"
    );
    up.shutdown().await;
}

/// The contract's answer to a `claude-cli/` version spelled any other way than the client's: a `403` the client is
/// told not to retry, its message naming the User-Agent header with no semicolon, and nothing sent upstream.
async fn assert_refused_for_the_version(resp: axum::http::Response<Body>, up: &FakeUpstream) {
    assert_eq!(resp.status(), 403);
    assert_eq!(resp.headers()["x-phoebis-error-code"], "client_contract");
    assert_eq!(resp.headers()["x-should-retry"], "false");
    assert_eq!(outcome(&resp).client_contract, Some("client_version"));
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let refusal: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(refusal["error"]["type"], "permission_error");
    let message = refusal["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("User-Agent") && !message.contains(';'),
        "{message}"
    );
    assert!(up.requests().is_empty(), "nothing was sent upstream");
}

/// A forged version half buys nothing on a body that carries no attribution line: the contract judges the version
/// before the line and refuses it before any account is chosen.
#[tokio::test]
async fn a_forged_claude_code_user_agent_is_refused_for_the_contract() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let mut req = without_billing_line("s-1");
    set_header(
        &mut req,
        "user-agent",
        "claude-cli/2.1.269;cc_prev_req=forged (external, cli)",
    );
    assert_refused_for_the_version(app.clone().oneshot(req).await.unwrap(), &up).await;
    up.shutdown().await;
}

/// The same forged version half on a body that carries the line: the version lands in a line this build signs, so
/// the contract refuses it before any account is chosen, and the refusal is recorded as any contract refusal is.
#[tokio::test]
async fn a_version_spelled_any_other_way_than_the_clients_is_refused_for_the_contract() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("acct", "sk-ant-oat01-VERSION", 0)],
        PoolSpec::default(),
        bus,
    )
    .await;
    let mut decisions = h.router.subscribe_events();
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", "s-1");
    set_header(
        &mut req,
        "user-agent",
        "claude-cli/2.1.269;cc_prev_req=forged (external, cli)",
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let request_id = outcome(&resp).request_id;
    assert_refused_for_the_version(resp, &up).await;
    assert_eq!(
        recorded_refusal(&mut decisions, &request_id).await,
        Some(RefusalFacts {
            kind: "client",
            code: "contract",
            reason: "client_version".into(),
        })
    );
    // The request row as the ledger assembles it from the same events.
    let events = drain_until_complete(&mut sub).await;
    let mut assembler = phoebis_ledger::Assembler::new(Arc::new(|| 0));
    let row = events
        .into_iter()
        .flat_map(|e| assembler.observe(e))
        .find_map(|record| match record {
            phoebis_ledger::Record::Request { request, .. } => Some(request),
            _ => None,
        })
        .expect("the refusal's request row");
    assert_eq!(
        (row.error_code.as_deref(), row.client_contract.as_deref()),
        (Some("client_contract"), Some("client_version"))
    );
    up.shutdown().await;
}

/// A prerelease build spells its version with a suffix; the fields the restoration fills use that spelling,
/// as the client itself would write it.
#[tokio::test]
async fn a_prerelease_client_version_keeps_the_first_party_restoration() {
    const PROMPT_ID: &str = "0e1d2c3b-4a59-4687-9a5b-4c3d2e1f0a9b";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", "s-1");
    set_header(&mut req, "x-claude-code-prompt-id", PROMPT_ID);
    set_header(
        &mut req,
        "user-agent",
        "claude-cli/2.1.269-beta.1 (external, cli)",
    );
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
    let sent = &up.requests()[0];
    let line = billing_line(&sent.body);
    assert!(field(&line, "cch").is_some());
    assert_eq!(field(&line, "cc_prompt_id"), Some(PROMPT_ID));
    assert!(
        header(sent, "x-client-request-id").is_some(),
        "the rest of the first-party shape is presented too"
    );
    up.shutdown().await;
}

/// The wire-level characterization of `insert_if_absent`'s seating rule: a first-party line that carries
/// `cc_turn_origin` and two of the fields the client writes after it keeps all three in place once a field is
/// filled ahead of them, and the `cch` filled ahead of it is the client's own placeholder.
#[tokio::test]
async fn a_first_party_line_carrying_cc_turn_origin_keeps_its_order_when_a_field_is_filled() {
    const SESSION: &str = "b2f7a3c1-9e44-4a3f-8b02-2a6a1e9c4f10";
    const PROMPT_ID: &str = "11111111-1111-4111-8111-111111111111";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(cli_request(SESSION, None, None))
        .await
        .unwrap();
    let native = format!(
        r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cc_prompt_id={PROMPT_ID}; cc_turn_origin=human; cc_turn_index=3; cc_project=p;"}}],"metadata":{{"user_id":"{{\"account_uuid\":\"\",\"session_id\":\"s-1\"}}"}},"messages":[{{"role":"user","content":"hi"}}]}}"#
    );
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", SESSION);
    app.clone().oneshot(req).await.unwrap();
    let wire = up.requests()[1].body.clone();
    let line = billing_line(&wire);
    let names: Vec<&str> = line
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
            "cc_prompt_id",
            "cc_turn_origin",
            "cc_turn_index",
            "cc_project"
        ],
        "cc_prev_req lands ahead of cc_prompt_id, and the fields after cc_turn_origin stay last"
    );
    assert_eq!(field(&line, "cc_prev_req"), Some("req_up_0001"));
    assert_eq!(field(&line, "cc_prompt_id"), Some(PROMPT_ID));
    assert_eq!(field(&line, "cc_turn_origin"), Some("human"));
    assert_eq!(
        cch_digits(&wire),
        Some(&b"00000"[..]),
        "the restored cch is the client's placeholder, unsigned"
    );
    up.shutdown().await;
}

/// A probe replays what the client sent: the reduced line stays reduced and no header is generated.
#[tokio::test]
async fn nothing_is_restored_for_a_probe_turn() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let probe = phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
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

/// The stamped first-party body naming `u-1` in its identity slot and signed again, as a client signed in to an
/// account sends it, so the admission's hash check reads a current stamp.
fn naming_an_account() -> Vec<u8> {
    let spliced = String::from_utf8(body(BodyFixture::FirstParty).to_vec())
        .unwrap()
        .replacen(r#"\"account_uuid\":\"\""#, r#"\"account_uuid\":\"u-1\""#, 1);
    assert!(
        spliced.contains(r#"\"account_uuid\":\"u-1\""#),
        "the splice landed"
    );
    phoebis_cch::sign(spliced.as_bytes()).unwrap()
}

/// The members of `metadata.user_id`, decoded from the string the client escapes them in.
fn user_id_of(wire: &[u8]) -> serde_json::Value {
    let root: serde_json::Value = serde_json::from_slice(wire).expect("the body parses");
    serde_json::from_str(
        root["metadata"]["user_id"]
            .as_str()
            .expect("user_id is a string"),
    )
    .expect("user_id holds JSON")
}

/// The upstream saw the empty uuid with the client's other identity members, under a stamp current over the
/// bytes it received.
fn assert_blanked(sent: &fake_upstream::RecordedRequest, native: &[u8]) {
    let (before, after) = (user_id_of(native), user_id_of(&sent.body));
    assert_eq!(after["account_uuid"], "", "the uuid is blanked");
    assert_eq!(after["device_id"], before["device_id"]);
    assert_eq!(after["session_id"], before["session_id"]);
    assert!(
        phoebis_cch::sign(&sent.body).unwrap() == sent.body,
        "the stamp is current over the sent bytes"
    );
}

#[tokio::test]
async fn an_api_key_leg_blanks_a_stamped_bodys_account_uuid_and_re_signs_it() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    let native = naming_an_account();
    let resp = app
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::ApiKey),
            &native,
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_blanked(&up.requests()[0], &native);
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_on_an_api_key_account_carries_the_empty_uuid_too() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let h = harness(
        &up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let native = naming_an_account();
    let probe = phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
    };
    let resp = h
        .relay
        .probe(
            probe,
            request_from_fixture(inference_head(FixtureKind::ApiKey), &native, KEY),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_blanked(&up.requests()[0], &native);
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
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s-1",
        "main",
    );
    // An answer's id is remembered only for a request that carried or received the member, so every body here
    // carries the client's own.
    let resp = app
        .clone()
        .oneshot(cli_request_with_member("s-1"))
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
        .oneshot(cli_request_with_member("s-1"))
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
        .oneshot(cli_request_with_member("s-1"))
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

#[tokio::test]
async fn the_previous_request_id_survives_a_restart_through_the_store() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let store = Arc::new(Store::open(&path, &storage_key).unwrap());
    let h = harness_on(
        store.clone(),
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    h.app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s-1",
        "main",
    );
    assert_eq!(
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .request_id
            .as_deref(),
        Some("req_up_0001")
    );
    // What the binary does at shutdown: the dirty entries reach the store before the process ends.
    h.relay.flush_memory().await;
    drop(h);
    drop(store);
    let store = Arc::new(
        wait_for(std::time::Duration::from_secs(2), || {
            Store::open(&path, &storage_key).ok()
        })
        .await,
    );
    let h = harness_on(
        store.clone(),
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    // The new process holds nothing for the session until a request restores it; the wire proves the read.
    assert_eq!(
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .request_id,
        None
    );
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        },
    );
    h.app
        .clone()
        .oneshot(tool_loop_request("s-1"))
        .await
        .unwrap();
    let second = billing_line(&up.requests()[1].body);
    assert_eq!(
        field(&second, "cc_prev_req"),
        Some("req_up_0001"),
        "restored across the restart"
    );
    assert_eq!(
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .request_id
            .as_deref(),
        Some("req_up_0002"),
        "the restored entry took the new request's id"
    );
    up.shutdown().await;
}

/// An id is reusable, so what the previous holder of `desktop` left in the session memory must never reach the
/// key created under it next: the memory is keyed by the holder's generation as well as its id, and the store's
/// cascade alone would still leave the in-process entry and its unflushed change behind.
#[tokio::test]
async fn a_key_created_again_under_a_used_id_reads_none_of_the_previous_holders_memory() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let first_generation = h.store.api_key("desktop").unwrap().unwrap().created_at;
    h.store.delete_api_key("desktop", "test").unwrap();
    h.store
        .insert_api_key(
            &ApiKeyRow {
                id: "desktop".into(),
                label: "desktop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: Vec::new(),
                model_rules_mode: RulesMode::Extend,
                long_context_twins: true,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: h.store.api_key_digest(KEY).to_vec(),
                prefix: phoebis_store::key_prefix(KEY),
                incarnation: 1,
                created_at: NOW,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    assert_ne!(
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        first_generation,
        "the replacement holder is a later generation of the same id"
    );
    // What the flush task does on its own tick: the entry the deleted holder left dirty is written back after
    // the store's cascade removed its row.
    h.relay.flush_memory().await;
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        },
    );
    let resp = h
        .app
        .clone()
        .oneshot(tool_loop_request("s-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let second = billing_line(&up.requests()[1].body);
    assert_eq!(
        field(&second, "cc_prev_req"),
        None,
        "the new holder's first request on the session names no previous answer"
    );
    up.shutdown().await;
}

/// The session's prompt id rides the saved record through a restart, and no request takes it from there: the
/// line's id is the request's own header, so a subagent asking first and the parent's next turn send none and get
/// none.
#[tokio::test]
async fn a_restart_changes_no_prompt_id_since_the_memory_is_no_longer_its_source() {
    const PROMPT_ID: &str = "3c2b1a09-8f7e-4d6c-9b5a-4f3e2d1c0b9a";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let store = Arc::new(Store::open(&path, &storage_key).unwrap());
    let h = harness_on(
        store.clone(),
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    h.app
        .clone()
        .oneshot(with_prompt_id(cli_request("s-1", None, None), PROMPT_ID))
        .await
        .unwrap();
    h.relay.flush_memory().await;
    drop(h);
    drop(store);
    let store = Arc::new(
        wait_for(std::time::Duration::from_secs(2), || {
            Store::open(&path, &storage_key).ok()
        })
        .await,
    );
    let h = harness_on(
        store.clone(),
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        },
    );
    // The parent has sent nothing since the restart, so its turn lives in the store alone.
    h.app
        .clone()
        .oneshot(cli_request("s-1", Some("sub-1"), Some("main")))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[1].body), "cc_prompt_id"),
        None,
        "the subagent takes nothing from the turn its parent left behind"
    );
    h.app
        .clone()
        .oneshot(tool_loop_request("s-1"))
        .await
        .unwrap();
    let parent = billing_line(&up.requests()[2].body);
    assert_eq!(
        field(&parent, "cc_prev_req"),
        Some("req_up_0001"),
        "the parent's record came back from the store"
    );
    assert_eq!(
        field(&parent, "cc_prompt_id"),
        None,
        "and its saved id stayed off the parent's next line"
    );
    up.shutdown().await;
}

/// A response that commits after its key was evicted and saved must not overwrite the saved record with the
/// bare entry its own write would create.
#[tokio::test]
async fn a_commit_after_its_key_was_evicted_and_saved_keeps_the_sessions_turn() {
    const PROMPT_ID: &str = "6d5c4b3a-2918-4f7e-8d6c-5b4a39281706";
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0001".into())],
            body: br#"{"id":"msg_01","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        })),
    }])
    .await;
    let h = harness_with_memory_cap(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        1,
    )
    .await;
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        },
    );
    let held = tokio::spawn({
        let app = h.app.clone();
        async move {
            app.oneshot(with_prompt_id(cli_request("s-1", None, None), PROMPT_ID))
                .await
                .unwrap()
        }
    });
    wait_for(std::time::Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    assert_eq!(
        field(&billing_line(&up.requests()[0].body), "cc_prompt_id"),
        Some(PROMPT_ID)
    );
    // The one slot goes to another session while that request is still upstream, and the flush the periodic
    // task would run saves what the eviction left behind.
    h.app
        .clone()
        .oneshot(cli_request("s-2", None, None))
        .await
        .unwrap();
    h.relay.flush_memory().await;
    up.release();
    let resp = held.await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    h.relay.flush_memory().await;
    // The commit writes the request id itself, so the record's prompt id alone tells the saved record from the
    // bare entry the commit would otherwise have built.
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s-1",
        "main",
    );
    let saved = h
        .store
        .load(&key)
        .unwrap()
        .expect("the session's record is saved");
    let saved: serde_json::Value = serde_json::from_str(&saved.payload).unwrap();
    assert_eq!(
        saved["prompt_id"], PROMPT_ID,
        "the commit read the saved record back rather than replacing it with a bare entry"
    );
    up.set_rule_after(
        2,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0003".into())],
            body: b"{\"id\":\"msg_03\",\"type\":\"message\"}".to_vec(),
        },
    );
    h.app
        .clone()
        .oneshot(tool_loop_request("s-1"))
        .await
        .unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[2].body), "cc_prev_req"),
        Some("req_up_0001"),
        "the next request names the answer the commit recorded"
    );
    up.shutdown().await;
}

/// The message-id hook fires from the body task, off the request path that admitted the key: at a cap of one
/// it has to restore the key itself before it writes, or an eviction and a save while the answer is still
/// streaming leaves the write to build a bare entry over the saved one.
#[tokio::test]
async fn the_message_id_hook_reads_the_saved_record_back_before_it_writes() {
    const PROMPT_ID: &str = "8e7d6c5b-4a39-4281-9f0e-1d2c3b4a5968";
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200_with_headers(&[("request-id", "req_up_0000")]),
    }])
    .await;
    let h = harness_with_memory_cap(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        1,
    )
    .await;
    // The hook runs only for a request that carried or received the member, so every session here sends the
    // client's own. The session's first answer leaves a request id in its record, which only the saved record
    // carries once the key is evicted.
    let resp = h
        .app
        .clone()
        .oneshot(with_prompt_id(cli_request_with_member("s-1"), PROMPT_ID))
        .await
        .unwrap();
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        field(&billing_line(&up.requests()[0].body), "cc_prompt_id"),
        Some(PROMPT_ID)
    );
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::SseGated {
            status: 200,
            // No `request-id`: the second answer records no request id, so the session's record keeps the first
            // answer's `req_up_0000` for the last assertion.
            headers: vec![],
            events: vec![
                // Past the hold's 64 KiB bound, so the answer commits on the overflow before the gate, and
                // `message_start`, which names the id the hook writes, arrives only once the gate opens.
                SseEvent {
                    event: "ping".into(),
                    data: format!(r#"{{"type":"ping","pad":"{}"}}"#, "x".repeat(80 * 1024)),
                },
                SseEvent {
                    event: "message_start".into(),
                    data: r#"{"type":"message_start","message":{"id":"msg_01","type":"message","usage":{"input_tokens":1,"output_tokens":1}}}"#.into(),
                },
                SseEvent {
                    event: "message_stop".into(),
                    data: r#"{"type":"message_stop"}"#.into(),
                },
            ],
        },
    );
    // The head comes back once the hold commits, so the commit-time restore has run and the eviction below lands
    // before the hook fires.
    let held = h
        .app
        .clone()
        .oneshot(cli_request_with_member("s-1"))
        .await
        .unwrap();
    assert_eq!(held.status(), 200);
    up.set_rule_after(
        2,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        },
    );
    // The one slot goes to another session while the first is still upstream, and the flush saves what the
    // eviction left behind: a single evictor, so the pending overlay (bounded by the same cap) holds exactly
    // this one record rather than shedding it under a pile-up of its own.
    h.app
        .clone()
        .oneshot(cli_request_with_member("s-2"))
        .await
        .unwrap();
    h.relay.flush_memory().await;
    // Nothing else commits before the flush below: the overlay holds one record at this cap, so a later eviction
    // would shed the bare entry a missing restore leaves, and the case could no longer see it.
    up.release();
    held.into_body().collect().await.unwrap();
    h.relay.flush_memory().await;
    // The prompt id came with the session's first request, so a bare entry the hook built without its restore and the
    // flush saved over the record carries none.
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s-1",
        "main",
    );
    let saved = h
        .store
        .load(&key)
        .unwrap()
        .expect("the session's record is saved");
    let saved: serde_json::Value = serde_json::from_str(&saved.payload).unwrap();
    assert_eq!(
        saved["prompt_id"], PROMPT_ID,
        "the hook read the saved record back rather than replacing it with a bare entry"
    );
    h.app
        .clone()
        .oneshot(tool_loop_request("s-1"))
        .await
        .unwrap();
    let last = up.requests().last().unwrap().body.clone();
    assert_eq!(
        field(&billing_line(&last), "cc_prev_req"),
        Some("req_up_0000"),
        "the hook restored before it wrote, so the saved record was not overwritten by a bare entry"
    );
    up.shutdown().await;
}

/// A failover leg reads `previous()` off the map with no restore of its own: at a cap of one, a key evicted and
/// saved while the first leg is still held has to come back before the leg that follows can carry `cc_prev_req`.
#[tokio::test]
async fn a_failover_leg_reads_the_saved_record_back_before_it_builds() {
    const A: &str = "sk-ant-oat01-AAAA";
    const B: &str = "sk-ant-oat01-BBBB";
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0001".into())],
            body: br#"{"id":"msg_01","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }])
    .await;
    let h = harness_with_memory_cap(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        1,
    )
    .await;
    // `a` is out of the pool for the first turn, so it lands on `b` and establishes b's own saved record;
    // readmitted before the second turn so the failover from `a` retries against that earlier data.
    h.store
        .set_pool_members("main", &[member("b", 0)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    h.app
        .clone()
        .oneshot(cli_request("s-1", None, None))
        .await
        .unwrap();
    h.relay.flush_memory().await;
    h.store
        .set_pool_members("main", &[member("a", 0), member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    // The first turn pinned the session to `b`; releasing it lets the second turn's selection start over from
    // priority, so it tries `a` first and only reaches `b` (with its own earlier record intact) on a failover.
    h.router.release_session("desktop", "s-1").await.unwrap();
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(json_429_unified(4_600, "five_hour"))),
    );
    let first_leg = tokio::spawn({
        let app = h.app.clone();
        async move { app.oneshot(cli_request("s-1", None, None)).await.unwrap() }
    });
    wait_for(std::time::Duration::from_secs(5), || {
        (up.requests().len() == 2).then_some(())
    })
    .await;
    // The one slot goes to another session while the first leg is held at `a`; admission evicts `s-1` before
    // this session's own dispatch, which lands on `a` too and gates on the same rule.
    let second_session = tokio::spawn({
        let app = h.app.clone();
        async move { app.oneshot(cli_request("s-2", None, None)).await.unwrap() }
    });
    wait_for(std::time::Duration::from_secs(5), || {
        (up.requests().len() == 3).then_some(())
    })
    .await;
    h.relay.flush_memory().await;
    up.release();
    let (first_resp, second_resp) = tokio::join!(first_leg, second_session);
    let first_resp = first_resp.unwrap();
    let second_resp = second_resp.unwrap();
    assert_eq!(
        first_resp.status(),
        200,
        "the failover leg to b lands the answer"
    );
    first_resp.into_body().collect().await.unwrap();
    second_resp.into_body().collect().await.unwrap();
    // The last request to `b` naming `s-1` is the failover leg; the first is turn one's own original commit.
    let recorded = up
        .requests()
        .into_iter()
        .rfind(|r| {
            r.credential() == Some(B) && header(r, "x-claude-code-session-id") == Some("s-1")
        })
        .expect("b answered the failover leg for s-1");
    assert_eq!(
        field(&billing_line(&recorded.body), "cc_prev_req"),
        Some("req_up_0001"),
        "the failover leg read the saved record back before it built its body"
    );
    up.shutdown().await;
}

/// The first-party body with a system reminder cut mid-pair, stamped again as the client stamps what it
/// sends: a strict decoder refuses it and the drift check admits it.
fn cut_first_party_body(container: bool) -> Vec<u8> {
    let fixture = String::from_utf8(body(BodyFixture::FirstParty).to_vec()).unwrap();
    let mut native = fixture.replacen(
        "Today's date is 2026-10-27.",
        "Today's date is 2026-10-27. \\ud83d",
        1,
    );
    assert!(native.contains("\\ud83d"), "the cut landed");
    if container {
        native = native.replacen(r#"{"model""#, r#"{"container":"cont_1","model""#, 1);
    }
    let signed = phoebis_cch::sign(native.as_bytes()).unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&signed).is_err(),
        "a strict decoder refuses the cut body"
    );
    signed
}

fn cut_request(native: &[u8]) -> Request<Body> {
    request_from_fixture(inference_head(FixtureKind::OauthFirstParty), native, KEY)
}

/// What the guard's first-party fixtures get on their way: the account's uuid spliced into `metadata.user_id`
/// and the stamp signed again over the result.
fn as_relayed(native: &[u8]) -> Vec<u8> {
    let spliced = String::from_utf8(native.to_vec()).unwrap().replacen(
        r#"\"account_uuid\":\"\""#,
        &format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#),
        1,
    );
    phoebis_cch::sign(spliced.as_bytes()).unwrap()
}

#[tokio::test]
async fn a_stamped_body_with_a_lone_surrogate_keeps_its_restoration_its_session_and_a_containers_non_portability(
) {
    const A: &str = "sk-ant-oat01-CUTPAIRA";
    const B: &str = "sk-ant-oat01-CUTPAIRB";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let bus = phoebis_bus::Bus::new();
    let mut sub = bus.subscribe("test", phoebis_bus::Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[
            TestAccount {
                uuid: Some(ACCOUNT_UUID),
                ..oauth("a", A, 0)
            },
            oauth("b", B, 1),
        ],
        PoolSpec::default(),
        bus,
    )
    .await;
    let native = cut_first_party_body(false);

    let resp = h.app.clone().oneshot(cut_request(&native)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await;
    drain_until_complete(&mut sub).await;
    // A pins read rides the router's own queue, so the first turn's report is in the map once it answers.
    let _ = h.router.pins().await;
    assert_eq!(
        up.requests()[0].body,
        as_relayed(&native),
        "the cut escape travels as the client sent it"
    );

    let resp = h.app.clone().oneshot(cut_request(&native)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await;
    drain_until_complete(&mut sub).await;
    let line = billing_line(&up.requests()[1].body);
    assert_eq!(field(&line, "cc_prev_req"), Some("req_up_0001"));
    let restored = String::from_utf8(native.clone()).unwrap().replacen(
        "cc_prompt_id=",
        "cc_prev_req=req_up_0001; cc_prompt_id=",
        1,
    );
    assert_eq!(
        up.requests()[1].body,
        as_relayed(restored.as_bytes()),
        "the restoration is the one edit beyond the guard's"
    );

    // The container lives on the account that served the conversation, so its rejection is the answer.
    up.add_credential_rule(A, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let resp = h
        .app
        .clone()
        .oneshot(cut_request(&cut_first_party_body(true)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        429,
        "the upstream's own rejection comes back"
    );
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-status")
            .unwrap(),
        "rejected"
    );
    assert_eq!(
        credentials_seen(&up),
        [A, A, A],
        "no request reached the second account"
    );
    up.shutdown().await;
}

// The restoration by class: a headless Claude Code 2.1.302 session captured on the wire showed a main turn, which
// `cc_prev_req_names_the_previous_request_id_on_the_same_session_agent_and_account` pins, and a classifier call, one
// of the five requests below. It sent no compaction request, subagent summary fork, main-thread fork or side reply, so
// those four cases pin the table by class as read from the client's builder.

/// A request of the reduced-shape fixture on `session` under `class`, with the contract's items in place: the
/// session and class headers here, the line and `metadata.user_id` in the fixture's body.
fn class_request(session: &str, class: &str) -> Request<Body> {
    let mut req = cli_request(session, None, None);
    set_header(&mut req, "x-claude-code-request-class", class);
    req
}

/// `class_request` on a subagent's line, which names `cc_is_subagent`, under the agent-id header.
fn subagent_class_request(session: &str, class: &str, agent: &str) -> Request<Body> {
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(
            "cc_entrypoint=sdk-cli;",
            "cc_entrypoint=sdk-cli; cc_is_subagent=true;",
            1,
        );
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    set_header(&mut req, "x-claude-code-request-class", class);
    set_header(&mut req, "x-claude-code-agent-id", agent);
    req
}

fn field_names(line: &str) -> Vec<&str> {
    line.trim_start_matches("x-anthropic-billing-header: ")
        .split("; ")
        .map(|f| f.split('=').next().unwrap())
        .collect()
}

#[tokio::test]
async fn a_compaction_request_gets_the_previous_id_too() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(class_request("s-1", "main"))
        .await
        .unwrap();
    let mut compaction = class_request("s-1", "compaction");
    set_header(&mut compaction, "x-claude-code-compaction", "auto");
    let resp = app.clone().oneshot(compaction).await.unwrap();
    assert_eq!(resp.status(), 200);
    let line = billing_line(&up.requests()[1].body);
    assert_eq!(field(&line, "cc_prev_req"), Some("req_up_0001"));
    assert_eq!(
        field(&line, "cc_prompt_id"),
        None,
        "the request sent no prompt-id header"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_classifier_call_gets_cch_alone_though_the_memory_holds_a_previous_id_and_a_prompt_id_for_its_session(
) {
    const PROMPT_ID: &str = "1f2e3d4c-5b6a-4798-8a7b-6c5d4e3f2a1b";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(with_prompt_id(class_request("s-1", "main"), PROMPT_ID))
        .await
        .unwrap();
    let mut classifier = class_request("s-1", "auxiliary");
    classifier
        .headers_mut()
        .append("anthropic-beta", HeaderValue::from_static(CLASSIFIER_BETA));
    let resp = app.clone().oneshot(classifier).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        field_names(&billing_line(&up.requests()[1].body)),
        ["cc_version", "cc_entrypoint", "cch"],
        "a side query's native line carries neither id"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_subagent_summary_fork_with_the_agent_id_header_gets_its_cc_prev_req() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(subagent_class_request("s-1", "subagent", "sub-1"))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(subagent_class_request("s-1", "auxiliary", "sub-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let line = billing_line(&up.requests()[1].body);
    assert_eq!(field(&line, "cc_prev_req"), Some("req_up_0001"));
    assert_eq!(
        field_names(&line),
        [
            "cc_version",
            "cc_entrypoint",
            "cch",
            "cc_is_subagent",
            "cc_prev_req"
        ],
        "the fork sent no prompt-id header, so no cc_prompt_id joins the line"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_main_thread_fork_with_the_prompt_id_header_gets_its_cc_prev_req_and_its_cc_prompt_id() {
    const PROMPT_ID: &str = "4d3c2b1a-0f9e-48d7-9c6b-5a4f3e2d1c0b";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(class_request("s-1", "main"))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(with_prompt_id(class_request("s-1", "auxiliary"), PROMPT_ID))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let line = billing_line(&up.requests()[1].body);
    assert_eq!(field(&line, "cc_prev_req"), Some("req_up_0001"));
    assert_eq!(field(&line, "cc_prompt_id"), Some(PROMPT_ID));
    up.shutdown().await;
}

#[tokio::test]
async fn a_side_reply_gets_cch_alone() {
    const PROMPT_ID: &str = "7b6a5f4e-3d2c-4b1a-8f9e-0d1c2b3a4f5e";
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    app.clone()
        .oneshot(with_prompt_id(class_request("s-1", "main"), PROMPT_ID))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(class_request("s-1", "side_reply"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        field_names(&billing_line(&up.requests()[1].body)),
        ["cc_version", "cc_entrypoint", "cch"],
        "a side query's native line carries neither id"
    );
    up.shutdown().await;
}
