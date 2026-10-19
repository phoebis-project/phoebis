//! What the relay tells the bus, request by request.
mod common;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{
    AttemptOutcome, Bus, ClientFacts, Delivery, DiagnosticsSource, Event, Fork, Outcome, Phase,
    RequestComplete, ResponseChunk, Subscriber,
};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{Limits, MemoryKey};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
/// The session id the first-party fixture's head carries.
const FIRST_PARTY_SESSION: &str = "11111111-1111-4111-8111-111111111111";
const REFUSED_DIAGNOSTICS: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"unknown field diagnostics."}}"#;

/// The events of one request in publish order, with the chunks pulled out beside them.
fn split(events: Vec<Event>) -> (Vec<Event>, Vec<ResponseChunk>) {
    let mut lifecycle = Vec::new();
    let mut chunks = Vec::new();
    for event in events {
        match event {
            Event::ResponseChunk(chunk) => chunks.push(chunk),
            other => lifecycle.push(other),
        }
    }
    (lifecycle, chunks)
}

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

fn stream_events() -> Vec<SseEvent> {
    vec![
        ev("ping", r#"{"type":"ping"}"#),
        ev(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5,"cache_read_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":0}},"diagnostics":{"cache_miss_reason":{"type":"none"}}}}"#,
        ),
        ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ),
        ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
        ),
        ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        ),
        ev("message_stop", r#"{"type":"message_stop"}"#),
    ]
}

/// Everything the subscriber holds right now, without waiting for more, lifecycle events and chunks apart.
fn drain(sub: &mut Subscriber) -> (Vec<Event>, Vec<ResponseChunk>) {
    let mut events = Vec::new();
    while let Ok(e) = sub.events.try_recv() {
        events.push(e);
    }
    split(events)
}

/// The completion is published by the body task after the caller has the whole body, so it can land a
/// moment later than the response.
async fn drain_until_complete(sub: &mut Subscriber) -> (Vec<Event>, Vec<ResponseChunk>) {
    let give_up = std::time::Instant::now() + Duration::from_secs(2);
    let mut all = Vec::new();
    loop {
        while let Ok(e) = sub.events.try_recv() {
            all.push(e);
        }
        if all.iter().any(|e| matches!(e, Event::RequestComplete(_))) {
            return split(all);
        }
        assert!(
            std::time::Instant::now() < give_up,
            "no completion was published"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_streamed_request_publishes_observed_selected_head_chunks_and_complete_in_order() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![("request-id".into(), "req_up_1".into())],
            events: stream_events(),
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let (events, chunks) = drain_until_complete(&mut sub).await;
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("first event: {:?}", events[0])
    };
    assert_eq!(
        (
            observed.key_id.as_deref(),
            observed.pool_id.as_deref(),
            observed.stream,
            observed.canonical_model.as_str()
        ),
        (
            Some("desktop"),
            Some("main"),
            true,
            "claude-sonnet-4-5-20250929"
        )
    );
    assert!(observed.headers.iter().any(|(n, _)| n == "user-agent"));
    assert!(observed
        .headers
        .iter()
        .all(|(n, _)| n != "authorization" && n != "x-api-key"));
    assert_eq!(
        observed.body.as_ref(),
        phoebis_profile::fixtures::body(phoebis_profile::fixtures::BodyFixture::Oauth)
    );
    assert_eq!(
        observed.body_capture_hours, 0,
        "the harness pool captures nothing"
    );
    let Event::AccountSelected(selected) = &events[1] else {
        panic!()
    };
    assert_eq!(
        (
            selected.attempt,
            selected.account_id.as_str(),
            selected.account_kind.as_str()
        ),
        (1, "a", "oauth")
    );
    assert!(
        selected.selection.is_some(),
        "a real request's own placement carries a trace"
    );
    let Event::ResponseHead(head) = &events[2] else {
        panic!()
    };
    assert_eq!(
        (head.attempt, head.status, head.outcome),
        (1, 200, AttemptOutcome::Succeeded)
    );
    assert!(
        selected.started_at <= head.written_at && head.written_at <= head.observed_at,
        "selected, written, head are in order"
    );
    assert!(
        head.first_event
            .as_ref()
            .is_some_and(|d| d.starts_with(br#"{"type":"message_start""#)),
        "the committing event rides on the head"
    );
    assert!(head
        .headers
        .iter()
        .any(|(n, v)| n == "request-id" && v == "req_up_1"));
    let Event::RequestComplete(complete) = &events[3] else {
        panic!()
    };
    assert_eq!(
        (
            complete.attempt,
            complete.account_id.as_deref(),
            complete.outcome,
            complete.status
        ),
        (1, Some("a"), Outcome::Succeeded, 200)
    );
    let usage = complete.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.cache_write_5m_tokens,
            usage.cache_write_1h_tokens,
            usage.output_tokens
        ),
        (Some(5), Some(2), Some(3), Some(0), Some(7))
    );
    assert_eq!(usage.cache_miss_reason.as_deref(), Some("none"));
    assert!(
        complete.ttfb_ms.is_some()
            && complete.ttfc_ms.is_some()
            && complete.ttfc_ms >= complete.ttfb_ms
    );
    assert_eq!(complete.response_bytes, Some(body.len() as u64));
    assert_eq!(events.len(), 4);
    let joined: Vec<u8> = chunks.iter().flat_map(|c| c.body.to_vec()).collect();
    assert_eq!(
        joined,
        body.to_vec(),
        "the chunks are the decoded body, in order"
    );
    assert!(chunks.windows(2).all(|w| w[0].index < w[1].index));
    // The harness clock stands still, so the completion can only be shown not to precede the head.
    assert!(complete.status == 200 && head.observed_at <= complete.finished_at);
    let phases: Vec<&Phase> = chunks.iter().filter_map(|c| c.phase.as_ref()).collect();
    assert!(matches!(phases[0], Phase::MessageStart { usage } if usage.input_tokens == Some(5)));
    assert!(matches!(phases[1], Phase::FirstContentDelta));
    assert!(matches!(phases[2], Phase::ToolUseDelta));
    assert!(matches!(phases[3], Phase::MessageDelta { usage } if usage.output_tokens == Some(7)));
    up.shutdown().await;
}

/// An operator can lower the global cap below a window a key or pool already holds. The stored value is not
/// rewritten, so what a request is allowed to keep is decided where the request is published.
#[tokio::test]
async fn a_capture_window_past_the_running_cap_is_published_at_the_cap() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        Limits {
            body_capture_max_hours: 1,
            ..default_limits()
        },
    )
    .await;
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.body_capture_hours = 72;
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("first event: {:?}", events[0])
    };
    assert_eq!(
        observed.body_capture_hours, 1,
        "the window a request may keep is the running cap, not the one the pool was left holding"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_failover_publishes_a_head_per_attempt_and_the_completion_names_the_serving_account() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body:
                br#"{"id":"msg_2","type":"message","usage":{"input_tokens":3,"output_tokens":4}}"#
                    .to_vec(),
        },
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        bus,
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let (events, chunks) = drain_until_complete(&mut sub).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            Event::RequestObserved(_) => "observed",
            Event::AccountSelected(_) => "selected",
            Event::ResponseHead(_) => "head",
            Event::RequestComplete(_) => "complete",
            Event::ResponseChunk(_) => "chunk",
        })
        .collect();
    assert_eq!(
        kinds,
        ["observed", "selected", "head", "selected", "head", "complete"]
    );
    let Event::ResponseHead(first) = &events[2] else {
        panic!()
    };
    assert_eq!(
        (first.attempt, first.status, first.outcome),
        (1, 429, AttemptOutcome::RateLimited)
    );
    let cooldown = first.cooldown.as_ref().unwrap();
    assert_eq!(
        (
            cooldown.scope.as_str(),
            cooldown.window.as_str(),
            cooldown.until
        ),
        ("account", "five_hour", 4_600_000 + 1_000)
    );
    let Event::AccountSelected(second) = &events[3] else {
        panic!()
    };
    assert_eq!((second.attempt, second.account_id.as_str()), (2, "b"));
    let Event::RequestComplete(complete) = &events[5] else {
        panic!()
    };
    assert_eq!(
        (
            complete.attempt,
            complete.account_id.as_deref(),
            complete.outcome
        ),
        (2, Some("b"), Outcome::Succeeded)
    );
    assert_eq!(
        complete
            .usage
            .as_ref()
            .map(|u| (u.input_tokens, u.output_tokens)),
        Some((Some(3), Some(4))),
        "a JSON 200 relayed through the body task still yields its usage"
    );
    assert!(
        complete.ttfc_ms.is_none(),
        "a JSON answer has no content delta"
    );
    let joined: Vec<u8> = chunks.iter().flat_map(|c| c.body.to_vec()).collect();
    assert_eq!(
        joined,
        br#"{"id":"msg_2","type":"message","usage":{"input_tokens":3,"output_tokens":4}}"#
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_gzipped_json_200_the_caller_accepts_as_is_yields_usage_and_so_does_one_past_64_kib() {
    use std::io::Write;
    let small = br#"{"id":"msg_3","type":"message","usage":{"input_tokens":11,"output_tokens":2},"diagnostics":{"cache_miss_reason":{"type":"tools_changed"}}}"#;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(small).unwrap();
    let gz = encoder.finish().unwrap();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("content-encoding".into(), "gzip".into())],
            body: gz,
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "accept-encoding", "gzip");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("gzip"),
        "passed through as the caller asked"
    );
    let _ = resp.into_body().collect().await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    let usage = complete
        .usage
        .as_ref()
        .expect("decoded for the bus although forwarded as gzip");
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_miss_reason.as_deref()
        ),
        (Some(11), Some(2), Some("tools_changed"))
    );
    let padding = format!(
        r#"{{"id":"msg_4","type":"message","pad":"{}","usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
        "x".repeat(200 * 1024)
    );
    up.set_rule_after(
        0,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: padding.clone().into_bytes(),
        },
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), padding.len());
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(
        complete
            .usage
            .as_ref()
            .map(|u| (u.input_tokens, u.output_tokens)),
        Some((Some(1), Some(1))),
        "read up to the request bound, not 64 KiB"
    );
    // The same answer on a router whose request bound it outgrows: the client still gets every byte. The bound
    // sits above the 112 KiB fixture request, which a lower one would refuse before any answer.
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let bounded = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        Limits {
            max_request_bytes: 150 << 10,
            ..default_limits()
        },
    )
    .await;
    let resp = bounded
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s3"))
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), padding.len());
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert!(
        complete.usage.is_none(),
        "past the request bound the usage stays unknown"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn after_the_head_an_sse_error_fails_a_lost_upstream_reads_incomplete_and_a_deleted_account_cancels(
) {
    let after_head = vec![
        ev(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        ),
        ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ),
        ev(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        ),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: after_head,
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the head was committed before the error"
    );
    let _ = resp.into_body().collect().await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.status
        ),
        (Outcome::Failed, Some("overloaded_error"), 200),
        "the client got a 200 and the stream ended in an error"
    );
    assert_eq!(
        complete.usage.as_ref().and_then(|u| u.input_tokens),
        Some(5)
    );
    // The post-commit observer may have cooled the account on that error; the next leg needs it eligible.
    let _ = h.router.clear_cooldown("a", "account").await;
    let _ = h
        .router
        .clear_cooldown("a", "model:claude-sonnet-4-5")
        .await;
    // The upstream dies after the head: the body task reports the transport failure.
    up.set_rule_after(0, "/v1/messages", Respond::DropAfterHead);
    let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.into_body().collect().await.is_err(),
        "the body ends in an error"
    );
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (Outcome::Incomplete, Some("stream_cut")),
        "the body ended before its end"
    );
    // The account is deleted under a stalled stream: the completion says canceled with the client's 503.
    up.set_rule_after(
        0,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    );
    let app = h.app.clone();
    let stalled = tokio::spawn(async move { app.oneshot(stream_request(KEY)).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 3).then_some(())
    })
    .await;
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = stalled.await.unwrap();
    assert_eq!(resp.status(), 503);
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.status
        ),
        (Outcome::Canceled, Some("account_cancelled"), 503)
    );
    up.release();
    up.shutdown().await;
}

fn json_answer(status: u16, body: &str) -> Respond {
    Respond::Json {
        status,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

fn ok_with_id(id: &str) -> Respond {
    json_answer(
        200,
        &format!(
            r#"{{"id":"{id}","type":"message","content":[],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        ),
    )
}

/// Sends `request`, reads its answer to the end and returns the completion the relay published for it.
async fn completion_of(
    h: &Harness,
    sub: &mut Subscriber,
    request: Request<Body>,
) -> (u16, RequestComplete) {
    let resp = h.app.clone().oneshot(request).await.unwrap();
    let status = resp.status().as_u16();
    let _ = resp.into_body().collect().await;
    let (events, _) = drain_until_complete(sub).await;
    let complete = events
        .into_iter()
        .find_map(|event| match event {
            Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .expect("the request completed");
    (status, complete)
}

#[tokio::test]
async fn the_completion_event_names_the_members_source_and_the_writers_follow_it() {
    let up = FakeUpstream::start(vec![]).await;
    for respond in [
        ok_with_id("msg_client"),
        ok_with_id("msg_router"),
        ok_with_id("msg_gated"),
        json_answer(200, r#"{"input_tokens":7}"#),
        json_answer(400, REFUSED_DIAGNOSTICS),
        ok_with_id("msg_strip"),
    ] {
        up.push_credential_rule(A, "/v1/messages", respond);
    }
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
        bus,
    )
    .await;
    let client_session = MemoryKey::for_session(
        "desktop",
        h.store.api_key("desktop").unwrap().unwrap().created_at,
        FIRST_PARTY_SESSION,
        "main",
    );
    let remembered = || {
        h.relay
            .previous(&client_session, "a", "claude-sonnet-4-5")
            .message_id
    };

    let client = request_from_fixture(
        inference_head(FixtureKind::OauthFirstParty),
        body(BodyFixture::FirstParty9b244),
        KEY,
    );
    let (status, complete) = completion_of(&h, &mut sub, client).await;
    assert_eq!(status, 200);
    assert_eq!(complete.diagnostics, Some(DiagnosticsSource::Client));
    assert_eq!(remembered().as_deref(), Some("msg_client"));

    let (status, complete) = completion_of(
        &h,
        &mut sub,
        request_for(KEY, "claude-sonnet-4-5", "s-router"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(complete.diagnostics, Some(DiagnosticsSource::Router));

    let mut gated = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &without_tools(body(BodyFixture::Oauth), false),
        KEY,
    );
    set_header(&mut gated, "x-claude-code-session-id", "s-gated");
    let (status, complete) = completion_of(&h, &mut sub, gated).await;
    assert_eq!(status, 200);
    assert_eq!(
        complete.diagnostics, None,
        "the gate gave the body no member"
    );

    let count = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("Authorization", format!("Bearer {KEY}"))
        .header("User-Agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", "s-count")
        .body(Body::from(
            r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"diagnostics":{"previous_message_id":null}}"#,
        ))
        .unwrap();
    let (status, complete) = completion_of(&h, &mut sub, count).await;
    assert_eq!(status, 200);
    assert_eq!(
        complete.diagnostics, None,
        "a token count records no source, a member of the client's included"
    );

    // Under the client-member turn's session: the first pass names that turn's answer, the 400 strips the member,
    // and the retry's answer, sent without one, must not replace it.
    let (status, complete) = completion_of(
        &h,
        &mut sub,
        request_for(KEY, "claude-sonnet-4-5", FIRST_PARTY_SESSION),
    )
    .await;
    assert_eq!(status, 200);
    let sent = up.requests();
    assert_eq!(sent.len(), 6, "the strip retried once on the same account");
    assert!(phoebis_cch::object_member_span(&sent[4].body, &["diagnostics"]).is_some());
    assert!(phoebis_cch::object_member_span(&sent[5].body, &["diagnostics"]).is_none());
    assert_eq!(
        complete.diagnostics, None,
        "the answering pass sent no member"
    );
    assert_eq!(
        remembered().as_deref(),
        Some("msg_client"),
        "the session still names the turn that carried the member"
    );
    up.shutdown().await;
}

/// A JSON 200 whose `diagnostics` member is `member`, written as its raw JSON value.
fn answer_with_diagnostics(id: &str, member: &str) -> Respond {
    json_answer(
        200,
        &format!(
            r#"{{"id":"{id}","type":"message","content":[],"usage":{{"input_tokens":1,"output_tokens":1}},"diagnostics":{member}}}"#
        ),
    )
}

/// A streamed 200 whose `message_start` and `message_delta` end with `head` and `delta`, each empty or a
/// `diagnostics` member led by its comma.
fn stream_with_diagnostics(id: &str, head: &str, delta: &str) -> Respond {
    Respond::Sse {
        status: 200,
        headers: vec![],
        events: vec![
            ev(
                "message_start",
                &format!(
                    r#"{{"type":"message_start","message":{{"id":"{id}","usage":{{"input_tokens":1}}{head}}}}}"#
                ),
            ),
            ev(
                "message_delta",
                &format!(
                    r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{{"output_tokens":1}}{delta}}}"#
                ),
            ),
            ev("message_stop", r#"{"type":"message_stop"}"#),
        ],
        delay_between: Duration::from_millis(1),
    }
}

#[tokio::test]
async fn a_null_diagnostics_member_reads_none_only_where_the_body_named_a_message_and_a_null_reason_reads_pending(
) {
    const PENDING: &str = r#"{"cache_miss_reason":null}"#;
    const NULL_MEMBER: &str = r#","diagnostics":null"#;
    const PENDING_MEMBER: &str = r#","diagnostics":{"cache_miss_reason":null}"#;
    let up = FakeUpstream::start(vec![]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    // The pool's switch stays off, so a pass carries the member only where the client sent it.
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let named = || {
        let mut value: serde_json::Value = serde_json::from_slice(body(BodyFixture::Oauth))
            .expect("the fixture body is valid JSON");
        value["diagnostics"] = serde_json::json!({ "previous_message_id": "msg_prev" });
        let mut req = request_from_fixture(
            inference_head(FixtureKind::OauthNonFirstParty),
            &serde_json::to_vec(&value).expect("a Value always re-encodes"),
            KEY,
        );
        set_header(&mut req, "x-claude-code-session-id", "s-named");
        req
    };
    // The captured first-party body carries the member with a null id: a first turn, naming no message.
    let unnamed = || {
        request_from_fixture(
            inference_head(FixtureKind::OauthFirstParty),
            body(BodyFixture::FirstParty9b244),
            KEY,
        )
    };
    let sourceless = |session: &str| request_for(KEY, "claude-sonnet-4-5", session);
    let read = |complete: &RequestComplete| {
        (
            complete.diagnostics,
            complete
                .usage
                .as_ref()
                .and_then(|u| u.cache_miss_reason.clone()),
        )
    };
    let client = Some(DiagnosticsSource::Client);
    let cases = [
        (
            named(),
            answer_with_diagnostics("msg_named_null", "null"),
            client,
            Some("none"),
            "a null member answering a body that named a message",
        ),
        (
            unnamed(),
            answer_with_diagnostics("msg_unnamed_null", "null"),
            client,
            None,
            "a null member answering a body whose member named no message",
        ),
        (
            sourceless("s-null"),
            answer_with_diagnostics("msg_sourceless_null", "null"),
            None,
            None,
            "a null member answering a body with no member",
        ),
        (
            unnamed(),
            answer_with_diagnostics("msg_unnamed_pending", PENDING),
            client,
            Some("pending"),
            "the pending shape answering a body that named no message",
        ),
        (
            sourceless("s-pending"),
            answer_with_diagnostics("msg_sourceless_pending", PENDING),
            None,
            Some("pending"),
            "the pending shape answering a body with no member",
        ),
        (
            sourceless("s-typed"),
            answer_with_diagnostics(
                "msg_sourceless_typed",
                r#"{"cache_miss_reason":{"type":"none"}}"#,
            ),
            None,
            Some("none"),
            "a type the upstream names",
        ),
        (
            named(),
            stream_with_diagnostics("msg_named_head", NULL_MEMBER, ""),
            client,
            Some("none"),
            "a streamed head's null member on a body that named a message",
        ),
        (
            unnamed(),
            stream_with_diagnostics("msg_unnamed_head", NULL_MEMBER, ""),
            client,
            None,
            "a streamed head's null member on a body that named no message",
        ),
        (
            unnamed(),
            stream_with_diagnostics("msg_unnamed_head_pending", PENDING_MEMBER, ""),
            client,
            Some("pending"),
            "a streamed head's pending shape on a body that named no message",
        ),
        (
            named(),
            stream_with_diagnostics("msg_named_delta", "", NULL_MEMBER),
            client,
            Some("none"),
            "a delta's null member on a body that named a message",
        ),
        (
            unnamed(),
            stream_with_diagnostics("msg_unnamed_delta", "", NULL_MEMBER),
            client,
            None,
            "a delta's null member on a body that named no message",
        ),
    ];
    let count = cases.len();
    for (request, respond, source, reason, case) in cases {
        up.push_credential_rule(A, "/v1/messages", respond);
        let (status, complete) = completion_of(&h, &mut sub, request).await;
        assert_eq!(status, 200, "{case}");
        assert_eq!(
            read(&complete),
            (source, reason.map(str::to_string)),
            "{case}"
        );
    }
    assert_eq!(up.requests().len(), count, "one pass per case");
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_whose_account_left_after_its_head_completes_with_the_legs_source() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            cache_diagnostics: true,
            ..Default::default()
        },
        bus,
    )
    .await;
    let app = h.app.clone();
    let stalled = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = stalled.await.unwrap();
    assert_eq!(resp.status(), 503);
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.diagnostics
        ),
        (
            Outcome::Canceled,
            Some("account_cancelled"),
            Some(DiagnosticsSource::Router)
        ),
        "the upstream compared the leg's body before the account left"
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn refused_and_exhausted_requests_publish_an_observed_event_and_their_completion() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_unified(4_600, "five_hour"),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["claude-sonnet-4-5*", "NOT *"]),
            ..PoolSpec::default()
        },
        bus,
    )
    .await;
    // An unknown key: a row with no key and no pool.
    let resp = h
        .app
        .clone()
        .oneshot(request_for("phoebis-nobody", "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let (events, _) = drain(&mut sub);
    let [Event::RequestObserved(observed), Event::RequestComplete(complete)] = events.as_slice()
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        (
            observed.key_id.as_deref(),
            observed.pool_id.as_deref(),
            observed.model.as_str()
        ),
        (None, None, "")
    );
    assert_eq!(
        (
            complete.outcome,
            complete.status,
            complete.error_code.as_deref(),
            complete.attempt
        ),
        (Outcome::Rejected, 401, Some("auth_unknown"), 0),
        "the status the client received, not an upstream status"
    );
    // A policy 404: a row, no attempt.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let (events, _) = drain(&mut sub);
    let [Event::RequestObserved(observed), Event::RequestComplete(complete)] = events.as_slice()
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        (
            observed.key_id.as_deref(),
            observed.canonical_model.as_str()
        ),
        (Some("desktop"), "claude-opus-5")
    );
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (Outcome::Rejected, Some("model_not_allowed"))
    );
    // Exhaustion: the one attempt's head, then an exhausted completion.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let (events, _) = drain(&mut sub);
    assert!(
        matches!(events.last(), Some(Event::RequestComplete(c)) if c.outcome == Outcome::Exhausted && c.error_code.as_deref() == Some("accounts_exhausted") && c.status == 429)
    );
    assert_eq!(events.len(), 4, "observed, selected, head, complete");
    up.shutdown().await;
}

/// The class is derived at admission and the hint headers and the turn origin are read off the request that
/// carries them; none of it gates the request, so the fact reaches the bus whether or not the client sent it.
#[tokio::test]
async fn the_observed_event_carries_the_client_class_the_hint_headers_and_the_turn_origin() {
    use phoebis_profile::fixtures::{
        body as fixture_body, inference_head, BodyFixture, FixtureKind,
    };
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut with_line: serde_json::Value =
        serde_json::from_slice(fixture_body(BodyFixture::Oauth)).unwrap();
    with_line["system"][0]["text"] = serde_json::Value::String(
        "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; \
         cc_turn_origin=human;"
            .into(),
    );
    let native = serde_json::to_vec(&with_line).unwrap();
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &native,
        KEY,
    );
    set_header(&mut req, "x-claude-code-agent-type", "Explore");
    set_header(&mut req, "x-claude-code-compaction", "auto");
    set_header(&mut req, "x-claude-code-context-compacted", "manual");
    set_header(
        &mut req,
        "x-claude-code-prev-tool-durations",
        "Bash=1200;Read=35;",
    );
    h.app.clone().oneshot(req).await.unwrap();
    let (events, _) = drain(&mut sub);
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(
        observed.client,
        ClientFacts {
            class: "claude-code/sdk-cli".into(),
            agent_type: Some("Explore".into()),
            classifier: None,
            compaction: Some("auto".into()),
            context_compacted: Some("manual".into()),
            prev_tool_ms: Some(1_235),
            request_class: Some("main".into()),
            turn_origin: Some("human".into()),
        }
    );
    let req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        fixture_body(BodyFixture::Oauth),
        KEY,
    );
    h.app.clone().oneshot(req).await.unwrap();
    let (events, _) = drain(&mut sub);
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(
        observed.client,
        ClientFacts {
            class: "claude-code/sdk-cli".into(),
            request_class: Some("main".into()),
            ..Default::default()
        }
    );
    up.shutdown().await;
}

/// A session's own turn continues no other conversation, so its observed event carries no fork facts.
#[tokio::test]
async fn the_observed_event_carries_no_fork_for_a_sessions_own_turn() {
    use phoebis_profile::fixtures::{inference_head, FixtureKind};
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[api_key("a", A, 0)], PoolSpec::default(), bus).await;
    let plain_body = br#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s1\"}"}}"#;

    let mut with_session =
        request_from_fixture(inference_head(FixtureKind::ApiKey), plain_body, KEY);
    set_header(&mut with_session, "x-claude-code-session-id", "s1");
    h.app.clone().oneshot(with_session).await.unwrap();
    let (events, _) = drain(&mut sub);
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(observed.fork, Fork::default());
    up.shutdown().await;
}

/// The class and the two hash chains the body implies both reach the bus and the router alike; a header-less
/// request selects with neither the class nor `context_compacted`, and a refusal before the body is read
/// publishes neither hash.
#[tokio::test]
async fn the_observation_carries_the_class_and_the_chains_and_a_refusal_carries_the_class_alone() {
    use phoebis_profile::fixtures::{
        body as fixture_body, inference_head, BodyFixture, FixtureKind,
    };
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "x-claude-code-request-class", "main");
    h.app.clone().oneshot(req).await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    let root: serde_json::Value = serde_json::from_slice(fixture_body(BodyFixture::Oauth)).unwrap();
    let chains = phoebis_relay::Chains::read(&root).expect("the fixture body is a JSON object");
    assert_eq!(observed.client.request_class.as_deref(), Some("main"));
    assert_eq!(observed.static_hash.as_deref(), chains.static_hash());
    assert_eq!(observed.boundaries, chains.message_boundaries.to_vec());
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        (pins[0].static_hash.as_deref(), pins[0].tail_hash.as_deref()),
        (chains.static_hash(), chains.tail_hash()),
        "the fresh pin's hashes are the request's"
    );

    // The class reaches the router: an auxiliary turn on the same session, with a body whose messages differ
    // but whose tools and system stay put, is untouched rather than judged cold on the matching static hash.
    // Without `request_class` reaching `SelectRequest`, the equal static hash alone would read as a restart and
    // restamp the pin to this body's own hashes.
    let mut auxiliary_body: serde_json::Value = root.clone();
    auxiliary_body["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"role": "user", "content": "a second, unrelated turn"}));
    let auxiliary_native = serde_json::to_vec(&auxiliary_body).unwrap();
    let mut auxiliary_req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &auxiliary_native,
        KEY,
    );
    set_header(&mut auxiliary_req, "x-claude-code-session-id", "s1");
    set_header(
        &mut auxiliary_req,
        "x-claude-code-request-class",
        "auxiliary",
    );
    h.app.clone().oneshot(auxiliary_req).await.unwrap();
    let _ = drain_until_complete(&mut sub).await;
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        (pins[0].static_hash.as_deref(), pins[0].tail_hash.as_deref()),
        (chains.static_hash(), chains.tail_hash()),
        "an auxiliary turn is untouched, so the pin keeps the first request's hashes"
    );

    // The compacted flag reaches the router: a request naming it is cold and restamps the pin to its own
    // hashes, whatever the static hash says. This body continues the pin's own tail (it is the auxiliary
    // request's body, unchanged), so the hash-restamp alone does not tell `Cold` from the continuation
    // fallback's own `Refresh`, which stamps identical hashes; `last_refresh_at` is the one field `renew` sets
    // differently between the two verdicts (0 for `Cold`, the clock for `Refresh`), so it is what actually
    // proves the flag was read.
    let compacted_root = auxiliary_body;
    let compacted_chains =
        phoebis_relay::Chains::read(&compacted_root).expect("the body is a JSON object");
    let compacted_native = serde_json::to_vec(&compacted_root).unwrap();
    let mut compacted_req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &compacted_native,
        KEY,
    );
    set_header(&mut compacted_req, "x-claude-code-session-id", "s1");
    set_header(
        &mut compacted_req,
        "x-claude-code-context-compacted",
        "manual",
    );
    h.app.clone().oneshot(compacted_req).await.unwrap();
    let _ = drain_until_complete(&mut sub).await;
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        (pins[0].static_hash.as_deref(), pins[0].tail_hash.as_deref()),
        (compacted_chains.static_hash(), compacted_chains.tail_hash()),
        "a compacted request restamps the pin to its own hashes"
    );
    assert_eq!(
        pins[0].last_refresh_at, 0,
        "cold zeroes the refresh; the continuation fallback's own Refresh would leave it at the clock instead"
    );

    // Without the compacted header: the request carries the class the client always sends and no compacted fact.
    let req = request_for(KEY, "claude-sonnet-4-5", "s2");
    h.app.clone().oneshot(req).await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(observed.client.request_class.as_deref(), Some("main"));
    assert_eq!(observed.client.context_compacted, None);

    // A refusal before the body is read reads the class off the head alone and carries no chain.
    let refused = request_for("phoebis-nobody", "claude-sonnet-4-5", "s3");
    h.app.clone().oneshot(refused).await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(observed.client.request_class.as_deref(), Some("main"));
    assert_eq!(observed.static_hash, None);
    assert!(observed.boundaries.is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejection_body_that_cannot_be_read_still_completes_the_request() {
    // A 429 whose body never arrives: the drain times out and the relay answers its own 502.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Stall {
            status: 429,
            headers: vec![],
            events: vec![],
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        bus,
        Limits {
            idle_timeout: Duration::from_millis(200),
            ..default_limits()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (502, "upstream_stalled")
    );
    let (events, _) = drain_until_complete(&mut sub).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            Event::RequestObserved(_) => "observed",
            Event::AccountSelected(_) => "selected",
            Event::ResponseHead(_) => "head",
            Event::RequestComplete(_) => "complete",
            Event::ResponseChunk(_) => "chunk",
        })
        .collect();
    assert_eq!(
        kinds,
        ["observed", "selected", "complete"],
        "no head was classified, so none is published; the early exit of respond still completes"
    );
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(
        (
            complete.attempt,
            complete.status,
            complete.error_code.as_deref(),
            complete.outcome
        ),
        (1, 502, Some("stream_stalled"), Outcome::Failed)
    );
    up.release();
    up.shutdown().await;
}

/// Chunks keep the drop-and-count behavior a subscriber that never reads cannot hold up; the completion is a
/// fact rather than a chunk, so it waits for the room the same subscriber has to give back before the stream
/// the client reads can close.
#[tokio::test]
async fn a_subscriber_that_never_reads_drops_chunks_but_the_completion_waits_for_room() {
    let events: Vec<SseEvent> = std::iter::once(ev("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#))
        .chain((0..4_096).map(|i| ev("content_block_delta", &format!(r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{i}"}}}}"#))))
        .chain(std::iter::once(ev("message_stop", r#"{"type":"message_stop"}"#)))
        .collect();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        // One flushed write per event, so each reaches the relay as its own read and its own chunk.
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: events.clone(),
            delay_between: Duration::from_millis(1),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut reader = bus.subscribe("reader", Delivery::Durable);
    let mut sleeper = bus.subscribe("sleeper", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut request = tokio::spawn(async move {
        let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
        resp.into_body().collect().await.unwrap().to_bytes()
    });
    // `publish_fact` walks the slots in subscribe order and awaits each, so the completion fact reaches the reader
    // before the publisher waits on the sleeper, and every chunk went by before the completion: the drop count is
    // settled the moment the reader holds the completion, with nothing slept. The wait is the stream's own length,
    // its 4,097 one-millisecond gaps, which the drop needs to overfill the queue.
    tokio::time::timeout(Duration::from_secs(60), up.written(0))
        .await
        .expect("the stream's response is written whole");
    drain_until_complete(&mut reader).await;
    let dropped = sleeper.stats.dropped.load(Ordering::SeqCst);
    assert!(dropped > 0, "the queue filled and dropped");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut request)
            .await
            .is_err(),
        "the completion waits for a subscriber that has given back no room"
    );
    while sleeper.events.try_recv().is_ok() {}
    let body = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("the completion lands once the sleeper's queue has room")
        .unwrap();
    let expected: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e.event, e.data))
        .collect();
    assert_eq!(
        body.as_ref(),
        expected.as_bytes(),
        "the client got every byte"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_relay_without_a_bus_publishes_nothing_and_still_answers() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert!(
        o.received_at <= o.head_at.unwrap(),
        "the outcome carries the timeline whether or not a bus is attached"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_buffered_body_the_bus_cannot_decode_publishes_no_chunk_and_no_usage() {
    // A rejection that names a coding its body does not use. The caller takes that coding as it is, so the
    // relay never decodes and only the bus copy would be wrong.
    let rejection = br#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#;
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 429,
            headers: vec![("content-encoding".into(), "gzip".into())],
            body: rejection.to_vec(),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    // A non-portable request keeps its account, so the rejection is forwarded instead of failing over.
    let mut req = non_portable_request(KEY, "s1");
    set_header(&mut req, "accept-encoding", "gzip");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 429);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        rejection,
        "the caller got the bytes unchanged"
    );
    let (events, chunks) = drain_until_complete(&mut sub).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            Event::RequestObserved(_) => "observed",
            Event::AccountSelected(_) => "selected",
            Event::ResponseHead(_) => "head",
            Event::RequestComplete(_) => "complete",
            Event::ResponseChunk(_) => "chunk",
        })
        .collect();
    assert_eq!(kinds, ["observed", "selected", "head", "complete"]);
    assert!(
        chunks.is_empty(),
        "a copy that will not decode is published as nothing"
    );
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert!(complete.usage.is_none());
    assert_eq!(complete.response_bytes, Some(body.len() as u64));
    up.shutdown().await;
}

#[tokio::test]
async fn a_stream_the_bus_cannot_decode_publishes_no_chunk_and_still_completes() {
    let sse = vec![
        ev(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
        ),
        ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ),
        ev("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![("content-encoding".into(), "gzip".into())],
            events: sse.clone(),
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut req = stream_request(KEY);
    set_header(&mut req, "accept-encoding", "gzip");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let expected: String = sse
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e.event, e.data))
        .collect();
    assert_eq!(
        body.as_ref(),
        expected.as_bytes(),
        "the caller got every byte"
    );
    let (events, chunks) = drain_until_complete(&mut sub).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            Event::RequestObserved(_) => "observed",
            Event::AccountSelected(_) => "selected",
            Event::ResponseHead(_) => "head",
            Event::RequestComplete(_) => "complete",
            Event::ResponseChunk(_) => "chunk",
        })
        .collect();
    assert_eq!(kinds, ["observed", "selected", "head", "complete"]);
    assert!(
        chunks.is_empty(),
        "not one piece decoded, so not one chunk is published"
    );
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert!(complete.usage.is_none());
    assert_eq!(
        complete.response_bytes,
        Some(body.len() as u64),
        "every piece the caller got still counts toward the size"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_pass_through_chunk_past_the_decoded_bound_reaches_the_caller_and_leaves_no_chunk_on_the_bus(
) {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&vec![0u8; 8 << 20]).unwrap();
    let bomb = encoder.finish().unwrap();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![
                ("content-encoding".into(), "gzip".into()),
                ("content-type".into(), "text/event-stream".into()),
            ],
            body: bomb.clone(),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "accept-encoding", "gzip");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("gzip")
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        bomb.as_slice(),
        "the caller gets the wire bytes untouched"
    );
    let (events, chunks) = drain_until_complete(&mut sub).await;
    assert!(
        chunks.is_empty(),
        "the expansion past the bound reaches no chunk"
    );
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!()
    };
    assert_eq!(complete.response_bytes, Some(bomb.len() as u64));
    up.shutdown().await;
}

/// The probe path authenticates by key id through an extension nothing on the socket can set, and publishes the
/// request as a keepalive probe: the ledger's `kind = probe` and the engine's own-probe filter both read that.
#[tokio::test]
async fn a_probe_is_authenticated_by_key_id_and_published_under_origin_keepalive() {
    use phoebis_profile::fixtures::{body as fixture_body, BodyFixture};
    let body_oauth = fixture_body(BodyFixture::Oauth);
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_p","type":"message","content":[],"usage":{"input_tokens":0,"cache_read_input_tokens":2000,"output_tokens":0}}"#.to_vec() },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    // No credential header at all: the engine holds the key's id, never its secret.
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    req.headers_mut().remove("authorization");
    req.extensions_mut().remove::<phoebis_relay::OriginalHead>();
    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
            },
            req,
        )
        .await;
    assert_eq!(resp.status(), 200);
    let out = outcome(&resp);
    assert_eq!(
        (
            out.key_id.as_deref(),
            out.account_id.as_str(),
            out.session_id.as_deref()
        ),
        (Some("desktop"), "a", Some("s1"))
    );
    let _ = resp.into_body().collect().await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Event::RequestObserved(observed) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(
        (observed.origin, observed.kind, observed.key_id.as_deref()),
        (
            phoebis_bus::Origin::Keepalive,
            phoebis_bus::Kind::Probe,
            Some("desktop")
        )
    );
    let Event::AccountSelected(selected) = &events[1] else {
        panic!("{events:?}")
    };
    assert!(
        selected.selection.is_none(),
        "a probe replays a recipe rather than making its own placement"
    );
    let sent = up.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].credential(),
        Some(A),
        "the account's own credential went upstream"
    );
    let names: Vec<&str> = sent[0].headers.iter().map(|(n, _)| n.as_str()).collect();
    let version = names
        .iter()
        .position(|n| n.eq_ignore_ascii_case("anthropic-version"))
        .unwrap();
    assert!(
        names[version + 1].eq_ignore_ascii_case("authorization"),
        "no recorded head, so the credential lands after anthropic-version: {names:?}"
    );
    // An unknown, revoked, or disabled id is the relay's 401 with nothing sent upstream.
    let probe = |key: &str| phoebis_relay::Probe {
        key_id: key.into(),
        key_generation: 0,
    };
    let refused = h
        .relay
        .probe(probe("nobody"), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()["x-phoebis-error-code"], "auth_unknown");
    h.store.revoke_api_key("desktop", NOW, "test").unwrap();
    h.router.reload().await.unwrap();
    assert_eq!(
        h.relay
            .probe(
                probe("desktop"),
                request_for(KEY, "claude-sonnet-4-5", "s1")
            )
            .await
            .status(),
        401
    );
    assert_eq!(up.requests().len(), 1);
    // Nothing on the socket can set the extension: a request through the router with a bogus bearer and no
    // secret is refused, whatever it claims.
    let served = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = served.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_relay::serve(
        served,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        phoebis_relay::ListenLimits {
            drain_timeout: Duration::from_secs(30),
            header_read_timeout: Duration::from_secs(30),
            max_connections: 64,
        },
        None,
    ));
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/v1/messages"))
        .header("authorization", "Bearer phoebis-not-a-key")
        .header("content-type", "application/json")
        .body(body_oauth.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "an extension set in process is not something the wire can carry"
    );
    let _ = stop.send(true);
    up.shutdown().await;
}

/// A probe carries the generation of the key its recipe was retained under; a key created again under the
/// same id is another key, so the old recipe's probe is refused before anything goes upstream.
#[tokio::test]
async fn a_probe_from_an_earlier_generation_of_a_reused_id_is_refused_before_anything_goes_upstream(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let live = h.store.api_key("desktop").unwrap().unwrap().created_at;
    let probe = |generation: i64| phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: generation,
    };
    let refused = h
        .relay
        .probe(probe(live - 1), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()["x-phoebis-error-code"], "auth_unknown");
    assert!(
        up.requests().is_empty(),
        "nothing reached the upstream under the replacement"
    );
    let served = h
        .relay
        .probe(probe(live), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(
        served.status(),
        200,
        "the recipe's own generation is served"
    );
    up.shutdown().await;
}

/// A 429 on the account a probe first reaches fails it over to the sibling a real request's failover would take,
/// whatever the sibling's kind.
#[tokio::test]
async fn a_probe_fails_over_to_an_oauth_sibling_as_a_real_request_does() {
    let up = FakeUpstream::start_tls_for(
        vec![Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        }],
        &["api.anthropic.com"],
    )
    .await;
    up.add_credential_rule(
        "sk-ant-api03-AAAA",
        "/v1/messages",
        json_429_unified(4_600, "five_hour"),
    );
    let h = harness_with_bus(
        &up,
        &[
            api_key("a", "sk-ant-api03-AAAA", 0),
            oauth("o", "sk-ant-oat01-OOOO", 1),
        ],
        PoolSpec::default(),
        Bus::new(),
    )
    .await;
    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
            },
            request_for(KEY, "claude-sonnet-4-5", "s1"),
        )
        .await;
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "o")
    );
    up.shutdown().await;
}

/// A probe held on its pinned member's pause answers the probe alone while a sibling could serve: the relay hands
/// the keepalive the obstacle, and the completion reads rejected rather than exhausted, so the gauge's pool exhaustion
/// condition, which reads only an exhausted completion, does not hold for a pool that can still serve.
#[tokio::test]
async fn a_probe_held_on_its_pinned_members_pause_completes_rejected_beside_a_sibling_that_can_serve(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[
            api_key("a", "sk-ant-api03-AAAA", 0),
            api_key("b", "sk-ant-api03-BBBB", 1),
        ],
        PoolSpec::default(),
        bus,
    )
    .await;
    let real = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&real).account_id, "a", "the session pins on a");
    real.into_body().collect().await.unwrap();
    drain_until_complete(&mut sub).await;
    h.store.set_enabled("a", false).unwrap();
    h.router.reload().await.unwrap();

    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
            },
            request_for(KEY, "claude-sonnet-4-5", "s1"),
        )
        .await;
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap(),
            outcome(&resp).obstacle,
        ),
        (503, "accounts_unavailable", Some("operator")),
        "the probe is answered on its pin's pause, with the obstacle for the keepalive"
    );
    resp.into_body().collect().await.unwrap();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events.last() else {
        panic!("last event: {:?}", events.last())
    };
    assert_eq!(
        complete.outcome,
        Outcome::Rejected,
        "a held pin is no exhaustion of a pool whose sibling can serve"
    );
    assert_eq!(
        credentials_seen(&up),
        vec!["sk-ant-api03-AAAA".to_string()],
        "the probe never reached b"
    );
    up.shutdown().await;
}

/// A streamed answer written at the 1-hour tier: the start names the split, the final delta repeats the write as one
/// total with no tier, and the completion keeps the split rather than filing the total as a second write.
#[tokio::test]
async fn a_deltas_bare_write_total_does_not_double_the_starts_split() {
    let events = vec![
        ev(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":2,"cache_read_input_tokens":50363,"cache_creation_input_tokens":11285,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":11285},"output_tokens":1}}}"#,
        ),
        ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ),
        ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":2,"cache_read_input_tokens":50363,"cache_creation_input_tokens":11285,"output_tokens":90}}"#,
        ),
        ev("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events,
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap().to_bytes();
    let (events, _) = drain_until_complete(&mut sub).await;
    let Some(Event::RequestComplete(complete)) = events
        .iter()
        .find(|e| matches!(e, Event::RequestComplete(_)))
    else {
        panic!("no completion")
    };
    let usage = complete.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.cache_write_5m_tokens,
            usage.cache_write_1h_tokens,
            usage.output_tokens
        ),
        (Some(2), Some(50_363), Some(0), Some(11_285), Some(90)),
        "the 1-hour write is not copied into the 5-minute column"
    );
    up.shutdown().await;
}
