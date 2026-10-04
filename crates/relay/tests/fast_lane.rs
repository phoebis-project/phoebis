//! A fast-mode request against the fast lane: a fast rate limit or a usage-credits refusal cools one upstream's
//! fast lane and the request moves to a sibling that can serve it; the router refuses it locally, before any dial,
//! only when no upstream can.

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery};
use phoebis_ledger::{Assembler, Record};
use phoebis_routing::{CreditsState, Report};
use phoebis_store::CooldownRow;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const MODEL: &str = "claude-sonnet-4-5";
const HOUR: i64 = 3_600_000;

async fn messages_up() -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await
}

async fn two_accounts(up: &FakeUpstream) -> Harness {
    harness(
        up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await
}

async fn body_json(resp: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn rows(cooldowns: &[CooldownRow]) -> Vec<(&str, &str, &str, &str, i64)> {
    cooldowns
        .iter()
        .map(|c| {
            (
                c.account_id.as_str(),
                c.scope.as_str(),
                c.reason.as_str(),
                c.window.as_str(),
                c.until,
            )
        })
        .collect()
}

#[tokio::test]
async fn a_fast_429_on_the_first_account_walks_to_the_sibling_and_migrates_the_pin_under_fast_lane()
{
    let up = messages_up().await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        json_429_fast_rate_limit(None, Some(("1060", "1090"))),
    );
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        bus,
    )
    .await;
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(credentials_seen(&up), [A, B]);
    body_json(resp).await;
    assert_eq!(
        rows(&h.router.cooldowns().await.unwrap()),
        [("a", "fast", "fast_rate_limit", "fast", 1_090_000 + 1_000)],
        "the later fast reset cools a's fast lane alone"
    );
    let events = drain_until_complete(&mut sub).await;
    let mut assembler = Assembler::new(std::sync::Arc::new(|| 0));
    let attempts = events
        .into_iter()
        .flat_map(|e| assembler.observe(e))
        .find_map(|r| match r {
            Record::Request { attempts, .. } => Some(attempts),
            _ => None,
        })
        .expect("the request lands a ledger row");
    assert_eq!(
        attempts
            .iter()
            .map(|a| (
                a.account_id.as_str(),
                a.upstream_status,
                a.cooldown_scope.as_deref()
            ))
            .collect::<Vec<_>>(),
        [("a", Some(429), Some("fast")), ("b", Some(200), None)]
    );
    let moved: serde_json::Value =
        serde_json::from_str(attempts[1].selection.as_deref().expect("a trace")).unwrap();
    assert_eq!(moved["moved"], "fast_lane");
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        pins.iter()
            .map(|p| (p.session_id.as_str(), p.account_id.as_str()))
            .collect::<Vec<_>>(),
        [("s1", "b")]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn with_every_account_at_its_fast_limit_the_router_answers_fast_lane_exhausted_before_any_dial(
) {
    let up = messages_up().await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        json_429_fast_rate_limit(None, Some(("1060", "1090"))),
    );
    up.add_credential_rule(
        B,
        "/v1/messages",
        json_429_fast_rate_limit(Some(30), Some(("1120", "1150"))),
    );
    let h = two_accounts(&up).await;
    let refused = |resp: &axum::http::Response<Body>| {
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"]
                .to_str()
                .unwrap()
                .to_string(),
            resp.headers()["retry-after"].to_str().unwrap().to_string(),
        )
    };
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(credentials_seen(&up), [A, B]);
    assert_eq!(
        refused(&resp),
        (429, "fast_lane_exhausted".to_string(), "91".to_string()),
        "the earlier fast reset with its second of margin, 1091 s, less the clock's 1000 s"
    );
    let dialed = up.requests().len();
    let resp = h.app.clone().oneshot(fast_request("s2")).await.unwrap();
    assert_eq!(up.requests().len(), dialed, "no upstream was dialled");
    assert_eq!(
        refused(&resp),
        (429, "fast_lane_exhausted".to_string(), "91".to_string())
    );
    let body = body_json(resp).await;
    assert_eq!(
        (&body["error"]["type"], &body["error"]["message"]),
        (
            &serde_json::json!("rate_limit_error"),
            &serde_json::json!("every account in this pool is at its fast limit")
        )
    );
    up.shutdown().await;
}

#[tokio::test]
async fn with_every_account_credits_blocked_the_router_answers_fast_credits_unavailable_with_the_pinned_accounts_reason(
) {
    let up = messages_up().await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        json_429_fast_credits("out_of_credits", None),
    );
    let h = two_accounts(&up).await;
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    body_json(resp).await;
    assert_eq!(
        rows(&h.router.cooldowns().await.unwrap()),
        [(
            "a",
            "fast_credits",
            "fast_credits",
            "out_of_credits",
            NOW + HOUR + 1_000
        )],
        "a refusal without an overage reset holds for an hour"
    );
    // The poll reads b's credits as switched off.
    h.router
        .report(Report::Credits {
            account_id: "b".into(),
            state: CreditsState {
                enabled: false,
                disabled_reason: Some("org_level_disabled".into()),
                utilization: None,
                observed_at: NOW,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    let dialed = up.requests().len();
    let reason = |resp: &axum::http::Response<Body>| {
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"]
                .to_str()
                .unwrap()
                .to_string(),
            resp.headers()["anthropic-ratelimit-unified-overage-disabled-reason"]
                .to_str()
                .unwrap()
                .to_string(),
        )
    };
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(
        reason(&resp),
        (
            429,
            "fast_credits_unavailable".to_string(),
            "org_level_disabled".to_string()
        ),
        "the session's pinned account's reason"
    );
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["message"],
        "no account in this pool can draw usage credits for fast mode"
    );
    let resp = h.app.clone().oneshot(fast_request("s2")).await.unwrap();
    assert_eq!(
        reason(&resp).2,
        "out_of_credits",
        "a session with no pin reads the first account's"
    );
    assert_eq!(up.requests().len(), dialed, "no upstream was dialled");
    up.shutdown().await;
}

/// Two fast requests in flight on one upstream at once, one refused its usage credits and one rate limited: the
/// first is held at the upstream while the second is answered, so the second's refusal lands first.
#[tokio::test]
async fn concurrent_credits_and_rate_limit_refusals_keep_a_row_each_and_a_credits_lift_leaves_the_rate_limit(
) {
    let rate_limit = || json_429_fast_rate_limit(None, Some(("1060", "1090")));
    let credits = || json_429_fast_credits("out_of_credits", None);
    for (held, answered, order) in [
        (
            credits(),
            rate_limit(),
            "a rate limit, then a credits refusal",
        ),
        (
            rate_limit(),
            credits(),
            "a credits refusal, then a rate limit",
        ),
    ] {
        let up = messages_up().await;
        up.add_credential_rule(A, "/v1/messages", json_200());
        up.push_credential_rule(A, "/v1/messages", Respond::Gated(Box::new(held)));
        up.push_credential_rule(A, "/v1/messages", answered);
        let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
        let app = h.app.clone();
        let first = tokio::spawn(async move { app.oneshot(fast_request("s1")).await.unwrap() });
        wait_for(Duration::from_secs(2), || {
            (credentials_seen(&up).len() == 1).then_some(())
        })
        .await;
        let second = h.app.clone().oneshot(fast_request("s2")).await.unwrap();
        assert_eq!(second.status(), 429, "{order}");
        body_json(second).await;
        up.release();
        let first = first.await.unwrap();
        assert_eq!(first.status(), 429, "{order}");
        body_json(first).await;
        assert_eq!(
            rows(&h.router.cooldowns().await.unwrap()),
            [
                ("a", "fast", "fast_rate_limit", "fast", 1_090_000 + 1_000),
                (
                    "a",
                    "fast_credits",
                    "fast_credits",
                    "out_of_credits",
                    NOW + HOUR + 1_000
                ),
            ],
            "{order}: each refusal keeps its own row and deadline"
        );
        let code = |resp: &axum::http::Response<Body>| {
            resp.headers()["x-phoebis-error-code"]
                .to_str()
                .unwrap()
                .to_string()
        };
        let resp = h.app.clone().oneshot(fast_request("s3")).await.unwrap();
        assert_eq!(
            code(&resp),
            "fast_credits_unavailable",
            "{order}: the credits row answers ahead of the rate limit beside it"
        );
        body_json(resp).await;
        // The poll reads a's credits back with room, after both refusals.
        h.router
            .report(Report::Credits {
                account_id: "a".into(),
                state: CreditsState {
                    enabled: true,
                    disabled_reason: None,
                    utilization: Some(0.2),
                    observed_at: NOW + 1,
                    incarnation: 1,
                },
                incarnation: 1,
            })
            .await;
        assert_eq!(
            rows(&h.router.cooldowns().await.unwrap()),
            [("a", "fast", "fast_rate_limit", "fast", 1_090_000 + 1_000)],
            "{order}: the credits reading lifts its own row alone"
        );
        let resp = h.app.clone().oneshot(fast_request("s4")).await.unwrap();
        assert_eq!(
            code(&resp),
            "fast_lane_exhausted",
            "{order}: the rate limit still refuses until its own reset"
        );
        body_json(resp).await;
        assert_eq!(
            credentials_seen(&up),
            [A, A],
            "{order}: nothing is dialled after the two refusals"
        );
        up.shutdown().await;
    }
}

#[tokio::test]
async fn a_pinned_fast_session_under_a_plan_window_cooldown_is_served_on_its_account() {
    let up = messages_up().await;
    let h = two_accounts(&up).await;
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(outcome(&resp).account_id, "a");
    body_json(resp).await;
    h.router
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: "account".into(),
                until: NOW + HOUR,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: None,
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation: 1,
        })
        .await;
    assert_eq!(h.router.cooldowns().await.unwrap().len(), 1);
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "a"),
        "a fast request draws no plan window, so the session stays"
    );
    body_json(resp).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "a standard request keeps off the spent window"
    );
    assert_eq!(credentials_seen(&up), [A, A, B]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_standard_request_on_a_fast_cooled_account_is_served_there() {
    let up = messages_up().await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.push_credential_rule(
        A,
        "/v1/messages",
        json_429_fast_rate_limit(None, Some(("1060", "1090"))),
    );
    let h = two_accounts(&up).await;
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(outcome(&resp).account_id, "b");
    body_json(resp).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s2"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "a"),
        "the fast lane's row holds no standard request"
    );
    body_json(resp).await;
    let resp = h.app.clone().oneshot(fast_request("s3")).await.unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "while a fast one still keeps off it"
    );
    assert_eq!(credentials_seen(&up), [A, B, A, B]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_fast_retry_after_under_the_bound_retries_in_place_once() {
    let up = messages_up().await;
    up.add_credential_rule(A, "/v1/messages", json_200());
    up.push_credential_rule(A, "/v1/messages", json_429_fast_rate_limit(Some(1), None));
    let h = two_accounts(&up).await;
    let started = Instant::now();
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    let waited = started.elapsed();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!((o.account_id.as_str(), o.attempt), ("a", 2));
    assert_eq!(credentials_seen(&up), [A, A]);
    assert!(
        waited >= Duration::from_millis(950),
        "the Retry-After was waited out: {waited:?}"
    );
    body_json(resp).await;
    assert_eq!(
        rows(&h.router.cooldowns().await.unwrap()),
        [(
            "a",
            "fast",
            "fast_rate_limit",
            "retry_after",
            NOW + 1_000 + 1_000
        )],
        "the row is still recorded for the siblings"
    );

    // A second throttle on the same leg is past the one retry, so the request moves on.
    h.router.clear_cooldown("a", "fast").await.unwrap();
    up.push_credential_rule(A, "/v1/messages", json_429_fast_rate_limit(Some(1), None));
    up.push_credential_rule(A, "/v1/messages", json_429_fast_rate_limit(Some(1), None));
    let resp = h.app.clone().oneshot(fast_request("s2")).await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    assert_eq!(credentials_seen(&up), [A, A, A, A, B]);
    body_json(resp).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_fast_429_that_rejects_a_shared_window_cools_the_whole_account() {
    const RESET: i64 = 4_600;
    let up = messages_up().await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    let h = two_accounts(&up).await;
    let resp = h.app.clone().oneshot(fast_request("s1")).await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    body_json(resp).await;
    assert_eq!(
        rows(&h.router.cooldowns().await.unwrap()),
        [(
            "a",
            "account",
            "rate_limit",
            "five_hour",
            RESET * 1_000 + 1_000
        )],
        "a shared plan window the head rejects keeps the account's own row, and no fast row"
    );
    up.shutdown().await;
}
