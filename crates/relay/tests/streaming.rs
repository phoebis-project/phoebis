use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;

#[tokio::test]
async fn sse_is_relayed_event_by_event_with_pings_and_unknown_events_intact() {
    let events = vec![
        SseEvent { event: "ping".into(), data: r#"{"type":"ping"}"#.into() },
        SseEvent { event: "message_start".into(), data: r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":2}}}"#.into() },
        SseEvent { event: "weird_future_event".into(), data: r#"{"type":"weird_future_event"}"#.into() },
        SseEvent { event: "content_block_delta".into(), data: r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#.into() },
        SseEvent { event: "message_stop".into(), data: r#"{"type":"message_stop"}"#.into() },
    ];
    // Gated: the fake writes the first event and then blocks until release(), so a relay that buffered the whole
    // response could not produce a frame at all. A timing bound would only say the relay was fast.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::SseGated {
            status: 200,
            headers: vec![("request-id".into(), "req_up_1".into())],
            events: events.clone(),
        },
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_up_1");
    let mut body = resp.into_body();
    let mut seen = String::new();
    while !seen.ends_with("\n\n") {
        let frame = body
            .frame()
            .await
            .expect("the first event arrives while the rest is unwritten")
            .unwrap();
        seen.push_str(std::str::from_utf8(&frame.into_data().unwrap()).unwrap());
    }
    assert_eq!(
        seen,
        format!("event: {}\ndata: {}\n\n", events[0].event, events[0].data)
    );
    up.release();
    while let Some(frame) = body.frame().await {
        seen.push_str(std::str::from_utf8(&frame.unwrap().into_data().unwrap()).unwrap());
    }
    let expected: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e.event, e.data))
        .collect();
    assert_eq!(
        seen, expected,
        "bytes, order, pings, and unknown events preserved"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn upstream_429_and_529_are_forwarded_with_their_headers_and_body() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 429,
            headers: vec![
                ("retry-after".into(), "7".into()),
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "rejected".into(),
                ),
            ],
            body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
                .to_vec(),
        },
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "7");
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-status")
            .unwrap(),
        "rejected"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"{\"type\":\"error\""));
    up.shutdown().await;
}

#[tokio::test]
async fn a_401_triggers_one_refresh_and_one_replay() {
    // First /v1/messages answers 401, the refresh succeeds, the replay answers 200.
    let up = FakeUpstream::start(vec![
        Rule { path_prefix: "/v1/oauth/token".into(), respond: Respond::Json { status: 200, headers: vec![], body: phoebis_profile::fixtures::control_response(phoebis_profile::fixtures::ControlFixture::Token).to_vec() } },
        Rule { path_prefix: "/v1/messages".into(), respond: Respond::Json { status: 401, headers: vec![], body: br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#.to_vec() } },
    ]).await;
    up.set_rule_after(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let lines: Vec<String> = up.requests().iter().map(|r| r.line.clone()).collect();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("POST /v1/messages"))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("POST /v1/oauth/token"))
            .count(),
        1
    );
    assert!(
        up.requests()[2]
            .headers
            .iter()
            .any(|(n, v)| n == "Authorization" && v == "Bearer sk-ant-oat01-PLACEHOLDER"),
        "the replay carries the refreshed token"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_lost_response_after_the_body_was_written_is_not_replayed() {
    // CloseAfterRequest, not DropAfterHead: the upstream received the whole request and answered nothing at all,
    // which is exactly the case where a replay could double-bill.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::CloseAfterRequest,
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "upstream_unreachable"
    );
    let recorded = up.requests();
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0].complete,
        "the upstream read the request in full before closing"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_connection_that_dies_before_any_byte_is_retried_once_on_a_fresh_one() {
    // The first connection is closed at accept, which is what a pooled connection the upstream already dropped
    // looks like to the client: nothing was written and nothing can have been billed.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::CloseImmediately,
    }])
    .await;
    up.set_rule_after_connections(
        1,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.connections(), 2, "one dead connection, one retry");
    let recorded = up.requests();
    assert_eq!(
        recorded.len(),
        1,
        "the dead connection never carried a request"
    );
    assert!(recorded[0].complete);
    up.shutdown().await;
}

#[tokio::test]
async fn keep_alive_reuses_the_upstream_connection() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(common::stream_request("phoebis-testkey"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // A connection returns to the pool only once its body has been read to the end.
        resp.into_body().collect().await.unwrap();
    }
    assert_eq!(
        up.connections(),
        1,
        "three requests, one upstream connection"
    );
    up.shutdown().await;
}
