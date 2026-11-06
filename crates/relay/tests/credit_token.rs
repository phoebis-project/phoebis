//! A refusal's fallback credit token is spent on the account that issued it: the relay remembers the issuer under the
//! token's digest the moment it reads the refusal, and the selection places the next body carrying the token there.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, RecordedRequest, Respond, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_routing::{DecisionRecord, FactValue, RouterEvent};
use phoebis_store::{Failover, NewBundle, Secret};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-CREDITAA";
const B: &str = "sk-ant-oat01-CREDITBB";
const MESSAGES: &str = "/v1/messages";
const TOKEN: &str = "cred-abc";
/// Past the `retry-after: 1` cooldown and its margin, well inside the hour an entry stands.
const PAST_THE_COOLDOWN_MS: i64 = 2_000;

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

const MESSAGE_START: &str =
    r#"{"type":"message_start","message":{"id":"msg_refused","usage":{"input_tokens":1}}}"#;
const MESSAGE_STOP: &str = r#"{"type":"message_stop"}"#;

/// The refusal's `message_delta`, its details carrying the fallback credit token `token`.
fn refusal_delta(token: &str) -> String {
    serde_json::json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": "refusal",
            "stop_sequence": null,
            "stop_details": {
                "type": "refusal",
                "category": "harmful_request",
                "fallback_credit_token": token,
            },
        },
        "usage": {"output_tokens": 0},
    })
    .to_string()
}

/// A refused stream whose upstream body ends after its last event.
fn refusal_stream(token: &str) -> Respond {
    Respond::Sse {
        status: 200,
        headers: vec![],
        events: vec![
            ev("message_start", MESSAGE_START),
            ev("message_delta", &refusal_delta(token)),
            ev("message_stop", MESSAGE_STOP),
        ],
        delay_between: Duration::ZERO,
    }
}

/// A refused stream the fake holds open past `message_stop` until the case releases it. The gate holds after the
/// first chunk, so the one event's data carries the next two events' lines and the chunk renders all three, as an
/// upstream that coalesces events in one chunk sends them, leaving the chunked terminator alone behind the gate.
fn refusal_held_open(token: &str) -> Respond {
    let data = format!(
        "{MESSAGE_START}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {MESSAGE_STOP}",
        refusal_delta(token)
    );
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![ev("message_start", &data)],
    }
}

/// A refusal answered whole as one JSON document, its details carrying the fallback credit token `token`.
fn refusal_answer(token: &str) -> Respond {
    let answer = serde_json::json!({
        "id": "msg_refused",
        "type": "message",
        "role": "assistant",
        "content": [],
        "stop_reason": "refusal",
        "stop_sequence": null,
        "stop_details": {
            "type": "refusal",
            "category": "harmful_request",
            "fallback_credit_token": token,
        },
        "usage": {"input_tokens": 1, "output_tokens": 0},
    });
    Respond::Json {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&answer).expect("a Value always encodes"),
    }
}

/// A rate limit `a` answers once, which fails the first turn over to `b` and cools `a` for its one second.
fn rate_limited() -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), "1".into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}}"#
            .to_vec(),
    }
}

/// `a` and `b` under priority, `a` first: `a` answers its first request with the rate limit and every later one with
/// a plain answer, `b` answers its first with `first_on_b` and every later one with a plain answer.
async fn upstream(first_on_b: Respond) -> FakeUpstream {
    let up = FakeUpstream::start(Vec::new()).await;
    up.add_credential_rule(A, MESSAGES, json_200());
    up.push_credential_rule(A, MESSAGES, rate_limited());
    up.add_credential_rule(B, MESSAGES, json_200());
    up.push_credential_rule(B, MESSAGES, first_on_b);
    up
}

fn accounts() -> [TestAccount; 2] {
    [oauth("a", A, 0), oauth("b", B, 1)]
}

/// No retry in place, so `a`'s one-second rate limit fails the turn over rather than waiting it out on `a`.
fn pool() -> PoolSpec {
    PoolSpec {
        failover: Failover {
            retry_in_place_seconds: 0,
            ..Failover::default()
        },
        ..PoolSpec::default()
    }
}

/// The OAuth fixture's body under `session`, carrying `token` at its top level where one is given.
fn turn(session: &str, token: Option<&str>) -> Request<Body> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    if let Some(token) = token {
        value["fallback_credit_token"] = serde_json::Value::from(token);
    }
    let bytes = serde_json::to_vec(&value).expect("a Value always encodes");
    let mut req =
        request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), &bytes, KEY);
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// The account that served `req`, its body read to the end.
async fn served(h: &Harness, req: Request<Body>) -> (String, String) {
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let landed = outcome(&resp);
    tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap();
    (landed.account_id, landed.request_id)
}

fn last_message(up: &FakeUpstream) -> RecordedRequest {
    up.requests()
        .into_iter()
        .rfind(|r| r.line.starts_with("POST /v1/messages"))
        .expect("a message reached the upstream")
}

/// The decision the router recorded for `request_id`, under a five-second bound.
async fn decision(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> DecisionRecord {
    let seen = router_events_until(events, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == request_id)
    })
    .await;
    decisions_of(&seen, request_id)
        .pop()
        .expect("the request's decision")
        .1
}

/// The `credit` fact of the walk's `pin` line, with the line's verdict.
fn credit_of(record: &DecisionRecord) -> (&str, Option<&FactValue>) {
    let pin = record
        .walk
        .iter()
        .find(|w| w.step == "pin")
        .expect("every walk records its pin");
    (
        pin.verdict,
        pin.facts
            .iter()
            .find(|f| f.key == "credit")
            .map(|f| &f.value),
    )
}

/// `account`'s `standing` fact on the walk's `eligible` line.
fn standing_of<'a>(record: &'a DecisionRecord, account: &str) -> Option<&'a FactValue> {
    record
        .walk
        .iter()
        .find(|w| w.step == "eligible")
        .expect("every walk records its eligible set")
        .facts
        .iter()
        .find(|f| f.key == "standing" && f.account_id.as_deref() == Some(account))
        .map(|f| &f.value)
}

fn step_past_the_cooldown(h: &Harness) {
    h.clock.fetch_add(PAST_THE_COOLDOWN_MS, Ordering::SeqCst);
}

#[tokio::test]
async fn the_next_body_carrying_a_refusals_credit_token_lands_on_the_issuer_while_the_refusal_is_still_open(
) {
    let up = upstream(refusal_held_open(TOKEN)).await;
    let h = harness(&up, &accounts(), pool()).await;
    let mut decisions = h.router.subscribe_events();
    let refused = h
        .app
        .clone()
        .oneshot(turn(&fresh_session(), None))
        .await
        .unwrap();
    assert_eq!(
        outcome(&refused).account_id,
        "b",
        "the rate limit failed the turn over to b"
    );
    let mut refusal = refused.into_body();
    let first = tokio::time::timeout(Duration::from_secs(5), refusal.frame())
        .await
        .expect("the refusal's events arrive")
        .unwrap()
        .unwrap()
        .into_data()
        .expect("a data frame");
    let text = String::from_utf8_lossy(&first).into_owned();
    assert!(
        text.contains("event: message_stop") && text.contains(TOKEN),
        "the client holds the whole refusal while the upstream body stays open: {text}"
    );
    step_past_the_cooldown(&h);

    let (account, request_id) = served(&h, turn(&fresh_session(), Some(TOKEN))).await;
    assert_eq!(
        account, "b",
        "the body carrying the token lands on its issuer, though a answers again and comes first"
    );
    let sent = last_message(&up);
    assert_eq!(sent.credential(), Some(B));
    let sent_body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        sent_body["fallback_credit_token"], TOKEN,
        "the token goes upstream at the body's top level as the client sent it"
    );
    assert_eq!(
        credit_of(&decision(&mut decisions, &request_id).await),
        ("match", Some(&FactValue::Text("b".into())))
    );

    let (account, request_id) = served(&h, turn(&fresh_session(), Some("cred-unknown"))).await;
    assert_eq!(
        account, "a",
        "a token the store does not hold places as the walk would"
    );
    assert_eq!(
        credit_of(&decision(&mut decisions, &request_id).await).1,
        None
    );

    up.release();
    tokio::time::timeout(Duration::from_secs(5), refusal.collect())
        .await
        .expect("the refusal's body ends once the gate opens")
        .unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn the_next_body_carrying_a_json_refusals_credit_token_lands_on_the_issuer() {
    let up = upstream(refusal_answer(TOKEN)).await;
    let h = harness(&up, &accounts(), pool()).await;
    let mut decisions = h.router.subscribe_events();
    assert_eq!(
        served(&h, turn(&fresh_session(), None)).await.0,
        "b",
        "the rate limit failed the turn over to b"
    );
    step_past_the_cooldown(&h);
    let (account, request_id) = served(&h, turn(&fresh_session(), Some(TOKEN))).await;
    assert_eq!(
        account, "b",
        "the whole answer's token lands the next body on its issuer, though a answers again and comes first"
    );
    assert_eq!(
        credit_of(&decision(&mut decisions, &request_id).await),
        ("match", Some(&FactValue::Text("b".into())))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_credit_token_places_as_today_once_its_issuer_is_created_again_under_its_id() {
    let up = upstream(refusal_stream(TOKEN)).await;
    let h = harness(&up, &accounts(), pool()).await;
    let session = fresh_session();
    assert_eq!(served(&h, turn(&session, None)).await.0, "b");
    step_past_the_cooldown(&h);
    assert_eq!(
        served(&h, turn(&fresh_session(), Some(TOKEN))).await.0,
        "b",
        "the credit applies while its issuer stands"
    );
    // A second refusal on b, where the session's pin holds the turn, records the token again.
    up.push_credential_rule(B, MESSAGES, refusal_stream(TOKEN));
    assert_eq!(served(&h, turn(&session, None)).await.0, "b");

    h.store.delete_account("b", "test").unwrap();
    h.store
        .create_account(&test_account_row(&oauth("b", B, 1)), None, NOW, "test")
        .unwrap();
    let row = h.store.account("b").unwrap().expect("b is there again");
    h.store
        .commit_credentials(
            "b",
            row.incarnation,
            row.lifecycle_generation,
            None,
            None,
            &NewBundle {
                access_token: Secret::new(B),
                refresh_token: Some(Secret::new(&format!("{B}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    // The delete took the membership with it.
    h.store
        .set_pool_members("main", &[member("a", 0), member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    assert_eq!(
        h.router.snapshot().account("b").map(|b| b.incarnation),
        Some(2)
    );
    let mut decisions = h.router.subscribe_events();
    let (account, request_id) = served(&h, turn(&fresh_session(), Some(TOKEN))).await;
    assert_eq!(
        account, "a",
        "the holder created again under the id is another account, which the credit does not reach"
    );
    let record = decision(&mut decisions, &request_id).await;
    assert_eq!(credit_of(&record).1, None);
    assert_eq!(
        standing_of(&record, "b"),
        Some(&FactValue::Text("eligible".into())),
        "the holder created again can serve, so its incarnation and not its standing kept the credit off it"
    );
    up.shutdown().await;
}

/// The bus events up to the `n`th completion, under a five-second bound.
async fn events_until_completions(sub: &mut phoebis_bus::Subscriber, n: usize) -> Vec<Event> {
    let give_up = std::time::Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        while let Ok(event) = sub.events.try_recv() {
            all.push(event);
        }
        let completions = all
            .iter()
            .filter(|e| matches!(e, Event::RequestComplete(_)))
            .count();
        if completions >= n {
            return all;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "{completions} of {n} completions were published"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `event` formatted with the two verbatim bodies left out: the relay's in-process copies of the client's and the
/// upstream's bytes, which carry the token as every other byte of them.
fn without_bodies(event: &Event) -> String {
    let mut event = event.clone();
    match &mut event {
        Event::RequestObserved(observed) => observed.body = Bytes::new(),
        Event::ResponseChunk(chunk) => chunk.body = Bytes::new(),
        _ => {}
    }
    format!("{event:?}")
}

/// The refused stream's completion carries the token's digest as `issued_digest` and the next body's observation as
/// `fallback_digest`, the completion's through the stream's merge of its events. The token itself reaches no router
/// event, bus event or record, and the digest no router event and no decision record.
#[tokio::test]
async fn the_credit_digest_rides_the_refusal_and_the_next_body_and_the_token_rides_nothing() {
    let up = upstream(refusal_stream(TOKEN)).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &accounts(), pool(), bus).await;
    let mut router_events = h.router.subscribe_events();
    let (refusing, refused_id) = served(&h, turn(&fresh_session(), None)).await;
    assert_eq!(refusing, "b");
    step_past_the_cooldown(&h);
    let (account, request_id) = served(&h, turn(&fresh_session(), Some(TOKEN))).await;
    assert_eq!(
        account, "b",
        "the store held the token, so the search covers a placement it made"
    );
    let bus_events = events_until_completions(&mut sub, 2).await;
    let mut seen = router_events_until(&mut router_events, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == request_id)
    })
    .await;
    // A read queued behind every report the requests made, so the events they emitted are all in the channel.
    h.router.pins().await.unwrap();
    while let Ok(event) = router_events.try_recv() {
        seen.push(event);
    }
    let digest: String = Sha256::digest(TOKEN.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let issued = bus_events.iter().find_map(|event| match event {
        Event::RequestComplete(complete) if complete.request_id == refused_id => {
            Some(complete.response.issued_digest.clone())
        }
        _ => None,
    });
    let presented = bus_events.iter().find_map(|event| match event {
        Event::RequestObserved(observed) if observed.request_id == request_id => {
            Some(observed.fallback_digest.clone())
        }
        _ => None,
    });
    assert_eq!(
        (issued, presented),
        (Some(Some(digest.clone())), Some(Some(digest.clone()))),
        "the refusal's completion and the next request's observation each carry the digest"
    );
    let mut router_side: Vec<(String, String)> = Vec::new();
    for event in &seen {
        if let RouterEvent::Decision { record, .. } = event {
            router_side.push((
                "decision record".into(),
                serde_json::to_string(&**record).unwrap(),
            ));
        }
        router_side.push(("router event".into(), format!("{event:?}")));
    }
    let mut ledger_side: Vec<(String, String)> = Vec::new();
    for event in &bus_events {
        ledger_side.push(("bus event".into(), without_bodies(event)));
    }
    let mut assembler = phoebis_ledger::Assembler::new(std::sync::Arc::new(|| 0));
    let mut requests = 0;
    for record in bus_events
        .iter()
        .cloned()
        .flat_map(|event| assembler.observe(event))
    {
        if let phoebis_ledger::Record::Request {
            request, attempts, ..
        } = record
        {
            requests += 1;
            ledger_side.push(("request record".into(), format!("{request:?}")));
            for attempt in attempts {
                ledger_side.push(("attempt record".into(), format!("{attempt:?}")));
            }
        }
    }
    assert_eq!(requests, 2, "both requests assembled their rows");
    for (what, text) in &router_side {
        for needle in [TOKEN, digest.as_str()] {
            assert!(!text.contains(needle), "a {what} carries {needle}: {text}");
        }
    }
    for (what, text) in &ledger_side {
        assert!(!text.contains(TOKEN), "a {what} carries the token: {text}");
    }
    up.shutdown().await;
}

/// The rate-limit tier the round case puts its account on, which its pricer keys on.
const ROUND_TIER: &str = "default_claude_max_20x";

/// An answer whose stop reason ends the turn.
fn end_turn_answer() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: br#"{"id":"msg_turn","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":1}}"#.to_vec(),
    }
}

/// The OAuth fixture's body under `session` and the turn class `class`, for the agent `agent` where one is named.
fn classed_turn(session: &str, class: &str, agent: Option<&str>) -> Request<Body> {
    let mut req = turn(session, None);
    set_header(&mut req, "x-claude-code-request-class", class);
    if let Some(agent) = agent {
        set_header(&mut req, "x-claude-code-agent-id", agent);
    }
    req
}

#[tokio::test]
async fn a_subagents_answer_that_ends_its_turn_leaves_its_pin_out_of_a_round_and_a_mains_does_not()
{
    use phoebis_routing::{
        burn_average_ms, Report, RouterConfig, SharePricer, UtilizationSample, WindowReading,
        WindowSource, FIVE_HOUR,
    };
    let up = FakeUpstream::start(Vec::new()).await;
    up.add_credential_rule(A, MESSAGES, end_turn_answer());
    // Every pin on the tier burns a tenth of the 5-hour window an hour, so each one the round walks is priced.
    let h = harness_with_router_config(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        RouterConfig {
            share: SharePricer::new(|_, _, _, _, _, _| None).with_rate(
                |window, _, tier, _, _, _| {
                    (window == FIVE_HOUR && tier == ROUND_TIER).then_some(0.1 / 3_600.0)
                },
            ),
            ..RouterConfig::default()
        },
    )
    .await;
    h.store
        .set_profile(
            "a",
            "{}",
            &phoebis_store::SubscriptionRow {
                tier: Some(ROUND_TIER.into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let pool = h.store.pool("main").unwrap().expect("the harness pool");
    // Two burn averages on, so the window's stamp below stands past the trigger's wait at the round.
    let at = NOW + 2 * burn_average_ms(&pool);
    h.clock.store(at, Ordering::SeqCst);
    // s2's main places the pin whose next request runs the round. In s1 the main turn and a subagent's each end
    // with `end_turn`.
    let s1 = fresh_session();
    let s2 = fresh_session();
    served(&h, classed_turn(&s2, "main", None)).await;
    served(&h, classed_turn(&s1, "main", None)).await;
    served(&h, classed_turn(&s1, "subagent", Some("sub"))).await;
    h.router
        .report(Report::Sample {
            account_id: "a".into(),
            sample: UtilizationSample::default().with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.9,
                    reset: None,
                    observed_at: at,
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    h.router.samples().await.unwrap();
    h.router
        .stamp_projection_over("main", "a", FIVE_HOUR, NOW, None)
        .await;
    let mut events = h.router.subscribe_events();
    served(&h, classed_turn(&s2, "main", None)).await;
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::SheddingRound { .. })
    })
    .await;
    let Some(RouterEvent::SheddingRound { round, .. }) = seen.last() else {
        unreachable!("the wait ends on the round");
    };
    let skip_of = |agent: &str| {
        round
            .pins
            .iter()
            .find(|p| p.session_id == s1 && p.agent_id == agent)
            .unwrap_or_else(|| panic!("s1's {agent} pin is on the account: {round:?}"))
            .skip
    };
    assert_eq!(
        skip_of("sub"),
        Some("ended"),
        "the subagent's end_turn ended its agent"
    );
    assert_ne!(
        skip_of("main"),
        Some("ended"),
        "a main turn's end_turn ends no agent"
    );
    up.shutdown().await;
}
