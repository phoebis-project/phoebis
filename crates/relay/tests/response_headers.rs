//! Response header hygiene: the upstream's own identity and cookie never reach a key holder, and a pool's
//! `quota_headers` decides whether the quota family does.
mod common;
use common::*;

use fake_upstream::{FakeUpstream, Rule};
use phoebis_routing::FIVE_HOUR;
use phoebis_store::QuotaHeaders;
use tower::ServiceExt;

const OA: &str = "sk-ant-oat01-AAAA0001";

fn spec(quota_headers: QuotaHeaders) -> PoolSpec {
    PoolSpec {
        quota_headers,
        ..Default::default()
    }
}

/// The upstream's own identity and cookie, the caller-visible request id, and the quota family
/// `quota_headers` gates, on the answers this suite scripts.
fn upstream_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("anthropic-organization-id", "org_01ABCDEF"),
        ("set-cookie", "sesh=abc123; Path=/; HttpOnly"),
        ("request-id", "req_upstream_1"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
    ]
}

#[tokio::test]
async fn the_upstreams_organization_id_and_cookies_never_reach_the_caller() {
    // A committed answer, under the default `pass`.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(200, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], spec(QuotaHeaders::Pass)).await;
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

    // A rejection, still under `pass`: the identity headers leave regardless of status.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(429, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], spec(QuotaHeaders::Pass)).await;
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
async fn under_pass_the_quota_family_and_the_request_id_reach_the_caller() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(200, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], spec(QuotaHeaders::Pass)).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-5h-status")
            .unwrap(),
        "allowed"
    );
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_upstream_1");
    up.shutdown().await;
}

#[tokio::test]
async fn under_hide_a_committed_answer_loses_the_family_and_keeps_the_request_id() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(200, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], spec(QuotaHeaders::Hide)).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("anthropic-ratelimit-unified-5h-status")
        .is_none());
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_upstream_1");
    up.shutdown().await;
}

#[tokio::test]
async fn under_hide_a_429_keeps_the_family_so_the_clients_continue_arms() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(429, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], spec(QuotaHeaders::Hide)).await;
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-5h-status")
            .unwrap(),
        "allowed"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn under_hide_the_relay_still_reads_the_family_for_its_own_sample() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_with_headers(200, &upstream_headers()),
    }])
    .await;
    let h = harness(&up, &[oauth("o", OA, 0)], spec(QuotaHeaders::Hide)).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("anthropic-ratelimit-unified-5h-status")
        .is_none());
    let samples = h.router.samples().await.unwrap();
    let (_, sample) = samples
        .iter()
        .find(|(id, _)| id == "o")
        .expect("the account carries a sample despite the hidden family");
    assert!(
        sample
            .reading(FIVE_HOUR)
            .is_some_and(|r| (r.utilization - 0.42).abs() < 1e-9),
        "the router's own read of the family runs on the upstream head before the hygiene edit"
    );
    up.shutdown().await;
}
