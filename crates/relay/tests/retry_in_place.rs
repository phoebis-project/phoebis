//! A short throttle on a portable request is waited out on the same account once, with the cooldown row still
//! recorded for the siblings; anything longer, or a request bound to its account, takes the fail-over path.

mod common;

use std::time::{Duration, Instant};

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_routing::RouterEvent;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const MODEL: &str = "claude-sonnet-4-5";

fn messages(respond: Respond) -> Vec<Rule> {
    vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }]
}

/// An OAuth 429 with no unified headers: the upstream's `Retry-After` is the only deadline it names.
fn json_429_retry_after(seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), seconds.into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_vec(),
    }
}

fn json_200_with_usage() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":100,"output_tokens":20}}"#
            .to_vec(),
    }
}

async fn drain(resp: axum::http::Response<axum::body::Body>) {
    tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap();
}

#[tokio::test]
async fn a_short_retry_after_is_waited_out_on_the_same_account_once() {
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let started = Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let waited = started.elapsed();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!(
        (o.account_id.as_str(), o.attempt, o.error_code),
        ("a", 2, None),
        "two wire attempts, both on a"
    );
    assert_eq!(credentials_seen(&up), [A, A]);
    assert!(
        waited >= Duration::from_millis(950),
        "the Retry-After was waited out: {waited:?}"
    );
    let rows = h.router.cooldowns().await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.account_id.as_str(), r.window.as_str(), r.until))
            .collect::<Vec<_>>(),
        [("a", "retry_after", NOW + 1_000 + 1_000)],
        "the row is still recorded for the siblings"
    );
    drain(resp).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_throttle_on_the_retry_fails_over_or_ends_at_the_pools_exhaustion() {
    // One account and two throttles: one retry in place, then the second throttle fails over, nothing is left,
    // and the pool answers with the cooldown's deadline.
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap(),
            resp.headers()["retry-after"].to_str().unwrap(),
            outcome(&resp).attempt
        ),
        (429, "accounts_exhausted", "2", 2)
    );
    assert_eq!(credentials_seen(&up), [A, A], "one retry in place, no more");
    up.shutdown().await;

    // A sibling: the second throttle fails over.
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            outcome(&resp).account_id.as_str(),
            outcome(&resp).attempt
        ),
        (200, "b", 3)
    );
    assert_eq!(credentials_seen(&up), [A, A, B]);
    drain(resp).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_pinned_sessions_retry_in_place_keeps_its_pin_and_a_sibling_sees_the_cooldown() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&first).account_id, "a");
    drain(first).await;
    let pin = h.router.pins().await.unwrap()[0].clone();
    assert_eq!(pin.burn_at, None, "the first answer carried no usage");
    let mut events = h.router.subscribe_events();
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200_with_usage());
    let app = h.app.clone();
    let retried =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.router.cooldowns().await.unwrap();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        (rows[0].account_id.as_str(), rows[0].window.as_str()),
        ("a", "retry_after")
    );
    assert_eq!(
        h.router.in_flight().key("desktop"),
        1,
        "the waiting request holds its key permit"
    );
    let sibling = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&sibling).account_id,
        "b",
        "a second session sees a's row while the first waits"
    );
    drain(sibling).await;
    assert_eq!(h.router.in_flight().key("desktop"), 1);
    let resp = tokio::time::timeout(Duration::from_secs(5), retried)
        .await
        .expect("the retry lands")
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "a", 2)
    );
    drain(resp).await;
    let kept = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.session_id == "s1" && p.burn_at > pin.burn_at)
    })
    .await;
    assert_eq!(
        (kept.account_id.as_str(), kept.epoch),
        ("a", pin.epoch),
        "the pin stayed where it was"
    );
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                &event,
                RouterEvent::PinMigrated { session_id, .. } if session_id == "s1"
            ),
            "no pin_migrated for the waiting session: {event:?}"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_after_over_the_bound_or_a_non_portable_request_fails_over_as_today() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&first).account_id, "a");
    drain(first).await;
    let pinned = h.router.pins().await.unwrap()[0].clone();
    // Past the ten-second bound: the pin leaves a and the sibling takes the request at once.
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("30"));
    let started = Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(900), "no wait");
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 2)
    );
    drain(resp).await;
    let pins = h.router.pins().await.unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].account_id, "b", "the pin was invalidated and moved");
    assert_ne!(pins[0].epoch, pinned.epoch);
    up.shutdown().await;

    // The same one-second throttle a portable request waits out on its account...
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    drain(
        h.app
            .clone()
            .oneshot(request_for(KEY, MODEL, "s1"))
            .await
            .unwrap(),
    )
    .await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "a", 2)
    );
    drain(resp).await;
    // ...is the client's own answer on a request bound to its account. Past the row the retry left.
    h.clock
        .store(NOW + 5_000, std::sync::atomic::Ordering::SeqCst);
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    let started = Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(900), "no wait");
    assert_eq!(resp.status(), 429);
    assert_eq!(outcome(&resp).attempt, 1);
    assert_eq!(credentials_seen(&up), [A, A, A, A]);
    up.shutdown().await;
}
