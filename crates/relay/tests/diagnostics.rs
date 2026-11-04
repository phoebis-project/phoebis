mod common;
use common::*;

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fake_upstream::{install, Captured, FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, DiagnosticsSource, Event, RequestComplete, Subscriber};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{MemoryKey, CACHE_DIAGNOSIS_BETA};
use phoebis_routing::{
    DecisionRecord, Report, RouterConfig, RouterEvent, UtilizationSample, WindowReading,
    WindowSource, FIVE_HOUR, SEVEN_DAY,
};
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const ACCT_SECRET: &str = "sk-ant-api03-ACCT0001";
const RESET: i64 = 4_600; // one hour past NOW (1_000 s), in Unix seconds

const REFUSED_BETA: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `cache-diagnosis-2026-04-07` for the `anthropic-beta` header."}}"#;
const REFUSED_REQUEST_ID: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unrecognized request header `x-client-request-id`."}}"#;
const REFUSED_DIAGNOSTICS: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"unknown field diagnostics."}}"#;
const REFUSED_CCH: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unrecognized value in system[0]: cch."}}"#;

fn json_400(message: &str) -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: message.as_bytes().to_vec(),
    }
}

fn header_of<'a>(r: &'a fake_upstream::RecordedRequest, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn beta_header(r: &fake_upstream::RecordedRequest) -> Option<&str> {
    header_of(r, "anthropic-beta")
}

fn has_diagnostics(body: &[u8]) -> bool {
    phoebis_cch::object_member_span(body, &["diagnostics"]).is_some()
}

fn diagnostics_previous(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap();
    value["diagnostics"]["previous_message_id"]
        .as_str()
        .map(String::from)
}

fn carries_the_beta(r: &fake_upstream::RecordedRequest) -> bool {
    beta_header(r).is_some_and(|b| b.split(',').any(|t| t.trim() == CACHE_DIAGNOSIS_BETA))
}

/// Every `cache_control` value's raw text, in body order.
fn markers(body: &[u8]) -> Vec<String> {
    phoebis_cch::cache_control_spans(body)
        .unwrap()
        .into_iter()
        .map(|(start, end)| String::from_utf8(body[start..end].to_vec()).unwrap())
        .collect()
}

/// `native` on the OAuth non-first-party head under `session`.
fn oauth_request(native: &[u8], session: &str) -> Request<Body> {
    let mut req =
        request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), native, KEY);
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// The five digits after `cch=` on the billing line `find_cch` reads, `None` for a body that carries none.
fn cch_digits(body: &[u8]) -> Option<&[u8]> {
    phoebis_cch::find_cch(body).map(|l| &body[l.digits_start..l.digits_start + 5])
}

/// `assert_diagnosed_turn` on a Claude Code inference turn, whose reduced line the relay restores: the restored
/// `cch` is the client's placeholder.
fn assert_diagnosed_inference_turn(r: &fake_upstream::RecordedRequest, previous: Option<&str>) {
    assert_diagnosed_turn(r, previous);
    assert_eq!(
        cch_digits(&r.body),
        Some(&b"00000"[..]),
        "the rewrite leaves the placeholder unsigned"
    );
}

/// Asserts the shape `cache_diagnostics` forces on a native wire body, and that the member leaves no signed hash
/// behind: a probe's line carries no `cch`, and an inference turn's carries the placeholder.
fn assert_diagnosed_turn(r: &fake_upstream::RecordedRequest, previous: Option<&str>) {
    assert_diagnosed_shape(r, previous);
    assert!(
        matches!(cch_digits(&r.body), None | Some(b"00000")),
        "the rewrite leaves the placeholder unsigned: {}",
        String::from_utf8_lossy(&r.body)
    );
}

/// The beta ends the caller's list exactly once, `diagnostics` precedes `stream`, and `previous_message_id` matches
/// `previous`.
fn assert_diagnosed_shape(r: &fake_upstream::RecordedRequest, previous: Option<&str>) {
    let beta = beta_header(r).expect("the beta line is present");
    let tokens: Vec<&str> = beta.split(',').map(str::trim).collect();
    assert_eq!(
        tokens
            .iter()
            .filter(|t| **t == CACHE_DIAGNOSIS_BETA)
            .count(),
        1,
        "{beta}"
    );
    assert_eq!(tokens.last(), Some(&CACHE_DIAGNOSIS_BETA), "{beta}");
    assert_eq!(
        diagnostics_previous(&r.body).as_deref(),
        previous,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let text = String::from_utf8_lossy(&r.body);
    let d_at = text.find("\"diagnostics\"").expect("the member is present");
    let s_at = text.find("\"stream\"").expect("the fixture body streams");
    assert!(d_at < s_at, "diagnostics precedes stream: {text}");
}

const SONNET: &str = "claude-sonnet-4-5";

/// Forces a round trip through the router's command queue: a report reaches the affinity map only when the router
/// drains it, and the map is read directly, so a map read or a later session's conversation climb sees an earlier
/// answer's report only once something queued behind it has come back.
async fn settle(h: &Harness) {
    let _ = h.router.pins().await;
}

/// The message boundaries the relay reads off a body whose `messages` are these.
fn boundaries_of(messages: &[serde_json::Value]) -> Vec<String> {
    phoebis_relay::Chains::read(&serde_json::json!({ "messages": messages }))
        .expect("an object always reads")
        .message_boundaries
        .to_vec()
}

/// The tail boundary of the OAuth fixture's own `messages`, the conversation `request_for` sends.
fn fixture_tail() -> String {
    let value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    phoebis_relay::Chains::read(&value)
        .and_then(|chains| chains.tail_hash().map(String::from))
        .expect("the fixture carries messages")
}

/// The map's entry at `boundary` under `model`, copied out of the lock.
fn entry_at(h: &Harness, model: &str, boundary: &str) -> Option<phoebis_routing::AffinityEntry> {
    let map = h.router.affinity();
    let guard = map.read().unwrap();
    guard.entry(model, boundary).cloned()
}

/// The answer id the map holds at `boundary` under `model`, `None` for no entry or an entry without one.
fn id_at(h: &Harness, model: &str, boundary: &str) -> Option<String> {
    entry_at(h, model, boundary).and_then(|entry| entry.message_id)
}

/// The `previous_message_id` a captured body names: `None` when the body carries no member at all, so a lost
/// member never reads as the member's `null`.
fn member_of(body: &[u8]) -> Option<Option<String>> {
    has_diagnostics(body).then(|| diagnostics_previous(body))
}

/// `member_of` the newest captured body.
fn named(up: &FakeUpstream) -> Option<Option<String>> {
    let sent = up.requests();
    member_of(&sent.last().expect("a request reached the upstream").body)
}

/// Sends `req` and reads its answer whole, as a client does, answering the account that served it.
async fn turn(h: &Harness, req: Request<Body>) -> String {
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let served = outcome(&resp).account_id;
    resp.into_body().collect().await.unwrap();
    served
}

/// One API key account under the switch, answering each request with the next of `ids`.
async fn answering(up: &FakeUpstream, ids: &[&str]) -> Harness {
    for id in ids {
        up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id(id));
    }
    harness(
        up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn the_beta_and_the_member_are_added_when_absent_with_the_previous_id_across_a_turn_a_migration_and_a_failover(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_should_not_be_used"),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", ok_with_id("msg_a01"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;

    // Turn 1 on a: no previous id yet.
    let r1 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    assert_diagnosed_inference_turn(&up.requests()[0], None);

    // Turn 2 on a: previous is turn 1's message_start id.
    let r2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    assert_diagnosed_inference_turn(&up.requests()[1], Some("msg_a01"));

    // Turn 3: a 429 fails the session over to b, which has never answered it, so its previous id is null.
    up.set_credential_rule_after(A, 2, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.add_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    let r3 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r3.status(), 200);
    assert_eq!(outcome(&r3).account_id, "b");
    assert_eq!(up.requests().len(), 4, "a's 429 and b's 200 both count");
    assert_diagnosed_inference_turn(&up.requests()[3], None);

    // Turn 4 on b: previous is b's own turn-3 id.
    let r4 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r4.status(), 200);
    assert_diagnosed_inference_turn(&up.requests()[4], Some("msg_b01"));

    up.shutdown().await;
}

/// The first-party fixture as a client that sends no `diagnostics` member signs it: the fixture's own member cut
/// and the hash signed over what is left, so the relay has a member to insert under a client hash.
fn first_party_without_diagnostics() -> Vec<u8> {
    let mut native = body(BodyFixture::FirstParty).to_vec();
    phoebis_cch::remove_top_level_member(&mut native, "diagnostics")
        .expect("the fixture carries the client's own member");
    phoebis_cch::sign(&native).expect("the fixture signs")
}

/// The inserted member is part of the edit a client hash is re-signed over, so the hash the upstream reads covers
/// the body with the member in it.
#[tokio::test]
async fn a_client_hash_is_re_signed_over_the_inserted_member() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let native = first_party_without_diagnostics();
    assert!(!has_diagnostics(&native), "the client sent no member");
    let req = request_from_fixture(inference_head(FixtureKind::OauthFirstParty), &native, KEY);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0].body;
    assert!(
        has_diagnostics(sent),
        "the member is inserted: {}",
        String::from_utf8_lossy(sent)
    );
    assert_eq!(diagnostics_previous(sent), None);
    assert_ne!(
        cch_digits(sent),
        cch_digits(&native),
        "the client's hash no longer covers the edited body"
    );
    // `sign` rewrites the five digits alone, so equal digits are an equal body.
    let resigned = phoebis_cch::sign(sent).unwrap();
    assert_eq!(
        cch_digits(&resigned),
        cch_digits(sent),
        "the hash is re-signed over the body with the member in it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_flush_after_the_answer_completes_carries_its_id_to_the_store() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let r1 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    r1.into_body().collect().await.unwrap();
    // A spawned write can land after the flush unless the hand-off is awaited ahead of it; the drain above is
    // what makes the flush see it.
    h.relay.flush_memory().await;
    // A relay restarted on the same store holds nothing for the session, and a history that matches no boundary
    // leaves the session memory's id the only one it can name, so the id comes from the row the flush wrote.
    let store = h.store.clone();
    drop(h);
    let h = harness_on(
        store,
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
        default_limits(),
    )
    .await;
    let r2 = h
        .app
        .clone()
        .oneshot(conversation_request(
            KEY,
            "claude-sonnet-4-5",
            "s1",
            &[user("Summary of the conversation so far")],
        ))
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    assert_eq!(
        diagnostics_previous(&up.requests()[1].body).as_deref(),
        Some("msg_01")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_unsent_retry_still_lands_a_well_formed_edit_and_a_present_member_or_beta_is_left_alone()
{
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::CloseImmediately,
    }])
    .await;
    // `CloseImmediately` decides at accept time, before any byte is read, so the fake never records the first
    // connection's request; there is nothing to compare its bytes against, only the retry's own edit to check
    // for correctness.
    up.set_rule_after_connections(1, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.requests().len(), 1);
    assert_diagnosed_inference_turn(&up.requests()[0], None);

    // A body that already names both the member and the beta reaches the upstream with neither touched.
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    value["diagnostics"] = serde_json::json!({ "previous_message_id": "mine" });
    let native = serde_json::to_vec(&value).expect("a Value always re-encodes");
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &native,
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", "s2");
    set_header(&mut req, "anthropic-beta", CACHE_DIAGNOSIS_BETA);
    let resp2 = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp2.status(), 200);
    let sent = &up.requests()[1];
    assert_eq!(diagnostics_previous(&sent.body).as_deref(), Some("mine"));
    assert_eq!(
        beta_header(sent)
            .unwrap()
            .matches(CACHE_DIAGNOSIS_BETA)
            .count(),
        1,
        "the caller's own beta travels once, not doubled by the injector"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_full_shape_request_gets_the_beta_and_the_member_on_a_pool_with_the_switch_on() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let req = request_from_fixture(
        inference_head(FixtureKind::OauthFirstParty),
        body(BodyFixture::FirstParty),
        KEY,
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_diagnosed_shape(sent, None);
    assert_eq!(
        phoebis_cch::sign(&sent.body).unwrap(),
        sent.body,
        "the client's own hash is re-signed over the inserted member"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_clients_own_beta_passes_through_once_with_the_switch_off_and_no_member_is_added() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    let sent_list = req
        .headers()
        .get("anthropic-beta")
        .map(|v| format!("{},{CACHE_DIAGNOSIS_BETA}", v.to_str().unwrap()))
        .unwrap_or_else(|| CACHE_DIAGNOSIS_BETA.to_string());
    set_header(&mut req, "anthropic-beta", &sent_list);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        beta_header(sent)
            .expect("the beta line is present")
            .split(',')
            .filter(|t| t.trim() == CACHE_DIAGNOSIS_BETA)
            .count(),
        1,
        "the client's own beta arrives once"
    );
    assert!(
        !has_diagnostics(&sent.body),
        "the relay adds no member with the switch off"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_with_tools_and_no_marker_gets_neither_the_member_nor_the_beta() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let native = without_markers(body(BodyFixture::Oauth));
    assert!(markers(&native).is_empty(), "the client cached nothing");
    let resp = h
        .app
        .clone()
        .oneshot(oauth_request(&native, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(
        !has_diagnostics(&sent.body),
        "no member for a body without a marker of the client's: {}",
        String::from_utf8_lossy(&sent.body)
    );
    assert!(!carries_the_beta(sent), "{:?}", beta_header(sent));
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_with_a_marker_and_no_tools_or_an_empty_tools_array_gets_neither() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    for (session, empty) in [("s1", false), ("s2", true)] {
        let native = without_tools(body(BodyFixture::Oauth), empty);
        assert!(!markers(&native).is_empty(), "the client's markers stay");
        let resp = h
            .app
            .clone()
            .oneshot(oauth_request(&native, session))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    let sent = up.requests();
    assert_eq!(sent.len(), 2);
    for (r, shape) in sent
        .iter()
        .zip(["no `tools` member", "an empty `tools` array"])
    {
        assert!(!has_diagnostics(&r.body), "{shape}: no member");
        assert!(!carries_the_beta(r), "{shape}: no beta");
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_marker_less_body_with_tools_under_inject_when_absent_takes_the_policys_member_and_no_diagnostics(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            cache_ttl: r#"{"enabled":true,"inject_when_absent":true,"plan":{"main":"1h"}}"#,
            ..Default::default()
        },
    )
    .await;
    // The fixture's head names no request class, so the policy gives the body its member.
    let native = without_markers(body(BodyFixture::Oauth));
    let resp = h
        .app
        .clone()
        .oneshot(oauth_request(&native, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        markers(&sent.body),
        vec![r#"{"type":"ephemeral","ttl":"1h"}"#.to_string()],
        "the wire carries the policy's one marker"
    );
    assert!(
        phoebis_cch::object_member_span(&sent.body, &["cache_control"]).is_some(),
        "and it is the top-level member"
    );
    assert!(
        !has_diagnostics(&sent.body),
        "the gate reads the client's own markers, before the policy's rewrite"
    );
    assert!(!carries_the_beta(sent), "{:?}", beta_header(sent));
    up.shutdown().await;
}

/// Pins the member-less client, the case the insertion exists for: a reduced-shape turn with markers and tools
/// that sends no member of its own.
#[tokio::test]
async fn a_member_less_reduced_shape_turn_takes_the_member_and_the_beta_and_its_second_turn_names_the_first_answer(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    assert!(
        !has_diagnostics(body(BodyFixture::Oauth)),
        "the reduced shape sends no member"
    );
    let r1 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    r1.into_body().collect().await.unwrap();
    assert!(has_diagnostics(&up.requests()[0].body));
    assert_diagnosed_shape(&up.requests()[0], None);
    let r2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    assert_diagnosed_shape(&up.requests()[1], Some("msg_01"));
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_sent_member_on_a_body_without_tools_passes_through_as_sent_and_its_answers_id_is_remembered(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    // The first-party fixture as a client that sends its own member and no tools signs it.
    let mut native = body(BodyFixture::FirstParty).to_vec();
    phoebis_cch::remove_top_level_member(&mut native, "tools").expect("the fixture carries tools");
    let native = phoebis_cch::sign(&native).expect("the fixture signs");
    let (start, end) = phoebis_cch::object_member_span(&native, &["diagnostics"])
        .expect("the client sends its own member");
    let req = request_from_fixture(inference_head(FixtureKind::OauthFirstParty), &native, KEY);
    let betas_of = |list: Option<&str>| {
        list.map_or(0, |l| {
            l.split(',')
                .filter(|t| t.trim() == CACHE_DIAGNOSIS_BETA)
                .count()
        })
    };
    let client_betas = betas_of(
        req.headers()
            .get("anthropic-beta")
            .map(|v| v.to_str().unwrap()),
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    let sent = &up.requests()[0];
    let (sent_start, sent_end) = phoebis_cch::object_member_span(&sent.body, &["diagnostics"])
        .expect("the client's member reaches the upstream");
    assert_eq!(
        &sent.body[sent_start..sent_end],
        &native[start..end],
        "the member goes as the client sent it"
    );
    assert!(
        betas_of(beta_header(sent)) <= client_betas,
        "the beta goes no more often than the client sent it: {:?}",
        beta_header(sent)
    );
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        FIRST_PARTY_SESSION,
        "main",
    );
    assert_eq!(
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .message_id
            .as_deref(),
        Some("msg_01"),
        "a request that carried the member has its answer remembered"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn after_a_turn_the_gate_refused_the_memory_still_names_the_turn_before_it() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_01"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_02"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_03"));
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let key = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s1",
        "main",
    );
    let remembered = || {
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .message_id
    };

    let r1 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    r1.into_body().collect().await.unwrap();
    assert_eq!(remembered().as_deref(), Some("msg_01"));

    // A turn of the same session that sends no tools list: the gate gives it no member, so its answer is not
    // the next turn's predecessor.
    let r2 = h
        .app
        .clone()
        .oneshot(oauth_request(
            &without_tools(body(BodyFixture::Oauth), false),
            "s1",
        ))
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    r2.into_body().collect().await.unwrap();
    assert!(!has_diagnostics(&up.requests()[1].body));
    assert_eq!(
        remembered().as_deref(),
        Some("msg_01"),
        "the gated turn's answer is not remembered"
    );
    // The gated turn sends the first turn's messages, so its report refreshes the same tail, and only the turn
    // that carried the member leaves its id there.
    settle(&h).await;
    assert_eq!(
        id_at(&h, SONNET, &fixture_tail()).as_deref(),
        Some("msg_01"),
        "the gated turn's answer never reaches the map"
    );

    let r3 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r3.status(), 200);
    assert_eq!(
        diagnostics_previous(&up.requests()[2].body).as_deref(),
        Some("msg_01"),
        "the next diagnosed turn names the turn before the gated one"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_400_naming_the_injected_beta_strips_it_retries_once_on_the_same_account_and_latches() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_BETA));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
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
    assert!(
        !beta_header(&up.requests()[1])
            .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)),
        "the retry carries no cache-diagnosis beta"
    );
    assert!(
        has_diagnostics(&up.requests()[1].body),
        "the diagnostics member still stands"
    );
    assert_eq!(outcome(&resp).attempt, 2);

    // A third request on the same session: the latch skips straight to one upstream request.
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let resp3 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp3.status(), 200);
    assert_eq!(up.requests().len(), 3);
    assert!(!beta_header(&up.requests()[2])
        .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    assert!(has_diagnostics(&up.requests()[2].body));

    // The account's label patched twice, with no request in between: the revision moved twice, so a reload
    // clears the latch and the beta is injected again.
    // `set_api_key_secret` already bumped the row to revision 2 when the harness sealed its secret.
    rename(&h, "acct", 2, "acct", "test").unwrap();
    rename(&h, "acct", 3, "acct", "test").unwrap();
    h.router.reload().await.unwrap();
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let resp4 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp4.status(), 200);
    assert!(beta_header(up.requests().last().unwrap())
        .unwrap()
        .split(',')
        .any(|t| t == CACHE_DIAGNOSIS_BETA));

    // Latched again with a scripted 400 on both the first attempt and its strip retry, then the account
    // deleted and re-created under the same id: the incarnation moved, so the latch clears and the beta is
    // injected again.
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_BETA));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_BETA));
    let resp5 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp5.status(),
        400,
        "the strip retry's own 400 has no attempt left to strip again"
    );

    h.store.delete_account("acct", "test").unwrap();
    h.store
        .insert_account(&test_account_row(&api_key("acct", ACCT_SECRET, 0)))
        .unwrap();
    h.store
        .set_api_key_secret("acct", &phoebis_store::Secret::new(ACCT_SECRET))
        .unwrap();
    h.store
        .set_pool_members("main", &[member("acct", 0)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let resp6 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp6.status(), 200);
    assert!(beta_header(up.requests().last().unwrap())
        .unwrap()
        .split(',')
        .any(|t| t == CACHE_DIAGNOSIS_BETA));

    up.shutdown().await;
}

#[tokio::test]
async fn a_400_over_the_buffer_bound_naming_the_injected_beta_strips_it_and_retries_once() {
    // The message names the beta in its first bytes and runs the body far past the 64 KiB read before a verdict, so
    // the drain stops before the body's end and the document never parses whole.
    let refused = format!(
        r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"Unexpected value(s) `cache-diagnosis-2026-04-07` for the `anthropic-beta` header. {}"}}}}"#,
        "x".repeat(200 * 1024)
    );
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(&refused));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).attempt),
        (200, 2),
        "the cut message named the router's own beta, so the leg stripped it and retried"
    );
    assert_eq!(up.requests().len(), 2);
    assert!(!beta_header(&up.requests()[1])
        .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    up.shutdown().await;
}

#[tokio::test]
async fn a_400_naming_a_beta_the_client_sent_is_forwarded_unchanged() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_BETA));
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "anthropic-beta", CACHE_DIAGNOSIS_BETA);
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        up.requests().len(),
        1,
        "the caller's own beta is never stripped"
    );
    assert_eq!(outcome(&resp).attempt, 1);

    // Nothing was latched: the next request, this time naming no beta of its own, still gets the relay's own
    // injection, in one request. A wrongly set latch would show up here as a missing beta.
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    assert_eq!(up.requests().len(), 2);
    assert!(beta_header(&up.requests()[1])
        .unwrap()
        .split(',')
        .any(|t| t == CACHE_DIAGNOSIS_BETA));
    up.shutdown().await;
}

#[tokio::test]
async fn a_400_naming_the_generated_request_id_strips_the_header_and_retries() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_REQUEST_ID));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.requests().len(), 2);
    assert!(header_of(&up.requests()[0], "x-client-request-id").is_some());
    assert!(header_of(&up.requests()[1], "x-client-request-id").is_none());

    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    assert_eq!(
        up.requests().len(),
        3,
        "the latch skips straight to one upstream request"
    );
    assert!(header_of(&up.requests()[2], "x-client-request-id").is_none());
    up.shutdown().await;
}

#[tokio::test]
async fn a_400_naming_the_diagnostics_member_strips_the_member_and_a_second_400_is_forwarded() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_DIAGNOSTICS));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_CCH));
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "one strip round per leg: the second 400 is forwarded"
    );
    assert_eq!(up.requests().len(), 2);
    assert!(!has_diagnostics(&up.requests()[1].body));
    assert_eq!(outcome(&resp).attempt, 2);
    up.shutdown().await;
}

#[tokio::test]
async fn a_strip_retry_runs_on_the_same_account_before_any_failover() {
    // Two accounts: a's 429 fails over and the strip on b still runs on b.
    let up2 = FakeUpstream::start(vec![]).await;
    up2.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up2.push_credential_rule(B, "/v1/messages", json_400(REFUSED_BETA));
    up2.push_credential_rule(B, "/v1/messages", json_200());
    let h2 = harness(
        &up2,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let resp3 = h2
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp3.status(), 200);
    assert_eq!(
        up2.requests().len(),
        3,
        "a's 429, b's 400, and b's corrected retry: three submissions"
    );
    assert_eq!(outcome(&resp3).account_id, "b");
    assert!(!beta_header(&up2.requests()[2])
        .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    up2.shutdown().await;
}

#[tokio::test]
async fn count_tokens_gets_neither_the_member_nor_the_beta_under_the_switch() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages/count_tokens".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"input_tokens":7}"#.to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
    ])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    const COUNT_BODY: &str =
        r#"{"model":"claude-sonnet-4-5[1m]","messages":[{"role":"user","content":"hi"}]}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("Authorization", "Bearer phoebis-testkey")
        .header("User-Agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", "s1")
        .body(Body::from(COUNT_BODY))
        .unwrap();
    let resp = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = &up.requests()[0];
    assert!(!has_diagnostics(&sent.body));
    assert!(beta_header(sent).is_none());
    assert_eq!(
        String::from_utf8_lossy(&sent.body),
        COUNT_BODY.replace("claude-sonnet-4-5[1m]", "claude-sonnet-4-5"),
        "a count's markers are never touched; only the model spelling the fold resolves changes"
    );

    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let sent2 = &up.requests()[1];
    assert!(has_diagnostics(&sent2.body));
    assert!(beta_header(sent2)
        .unwrap()
        .split(',')
        .any(|t| t == CACHE_DIAGNOSIS_BETA));
    up.shutdown().await;
}

/// A probe replays its recipe's member, which from a chain's second probe on names a message older than the upstream
/// keeps, so no probe names one, whether the recipe carried the client's member or the router inserts it.
#[tokio::test]
async fn a_probe_names_no_previous_message_id_and_its_own_answer_is_never_remembered() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_turn"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_probe_1"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_probe_2"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_probe_3"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_turn_2"));
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_diagnosed_inference_turn(&up.requests()[0], None);

    // The probe replays the turn's own headers, session id among them.
    let probe = || phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
    };
    let recipe = first_party_naming(Some("msg_a01"));
    let resp = h
        .relay
        .probe(probe(), first_party_request(&recipe, "s1"))
        .await;
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[1].body;
    assert_eq!(
        member_of(sent),
        Some(None),
        "the recipe's own member names no message"
    );
    assert!(
        signed_over_itself(sent),
        "the hash is re-signed over the null"
    );

    let resp = h
        .relay
        .probe(probe(), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(resp.status(), 200);
    assert_diagnosed_turn(&up.requests()[2], None);
    let resp = h
        .relay
        .probe(probe(), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(resp.status(), 200);
    assert_diagnosed_turn(&up.requests()[3], None);
    let key = phoebis_relay::MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        "s1",
        "main",
    );
    assert_eq!(
        h.relay
            .previous(&key, "acct", "claude-sonnet-4-5")
            .message_id
            .as_deref(),
        Some("msg_turn"),
        "no probe's answer was remembered"
    );
    settle(&h).await;
    assert_eq!(
        id_at(&h, SONNET, &fixture_tail()).as_deref(),
        Some("msg_turn"),
        "the probes refresh the turn's tail and leave its answer there"
    );

    // The next real turn names the turn before it, not a probe.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_diagnosed_inference_turn(&up.requests()[4], Some("msg_turn"));
    up.shutdown().await;
}

/// `req` sent by the agent `agent` of its session.
fn as_agent(mut req: Request<Body>, agent: &str) -> Request<Body> {
    set_header(&mut req, "x-claude-code-agent-id", agent);
    req
}

#[tokio::test]
async fn a_continuation_names_the_answer_to_its_prefix_found_on_the_third_probe() {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_a1", "msg_a2"]).await;
    let second = [user("u1"), assistant("a1"), user("u2")];

    turn(&h, conversation_request(KEY, SONNET, "s1", &second[..1])).await;
    assert_eq!(named(&up), Some(None), "a first turn has no answer to name");
    settle(&h).await;
    // Read before the second turn, whose own report writes its boundaries: the walk from its tail meets no entry
    // at `u2` or at `a1`, and the first turn's answer at `u1`.
    let walked = boundaries_of(&second);
    assert!(entry_at(&h, SONNET, &walked[2]).is_none());
    assert!(entry_at(&h, SONNET, &walked[1]).is_none());
    assert_eq!(id_at(&h, SONNET, &walked[0]).as_deref(), Some("msg_a1"));

    turn(&h, conversation_request(KEY, SONNET, "s1", &second)).await;
    assert_eq!(named(&up), Some(Some("msg_a1".into())));
    up.shutdown().await;
}

#[tokio::test]
async fn a_rewind_names_the_answer_at_the_rewind_point() {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_a1", "msg_a2", "msg_a3", "msg_a4"]).await;
    let third = [
        user("u1"),
        assistant("a1"),
        user("u2"),
        assistant("a2"),
        user("u3"),
    ];
    for depth in [1, 3, 5] {
        turn(&h, conversation_request(KEY, SONNET, "s1", &third[..depth])).await;
    }

    // The user edits `u2` and resends: the conversation continues `a1`'s answer, not the newest one.
    let rewound = [user("u1"), assistant("a1"), user("u2 edited")];
    turn(&h, conversation_request(KEY, SONNET, "s1", &rewound)).await;
    assert_eq!(
        named(&up),
        Some(Some("msg_a1".into())),
        "the memory holds msg_a3"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_forks_first_request_names_the_parents_tail_answer_at_the_fork_point() {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_a1", "msg_a2", "msg_f1"]).await;
    let forked = [
        user("u1"),
        assistant("a1"),
        user("u2"),
        assistant("a2"),
        user("u3"),
    ];
    for depth in [1, 3] {
        turn(
            &h,
            conversation_request(KEY, SONNET, "s1", &forked[..depth]),
        )
        .await;
    }
    // A request with a session header waits on its own session's report alone, so the parent's last report has to
    // be applied before the fork's conversation climb looks for the conversation it branched from.
    settle(&h).await;

    turn(
        &h,
        as_agent(conversation_request(KEY, SONNET, "s2", &forked), "fork-1"),
    )
    .await;
    assert_eq!(
        named(&up),
        Some(Some("msg_a2".into())),
        "the parent's entry at `u2` carries its answer under the parent's root"
    );
    up.shutdown().await;
}

/// `conversation_request` under a system prompt of its own, so the upstream would read a change of it as a
/// divergence from whichever answer the member names.
fn under_another_system_prompt(session: &str, messages: &[serde_json::Value]) -> Request<Body> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    value["model"] = serde_json::Value::from(SONNET);
    value["messages"] = serde_json::Value::Array(messages.to_vec());
    value["system"][2]["text"] = serde_json::Value::from("You review the fork's changes.");
    let native = serde_json::to_vec(&value).expect("a Value always re-encodes");
    oauth_request(&native, session)
}

#[tokio::test]
async fn a_fork_that_regenerates_its_parents_tail_names_its_own_answer_on_its_next_turn() {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_p", "msg_f1", "msg_f2"]).await;
    let next = [
        user("u1"),
        assistant("a1"),
        user("u2"),
        assistant("a2 of the fork"),
        user("u3"),
    ];
    turn(&h, conversation_request(KEY, SONNET, "s1", &next[..3])).await;
    settle(&h).await;

    let fork = |messages: &[serde_json::Value]| {
        as_agent(under_another_system_prompt("s2", messages), "fork-1")
    };
    turn(&h, fork(&next[..3])).await;
    assert_eq!(
        named(&up),
        Some(Some("msg_p".into())),
        "a fork without an answer of its own names its parent's"
    );
    settle(&h).await;
    assert_eq!(
        id_at(&h, SONNET, &boundaries_of(&next[..3])[2]).as_deref(),
        Some("msg_p"),
        "the shared tail keeps the parent's answer under the parent's root"
    );

    turn(&h, fork(&next)).await;
    assert_eq!(
        named(&up),
        Some(Some("msg_f1".into())),
        "the fork continues its own answer at the shared tail, not the parent's"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_describe_helper_of_a_member_less_client_names_the_subagents_last_answer_and_the_next_turn_skips_its_tail(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_a1", "msg_a2", "msg_h", "msg_a3"]).await;
    let next = [
        user("u1"),
        assistant("a1"),
        user("u2"),
        assistant("a2"),
        user("u3"),
    ];
    let subagent = |messages: &[serde_json::Value]| {
        as_agent(conversation_request(KEY, SONNET, "s1", messages), "sub-1")
    };
    for depth in [1, 3] {
        turn(&h, subagent(&next[..depth])).await;
    }

    // The helper resends the subagent's first message with its question appended, under the same tools and
    // markers: one boundary no turn wrote, so the memory names the subagent's last answer.
    let helper = [user("u1 Describe your most recent action")];
    turn(&h, subagent(&helper)).await;
    assert_eq!(named(&up), Some(Some("msg_a2".into())));
    settle(&h).await;
    assert_eq!(
        id_at(&h, SONNET, &boundaries_of(&helper)[0]).as_deref(),
        Some("msg_h"),
        "the helper's answer lands on its own tail"
    );

    turn(&h, subagent(&next)).await;
    assert_eq!(
        named(&up),
        Some(Some("msg_a2".into())),
        "the next turn continues `a2`, which the helper's answer did not"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_first_turn_after_a_compaction_names_the_memorys_newest_answer() {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_a2", "msg_c1"]).await;
    let before = [user("u1"), assistant("a1"), user("u2")];
    turn(&h, conversation_request(KEY, SONNET, "s1", &before)).await;

    let compacted = [user("Summary of the conversation so far, then u3")];
    turn(&h, conversation_request(KEY, SONNET, "s1", &compacted)).await;
    assert_eq!(
        named(&up),
        Some(Some("msg_a2".into())),
        "a new history matches no boundary, so the memory's newest answer is named"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_turn_that_switches_the_model_names_the_memorys_last_id_for_that_model() {
    const OPUS: &str = "claude-opus-4-1";
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_s1", "msg_o1", "msg_o2"]).await;
    turn(&h, conversation_request(KEY, SONNET, "s1", &[user("u1")])).await;

    turn(&h, conversation_request(KEY, OPUS, "s1", &[user("u1")])).await;
    assert_eq!(
        named(&up),
        Some(None),
        "no entry under the new model and no memory for it"
    );

    let next = [user("u1"), assistant("o1"), user("u2")];
    turn(&h, conversation_request(KEY, OPUS, "s1", &next)).await;
    assert_eq!(named(&up), Some(Some("msg_o1".into())));
    up.shutdown().await;
}

#[tokio::test]
async fn an_account_move_names_the_new_accounts_memory_id_and_its_answer_replaces_the_boundaries() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a1"));
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a2"));
    up.push_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b3"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b4"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let fourth = [
        user("u1"),
        assistant("a1"),
        user("u2"),
        assistant("a2"),
        user("u3"),
        assistant("b3"),
        user("u4"),
    ];
    for depth in [1, 3] {
        let served = turn(
            &h,
            conversation_request(KEY, SONNET, "s1", &fourth[..depth]),
        )
        .await;
        assert_eq!(served, "a");
    }

    let third = &fourth[..5];
    let served = turn(&h, conversation_request(KEY, SONNET, "s1", third)).await;
    assert_eq!(served, "b");
    let sent = up.requests();
    assert_eq!(sent.len(), 4, "a's 429 and b's answer both count");
    assert_eq!(
        member_of(&sent[2].body),
        Some(Some("msg_a2".into())),
        "a's leg names a's answer"
    );
    assert_eq!(
        member_of(&sent[3].body),
        Some(None),
        "b's leg asks again on b, which holds no answer and no memory"
    );
    settle(&h).await;
    let entries: Vec<phoebis_routing::AffinityEntry> = boundaries_of(third)
        .iter()
        .map(|boundary| entry_at(&h, SONNET, boundary).expect("every boundary has an entry"))
        .collect();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.account_id.as_str(), entry.message_id.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("b", None),
            ("b", None),
            ("b", None),
            ("b", None),
            ("b", Some("msg_b3")),
        ],
        "each of the third turn's five boundaries is b's, and the tail alone names b's answer"
    );

    let served = turn(&h, conversation_request(KEY, SONNET, "s1", &fourth)).await;
    assert_eq!(served, "b");
    assert_eq!(named(&up), Some(Some("msg_b3".into())));
    up.shutdown().await;
}

#[tokio::test]
async fn a_first_turn_whose_first_message_another_session_shares_names_nothing() {
    let up = FakeUpstream::start(vec![]).await;
    let h = answering(&up, &["msg_a1", "msg_b1"]).await;
    turn(&h, conversation_request(KEY, SONNET, "s1", &[user("u1")])).await;
    settle(&h).await;
    assert_eq!(
        id_at(&h, SONNET, &boundaries_of(&[user("u1")])[0]).as_deref(),
        Some("msg_a1")
    );

    turn(&h, conversation_request(KEY, SONNET, "s2", &[user("u1")])).await;
    assert_eq!(
        named(&up),
        Some(None),
        "the entry belongs to s1, and a shared first message is no fork"
    );
    up.shutdown().await;
}

/// The first-party fixture as a client that names `previous` in its own member signs it: the fixture's member cut,
/// one naming `previous` put back before `stream` where the client sends it, and the hash signed over the result.
fn first_party_naming(previous: Option<&str>) -> Vec<u8> {
    let mut native = first_party_without_diagnostics();
    let (stream, _) =
        phoebis_cch::object_member_span(&native, &["stream"]).expect("the fixture streams");
    let id = previous.map_or_else(|| "null".to_string(), |id| format!(r#""{id}""#));
    native.splice(
        stream..stream,
        format!(r#""diagnostics":{{"previous_message_id":{id}}},"#).bytes(),
    );
    phoebis_cch::sign(&native).expect("the fixture signs")
}

/// `native` on the first-party head under `session`: a client that sends the member sends the diagnosis beta too.
fn first_party_request(native: &[u8], session: &str) -> Request<Body> {
    let mut req = request_from_fixture(inference_head(FixtureKind::OauthFirstParty), native, KEY);
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// Whether the hash on `body`'s billing line covers `body` as it stands: `sign` rewrites the five digits alone, so
/// equal digits are an equal body.
fn signed_over_itself(body: &[u8]) -> bool {
    cch_digits(&phoebis_cch::sign(body).unwrap()) == cch_digits(body)
}

/// An OAuth 429 whose `Retry-After` is the only deadline it names, short enough to be waited out in place.
fn json_429_retry_after(seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), seconds.into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_vec(),
    }
}

/// `turn` with the completion the relay published for it on `sub`.
async fn completed_turn(
    h: &Harness,
    sub: &mut Subscriber,
    req: Request<Body>,
) -> (String, RequestComplete) {
    let served = turn(h, req).await;
    let complete = drain_until_complete(sub)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .expect("the request completed");
    (served, complete)
}

/// The decision the router recorded for `request_id`, read off its events under a five-second bound.
async fn decision_of(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> DecisionRecord {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the router's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision { record, .. } = event {
            if record.request.request_id == request_id {
                return std::sync::Arc::unwrap_or_clone(record);
            }
        }
    }
}

/// A client's member names the message its previous request was answered with, which only the account that
/// answered it can find: the leg a failover sends to another account names none, signed over the null, and the
/// member stays the client's on every completion.
#[tokio::test]
async fn a_client_member_is_detached_on_the_leg_that_fails_over_and_the_hash_re_signed() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a01"));
    up.push_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b02"));
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
        bus,
    )
    .await;
    let naming_a = first_party_naming(Some("msg_a01"));

    let (served, complete) =
        completed_turn(&h, &mut sub, first_party_request(&naming_a, "s1")).await;
    assert_eq!(served, "a");
    assert_eq!(
        named(&up),
        Some(Some("msg_a01".into())),
        "the session's first request passes the client's member as sent"
    );
    assert_eq!(complete.diagnostics, Some(DiagnosticsSource::Client));

    let (served, complete) =
        completed_turn(&h, &mut sub, first_party_request(&naming_a, "s1")).await;
    assert_eq!(served, "b");
    assert_eq!(credentials_seen(&up), [A, A, B], "a's 429 and b's 200");
    let sent = &up.requests()[2].body;
    assert_eq!(
        member_of(sent),
        Some(None),
        "the leg to b names no message of a's"
    );
    assert!(
        signed_over_itself(sent),
        "the hash is re-signed over the null"
    );
    assert_eq!(complete.diagnostics, Some(DiagnosticsSource::Client));

    let (served, complete) = completed_turn(
        &h,
        &mut sub,
        first_party_request(&first_party_naming(Some("msg_b01")), "s1"),
    )
    .await;
    assert_eq!(served, "b");
    assert_eq!(
        named(&up),
        Some(Some("msg_b01".into())),
        "the next turn on b names b's answer as sent"
    );
    assert_eq!(complete.diagnostics, Some(DiagnosticsSource::Client));
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_member_is_detached_on_the_first_turn_after_a_spent_move() {
    let up = FakeUpstream::start(vec![]).await;
    // Ten hours past NOW in Unix seconds, so the spent rule moves the pin rather than waiting for the reset.
    let reset = NOW / 1_000 + 10 * 3_600;
    up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-5h-utilization".into(),
                    "1.0".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset".into(),
                    reset.to_string(),
                ),
            ],
            body: br#"{"id":"msg_a01","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    );
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b02"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(
        turn(&h, first_party_request(&first_party_naming(None), "s1")).await,
        "a"
    );
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .samples()
            .await
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == "a")
            .filter(|(_, sample)| {
                sample
                    .reading(FIVE_HOUR)
                    .is_some_and(|r| r.utilization >= 1.0)
            })
    })
    .await;
    // A poll reads b's allowance, which the spent rule has to know before it moves the pin there.
    let reading = |utilization: f64, reset: i64| WindowReading {
        utilization,
        reset: Some(reset),
        observed_at: NOW,
        source: WindowSource::Poll,
        plan: None,
        tier: None,
    };
    h.router
        .report(Report::Sample {
            account_id: "b".into(),
            sample: UtilizationSample::default()
                .with(FIVE_HOUR, reading(0.3, NOW + 3 * 3_600_000))
                .with(SEVEN_DAY, reading(0.1, NOW + 72 * 3_600_000)),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    let mut events = h.router.subscribe_events();
    let naming_a = first_party_naming(Some("msg_a01"));
    let resp = h
        .app
        .clone()
        .oneshot(first_party_request(&naming_a, "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 1)
    );
    resp.into_body().collect().await.unwrap();
    let record = decision_of(&mut events, &o.request_id).await;
    assert_eq!(
        (record.outcome.kind, record.outcome.rule),
        ("moved", Some("spent")),
        "the selection moves the pin off a's spent window"
    );
    let sent = &up.requests()[1].body;
    assert_eq!(
        member_of(sent),
        Some(None),
        "the first turn on b names no message of a's"
    );
    assert!(
        signed_over_itself(sent),
        "the hash is re-signed over the null"
    );

    assert_eq!(
        turn(
            &h,
            first_party_request(&first_party_naming(Some("msg_b01")), "s1")
        )
        .await,
        "b"
    );
    assert_eq!(named(&up), Some(Some("msg_b01".into())));
    up.shutdown().await;
}

/// A client that sent the null it sends on a conversation's first message names nothing to detach, so its body and
/// its hash go as sent. API key accounts, whose blank finds the uuid already empty, so it moves no byte either.
#[tokio::test]
async fn a_client_member_already_null_is_left_as_sent() {
    const A_SECRET: &str = "sk-ant-api03-AAAA";
    const B_SECRET: &str = "sk-ant-api03-BBBB";
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A_SECRET, "/v1/messages", ok_with_id("msg_a01"));
    up.push_credential_rule(
        A_SECRET,
        "/v1/messages",
        json_429_unified(RESET, "five_hour"),
    );
    up.push_credential_rule(B_SECRET, "/v1/messages", ok_with_id("msg_b01"));
    let h = harness(
        &up,
        &[api_key("a", A_SECRET, 0), api_key("b", B_SECRET, 1)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let null = first_party_naming(None);
    assert_eq!(turn(&h, first_party_request(&null, "s1")).await, "a");
    assert_eq!(turn(&h, first_party_request(&null, "s1")).await, "b");
    assert_eq!(credentials_seen(&up), [A_SECRET, A_SECRET, B_SECRET]);
    let sent = &up.requests()[2].body;
    assert_eq!(member_of(sent), Some(None));
    assert_eq!(
        cch_digits(sent),
        cch_digits(&null),
        "the hash is not re-signed"
    );
    assert_eq!(
        sent.as_slice(),
        null.as_slice(),
        "the body goes byte for byte as sent"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_400_naming_a_detached_client_member_is_forwarded_unchanged() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a01"));
    up.push_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.push_credential_rule(B, "/v1/messages", json_400(REFUSED_DIAGNOSTICS));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
    )
    .await;
    let naming_a = first_party_naming(Some("msg_a01"));
    assert_eq!(turn(&h, first_party_request(&naming_a, "s1")).await, "a");
    let resp = h
        .app
        .clone()
        .oneshot(first_party_request(&naming_a, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(outcome(&resp).account_id, "b");
    let answer = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        answer.as_ref(),
        REFUSED_DIAGNOSTICS.as_bytes(),
        "the upstream's 400 reaches the client as it came"
    );
    assert_eq!(
        up.requests().len(),
        3,
        "a client member is never stripped and retried, detached or not"
    );

    // Nothing was latched: the next request, naming no member of its own, still gets the router's.
    assert_eq!(
        turn(&h, request_for(KEY, "claude-sonnet-4-5", "s2")).await,
        "b"
    );
    assert!(has_diagnostics(&up.requests()[3].body));
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_in_place_after_a_move_detaches_nothing_more() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a01"));
    up.push_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.push_credential_rule(B, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let naming_a = first_party_naming(Some("msg_a01"));
    assert_eq!(turn(&h, first_party_request(&naming_a, "s1")).await, "a");
    // The failover's leg to b meets a one-second throttle and waits it out on b.
    let resp = h
        .app
        .clone()
        .oneshot(first_party_request(&naming_a, "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 3)
    );
    resp.into_body().collect().await.unwrap();
    assert_eq!(credentials_seen(&up), [A, A, B, B]);
    let sent = up.requests();
    assert_eq!(
        member_of(&sent[3].body),
        member_of(&sent[2].body),
        "the retry resends the bytes the leg built"
    );
    up.shutdown().await;
}

/// The purge dropped the session's row and nothing landed it since, so the routing holds no previous landing and
/// the client's member goes as sent wherever the session is placed.
#[tokio::test]
async fn a_placement_with_no_previous_landing_passes_a_client_member_as_sent() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a01"));
    up.push_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b00"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    let h = harness_with_router_config(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            affinity_ttl_seconds: 60,
            ..Default::default()
        },
        RouterConfig {
            purge_interval: Duration::from_millis(10),
            ..RouterConfig::default()
        },
    )
    .await;
    let naming_a = first_party_naming(Some("msg_a01"));
    assert_eq!(turn(&h, first_party_request(&naming_a, "s1")).await, "a");
    h.clock.store(NOW + 120_000, Ordering::SeqCst);
    wait_for_async(Duration::from_secs(5), || async {
        h.router.pins().await.unwrap().is_empty().then_some(())
    })
    .await;
    // Another session's 429 cools a, so s1's next request is placed on b.
    assert_eq!(
        turn(&h, request_for(KEY, "claude-sonnet-4-5", "s2")).await,
        "b"
    );
    assert_eq!(turn(&h, first_party_request(&naming_a, "s1")).await, "b");
    assert_eq!(
        named(&up),
        Some(Some("msg_a01".into())),
        "with no previous landing the member goes as the client sent it"
    );
    up.shutdown().await;
}

/// The session's own request met no 429 and dispatched no earlier leg: another session's 429 cooled `a`, and the
/// selection released the pin onto `b`.
#[tokio::test]
async fn a_client_member_is_detached_when_a_cooldown_releases_the_pin_onto_a_sibling() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a01"));
    up.push_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b00"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b01"));
    up.push_credential_rule(B, "/v1/messages", ok_with_id("msg_b02"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(
        turn(&h, first_party_request(&first_party_naming(None), "s1")).await,
        "a"
    );
    assert_eq!(
        turn(&h, request_for(KEY, "claude-sonnet-4-5", "s2")).await,
        "b"
    );
    let mut events = h.router.subscribe_events();
    let naming_a = first_party_naming(Some("msg_a01"));
    let resp = h
        .app
        .clone()
        .oneshot(first_party_request(&naming_a, "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 1)
    );
    resp.into_body().collect().await.unwrap();
    let record = decision_of(&mut events, &o.request_id).await;
    assert!(
        record
            .pin
            .as_ref()
            .is_some_and(|pin| pin.released.contains(&"cooldown")),
        "the selection released the pin for the cooldown: {:?}",
        record.pin
    );
    let sent = &up.requests()[3].body;
    assert_eq!(
        member_of(sent),
        Some(None),
        "the first turn on b names no message of a's"
    );
    assert!(
        signed_over_itself(sent),
        "the hash is re-signed over the null"
    );

    assert_eq!(
        turn(
            &h,
            first_party_request(&first_party_naming(Some("msg_b01")), "s1")
        )
        .await,
        "b"
    );
    assert_eq!(named(&up), Some(Some("msg_b01".into())));
    up.shutdown().await;
}

/// What the client writes as the whole first user message of a resumed conversation whose opening tool results it
/// dropped. It writes it after it stamped the request, over a message that then held those results alone.
const RESUME_PLACEHOLDER: &str = "[Orphaned tool result removed due to conversation resume]";

/// The drift warning's own words, which pick its line out of the capture.
const DRIFT_WARNING: &str = "a caller stamped a billing line this build cannot reproduce";

/// The first-party fixture stamped `cc_version=2.1.310.{suffix}`, its one user message replaced by the resume
/// placeholder where `resumed`, and re-signed so the hash check passes and the suffix check decides. The fixture's
/// own `077` is the empty prompt's suffix under 2.1.310.
fn stamped(suffix: &str, resumed: bool) -> Vec<u8> {
    let mut text = String::from_utf8(body(BodyFixture::FirstParty).to_vec())
        .unwrap()
        .replacen(
            "cc_version=2.1.310.077",
            &format!("cc_version=2.1.310.{suffix}"),
            1,
        );
    if resumed {
        let open = r#""messages":["#;
        let start = text.find(open).expect("the fixture carries messages") + open.len();
        let end = text
            .find(r#"],"system":"#)
            .expect("the system blocks follow the messages");
        text.replace_range(
            start..end,
            &format!(
                r#"{{"role":"user","content":[{{"type":"text","text":"{RESUME_PLACEHOLDER}"}}]}}"#
            ),
        );
    }
    phoebis_cch::sign(text.as_bytes()).expect("the fixture signs")
}

/// Sends `native`, asserts its `403` `client_drift` refusal of `kind` before any account is tried, with the
/// decision's reason and nothing dialled, and answers the client's message with the drift warning's line.
async fn drift_refusal(native: &[u8], kind: &str) -> (String, String) {
    let captured = Captured::default();
    let _log = install(&captured);
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(first_party_request(native, "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status(),
            resp.headers()
                .get("x-phoebis-error-code")
                .and_then(|v| v.to_str().ok())
        ),
        (StatusCode::FORBIDDEN, Some("client_drift"))
    );
    let o = outcome(&resp);
    assert_eq!(
        (o.account_id.as_str(), o.attempt, o.client_drift.as_deref()),
        ("", 0, Some(kind))
    );
    let refusal: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let record = decision_of(&mut events, &o.request_id).await;
    assert_eq!(
        record
            .outcome
            .refusal
            .as_ref()
            .map(|r| (r.kind, r.code, r.reason.as_str())),
        Some(("client", "drift", kind))
    );
    assert!(
        up.requests().is_empty(),
        "a drifted body is not sent upstream in any form"
    );
    up.shutdown().await;
    let log = captured.text();
    let warnings: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(DRIFT_WARNING))
        .collect();
    assert_eq!(warnings.len(), 1, "one drift warning: {log}");
    (
        refusal["error"]["message"].as_str().unwrap().to_string(),
        warnings[0].to_string(),
    )
}

/// The client hashed the empty prompt for a resumed request, since the message it stamped held tool results
/// alone, so the router reproduces the stamp and the request relays.
#[tokio::test]
async fn a_resumed_conversation_stamped_over_the_empty_prompt_relays() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_with_id("msg_01"),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(first_party_request(&stamped("077", true), "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(outcome(&resp).client_drift, None);
    resp.into_body().collect().await.unwrap();
    assert_eq!(up.requests().len(), 1, "the body reached the upstream");
    up.shutdown().await;
}

/// The router reproduces a resumed stamp over the empty prompt, so a resumed suffix it cannot reproduce is a real
/// non-reproduction: refused, with words and a warning that name the resume beside the re-pin.
#[tokio::test]
async fn a_resumed_conversations_suffix_that_reproduces_from_no_candidate_is_refused_naming_the_resume(
) {
    let (message, warning) = drift_refusal(&stamped("abc", true), "version_suffix").await;
    assert_eq!(
        message,
        "the conversation was resumed, and the client's version suffix does not reproduce from the resume \
         placeholder, the empty prompt or the user agent. Re-pin the router to this client version"
    );
    assert!(warning.contains(" resume=true"), "{warning}");
}

/// A conversation that was not resumed keeps the words of a version change, and its warning says so.
#[tokio::test]
async fn a_fresh_conversations_suffix_that_does_not_reproduce_keeps_the_version_change_words() {
    let (message, warning) = drift_refusal(&stamped("abc", false), "version_suffix").await;
    assert_eq!(
        message,
        "the client's version suffix does not reproduce from the prompt and the user agent. Re-pin the router to \
         this client version"
    );
    assert!(warning.contains(" resume=false"), "{warning}");
}

/// The hash check reads no prompt, so its words stay a version change's, and its warning still marks the resume
/// for the operator reading the journal.
#[tokio::test]
async fn a_resumed_conversations_drifted_hash_keeps_the_hash_words_and_warns_the_resume() {
    let mut native = stamped("077", true);
    let at = phoebis_cch::find_cch(&native)
        .expect("the line carries a hash")
        .digits_start;
    native[at] = if native[at] == b'0' { b'1' } else { b'0' };
    let (message, warning) = drift_refusal(&native, "hash").await;
    assert_eq!(
        message,
        "the client's billing hash does not reproduce under the Claude Code release this build is measured against. \
         Re-pin the router to this client version"
    );
    assert!(warning.contains(" resume=true"), "{warning}");
}
