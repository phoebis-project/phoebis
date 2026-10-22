//! A short throttle on a portable request is waited out on the same account once, with the cooldown row still
//! recorded for the siblings; anything longer, or a request bound to its account, takes the fail-over path.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_profile::fixtures::{body, BodyFixture};
use phoebis_routing::{
    DecisionRecord, DecisionSummary, HoldOutcome, Report, RouterConfig, RouterEvent, SharePricer,
    UtilizationSample, WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
};
use phoebis_store::{CooldownRow, Failover, SlowDown, SubscriptionRow};
use tokio::sync::broadcast;
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

/// An OAuth 429 whose unified headers read the 5-hour window spent with its reset at `reset_secs`: the head sample
/// records the utilization, and the cooldown and the spent rule's wait read the reset.
fn json_429_spent(reset_secs: i64) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![
            (
                "anthropic-ratelimit-unified-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "1.0".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".into(),
                reset_secs.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-reset".into(),
                reset_secs.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-representative-claim".into(),
                "five_hour".into(),
            ),
        ],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#.to_vec(),
    }
}

async fn drain(resp: axum::http::Response<axum::body::Body>) {
    tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap();
}

/// The router's events up to and including the cache map `request_id`'s answer wrote, under a five-second bound.
async fn events_through_cache_map(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> Vec<RouterEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the router's event arrives")
            .expect("not lagged");
        let done = matches!(&event, RouterEvent::CacheMapWritten { request_id: id, .. } if id == request_id);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

/// The decisions among `events` for `request_id` as `(seq, attempt, kind)`, in the order the router made them.
fn decision_facts_of(events: &[RouterEvent], request_id: &str) -> Vec<(u32, u32, &'static str)> {
    events
        .iter()
        .filter_map(|event| match event {
            RouterEvent::Decision {
                summary, record, ..
            } if record.request.request_id == request_id => Some(facts(summary, record)),
            _ => None,
        })
        .collect()
}

fn facts(summary: &DecisionSummary, record: &DecisionRecord) -> (u32, u32, &'static str) {
    (record.request.seq, record.request.attempt, summary.kind)
}

/// The `(seq, attempt)` of the cache map `request_id`'s answer wrote.
fn cache_map_of(events: &[RouterEvent], request_id: &str) -> Option<(u32, u32)> {
    events.iter().find_map(|event| match event {
        RouterEvent::CacheMapWritten {
            request_id: id,
            seq,
            attempt,
            ..
        } if id == request_id => Some((*seq, *attempt)),
        _ => None,
    })
}

/// The wire attempts the bus announced for `request_id`, in order.
async fn selected_attempts(sub: &mut phoebis_bus::Subscriber, request_id: &str) -> Vec<u8> {
    drain_until_complete(sub)
        .await
        .iter()
        .filter_map(|event| match event {
            Event::AccountSelected(selected) if selected.request_id == request_id => {
                Some(selected.attempt)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_completed_first_request_s_decision_names_attempt_one_and_its_cache_map_its_seq() {
    let up = FakeUpstream::start(messages(json_200_with_usage())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = outcome(&resp).request_id;
    drain(resp).await;
    let seen = events_through_cache_map(&mut events, &id).await;
    assert_eq!(decision_facts_of(&seen, &id), [(1, 1, "placed")]);
    assert_eq!(cache_map_of(&seen, &id), Some((1, 1)));
    assert_eq!(selected_attempts(&mut sub, &id).await, [1]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_in_place_that_succeeds_reports_its_cache_map_under_the_selecting_seq() {
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200_with_usage());
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut events = h.router.subscribe_events();
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
    let seen = events_through_cache_map(&mut events, &o.request_id).await;
    assert_eq!(
        decision_facts_of(&seen, &o.request_id),
        [(1, 1, "placed")],
        "the retry in place dispatched again without a new selection"
    );
    assert_eq!(
        cache_map_of(&seen, &o.request_id),
        Some((1, 2)),
        "the rows reach the selecting decision from the pass that answered"
    );
    assert_eq!(selected_attempts(&mut sub, &o.request_id).await, [1, 2]);
    up.shutdown().await;
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
async fn a_spent_window_resetting_inside_the_bound_is_waited_out_in_place_and_the_pin_renews_there()
{
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
    assert_eq!(outcome(&first).account_id, "a", "s1 pins to a");
    drain(first).await;
    // Another session's 429 reads a's 5-hour window spent with its reset two seconds out, and that request fails
    // over to b.
    up.push_credential_rule(A, "/v1/messages", json_429_spent(NOW / 1_000 + 2));
    let spent = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        (outcome(&spent).account_id.as_str(), outcome(&spent).attempt),
        ("b", 2)
    );
    drain(spent).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    // The hold's decision goes out before the wait starts, so the clock is stepped while the request waits: first to
    // a point past the reset but inside the cooldown's one-second margin, where the account is still cooled and a
    // wait that ended would place the request afresh, then past the margin.
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the hold's decision arrives")
            .expect("not lagged");
        let held =
            matches!(&event, RouterEvent::Decision { summary, .. } if summary.kind == "held");
        seen.push(event);
        if held {
            break;
        }
    }
    h.clock.store(NOW + 2_500, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    h.clock.store(NOW + 3_000, Ordering::SeqCst);
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the request lands after the reset")
        .unwrap();
    tokio::time::resume();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "a", 1),
        "one wire attempt, on the account the pin stayed on"
    );
    drain(resp).await;
    seen.extend(events_through_cache_map(&mut events, &o.request_id).await);
    assert_eq!(
        decision_facts_of(&seen, &o.request_id),
        [(1, 1, "held"), (2, 1, "renewed")],
        "the hold and the renewal are two decisions of one wire attempt"
    );
    let records: Vec<&DecisionRecord> = seen
        .iter()
        .filter_map(|event| match event {
            RouterEvent::Decision { record, .. } if record.request.request_id == o.request_id => {
                Some(&**record)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        records[0].outcome.hold.as_ref().map(|hold| (
            hold.account_id.as_str(),
            hold.reason,
            hold.until
        )),
        Some(("a", "reset", Some(NOW + 3_000))),
        "the hold runs to the cooldown's end, a margin past the reset"
    );
    assert_eq!(
        records[1]
            .request
            .holds
            .iter()
            .map(|hold| (
                hold.account_id.as_str(),
                hold.reason,
                hold.until,
                hold.ran_out
            ))
            .collect::<Vec<_>>(),
        [("a", "reset", Some(NOW + 3_000), false)],
        "the renewal's decision carries the one wait with its reason"
    );
    assert_eq!(credentials_seen(&up), [A, A, B, A]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_reset_that_moves_during_the_wait_moves_the_pin_rather_than_holding_the_request_again() {
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
    assert_eq!(outcome(&first).account_id, "a", "s1 pins to a");
    drain(first).await;
    up.push_credential_rule(A, "/v1/messages", json_429_spent(NOW / 1_000 + 2));
    let spent = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(outcome(&spent).account_id, "b");
    drain(spent).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the hold's decision arrives")
            .expect("not lagged");
        let held =
            matches!(&event, RouterEvent::Decision { summary, .. } if summary.kind == "held");
        seen.push(event);
        if held {
            break;
        }
    }
    // A poll during the wait reads the window still spent, its reset a minute out: past the hold's end and inside the
    // bound of the selection that follows it, so without the one-hold rule the request would hold on a again. The
    // head's reading is dated by the upstream's clock, so the poll's is dated just after it to replace it.
    let read_at = h
        .router
        .samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == "a")
        .and_then(|(_, sample)| sample.windows.get(FIVE_HOUR).map(|r| r.observed_at))
        .expect("the 429 recorded a's reading");
    let reading = |utilization: f64, reset: i64| WindowReading {
        utilization,
        reset: Some(reset),
        observed_at: read_at + 1,
        source: WindowSource::Poll,
    };
    h.router
        .report(Report::Sample {
            account_id: "a".into(),
            sample: UtilizationSample::default().with(FIVE_HOUR, reading(1.0, NOW + 63_000)),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    // The same poll reads b's allowance, which the spent rule has to know before it moves the pin there.
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
    h.clock.store(NOW + 3_000, Ordering::SeqCst);
    // The decision after the wait is read as it lands, so a second hold fails the case rather than spinning it.
    let second = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the decision after the wait arrives")
            .expect("not lagged");
        let kind = match &event {
            RouterEvent::Decision { summary, .. } => Some(summary.kind),
            _ => None,
        };
        seen.push(event);
        if let Some(kind) = kind {
            break kind;
        }
    };
    assert_eq!(
        second, "moved",
        "the request held once and moves on its next selection"
    );
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the request lands on the sibling")
        .unwrap();
    tokio::time::resume();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 1),
        "one wire attempt, on the sibling the pin moved to"
    );
    drain(resp).await;
    seen.extend(events_through_cache_map(&mut events, &o.request_id).await);
    assert_eq!(
        decision_facts_of(&seen, &o.request_id),
        [(1, 1, "held"), (2, 1, "moved")],
        "the request's decisions stay at one hold"
    );
    assert_eq!(credentials_seen(&up), [A, A, B, B]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_reset_wait_the_request_deadline_would_outlast_is_refused_and_the_sibling_tried() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", json_200());
    // 35 s is five past the margin, so the request's deadline is five seconds after its admission on the harness's
    // clock, which stands still.
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(35),
            ..default_limits()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&first).account_id, "a", "s1 pins to a");
    drain(first).await;
    // A poll reads b's allowance, which the spent rule has to know before it moves the pin there.
    let reading = |utilization: f64, reset: i64| WindowReading {
        utilization,
        reset: Some(reset),
        observed_at: NOW,
        source: WindowSource::Poll,
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
    // Another session's 429 reads a's 5-hour window spent with its reset ten seconds out: inside the retry bound and
    // past the deadline of a request admitted now.
    up.push_credential_rule(A, "/v1/messages", json_429_spent(NOW / 1_000 + 10));
    let spent = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(outcome(&spent).account_id, "b");
    drain(spent).await;
    let mut events = h.router.subscribe_events();
    // Bounded on the wall clock: a wait for the reset would run on the harness's clock, which never reaches it.
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
    )
    .await
    .expect("the request answers without waiting for the reset")
    .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 1),
        "the pin moves to the sibling rather than waiting past the deadline"
    );
    drain(resp).await;
    let seen = events_through_cache_map(&mut events, &o.request_id).await;
    assert_eq!(
        decision_facts_of(&seen, &o.request_id),
        [(1, 1, "held"), (2, 1, "moved")],
        "the hold is answered, its wait refused, and the sibling tried"
    );
    assert_eq!(credentials_seen(&up), [A, A, B, B]);
    up.shutdown().await;
}

/// The two-account harness under a 20-second head timeout, at or under the margin, so a request has no deadline and
/// its waits are bounded by the head timeout itself. `s1` is pinned to `a`, `b` is polled with room, and another
/// session's 429 reads `a`'s 5-hour window spent with its reset `reset_in` seconds out, which fails that session over
/// to `b`. The harness's clock stands at `NOW`.
async fn spent_under_a_short_head_timeout(up: &FakeUpstream, reset_in: i64) -> Harness {
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness_with_limits(
        up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(20),
            ..default_limits()
        },
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&first).account_id, "a", "s1 pins to a");
    drain(first).await;
    let reading = |utilization: f64, reset: i64| WindowReading {
        utilization,
        reset: Some(reset),
        observed_at: NOW,
        source: WindowSource::Poll,
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
    up.push_credential_rule(A, "/v1/messages", json_429_spent(NOW / 1_000 + reset_in));
    let spent = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(outcome(&spent).account_id, "b");
    drain(spent).await;
    h
}

#[tokio::test]
async fn a_reset_wait_a_short_head_timeout_would_outlast_is_refused_and_the_sibling_tried() {
    let up = FakeUpstream::start(vec![]).await;
    // a's reset 25 s out lies inside the retry bound and past the head timeout of a request admitted now.
    let h = spent_under_a_short_head_timeout(&up, 25).await;
    let mut events = h.router.subscribe_events();
    // Bounded on the wall clock: a wait for the reset would run on the harness's clock, which never reaches it.
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        h.app.clone().oneshot(request_for(KEY, MODEL, "s1")),
    )
    .await
    .expect("the request answers without waiting for the reset")
    .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 1),
        "the pin moves to the sibling rather than waiting past the head timeout"
    );
    drain(resp).await;
    let seen = events_through_cache_map(&mut events, &o.request_id).await;
    assert_eq!(
        decision_facts_of(&seen, &o.request_id),
        [(1, 1, "held"), (2, 1, "moved")],
        "the hold is answered, its wait refused, and the sibling tried"
    );
    assert_eq!(credentials_seen(&up), [A, A, B, B]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_reset_inside_a_short_head_timeout_is_waited_out_in_place_and_the_pin_renews_there() {
    let up = FakeUpstream::start(vec![]).await;
    // a's reset 10 s out, and the cooldown's one-second margin past it, lie inside the head timeout.
    let h = spent_under_a_short_head_timeout(&up, 10).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    // The hold's decision goes out before the wait starts, so the clock is stepped while the request waits, past the
    // reset and the cooldown's margin.
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the hold's decision arrives")
            .expect("not lagged");
        let held =
            matches!(&event, RouterEvent::Decision { summary, .. } if summary.kind == "held");
        seen.push(event);
        if held {
            break;
        }
    }
    h.clock.store(NOW + 11_000, Ordering::SeqCst);
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the request lands after the reset")
        .unwrap();
    tokio::time::resume();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "a", 1),
        "one wire attempt, on the account the pin stayed on"
    );
    drain(resp).await;
    seen.extend(events_through_cache_map(&mut events, &o.request_id).await);
    assert_eq!(
        decision_facts_of(&seen, &o.request_id),
        [(1, 1, "held"), (2, 1, "renewed")],
        "the hold and the renewal are two decisions of one wire attempt"
    );
    assert_eq!(credentials_seen(&up), [A, A, B, A]);
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
    // Past the one-minute bound: the pin leaves a and the sibling takes the request at once.
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("90"));
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

#[tokio::test]
async fn a_throttle_the_request_deadline_would_outlast_fails_over_at_once_and_one_inside_it_is_waited(
) {
    // 31 s is one second past the margin, so the request's deadline is one second after its admission on the
    // harness's clock, which stands still: a two-second throttle outlasts it on every account.
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_429_retry_after("2"));
    up.add_credential_rule(B, "/v1/messages", json_429_retry_after("2"));
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(31),
            ..default_limits()
        },
    )
    .await;
    let started = Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(900), "no wait: {waited:?}");
    assert_eq!(
        credentials_seen(&up),
        [A, B],
        "one submission per account, none retried in place"
    );
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "accounts_exhausted"),
        "the pool's own answer once no account is left"
    );
    up.shutdown().await;

    // Three seconds left: a one-second throttle ends inside the deadline and is waited out on its account.
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(33),
            ..default_limits()
        },
    )
    .await;
    let started = Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let waited = started.elapsed();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).attempt),
        (200, 2),
        "retried in place"
    );
    assert_eq!(credentials_seen(&up), [A, A]);
    assert!(
        waited >= Duration::from_millis(950),
        "the Retry-After was waited out: {waited:?}"
    );
    drain(resp).await;
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

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

/// A committed stream of `input_tokens` the fake holds after its first event until the test releases it, so the
/// request is in flight while the test reads the router.
fn held_stream(input_tokens: u64) -> Respond {
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![
            ev(
                "message_start",
                &format!(
                    r#"{{"type":"message_start","message":{{"id":"msg_1","usage":{{"input_tokens":{input_tokens}}}}}}}"#
                ),
            ),
            ev("message_stop", r#"{"type":"message_stop"}"#),
        ],
    }
}

#[tokio::test]
async fn a_retry_in_place_sends_no_ended_between_its_attempts_and_one_usage_for_the_request() {
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", held_stream(100));
    let h = priced_harness(&up, &[oauth("a", A, 0)]).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!((resp.status().as_u16(), o.attempt), (200, 2));
    let standing = five_hour(&h, "a").await;
    assert!(
        (standing - (0.5 + fixture_rewrite())).abs() < 1e-9,
        "the booking of attempt 1's selection, the body's estimate, stands while attempt 2 streams: {standing}"
    );
    up.release();
    drain(resp).await;
    let seen = events_through_cache_map(&mut events, &o.request_id).await;
    assert!(
        seen.iter()
            .all(|event| !matches!(event, RouterEvent::ReservationExpired { .. })),
        "no end report between the attempts: {seen:?}"
    );
    let settled = five_hour(&h, "a").await;
    assert!(
        (settled - (0.5 + 100.0 / 10_000_000.0)).abs() < 1e-9,
        "the usage of attempt 2 replaced the booking once: {settled}"
    );
    up.shutdown().await;
}

/// Every pin on `TIER` burns a tenth of the 5-hour window an hour whatever its usage, and nothing is charged, so the
/// window reads what the test reports.
fn tenth_an_hour() -> SharePricer {
    SharePricer::new(|_, _, _, _, _, _| None).with_rate(|window, _, tier, _, _, _| {
        (window == FIVE_HOUR && tier == TIER).then_some(0.1 / 3_600.0)
    })
}

/// Each of `accounts` on `TIER`, so the slow-down reads its 5-hour window.
async fn on_tier(h: &Harness, accounts: &[&str]) {
    for account in accounts {
        h.store
            .set_profile(
                account,
                "{}",
                &SubscriptionRow {
                    tier: Some(TIER.into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
    }
    h.router.reload().await.unwrap();
}

/// Two turns of `s1` ten seconds apart, which build its cycle, then the pool's slow-down turned on with a two-second
/// cap and the shedding round off, the clock left at `s1`'s next turn ten seconds on.
async fn cycle_then_slow(h: &Harness) {
    for at in [NOW, NOW + 10_000] {
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
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.slow_down = SlowDown {
        slow_down: true,
        slow_down_hold_seconds: 2,
        ..SlowDown::default()
    };
    pool.shedding.load_shedding = false;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
    h.clock.store(NOW + 20_000, Ordering::SeqCst);
}

/// The burn average `slow_then_cycle` sets on the pool, and how far before the held turn's clock its turns run. Five
/// minutes keeps every instant the helper dates after the epoch, since `NOW` lies a thousand seconds past it.
const SLOWED_AVERAGE_SECONDS: u64 = 300;

/// The pool's slow-down turned on with a two-second cap, the shedding round off and the burn average at
/// `SLOWED_AVERAGE_SECONDS`, `a`'s 5-hour window at `utilization` with its reset at `reset`, and three turns of `s1`
/// ten seconds apart. The reading and the turns are dated one burn average before `s1`'s next turn, at the clock this
/// leaves, so the stamp the renewals write has stood a burn average by then. The first turn places the pin and reads
/// no slow-down, the second starts its pace and stamps the ramp, and the third builds its ten-second cycle, both
/// renewals passing unheld at a ramp near 0. The gap of one burn average to the next turn is a pause that leaves the
/// cycle standing.
async fn slow_then_cycle(h: &Harness, utilization: f64, reset: i64) {
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
    five_hour_at(h, "a", utilization, reset, NOW - back).await;
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
}

/// `account`'s 5-hour window at `utilization` with its reset at `reset`, read at `at`.
async fn five_hour_at(h: &Harness, account: &str, utilization: f64, reset: i64, at: i64) {
    h.router
        .report(Report::Sample {
            account_id: account.into(),
            sample: UtilizationSample::default().with(
                FIVE_HOUR,
                WindowReading {
                    utilization,
                    reset: Some(reset),
                    observed_at: at,
                    source: WindowSource::Poll,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
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

/// The next decision that placed or renewed a request rather than holding it, its record whole.
async fn next_selected(events: &mut broadcast::Receiver<RouterEvent>) -> Box<DecisionRecord> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if summary.kind != "held" {
                return record;
            }
        }
    }
}

/// The waits a request's outcome names, each with its reason, as the completion writes them.
fn waits_of(resp: &axum::http::Response<axum::body::Body>) -> Vec<(String, u64)> {
    let held = outcome(resp)
        .held
        .expect("the outcome names the holds it waited through");
    serde_json::to_value(&held).unwrap()["waits"]
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

fn reasons(waits: &[(String, u64)]) -> Vec<&str> {
    waits.iter().map(|(reason, _)| reason.as_str()).collect()
}

#[tokio::test]
async fn a_slow_down_hold_is_cut_at_the_request_deadline_and_a_slot_wait_after_it_keeps_the_retry_bound(
) {
    // 31 s is one second past the margin, so the request's deadline is one second after its admission. a reads at its
    // ceiling, where the factor is zero whatever the demand, so the hold is the two-second cap and the deadline cuts
    // it.
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(31),
            ..default_limits()
        },
    )
    .await;
    on_tier(&h, &["a"]).await;
    cycle_then_slow(&h).await;
    five_hour_at(&h, "a", 0.96, NOW + 3_600_000, NOW + 10_000).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let hold = next_hold(&mut events).await;
    assert_eq!((hold.reason, hold.until), ("slow_down", Some(NOW + 22_000)));
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    h.clock.store(NOW + 21_000, Ordering::SeqCst);
    tokio::time::resume();
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the deadline ends the hold")
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable"),
        "the request is answered exhausted at its deadline"
    );
    assert_eq!(
        credentials_seen(&up),
        [A, A],
        "nothing was dialed after the hold"
    );
    let waits = waits_of(&resp);
    assert_eq!(reasons(&waits), ["slow_down"]);
    assert!(
        (1_000..2_000).contains(&waits[0].1),
        "the wait ran to the deadline, short of the cap: {waits:?}"
    );
    up.shutdown().await;

    // Without a request deadline the hold runs out, and the slot wait after it on a sibling at its cap runs its own
    // whole retry bound, which the slow-down hold did not start.
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", held_stream(100));
    let h = harness_with_router_config(
        &up,
        &[oauth("a", A, 1), oauth("b", B, 0)],
        PoolSpec {
            failover: Failover {
                in_flight_cap: 1,
                retry_in_place_seconds: 2,
                ..Failover::default()
            },
            ..PoolSpec::default()
        },
        RouterConfig {
            share: tenth_an_hour(),
            ..RouterConfig::default()
        },
    )
    .await;
    on_tier(&h, &["a", "b"]).await;
    // s2 holds b's one slot to the end of the case, so s1 is placed on a.
    let open = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(outcome(&open).account_id, "b");
    slow_then_cycle(&h, 0.9, NOW + 10_000 + 3_600_000).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let hold = next_hold(&mut events).await;
    assert_eq!((hold.account_id.as_str(), hold.reason), ("a", "slow_down"));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // a cools during the hold, so the selection after it places the request afresh, where b is at its cap.
    h.router
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: "account".into(),
                until: NOW + 3_600_000,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    let hold = next_hold(&mut events).await;
    assert_eq!(
        (hold.account_id.as_str(), hold.reason, hold.bound_ms),
        ("b", "cap", 2_000)
    );
    tokio::time::resume();
    let resp = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("the slot wait runs out")
        .unwrap();
    assert_ne!(resp.status(), 200, "no slot freed on b and a stays cooled");
    let waits = waits_of(&resp);
    assert_eq!(reasons(&waits), ["slow_down", "cap"]);
    assert!(
        waits[1].1 >= 1_900,
        "the slot wait ran its whole retry bound after the slow-down hold: {waits:?}"
    );
    drop(open);
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_slow_down_hold_under_a_short_head_timeout_runs_out_at_the_head_timeout_and_selects_again(
) {
    // 20 s is under the margin, so the request has no deadline and its waits are bounded by the head timeout itself.
    // a reads at its ceiling, where the factor is zero whatever the demand, so the hold is the cap, which lies past
    // the head timeout.
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(20),
            ..default_limits()
        },
    )
    .await;
    on_tier(&h, &["a"]).await;
    cycle_then_slow(&h).await;
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.slow_down.slow_down_hold_seconds = 25;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
    five_hour_at(&h, "a", 0.96, NOW + 3_600_000, NOW + 10_000).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let hold = next_hold(&mut events).await;
    assert_eq!(
        (hold.reason, hold.until, hold.bound_ms),
        ("slow_down", Some(NOW + 45_000), 25_000)
    );
    // Both clocks run with the hold: a second short of the head timeout the request still waits.
    tokio::time::sleep(Duration::from_millis(19_000)).await;
    h.clock.store(NOW + 39_000, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(600)).await;
    while let Ok(event) = events.try_recv() {
        if let RouterEvent::Decision { summary, .. } = event {
            panic!("no selection before the head timeout, got {}", summary.kind);
        }
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    h.clock.store(NOW + 40_000, Ordering::SeqCst);
    tokio::time::resume();
    let selected = next_selected(&mut events).await;
    let held = selected
        .request
        .holds
        .iter()
        .map(|hold| (hold.reason, hold.ran_out, hold.held_ms))
        .collect::<Vec<_>>();
    assert_eq!(
        held.iter()
            .map(|(reason, ran_out, _)| (*reason, *ran_out))
            .collect::<Vec<_>>(),
        [("slow_down", true)],
        "the head timeout ended the hold short of its cap"
    );
    assert!(
        (20_000..21_000).contains(&held[0].2),
        "the hold ran to the head timeout: {held:?}"
    );
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the request selects again after the hold")
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "a", 1),
        "the selection after the hold places the request"
    );
    assert_eq!(credentials_seen(&up), [A, A, A]);
    drain(resp).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_slot_that_frees_inside_the_bound_dispatches_the_pin_at_the_admission_and_its_next_gap_holds_no_wait(
) {
    // The path of the case above, a's slow-down hold, a cools and the selection after it waits for b's one slot, with
    // the slot freed inside the bound.
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.add_credential_rule(B, "/v1/messages", held_stream(100));
    let h = harness_with_router_config(
        &up,
        &[oauth("a", A, 1), oauth("b", B, 0)],
        PoolSpec {
            failover: Failover {
                in_flight_cap: 1,
                retry_in_place_seconds: 2,
                ..Failover::default()
            },
            ..PoolSpec::default()
        },
        RouterConfig {
            share: tenth_an_hour(),
            ..RouterConfig::default()
        },
    )
    .await;
    on_tier(&h, &["a", "b"]).await;
    let open = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(outcome(&open).account_id, "b");
    slow_then_cycle(&h, 0.9, NOW + 10_000 + 3_600_000).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let hold = next_hold(&mut events).await;
    assert_eq!((hold.account_id.as_str(), hold.reason), ("a", "slow_down"));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    h.router
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: "account".into(),
                until: NOW + 3_600_000,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    let hold = next_hold(&mut events).await;
    assert_eq!((hold.account_id.as_str(), hold.reason), ("b", "cap"));
    tokio::time::resume();
    // b's slot frees three seconds after the slow-down hold's end on the relay's clock.
    h.clock.store(NOW + 25_000, Ordering::SeqCst);
    up.release();
    drain(open).await;
    let admitted = next_selected(&mut events).await;
    assert_eq!(
        admitted
            .request
            .holds
            .iter()
            .map(|hold| (hold.reason, hold.ran_out, hold.until.is_some()))
            .collect::<Vec<_>>(),
        [("slow_down", false, true), ("cap", false, true)]
    );
    assert_eq!(
        admitted.request.holds[1].until,
        Some(NOW + 25_000),
        "the cap hold ends at the admission"
    );
    let resp = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("the slot frees inside the bound")
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    drain(resp).await;
    // The pin's next turn ten seconds after the admission: its cycle of ten seconds folds a gap of ten.
    h.clock.store(NOW + 35_000, Ordering::SeqCst);
    let next = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(next.status(), 200);
    let renewed = next_selected(&mut events).await;
    assert_eq!(
        renewed.pin.as_ref().and_then(|pin| pin.cycle_ms),
        Some(10_000),
        "the slot wait joins no gap of the pin"
    );
    drain(next).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_slow_down_hold_then_a_spent_window_waits_out_the_reset_under_its_own_bound() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with_router_config(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: Failover {
                retry_in_place_seconds: 8,
                ..Failover::default()
            },
            ..PoolSpec::default()
        },
        RouterConfig {
            share: tenth_an_hour(),
            ..RouterConfig::default()
        },
    )
    .await;
    on_tier(&h, &["a"]).await;
    slow_then_cycle(&h, 0.9, NOW + 10_000 + 3_600_000).await;
    let mut events = h.router.subscribe_events();
    tokio::time::pause();
    let app = h.app.clone();
    let waiting =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    let hold = next_hold(&mut events).await;
    assert_eq!((hold.reason, hold.until), ("slow_down", Some(NOW + 22_000)));
    // a's window reads spent during the hold with its reset seven seconds after the hold ends: inside an eight-second
    // bound that starts with the reset wait, past one that started with the slow-down hold, and before the harness's
    // ten-second head timeout runs out.
    five_hour_at(&h, "a", 1.0, NOW + 29_000, NOW + 21_000).await;
    h.clock.store(NOW + 22_000, Ordering::SeqCst);
    let hold = next_hold(&mut events).await;
    assert_eq!(
        (hold.reason, hold.until, hold.bound_ms),
        ("reset", Some(NOW + 29_000), 8_000)
    );
    h.clock.store(NOW + 29_000, Ordering::SeqCst);
    tokio::time::resume();
    let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the request lands after the reset")
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "a", 1),
        "one wire attempt, on the account the pin stayed on"
    );
    assert_eq!(credentials_seen(&up), [A, A, A, A]);
    assert_eq!(reasons(&waits_of(&resp)), ["slow_down", "reset"]);
    drain(resp).await;
    up.shutdown().await;
}
