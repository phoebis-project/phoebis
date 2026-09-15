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
    // One attempt in the budget, so the rejection is committed to the caller rather than failed over: with a
    // budget left, a rate-limited pool with nothing else to select answers its own exhaustion instead.
    let app = common::relay_with_limits(
        phoebis_store::AccountKind::Oauth,
        &up,
        phoebis_relay::Limits {
            max_attempts: 1,
            ..common::default_limits()
        },
    )
    .await;
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
async fn a_chunked_rejection_is_read_and_replayed() {
    // A real upstream does not have to declare a length; the rejection still has to be read and acted on.
    let up = FakeUpstream::start(vec![
        Rule { path_prefix: "/v1/oauth/token".into(), respond: Respond::Json { status: 200, headers: vec![], body: phoebis_profile::fixtures::control_response(phoebis_profile::fixtures::ControlFixture::Token).to_vec() } },
        Rule { path_prefix: "/v1/messages".into(), respond: Respond::Chunked { status: 401, headers: vec![], body: br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#.to_vec() } },
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
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejection_too_large_to_read_is_forwarded_rather_than_replayed() {
    // The relay cannot tell whether this rejection is the one that warrants a refresh without holding 200 KiB
    // of it in memory, so the caller gets the upstream's own answer instead of a local error.
    let body = format!(
        r#"{{"type":"error","error":{{"type":"authentication_error","message":"{}"}}}}"#,
        "x".repeat(200 * 1024)
    );
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 401,
            headers: vec![],
            body: body.clone().into_bytes(),
        },
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        bytes,
        body.as_bytes(),
        "the whole rejection reaches the caller, byte for byte"
    );
    assert_eq!(
        up.requests().len(),
        1,
        "an unread rejection triggers no refresh and no replay"
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

/// The body the fake serves compressed in both tests below.
const COMPRESSIBLE: &[u8] =
    br#"{"id":"msg_1","type":"message","content":[{"type":"text","text":"aaaaaaaaaaaaaaaaaaaa"}]}"#;

#[tokio::test]
async fn a_gzipped_response_stays_compressed_for_a_caller_that_reads_gzip() {
    let gzipped = gzip(COMPRESSIBLE);
    let up = gzip_upstream(gzipped.clone()).await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-encoding").unwrap(), "gzip");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        gzipped.as_slice(),
        "a caller that reads gzip gets the upstream payload untouched"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_gzipped_response_is_decoded_for_a_caller_that_refuses_it() {
    let up = gzip_upstream(gzip(COMPRESSIBLE)).await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let mut request = common::stream_request("phoebis-testkey");
    request.headers_mut().insert(
        axum::http::header::ACCEPT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("content-encoding").is_none(),
        "a decoded payload must not claim an encoding"
    );
    let declared = resp
        .headers()
        .get("content-length")
        .map(|value| value.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), COMPRESSIBLE);
    assert!(
        declared.is_none_or(|declared| declared == COMPRESSIBLE.len().to_string()),
        "no length describing the compressed body survives the decode"
    );
    up.shutdown().await;
}

async fn gzip_upstream(body: Vec<u8>) -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            body,
        },
    }])
    .await
}

fn gzip(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(payload)
        .expect("a Vec never fails a write");
    encoder.finish().expect("the encoder flushes into the Vec")
}

#[tokio::test]
async fn a_dead_pooled_connection_is_discarded_rather_than_handed_out() {
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
    let first = app
        .clone()
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    // A connection returns to the pool only once its body has been read to the end.
    first.into_body().collect().await.unwrap();
    assert_eq!(
        up.connections(),
        1,
        "the first request opened one connection"
    );
    up.close_idle_and_wait().await;
    let second = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        200,
        "the request went out on a connection the pool opened for it"
    );
    second.into_body().collect().await.unwrap();
    assert_eq!(
        up.requests().len(),
        2,
        "the dead pooled connection carried nothing, so the upstream saw the second request once"
    );
    assert_eq!(
        up.connections(),
        2,
        "the pool opened a connection rather than handing out the dead one"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_upstream_that_reads_a_request_and_closes_is_not_retried() {
    // The pool hands out a connection it just probed, so the close can only have happened after the upstream
    // read the request: it may have acted on it, and a second send would bill the caller twice.
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
    let first = app
        .clone()
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    // A connection returns to the pool only once its body has been read to the end.
    first.into_body().collect().await.unwrap();
    up.set_rule_after(1, "/v1/messages", Respond::CloseAfterRequest);
    let second = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(second.status(), 502);
    assert_eq!(
        second.headers().get("x-phoebis-error-code").unwrap(),
        "upstream_unreachable"
    );
    let outcome = second
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .expect("a relayed response carries its outcome")
        .clone();
    assert!(outcome.written, "the request went out in full");
    assert_eq!(outcome.attempt, 1, "a written request is never sent twice");
    assert_eq!(
        up.requests().len(),
        2,
        "the upstream saw the second request once"
    );
    assert_eq!(
        up.connections(),
        1,
        "the second request rode the pooled connection"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_gzipped_401_is_decoded_before_it_is_classified() {
    // The rejection that asks for a refresh says so in JSON, so a compressed one used to read as unclassifiable
    // and reach the caller untouched.
    let up = FakeUpstream::start(vec![
        Rule { path_prefix: "/v1/oauth/token".into(), respond: Respond::Json { status: 200, headers: vec![], body: phoebis_profile::fixtures::control_response(phoebis_profile::fixtures::ControlFixture::Token).to_vec() } },
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 401,
                headers: vec![("Content-Encoding".into(), "gzip".into())],
                body: gzip(br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#),
            },
        },
    ])
    .await;
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
    assert_eq!(resp.status(), 200, "the caller gets the replay's answer");
    let lines: Vec<String> = up.requests().iter().map(|r| r.line.clone()).collect();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("POST /v1/messages"))
            .count(),
        2,
        "one rejection and one replay"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("POST /v1/oauth/token"))
            .count(),
        1,
        "one refresh"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_timeout_with_the_request_in_the_socket_is_not_retried() {
    // The upstream reads the whole request and then says nothing at all, which is the shape of a request that
    // may already have been acted on: the relay has to give up on it rather than send it a second time.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        })),
    }])
    .await;
    let app = common::relay_with_head_timeout(
        phoebis_store::AccountKind::Oauth,
        &up,
        std::time::Duration::from_millis(100),
    )
    .await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "upstream_unreachable"
    );
    let outcome = resp
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .expect("a relayed response carries its outcome")
        .clone();
    assert!(
        outcome.written,
        "the request reached the upstream, so it counts as possibly billed"
    );
    assert_eq!(outcome.attempt, 1);
    let recorded = up.requests();
    assert_eq!(recorded.len(), 1, "the timed-out request is not sent again");
    assert!(
        recorded[0].complete,
        "the upstream read it in full before the relay gave up"
    );
    up.shutdown().await;
}
