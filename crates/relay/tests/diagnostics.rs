mod common;
use common::*;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{MemoryKey, CACHE_DIAGNOSIS_BETA};
use phoebis_store::MemoryStore;
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

/// A non-streaming commit, so the message id lands in memory synchronously within the request the fixture's
/// own `stream: true` would otherwise defer to a background drain of the response body; the streamed path is
/// covered on its own in `restoration.rs`.
fn ok_with_id(id: &str) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: format!(
            r#"{{"id":"{id}","type":"message","content":[],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        )
        .into_bytes(),
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

/// The reduced billing line stripped so `cch`, `cc_prompt_id`, and the whole line's insertion are genuinely
/// restored rather than already present, so a strip test has something of its own to name.
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
    let mut native = body(BodyFixture::FirstParty9b244).to_vec();
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
async fn an_immediate_next_request_carries_the_id_the_last_answer_named() {
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
    // No flush in between: the next request has to find the id the hook already wrote, which the awaited
    // hand-off guarantees once the first answer's body is fully drained.
    let r2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
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
    h.relay.flush_memory().await;
    // The key's own generation is the store's insertion stamp, not the harness clock: `insert_api_key` stamps
    // it from the wall clock.
    let generation = h.store.api_key("desktop").unwrap().unwrap().created_at;
    let key = MemoryKey::for_session("desktop", generation, "s1", "main");
    // A spawned write can land after the flush unless the hand-off is awaited ahead of it; the drain above is
    // what makes the flush see it.
    let record = h
        .store
        .load(&key)
        .unwrap()
        .expect("the flush wrote the session's row");
    let payload: serde_json::Value = serde_json::from_str(&record.payload).unwrap();
    assert_eq!(
        payload["accounts"]["acct"]["message_ids"]["claude-sonnet-4-5"],
        "msg_01"
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
async fn the_switch_is_off_by_default_and_on_for_the_pool_that_sets_it() {
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
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let sent = &up.requests()[0];
    assert!(!beta_header(sent).is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    assert!(!has_diagnostics(&sent.body));
    up.shutdown().await;

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
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_diagnosed_turn(&up.requests()[0], None);
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
        body(BodyFixture::FirstParty9b244),
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
        .oneshot(without_billing_line("s1"))
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
    assert_eq!(
        resp.status(),
        200,
        "the 400 named the router's own beta, so the corrected body went to the same account"
    );
    assert_eq!(up.requests().len(), 2);
    assert!(!beta_header(&up.requests()[1])
        .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    assert_eq!(
        outcome(&resp).attempt,
        2,
        "the outcome counts wire submissions, which the ledger keys its attempt rows by"
    );

    // The latch still lands: the next request carries no beta, in one upstream request.
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let resp2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    assert_eq!(up.requests().len(), 3);
    assert!(!beta_header(&up.requests()[2])
        .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    up.shutdown().await;

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

#[tokio::test]
async fn a_probe_names_the_sessions_previous_message_id_and_its_own_answer_is_never_remembered() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_turn"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_probe_1"));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", ok_with_id("msg_probe_2"));
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
    let resp = h
        .relay
        .probe(probe(), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(resp.status(), 200);
    assert_diagnosed_turn(&up.requests()[1], Some("msg_turn"));

    // A second probe still names the turn: the first probe's own answer was not remembered.
    let resp = h
        .relay
        .probe(probe(), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(resp.status(), 200);
    assert_diagnosed_turn(&up.requests()[2], Some("msg_turn"));
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
        Some("msg_turn")
    );

    // The next real turn names the turn before it, not a probe.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_diagnosed_inference_turn(&up.requests()[3], Some("msg_turn"));
    up.shutdown().await;
}
