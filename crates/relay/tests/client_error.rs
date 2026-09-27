//! A client error on the request that created its session's pin releases that pin: the pin never served, so
//! the session's next request is placed afresh rather than held to the account that answered the error. A pin
//! that already served keeps its place, and a throttle keeps the cooldown path.

mod common;

use std::time::Duration;

use phoebis_routing::{
    resolve, ClientClass, RouterEvent, SelectRequest, SelectResult, SelectionTrace,
};
use phoebis_store::AccountState;
use common::*;
use fake_upstream::{stays_for_async, FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use tokio::sync::broadcast;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const MODEL: &str = "claude-sonnet-4-5";

const INVALID_REQUEST: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: text content blocks must be non-empty"}}"#;

fn json(status: u16, body: &str) -> Respond {
    Respond::Json {
        status,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

/// An OAuth 429 with no unified headers, past the retry-in-place bound, so it fails over.
fn json_429_retry_after(seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), seconds.into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_vec(),
    }
}

async fn drain(resp: axum::http::Response<axum::body::Body>) {
    tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap();
}

async fn pinned(h: &Harness, session: &str) -> bool {
    h.router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.session_id == session)
}

/// The trace the router gives the session's next request, asked of the router directly so the test reads the
/// selection itself.
async fn next_trace(h: &Harness, session: &str) -> SelectionTrace {
    let snapshot = h.router.snapshot();
    let key = snapshot.key("desktop").expect("the harness key");
    let canonical = h.catalog.canonical_for(MODEL).await;
    let candidates = resolve(&snapshot, key, &canonical, MODEL).unwrap();
    let answer = h
        .router
        .select(SelectRequest {
            key_id: key.id.clone(),
            key_generation: key.created_at,
            agent_id: "main".into(),
            session_id: Some(session.into()),
            canonical_model: canonical,
            candidates,
            portable: true,
            attempted: Vec::new(),
            now: h.clock.load(std::sync::atomic::Ordering::SeqCst),
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Vec::new(),
            client: ClientClass::ClaudeCode,
            static_boundaries: Vec::new(),
            marker: false,
            parent_agent_id: None,
            holds: Vec::new(),
        })
        .await
        .unwrap();
    match answer.result {
        SelectResult::Selected(selection) => selection.trace,
        other => panic!("expected a selection, got {other:?}"),
    }
}

async fn next_event(events: &mut broadcast::Receiver<RouterEvent>) -> RouterEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event")
        .expect("not lagged")
}

/// The reasons the router released the session's pin with, failing on a move.
async fn released(
    events: &mut broadcast::Receiver<RouterEvent>,
    session: &str,
) -> Vec<&'static str> {
    loop {
        match next_event(events).await {
            RouterEvent::PinExpired {
                session_id,
                reasons,
                ..
            } if session_id == session => return reasons,
            RouterEvent::PinMigrated { session_id, .. } if session_id == session => {
                panic!("the pin moved rather than being released")
            }
            _ => {}
        }
    }
}

async fn a_fresh_pins_client_error_releases_it(status: u16, respond: Respond) {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond,
        },
        // A 401 is refreshed first; a token endpoint that fails leaves the upstream's own answer to forward.
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: json(503, "{}"),
        },
    ])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), status, "the upstream's own answer");
    drain(resp).await;
    assert_eq!(
        released(&mut events, "s1").await,
        vec!["client_error"],
        "{status}: the release names the one reason"
    );
    assert!(
        !pinned(&h, "s1").await,
        "{status}: the router holds no pin for the session"
    );
    let trace = next_trace(&h, "s1").await;
    assert_eq!(
        (trace.renewed, trace.moved),
        (false, None),
        "{status}: the session's next request is a fresh placement"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_fresh_pins_400_releases_it_and_the_sessions_next_request_is_a_fresh_placement() {
    a_fresh_pins_client_error_releases_it(400, json(400, INVALID_REQUEST)).await;
}

#[tokio::test]
async fn a_fresh_pins_401_releases_it_and_the_sessions_next_request_is_a_fresh_placement() {
    a_fresh_pins_client_error_releases_it(
        401,
        json(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#,
        ),
    )
    .await;
}

/// A 403 that is neither an authentication nor a permission error, as an edge in front of the upstream
/// answers, commits as the client's own to read.
#[tokio::test]
async fn a_fresh_pins_403_releases_it_and_the_sessions_next_request_is_a_fresh_placement() {
    a_fresh_pins_client_error_releases_it(
        403,
        Respond::Json {
            status: 403,
            headers: vec![("content-type".into(), "text/html".into())],
            body: b"<html><body><h1>403 Forbidden</h1></body></html>".to_vec(),
        },
    )
    .await;
}

#[tokio::test]
async fn a_fresh_pins_404_releases_it_and_the_sessions_next_request_is_a_fresh_placement() {
    a_fresh_pins_client_error_releases_it(404, json_404_plain()).await;
}

#[tokio::test]
async fn a_pin_renewed_by_an_earlier_200_survives_a_later_400() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let first = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);
    drain(first).await;
    let pin = h.router.pins().await.unwrap()[0].clone();
    up.push_credential_rule(A, "/v1/messages", json(400, INVALID_REQUEST));
    let rejected = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(rejected.status().as_u16(), 400);
    drain(rejected).await;
    // A renewal is a report as well, so this window bounds how long a release would have to land rather than
    // proving none is coming; the renewal the 400 sends in its place is what makes the pin stay.
    stays_for_async(Duration::from_millis(200), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.epoch == pin.epoch)
    })
    .await;
    let trace = next_trace(&h, "s1").await;
    assert_eq!(
        (trace.renewed, trace.moved, trace.strategy),
        (true, None, "priority"),
        "the pin that served renews in place, under the pool's strategy"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_429_on_a_fresh_pin_takes_the_cooldown_path_as_before() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_429_retry_after("30"));
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!((resp.status().as_u16(), o.account_id.as_str()), (200, "b"));
    drain(resp).await;
    loop {
        match next_event(&mut events).await {
            RouterEvent::PinMigrated {
                session_id,
                from_account,
                to_account,
                reason,
                ..
            } if session_id == "s1" => {
                assert_eq!(
                    (from_account.as_str(), to_account.as_str(), reason.as_str()),
                    ("a", "b", "cooldown"),
                    "the cooldown moved the pin"
                );
                break;
            }
            RouterEvent::PinExpired { session_id, .. } if session_id == "s1" => {
                panic!("the throttle released the pin rather than moving it")
            }
            _ => {}
        }
    }
    assert!(
        h.router
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|c| c.account_id == "a" && c.reason == "rate_limit"),
        "the cooldown row is on a"
    );
    up.shutdown().await;
}

const EXPIRED: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#;

/// Fails the test on any release of the session's pin within `window`.
async fn no_release_within(
    events: &mut broadcast::Receiver<RouterEvent>,
    session: &str,
    window: Duration,
) {
    let until = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(until, events.recv()).await {
            Err(_) => return,
            Ok(Ok(RouterEvent::PinExpired {
                session_id,
                reasons,
                ..
            })) if session_id == session => panic!("the pin was released: {reasons:?}"),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("{error}"),
        }
    }
}

#[tokio::test]
async fn a_401_whose_refresh_answers_invalid_grant_leaves_the_pin_to_the_mark() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json(401, EXPIRED),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: json(
                400,
                r#"{"error":"invalid_grant","error_description":"revoked"}"#,
            ),
        },
    ])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "the upstream's own answer");
    drain(resp).await;
    wait_for(Duration::from_secs(5), || {
        (h.store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
    no_release_within(&mut events, "s1", Duration::from_millis(200)).await;
    assert!(
        pinned(&h, "s1").await,
        "the mark ends the pin at the next selection, under its own reason"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_401_on_an_account_disconnected_meanwhile_leaves_the_pin_to_its_state() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Gated(Box::new(json(401, EXPIRED))),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: json(503, "{}"),
        },
    ])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut events = h.router.subscribe_events();
    let app = h.app.clone();
    let inflight =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (!up.requests().is_empty()).then_some(())
    })
    .await;
    // The router has not reloaded, so the answer still reaches the refresh, which finds the account disconnected.
    h.store.set_state("a", AccountState::Disconnected).unwrap();
    up.release();
    let resp = inflight.await.unwrap();
    assert_eq!(resp.status().as_u16(), 401, "the upstream's own answer");
    drain(resp).await;
    no_release_within(&mut events, "s1", Duration::from_millis(200)).await;
    assert!(
        pinned(&h, "s1").await,
        "the account's state ends the pin at the next selection, under its own reason"
    );
    assert!(
        up.requests()
            .iter()
            .all(|r| !r.line.starts_with("POST /v1/oauth/token")),
        "the refresh stopped at the account's state"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_fresh_pin_a_later_request_renewed_survives_the_first_requests_400() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(json(400, INVALID_REQUEST))),
    );
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut events = h.router.subscribe_events();
    let app = h.app.clone();
    let first =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (!up.requests().is_empty()).then_some(())
    })
    .await;
    let pin = h.router.pins().await.unwrap()[0].clone();
    // A second turn of the session renews the pin a second later, while the first is still upstream.
    h.clock
        .store(NOW + 1_000, std::sync::atomic::Ordering::SeqCst);
    let second = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);
    drain(second).await;
    up.release();
    let first = first.await.unwrap();
    assert_eq!(first.status().as_u16(), 400);
    drain(first).await;
    no_release_within(&mut events, "s1", Duration::from_millis(200)).await;
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.epoch == pin.epoch),
        "the pin served the second turn, so the first turn's 400 leaves it"
    );
    up.shutdown().await;
}

/// A committed stream the fake holds after its first event until the test releases it.
fn held_stream() -> Respond {
    let ev = |event: &str, data: &str| fake_upstream::SseEvent {
        event: event.into(),
        data: data.into(),
    };
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![
            ev(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#,
            ),
            ev("message_stop", r#"{"type":"message_stop"}"#),
        ],
    }
}

#[tokio::test]
async fn a_fresh_pin_renewed_in_the_same_millisecond_survives_the_first_requests_400() {
    let up = FakeUpstream::start(vec![]).await;
    up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(json(400, INVALID_REQUEST))),
    );
    up.push_credential_rule(A, "/v1/messages", held_stream());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let mut events = h.router.subscribe_events();
    let app = h.app.clone();
    let first =
        tokio::spawn(async move { app.oneshot(request_for(KEY, MODEL, "s1")).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (!up.requests().is_empty()).then_some(())
    })
    .await;
    let pin = h.router.pins().await.unwrap()[0].clone();
    // A second turn of the session renews the pin with the clock where the placement left it, so the renewal
    // leaves `last_used_at` equal to `created_at`, and stays open while the first turn's 400 lands.
    let second = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);
    assert_eq!(
        h.router.pins().await.unwrap()[0].last_used_at,
        pin.created_at,
        "the renewal shares the placement's millisecond"
    );
    up.release_one().await;
    let first = first.await.unwrap();
    assert_eq!(first.status().as_u16(), 400);
    drain(first).await;
    no_release_within(&mut events, "s1", Duration::from_millis(200)).await;
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.epoch == pin.epoch),
        "the pin serves the open second turn, so the first turn's 400 leaves it"
    );
    up.release();
    drain(second).await;
    up.shutdown().await;
}
