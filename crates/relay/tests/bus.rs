//! What the relay tells the bus, request by request.
mod common;
use std::sync::atomic::Ordering;
use std::time::Duration;

use phoebis_bus::{AttemptOutcome, Bus, Event, Outcome, Phase, ResponseChunk, Subscriber};
use phoebis_relay::Limits;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";

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
    let mut sub = bus.subscribe("test");
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
        (Some("desktop"), Some("main"), true, "claude-sonnet-4-5")
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
    let mut sub = bus.subscribe("test");
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
async fn a_gzipped_json_200_the_caller_accepts_as_is_still_yields_usage_and_a_huge_one_does_not() {
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
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    req.headers_mut()
        .insert("accept-encoding", "gzip".parse().unwrap());
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
    // Past the inspection bound the usage is unknown, and the client still gets every byte.
    let padding = format!(
        r#"{{"id":"msg_4","type":"message","pad":"{}","usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
        "x".repeat(70 * 1024)
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
    assert!(
        complete.usage.is_none(),
        "a body past 64 KiB is not inspected"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_sse_error_after_the_head_a_lost_upstream_and_a_cancelled_account_each_end_as_a_failure()
{
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
    let mut sub = bus.subscribe("test");
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
        (Outcome::Failed, Some("upstream_unreachable"))
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

#[tokio::test]
async fn refused_and_exhausted_requests_publish_an_observed_event_and_their_completion() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_unified(4_600, "five_hour"),
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            allowed_models: Some(vec!["claude-sonnet-4-5"]),
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
    let mut sub = bus.subscribe("test");
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
    assert_eq!(resp.status(), 502);
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
        (1, 502, Some("upstream_unreachable"), Outcome::Failed)
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_subscriber_that_never_reads_fills_its_queue_and_delays_nothing() {
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
    let sleeper = bus.subscribe("sleeper");
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let started = std::time::Instant::now();
    let resp = h.app.clone().oneshot(stream_request(KEY)).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let expected: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e.event, e.data))
        .collect();
    assert_eq!(
        body.as_ref(),
        expected.as_bytes(),
        "the client got every byte"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a full subscriber queue did not hold the stream"
    );
    assert!(
        sleeper.stats.dropped.load(Ordering::SeqCst) > 0,
        "the queue filled and dropped"
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
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    // A non-portable request keeps its account, so the rejection is forwarded instead of failing over.
    let mut req = non_portable_request(KEY, "s1");
    req.headers_mut()
        .insert("accept-encoding", "gzip".parse().unwrap());
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
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let mut req = stream_request(KEY);
    req.headers_mut()
        .insert("accept-encoding", "gzip".parse().unwrap());
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
