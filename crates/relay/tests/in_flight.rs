//! The in-flight counts: an account ticket per wire attempt, a key permit per request, both held until the
//! response ends, and what a selection does when an account is at its cap.

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_routing::{DecisionRecord, DecisionSummary, RouterEvent};
use phoebis_store::Failover;
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
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
        .header("content-type", "application/json");
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
    let started = Instant::now();
    let pinned = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
    )
    .await
    .expect("the pinned session is never held")
    .unwrap();
    assert!(started.elapsed() < HELD_FOR);
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
    // b's stream ends on its own before the bound, but a's never does: a held request waits on a alone.
    up.add_credential_rule(
        B,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: vec![],
            events: vec![message_start(), message_stop()],
            delay_between: Duration::from_millis(600),
        },
    );
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
    let started = Instant::now();
    let third = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s3")),
    )
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
    drop((on_a, on_b, third));
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
async fn a_hold_that_times_out_with_no_dispatch_names_its_wait_on_the_completion() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 1),
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
    // The paused clock runs the one-second bound out as soon as nothing else is left to run, so the wait is
    // measured on the runtime's clock rather than slept through.
    tokio::time::pause();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    tokio::time::resume();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "accounts_at_cap"),
        "past the bound the one account is attempted and the pool is at its cap"
    );
    assert_eq!(posts(&up), 1, "nothing was dialled for the held request");
    let held = outcome(&resp)
        .held
        .expect("the outcome names the hold it waited through");
    assert_eq!(held.waited_on, ["a"]);
    assert!(
        (900..=2_000).contains(&held.held_ms),
        "the wait ran the one-second bound out: {}",
        held.held_ms
    );
    let complete = completion_of(&mut sub, &outcome(&resp).request_id).await;
    assert_eq!(
        complete
            .held
            .as_deref()
            .map(|h| serde_json::from_str::<serde_json::Value>(h).unwrap()),
        Some(serde_json::to_value(&held).unwrap()),
        "the completion carries the hold although no attempt followed it"
    );
    drop(first);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_whose_every_hold_ran_out_answers_at_cap_naming_both_waits_on_the_completion() {
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
        "every account was held for and ran out"
    );
    assert_eq!(posts(&up), 2, "nothing was dialled for the held request");
    let held = outcome(&resp)
        .held
        .expect("the outcome names the holds it waited through");
    assert_eq!(held.waited_on, ["a", "b"]);
    assert!(
        (1_900..=4_000).contains(&held.held_ms),
        "both waits ran the one-second bound out: {}",
        held.held_ms
    );
    let complete = completion_of(&mut sub, &outcome(&resp).request_id).await;
    assert_eq!(
        complete
            .held
            .as_deref()
            .map(|h| serde_json::from_str::<serde_json::Value>(h).unwrap()),
        Some(serde_json::to_value(&held).unwrap()),
        "the completion carries both waits"
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

#[tokio::test]
async fn a_request_that_dialled_before_its_hold_ran_out_answers_accounts_unavailable() {
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
    assert_eq!(h.router.in_flight().account("b"), 1, "b is at its cap");
    let started = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s2")),
    )
    .await
    .expect("the bound ends the hold")
    .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "a was dialled and refused, then the request was held for the one-second bound on b: {:?}",
        started.elapsed()
    );
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable"),
        "a request that reached an account is no longer a pool merely at its cap"
    );
    assert_eq!(credentials_seen(&up), [B, KEYED]);
    drop(first);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn two_placements_woken_by_one_freed_slot_never_pass_the_accounts_cap() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
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
    assert_eq!(h.router.in_flight().account("a"), 1, "a is at its cap");
    // Both fresh placements are held on a. Ending the one stream frees one slot and wakes both waiters, which
    // select again in the same turn: the account is one below its cap for both of them.
    let (two, three, ()) = tokio::join!(
        h.app.clone().oneshot(request_for(KEY, MODEL, "s2")),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s3")),
        async {
            tokio::time::sleep(HELD_FOR).await;
            assert_eq!(posts(&up), 1, "both placements are held");
            up.release_one().await;
        },
    );
    let (two, three) = (two.unwrap(), three.unwrap());
    let mut statuses = [two.status().as_u16(), three.status().as_u16()];
    statuses.sort();
    assert_eq!(
        statuses,
        [200, 429],
        "one takes the freed slot; the other, already held for on a, passes it over and is answered at the cap"
    );
    let at_cap = if two.status() == 429 { &two } else { &three };
    assert_eq!(
        (
            at_cap.headers()["x-phoebis-error-code"].to_str().unwrap(),
            at_cap.headers()["retry-after"].to_str().unwrap()
        ),
        ("accounts_at_cap", "1")
    );
    assert_eq!(
        (posts(&up), h.router.in_flight().account("a")),
        (2, 1),
        "the cap held: one stream open on a"
    );
    drop((first, two, three));
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
    // Both placements wait on a. Ending the one stream wakes both in the same turn; a freed slot is read, not
    // reserved, so the placement whose selection runs second finds it taken by the first.
    let (two, three, ()) = tokio::join!(
        h.app.clone().oneshot(request_for(KEY, MODEL, "s2")),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s3")),
        async {
            tokio::time::sleep(HELD_FOR).await;
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
                            scope: "model:claude-unused".into(),
                            until: i64::MAX,
                            set_at: 0,
                            reason: "rate_limit".into(),
                            window: "five_hour".into(),
                            source_request_id: None,
                            incarnation: 1,
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
async fn a_cancelled_leg_releases_its_ticket() {
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
            body(BodyFixture::CountOauth),
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
            body(BodyFixture::CountOauth),
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
    h.store.set_enabled("a", false).unwrap();
    h.router.reload().await.unwrap();
    let started = Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert!(
        started.elapsed() < HELD_FOR,
        "answered without a hold: {:?}",
        started.elapsed()
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
