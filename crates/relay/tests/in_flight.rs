//! The in-flight counts: an account ticket per wire attempt, a key permit per request, both held until the
//! response ends, and what a selection does when an account is at its cap.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_routing::{
    DecisionRecord, FactValue, HoldOutcome, HoldReason, Report, RouterConfig, RouterEvent,
    SharePricer, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
};
use phoebis_store::{CooldownScope, Failover, SlowDown, SubscriptionRow};
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const C: &str = "sk-ant-oat01-CCCC";
const MODEL: &str = "claude-sonnet-4-5";
/// How long a request that should be held is watched for not landing.
const HELD_FOR: Duration = Duration::from_millis(300);

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

fn message_start() -> SseEvent {
    ev(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#,
    )
}

fn message_stop() -> SseEvent {
    ev("message_stop", r#"{"type":"message_stop"}"#)
}

/// A committed stream the fake holds after its first event until the test releases it.
fn held_stream() -> Respond {
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![message_start(), message_stop()],
    }
}

fn messages(respond: Respond) -> Vec<Rule> {
    vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }]
}

fn account_cap(cap: u32, retry_in_place_seconds: u64) -> Failover {
    Failover {
        in_flight_cap: cap,
        retry_in_place_seconds,
        ..Failover::default()
    }
}

fn posts(up: &FakeUpstream) -> usize {
    credentials_seen(up).len()
}

fn in_flight(h: &Harness, account: &str) -> (u32, u32) {
    (
        h.router.in_flight().account(account),
        h.router.in_flight().key("desktop"),
    )
}

async fn drain(resp: axum::http::Response<axum::body::Body>) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// The sender a channel body is fed through; dropping it ends the body.
type BodySender = tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>;

/// A Messages request on the harness key whose body arrives through a channel the test holds, with
/// `content-length` set when `declared` names one. The channel holds one chunk at a time.
fn channel_request(declared: Option<usize>) -> (Request<Body>, BodySender) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut request = Request::post("/v1/messages")
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("user-agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", fresh_session())
        .header("x-claude-code-request-class", "main");
    if let Some(length) = declared {
        request = request.header("content-length", length);
    }
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    (request.body(body).unwrap(), tx)
}

/// A request whose body has started arriving and stays open: the relay has taken the first chunk when this
/// returns, since the channel holds one and the second send waits for room.
async fn reading(
    h: &Harness,
) -> (
    tokio::task::JoinHandle<axum::http::Response<Body>>,
    BodySender,
) {
    let (request, tx) = channel_request(None);
    let app = h.app.clone();
    let task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    for chunk in [&b"{\"model\":"[..], b" "] {
        tokio::time::timeout(
            Duration::from_secs(5),
            tx.send(Ok(Bytes::from_static(chunk))),
        )
        .await
        .expect("the relay takes the chunk")
        .expect("the relay reads the body rather than refusing it");
    }
    (task, tx)
}

/// What the bus carried for `request_id`: its observation and its completion.
async fn observed_and_completed(
    sub: &mut phoebis_bus::Subscriber,
    request_id: &str,
) -> (phoebis_bus::RequestObserved, phoebis_bus::RequestComplete) {
    let mut events = Vec::new();
    wait_for(Duration::from_secs(5), || {
        while let Ok(e) = sub.events.try_recv() {
            events.push(e);
        }
        let observed = events.iter().find_map(|e| match e {
            Event::RequestObserved(o) if o.request_id == request_id => Some(o.clone()),
            _ => None,
        })?;
        let complete = events.iter().find_map(|e| match e {
            Event::RequestComplete(c) if c.request_id == request_id => Some(c.clone()),
            _ => None,
        })?;
        Some((observed, complete))
    })
    .await
}

fn json_429_retry_after(seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), seconds.into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_vec(),
    }
}

#[tokio::test]
async fn a_third_request_on_an_account_at_a_cap_of_two_waits_for_a_slot_and_then_lands() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(2, 10),
            ..PoolSpec::default()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let second = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        (first.status().as_u16(), second.status().as_u16()),
        (200, 200)
    );
    assert_eq!(h.router.in_flight().account("a"), 2, "two streams open");
    let app = h.app.clone();
    let third =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s3")).await.unwrap() });
    stays_for(HELD_FOR, || posts(&up) == 2).await;
    assert!(!third.is_finished(), "the third selection is held");
    up.release_one().await;
    let third = tokio::time::timeout(Duration::from_secs(5), third)
        .await
        .expect("a freed slot ends the hold")
        .unwrap();
    assert_eq!(
        (third.status().as_u16(), outcome(&third).account_id.as_str()),
        (200, "a")
    );
    assert_eq!(posts(&up), 3);
    drop((first, second, third));
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_hold_followed_by_a_dispatch_names_the_hold_in_the_attempts_trace() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 10),
            ..PoolSpec::default()
        },
        bus,
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(h.router.in_flight().account("a"), 1, "a is at its cap");
    let app = h.app.clone();
    let held =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s2")).await.unwrap() });
    stays_for(HELD_FOR, || posts(&up) == 1).await;
    assert!(!held.is_finished(), "the second selection is held");
    up.release_one().await;
    let held = tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("a freed slot ends the hold")
        .unwrap();
    assert_eq!(held.status().as_u16(), 200);
    let held_id = outcome(&held).request_id;
    let mut events = Vec::new();
    let selected = wait_for(Duration::from_secs(5), || {
        while let Ok(e) = sub.events.try_recv() {
            events.push(e);
        }
        events.iter().find_map(|e| match e {
            Event::AccountSelected(a) if a.request_id == held_id => Some(a.clone()),
            _ => None,
        })
    })
    .await;
    let selection: serde_json::Value =
        serde_json::from_str(&selected.selection.expect("a selection was made")).unwrap();
    assert_eq!(
        selection["cohort_hold"]["waited_on"],
        serde_json::json!(["a"]),
        "the attempt that follows the hold names the account it waited on: {selection}"
    );
    let held_ms = selection["cohort_hold"]["held_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("the trace names the wait: {selection}"));
    assert!(
        held_ms > 0 && held_ms <= 10_000,
        "the wait lasted until the release, within the bound: {held_ms}"
    );
    assert_eq!(
        serde_json::to_value(outcome(&held).held).unwrap(),
        selection["cohort_hold"],
        "the request's own summary is the attempt's entry"
    );
    drop(first);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_hold_and_the_placement_after_it_are_the_request_s_first_and_second_decision() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 10),
            ..PoolSpec::default()
        },
    )
    .await;
    let mut events = h.router.subscribe_events();
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let app = h.app.clone();
    let held =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s2")).await.unwrap() });
    let mut seen = router_events_until(
        &mut events,
        |event| matches!(event, RouterEvent::Decision { summary, .. } if summary.kind == "held"),
    )
    .await;
    up.release_one().await;
    let held = tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("a freed slot ends the hold")
        .unwrap();
    assert_eq!(held.status().as_u16(), 200);
    // The placement's decision went out before its answer reached the relay.
    seen.extend(std::iter::from_fn(|| events.try_recv().ok()));
    let facts = |request_id: &str| {
        decisions_of(&seen, request_id)
            .into_iter()
            .map(|(summary, record)| (record.request.seq, record.request.attempt, summary.kind))
            .collect::<Vec<_>>()
    };
    assert_eq!(facts(&outcome(&first).request_id), [(1, 1, "placed")]);
    assert_eq!(
        facts(&outcome(&held).request_id),
        [(1, 1, "held"), (2, 1, "placed")],
        "a hold and the placement after it are two decisions of one wire attempt"
    );
    drop(first);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_pinned_sessions_request_lands_on_its_account_at_the_cap_while_a_fresh_session_is_held() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 10),
            ..PoolSpec::default()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(h.router.in_flight().account("a"), 1, "a is at its cap");
    let pinned = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
    )
    .await
    .expect("the pinned session is never held")
    .unwrap();
    assert_eq!(
        (
            pinned.status().as_u16(),
            outcome(&pinned).account_id.as_str()
        ),
        (200, "a")
    );
    assert_eq!(
        h.router.in_flight().account("a"),
        2,
        "the cap steers new placements and never a pin"
    );
    let app = h.app.clone();
    let fresh =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s2")).await.unwrap() });
    stays_for(HELD_FOR, || posts(&up) == 2).await;
    assert!(!fresh.is_finished(), "a fresh session is held");
    up.release();
    drain(first).await;
    drain(pinned).await;
    let fresh = tokio::time::timeout(Duration::from_secs(5), fresh)
        .await
        .expect("the slots freed")
        .unwrap();
    assert_eq!(
        (fresh.status().as_u16(), outcome(&fresh).account_id.as_str()),
        (200, "a")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn past_the_bound_a_held_request_re_selects_with_the_account_attempted() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", held_stream());
    // b's slot frees when its client leaves during the hold, but a's never does: a held request waits on a alone.
    up.add_credential_rule(B, "/v1/messages", held_stream());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            failover: account_cap(1, 1),
            ..PoolSpec::default()
        },
    )
    .await;
    let on_a = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&on_a).account_id, "a");
    let on_b = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&on_b).account_id,
        "b",
        "a is at its cap, so a fresh placement goes to b"
    );
    let mut events = h.router.subscribe_events();
    // On the paused clock the one-second bound on a runs only while the test waits on it, so b's slot is free
    // well inside it whatever the machine's load. The clock runs again before b is dialed, since the fake's
    // answer comes over a real socket.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let app = h.app.clone();
    let third =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s3")).await.unwrap() });
    let id = next_held(&mut events).await.request.request_id;
    drop(on_b);
    wait_for(Duration::from_secs(5), || {
        (h.router.in_flight().account("b") == 0).then_some(())
    })
    .await;
    router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == id)
    })
    .await;
    tokio::time::resume();
    let third = tokio::time::timeout(Duration::from_secs(5), third)
        .await
        .expect("the bound ends the hold")
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "held for the one-second bound on a: {:?}",
        started.elapsed()
    );
    assert_eq!(
        (third.status().as_u16(), outcome(&third).account_id.as_str()),
        (200, "b"),
        "past the bound a is attempted and the re-selection takes b"
    );
    assert_eq!(credentials_seen(&up), [A, B, B]);
    assert_eq!(
        h.router.in_flight().account("a"),
        1,
        "a's stream is still open"
    );
    let held = outcome(&third)
        .held
        .expect("the outcome names the hold it waited through");
    assert_eq!(
        held.waited_on,
        ["a"],
        "the request waited on a alone, and b's slot freed inside the bound took it with no hold of its own"
    );
    drop((on_a, third));
    up.release();
    up.shutdown().await;
}

/// The completion the bus carried for `request_id`, read off `sub` as it arrives.
async fn completion_of(
    sub: &mut phoebis_bus::Subscriber,
    request_id: &str,
) -> phoebis_bus::RequestComplete {
    let mut events = Vec::new();
    wait_for(Duration::from_secs(5), || {
        while let Ok(e) = sub.events.try_recv() {
            events.push(e);
        }
        events.iter().find_map(|e| match e {
            Event::RequestComplete(c) if c.request_id == request_id => Some(c.clone()),
            _ => None,
        })
    })
    .await
}

#[tokio::test]
async fn a_request_whose_one_bound_ran_out_answers_at_cap_naming_its_one_wait_on_the_completion() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            failover: account_cap(1, 1),
            ..PoolSpec::default()
        },
        bus,
    )
    .await;
    let on_a = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let on_b = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            outcome(&on_a).account_id.as_str(),
            outcome(&on_b).account_id.as_str()
        ),
        ("a", "b"),
        "each account holds one stream at its cap"
    );
    tokio::time::pause();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s3"))
        .await
        .unwrap();
    tokio::time::resume();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "accounts_at_cap"),
        "a's hold ran the bound out and b was passed over"
    );
    assert_eq!(posts(&up), 2, "nothing was dialled for the held request");
    let held = outcome(&resp)
        .held
        .expect("the outcome names the holds it waited through");
    assert_eq!(
        held.waited_on,
        ["a"],
        "a's hold ran the bound out, so b is passed over with no wait of its own"
    );
    assert!(
        (900..=2_000).contains(&held.held_ms),
        "the one wait ran the one-second bound out and no second interval followed it: {}",
        held.held_ms
    );
    let complete = completion_of(&mut sub, &outcome(&resp).request_id).await;
    assert_eq!(
        complete
            .held
            .as_deref()
            .map(|h| serde_json::from_str::<serde_json::Value>(h).unwrap()),
        Some(serde_json::to_value(&held).unwrap()),
        "the completion carries the one wait"
    );
    assert_eq!(
        (
            complete.outcome,
            complete.status,
            complete.error_code.as_deref()
        ),
        (
            phoebis_bus::Outcome::Exhausted,
            429,
            Some("accounts_at_cap")
        ),
        "a pool at its cap is an exhausted pool to the ledger and the attention gauge"
    );
    let body: serde_json::Value = serde_json::from_slice(&drain(resp).await).unwrap();
    assert_eq!(
        (&body["error"]["type"], &body["error"]["message"]),
        (
            &serde_json::json!("rate_limit_error"),
            &serde_json::json!("every account in this pool is at its in-flight cap")
        )
    );
    drop((on_a, on_b));
    up.release();
    up.shutdown().await;
}

/// What the router decided for one request: each decision's `seq` and kind, then the last decision's facts and the
/// holds its request carried, each with its `seq` and whether it ran out.
struct Decided {
    kinds: Vec<(u32, &'static str)>,
    facts: Vec<(&'static str, FactValue, Option<&'static str>)>,
    holds: Vec<(String, u32, bool)>,
}

fn decided(events: &[RouterEvent], request_id: &str) -> Decided {
    let decisions = decisions_of(events, request_id);
    let (summary, record) = decisions.last().expect("the request's decisions");
    Decided {
        kinds: decisions
            .iter()
            .map(|(summary, record)| (record.request.seq, summary.kind))
            .collect(),
        facts: summary
            .fact
            .iter()
            .map(|f| (f.key, f.value.clone(), f.unit))
            .collect(),
        holds: record
            .request
            .holds
            .iter()
            .map(|hold| (hold.account_id.clone(), hold.seq, hold.ran_out))
            .collect(),
    }
}

#[tokio::test]
async fn a_hold_that_ran_out_reads_its_wait_and_the_client_s_status_on_the_exhausted_row() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1), oauth("c", C, 2)],
        PoolSpec {
            failover: account_cap(1, 2),
            ..PoolSpec::default()
        },
    )
    .await;
    let mut open = Vec::new();
    for session in ["s1", "s2", "s3"] {
        open.push(
            h.app
                .clone()
                .oneshot(request_for(KEY, MODEL, session))
                .await
                .unwrap(),
        );
    }
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s4"))
        .await
        .unwrap();
    tokio::time::resume();
    let id = outcome(&resp).request_id;
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::Decision { summary, record, .. } if record.request.request_id == id && summary.kind == "exhausted")
    })
    .await;
    let Decided {
        kinds,
        facts,
        holds,
    } = decided(&seen, &id);
    assert_eq!(
        kinds,
        [(1, "held"), (2, "held"), (3, "held"), (4, "exhausted")],
        "the wait on a ran the bound out, so b and c were passed over before the pool exhausted"
    );
    assert_eq!(holds, [("a".to_string(), 1, true)]);
    let held_ms = outcome(&resp).held.expect("the request's one wait").held_ms;
    assert_eq!(
        facts,
        [
            ("held", FactValue::Int(held_ms as i64), Some("ms")),
            ("retry", FactValue::Text("none".into()), Some("text")),
            (
                "answer",
                FactValue::Int(i64::from(resp.status().as_u16())),
                None
            ),
        ],
        "the row reads the wait three selections back and the status the client got"
    );
    assert_eq!(resp.status().as_u16(), 429);
    drop(open);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn after_a_dialed_leg_a_hold_that_ran_out_reads_the_client_s_503_on_the_exhausted_row() {
    const KEYED: &str = "sk-ant-api03-PLACEHOLDER";
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", held_stream());
    // A refused key marks its account and names no cooldown, so the exhaustion that follows is untimed.
    up.add_credential_rule(
        KEYED,
        "/v1/messages",
        Respond::Json {
            status: 401,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
                .to_vec(),
        },
    );
    let h = harness(
        &up,
        &[oauth("b", B, 0), api_key("a", KEYED, 1)],
        PoolSpec {
            failover: account_cap(1, 1),
            ..PoolSpec::default()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&first).account_id, "b");
    let mut events = h.router.subscribe_events();
    let app = h.app.clone();
    let pending = tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s2")).await });
    // a is dialed and refused before the request is held on b, so the clock stops only for the wait.
    let mut seen = router_events_until(
        &mut events,
        |event| matches!(event, RouterEvent::Decision { summary, .. } if summary.kind == "held"),
    )
    .await;
    tokio::time::pause();
    let resp = pending.await.unwrap().unwrap();
    tokio::time::resume();
    seen.extend(std::iter::from_fn(|| events.try_recv().ok()));
    let id = outcome(&resp).request_id;
    let Decided {
        kinds,
        facts,
        holds,
    } = decided(&seen, &id);
    assert_eq!(kinds, [(1, "placed"), (2, "held"), (3, "exhausted")]);
    assert_eq!(holds, [("b".to_string(), 2, true)]);
    let held_ms = outcome(&resp).held.expect("the request's one wait").held_ms;
    assert_eq!(
        facts,
        [
            ("held", FactValue::Int(held_ms as i64), Some("ms")),
            ("retry", FactValue::Text("none".into()), Some("text")),
            (
                "answer",
                FactValue::Int(i64::from(resp.status().as_u16())),
                None
            ),
        ],
        "the row's answer is the status the client got, not the cap's 429"
    );
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable"),
        "a request that reached an account is no longer a pool merely at its cap"
    );
    drop(first);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_freed_slot_another_request_takes_passes_the_account_over_instead_of_a_second_hold() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 2),
            ..PoolSpec::default()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(h.router.in_flight().account("a"), 1, "a is at its cap");
    let mut events = h.router.subscribe_events();
    let mut holds = h.router.subscribe_events();
    // Both placements wait on a. Ending the one stream wakes both in the same turn; a freed slot is read, not
    // reserved, so the placement whose selection runs second finds it taken by the first.
    let (two, three, ()) = tokio::join!(
        h.app.clone().oneshot(request_for(KEY, MODEL, "s2")),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s3")),
        async {
            next_hold(&mut holds).await;
            next_hold(&mut holds).await;
            assert_eq!(posts(&up), 1, "both placements are held");
            up.release_one().await;
        },
    );
    let (two, three) = (two.unwrap(), three.unwrap());
    let (landed, passed_over) = if two.status() == 200 {
        (&two, &three)
    } else {
        (&three, &two)
    };
    assert_eq!(
        (
            landed.status().as_u16(),
            outcome(landed).account_id.as_str()
        ),
        (200, "a"),
        "one placement takes the freed slot"
    );
    assert_eq!(
        (
            passed_over.status().as_u16(),
            passed_over.headers()["x-phoebis-error-code"]
                .to_str()
                .unwrap(),
            passed_over.headers()["retry-after"].to_str().unwrap()
        ),
        (429, "accounts_at_cap", "2"),
        "a was held for once, so the other passes it over, and with every leg a hold the pool is at its cap"
    );
    let held = outcome(passed_over)
        .held
        .expect("the outcome names the hold it waited through");
    assert_eq!(held.waited_on, ["a"], "one hold per account per request");
    assert!(
        held.held_ms < 2_000,
        "the one wait ended at the release, and no second wait ran the bound out: {}",
        held.held_ms
    );
    assert_eq!(
        posts(&up),
        2,
        "nothing was dialled for the placement passed over"
    );
    let id = outcome(passed_over).request_id;
    let seen = router_events_until(&mut events, |event| match event {
        RouterEvent::Decision {
            summary, record, ..
        } => record.request.request_id == id && summary.kind == "exhausted",
        _ => false,
    })
    .await;
    let Decided { kinds, facts, .. } = decided(&seen, &id);
    assert_eq!(kinds, [(1, "held"), (2, "held"), (3, "exhausted")]);
    assert_eq!(
        facts,
        [
            ("retry", FactValue::Text("none".into()), Some("text")),
            ("answer", FactValue::Int(429), None),
        ],
        "the row records the 429 the client got for the account passed over"
    );
    drop((first, two, three));
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_committed_stream_holds_its_key_permit_and_its_account_ticket_until_its_end() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the client holds the head");
    assert_eq!(
        in_flight(&h, "a"),
        (1, 1),
        "nothing is released at the head"
    );
    up.release();
    let body = drain(resp).await;
    assert!(String::from_utf8_lossy(&body).contains("message_stop"));
    assert_eq!(in_flight(&h, "a"), (0, 0), "released when the body ended");
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_reads_a_committed_responses_end_while_its_report_waits_on_a_full_mailbox() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the client holds the head");
    assert_eq!(in_flight(&h, "a"), (1, 1));
    // Parked once the selection is answered: nothing between the head and the body's end asks the loop.
    let park = h.router.park_loop().await;
    let fill: Vec<_> = (0..phoebis_routing::COMMANDS)
        .map(|_| {
            let router = h.router.clone();
            tokio::spawn(async move {
                router
                    .report(phoebis_routing::Report::Cooldown {
                        account_id: "a".into(),
                        row: phoebis_store::CooldownRow {
                            account_id: "a".into(),
                            scope: CooldownScope::Model("claude-unused".into()),
                            until: i64::MAX,
                            set_at: 0,
                            reason: "rate_limit".into(),
                            window: "five_hour".into(),
                            source_request_id: None,
                            incarnation: 1,
                            spend_share: None,
                        },
                        pin: None,
                        incarnation: 1,
                        lifecycle_generation: 1,
                        credential_generation: 1,
                    })
                    .await
            })
        })
        .collect();
    wait_for(Duration::from_secs(5), || {
        (h.router.queued() == phoebis_routing::COMMANDS).then_some(())
    })
    .await;
    up.release();
    let body = drain(resp).await;
    assert!(String::from_utf8_lossy(&body).contains("message_stop"));
    assert_eq!(
        in_flight(&h, "a"),
        (1, 1),
        "the reports waiting on the mailbox hold the key permit and the account ticket"
    );
    drop(park);
    for report in fill {
        report.await.unwrap();
    }
    wait_for(Duration::from_secs(5), || {
        (in_flight(&h, "a") == (0, 0)).then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_disconnect_mid_stream_releases_both() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(in_flight(&h, "a"), (1, 1));
    let mut body = resp.into_body();
    let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("the first event arrives")
        .unwrap()
        .unwrap();
    assert!(first.is_data());
    drop(body);
    wait_for(Duration::from_secs(5), || {
        (in_flight(&h, "a") == (0, 0)).then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_canceled_leg_releases_its_ticket() {
    let up = FakeUpstream::start(messages(Respond::Gated(Box::new(json_200())))).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let app = h.app.clone();
    let request =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;
    assert_eq!(
        in_flight(&h, "a"),
        (1, 1),
        "the upstream holds the head back"
    );
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(
        in_flight(&h, "a"),
        (0, 0),
        "the dropped future released both"
    );
    up.shutdown().await;
}

/// The id of the first request whose observation `sub` carries, read off the queue up to it.
async fn observed_id(sub: &mut phoebis_bus::Subscriber) -> String {
    wait_for(Duration::from_secs(5), || {
        std::iter::from_fn(|| sub.events.try_recv().ok()).find_map(|e| match e {
            Event::RequestObserved(o) => Some(o.request_id),
            _ => None,
        })
    })
    .await
}

/// Whether nothing queued on `sub` completes `request_id`, reading what is queued.
fn no_completion_queued(sub: &mut phoebis_bus::Subscriber, request_id: &str) -> bool {
    !std::iter::from_fn(|| sub.events.try_recv().ok())
        .any(|e| matches!(e, Event::RequestComplete(c) if c.request_id == request_id))
}

/// Every completion for `request_id` among `events`, the batch `drain_until_complete` read up to the first one, so a
/// second completion that arrived in the same batch is counted where `completion_of` would stop at the first.
fn completions_in(events: Vec<Event>, request_id: &str) -> Vec<phoebis_bus::RequestComplete> {
    events
        .into_iter()
        .filter_map(|e| match e {
            Event::RequestComplete(c) if c.request_id == request_id => Some(c),
            _ => None,
        })
        .collect()
}

/// A rejection the relay drains whole and forwards as it came.
fn rejection_400() -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#
            .to_vec(),
    }
}

#[tokio::test]
async fn a_client_that_leaves_before_the_head_lands_a_client_gone_completion() {
    let up = FakeUpstream::start(messages(Respond::Gated(Box::new(json_200())))).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        default_limits(),
    )
    .await;
    let app = h.app.clone();
    let request =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;
    let request_id = observed_id(&mut sub).await;
    // Off the instant the request arrived at, so the completion's `finished_at` reads the clock at the drop.
    h.clock.store(NOW + 2_500, Ordering::SeqCst);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let complete = completion_of(&mut sub, &request_id).await;
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.status,
            complete.attempt,
            complete.account_id.as_deref()
        ),
        (
            phoebis_bus::Outcome::Canceled,
            Some("client_gone"),
            0,
            1,
            Some("a")
        ),
        "the dropped handler completes the attempt it was on with nothing answered"
    );
    assert_eq!(
        (
            complete.usage.is_none(),
            complete.finished_at,
            complete.held.as_deref()
        ),
        (true, NOW + 2_500, None),
        "no usage, the clock at the drop, and the trace's holds, of which this request took none"
    );
    stays_for(HELD_FOR, || no_completion_queued(&mut sub, &request_id)).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_cut_before_its_head_lands_client_gone_until_the_mark_is_set_and_router_shutdown_after(
) {
    let up = FakeUpstream::start(messages(Respond::Gated(Box::new(json_200())))).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let mark = phoebis_relay::ShutdownMark::default();
    let h = harness_with_shutdown_mark(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        default_limits(),
        mark.clone(),
    )
    .await;
    // Attached and unset, as the mark is while the router serves, so a caller that leaves keeps its own code.
    let app = h.app.clone();
    let left =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s0")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;
    let left_id = observed_id(&mut sub).await;
    left.abort();
    assert!(left.await.unwrap_err().is_cancelled());
    let left_complete = completion_of(&mut sub, &left_id).await;
    assert_eq!(
        left_complete.error_code.as_deref(),
        Some("client_gone"),
        "the caller left while the mark was unset"
    );
    let app = h.app.clone();
    let request =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let request_id = observed_id(&mut sub).await;
    mark.store(true, Ordering::SeqCst);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let complete = completion_of(&mut sub, &request_id).await;
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.status,
            complete.attempt,
            complete.account_id.as_deref()
        ),
        (
            phoebis_bus::Outcome::Canceled,
            Some("router_shutdown"),
            0,
            1,
            Some("a")
        ),
        "the router's own cut completes the attempt it was on under its own code"
    );
    stays_for(HELD_FOR, || no_completion_queued(&mut sub, &request_id)).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_served_to_its_end_publishes_one_completion_and_no_client_gone_after_it() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        default_limits(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let request_id = outcome(&resp).request_id;
    drain(resp).await;
    let completions = completions_in(drain_until_complete(&mut sub).await, &request_id);
    assert_eq!(
        completions
            .iter()
            .map(|c| (c.outcome, c.error_code.as_deref()))
            .collect::<Vec<_>>(),
        [(phoebis_bus::Outcome::Succeeded, None)],
        "the body's own completion, and no other"
    );
    stays_for(HELD_FOR, || no_completion_queued(&mut sub, &request_id)).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_forwarded_rejection_publishes_one_completion_and_no_client_gone_after_it() {
    let up = FakeUpstream::start(messages(rejection_400())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        default_limits(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let request_id = outcome(&resp).request_id;
    drain(resp).await;
    let completions = completions_in(drain_until_complete(&mut sub).await, &request_id);
    assert_eq!(
        completions
            .iter()
            .map(|c| (c.status, c.outcome, c.error_code.as_deref()))
            .collect::<Vec<_>>(),
        [(
            400,
            phoebis_bus::Outcome::Failed,
            Some("invalid_request_error")
        )],
        "the forwarded answer's own completion, and no other"
    );
    stays_for(HELD_FOR, || no_completion_queued(&mut sub, &request_id)).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_that_leaves_while_the_completion_waits_for_the_ledger_lands_one_completion() {
    let up = FakeUpstream::start(messages(Respond::Gated(Box::new(rejection_400())))).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus.clone(),
        default_limits(),
    )
    .await;
    let app = h.app.clone();
    let request =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;
    let request_id = observed_id(&mut sub).await;
    while sub.events.try_recv().is_ok() {}
    // One slot stays free, which the head takes: the rejection's chunk then finds the queue full and is counted as a
    // drop, and the completion that follows it with no await between waits for room.
    for index in 0..phoebis_bus::QUEUE_DEPTH as u32 - 1 {
        bus.publish(Event::ResponseChunk(phoebis_bus::ResponseChunk {
            request_id: "filler".into(),
            attempt: 1,
            index,
            body: Bytes::new(),
            phase: None,
        }));
    }
    up.release();
    wait_for(Duration::from_secs(5), || {
        (sub.stats.dropped.load(Ordering::SeqCst) == 1).then_some(())
    })
    .await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let completions = completions_in(drain_until_complete(&mut sub).await, &request_id);
    assert_eq!(
        completions
            .iter()
            .map(|c| (c.outcome, c.error_code.as_deref(), c.status))
            .collect::<Vec<_>>(),
        [(phoebis_bus::Outcome::Canceled, Some("client_gone"), 0)],
        "the completion the wait never delivered lands from the dropped handler, once"
    );
    stays_for(HELD_FOR, || no_completion_queued(&mut sub, &request_id)).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_and_a_count_take_no_key_permit() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", held_stream());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let held = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(held.status().as_u16(), 200);
    assert_eq!(
        h.router.in_flight().key("desktop"),
        1,
        "the held message holds the key's one permit"
    );

    // The engine's own probe: no credential header, the key named by id.
    up.push_credential_rule(A, "/v1/messages", json_200());
    pin_session(&h, "s2", "main", MODEL).await;
    let mut probe = request_for(KEY, MODEL, "s2");
    probe.headers_mut().remove("authorization");
    probe
        .extensions_mut()
        .remove::<phoebis_relay::OriginalHead>();
    let probed = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
                holder: phoebis_routing::Holder {
                    account_id: "a".into(),
                    incarnation: h.store.account("a").unwrap().unwrap().incarnation,
                },
            },
            probe,
        )
        .await;
    assert_eq!(probed.status().as_u16(), 200, "the probe is served");
    drain(probed).await;

    up.push_credential_rule(A, "/v1/messages", json_200());
    let counted = h
        .app
        .clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::CountOauthNonFirstParty),
            body(BodyFixture::Count),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(counted.status().as_u16(), 200, "the count is served");
    drain(counted).await;
    assert_eq!(
        h.router.in_flight().key("desktop"),
        1,
        "neither took a permit"
    );
    drop(held);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_takes_no_read_permit_and_no_budget() {
    let up = FakeUpstream::start(messages(json_200())).await;
    // Neither bound admits a body: no key may have one arriving, and the budget holds a single byte.
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            body_budget_bytes: 1,
            key_body_reads: 0,
            ..default_limits()
        },
    )
    .await;
    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
    )
    .await
    .expect("refused at once")
    .unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "key_body_reads"),
        "a request from a socket meets the bounds"
    );

    pin_session(&h, "s2", "main", MODEL).await;
    let mut probe = request_for(KEY, MODEL, "s2");
    probe.headers_mut().remove("authorization");
    probe
        .extensions_mut()
        .remove::<phoebis_relay::OriginalHead>();
    let probed = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
                holder: phoebis_routing::Holder {
                    account_id: "a".into(),
                    incarnation: h.store.account("a").unwrap().unwrap().incarnation,
                },
            },
            probe,
        )
        .await;
    assert_eq!(
        probed.status().as_u16(),
        200,
        "the probe's body is the router's own and already in memory, so it takes neither"
    );
    drain(probed).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_declared_body_past_the_budget_is_refused_503_before_a_byte_is_read() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        phoebis_relay::Limits {
            body_budget_bytes: 4096,
            ..default_limits()
        },
    )
    .await;
    let (request, tx) = channel_request(Some(8192));
    let refused = tokio::time::timeout(Duration::from_secs(5), h.app.clone().oneshot(request))
        .await
        .expect("refused before the body that never comes")
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503);
    assert_eq!(
        refused.headers()["x-phoebis-error-code"],
        "body_budget_exhausted"
    );
    assert!(
        refused.headers().get("retry-after").is_none(),
        "a freed body says nothing about when"
    );
    let refused_id = outcome(&refused).request_id;
    let body = drain(refused).await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        (
            json["error"]["type"].as_str(),
            json["error"]["message"].as_str()
        ),
        (
            Some("api_error"),
            Some("the router holds as many request bytes as it allows")
        )
    );
    let (observed, complete) = observed_and_completed(&mut sub, &refused_id).await;
    assert!(observed.body.is_empty(), "no byte of the body was read");
    assert_eq!(
        (complete.status, complete.error_code.as_deref()),
        (503, Some("body_budget_exhausted"))
    );
    assert_eq!(posts(&up), 0);
    drop(tx);
    up.shutdown().await;
}

/// A Messages body that passes admission, 1,000 bytes long: the model, the account's `metadata.user_id`, `tools`
/// as given (empty or a member with its trailing comma), and spaces inside the one message's text for the rest.
fn thousand_byte_body(tools: &str) -> Vec<u8> {
    let start = format!(
        r#"{{"model":"{MODEL}","max_tokens":1,"stream":true,"metadata":{{"user_id":"{{\"device_id\":\"d\",\"account_uuid\":\"{ACCOUNT_UUID}\",\"session_id\":\"s1\"}}"}},{tools}"messages":[{{"role":"user","content":""#
    );
    let end = r#""}]}"#;
    let mut out = start.into_bytes();
    out.resize(1_000 - end.len(), b' ');
    out.extend_from_slice(end.as_bytes());
    out
}

/// `body` on the OAuth fixture head with its length declared, so the relay reserves the body's own bytes at the head.
fn declared(body: &[u8], session: &str) -> Request<Body> {
    let mut req = request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), body, KEY);
    set_header(&mut req, "x-claude-code-session-id", session);
    set_header(&mut req, "content-length", &body.len().to_string());
    req
}

#[tokio::test]
async fn the_budget_holds_a_body_s_parsed_tree_from_before_the_parse_until_the_relay_drops_it() {
    let up = FakeUpstream::start(messages(Respond::Gated(Box::new(json_200())))).await;
    // Each body declares its 1,000 bytes, which the head reserves, and the tree takes `TREE_BYTES_PER_ELEMENT`, 363
    // bytes, per counted element. The plain body counts 9 elements (the root's `{`, the commas after `model`,
    // `max_tokens`, `stream` and `metadata`, the `{` of `metadata`, the `[` of `messages`, its message's `{` and the
    // comma after `role`), a tree of 3,267 bytes. The swollen one adds a tool of 100 empty arrays and counts 215 (the
    // 9, then the `[` of `tools`, the tool's `{`, the comma after `name`, the `{` of `input_schema`, the comma after
    // `type`, the `[` of `x`, 100 `[`, 99 commas and the comma before `messages`), a tree of 78,045 bytes. Under a
    // budget of 7,000:
    // (a) the swollen body's 1,000 bytes fit, and 1,000 + 78,045 = 79,045 do not,
    // (b) the plain body's 1,000 + 3,267 = 4,267 fit,
    // (c) with (b) held at the upstream on its 1,000 bytes once its tree is dropped, a second plain body's
    //     1,000 + 1,000 + 3,267 = 5,267 fit, where 4,267 + 4,267 = 8,534 would not, had (b) kept its tree.
    // The three hold for any figure per element from 278 to 555.
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            body_budget_bytes: 7_000,
            ..default_limits()
        },
    )
    .await;
    let plain = thousand_byte_body("");
    let swollen_tool = format!(
        r#""tools":[{{"name":"t","input_schema":{{"type":"object","x":[{}[]]}}}}],"#,
        "[],".repeat(99)
    );
    let swollen = thousand_byte_body(&swollen_tool);

    // Raced against the upstream's record, since a relayed body waits at the gate.
    let app = h.app.clone();
    let swollen_request = declared(&swollen, "s1");
    let refused = tokio::spawn(async move { app.oneshot(swollen_request).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (posts(&up) == 1 || refused.is_finished()).then_some(())
    })
    .await;
    assert_eq!(posts(&up), 0, "the swollen body never reached the upstream");
    let refused = refused.await.unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap(),
            refused
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok())
        ),
        (413, "body_over_budget", Some("false")),
        "a body whose bytes and tree are more than the whole budget is refused for good"
    );
    drain(refused).await;

    let app = h.app.clone();
    let first_plain = declared(&plain, "s2");
    let first = tokio::spawn(async move { app.oneshot(first_plain).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;

    let app = h.app.clone();
    let second_plain = declared(&plain, "s3");
    let second = tokio::spawn(async move { app.oneshot(second_plain).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (posts(&up) == 2 || second.is_finished()).then_some(())
    })
    .await;
    assert_eq!(
        posts(&up),
        2,
        "the second body fits beside the first one's bytes once the first tree was given back"
    );

    up.release();
    for request in [first, second] {
        let answered = request.await.unwrap();
        assert_eq!(answered.status().as_u16(), 200);
        drain(answered).await;
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_and_tree_at_the_whole_budget_are_served_and_one_byte_over_is_refused_for_good() {
    let up = FakeUpstream::start(messages(json_200())).await;
    // The plain body's 1,000 bytes and its 9 elements at 363 bytes each, a tree of 3,267, fill a budget of 4,267
    // exactly. Both budgets are worked from `TREE_BYTES_PER_ELEMENT` at 363, which no suite can import, so a
    // remeasure of it moves both.
    let plain = thousand_byte_body("");
    let filled = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            body_budget_bytes: 4_267,
            ..default_limits()
        },
    )
    .await;
    let served = filled
        .app
        .clone()
        .oneshot(declared(&plain, "s1"))
        .await
        .unwrap();
    assert_eq!(
        served.status().as_u16(),
        200,
        "a body and tree equal to the whole budget are served"
    );
    drain(served).await;

    let short = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            body_budget_bytes: 4_266,
            ..default_limits()
        },
    )
    .await;
    let refused = short
        .app
        .clone()
        .oneshot(declared(&plain, "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (413, "body_over_budget"),
        "a body and tree one byte over the whole budget are refused for good"
    );
    drain(refused).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_within_the_whole_budget_that_the_bodies_in_flight_leave_no_room_for_answers_503() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            body_budget_bytes: 7_000,
            ..default_limits()
        },
    )
    .await;
    // The held read reserves its declared 4,267 bytes from the head on, which stands for the bytes and the tree of
    // one served plain body. A second plain body's 1,000 bytes and tree of 3,267 fit the whole budget of 7,000, and
    // 4,267 + 4,267 = 8,534 do not fit beside the held read, so that body only has to wait.
    let (holder, holder_tx) = channel_request(Some(4_267));
    let app = h.app.clone();
    let held = tokio::spawn(async move { app.oneshot(holder).await.unwrap() });
    // The channel holds one chunk, so the second send returns once the relay took the first, past its reservation.
    for chunk in [&b"{\"model\":"[..], b" "] {
        tokio::time::timeout(
            Duration::from_secs(5),
            holder_tx.send(Ok(Bytes::from_static(chunk))),
        )
        .await
        .expect("the relay takes the chunk")
        .expect("the relay reads the held body rather than refusing it");
    }

    let refused = h
        .app
        .clone()
        .oneshot(declared(&thousand_byte_body(""), "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "body_budget_exhausted"),
        "a body that fits the whole budget but not beside the bodies in flight is told to come again"
    );
    drain(refused).await;
    drop(holder_tx);
    held.await.unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn the_bodies_arriving_on_one_key_are_bounded_by_key_body_reads_with_retry_after_1() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        phoebis_relay::Limits {
            key_body_reads: 2,
            ..default_limits()
        },
    )
    .await;
    let (first, first_tx) = reading(&h).await;
    let (second, second_tx) = reading(&h).await;
    let (request, third_tx) = channel_request(None);
    let refused = tokio::time::timeout(Duration::from_secs(5), h.app.clone().oneshot(request))
        .await
        .expect("refused at once")
        .unwrap();
    assert_eq!(refused.status().as_u16(), 429);
    assert_eq!(refused.headers()["x-phoebis-error-code"], "key_body_reads");
    assert_eq!(refused.headers()["retry-after"], "1");
    let refused_id = outcome(&refused).request_id;
    let json: serde_json::Value = serde_json::from_slice(&drain(refused).await).unwrap();
    assert_eq!(
        (
            json["error"]["type"].as_str(),
            json["error"]["message"].as_str()
        ),
        (
            Some("rate_limit_error"),
            Some("this key has as many request bodies arriving as it allows")
        )
    );
    let (observed, complete) = observed_and_completed(&mut sub, &refused_id).await;
    assert!(
        observed.body.is_empty(),
        "the bound is met before the read, so the refusal carries no body"
    );
    assert_eq!(
        (complete.status, complete.error_code.as_deref()),
        (429, Some("key_body_reads"))
    );
    drop(third_tx);

    drop(first_tx);
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the ended body is answered")
        .unwrap();
    let fourth = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s4")),
    )
    .await
    .expect("admitted once a read ended")
    .unwrap();
    assert_eq!(fourth.status().as_u16(), 200, "the freed read let it in");
    drain(fourth).await;
    drop(second_tx);
    second.await.unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn a_declared_length_past_the_per_body_bound_is_413_without_a_read() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let (request, tx) = channel_request(Some((32 << 20) + 1));
    let refused = tokio::time::timeout(Duration::from_secs(5), h.app.clone().oneshot(request))
        .await
        .expect("refused before the body that never comes")
        .unwrap();
    assert_eq!(refused.status().as_u16(), 413);
    assert_eq!(
        refused.headers()["x-phoebis-error-code"],
        "request_too_large"
    );
    let refused_id = outcome(&refused).request_id;
    let (observed, complete) = observed_and_completed(&mut sub, &refused_id).await;
    assert!(observed.body.is_empty(), "no byte of the body was read");
    assert_eq!(
        (complete.status, complete.error_code.as_deref()),
        (413, Some("request_too_large"))
    );
    assert_eq!(posts(&up), 0);
    drop(tx);
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_count_at_the_accounts_cap_is_served_and_takes_no_slot() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", held_stream());
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 1),
            ..PoolSpec::default()
        },
    )
    .await;
    let held = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(held.status().as_u16(), 200);
    assert_eq!(
        in_flight(&h, "a"),
        (1, 1),
        "the stream holds the account's one slot and a key permit"
    );

    up.push_credential_rule(A, "/v1/messages", json_200());
    let counted = h
        .app
        .clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::CountOauthNonFirstParty),
            body(BodyFixture::Count),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(
        counted.status().as_u16(),
        200,
        "a count is neither held nor refused at the account's cap"
    );
    assert_eq!(
        in_flight(&h, "a"),
        (1, 1),
        "the count's open response holds no ticket and no permit"
    );
    drain(counted).await;
    assert_eq!(
        in_flight(&h, "a"),
        (1, 1),
        "the count took no ticket and no permit"
    );
    drop(held);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_pool_with_nothing_to_hold_for_still_answers_accounts_unavailable() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 2),
            ..PoolSpec::default()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(h.router.in_flight().account("a"), 1, "a is at its cap");
    // Paused, the account is no longer one a request could be held for, cap or not.
    h.store.set_enabled("a", false, "test").unwrap();
    h.router.reload().await.unwrap();
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    let id = outcome(&resp).request_id;
    let seen = router_events_until(&mut events, |event| match event {
        RouterEvent::Decision {
            summary, record, ..
        } => record.request.request_id == id && summary.kind == "exhausted",
        _ => false,
    })
    .await;
    assert_eq!(
        decided(&seen, &id).kinds,
        [(1, "exhausted")],
        "answered without a hold"
    );
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable")
    );
    assert!(resp.headers().get("retry-after").is_none());
    assert_eq!(posts(&up), 1, "nothing was dialled for the second request");
    drop(first);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn two_admissions_released_together_are_both_counted_and_both_released() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let (one, two) = tokio::join!(
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s2")),
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    assert_eq!(
        [one.status().as_u16(), two.status().as_u16()],
        [200, 200],
        "a key bounds none of its requests"
    );
    assert_eq!(posts(&up), 2);
    assert_eq!(
        h.router.in_flight().key("desktop"),
        2,
        "each admission holds its own permit"
    );
    up.release();
    drain(one).await;
    drain(two).await;
    assert_eq!(
        h.router.in_flight().key("desktop"),
        0,
        "each permit came back with its body's end"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_in_place_keeps_the_one_key_permit() {
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;
    assert_eq!(
        h.router.in_flight().key("desktop"),
        1,
        "the request sleeping in its retry still holds its one permit"
    );
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the retry lands")
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).attempt),
        (200, 2),
        "retried in place"
    );
    drain(resp).await;
    assert_eq!(h.router.in_flight().key("desktop"), 0);
    up.shutdown().await;
}

/// The rate-limit tier the slowed harness puts its account on, which its pricer keys on.
const SLOWED_TIER: &str = "default_claude_max_20x";

/// The burn average the slowed harness sets on its pool, and how far before the held turn's clock its turns run. Five
/// minutes keeps every instant the harness dates after the epoch, since `NOW` lies a thousand seconds past it.
const SLOWED_AVERAGE_SECONDS: u64 = 300;

/// Every pin on `SLOWED_TIER` burns a tenth of the 5-hour window an hour whatever its usage, and nothing is charged, so
/// the window reads what the test reports.
fn tenth_an_hour() -> SharePricer {
    SharePricer::new(|_, _, _, _, _, _| None).with_rate(|window, _, tier, _, _, _| {
        (window == FIVE_HOUR && tier == SLOWED_TIER).then_some(0.1 / 3_600.0)
    })
}

/// `a` priced by `tenth_an_hour`, the pool's slow-down turned on with a two-second cap, the shedding round off and the
/// burn average at `SLOWED_AVERAGE_SECONDS`, `a`'s 5-hour window at 0.9 an hour from its reset, and three turns of `s1`
/// ten seconds apart. The reading and the turns are dated one burn average before `s1`'s next turn, at the clock this
/// leaves, so the stamp the renewals write has stood a burn average by then. The first turn places the pin and reads
/// no slow-down, the second starts its pace and stamps the ramp, and the third builds its ten-second cycle, both
/// renewals passing unheld at a ramp near 0. The gap of one burn average to the next turn is a pause that leaves the
/// cycle standing, and the room under the ceiling lasts half the time to the reset, so that turn reads a factor of
/// about a half and waits the cap.
async fn slowed_harness(up: &FakeUpstream) -> Harness {
    let h = harness_with_router_config(
        up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        RouterConfig {
            share: tenth_an_hour(),
            ..RouterConfig::default()
        },
    )
    .await;
    h.store
        .set_profile(
            "a",
            "{}",
            &SubscriptionRow {
                tier: Some(SLOWED_TIER.into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.slow_down = SlowDown {
        slow_down: true,
        slow_down_hold_seconds: 2,
        ..SlowDown::default()
    };
    pool.shedding.load_shedding = false;
    pool.shedding.burn_average_seconds = SLOWED_AVERAGE_SECONDS;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
    let back = SLOWED_AVERAGE_SECONDS as i64 * 1_000;
    h.router
        .report(Report::Sample {
            account_id: "a".into(),
            sample: UtilizationSample::default().with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.9,
                    reset: Some(NOW + 10_000 + 3_600_000),
                    observed_at: NOW - back,
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    for at in [NOW - back, NOW + 10_000 - back, NOW + 20_000 - back] {
        h.clock.store(at, Ordering::SeqCst);
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, MODEL, "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        drain(resp).await;
    }
    h.clock.store(NOW + 20_000, Ordering::SeqCst);
    h
}

/// The next held decision's hold, as its record names it.
async fn next_hold(events: &mut broadcast::Receiver<RouterEvent>) -> HoldOutcome {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the hold's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if summary.kind == "held" {
                return record.outcome.hold.clone().expect("a held outcome");
            }
        }
    }
}

/// The waits a request's outcome names, each with its reason, as the completion writes them.
fn waits_of(resp: &axum::http::Response<Body>) -> Vec<(String, u64)> {
    let held = outcome(resp)
        .held
        .expect("the outcome names the holds it waited through");
    let value = serde_json::to_value(&held).unwrap();
    value["waits"]
        .as_array()
        .expect("the held JSON lists its waits")
        .iter()
        .map(|wait| {
            (
                wait["reason"].as_str().unwrap().to_string(),
                wait["held_ms"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_slowed_pins_request_waits_before_the_head_and_the_completion_names_the_wait() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = slowed_harness(&up).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let hold = next_hold(&mut events).await;
    assert_eq!(
        (hold.account_id.as_str(), hold.reason, hold.until),
        ("a", HoldReason::SlowDown, Some(NOW + 22_000)),
        "the pin waits the two-second cap from its arrival"
    );
    // The runtime's time runs on while the harness's clock stands short of the deadline: nothing is dialed.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(posts(&up), 3, "nothing is dialed while the hold runs");
    assert!(!waiting.is_finished());
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    tokio::time::resume();
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the request goes on at its deadline")
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(posts(&up), 4, "one post once the hold ends");
    let held = outcome(&resp).held.expect("the outcome names the hold");
    assert_eq!(held.waited_on, ["a"]);
    let waits = waits_of(&resp);
    assert_eq!(
        waits
            .iter()
            .map(|(reason, _)| reason.as_str())
            .collect::<Vec<_>>(),
        ["slow_down"]
    );
    assert!(
        waits[0].1 >= 1_500 && waits[0].1 == held.held_ms,
        "the wait lasted until the deadline: {waits:?}"
    );
    drain(resp).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_request_of_the_same_pin_joins_the_standing_deadline() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = slowed_harness(&up).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let mut waiting = Vec::new();
    let mut untils = Vec::new();
    for second in [false, true] {
        // The second request arrives a second later, so a fresh hold of the two-second cap would end a second past the
        // first one's deadline.
        if second {
            h.clock.store(NOW + 21_000, Ordering::SeqCst);
        }
        let app = h.app.clone();
        waiting.push(tokio::spawn(async move {
            app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap()
        }));
        let hold = next_hold(&mut events).await;
        untils.push((hold.reason, hold.until));
    }
    assert_eq!(
        untils,
        [
            (HoldReason::SlowDown, Some(NOW + 22_000)),
            (HoldReason::SlowDown, Some(NOW + 22_000))
        ],
        "the second request of the pin waits to the first one's deadline"
    );
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(posts(&up), 3, "neither is dialed before the deadline");
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    tokio::time::resume();
    for request in waiting {
        let resp = tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .expect("both go on at the one deadline")
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            waits_of(&resp)
                .iter()
                .map(|(reason, _)| reason.as_str())
                .collect::<Vec<_>>(),
            ["slow_down"],
            "each waited once"
        );
        drain(resp).await;
    }
    assert_eq!(posts(&up), 5);
    up.shutdown().await;
}

/// A committed stream whose head and commit both wait for `up.release()`.
fn gated_stream() -> Respond {
    Respond::Gated(Box::new(Respond::Sse {
        status: 200,
        headers: vec![],
        events: vec![message_start(), message_stop()],
        delay_between: Duration::ZERO,
    }))
}

/// A stream whose head comes at once and whose commit waits for `up.release()`.
fn stalled_stream() -> Respond {
    Respond::Stall {
        status: 200,
        headers: vec![],
        events: vec![message_start(), message_stop()],
    }
}

/// The hold cap of the moved harness's pool, in seconds.
const MOVED_HOLD_SECONDS: u32 = 2;

/// How long a woken request may take to go out: one slice of the relay's wait loop.
const TICK: Duration = Duration::from_millis(250);

/// The harness over `accounts` with the pool's hold cap at `MOVED_HOLD_SECONDS`, the slow-down and the shedding round
/// off, `b`'s windows polled with room, and `s1`'s first turn answered on `a` with `a`'s 5-hour window spent ten hours
/// from its reset, so the spent rule moves the session's next request to `b`. The clock is left at `NOW + 20_000`.
async fn moved_harness(up: &FakeUpstream, accounts: &[TestAccount]) -> Harness {
    let h = harness(up, accounts, PoolSpec::default()).await;
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.slow_down = SlowDown {
        slow_down: false,
        slow_down_hold_seconds: MOVED_HOLD_SECONDS,
        ..SlowDown::default()
    };
    pool.shedding.load_shedding = false;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
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
    let reset = (NOW / 1_000 + 10 * 3_600).to_string();
    up.push_credential_rule(
        A,
        "/v1/messages",
        json_200_with_headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
            ("anthropic-ratelimit-unified-5h-reset", &reset),
        ]),
    );
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&first).account_id, "a");
    drain(first).await;
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
    h.clock.store(NOW + 20_000, Ordering::SeqCst);
    h
}

/// The subagent's summary fork of `session`, the class the client sends it under.
fn fork_of(session: &str) -> Request<Body> {
    let mut req = request_for(KEY, MODEL, session);
    set_header(&mut req, "x-claude-code-request-class", "auxiliary");
    req
}

/// `req` sent through the harness on a task of its own.
fn sent(h: &Harness, req: Request<Body>) -> tokio::task::JoinHandle<axum::http::Response<Body>> {
    let app = h.app.clone();
    tokio::spawn(async move { app.oneshot(req).await.unwrap() })
}

/// The request id of the next decision that moved a pin to `account`.
async fn moved_to(events: &mut broadcast::Receiver<RouterEvent>, account: &str) -> String {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the move's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if summary.kind == "moved" && record.outcome.account_id.as_deref() == Some(account) {
                return record.request.request_id.clone();
            }
        }
    }
}

/// The next held decision's record.
async fn next_held(events: &mut broadcast::Receiver<RouterEvent>) -> DecisionRecord {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the hold's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if summary.kind == "held" {
                return std::sync::Arc::unwrap_or_clone(record);
            }
        }
    }
}

/// Every decision kind the router published for `request_id` until its last decision, a placement or a renewal.
async fn decision_kinds(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the request's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if record.request.request_id == request_id {
                kinds.push(summary.kind);
                if summary.kind != "held" {
                    return kinds;
                }
            }
        }
    }
}

/// Asserts the turn is still waiting with nothing dialed for it beyond the `dialed` posts already made.
fn still_held(
    up: &FakeUpstream,
    turn: &tokio::task::JoinHandle<axum::http::Response<Body>>,
    dialed: usize,
) {
    assert_eq!(
        posts(up),
        dialed,
        "nothing is dialed for the turn while the first request's head is out"
    );
    assert!(!turn.is_finished(), "the turn still waits");
}

/// `task`'s response within five seconds of the runtime's clock.
async fn answered(
    task: tokio::task::JoinHandle<axum::http::Response<Body>>,
) -> axum::http::Response<Body> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the request is answered")
        .unwrap()
}

#[tokio::test]
async fn the_moved_pins_turn_waits_for_the_forks_commit_and_goes_out_when_it_comes() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let fork = sent(&h, fork_of("s1"));
    let fork_id = moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    tokio::time::sleep_until(started + Duration::from_millis(207)).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let held = next_held(&mut events).await;
    let hold = held.outcome.hold.expect("a held outcome");
    assert_eq!(
        (hold.reason, hold.waits_for.as_deref(), hold.until),
        (
            HoldReason::Rewrite,
            Some(fork_id.as_str()),
            Some(NOW + 22_000)
        ),
        "the turn waits for the fork's head, the cap from the fork's dispatch"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        posts(&up),
        2,
        "nothing is dialed for the turn while the fork's head is out"
    );
    assert!(!turn.is_finished());
    tokio::time::sleep_until(started + Duration::from_millis(2_560)).await;
    up.release();
    let released = tokio::time::Instant::now();
    let turn = answered(turn).await;
    assert!(
        released.elapsed() <= TICK,
        "the turn goes out at the fork's commit"
    );
    assert_eq!(turn.status(), 200);
    assert_eq!(posts(&up), 3);
    let waits = waits_of(&turn);
    assert_eq!(
        waits
            .iter()
            .map(|(reason, _)| reason.as_str())
            .collect::<Vec<_>>(),
        ["rewrite"]
    );
    assert!(
        waits[0].1.abs_diff(2_353) <= 300,
        "the wait ran from the turn's arrival to the fork's commit: {waits:?}"
    );
    let turn_id = outcome(&turn).request_id;
    assert_eq!(
        held.request.request_id, turn_id,
        "the held decision is the turn's"
    );
    assert_eq!(
        decision_kinds(&mut events, &turn_id).await,
        ["renewed"],
        "the hold is followed by the renewal"
    );
    drain(turn).await;
    drain(answered(fork).await).await;
    up.shutdown().await;
}

#[tokio::test]
async fn the_bound_ends_the_wait_with_ran_out_and_a_later_head_holds_nothing_more() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", gated_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(207)).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let held = next_held(&mut events).await;
    let hold = held.outcome.hold.clone().expect("a held outcome");
    assert_eq!(hold.reason, HoldReason::Rewrite);
    tokio::time::sleep(Duration::from_secs(2)).await;
    still_held(&up, &turn, 2);
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    let turn = answered(turn).await;
    assert_eq!(turn.status(), 200);
    let waited = outcome(&turn).held.expect("the outcome names the wait");
    let waits = waits_of(&turn);
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].0, "rewrite");
    assert!(
        (2_000..=2_000 + TICK.as_millis() as u64).contains(&waits[0].1),
        "the wait ran to the bound: {waits:?}"
    );
    let value = serde_json::to_value(&waited).unwrap();
    assert_eq!(value["waits"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        last_wait(&after_the_wait(&mut events, &held.request.request_id).await),
        Some((HoldReason::Rewrite, true)),
        "the bound ended the wait"
    );
    drain(turn).await;
    up.release();
    drain(answered(fork).await).await;
    let third = answered(sent(&h, request_for(KEY, MODEL, "s1"))).await;
    assert_eq!(third.status(), 200);
    assert!(
        outcome(&third).held.is_none(),
        "the fork's head cleared the mark"
    );
    drain(third).await;
    up.shutdown().await;
}

#[tokio::test]
async fn the_first_requests_end_without_a_head_releases_the_waiter_at_once() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(
        B,
        "/v1/messages",
        Respond::Gated(Box::new(Respond::CloseAfterRequest)),
    );
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let held = next_held(&mut events).await;
    let hold = held.outcome.hold.clone().expect("a held outcome");
    assert_eq!(hold.reason, HoldReason::Rewrite);
    tokio::time::sleep(Duration::from_millis(500)).await;
    still_held(&up, &turn, 2);
    up.release();
    let released = tokio::time::Instant::now();
    let fork = answered(fork).await;
    assert_eq!(
        fork.status(),
        502,
        "the fork's leg was lost before its head"
    );
    drain(fork).await;
    let turn = answered(turn).await;
    assert!(
        released.elapsed() <= TICK,
        "the fork's end releases the turn"
    );
    assert_eq!(turn.status(), 200);
    let waits = waits_of(&turn);
    assert_eq!(waits.len(), 1);
    assert!(
        (500..u64::from(MOVED_HOLD_SECONDS) * 1_000).contains(&waits[0].1),
        "the wait ran to the fork's end, under the bound: {waits:?}"
    );
    let completion: serde_json::Value =
        serde_json::to_value(outcome(&turn).held.expect("the wait")).unwrap();
    assert_eq!(completion["waits"][0]["reason"], "rewrite");
    assert_eq!(
        last_wait(&after_the_wait(&mut events, &held.request.request_id).await),
        Some((HoldReason::Rewrite, false)),
        "the fork's end ended the wait inside its bound"
    );
    drain(turn).await;
    let third = answered(sent(&h, request_for(KEY, MODEL, "s1"))).await;
    assert!(outcome(&third).held.is_none(), "the end cleared the mark");
    drain(third).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_fresh_placement_and_its_fork_both_dial_at_once() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let turn = sent(&h, request_for(KEY, MODEL, "s2"));
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let fork = sent(&h, fork_of("s2"));
    wait_for(Duration::from_secs(5), || (posts(&up) == 3).then_some(())).await;
    while let Ok(event) = events.try_recv() {
        if let RouterEvent::Decision { summary, .. } = event {
            assert_ne!(summary.kind, "held", "a fresh placement holds nothing");
        }
    }
    up.release();
    drain(answered(turn).await).await;
    drain(answered(fork).await).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_count_on_the_moved_pin_goes_out_at_once() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages/count_tokens", json_200());
    up.add_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let mut count = request_for(KEY, MODEL, "s1");
    *count.uri_mut() = "/v1/messages/count_tokens".parse().unwrap();
    let count = answered(sent(&h, count)).await;
    assert_eq!(count.status(), 200);
    assert_eq!(posts(&up), 3, "the count goes out before the fork's commit");
    assert!(
        up.requests()[2]
            .line
            .starts_with("POST /v1/messages/count_tokens"),
        "the third post is the count"
    );
    while let Ok(event) = events.try_recv() {
        if let RouterEvent::Decision { summary, .. } = event {
            assert_ne!(summary.kind, "held", "a count is never held");
        }
    }
    drain(count).await;
    up.release();
    drain(answered(fork).await).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_in_place_after_the_wait_dials_with_no_second_wait() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", gated_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    // The turn's first pass reads a throttle that clears inside the pool's retry bound, so it waits it out on b.
    up.push_credential_rule(B, "/v1/messages", json_429_retry_after("1"));
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let hold = next_held(&mut events)
        .await
        .outcome
        .hold
        .expect("a held outcome");
    assert_eq!(hold.reason, HoldReason::Rewrite);
    tokio::time::sleep(Duration::from_secs(1)).await;
    still_held(&up, &turn, 2);
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    let turn = answered(turn).await;
    let o = outcome(&turn);
    assert_eq!(
        (turn.status().as_u16(), o.account_id.as_str()),
        (200, "b"),
        "retried in place on b"
    );
    assert_eq!(posts(&up), 4, "the throttled pass and its retry");
    let waits = waits_of(&turn);
    assert_eq!(
        waits
            .iter()
            .map(|(reason, _)| reason.as_str())
            .collect::<Vec<_>>(),
        ["rewrite"],
        "one wait, before the first pass"
    );
    assert!(waits[0].1 >= 1_000, "the wait ran to the bound: {waits:?}");
    drain(turn).await;
    up.release();
    drain(answered(fork).await).await;
    up.shutdown().await;
}

/// The decision of `request_id` that its wait led to, the first after its held one, which carries the wait in its
/// holds.
async fn after_the_wait(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> DecisionRecord {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the decision after the wait arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if record.request.request_id == request_id && summary.kind != "held" {
                return std::sync::Arc::unwrap_or_clone(record);
            }
        }
    }
}

/// Whether the last wait the decision carries ran out, with its reason.
fn last_wait(record: &DecisionRecord) -> Option<(HoldReason, bool)> {
    record
        .request
        .holds
        .last()
        .map(|hold| (hold.reason, hold.ran_out))
}

#[tokio::test]
async fn the_first_requests_retry_in_place_keeps_its_waiters_until_the_retrys_head() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    // The fork's first pass on b reads a throttle, which the gate holds back until the turn waits and which the fork
    // waits out on b, and its retry's head commits at the release.
    up.push_credential_rule(
        B,
        "/v1/messages",
        Respond::Gated(Box::new(json_429_retry_after("1"))),
    );
    up.push_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    // A cap long enough to hold the turn through the throttle and the retry's delay.
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.slow_down.slow_down_hold_seconds = 10;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    let fork_id = moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let held = next_held(&mut events).await;
    let hold = held.outcome.hold.expect("a held outcome");
    assert_eq!(
        (hold.reason, hold.waits_for.as_deref(), hold.until),
        (
            HoldReason::Rewrite,
            Some(fork_id.as_str()),
            Some(NOW + 30_000)
        ),
        "the turn waits for the fork's head before the fork's throttle comes"
    );
    still_held(&up, &turn, 2);
    up.release_one().await;
    // The throttle's head answers nothing for the fork's leg, so the turn stays held while the fork waits out the
    // delay on b and dials its retry.
    wait_for(Duration::from_secs(5), || (posts(&up) == 3).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    still_held(&up, &turn, 3);
    // The turn was held before the 429, through it and through the retry's delay, and the retry's commit releases it.
    // The clock steps past the cooldown the throttle set on the relay's clock, so the selection after the wait meets
    // none.
    h.clock.store(NOW + 22_500, Ordering::SeqCst);
    up.release();
    let released = tokio::time::Instant::now();
    let turn = answered(turn).await;
    assert!(
        released.elapsed() <= TICK,
        "the retry's commit releases the turn"
    );
    assert_eq!(turn.status(), 200);
    assert_eq!(posts(&up), 4);
    assert_eq!(
        waits_of(&turn)
            .iter()
            .map(|(reason, _)| reason.as_str())
            .collect::<Vec<_>>(),
        ["rewrite"]
    );
    let after = after_the_wait(&mut events, &held.request.request_id).await;
    assert_eq!(
        last_wait(&after),
        Some((HoldReason::Rewrite, false)),
        "the retry's head ended the wait inside its bound"
    );
    drain(turn).await;
    drain(answered(fork).await).await;
    up.shutdown().await;
}

#[tokio::test]
async fn the_forks_commit_alone_releases_the_waiter_and_an_error_event_releases_it_too() {
    // The fork's commit, with the clock standing under the bound.
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    next_held(&mut events).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    still_held(&up, &turn, 2);
    up.release();
    let released = tokio::time::Instant::now();
    let turn = answered(turn).await;
    assert!(
        released.elapsed() <= TICK,
        "the commit alone releases the turn"
    );
    let waits = waits_of(&turn);
    assert!(
        (500..u64::from(MOVED_HOLD_SECONDS) * 1_000).contains(&waits[0].1),
        "{waits:?}"
    );
    drain(turn).await;
    drain(answered(fork).await).await;
    tokio::time::resume();
    up.shutdown().await;
    drop(h);

    // An error event ahead of `message_start` on the fork's leg: the fork fails over to c, and so does the turn.
    let up = FakeUpstream::start(vec![]).await;
    let far = (NOW + 22_000) / 1_000 + 10 * 3_600;
    let rejected = json_429_unified(far, "five_hour");
    let Respond::Json {
        headers: rejected_headers,
        ..
    } = rejected.clone()
    else {
        unreachable!()
    };
    up.add_credential_rule(B, "/v1/messages", rejected);
    up.push_credential_rule(
        B,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: rejected_headers,
            events: vec![ev(
                "error",
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            )],
        },
    );
    up.add_credential_rule(C, "/v1/messages", json_200());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1), oauth("c", C, 2)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    let fork_id = moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    // a's spent window cools it, so a failover off b has c alone to land on.
    h.router
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: phoebis_store::CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: NOW + 10 * 3_600_000,
                set_at: NOW,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
                spend_share: None,
            },
            pin: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let held = next_held(&mut events).await;
    let hold = held.outcome.hold.clone().expect("a held outcome");
    assert_eq!(hold.waits_for.as_deref(), Some(fork_id.as_str()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    still_held(&up, &turn, 2);
    up.release();
    let released = tokio::time::Instant::now();
    let turn = answered(turn).await;
    // The error event fails the fork over, so its leg on b answers nothing and the turn waits for the fork's next
    // leg, which c answers at once.
    assert!(
        released.elapsed() <= TICK,
        "the turn goes out within a tick of the error event"
    );
    let fork = answered(fork).await;
    let waits = waits_of(&turn);
    assert_eq!(waits[0].0, "rewrite");
    assert!(
        (500..u64::from(MOVED_HOLD_SECONDS) * 1_000).contains(&waits[0].1),
        "the wait ran to c's answer on the fork's next leg: {waits:?}"
    );
    assert_eq!(
        last_wait(&after_the_wait(&mut events, &held.request.request_id).await),
        Some((HoldReason::Rewrite, false)),
        "the fork's answer ended the wait inside its bound"
    );
    assert_eq!(
        (
            turn.status().as_u16(),
            outcome(&turn).account_id.as_str(),
            fork.status().as_u16(),
            outcome(&fork).account_id.as_str()
        ),
        (200, "c", 200, "c"),
        "both end on the sibling"
    );
    assert_eq!(
        credentials_seen(&up)
            .iter()
            .filter(|c| c.as_str() == C)
            .count(),
        2
    );
    drain(turn).await;
    drain(fork).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_canceled_first_request_frees_the_waiter_at_once() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let held = next_held(&mut events).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    still_held(&up, &turn, 2);
    fork.abort();
    let aborted = tokio::time::Instant::now();
    let turn = answered(turn).await;
    assert!(
        aborted.elapsed() <= TICK,
        "the fork's cancel releases the turn"
    );
    let waits = waits_of(&turn);
    assert!(
        (500..u64::from(MOVED_HOLD_SECONDS) * 1_000).contains(&waits[0].1),
        "the wait ran to the fork's cancel, under the bound: {waits:?}"
    );
    assert_eq!(
        last_wait(&after_the_wait(&mut events, &held.request.request_id).await),
        Some((HoldReason::Rewrite, false)),
        "the cancel ended the wait inside its bound"
    );
    drain(turn).await;
    let third = answered(sent(&h, request_for(KEY, MODEL, "s1"))).await;
    assert!(
        outcome(&third).held.is_none(),
        "the canceled leg's report cleared the mark"
    );
    drain(third).await;
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn the_heads_map_is_empty_once_both_requests_ended() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    next_held(&mut events).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    up.release();
    drain(answered(turn).await).await;
    drain(answered(fork).await).await;
    wait_for(Duration::from_secs(5), || {
        (h.relay.heads_pending() == 0).then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn the_held_decisions_record_names_the_rewrites_hold() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    up.push_credential_rule(B, "/v1/messages", stalled_stream());
    let h = moved_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let fork = sent(&h, fork_of("s1"));
    let fork_id = moved_to(&mut events, "b").await;
    wait_for(Duration::from_secs(5), || (posts(&up) == 2).then_some(())).await;
    let turn = sent(&h, request_for(KEY, MODEL, "s1"));
    let record = next_held(&mut events).await;
    let hold = record.outcome.hold.clone().expect("a held outcome");
    assert_eq!(
        (
            hold.reason,
            hold.waits_for.as_deref(),
            hold.until,
            hold.bound_ms
        ),
        (
            HoldReason::Rewrite,
            Some(fork_id.as_str()),
            Some(NOW + 22_000),
            i64::from(MOVED_HOLD_SECONDS) * 1_000
        )
    );
    let value = serde_json::to_value(&record).unwrap();
    let held = &value["outcome"]["hold"];
    assert_eq!(held["waits_for"], serde_json::json!(fork_id));
    for figure in ["factor", "speed", "stretch_ms", "threshold_ms", "cycle_ms"] {
        assert!(held[figure].is_null(), "{figure} is null: {held}");
    }
    up.release();
    drain(answered(turn).await).await;
    drain(answered(fork).await).await;
    tokio::time::resume();
    up.shutdown().await;
    drop(h);

    // A slow-down's hold names no request it waits for, so its record carries no member.
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = slowed_harness(&up).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let waiting = sent(&h, request_for(KEY, MODEL, "s1"));
    let record = next_held(&mut events).await;
    assert_eq!(
        record.outcome.hold.as_ref().map(|hold| hold.reason),
        Some(HoldReason::SlowDown)
    );
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value["outcome"]["hold"].get("waits_for").is_none(),
        "the member is left out: {}",
        value["outcome"]["hold"]
    );
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    tokio::time::resume();
    drain(answered(waiting).await).await;
    up.shutdown().await;
}
