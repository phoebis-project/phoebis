//! The monthly spend-cap 429: an account-wide cooldown, reason `spend_cap`, until the first of next month.
mod common;

use std::sync::atomic::Ordering;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

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
    // Every cooldown deadline carries a one-second margin past the clock it was computed against.
    let expected_reset =
        (phoebis_relay::next_month_start(h.clock.load(Ordering::SeqCst)) + 1_000) / 1_000;
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-reset")
            .map(|v| v.to_str().unwrap()),
        Some(expected_reset.to_string().as_str())
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
