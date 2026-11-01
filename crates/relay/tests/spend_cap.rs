//! The monthly spend caps: the spend-cap 429, an account-wide cooldown with reason `spend_cap` until the first of next
//! month, and a usage-based seat at its cap, which the router holds out and answers under the claim `spend`.
mod common;

use std::sync::atomic::Ordering;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

/// The usage-based seat's OAuth secret.
const A: &str = "sk-ant-oat01-AAAA";

fn spend_cap_429() -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![],
        body: serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "This organization has reached its spend limit.",
                "details": {"error_code": "enforced_spend_limit_reached"}
            }
        })
        .to_string()
        .into_bytes(),
    }
}

#[tokio::test]
async fn a_spend_cap_answer_excludes_the_account_and_the_exhaustion_claims_overage() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: spend_cap_429(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("a", "sk-ant-api03-AAAA", 0)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .map(|v| v.to_str().unwrap()),
        Some("overage")
    );
    // The harness clock reads 1970-01-01T00:16:40Z, so the cap lifts at 1970-02-01T00:00:00Z, 2_678_400 s in, and
    // every cooldown deadline carries a one-second margin past it.
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-reset")
            .map(|v| v.to_str().unwrap()),
        Some("2678401")
    );
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (
            cooldowns[0].account_id.as_str(),
            cooldowns[0].scope.as_str(),
            cooldowns[0].reason.as_str(),
            cooldowns[0].window.as_str()
        ),
        ("a", "account", phoebis_relay::SPEND_CAP_REASON, "overage")
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_pool_its_seats_spend_cap_exhausts_claims_the_spend_window_until_its_reset() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    h.store
        .set_profile(
            "a",
            r#"{"organization":{"rate_limit_tier":"default_claude_zero"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_zero".into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let now = h.clock.load(Ordering::SeqCst);
    let reset = now + 3_600_000;
    h.router
        .report(phoebis_routing::Report::SpendHead {
            account_id: "a".into(),
            utilization: Some(1.0),
            reset: Some(reset),
            observed_at: now,
            incarnation: 1,
        })
        .await;
    h.router.spend_windows().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    assert_eq!(
        (
            header("anthropic-ratelimit-unified-representative-claim"),
            header("anthropic-ratelimit-unified-reset")
        ),
        (Some("spend".to_string()), Some((reset / 1_000).to_string())),
        "the seat at its cap names its own window and the reset its share was read under"
    );
    assert!(up.requests().is_empty(), "the seat is asked nothing");
    up.shutdown().await;
}
