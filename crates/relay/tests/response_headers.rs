//! Response header hygiene: the upstream's own identity and cookie never reach a key holder, and the upstream's
//! rate-limit header families reach the caller on a rate limit alone.
mod common;
use std::time::Duration;

use axum::http::HeaderMap;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use phoebis_relay::Limits;
use tower::ServiceExt;

const OA: &str = "sk-ant-oat01-AAAA0001";

/// The upstream's own identity and cookie, the caller-visible request id, and a header of each rate-limit family:
/// the unified windows, the Anthropic-key bucket and the fast-token limit.
fn upstream_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("anthropic-organization-id", "org_01ABCDEF"),
        ("set-cookie", "sesh=abc123; Path=/; HttpOnly"),
        ("request-id", "req_upstream_1"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
        ("anthropic-ratelimit-requests-remaining", "49"),
        ("anthropic-fast-input-tokens-limit", "400000"),
    ]
}

/// The rate-limit headers of `upstream_headers`, sorted.
const SCRIPTED_FAMILIES: [&str; 4] = [
    "anthropic-fast-input-tokens-limit",
    "anthropic-ratelimit-requests-remaining",
    "anthropic-ratelimit-unified-5h-status",
    "anthropic-ratelimit-unified-5h-utilization",
];

/// The names an answer carries under either rate-limit prefix, with `retry-after`, sorted.
fn rate_limit_names(headers: &HeaderMap) -> Vec<String> {
    let mut names: Vec<String> = headers
        .keys()
        .map(|name| name.as_str().to_string())
        .filter(|name| {
            name.starts_with("anthropic-ratelimit-")
                || name.starts_with("anthropic-fast-")
                || name == "retry-after"
        })
        .collect();
    names.sort();
    names
}

fn sse(first: SseEvent, rest: Vec<SseEvent>) -> Respond {
    Respond::Sse {
        status: 200,
        headers: upstream_headers()
            .into_iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        events: std::iter::once(first).chain(rest).collect(),
        delay_between: Duration::ZERO,
    }
}

fn event(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

#[tokio::test]
async fn the_upstreams_organization_id_and_cookies_never_reach_the_caller() {
    // A committed answer.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(200, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("anthropic-organization-id").is_none());
    assert!(resp.headers().get("set-cookie").is_none());
    up.shutdown().await;

    // A forwarded 429, which keeps the rate-limit families: the identity headers leave whatever the status.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(429, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().get("anthropic-organization-id").is_none());
    assert!(resp.headers().get("set-cookie").is_none());
    up.shutdown().await;
}

#[tokio::test]
async fn a_streamed_200_loses_every_rate_limit_family_and_keeps_the_request_id() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: sse(
            event(
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            ),
            vec![event("message_stop", r#"{"type":"message_stop"}"#)],
        ),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(rate_limit_names(resp.headers()), Vec::<String>::new());
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_upstream_1");
    up.shutdown().await;
}

#[tokio::test]
async fn a_json_200_loses_every_rate_limit_family_and_keeps_the_request_id() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(200, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(rate_limit_names(resp.headers()), Vec::<String>::new());
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_upstream_1");
    up.shutdown().await;
}

#[tokio::test]
async fn a_forwarded_400_and_a_forwarded_529_lose_every_rate_limit_family() {
    // The 400 is drained and forwarded from its buffer, the 529 streamed from the wire.
    for status in [400, 529] {
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_with_headers(status, &upstream_headers()),
        }])
        .await;
        let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), status);
        assert_eq!(
            rate_limit_names(resp.headers()),
            Vec::<String>::new(),
            "a forwarded {status}"
        );
        assert_eq!(resp.headers().get("request-id").unwrap(), "req_upstream_1");
        up.shutdown().await;
    }
}

#[tokio::test]
async fn a_non_portable_requests_forwarded_429_keeps_every_rate_limit_family_so_the_clients_continue_arms(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(429, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(rate_limit_names(resp.headers()), SCRIPTED_FAMILIES);
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-5h-status")
            .unwrap(),
        "allowed"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_portable_requests_429_forwarded_once_the_attempt_budget_is_spent_keeps_every_rate_limit_family(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(429, &upstream_headers()),
    }])
    .await;
    // One attempt in the budget, so the 429 is forwarded rather than failed over.
    let h = harness_with_limits(
        &up,
        &[oauth("o", OA, 0)],
        PoolSpec::default(),
        Limits {
            max_attempts: 1,
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
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().get("x-phoebis-error-code").is_none());
    assert_eq!(rate_limit_names(resp.headers()), SCRIPTED_FAMILIES);
    up.shutdown().await;
}

#[tokio::test]
async fn a_held_200_whose_first_event_is_a_rate_limit_error_keeps_every_rate_limit_family_when_forwarded(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: sse(
            event(
                "error",
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#,
            ),
            Vec::new(),
        ),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(rate_limit_names(resp.headers()), SCRIPTED_FAMILIES);
    up.shutdown().await;
}

#[tokio::test]
async fn the_composed_exhaustion_429_carries_retry_after_and_exactly_three_unified_headers() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_unified(9_000, "five_hour"),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "accounts_exhausted"
    );
    assert_eq!(
        rate_limit_names(resp.headers()),
        [
            "anthropic-ratelimit-unified-representative-claim",
            "anthropic-ratelimit-unified-reset",
            "anthropic-ratelimit-unified-status",
            "retry-after",
        ]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_composed_fast_credits_429_carries_the_overage_disabled_reason() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_fast_credits("out_of_credits", None),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], PoolSpec::default()).await;
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "fast_credits_unavailable"
    );
    assert_eq!(
        rate_limit_names(resp.headers()),
        ["anthropic-ratelimit-unified-overage-disabled-reason"]
    );
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-overage-disabled-reason")
            .unwrap(),
        "out_of_credits"
    );
    up.shutdown().await;
}
