//! A body stream that ends before it is complete is the caller leaving, not the caller sending too much.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use futures::stream;
use phoebis_profile::fixtures::{body, BodyFixture};
use phoebis_routing::{
    Report, RouterConfig, RouterEvent, SharePricer, UtilizationSample, WindowReading, WindowSource,
    FIVE_HOUR,
};
use phoebis_store::SubscriptionRow;
use tokio::sync::broadcast;
use tower::ServiceExt;

fn ok_rule() -> Rule {
    Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }
}

#[tokio::test]
async fn a_body_stream_that_errors_mid_read_is_answered_as_incomplete_not_too_large() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    let partial = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"#;
    let chunks = stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::from_static(partial.as_bytes())),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "the caller hung up",
        )),
    ]);
    let request = Request::post("/v1/messages")
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("user-agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", fresh_session())
        .header("x-claude-code-request-class", "main")
        .body(Body::from_stream(chunks))
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 400);
    let code = response
        .headers()
        .get("x-phoebis-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(code, "request_incomplete");
    assert!(up.requests().is_empty(), "the body never reached admission");
    up.shutdown().await;
}

/// The rate-limit tier the priced harness puts every account on, which its pricer keys on.
const TIER: &str = "default_claude_max_20x";

/// A ten-millionth of either window per token of input or cache write on `TIER`, so a body of `n` tokens books
/// `n / 10_000_000` and a usage of `n` input tokens charges the same.
fn tenth_per_million() -> SharePricer {
    SharePricer::new(|_, _, tier, _, usage, _| {
        (tier == TIER).then(|| (usage.input + usage.write_5m) as f64 / 10_000_000.0)
    })
}

/// `harness_with_router_config` over `accounts` on `TIER`, each reading 0.5 on its 5-hour window a second before the
/// harness clock, so a booking and a charge at the clock move the prediction the test reads.
async fn priced_harness(up: &FakeUpstream, accounts: &[TestAccount]) -> Harness {
    let h = harness_with_router_config(
        up,
        accounts,
        PoolSpec::default(),
        RouterConfig {
            share: tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .await;
    for account in accounts {
        h.store
            .set_profile(
                account.id,
                "{}",
                &SubscriptionRow {
                    tier: Some(TIER.into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
    }
    h.router.reload().await.unwrap();
    for account in accounts {
        h.router
            .report(Report::Sample {
                account_id: account.id.into(),
                sample: UtilizationSample::default().with(
                    FIVE_HOUR,
                    WindowReading {
                        utilization: 0.5,
                        reset: None,
                        observed_at: NOW - 1_000,
                        source: WindowSource::Poll,
                        plan: None,
                        tier: None,
                    },
                ),
                incarnation: 1,
                source_request_id: None,
            })
            .await;
    }
    h
}

/// The rewrite the fixture body books on `TIER`: its bytes over four, the relay's estimate of its prompt tokens, each
/// a ten-millionth of the window.
fn fixture_rewrite() -> f64 {
    (body(BodyFixture::Oauth).len() / 4) as f64 / 10_000_000.0
}

/// The 5-hour prediction of `account`, as the router answers it now.
async fn five_hour(h: &Harness, account: &str) -> f64 {
    h.router
        .samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == account)
        .and_then(|(_, sample)| sample.windows.get(FIVE_HOUR).map(|r| r.utilization))
        .expect("a reading")
}

/// The expiries among the router's events up to and including the first one, as `(request, attempt, cause)`,
/// under a five-second bound.
async fn expiries_until_the_first(
    events: &mut broadcast::Receiver<RouterEvent>,
) -> Vec<(String, u32, &'static str)> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the router's event arrives")
            .expect("not lagged");
        if let RouterEvent::ReservationExpired {
            request_id,
            attempt,
            cause,
            ..
        } = event
        {
            return vec![(request_id, attempt, cause)];
        }
    }
}

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

/// A committed stream whose first event names no usage yet, held by the fake after that event until the test
/// releases it, so the client can leave with the head read and no usage reported.
fn held_before_its_usage() -> Respond {
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![
            ev(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
            ),
            ev("message_stop", r#"{"type":"message_stop"}"#),
        ],
    }
}

#[tokio::test]
async fn a_canceled_request_sends_one_ended_report_with_the_cause_canceled() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: held_before_its_usage(),
    }])
    .await;
    let h = priced_harness(&up, &[oauth("a", "sk-ant-oat01-AAAA", 0)]).await;
    let mut events = h.router.subscribe_events();
    let response = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let id = outcome(&response).request_id;
    let booked = five_hour(&h, "a").await;
    assert!(
        (booked - (0.5 + fixture_rewrite())).abs() < 1e-9,
        "the body's estimate stands in the prediction while the answer streams: {booked}"
    );
    // The client leaves with the head read and the body unread: the stream ends with no usage.
    drop(response);
    assert_eq!(
        expiries_until_the_first(&mut events).await,
        [(id, 1, "canceled")]
    );
    assert!(
        (five_hour(&h, "a").await - 0.5).abs() < 1e-9,
        "the booking left and nothing was charged"
    );
    up.release();
    up.shutdown().await;
}
