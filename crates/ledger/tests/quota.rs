//! A response head writes a sample in its bucket; a poll in the same bucket does not overwrite it; the Fable
//! window is read from limits[]; extra-usage credits come from the poll only; a paused account is polled.
mod common;
use std::time::Duration;

use phoebis_ledger::LedgerConfig;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use tower::ServiceExt;

const USAGE: &[u8] = br#"{"five_hour":{"utilization":12.5,"resets_at":"2026-09-13T15:00:00+00:00"},"seven_day":{"utilization":40.0,"resets_at":"2026-09-18T00:00:00+00:00"},"cinder_cove":{"utilization":99.0,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":42,"resets_at":"2026-09-18T00:00:00+00:00","scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":0.2468,"currency":"USD"}}"#;

fn usage_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: USAGE.to_vec(),
        },
    }
}

fn profile_rule() -> Rule {
    Rule {
        path_prefix: "/api/oauth/profile".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Profile,
            )
            .to_vec(),
        },
    }
}

fn head_with_windows() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "0.77".into(),
            ),
            ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
            (
                "anthropic-ratelimit-unified-7d-utilization".into(),
                "0.2".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-in-use".into(),
                "false".into(),
            ),
        ],
        body: br#"{"id":"msg_1","type":"message","usage":{"input_tokens":1,"output_tokens":1}}"#
            .to_vec(),
    }
}

#[tokio::test]
async fn a_head_writes_its_bucket_and_a_poll_in_the_same_bucket_does_not_overwrite_it() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: head_with_windows(),
        },
        usage_rule(),
        profile_rule(),
    ])
    .await;
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            quota_poll_interval: Duration::from_millis(200),
            ..LedgerConfig::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let bucket = phoebis_ledger::bucket_of(NOW);
    let header = wait_for(Duration::from_secs(5), || {
        let rows = h.db.quota_samples("a", bucket, bucket + 300).unwrap();
        rows.iter()
            .any(|r| r.window == "5h" && r.source == "header")
            .then_some(rows)
    })
    .await;
    let five = header.iter().find(|r| r.window == "5h").unwrap();
    assert_eq!(
        (five.utilization, five.reset_at, five.bucket),
        (Some(0.77), Some(4_600_000), bucket)
    );
    assert!(header
        .iter()
        .any(|r| r.window == "7d" && r.utilization == Some(0.2)));
    let extra = h.db.extra_usage_samples("a", bucket, bucket + 300).unwrap();
    assert_eq!(
        (
            extra[0].source.as_str(),
            extra[0].in_use,
            extra[0].used_credits
        ),
        ("header", Some(false), None),
        "a head carries no credits"
    );
    // The poll lands in the same bucket (the clock is frozen at NOW): the header rows stay, the poll adds its
    // own windows beside them, and the credits arrive.
    wait_for(Duration::from_secs(5), || {
        let rows = h.db.quota_samples("a", bucket, bucket + 300).unwrap();
        rows.iter().any(|r| r.window == "fable").then_some(rows)
    })
    .await;
    let rows = h.db.quota_samples("a", bucket, bucket + 300).unwrap();
    let five = rows.iter().find(|r| r.window == "5h").unwrap();
    assert_eq!(
        (five.source.as_str(), five.utilization),
        ("header", Some(0.77)),
        "the header sample survived the poll"
    );
    let fable = rows.iter().find(|r| r.window == "fable").unwrap();
    assert_eq!(
        (fable.source.as_str(), fable.utilization),
        ("poll", Some(0.42)),
        "read from limits[], not from the named key"
    );
    assert!(rows.iter().all(|r| r.window != "cinder_cove"));
    assert!(rows
        .iter()
        .any(|r| r.window == "five_hour" && r.utilization == Some(0.125)));
    let extra = h.db.extra_usage_samples("a", bucket, bucket + 300).unwrap();
    assert_eq!(extra.len(), 1, "one extra-usage row per bucket");
    assert_eq!(
        (extra[0].source.as_str(), extra[0].used_credits),
        ("header", None),
        "the header row keeps its bucket; credits wait for a bucket the poll owns"
    );
    h.clock
        .store(NOW + 300_000, std::sync::atomic::Ordering::SeqCst);
    let next = bucket + 300;
    let extra = wait_for(Duration::from_secs(5), || {
        let rows = h.db.extra_usage_samples("a", next, next + 300).unwrap();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        (
            extra[0].source.as_str(),
            extra[0].monthly_limit,
            extra[0].used_credits,
            extra[0].utilization
        ),
        ("poll", Some(5_000), Some(1_234), Some(0.2468)),
        "extra-usage credits come from the poll only"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_paused_oauth_account_receives_hourly_samples_and_an_unpaired_one_does_not() {
    let up = FakeUpstream::start(vec![usage_rule(), profile_rule()]).await;
    let h = harness_with_poll(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        (0, None),
        LedgerConfig {
            quota_poll_interval: Duration::from_millis(200),
            ..LedgerConfig::default()
        },
    )
    .await;
    h.store.set_enabled("a", false).unwrap();
    h.store
        .set_state("b", phoebis_store::AccountState::Unpaired)
        .unwrap();
    h.router.reload().await.unwrap();
    let bucket = phoebis_ledger::bucket_of(NOW);
    wait_for(Duration::from_secs(5), || {
        (!h.db
            .quota_samples("a", bucket, bucket + 300)
            .unwrap()
            .is_empty())
        .then_some(())
    })
    .await;
    stays_for(Duration::from_millis(500), || {
        h.db.quota_samples("b", 0, i64::MAX).unwrap().is_empty()
    })
    .await;
    let polls = up
        .requests()
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
        .count();
    assert!(polls >= 1, "the paused account was polled");
    let profiles = up
        .requests()
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
        .count();
    assert!(profiles >= 1, "every poll refreshes the profile too");
    wait_for(Duration::from_secs(5), || {
        h.store.profile_json("a").unwrap()
    })
    .await;
    assert!(
        h.store.profile_json("b").unwrap().is_none(),
        "an unpaired account is not polled"
    );
    up.shutdown().await;
}
