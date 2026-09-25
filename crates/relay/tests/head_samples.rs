//! A response head lands a selection sample on the router: the bucket headers for an api_key account, the
//! unified windows for an OAuth account.
mod common;

use phoebis_routing::{FIVE_HOUR, SEVEN_DAY};
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use tower::ServiceExt;

const CLOSE: f64 = 1e-9;

#[tokio::test]
async fn a_head_from_each_account_kind_lands_a_sample_the_router_answers() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        "sk-ant-api03-AAAA",
        "/v1/messages",
        json_200_with_headers(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "40"),
            ("anthropic-ratelimit-input-tokens-limit", "100000"),
            ("anthropic-ratelimit-input-tokens-remaining", "75000"),
            ("anthropic-ratelimit-output-tokens-limit", "20000"),
            ("anthropic-ratelimit-output-tokens-remaining", "5000"),
        ]),
    );
    let ha = harness(
        &up,
        &[api_key("k", "sk-ant-api03-AAAA", 0)],
        PoolSpec::default(),
    )
    .await;
    ha.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let samples = ha.router.samples().await.unwrap();
    let (_, sample) = samples
        .iter()
        .find(|(id, _)| id == "k")
        .expect("the api_key account carries a sample");
    assert!(sample
        .reading("rpm:claude-sonnet-4-5-20250929")
        .is_some_and(|r| (r.utilization - 0.2).abs() < CLOSE));

    // A second head for another model keeps the first model's buckets: the sample folds per window, never
    // replacing the whole map wholesale.
    up.set_credential_rule_after(
        "sk-ant-api03-AAAA",
        1,
        "/v1/messages",
        json_200_with_headers(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "0"),
        ]),
    );
    ha.app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s2"))
        .await
        .unwrap();
    let samples = ha.router.samples().await.unwrap();
    let (_, sample) = samples.iter().find(|(id, _)| id == "k").unwrap();
    assert!(
        sample
            .reading("rpm:claude-sonnet-4-5-20250929")
            .is_some_and(|r| (r.utilization - 0.2).abs() < CLOSE),
        "the first model's bucket is still there"
    );
    assert!(
        sample
            .reading("rpm:claude-opus-5")
            .is_some_and(|r| (r.utilization - 1.0).abs() < CLOSE),
        "the second model's bucket landed beside it"
    );
    up.shutdown().await;

    let up_oauth = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200_with_headers(&[
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.13"),
        ]),
    }])
    .await;
    let ho = harness(
        &up_oauth,
        &[oauth("o", "sk-ant-oat01-OOOO", 0)],
        PoolSpec::default(),
    )
    .await;
    ho.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let samples = ho.router.samples().await.unwrap();
    let (_, sample) = samples
        .iter()
        .find(|(id, _)| id == "o")
        .expect("the oauth account carries a sample");
    assert!(sample
        .reading(FIVE_HOUR)
        .is_some_and(|r| (r.utilization - 0.42).abs() < CLOSE));
    assert!(sample
        .reading(SEVEN_DAY)
        .is_some_and(|r| (r.utilization - 0.13).abs() < CLOSE));
    up_oauth.shutdown().await;
}
