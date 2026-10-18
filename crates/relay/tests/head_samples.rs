//! A response head lands a selection sample on the router: the bucket headers for an api_key account, the
//! unified windows for an OAuth account.
mod common;

use std::time::Duration;

use common::*;
use fake_upstream::{wait_for_async, FakeUpstream, Respond, Rule};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{FAST_INPUT, FAST_OUTPUT};
use phoebis_routing::{RouterConfig, SharePricer, FIVE_HOUR, SEVEN_DAY};
use phoebis_store::SubscriptionRow;
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

/// The six `anthropic-fast-*` headers of an answer: the input bucket a quarter used, the output bucket half used.
const FAST_HEADERS: [(&str, &str); 6] = [
    ("anthropic-fast-input-tokens-limit", "400000"),
    ("anthropic-fast-input-tokens-remaining", "300000"),
    ("anthropic-fast-input-tokens-reset", "1700003600"),
    ("anthropic-fast-output-tokens-limit", "80000"),
    ("anthropic-fast-output-tokens-remaining", "40000"),
    ("anthropic-fast-output-tokens-reset", "2026-09-30T12:00:00Z"),
];

#[tokio::test]
async fn a_fast_answers_six_headers_read_into_two_fast_readings_beside_the_kinds_own_and_persist_nowhere(
) {
    let close = |reading: Option<phoebis_routing::WindowReading>, expected: f64| {
        reading.is_some_and(|r| (r.utilization - expected).abs() < CLOSE)
    };
    let with_fast = |own: &[(&'static str, &'static str)]| {
        let mut headers = own.to_vec();
        headers.extend(FAST_HEADERS);
        json_200_with_headers(&headers)
    };

    let up_oauth = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: with_fast(&[
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
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
    assert!(
        close(sample.reading(FIVE_HOUR), 0.42),
        "the plan window still lands"
    );
    assert!(close(sample.reading(FAST_INPUT), 0.25));
    assert!(close(sample.reading(FAST_OUTPUT), 0.5));
    assert_eq!(
        sample.reading(FAST_INPUT).and_then(|r| r.reset),
        Some(1_700_003_600_000),
        "an epoch-seconds reset reads in milliseconds"
    );
    assert_eq!(
        sample.reading(FAST_OUTPUT).and_then(|r| r.reset),
        Some(
            "2026-09-30T12:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_millisecond()
        ),
        "an RFC 3339 reset reads too"
    );
    // The plan window's row is the proof the writer ran for this head, so the fast pair's absence is not a race.
    let stored = wait_for_async(Duration::from_secs(2), || {
        let store = ho.store.clone();
        async move {
            let windows = store.load_routing().unwrap().windows;
            windows
                .iter()
                .any(|w| w.window == FIVE_HOUR)
                .then_some(windows)
        }
    })
    .await;
    assert!(
        stored
            .iter()
            .all(|w| w.window != FAST_INPUT && w.window != FAST_OUTPUT),
        "the fast pair is held in memory alone: {stored:?}"
    );
    up_oauth.shutdown().await;

    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        "sk-ant-api03-AAAA",
        "/v1/messages",
        with_fast(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "40"),
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
    assert!(close(sample.reading("rpm:claude-sonnet-4-5-20250929"), 0.2));
    assert!(close(sample.reading(FAST_INPUT), 0.25));
    assert!(close(sample.reading(FAST_OUTPUT), 0.5));
    assert!(
        sample
            .worst_bucket(sample.observed_at)
            .is_some_and(|r| (r.utilization - 0.2).abs() < CLOSE),
        "the fast pair never reads as the worst api_key bucket"
    );
    // The shutdown drains the router's queue and its writer, so the empty table is read after any write this head
    // could have queued.
    ha.router.shutdown().await.unwrap();
    assert!(
        ha.store.load_routing().unwrap().windows.is_empty(),
        "an api_key account persists no window"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_finished_answer_reports_its_usage_to_the_router() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![
                // A second before the harness clock, so the answer completes after the reading its head carries.
                ("date".into(), "Thu, 01 Jan 1970 00:16:39 GMT".into()),
                ("anthropic-ratelimit-unified-5h-status".into(), "allowed".into()),
                ("anthropic-ratelimit-unified-5h-utilization".into(), "0.42".into()),
                ("anthropic-ratelimit-unified-7d-status".into(), "allowed".into()),
                ("anthropic-ratelimit-unified-7d-utilization".into(), "0.13".into()),
            ],
            body: br#"{"id":"msg_usage","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1000000,"output_tokens":0}}"#.to_vec(),
        },
    }])
    .await;
    let h = harness_with_router_config(
        &up,
        &[oauth("o", "sk-ant-oat01-OOOO", 0)],
        PoolSpec::default(),
        RouterConfig {
            share: SharePricer::new(|_, _, _, _, usage, _| {
                Some(usage.input as f64 / 1_000_000.0 * 0.1)
            }),
            ..RouterConfig::default()
        },
    )
    .await;
    h.store
        .set_profile(
            "o",
            "{}",
            &SubscriptionRow {
                tier: Some("default_claude_max_5x".into()),
                ..SubscriptionRow::default()
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let response = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let read = |window: &'static str, expected: f64| {
        let router = h.router.clone();
        async move {
            router
                .samples()
                .await
                .unwrap()
                .into_iter()
                .find(|(id, _)| id == "o")
                .and_then(|(_, sample)| sample.reading(window))
                .filter(|r| (r.utilization - expected).abs() < CLOSE)
        }
    };
    wait_for_async(Duration::from_secs(2), || read(FIVE_HOUR, 0.52)).await;
    wait_for_async(Duration::from_secs(2), || read(SEVEN_DAY, 0.23)).await;

    // Another session's answer on another model adds its usage to the same account's windows.
    let mut other: serde_json::Value = serde_json::from_slice(body(BodyFixture::Oauth)).unwrap();
    other["model"] = "claude-opus-5".into();
    let mut second = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &serde_json::to_vec(&other).unwrap(),
        KEY,
    );
    set_header(&mut second, "x-claude-code-session-id", "s2");
    let response = h.app.clone().oneshot(second).await.unwrap();
    assert_eq!(response.status(), 200);
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    wait_for_async(Duration::from_secs(2), || read(FIVE_HOUR, 0.62)).await;
    wait_for_async(Duration::from_secs(2), || read(SEVEN_DAY, 0.33)).await;
    up.shutdown().await;
}
