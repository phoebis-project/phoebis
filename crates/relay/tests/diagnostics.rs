mod common;
use common::*;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{Limits, CACHE_DIAGNOSIS_BETA};
use fake_upstream::{FakeUpstream, Respond, Rule};
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

/// Asserts the shape `cache_diagnostics` forces on a native wire body: the beta ends the caller's list exactly
/// once, `diagnostics` precedes `stream`, `previous_message_id` matches `previous`, and the restamp is valid.
fn assert_diagnosed_turn(r: &fake_upstream::RecordedRequest, previous: Option<&str>) {
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
    assert_eq!(
        phoebis_cch::sign(&r.body).unwrap(),
        r.body,
        "the restamp covers the inserted member"
    );
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
            cache_diagnostics: Some(true),
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
    assert_diagnosed_turn(&up.requests()[0], None);

    // Turn 2 on a: previous is turn 1's message_start id.
    let r2 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    assert_diagnosed_turn(&up.requests()[1], Some("msg_a01"));

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
    assert_diagnosed_turn(&up.requests()[3], None);

    // Turn 4 on b: previous is b's own turn-3 id.
    let r4 = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(r4.status(), 200);
    assert_diagnosed_turn(&up.requests()[4], Some("msg_b01"));

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
            cache_diagnostics: Some(true),
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
    assert_diagnosed_turn(&up.requests()[0], None);

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
async fn the_option_is_inherited_overridden_and_off_by_default() {
    // The pool leaves cache_diagnostics unset and the relay's own default is off: nothing is added.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness_with_cache_diagnostics_default(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        false,
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

    // The relay's own default is on and the pool leaves it unset: it inherits and is added.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness_with_cache_diagnostics_default(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        true,
    )
    .await;
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let sent = &up.requests()[0];
    assert!(beta_header(sent)
        .unwrap()
        .split(',')
        .any(|t| t == CACHE_DIAGNOSIS_BETA));
    assert!(has_diagnostics(&sent.body));
    up.shutdown().await;

    // The pool overrides the relay's own true default with an explicit false: nothing is added.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness_with_cache_diagnostics_default(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec {
            cache_diagnostics: Some(false),
            ..Default::default()
        },
        true,
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
            cache_diagnostics: Some(true),
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
    h.store.update_account("acct", 2, "acct", "test").unwrap();
    h.store.update_account("acct", 3, "acct", "test").unwrap();
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
        .apply_catalog_sync(
            "acct",
            &[
                ("claude-sonnet-4-5".to_string(), false),
                ("claude-opus-5".to_string(), false),
            ],
            None,
            NOW,
            "test",
        )
        .unwrap();
    h.store
        .set_pool_members("main", &[("acct".to_string(), 0)], "test")
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
async fn a_400_naming_a_beta_the_client_sent_is_forwarded_unchanged() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_BETA));
    let h = harness(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: Some(true),
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
            cache_diagnostics: Some(true),
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
            cache_diagnostics: Some(true),
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
async fn a_strip_retry_spends_no_slot_of_the_budget_so_a_one_attempt_pool_still_strips() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_400(REFUSED_BETA));
    up.push_credential_rule(ACCT_SECRET, "/v1/messages", json_200());
    let limits = Limits {
        max_attempts: 1,
        ..default_limits()
    };
    let h = harness_with(
        &up,
        &[api_key("acct", ACCT_SECRET, 0)],
        PoolSpec {
            cache_diagnostics: Some(true),
            ..Default::default()
        },
        &["claude-sonnet-4-5", "claude-opus-5"],
        limits,
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
        "the one budget slot went to the 400; the strip retry cost none"
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

    // Two accounts, two slots: a's 429 spends the first and b's 400 the second, and the strip on b still runs.
    let up2 = FakeUpstream::start(vec![]).await;
    up2.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up2.push_credential_rule(B, "/v1/messages", json_400(REFUSED_BETA));
    up2.push_credential_rule(B, "/v1/messages", json_200());
    let limits2 = Limits {
        max_attempts: 2,
        ..default_limits()
    };
    let h2 = harness_with(
        &up2,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            cache_diagnostics: Some(true),
            ..Default::default()
        },
        &["claude-sonnet-4-5", "claude-opus-5"],
        limits2,
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
        "a's 429, b's 400, and b's corrected retry: two slots, three submissions"
    );
    assert_eq!(outcome(&resp3).account_id, "b");
    assert!(!beta_header(&up2.requests()[2])
        .is_some_and(|b| b.split(',').any(|t| t == CACHE_DIAGNOSIS_BETA)));
    up2.shutdown().await;
}

#[tokio::test]
async fn count_tokens_gets_neither_the_member_nor_the_beta_under_the_option() {
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
            cache_diagnostics: Some(true),
            ..Default::default()
        },
    )
    .await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("Authorization", "Bearer phoebis-testkey")
        .header("x-claude-code-session-id", "s1")
        .body(Body::from(
            r#"{"model":"claude-sonnet-4-5[1m]","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = &up.requests()[0];
    assert!(!has_diagnostics(&sent.body));
    assert!(beta_header(sent).is_none());

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
            cache_diagnostics: Some(true),
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
    assert_diagnosed_turn(&up.requests()[0], None);

    // The probe replays the turn's own headers, session id among them.
    let probe = || phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
        allow_oauth: false,
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
    let key = phoebis_relay::MemoryKey {
        key_id: "desktop".into(),
        session_id: "s1".into(),
        agent_id: "main".into(),
    };
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
    assert_diagnosed_turn(&up.requests()[3], Some("msg_turn"));
    up.shutdown().await;
}
