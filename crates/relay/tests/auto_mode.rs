//! The pool's and the key's server-side auto mode setting. On, the default, the client's `safeguards` member and
//! the `dangerous-tool-use-2026-09-03` beta travel as sent. Off, both leave every request of the key, a first-party
//! body is restamped, and a member the cut cannot place is refused before anything reaches the upstream.
mod common;
use common::*;

use phoebis_profile::fixtures::{
    body, inference_head, with_dangerous_tool_use_beta, with_safeguards, BodyFixture, FixtureKind,
    SAFEGUARDS_MEMBER,
};
use phoebis_relay::DANGEROUS_TOOL_USE_BETA;
use fake_upstream::{FakeUpstream, RecordedRequest, Respond, Rule};
use http_body_util::BodyExt;
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA0001";
const OA: &str = "sk-ant-oat01-AAAA0001";

const REFUSED_AUTO_MODE_BETA: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `dangerous-tool-use-2026-09-03` for the `anthropic-beta` header."}}"#;

fn messages_rule(respond: Respond) -> Rule {
    Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }
}

/// The one OAuth account every inference test runs on, with the uuid the recorded bodies are compared under.
fn account() -> TestAccount {
    TestAccount {
        uuid: Some(ACCOUNT_UUID),
        ..oauth("acct", OA, 0)
    }
}

/// The recorded first-party body with the member spliced in and re-signed, as the client in auto mode stamps it.
fn first_party_with_safeguards() -> Vec<u8> {
    phoebis_cch::sign(&with_safeguards(body(BodyFixture::FirstParty9b244))).unwrap()
}

/// The fixture head of `kind` with the beta the client sends beside the member.
fn head_with_beta(kind: FixtureKind) -> Vec<u8> {
    with_dangerous_tool_use_beta(inference_head(kind))
}

/// The `anthropic-beta` list a raw head carries, in its order.
fn caller_betas(head: &[u8]) -> Vec<String> {
    String::from_utf8(head.to_vec())
        .unwrap()
        .split("\r\n")
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, value)| value.split(',').map(|b| b.trim().to_string()).collect())
        })
        .unwrap_or_default()
}

/// The account's uuid spliced into the recorded identity slot, as the relay does before it signs.
fn with_uuid(native: &[u8]) -> Vec<u8> {
    String::from_utf8(native.to_vec())
        .unwrap()
        .replacen(
            r#"\"account_uuid\":\"\""#,
            &format!(r#"\"account_uuid\":\"{ACCOUNT_UUID}\""#),
            1,
        )
        .into_bytes()
}

fn top_level_keys(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("the body parses")
        .as_object()
        .expect("the body is an object")
        .keys()
        .cloned()
        .collect()
}

fn stamp(body: &[u8]) -> Vec<u8> {
    let at = body
        .windows(4)
        .position(|w| w == b"cch=")
        .expect("a stamped body")
        + 4;
    body[at..at + 5].to_vec()
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

struct Relayed {
    status: u16,
    code: String,
    answer: Vec<u8>,
    sent: Vec<RecordedRequest>,
}

/// One request through a fresh harness on `accounts` under `pool`, answered by `respond`.
async fn relay(
    accounts: &[TestAccount],
    pool: PoolSpec,
    respond: Respond,
    head: &[u8],
    sent_body: &[u8],
) -> Relayed {
    let up = FakeUpstream::start(vec![messages_rule(respond)]).await;
    let h = harness(&up, accounts, pool).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_from_fixture(head, sent_body, KEY))
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let code = resp
        .headers()
        .get("x-phoebis-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let answer = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    let sent = up.requests();
    up.shutdown().await;
    Relayed {
        status,
        code,
        answer,
        sent,
    }
}

fn key_off() -> PoolSpec {
    PoolSpec {
        key_auto_mode_server: Some(false),
        ..PoolSpec::default()
    }
}

fn pool_off() -> PoolSpec {
    PoolSpec {
        auto_mode_server: false,
        ..PoolSpec::default()
    }
}

fn ok_answer() -> Vec<u8> {
    match json_200() {
        Respond::Json { body, .. } => body,
        _ => unreachable!("json_200 is a JSON answer"),
    }
}

#[tokio::test]
async fn the_default_passes_the_member_and_the_beta_through_unchanged() {
    for (kind, sent_body) in [
        (FixtureKind::OauthFirstParty, first_party_with_safeguards()),
        (
            FixtureKind::OauthNonFirstParty,
            with_safeguards(body(BodyFixture::Oauth)),
        ),
    ] {
        let head = head_with_beta(kind);
        let r = relay(
            &[account()],
            PoolSpec::default(),
            json_200(),
            &head,
            &sent_body,
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.answer,
            ok_answer(),
            "the answer reaches the caller as sent"
        );
        let received = &r.sent[0].body;
        assert!(
            received
                .ends_with(&[b",", SAFEGUARDS_MEMBER.as_bytes(), br#","stream":true}"#].concat()),
            "the member travels verbatim where the client put it"
        );
        assert_eq!(beta_list(&r.sent[0]), caller_betas(&head));
        assert_eq!(
            beta_list(&r.sent[0]).last().map(String::as_str),
            Some(DANGEROUS_TOOL_USE_BETA),
            "the beta stays where the caller put it"
        );
        assert_eq!(&phoebis_cch::sign(received).unwrap(), received);
    }
}

#[tokio::test]
async fn off_on_the_key_drops_the_member_and_the_beta_and_restamps_a_first_party_body() {
    let head = head_with_beta(FixtureKind::OauthFirstParty);
    let sent_body = first_party_with_safeguards();
    let r = relay(&[account()], key_off(), json_200(), &head, &sent_body).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.answer,
        ok_answer(),
        "the answer reaches the caller as sent"
    );
    let received = &r.sent[0].body;

    let mut expected_keys = top_level_keys(&sent_body);
    expected_keys.retain(|k| k != "safeguards");
    assert_eq!(
        top_level_keys(received),
        expected_keys,
        "no safeguards member, every other member in the client's order"
    );
    assert_eq!(
        received.len() + SAFEGUARDS_MEMBER.len() + 1,
        with_uuid(&sent_body).len(),
        "the member left with one comma"
    );
    assert_eq!(&phoebis_cch::sign(received).unwrap(), received);
    assert_ne!(stamp(received), stamp(&sent_body), "the body was restamped");
    assert!(
        received == &phoebis_cch::sign(&with_uuid(body(BodyFixture::FirstParty9b244))).unwrap(),
        "the body is the one the recording without the member relays as"
    );

    let mut expected_betas = caller_betas(&head);
    expected_betas.retain(|b| b != DANGEROUS_TOOL_USE_BETA);
    assert_eq!(
        beta_list(&r.sent[0]),
        expected_betas,
        "the beta is gone and the others keep their order"
    );
}

#[tokio::test]
async fn off_on_the_pool_drops_the_pair_from_an_oauth_body_and_never_reports_the_drop() {
    let head = head_with_beta(FixtureKind::OauthNonFirstParty);
    let sent_body = with_safeguards(body(BodyFixture::Oauth));
    let r = relay(
        &[account()],
        pool_off(),
        Respond::Json {
            status: 400,
            headers: vec![],
            body: REFUSED_AUTO_MODE_BETA.as_bytes().to_vec(),
        },
        &head,
        &sent_body,
    )
    .await;
    assert_eq!(r.status, 400, "the upstream's 400 is the caller's");
    assert!(
        String::from_utf8_lossy(&r.answer).contains(DANGEROUS_TOOL_USE_BETA),
        "{}",
        String::from_utf8_lossy(&r.answer)
    );
    assert_eq!(
        r.sent.len(),
        1,
        "no strip retry: the drop is not the relay's own"
    );
    let received = &r.sent[0];
    assert!(!top_level_keys(&received.body).contains(&"safeguards".to_string()));
    assert_eq!(count(&received.body, b"\"safeguards\""), 0);
    assert!(!beta_list(received)
        .iter()
        .any(|b| b == DANGEROUS_TOOL_USE_BETA));
}

#[tokio::test]
async fn off_drops_the_beta_alone_from_a_request_without_the_member() {
    let head = head_with_beta(FixtureKind::OauthFirstParty);
    let native = body(BodyFixture::FirstParty9b244);
    let on = relay(&[account()], PoolSpec::default(), json_200(), &head, native).await;
    let off = relay(&[account()], key_off(), json_200(), &head, native).await;
    assert_eq!((on.status, off.status), (200, 200));
    assert!(
        off.sent[0].body == on.sent[0].body,
        "the body the default sends, byte for byte"
    );
    let mut expected_betas = beta_list(&on.sent[0]);
    assert!(expected_betas.iter().any(|b| b == DANGEROUS_TOOL_USE_BETA));
    expected_betas.retain(|b| b != DANGEROUS_TOOL_USE_BETA);
    assert_eq!(beta_list(&off.sent[0]), expected_betas);
}

/// A probe replays a recipe whose retained head keeps the beta the client sent, so the drop has to happen on the
/// probe's own pass through the relay, as it does for a request the client sends.
#[tokio::test]
async fn a_probe_under_an_off_key_loses_the_beta_as_an_inference_does() {
    let head = head_with_beta(FixtureKind::OauthNonFirstParty);
    let native = body(BodyFixture::Oauth);
    let mut sent = Vec::new();
    for pool in [PoolSpec::default(), key_off()] {
        let up = FakeUpstream::start(vec![messages_rule(json_200())]).await;
        let h = harness(&up, &[account()], pool).await;
        // The engine replays the recipe's head with no credential and no recorded head.
        let mut req = request_from_fixture(&head, native, KEY);
        remove_header(&mut req, "authorization");
        req.extensions_mut().remove::<phoebis_relay::OriginalHead>();
        let resp = h
            .relay
            .probe(
                phoebis_relay::Probe {
                    key_id: "desktop".into(),
                    key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
                    allow_oauth: true,
                },
                req,
            )
            .await;
        assert_eq!(resp.status(), 200);
        let requests = up.requests();
        assert_eq!(requests.len(), 1);
        sent.extend(requests);
        up.shutdown().await;
    }
    let (on, off) = (&sent[0], &sent[1]);
    assert!(
        off.body == on.body,
        "a body without the member is the one the default sends"
    );
    let mut expected_betas = caller_betas(&head);
    assert_eq!(beta_list(on), expected_betas, "the default keeps the beta");
    expected_betas.retain(|b| b != DANGEROUS_TOOL_USE_BETA);
    assert_eq!(
        beta_list(off),
        expected_betas,
        "the off key drops the beta alone"
    );
}

#[tokio::test]
async fn a_count_under_off_loses_the_beta_as_an_inference_does() {
    let head = head_with_beta(FixtureKind::CountApiKey);
    let native = body(BodyFixture::CountApiKey);
    let r = relay(
        &[api_key("acct", A, 0)],
        pool_off(),
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"input_tokens":7}"#.to_vec(),
        },
        &head,
        native,
    )
    .await;
    assert_eq!(r.status, 200);
    let sent = &r.sent[0];
    assert!(sent.line.starts_with("POST /v1/messages/count_tokens"));
    assert!(sent.body == native, "the count body as sent");
    let mut expected_betas = caller_betas(&head);
    expected_betas.retain(|b| b != DANGEROUS_TOOL_USE_BETA);
    assert_eq!(beta_list(sent), expected_betas);
}

#[tokio::test]
async fn a_count_carrying_the_member_under_an_off_key_reaches_the_upstream_as_the_native_count() {
    let head = head_with_beta(FixtureKind::CountApiKey);
    let native = body(BodyFixture::CountApiKey);
    let sent_body = with_safeguards(native);
    assert_eq!(count(&sent_body, br#""safeguards":["#), 1);
    let r = relay(
        &[api_key("acct", A, 0)],
        key_off(),
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"input_tokens":7}"#.to_vec(),
        },
        &head,
        &sent_body,
    )
    .await;
    assert_eq!(r.status, 200);
    let sent = &r.sent[0];
    assert!(sent.line.starts_with("POST /v1/messages/count_tokens"));
    assert!(
        sent.body == native,
        "the member left with one comma, so the count body is the native one"
    );
    let mut expected_betas = caller_betas(&head);
    expected_betas.retain(|b| b != DANGEROUS_TOOL_USE_BETA);
    assert_eq!(beta_list(sent), expected_betas);
}

#[tokio::test]
async fn off_refuses_a_body_whose_member_the_walk_cannot_place() {
    let head = head_with_beta(FixtureKind::OauthNonFirstParty);
    let spliced = with_safeguards(body(BodyFixture::Oauth));
    let escaped = String::from_utf8(spliced.clone())
        .unwrap()
        .replacen(r#""safeguards""#, r#""safeg\u0075ards""#, 1)
        .into_bytes();
    assert_eq!(
        count(&escaped, br#""safeg\u0075ards":["#),
        1,
        "the escape is in the raw bytes"
    );
    assert_eq!(count(&escaped, br#""safeguards":["#), 0);
    let twice = String::from_utf8(spliced)
        .unwrap()
        .replacen(r#""stream":"#, r#""safeguards":[],"stream":"#, 1)
        .into_bytes();
    assert_eq!(count(&twice, br#""safeguards":["#), 2);

    for sent_body in [&escaped, &twice] {
        let r = relay(&[account()], key_off(), json_200(), &head, sent_body).await;
        assert_eq!(r.status, 400);
        assert_eq!(r.code, "body_shape");
        assert!(
            String::from_utf8_lossy(&r.answer).contains("safeguards"),
            "the message names the member: {}",
            String::from_utf8_lossy(&r.answer)
        );
        assert!(r.sent.is_empty(), "nothing reached the upstream");
    }

    // The default is untouched: both travel with the member as the client wrote it.
    let r = relay(
        &[account()],
        PoolSpec::default(),
        json_200(),
        &head,
        &escaped,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(count(&r.sent[0].body, br#""safeg\u0075ards":["#), 1);
    let r = relay(&[account()], PoolSpec::default(), json_200(), &head, &twice).await;
    assert_eq!(r.status, 200);
    assert_eq!(count(&r.sent[0].body, br#""safeguards":["#), 2);
}
