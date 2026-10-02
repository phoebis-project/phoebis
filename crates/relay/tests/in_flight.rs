//! The in-flight counts: an account ticket per wire attempt, a key permit per request, both held until the
//! response ends, and what a selection does when an account is at its cap.

mod common;

use std::time::{Duration, Instant};

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Event};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_store::Failover;
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
async fn a_hold_followed_by_a_dispatch_names_the_hold_in_the_attempts_trace() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
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
async fn a_hold_that_would_leave_no_attempt_for_a_dial_answers_the_exhaustion_at_once() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            failover: account_cap(1, 2),
            ..PoolSpec::default()
        },
        bus,
        phoebis_relay::Limits {
            max_attempts: 1,
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
    assert_eq!(h.router.in_flight().account("a"), 1, "a is at its cap");
    let app = h.app.clone();
    let held = tokio::spawn(async move {
        let started = Instant::now();
        let resp = app.oneshot(request_for(KEY, MODEL, "s2")).await.unwrap();
        (resp, started.elapsed())
    });
    // A slot frees inside the bound: a request that waited would find it and still have no attempt to dial.
    tokio::time::sleep(HELD_FOR).await;
    up.release_one().await;
    let (resp, waited) = tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap(),
            resp.headers()["retry-after"].to_str().unwrap()
        ),
        (429, "accounts_at_cap", "2"),
        "the pool at its cap answers with the hold's bound, not the budget's refusal"
    );
    assert_eq!(
        outcome(&resp).error_code.as_deref(),
        Some("accounts_at_cap"),
        "the row names the code"
    );
    let at_cap_id = outcome(&resp).request_id;
    let body: serde_json::Value = serde_json::from_slice(&drain(resp).await).unwrap();
    assert_eq!(
        (&body["error"]["type"], &body["error"]["message"]),
        (
            &serde_json::json!("rate_limit_error"),
            &serde_json::json!("every account in this pool is at its in-flight cap")
        )
    );
    let mut events = Vec::new();
    let complete = wait_for(Duration::from_secs(5), || {
        while let Ok(e) = sub.events.try_recv() {
            events.push(e);
        }
        events.iter().find_map(|e| match e {
            Event::RequestComplete(c) if c.request_id == at_cap_id => Some(c.clone()),
            _ => None,
        })
    })
    .await;
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
        "a pool at its cap is an exhausted pool to the ledger and the alerts"
    );
    assert!(waited < HELD_FOR, "answered without waiting: {waited:?}");
    assert_eq!(posts(&up), 1, "nothing was dialled for the held request");
    drain(first).await;
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
    let mut sub = bus.subscribe("test");
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
async fn a_hold_whose_next_selection_finds_no_attempt_left_names_its_wait_on_the_completion() {
    let up = FakeUpstream::start(messages(held_stream())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            failover: account_cap(1, 1),
            ..PoolSpec::default()
        },
        bus,
        phoebis_relay::Limits {
            max_attempts: 2,
            ..default_limits()
        },
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
        "the hold on b would leave no slot of the budget for a dial"
    );
    assert_eq!(posts(&up), 2, "nothing was dialled for the held request");
    let held = outcome(&resp)
        .held
        .expect("the outcome names the hold it waited through");
    assert_eq!(
        held.waited_on,
        ["a"],
        "the hold on b answered at once and was never waited through"
    );
    assert!(
        (900..=2_000).contains(&held.held_ms),
        "the first wait ran the one-second bound out: {}",
        held.held_ms
    );
    let complete = completion_of(&mut sub, &outcome(&resp).request_id).await;
    assert_eq!(
        complete
            .held
            .as_deref()
            .map(|h| serde_json::from_str::<serde_json::Value>(h).unwrap()),
        Some(serde_json::to_value(&held).unwrap()),
        "the completion carries the first wait"
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
        "one takes the freed slot; the other, held again past the bound, is answered at the cap"
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
