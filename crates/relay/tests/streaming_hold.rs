mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const C: &str = "sk-ant-oat01-CCCC";

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

fn rate_limit_error() -> SseEvent {
    ev(
        "error",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#,
    )
}

fn message_start() -> SseEvent {
    ev(
        "message_start",
        r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
    )
}

fn unified_7d() -> Vec<(String, String)> {
    vec![
        (
            "anthropic-ratelimit-unified-7d-status".into(),
            "rejected".into(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset".into(),
            "90000".into(),
        ),
    ]
}

async fn collect(resp: axum::http::Response<axum::body::Body>) -> String {
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// The first cooldown row, once the router has it. A post-commit rate-limit error is reported from the body
/// task, so the row can land a moment after the caller has the whole response.
async fn first_cooldown(h: &Harness) -> phoebis_store::CooldownRow {
    let give_up = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(row) = h.router.cooldowns().await.unwrap().into_iter().next() {
            return row;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the router never recorded a cooldown"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_ping_then_an_error_before_message_start_fails_over_with_no_client_bytes() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: vec![message_start(), ev("message_stop", "{}")],
            delay_between: Duration::ZERO,
        },
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: unified_7d(),
            events: vec![ev("ping", r#"{"type":"ping"}"#), rate_limit_error()],
            delay_between: Duration::from_millis(5),
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!((o.account_id.as_str(), o.attempt), ("b", 2));
    let body = collect(resp).await;
    assert!(
        body.starts_with("event: message_start\n"),
        "nothing from a's stream reached the client: {body}"
    );
    assert!(!body.contains("rate_limit_error"));
    let c = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (
            c[0].account_id.as_str(),
            c[0].scope.as_str(),
            c[0].window.as_str()
        ),
        ("a", "account", "seven_day")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn message_start_then_an_error_reaches_the_client_and_the_held_ping_is_relayed_after_commit()
{
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: unified_7d(),
            events: vec![message_start(), rate_limit_error()],
            delay_between: Duration::ZERO,
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "a");
    let body = collect(resp).await;
    assert!(
        body.contains("event: message_start\n") && body.contains("rate_limit_error"),
        "after commit nothing fails over"
    );
    assert_eq!(credentials_seen(&up), [A]);
    assert_eq!(
        first_cooldown(&h).await.account_id,
        "a",
        "the pin transition still runs so the next request migrates"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "b");
    up.add_credential_rule(
        B,
        "/v1/messages",
        Respond::SseGated {
            status: 200,
            headers: vec![],
            events: vec![
                ev("ping", r#"{"type":"ping"}"#),
                message_start(),
                ev("content_block_delta", "{}"),
            ],
        },
    );
    // The fake writes the ping and then gates: the relay holds it, so the response head itself cannot be
    // answered until the release lets message_start through.
    let app = h.app.clone();
    let pending = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!pending.is_finished(), "the ping is held until commit");
    up.release();
    let resp = pending.await.unwrap();
    assert_eq!(resp.status(), 200);
    let seen = collect(resp).await;
    assert!(
        seen.starts_with("event: ping\n"),
        "the held ping goes out first, in order: {seen}"
    );
    assert!(seen.contains("event: message_start\n"));
    up.shutdown().await;
}

#[tokio::test]
async fn a_stream_that_never_commits_is_relayed_as_received_after_the_hold_timeout() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            events: vec![ev("ping", "{}")],
        },
    );
    let limits = phoebis_relay::Limits {
        body_capture_max_hours: 72,
        hold_timeout: Duration::from_millis(200),
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        idle_timeout: Duration::from_secs(600),
        max_attempts: 3,
        max_request_bytes: 32 << 20,
        model_rejection: Duration::from_secs(3_600),
    };
    let h = harness_with(&up, &[oauth("a", A, 0)], PoolSpec::default(), limits).await;
    let started = std::time::Instant::now();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the head is answered once the hold gives up"
    );
    up.release();
    assert!(collect(resp).await.contains("event: ping\n"));
    up.shutdown().await;
}

#[tokio::test]
async fn mixed_429_and_sse_rate_limit_errors_across_three_accounts_produce_at_most_three_upstream_requests(
) {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(4_600, "five_hour"));
    up.add_credential_rule(
        B,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: unified_7d(),
            events: vec![rate_limit_error()],
            delay_between: Duration::ZERO,
        },
    );
    up.add_credential_rule(C, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 0), oauth("c", C, 0)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).attempt, 3);
    assert_eq!(credentials_seen(&up).len(), 3);
    assert!(
        matches!(resp.status().as_u16(), 200 | 429),
        "the last failed answer is committed, whichever shape it had"
    );
    assert_eq!(h.router.cooldowns().await.unwrap().len(), 3);
    up.shutdown().await;
}

#[tokio::test]
async fn overage_evidence_moves_forward_only() {
    let up = FakeUpstream::start(vec![]).await;
    let dated = |date: &str, flag: &str| Respond::Json {
        status: 200,
        headers: vec![
            ("Date".into(), date.into()),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-in-use".into(),
                flag.into(),
            ),
        ],
        body: b"{}".to_vec(),
    };
    // An ordered queue: each request takes the next response, so the sequence below is what the relay sees.
    up.push_credential_rule(
        A,
        "/v1/messages",
        dated("Wed, 21 Oct 2015 07:28:00 GMT", "true"),
    );
    up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 529,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    up.push_credential_rule(
        A,
        "/v1/messages",
        dated("Wed, 21 Oct 2015 07:29:00 GMT", "false"),
    );
    up.push_credential_rule(
        A,
        "/v1/messages",
        dated("Wed, 21 Oct 2015 07:27:00 GMT", "true"),
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            overage_policy: phoebis_store::OveragePolicy::Allow,
            ..PoolSpec::default()
        },
    )
    .await;
    let mut statuses = Vec::new();
    let mut flags = Vec::new();
    // The evidence is reported before the response returns and the query travels the same router mailbox, so
    // each flag below is read after the head that produced it was recorded.
    for _ in 0..4 {
        statuses.push(
            h.app
                .clone()
                .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
                .await
                .unwrap()
                .status()
                .as_u16(),
        );
        flags.push(h.router.overage("a").await.unwrap().unwrap().in_use);
    }
    assert_eq!(
        statuses,
        [200, 529, 200, 200],
        "the fake served the queue in order"
    );
    assert_eq!(
        flags,
        [true, true, false, false],
        "set; a headerless 529 is not evidence; a newer head clears it; an older head arriving late cannot set it again"
    );
    let evidence = h.router.overage("a").await.unwrap().unwrap();
    assert_eq!(
        evidence.since, None,
        "the persisted row follows the normalized state"
    );
    wait_for(Duration::from_secs(2), || {
        let row = h.store.load_routing().unwrap().overage;
        (row.len() == 1 && !row[0].in_use).then_some(())
    })
    .await;
    assert_eq!(h.store.load_routing().unwrap().overage[0].since, None);
    up.shutdown().await;
}

#[tokio::test]
async fn a_compressed_post_commit_rate_limit_error_records_the_cooldown_with_the_client_bytes_unchanged(
) {
    let up = FakeUpstream::start(vec![]).await;
    let text = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}\n\n";
    let gz = {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(text.as_bytes()).unwrap();
        e.finish().unwrap()
    };
    let mut headers = unified_7d();
    headers.push(("Content-Type".into(), "text/event-stream".into()));
    headers.push(("Content-Encoding".into(), "gzip".into()));
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Chunked {
            status: 200,
            headers,
            body: gz.clone(),
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "accept-encoding", "gzip");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "a")
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        gz.as_slice(),
        "the client gets the compressed bytes as they came"
    );
    assert_eq!(
        credentials_seen(&up),
        [A],
        "one upstream request after message_start"
    );
    assert_eq!(
        first_cooldown(&h).await.account_id,
        "a",
        "the observer read the decoded copy"
    );
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
                .await
                .unwrap()
        )
        .account_id,
        "b"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_disconnect_during_a_stalled_pre_commit_read_closes_the_upstream_connection() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![message_start()],
        },
    );
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let app = h.app.clone();
    let pending = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !pending.is_finished(),
        "the hold is waiting on the stalled stream"
    );
    assert_eq!(up.open_connections(), 1);
    h.store.bump_lifecycle("a").unwrap();
    h.router.reload().await.unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("the hold ends on cancellation")
        .unwrap();
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "account_cancelled"
    );
    up.release();
    wait_for(Duration::from_secs(2), || {
        (up.open_connections() == 0).then_some(())
    })
    .await;
    assert!(
        h.router.cooldowns().await.unwrap().is_empty(),
        "nothing was classified after the cancellation"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn prefer_plan_moves_a_session_off_an_account_in_overage_and_back_when_nothing_else_is_eligible(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "allowed".into(),
                ),
                (
                    "anthropic-ratelimit-unified-overage-in-use".into(),
                    "true".into(),
                ),
            ],
            body: b"{}".to_vec(),
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
                .await
                .unwrap()
        )
        .account_id,
        "a"
    );
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
                .await
                .unwrap()
        )
        .account_id,
        "b",
        "the plan-quota sibling takes the session"
    );
    up.add_credential_rule(B, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "a",
        "back to the overage account only when no sibling is eligible"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_post_commit_cooldown_is_measured_from_the_error_events_arrival() {
    let up = FakeUpstream::start(vec![]).await;
    let mut headers = vec![("retry-after".to_string(), "10".to_string())];
    headers.extend(
        unified_7d()
            .into_iter()
            .filter(|(n, _)| n.ends_with("status")),
    );
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::SseGated {
            status: 200,
            headers,
            events: vec![message_start(), rate_limit_error()],
        },
    );
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let app = h.app.clone();
    let pending = tokio::spawn(async move {
        let resp = app
            .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap();
        collect(resp).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The error event arrives long after the head: the clock moved an hour in between.
    h.clock
        .store(NOW + 3_600_000, std::sync::atomic::Ordering::SeqCst);
    up.release();
    pending.await.unwrap();
    assert_eq!(
        first_cooldown(&h).await.until,
        NOW + 3_600_000 + 10_000 + 1_000,
        "based on the error's arrival, not the selection"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_pre_write_retry_counts_against_the_attempt_budget() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::CloseImmediately,
    }])
    .await;
    up.set_rule_after_connections(1, "/v1/messages", json_429_unified(4_600, "five_hour"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 0), oauth("c", C, 0)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).attempt,
        3,
        "one dead connection, then two wire submissions"
    );
    assert_eq!(
        credentials_seen(&up).len(),
        2,
        "the dead connection never carried a request"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_cancelled_stream_releases_the_upstream_while_the_client_holds_the_body() {
    // Past the relay's 64-deep channel, and far past what the relay's buffers and the socket's can hold
    // between them, so the fake is still writing when the account goes away.
    let filler = "x".repeat(8192);
    let mut events = vec![message_start()];
    events.extend((0..4096).map(|_| ev("content_block_delta", &filler)));
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: vec![],
            events,
            delay_between: Duration::ZERO,
        },
    );
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // The body is never polled, so the relay's channel fills and the upstream reader stops draining.
    stays_for(Duration::from_millis(200), || up.open_connections() == 1).await;
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    wait_for(Duration::from_secs(2), || {
        (up.open_connections() == 0).then_some(())
    })
    .await;
    // Held to here on purpose: the upstream had to go while the caller was still not reading.
    drop(resp);
    up.shutdown().await;
}
