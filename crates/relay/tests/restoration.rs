mod common;
use common::*;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::MemoryKey;
use phoebis_store::{AccountKind, ApiKeyRow, RulesMode, StorageKey, Store};
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

/// The opt-out shape: `CLAUDE_CODE_ATTRIBUTION_HEADER=false` drops the billing line but leaves the rest of
/// `system` in place, so `system[0]` becomes whatever followed it. `metadata.user_id` is seeded with the
/// account `relay_with` selects, so a test that wants the wire bytes untouched does not have `edit_body`'s own
/// account-identity splice rewrite the fixture's empty one out from under it.
fn bytes_without_billing_line() -> Vec<u8> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    value["system"]
        .as_array_mut()
        .expect("the fixture carries a system array")
        .remove(0);
    value["metadata"]["user_id"] = serde_json::Value::String(format!(
        r#"{{"account_uuid":"{ACCOUNT_UUID}","session_id":"s-1"}}"#
    ));
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
    let placeholder = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen("cch=9b244", "cch=00000", 1)
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
async fn a_turn_that_ends_with_a_control_entry_mints_its_own_prompt_id() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let turn = |bytes: &[u8]| {
        let mut req =
            request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), bytes, KEY);
        set_header(&mut req, "x-claude-code-session-id", "s-1");
        req
    };
    app.clone()
        .oneshot(turn(body(BodyFixture::Oauth)))
        .await
        .unwrap();
    let mut next: serde_json::Value = serde_json::from_slice(body(BodyFixture::Oauth)).unwrap();
    next["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"role": "user", "content": "say hi again"}));
    let next = with_control_entry(&serde_json::to_vec(&next).unwrap(), "high");
    app.clone().oneshot(turn(&next)).await.unwrap();
    let first = billing_line(&up.requests()[0].body);
    let second = billing_line(&up.requests()[1].body);
    let first_id = field(&first, "cc_prompt_id");
    let second_id = field(&second, "cc_prompt_id");
    assert!(first_id.is_some() && second_id.is_some());
    assert_ne!(
        second_id, first_id,
        "the control entry after the user's message leaves the turn the user's, so it mints its own id"
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
async fn a_carried_prompt_id_past_the_bound_is_retained_by_nothing_and_travels_as_sent() {
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
        field(&line, "cc_prompt_id").map(str::len),
        Some(UUID_RE_LEN),
        "nothing was retained to inherit, so the subagent's turn carries a minted id"
    );
    up.shutdown().await;
}

/// A forged `claude-cli/` agent buys nothing on a body that carries no attribution line: the relay never
/// writes a line the client did not, so there is nothing for the agent to unlock.
#[tokio::test]
async fn a_forged_claude_code_user_agent_on_a_body_without_the_block_restores_nothing() {
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

/// The same forged version half, proven where a line already exists to fill: the version gate refuses it
/// there too, not only where there is no line at all.
#[tokio::test]
async fn a_version_spelled_any_other_way_than_the_clients_restores_nothing_on_a_sent_line() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
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
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
    let sent = &up.requests()[0];
    let line = billing_line(&sent.body);
    assert_eq!(field(&line, "cch"), None);
    assert_eq!(field(&line, "cc_prompt_id"), None);
    assert_eq!(header(sent, "x-client-request-id"), None);
    up.shutdown().await;
}

/// A prerelease build spells its version with a suffix; the fields the restoration fills use that spelling,
/// as the client itself would write it.
#[tokio::test]
async fn a_prerelease_client_version_keeps_the_first_party_restoration() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", "s-1");
    set_header(
        &mut req,
        "user-agent",
        "claude-cli/2.1.269-beta.1 (external, cli)",
    );
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
    let sent = &up.requests()[0];
    let line = billing_line(&sent.body);
    assert!(field(&line, "cch").is_some());
    assert!(field(&line, "cc_prompt_id").is_some());
    assert!(
        header(sent, "x-client-request-id").is_some(),
        "the rest of the first-party shape is presented too"
    );
    up.shutdown().await;
}

/// With the account identity already the one `relay_with` seeds and the model already one the pool serves as
/// it is, `edit_body` finds nothing else to write either, so the body a client without the block sends reaches
/// the upstream byte for byte.
#[tokio::test]
async fn a_body_without_the_block_relays_byte_identical_under_a_claude_code_user_agent() {
    let up = FakeUpstream::start(vec![ok_with_request_id("req_up_0001")]).await;
    let app = relay_with(AccountKind::Oauth, &up).await;
    let sent = bytes_without_billing_line();
    let resp = app
        .clone()
        .oneshot(without_billing_line("s-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let recorded = &up.requests()[0];
    assert_eq!(
        recorded.body, sent,
        "the body reaches the upstream exactly as sent"
    );
    assert!(
        !String::from_utf8_lossy(&recorded.body).contains("x-anthropic-billing-header"),
        "no line was inserted, the reason the bytes are identical: {}",
        String::from_utf8_lossy(&recorded.body)
    );
    assert_eq!(header(recorded, "x-client-request-id"), None);
    up.shutdown().await;
}

/// The wire-level characterization of `insert_if_absent`'s seating rule: a first-party line that already ends
/// in `cc_turn_origin` still ends in it once a field is filled ahead of it, and the `cch` filled ahead of it is
/// the client's own placeholder.
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
        r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cc_prompt_id={PROMPT_ID}; cc_turn_origin=human;"}}],"metadata":{{"user_id":"{{\"account_uuid\":\"\",\"session_id\":\"s-1\"}}"}},"messages":[{{"role":"user","content":"hi"}}]}}"#
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
            "cc_turn_origin"
        ],
        "cc_prev_req lands ahead of cc_prompt_id, and cc_turn_origin still closes the line"
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
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s-1",
        "main",
    );
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

#[tokio::test]
async fn the_previous_request_id_and_the_prompt_id_survive_a_restart_through_the_store() {
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
    let prompt = field(&billing_line(&up.requests()[0].body), "cc_prompt_id")
        .unwrap()
        .to_string();
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
        field(&second, "cc_prompt_id"),
        Some(prompt.as_str()),
        "the tool loop keeps the turn's id"
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

/// A subagent's first request after a restart inherits its parent's turn: the parent's own row is read back
/// for the lookup, which reads what is in memory and nothing else.
#[tokio::test]
async fn a_subagent_that_asks_first_after_a_restart_inherits_the_parents_prompt_id() {
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
    let prompt = field(&billing_line(&up.requests()[0].body), "cc_prompt_id")
        .unwrap()
        .to_string();
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
        Some(prompt.as_str()),
        "the subagent inherits the turn its parent left behind rather than minting one beside it"
    );
    up.shutdown().await;
}

/// A response that commits after its key was evicted and saved must not overwrite the saved record with the
/// bare entry its own write would create.
#[tokio::test]
async fn a_commit_after_its_key_was_evicted_and_saved_keeps_the_sessions_turn() {
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
        async move { app.oneshot(cli_request("s-1", None, None)).await.unwrap() }
    });
    wait_for(std::time::Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    let prompt = field(&billing_line(&up.requests()[0].body), "cc_prompt_id")
        .unwrap()
        .to_string();
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
        field(&billing_line(&up.requests()[2].body), "cc_prompt_id"),
        Some(prompt.as_str()),
        "the commit read the saved record back rather than replacing it with a bare entry"
    );
    up.shutdown().await;
}

/// The message-id hook fires from the body task, off the request path that admitted the key: at a cap of one
/// it has to restore the key itself before it writes, or an eviction and a save while the answer is still
/// streaming leaves the write to build a bare entry over the saved one.
#[tokio::test]
async fn the_message_id_hook_reads_the_saved_record_back_before_it_writes() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::SseGated {
            status: 200,
            // No `request-id`: `record_request_id` is skipped, so the commit-time restore installs the entry
            // clean and nothing marks it dirty until the hook does, leaving the narrow window the hook's own
            // restore has to close on its own.
            headers: vec![],
            events: vec![
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
    }])
    .await;
    let h = harness_with_memory_cap(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        1,
    )
    .await;
    let held = tokio::spawn({
        let app = h.app.clone();
        async move { app.oneshot(cli_request("s-1", None, None)).await.unwrap() }
    });
    wait_for(std::time::Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    let prompt = field(&billing_line(&up.requests()[0].body), "cc_prompt_id")
        .unwrap()
        .to_string();
    up.set_rule_after(
        1,
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
        .oneshot(cli_request("s-2", None, None))
        .await
        .unwrap();
    h.relay.flush_memory().await;
    // Several more sessions are gated on the same rule and released together with the held answer: their
    // commits race the hook's own restore for the cap, so the eviction it has to survive can still land
    // between the restore and the write, whichever of them the scheduler runs last.
    up.set_rule_after(
        2,
        "/v1/messages",
        Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0002".into())],
            body: b"{\"id\":\"msg_02\",\"type\":\"message\"}".to_vec(),
        })),
    );
    let others: Vec<_> = ["s-3", "s-4", "s-5", "s-6"]
        .into_iter()
        .map(|session| {
            tokio::spawn({
                let app = h.app.clone();
                async move { app.oneshot(cli_request(session, None, None)).await.unwrap() }
            })
        })
        .collect();
    wait_for(std::time::Duration::from_secs(5), || {
        (up.requests().len() == 6).then_some(())
    })
    .await;
    up.release();
    let resp = held.await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    for other in others {
        other.await.unwrap().into_body().collect().await.unwrap();
    }
    h.relay.flush_memory().await;
    h.app
        .clone()
        .oneshot(tool_loop_request("s-1"))
        .await
        .unwrap();
    let last = up.requests().last().unwrap().body.clone();
    assert_eq!(
        field(&billing_line(&last), "cc_prompt_id"),
        Some(prompt.as_str()),
        "the hook restored before it wrote, so the saved turn was not overwritten by a bare entry"
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
