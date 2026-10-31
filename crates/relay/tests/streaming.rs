use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use std::time::{Duration, Instant};
use tower::ServiceExt;

mod common;

fn ping_event() -> SseEvent {
    SseEvent {
        event: "ping".into(),
        data: r#"{"type":"ping"}"#.into(),
    }
}

fn message_start_event() -> SseEvent {
    SseEvent {
        event: "message_start".into(),
        data: r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":2}}}"#.into(),
    }
}

fn weird_future_event() -> SseEvent {
    SseEvent {
        event: "weird_future_event".into(),
        data: r#"{"type":"weird_future_event"}"#.into(),
    }
}

fn text_delta_event(text: &str) -> SseEvent {
    SseEvent {
        event: "content_block_delta".into(),
        data: format!(
            r#"{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{text}"}}}}"#
        ),
    }
}

fn message_stop_event() -> SseEvent {
    SseEvent {
        event: "message_stop".into(),
        data: r#"{"type":"message_stop"}"#.into(),
    }
}

#[tokio::test]
async fn sse_is_relayed_event_by_event_with_pings_and_unknown_events_intact() {
    let events = vec![
        ping_event(),
        message_start_event(),
        weird_future_event(),
        text_delta_event("hi"),
        message_stop_event(),
    ];
    // Gated: the fake writes the first event and then blocks until release(). The relay holds that ping until
    // the stream commits, so the response head itself cannot arrive before the release.
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
    let pending = tokio::spawn(async move {
        app.oneshot(common::stream_request("phoebis-testkey"))
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !pending.is_finished(),
        "the ping alone commits nothing, so the caller has no head yet"
    );
    up.release();
    let resp = pending.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_up_1");
    let seen = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
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
    // A request bound to its account commits the rejection to the caller as it came; a portable one would fail
    // over to the pool's own exhaustion answer.
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::non_portable_request(common::KEY, "s1"))
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
async fn a_401_over_the_buffer_bound_is_refreshed_and_replayed_as_a_small_one_is() {
    // Classified from its first 64 KiB, where the authentication error asks for the same refresh a small 401 gets.
    let body = format!(
        r#"{{"type":"error","error":{{"type":"authentication_error","message":"{}"}}}}"#,
        "x".repeat(200 * 1024)
    );
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: phoebis_profile::fixtures::control_response(
                    phoebis_profile::fixtures::ControlFixture::Token,
                )
                .to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 401,
                headers: vec![],
                body: body.into_bytes(),
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
    let bus = phoebis_bus::Bus::new();
    let mut sub = bus.subscribe("test", phoebis_bus::Delivery::Durable);
    // The token fixture names `ACCOUNT_UUID`, so the refresh rotates rather than refusing a mismatched identity.
    let h = common::harness_with_bus(
        &up,
        &[common::TestAccount {
            uuid: Some(common::ACCOUNT_UUID),
            ..common::oauth("a", "sk-ant-oat01-AAAA", 0)
        }],
        common::PoolSpec::default(),
        bus,
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the caller gets the replay's answer");
    resp.into_body().collect().await.unwrap();
    let lines: Vec<String> = up.requests().iter().map(|r| r.line.clone()).collect();
    assert_eq!(
        (
            lines
                .iter()
                .filter(|l| l.starts_with("POST /v1/messages"))
                .count(),
            lines
                .iter()
                .filter(|l| l.starts_with("POST /v1/oauth/token"))
                .count()
        ),
        (2, 1),
        "one rejection, one refresh and one replay"
    );
    let events = common::drain_until_complete(&mut sub).await;
    let heads: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            phoebis_bus::Event::ResponseHead(head) => Some((head.status, head.outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        heads,
        [
            (401, phoebis_bus::AttemptOutcome::AuthFailed),
            (200, phoebis_bus::AttemptOutcome::Succeeded)
        ]
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

/// The relay's wait before it retries a request whose connection could not be opened.
const CONNECT_RETRY_WAIT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn a_connection_that_cannot_be_opened_is_retried_once_after_the_wait() {
    // The first connection is closed at accept, so its handshake fails: no connection was opened, nothing was
    // written and nothing can have been billed. Real time rather than the test clock, since the wait sits before a
    // real socket's dial.
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
    let started = Instant::now();
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(resp.status(), 200);
    assert!(
        elapsed >= CONNECT_RETRY_WAIT,
        "the retry waits before it dials again, answered after {elapsed:?}"
    );
    let outcome = resp
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .expect("a relayed response carries its outcome");
    assert_eq!(outcome.attempt, 2, "the failed dial and the one retry");
    let recorded = up.requests();
    assert_eq!(
        recorded.len(),
        1,
        "the closed connection never carried a request"
    );
    assert!(recorded[0].complete);
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_refused_on_both_connects_answers_502_after_one_wait() {
    // The listener is bound and dropped before the request, so both connects are refused. An API-key account, so
    // the relay calls no control plane on the dead address.
    let up = FakeUpstream::start(Vec::new()).await;
    let app = common::relay_with(phoebis_store::AccountKind::ApiKey, &up).await;
    up.shutdown().await;
    let started = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        app.oneshot(common::stream_request("phoebis-testkey")),
    )
    .await
    .expect("two refused connects and one wait end the request well inside the bound")
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "upstream_unreachable"
    );
    eprintln!("two refused connects answered after {elapsed:?}");
    assert!(
        elapsed >= CONNECT_RETRY_WAIT && elapsed < 2 * CONNECT_RETRY_WAIT,
        "one wait between the two connects and none after the second, answered after {elapsed:?}"
    );
    let outcome = resp
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .expect("a relayed response carries its outcome");
    assert_eq!(outcome.attempt, 2, "no third attempt after the retry");
    assert!(!outcome.written, "neither connect carried a byte");
}

#[tokio::test]
async fn the_accounts_removal_during_the_connect_wait_answers_account_canceled_with_no_retry() {
    // Every connection is closed at accept, so the first dial fails its handshake and the wait starts. The relay's
    // debug line right before the wait is the signal to remove the account, and the removal and the reload take
    // milliseconds on this harness, well inside the one-second wait.
    let captured = common::Captured::default();
    let _log = captured.install_at(tracing::Level::DEBUG);
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::CloseImmediately,
    }])
    .await;
    let (h, _sub) = bus_harness(&up, common::default_limits()).await;
    let inflight = tokio::spawn(h.app.clone().oneshot(common::stream_request(common::KEY)));
    fake_upstream::wait_for(Duration::from_secs(5), || {
        captured
            .text()
            .contains("retrying a request that never reached the upstream")
            .then_some(())
    })
    .await;
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(5), inflight)
        .await
        .expect("the removal ends the request")
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "account_canceled"
    );
    let outcome = resp
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .expect("a relayed response carries its outcome");
    assert_eq!(
        outcome.attempt, 1,
        "the cancellation ended the wait, so the removed account was not dialed again"
    );
    assert!(!outcome.written, "the failed dial carried no byte");
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
    // The relay's own count, since the fake's accepts would count a connect another process on the host opened.
    let h = common::one_account_harness(
        phoebis_store::AccountKind::Oauth,
        &up,
        common::default_limits(),
    )
    .await;
    for _ in 0..3 {
        let resp = h
            .app
            .clone()
            .oneshot(common::stream_request("phoebis-testkey"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // A connection returns to the pool only once its body has been read to the end.
        resp.into_body().collect().await.unwrap();
    }
    assert_eq!(
        h.relay.connections_opened(),
        1,
        "three requests, one upstream connection"
    );
    up.shutdown().await;
}

/// A body the fake serves compressed.
const COMPRESSIBLE: &[u8] =
    br#"{"id":"msg_1","type":"message","content":[{"type":"text","text":"aaaaaaaaaaaaaaaaaaaa"}]}"#;

/// A committed answer with its usage block, which the relay reads whatever coding carried it.
const ANSWER_WITH_USAGE: &[u8] = br#"{"id":"msg_coded","type":"message","content":[{"type":"text","text":"aaaaaaaaaaaaaaaaaaaa"}],"usage":{"input_tokens":5,"output_tokens":7}}"#;

#[tokio::test]
async fn a_gzipped_response_arrives_as_identity_with_its_usage_read_whatever_the_caller_accepts() {
    let (coding, body, complete, attempt) = one_coded_answer("gzip", gzip(ANSWER_WITH_USAGE)).await;
    assert_eq!(
        coding, None,
        "a caller that reads gzip still gets the answer decoded"
    );
    assert_eq!(body.as_ref(), ANSWER_WITH_USAGE);
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.interruption
        ),
        (phoebis_bus::Outcome::Succeeded, None, None)
    );
    assert_eq!(
        (attempt.input_tokens, attempt.output_tokens),
        (Some(5), Some(7)),
        "the attempt's row carries the usage the decoded answer named"
    );
}

#[tokio::test]
async fn a_deflated_response_arrives_as_identity_with_its_usage_read_whatever_the_caller_accepts() {
    let (coding, body, complete, attempt) =
        one_coded_answer("deflate", deflate(ANSWER_WITH_USAGE)).await;
    assert_eq!(
        coding, None,
        "a caller that reads deflate still gets the answer decoded"
    );
    assert_eq!(body.as_ref(), ANSWER_WITH_USAGE);
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.interruption
        ),
        (phoebis_bus::Outcome::Succeeded, None, None)
    );
    assert_eq!(
        (attempt.input_tokens, attempt.output_tokens),
        (Some(5), Some(7)),
        "the attempt's row carries the usage the decoded answer named"
    );
}

/// A committed JSON answer served as `body` under `coding` to a caller whose `Accept-Encoding` names that coding: the
/// coding the client was answered in, the body it read, the completion the bus carried and the attempt row the ledger
/// files from the request's events.
async fn one_coded_answer(
    coding: &str,
    body: Vec<u8>,
) -> (
    Option<String>,
    Bytes,
    phoebis_bus::RequestComplete,
    phoebis_ledger::AttemptRecord,
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("Content-Encoding".into(), coding.into())],
            body,
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let mut request = common::stream_request(common::KEY);
    common::set_header(&mut request, "accept-encoding", coding);
    let resp = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), 200);
    let answered_in = resp
        .headers()
        .get("content-encoding")
        .map(|value| value.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let events = common::drain_until_complete(&mut sub).await;
    let complete = events
        .iter()
        .find_map(|e| match e {
            phoebis_bus::Event::RequestComplete(complete) => Some(complete.clone()),
            _ => None,
        })
        .expect("the request completed");
    let mut assembler = phoebis_ledger::Assembler::new(std::sync::Arc::new(|| 0));
    let attempt = events
        .into_iter()
        .flat_map(|e| assembler.observe(e))
        .find_map(|record| match record {
            phoebis_ledger::Record::Request { mut attempts, .. } => attempts.pop(),
            _ => None,
        })
        .expect("the request's ledger row with its attempt");
    up.shutdown().await;
    (answered_in, body, complete, attempt)
}

#[tokio::test]
async fn a_gzipped_response_is_decoded_for_a_caller_that_refuses_it() {
    let up = gzip_upstream(gzip(COMPRESSIBLE)).await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let mut request = common::stream_request("phoebis-testkey");
    common::set_header(&mut request, "accept-encoding", "identity");
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
        respond: gzipped_json(body),
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

/// `payload` under the HTTP `deflate` coding, which is the zlib format.
fn deflate(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
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
    // The relay's own count, since the fake's accepts would count a connect another process on the host opened.
    let h = common::one_account_harness(
        phoebis_store::AccountKind::Oauth,
        &up,
        common::default_limits(),
    )
    .await;
    let first = h
        .app
        .clone()
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    // A connection returns to the pool only once its body has been read to the end.
    first.into_body().collect().await.unwrap();
    assert_eq!(
        h.relay.connections_opened(),
        1,
        "the first request opened one connection"
    );
    up.close_idle_and_wait().await;
    let second = h
        .app
        .clone()
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
        h.relay.connections_opened(),
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

#[tokio::test]
async fn a_stalled_handshake_answers_502_within_two_connect_bounds_and_the_wait() {
    // The listener accepts and never answers the ClientHello. Real time rather than the test clock: the bound
    // under test is a timer racing a socket, and a paused clock would jump straight past the socket wait.
    let up = FakeUpstream::start_silent().await;
    let connect_timeout = Duration::from_secs(1);
    let app = common::relay_with_limits(
        phoebis_store::AccountKind::ApiKey,
        &up,
        phoebis_relay::Limits {
            connect_timeout,
            head_timeout: Duration::from_secs(60),
            ..common::default_limits()
        },
    )
    .await;
    let started = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        app.oneshot(common::stream_request("phoebis-testkey")),
    )
    .await
    .expect("two connect bounds and the wait end the request long before the head timeout")
    .unwrap();
    let elapsed = started.elapsed();
    eprintln!("a stalled handshake answered after {elapsed:?}");
    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "upstream_unreachable"
    );
    assert!(
        elapsed >= 2 * connect_timeout + CONNECT_RETRY_WAIT,
        "each dial waits out the connect bound, with the wait between them, answered after {elapsed:?}"
    );
    assert_eq!(
        up.connections(),
        2,
        "the first dial and the one retry on a fresh connection"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_handshake_stalled_past_the_head_bound_answers_502_within_two_head_bounds_and_the_wait() {
    // The head bound is the shorter one here, so it expires while the handshake is still running and nothing was
    // sent: a connection that could not be opened, as the connect bound's expiry is.
    let up = FakeUpstream::start_silent().await;
    let head_timeout = Duration::from_secs(1);
    let app = common::relay_with_limits(
        phoebis_store::AccountKind::ApiKey,
        &up,
        phoebis_relay::Limits {
            connect_timeout: Duration::from_secs(60),
            head_timeout,
            ..common::default_limits()
        },
    )
    .await;
    let started = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        app.oneshot(common::stream_request("phoebis-testkey")),
    )
    .await
    .expect("two head bounds and the wait end the request long before the connect timeout")
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(resp.status(), 502);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "upstream_unreachable"
    );
    assert!(
        elapsed >= 2 * head_timeout + CONNECT_RETRY_WAIT,
        "each dial waits out the head bound, with the wait between them, answered after {elapsed:?}"
    );
    let outcome = resp
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .expect("a relayed response carries its outcome");
    assert!(
        !outcome.written,
        "nothing was sent before either bound expired"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_upstreams_request_id_rides_on_the_outcome_of_the_committed_attempt() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_0001".into())],
            body: b"{\"id\":\"msg_01\",\"type\":\"message\"}".to_vec(),
        },
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(
        common::outcome(&resp).upstream_request_id.as_deref(),
        Some("req_up_0001")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_compressed_chunk_that_expands_past_the_bound_ends_the_stream_with_an_error() {
    let bomb = gzip(&vec![b'a'; 8 << 20]);
    let up = gzip_upstream(bomb).await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let mut request = common::stream_request("phoebis-testkey");
    common::set_header(&mut request, "accept-encoding", "identity");
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await;
    assert!(
        body.is_err(),
        "the body ended in an error rather than a multi-megabyte chunk"
    );
    up.shutdown().await;
}

/// A request whose body sends a prefix and then stalls, on a relay with `limits`; the sender keeps it open.
async fn stalled_body(
    limits: phoebis_relay::Limits,
) -> (
    FakeUpstream,
    common::Harness,
    Request<Body>,
    tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = common::harness_with_limits(
        &up,
        &[common::oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        common::PoolSpec::default(),
        limits,
    )
    .await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    tx.send(Ok(Bytes::from_static(b"{\"model\":")))
        .await
        .unwrap();
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let request = Request::post("/v1/messages")
        .header("authorization", format!("Bearer {}", common::KEY))
        .header("content-type", "application/json")
        .header("user-agent", common::CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", common::fresh_session())
        .header("x-claude-code-request-class", "main")
        .body(body)
        .unwrap();
    (up, h, request, tx)
}

#[tokio::test]
async fn a_body_that_stops_arriving_is_refused_at_the_body_read_deadline() {
    let (up, h, request, tx) = stalled_body(phoebis_relay::Limits {
        body_read_timeout: Duration::from_millis(300),
        ..common::default_limits()
    })
    .await;
    let resp = tokio::time::timeout(Duration::from_secs(5), h.app.clone().oneshot(request))
        .await
        .expect("refused at the body read deadline")
        .unwrap();
    assert_eq!(resp.status(), 408);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "request_timeout"
    );
    drop(tx);
    up.shutdown().await;
}

#[tokio::test]
async fn a_stalled_body_is_refused_by_the_read_deadline_and_not_the_stream_idle_bound() {
    let (up, h, request, tx) = stalled_body(phoebis_relay::Limits {
        body_read_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_millis(300),
        ..common::default_limits()
    })
    .await;
    let waited =
        tokio::time::timeout(Duration::from_millis(600), h.app.clone().oneshot(request)).await;
    assert!(
        waited.is_err(),
        "the body still had its own deadline to arrive in past the idle bound"
    );
    drop(tx);
    up.shutdown().await;
}

#[tokio::test]
async fn a_relayed_stream_is_delivered_frame_by_frame_as_the_upstream_writes() {
    let events = vec![
        message_start_event(),
        text_delta_event("hi"),
        message_stop_event(),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events,
            delay_between: Duration::from_millis(400),
        },
    }])
    .await;
    let app = common::relay_with(phoebis_store::AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(common::stream_request("phoebis-testkey"))
        .await
        .unwrap();
    let mut body = resp.into_body();
    let started = Instant::now();
    let first = body.frame().await.unwrap().unwrap();
    assert!(first
        .data_ref()
        .unwrap()
        .starts_with(b"event: message_start"));
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "the first event was not held for the second"
    );
    let second = body.frame().await.unwrap().unwrap();
    assert!(second
        .data_ref()
        .unwrap()
        .starts_with(b"event: content_block_delta"));
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "the second arrived with the upstream's delay"
    );
    up.shutdown().await;
}

/// A harness with a bus on `limits`, and the subscriber that reads what the relay published.
async fn bus_harness(
    up: &FakeUpstream,
    limits: phoebis_relay::Limits,
) -> (common::Harness, phoebis_bus::Subscriber) {
    let bus = phoebis_bus::Bus::new();
    let sub = bus.subscribe("test", phoebis_bus::Delivery::Durable);
    let h = common::harness_with_bus_limits(
        up,
        &[common::oauth("a", "sk-ant-oat01-AAAA", 0)],
        common::PoolSpec::default(),
        bus,
        limits,
    )
    .await;
    (h, sub)
}

/// The completion the body task published for the one request the test sent.
async fn completion(sub: &mut phoebis_bus::Subscriber) -> phoebis_bus::RequestComplete {
    common::drain_until_complete(sub)
        .await
        .into_iter()
        .find_map(|e| match e {
            phoebis_bus::Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .expect("the request completed")
}

fn rendered(events: &[SseEvent]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e.event, e.data))
        .collect()
}

#[tokio::test]
async fn a_stream_closed_cleanly_after_message_start_without_message_stop_reads_incomplete() {
    let events = vec![message_start_event(), text_delta_event("hi")];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: events.clone(),
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        rendered(&events),
        "the client gets every byte the upstream sent"
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Incomplete, Some("stream_incomplete")),
        "a clean close before message_stop is not a success"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_stream_closed_cleanly_before_its_stop_is_warned_of_nothing_and_carries_no_interruption()
{
    let captured = common::Captured::default();
    let _log = captured.install();
    let (status, cut, complete) = one_stream(
        Respond::Sse {
            status: 200,
            headers: with_request_id(),
            events: vec![message_start_event(), text_delta_event("hi")],
            delay_between: Duration::from_millis(2),
        },
        common::default_limits(),
    )
    .await;
    assert_eq!((status, cut), (200, false));
    assert_eq!(
        (complete.error_code.as_deref(), complete.interruption),
        (Some("stream_incomplete"), None),
        "a clean end has no error to name"
    );
    assert!(
        !captured.text().contains(CUT_WARNING),
        "{}",
        captured.text()
    );
}

/// One streamed request answered by `respond`: the status the client got, whether its body ended in an error, and
/// the completion the bus carried.
async fn one_stream(
    respond: Respond,
    limits: phoebis_relay::Limits,
) -> (u16, bool, phoebis_bus::RequestComplete) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, limits).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let cut = resp.into_body().collect().await.is_err();
    let complete = completion(&mut sub).await;
    up.release();
    up.shutdown().await;
    (status, cut, complete)
}

#[tokio::test]
async fn a_stream_cut_by_the_reader_mid_body_reads_stream_cut() {
    let (status, cut, complete) = one_stream(
        Respond::SseCut {
            status: 200,
            headers: vec![],
            events: vec![message_start_event(), text_delta_event("hi")],
        },
        common::default_limits(),
    )
    .await;
    assert_eq!(
        (status, cut),
        (200, true),
        "the head committed and the body ends in an error"
    );
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Incomplete, Some("stream_cut")),
        "the head committed, so the cut is no unreachable upstream"
    );
}

const CUT_WARNING: &str = "the upstream body ended short after the head";

/// The `request-id` head the cut cases answer with, so the warning has an upstream request id to name.
const UPSTREAM_REQUEST_ID: &str = "req_up_cut";

fn with_request_id() -> Vec<(String, String)> {
    vec![("request-id".into(), UPSTREAM_REQUEST_ID.into())]
}

/// The completion carries the cut as `code` and `text` name it with the bytes the client received, and `log` holds
/// one warning for it with the six fields: the account, the request, the upstream's request id, those bytes, the code
/// and the error's words.
fn assert_cut_warned(log: &str, complete: &phoebis_bus::RequestComplete, code: &str, text: &str) {
    let bytes_relayed = complete
        .response_bytes
        .expect("a relayed body counts its bytes");
    assert_eq!(
        complete.interruption,
        Some(phoebis_bus::Interruption {
            code: code.into(),
            text: text.into(),
            bytes_relayed,
        }),
        "the completion carries the cut"
    );
    let warnings: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(CUT_WARNING))
        .collect();
    assert_eq!(warnings.len(), 1, "one warning for the attempt: {log}");
    for field in [
        " WARN ".to_string(),
        " account=a ".to_string(),
        format!(" request_id={} ", complete.request_id),
        format!(" upstream_request_id={UPSTREAM_REQUEST_ID} "),
        format!(" bytes_relayed={bytes_relayed} "),
        format!(" code={code} "),
        format!(" error={text}"),
    ] {
        assert!(warnings[0].contains(&field), "{field:?} in {}", warnings[0]);
    }
}

#[tokio::test]
async fn a_stream_cut_after_its_head_warns_once_and_carries_the_transport_errors_words() {
    let captured = common::Captured::default();
    let _log = captured.install();
    let (status, cut, complete) = one_stream(
        Respond::SseCut {
            status: 200,
            headers: with_request_id(),
            events: vec![message_start_event(), text_delta_event("hi")],
        },
        common::default_limits(),
    )
    .await;
    assert_eq!((status, cut), (200, true));
    assert_eq!(complete.error_code.as_deref(), Some("stream_cut"));
    assert_cut_warned(
        &captured.text(),
        &complete,
        "stream_cut",
        "malformed response: connection closed inside chunked body",
    );
}

/// A stream whose `message_start` commits the head, after which the fake writes nothing until it is released.
fn committed_then_gated() -> Respond {
    Respond::SseGated {
        status: 200,
        headers: with_request_id(),
        events: vec![message_start_event(), message_stop_event()],
    }
}

#[tokio::test]
async fn a_stream_its_accounts_removal_ends_after_the_head_warns_once_with_account_canceled() {
    let captured = common::Captured::default();
    let _log = captured.install();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: committed_then_gated(),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the message_start committed the head");
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    assert!(
        resp.into_body().collect().await.is_err(),
        "the body ends in the cancel's error"
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Canceled, Some("account_canceled"))
    );
    assert_cut_warned(
        &captured.text(),
        &complete,
        "account_canceled",
        "io: the bound account was disconnected",
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_that_leaves_mid_stream_is_warned_of_nothing_and_carries_no_interruption() {
    let captured = common::Captured::default();
    let _log = captured.install();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: committed_then_gated(),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp);
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.error_code.as_deref(), complete.interruption),
        (Some("client_gone"), None),
        "the client left, so nothing ended short"
    );
    assert!(
        !captured.text().contains(CUT_WARNING),
        "{}",
        captured.text()
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_stream_the_router_s_shutdown_cuts_after_the_head_carries_router_shutdown_and_no_interruption(
) {
    let captured = common::Captured::default();
    let _log = captured.install();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: committed_then_gated(),
    }])
    .await;
    let bus = phoebis_bus::Bus::new();
    let mut sub = bus.subscribe("test", phoebis_bus::Delivery::Durable);
    let mark = phoebis_relay::ShutdownMark::default();
    let h = common::harness_with_shutdown_mark(
        &up,
        &[common::oauth("a", "sk-ant-oat01-AAAA", 0)],
        common::PoolSpec::default(),
        bus,
        common::default_limits(),
        mark.clone(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    mark.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(resp);
    let complete = completion(&mut sub).await;
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.interruption
        ),
        (
            phoebis_bus::Outcome::Canceled,
            Some("router_shutdown"),
            None
        ),
        "the router cut the stream, so nothing ended short and its own code names the cut"
    );
    assert!(
        !captured.text().contains(CUT_WARNING),
        "{}",
        captured.text()
    );
    up.release();
    up.shutdown().await;
}

/// The mark attached and unset, as it is while the router serves, so the stream arm keeps the caller's own code.
#[tokio::test]
async fn a_client_that_leaves_mid_stream_under_an_unset_mark_keeps_client_gone() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: committed_then_gated(),
    }])
    .await;
    let bus = phoebis_bus::Bus::new();
    let mut sub = bus.subscribe("test", phoebis_bus::Delivery::Durable);
    let h = common::harness_with_shutdown_mark(
        &up,
        &[common::oauth("a", "sk-ant-oat01-AAAA", 0)],
        common::PoolSpec::default(),
        bus,
        common::default_limits(),
        phoebis_relay::ShutdownMark::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp);
    let complete = completion(&mut sub).await;
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            complete.interruption
        ),
        (phoebis_bus::Outcome::Canceled, Some("client_gone"), None),
        "the client left while the mark was unset"
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_forwarded_error_stream_cut_mid_body_stays_failed_with_stream_cut() {
    let (status, cut, complete) = one_stream(
        Respond::SseCut {
            status: 529,
            headers: vec![],
            events: vec![message_start_event()],
        },
        common::default_limits(),
    )
    .await;
    assert_eq!((status, cut), (529, true));
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_cut")),
        "an error answer never committed to a message, so its cut names the cause under failed"
    );
}

#[tokio::test]
async fn a_forwarded_error_stream_silent_past_the_idle_bound_stays_failed_with_stream_stalled() {
    let (status, cut, complete) = one_stream(
        Respond::Stall {
            status: 529,
            headers: vec![],
            events: vec![],
        },
        phoebis_relay::Limits {
            idle_timeout: Duration::from_millis(300),
            ..common::default_limits()
        },
    )
    .await;
    assert_eq!((status, cut), (529, true));
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_stalled"))
    );
}

#[tokio::test]
async fn a_forwarded_error_stream_closed_without_its_stop_keeps_the_heads_failure() {
    let (status, cut, complete) = one_stream(
        Respond::Sse {
            status: 500,
            headers: vec![],
            events: vec![message_start_event()],
            delay_between: Duration::from_millis(2),
        },
        common::default_limits(),
    )
    .await;
    assert_eq!((status, cut), (500, false));
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, None),
        "a missing stop on an error answer is no incomplete message"
    );
}

#[tokio::test]
async fn a_stream_silent_past_the_idle_bound_reads_stream_stalled() {
    // The stall sends an event-stream head and nothing more, so the hold commits at its own deadline before the
    // body loop's idle bound can run; both are short so the test does not wait out the defaults.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(
        &up,
        phoebis_relay::Limits {
            hold_timeout: Duration::from_millis(100),
            idle_timeout: Duration::from_millis(300),
            ..common::default_limits()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.into_body().collect().await.is_err(),
        "the body ends in an error at the idle bound"
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Incomplete, Some("stream_stalled"))
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_stream_stalled_after_its_head_warns_once_with_no_chunk_for_the_idle_bound() {
    let captured = common::Captured::default();
    let _log = captured.install();
    let (status, cut, complete) = one_stream(
        Respond::Stall {
            status: 200,
            headers: with_request_id(),
            events: vec![],
        },
        phoebis_relay::Limits {
            hold_timeout: Duration::from_millis(100),
            idle_timeout: Duration::from_millis(300),
            ..common::default_limits()
        },
    )
    .await;
    assert_eq!((status, cut), (200, true));
    assert_eq!(complete.error_code.as_deref(), Some("stream_stalled"));
    assert_cut_warned(
        &captured.text(),
        &complete,
        "stream_stalled",
        "no chunk for 0.3 s",
    );
}

#[tokio::test]
async fn a_stream_whose_oversized_event_is_skipped_keeps_its_later_events() {
    // A held 200 that commits at its message_start, so the bus copy goes on with the hold's parser: the one event
    // far past the bound is skipped there, and the delta and the stop after it still reach the completion.
    let events = vec![
        message_start_event(),
        text_delta_event(&"x".repeat(4 * phoebis_relay::INSPECT_BUFFER)),
        SseEvent {
            event: "message_delta".into(),
            data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#
                .into(),
        },
        message_stop_event(),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: events.clone(),
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    // Compared without `assert_eq!`, whose failure would print both 2 MiB bodies.
    assert!(
        body.as_ref() == rendered(&events).as_bytes(),
        "the client gets every byte the upstream sent"
    );
    let published = common::drain_until_complete(&mut sub).await;
    let complete = published
        .iter()
        .find_map(|e| match e {
            phoebis_bus::Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .unwrap();
    let usage = complete.usage.as_ref().expect("the completion has a usage");
    assert_eq!(
        (
            complete.outcome,
            complete.error_code.as_deref(),
            usage.input_tokens,
            usage.output_tokens,
            complete.response.stop_reason.as_deref(),
            complete.skipped_events,
        ),
        (
            phoebis_bus::Outcome::Succeeded,
            None,
            Some(5),
            Some(7),
            Some("end_turn"),
            1
        ),
        "the head's usage merged with the delta's, the delta's stop, and the one skipped event"
    );
    assert!(
        published.iter().any(|e| matches!(
            e,
            phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
                phase: Some(phoebis_bus::Phase::MessageStop),
                ..
            })
        )),
        "the stop's phase is on the bus"
    );
    up.shutdown().await;
}

fn gzipped_stream(events: &[SseEvent]) -> Respond {
    Respond::Chunked {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "text/event-stream".into()),
            ("Content-Encoding".into(), "gzip".into()),
        ],
        body: gzip(rendered(events).as_bytes()),
    }
}

#[tokio::test]
async fn a_gzipped_stream_decoded_for_the_client_is_read_for_its_message_stop_on_the_bus_copy() {
    // The bus copy reads the pieces the forward decoded, and that decoder flushes every piece it is fed, so a
    // message_stop cannot sit unread in its tail at the end.
    let events = [
        message_start_event(),
        text_delta_event("hi"),
        message_stop_event(),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: gzipped_stream(&events),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert!(
        resp.headers().get("content-encoding").is_none(),
        "the fixture's caller reads gzip and still gets the stream decoded"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), rendered(&events));
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Succeeded, None)
    );
    up.set_rule_after(0, "/v1/messages", gzipped_stream(&events[..2]));
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert!(resp.headers().get("content-encoding").is_none());
    resp.into_body().collect().await.unwrap();
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Incomplete, Some("stream_incomplete")),
        "the same stream without its stop"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_connection_cut_after_the_head_answers_502_upstream_cut_and_files_stream_cut() {
    // The cut comes before any committing event, so the hold has nothing the client could be sent.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::SseCut {
            status: 200,
            headers: vec![],
            events: vec![ping_event()],
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (502, "upstream_cut")
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_cut"))
    );
    up.shutdown().await;
}

/// `payload` gzipped with its first deflate byte replaced, so the head reads as gzip and the body does not inflate:
/// the byte opens a final block of the reserved type 3.
fn corrupt_gzip(payload: &[u8]) -> Vec<u8> {
    let mut body = gzip(payload);
    body[10] = 0xff;
    body
}

/// The stream request from a caller that takes no content coding.
fn identity_request() -> Request<Body> {
    let mut request = common::stream_request(common::KEY);
    common::set_header(&mut request, "accept-encoding", "identity");
    request
}

/// One answer to `request`: its status, its error code header, the message its body names, and the completion the bus
/// carried.
async fn one_refusal(
    respond: Respond,
    request: Request<Body>,
) -> (
    u16,
    Option<String>,
    Option<String>,
    phoebis_bus::RequestComplete,
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h.app.clone().oneshot(request).await.unwrap();
    let status = resp.status().as_u16();
    let code = resp
        .headers()
        .get("x-phoebis-error-code")
        .map(|value| value.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_string));
    let complete = completion(&mut sub).await;
    up.shutdown().await;
    (status, code, message, complete)
}

const UNDECODABLE: &str = "the upstream response could not be decoded after its head";

#[tokio::test]
async fn an_answer_in_a_coding_the_relay_cannot_read_answers_502_upstream_undecodable_and_files_stream_undecodable(
) {
    // `compress` is a registered coding outside the five the relay decodes.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("Content-Encoding".into(), "compress".into())],
            body: COMPRESSIBLE.to_vec(),
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let code = resp
        .headers()
        .get("x-phoebis-error-code")
        .map(|value| value.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let refusal: serde_json::Value =
        serde_json::from_slice(&body).expect("the body is the relay's own error document");
    assert_eq!(
        (
            status,
            code.as_deref(),
            refusal["error"]["message"].as_str()
        ),
        (502, Some("upstream_undecodable"), Some(UNDECODABLE))
    );
    assert!(
        !body
            .windows(COMPRESSIBLE.len())
            .any(|window| window == COMPRESSIBLE),
        "no byte of the upstream's body reaches the client"
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_undecodable"))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_answer_whose_codings_sit_in_two_fields_answers_502_upstream_undecodable_and_files_stream_undecodable(
) {
    // Two fields read as the list `gzip, gzip`, which the relay refuses as it refuses that list in one field.
    let (status, code, message, complete) = one_refusal(
        Respond::Json {
            status: 200,
            headers: vec![
                ("Content-Encoding".into(), "gzip".into()),
                ("Content-Encoding".into(), "gzip".into()),
            ],
            body: gzip(&gzip(COMPRESSIBLE)),
        },
        common::stream_request(common::KEY),
    )
    .await;
    assert_eq!(
        (status, code.as_deref(), message.as_deref()),
        (502, Some("upstream_undecodable"), Some(UNDECODABLE)),
        "the body is the relay's own error document, with no byte of the upstream's"
    );
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_undecodable"))
    );
}

#[tokio::test]
async fn a_rejection_whose_gzip_body_does_not_inflate_answers_502_upstream_undecodable_and_files_stream_undecodable(
) {
    // The 400 is drained whole and forwarded, so the relay decodes it for this caller before a byte goes out.
    let (status, code, message, complete) = one_refusal(
        Respond::Json {
            status: 400,
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            body: corrupt_gzip(
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
            ),
        },
        identity_request(),
    )
    .await;
    assert_eq!(
        (status, code.as_deref(), message.as_deref()),
        (502, Some("upstream_undecodable"), Some(UNDECODABLE))
    );
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_undecodable"))
    );
}

#[tokio::test]
async fn a_streamed_answer_undecodable_after_its_head_warns_once_with_the_decoders_words() {
    let captured = common::Captured::default();
    let _log = captured.install();
    let events = [
        message_start_event(),
        text_delta_event("hi"),
        message_stop_event(),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Chunked {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/event-stream".into()),
                ("Content-Encoding".into(), "gzip".into()),
                ("request-id".into(), UPSTREAM_REQUEST_ID.into()),
            ],
            body: corrupt_gzip(rendered(&events).as_bytes()),
        },
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h.app.clone().oneshot(identity_request()).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.into_body().collect().await.is_err());
    let complete = completion(&mut sub).await;
    assert_eq!(complete.error_code.as_deref(), Some("stream_undecodable"));
    assert_cut_warned(
        &captured.text(),
        &complete,
        "stream_undecodable",
        "undecodable response: corrupt deflate stream",
    );
    up.shutdown().await;
}

/// A JSON answer gzipped into `body`, which the relay reads as it arrives with no held prefix, so its first piece is
/// a body chunk.
fn gzipped_json(body: Vec<u8>) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![("Content-Encoding".into(), "gzip".into())],
        body,
    }
}

#[tokio::test]
async fn a_json_answer_whose_gzip_breaks_inside_a_chunk_ends_incomplete_with_stream_undecodable() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: gzipped_json(corrupt_gzip(COMPRESSIBLE)),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h.app.clone().oneshot(identity_request()).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.into_body().collect().await.is_err(),
        "the body ends in an error at the chunk that does not decode"
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Incomplete, Some("stream_undecodable"))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_json_answer_whose_gzip_tail_does_not_finish_ends_incomplete_with_stream_undecodable() {
    // Without its eight-byte trailer every chunk decodes, and the decoder's finish is what refuses the body.
    let mut body = gzip(COMPRESSIBLE);
    body.truncate(body.len() - 8);
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: gzipped_json(body),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let resp = h.app.clone().oneshot(identity_request()).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.into_body().collect().await.is_err(),
        "the body ends in an error at the tail that does not finish"
    );
    let complete = completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Incomplete, Some("stream_undecodable"))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejection_cut_inside_its_body_answers_502_upstream_cut_and_files_stream_cut() {
    // The drain fails on the body reader's framing, a cut connection rather than a payload that does not decode.
    let (status, code, message, complete) = one_refusal(
        Respond::SseCut {
            status: 400,
            headers: vec![],
            events: vec![ping_event()],
        },
        common::stream_request(common::KEY),
    )
    .await;
    assert_eq!(
        (status, code.as_deref(), message.as_deref()),
        (
            502,
            Some("upstream_cut"),
            Some("the upstream response could not be read after its head")
        )
    );
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Failed, Some("stream_cut"))
    );
}

#[tokio::test]
async fn a_gzip_stream_decoded_for_the_client_builds_the_holds_decoder_alone_and_keeps_its_message_start(
) {
    let message_start = SseEvent {
        event: "message_start".into(),
        data: r#"{"type":"message_start","message":{"id":"msg_gz","usage":{"input_tokens":5,"cache_read_input_tokens":2}}}"#.into(),
    };
    let events = [message_start, text_delta_event("hi"), message_stop_event()];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: gzipped_stream(&events),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let before = h.relay.decoders_built();
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert!(resp.headers().get("content-encoding").is_none());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), rendered(&events));
    let complete = completion(&mut sub).await;
    assert_eq!(
        h.relay.decoders_built() - before,
        1,
        "the hold's decoder alone: past the commit the bus copy reads the pieces the forward decoded"
    );
    assert_eq!(
        (complete.outcome, complete.message_id.as_deref()),
        (phoebis_bus::Outcome::Succeeded, Some("msg_gz")),
        "the held message_start still names the message"
    );
    let usage = complete.usage.expect("the held message_start's usage");
    assert_eq!(
        (usage.input_tokens, usage.cache_read_tokens),
        (Some(5), Some(2))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_forwarded_coded_rejection_past_the_bound_reaches_the_client_decoded_prefix_and_rest() {
    // A non-portable request's 429 past the bound, decoded for the client as it is forwarded: no bus, no pin, no
    // hook and no conversation report read its body, so no copy of it is decoded beside the forward.
    let mut state: u32 = 0x9e37_79b9;
    let noise: String = (0..160 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    let rejection =
        format!(r#"{{"type":"error","error":{{"type":"rate_limit_error","message":"{noise}"}}}}"#);
    let body = gzip(rejection.as_bytes());
    assert!(body.len() > 64 * 1024, "past the bound on the wire");
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 429,
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            body: body.clone(),
        },
    }])
    .await;
    let h = common::harness(
        &up,
        &[common::oauth("a", "sk-ant-oat01-AAAA", 0)],
        common::PoolSpec::default(),
    )
    .await;
    let before = h.relay.decoders_built();
    let resp = h
        .app
        .clone()
        .oneshot(common::non_portable_request(common::KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().get("content-encoding").is_none());
    let forwarded = resp.into_body().collect().await.unwrap().to_bytes();
    // Compared without `assert_eq!`, whose failure would print both 160 KiB bodies.
    assert!(
        forwarded.as_ref() == rejection.as_bytes(),
        "the prefix, then the rest, both decoded"
    );
    // The counter counts the hold's decoders, and no hold reads a rejection, so the forward's own decoder leaves it
    // at zero.
    assert_eq!(h.relay.decoders_built() - before, 0);
    up.shutdown().await;
}

#[tokio::test]
async fn a_held_stream_whose_hold_copy_overflowed_reaches_the_bus_whole_from_the_forwards_decode() {
    // The first event decodes past the hold's inspection bound in one wire chunk, so the hold commits on its
    // overflow with a copy that no longer follows the stream, and the bus copy reads the stream from its start in
    // the pieces the forward decoded.
    let message_start = SseEvent {
        event: "message_start".into(),
        data: r#"{"type":"message_start","message":{"id":"msg_ov","usage":{"input_tokens":5}}}"#
            .into(),
    };
    let events = [
        SseEvent {
            event: "weird_future_event".into(),
            // Past `INSPECT_BUFFER`, and under the 1 MiB a decoder hands back for one wire chunk.
            data: "y".repeat(600 * 1024),
        },
        message_start,
        text_delta_event("hi"),
        message_stop_event(),
    ];
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: gzipped_stream(&events),
    }])
    .await;
    let (h, mut sub) = bus_harness(&up, common::default_limits()).await;
    let before = h.relay.decoders_built();
    let resp = h
        .app
        .clone()
        .oneshot(common::stream_request(common::KEY))
        .await
        .unwrap();
    assert!(resp.headers().get("content-encoding").is_none());
    let client = resp.into_body().collect().await.unwrap().to_bytes();
    // Compared without `assert_eq!`, whose failure would print both 600 KiB bodies.
    assert!(
        client.as_ref() == rendered(&events).as_bytes(),
        "the client reads the stream decoded"
    );
    let published = common::drain_until_complete(&mut sub).await;
    let chunks: Vec<u8> = published
        .iter()
        .filter_map(|e| match e {
            phoebis_bus::Event::ResponseChunk(chunk) => Some(chunk.body.to_vec()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        String::from_utf8(chunks).unwrap(),
        rendered(&events),
        "the bus reads the whole stream decoded"
    );
    let complete = published
        .iter()
        .find_map(|e| match e {
            phoebis_bus::Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (complete.outcome, complete.message_id.as_deref()),
        (phoebis_bus::Outcome::Succeeded, Some("msg_ov"))
    );
    assert_eq!(
        h.relay.decoders_built() - before,
        1,
        "the hold's decoder alone: the bus copy builds none of its own"
    );
    up.shutdown().await;
}
