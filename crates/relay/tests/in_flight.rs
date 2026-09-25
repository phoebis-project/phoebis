//! The in-flight counts: an account ticket per wire attempt, a key permit per request, both held until the
//! response ends, and what a selection or an admission does when one is at its cap.

mod common;

use std::time::{Duration, Instant};

use phoebis_bus::{Bus, Event};
use phoebis_store::Failover;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
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
async fn a_key_past_its_cap_is_refused_before_selection_with_a_retry_after_and_the_row_names_the_code(
) {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            key_in_flight_cap: Some(1),
            ..PoolSpec::default()
        },
        bus,
    )
    .await;
    let held = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(held.status().as_u16(), 200);
    let refused = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 429);
    assert_eq!(refused.headers()["retry-after"], "10");
    assert_eq!(refused.headers()["x-phoebis-error-code"], "key_in_flight_cap");
    assert!(
        refused.headers().get("x-should-retry").is_none(),
        "the client's own retry policy decides"
    );
    let refused_id = outcome(&refused).request_id;
    let body = drain(refused).await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "rate_limit_error");
    assert_eq!(posts(&up), 1, "the refusal came before any selection");
    let mut events = Vec::new();
    wait_for(Duration::from_secs(5), || {
        while let Ok(e) = sub.events.try_recv() {
            events.push(e);
        }
        events
            .iter()
            .any(|e| matches!(e, Event::RequestComplete(c) if c.request_id == refused_id))
            .then_some(())
    })
    .await;
    let observed = events
        .iter()
        .find_map(|e| match e {
            Event::RequestObserved(o) if o.request_id == refused_id => Some(o),
            _ => None,
        })
        .expect("the refused request is observed");
    assert!(
        observed.body.starts_with(b"{"),
        "the observation carries the body as it came"
    );
    let complete = events
        .iter()
        .find_map(|e| match e {
            Event::RequestComplete(c) if c.request_id == refused_id => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (complete.status, complete.error_code.as_deref()),
        (429, Some("key_in_flight_cap"))
    );
    drop(held);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn two_admissions_released_together_against_a_key_capped_at_one_admit_exactly_one() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            key_in_flight_cap: Some(1),
            ..PoolSpec::default()
        },
    )
    .await;
    let (one, two) = tokio::join!(
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s2")),
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    let mut statuses = [one.status().as_u16(), two.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 429], "exactly one admission");
    assert_eq!(posts(&up), 1);
    let (admitted, refused) = if one.status() == 200 {
        (one, two)
    } else {
        (two, one)
    };
    assert_eq!(refused.headers()["x-phoebis-error-code"], "key_in_flight_cap");
    up.release();
    drain(admitted).await;
    let third = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s3"))
        .await
        .unwrap();
    assert_eq!(
        third.status().as_u16(),
        200,
        "the permit came back with the body's end"
    );
    drain(third).await;
    assert_eq!(posts(&up), 2);
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_in_place_keeps_the_one_key_permit() {
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            key_in_flight_cap: Some(1),
            ..PoolSpec::default()
        },
    )
    .await;
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || (posts(&up) == 1).then_some(())).await;
    let refused = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "key_in_flight_cap"),
        "the request sleeping in its retry still holds the key's one slot"
    );
    assert_eq!(h.router.in_flight().key("desktop"), 1);
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
