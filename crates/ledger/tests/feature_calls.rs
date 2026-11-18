//! The rows of the claude.ai feature calls: each call the feature listener's routes send upstream under an account
//! leaves one, read back through the list, and the retention run takes them with the request rows.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Event, FeatureCall, FeatureEnd, FeatureOutcome};
use phoebis_ledger::{
    list_feature_calls, list_requests, run_retention_blocking, Direction, FeatureCallFilter,
    FeatureCallRow, LedgerConfig, LiveKeys, PriceRevisions, RequestFilter, RequestOrder,
};
use phoebis_store::{FeatureOverride, KeyFeatureAccounts, PoolFeatureAccounts};
use tower::ServiceExt;

const API: &str = "api.anthropic.com";

/// The pool `main` with the OAuth account `a` as its account for both features.
fn both_at_a() -> PoolFeatureAccounts {
    PoolFeatureAccounts {
        artifacts: Some("a".into()),
        connectors: Some("a".into()),
    }
}

/// The fake of `api.anthropic.com`, answering each path under `path_prefix` as `respond` says.
async fn fake(path_prefix: &str, respond: Respond) -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: path_prefix.into(),
        respond,
    }])
    .await
}

/// The harness with the account `a` as the pool's account for both features and no override on the key.
async fn features_harness(up: &FakeUpstream) -> Harness {
    harness_with_features(
        up,
        &[oauth("a", A, 0)],
        both_at_a(),
        KeyFeatureAccounts::default(),
    )
    .await
}

/// A request under the router key with the `Host` line of `api.anthropic.com`.
fn keyed(method: &str, target: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(target)
        .header("host", API)
        .header("authorization", format!("Bearer {KEY}"))
        .body(Body::empty())
        .unwrap()
}

/// The feature listener's answer to `request`, its body unread.
async fn call(h: &Harness, request: Request<Body>) -> Response {
    phoebis_relay::features::router(h.relay.clone())
        .oneshot(request)
        .await
        .unwrap()
}

async fn rows(h: &Harness) -> Vec<FeatureCallRow> {
    h.db.read(|c| list_feature_calls(c, &FeatureCallFilter::default(), None, 50))
        .await
        .unwrap()
        .0
}

/// Every row, once the first has landed.
async fn landed(h: &Harness) -> Vec<FeatureCallRow> {
    wait_for_async(Duration::from_secs(5), || async {
        let rows = rows(h).await;
        (!rows.is_empty()).then_some(rows)
    })
    .await
}

/// Every row, once one for `path` has landed. The writer lands in the order the events came, so each end published
/// ahead of that call's has landed too.
async fn rows_once(h: &Harness, path: &str) -> Vec<FeatureCallRow> {
    wait_for_async(Duration::from_secs(5), || async {
        let rows = rows(h).await;
        rows.iter().any(|row| row.path == path).then_some(rows)
    })
    .await
}

/// The outcome, the status and the code of the one row.
fn end_of(rows: &[FeatureCallRow]) -> (&str, Option<i64>, Option<&str>) {
    let [row] = rows else {
        panic!("{} rows: {rows:?}", rows.len())
    };
    (row.outcome.as_str(), row.status, row.error_code.as_deref())
}

fn stream_event() -> SseEvent {
    SseEvent {
        event: "message".into(),
        data: "first".into(),
    }
}

#[tokio::test]
async fn the_path_is_stored_without_its_query() {
    let up = fake("/api/frame/", json_200()).await;
    let h = features_harness(&up).await;
    let answer = call(&h, keyed("GET", "/api/frame/artifacts?kind=page&limit=5")).await;
    assert_eq!(answer.status(), 200);
    answer.into_body().collect().await.unwrap();
    let rows = landed(&h).await;
    assert_eq!(
        rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
        ["/api/frame/artifacts"]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refused_call_leaves_no_row() {
    let up = fake("/api/", json_200()).await;
    let h = features_harness(&up).await;
    let refused = call(&h, keyed("GET", "/api/oauth/profile")).await;
    assert_eq!(refused.status(), 403);
    let answer = call(&h, keyed("GET", "/api/frame/x")).await;
    assert_eq!(answer.status(), 200);
    answer.into_body().collect().await.unwrap();
    let rows = rows_once(&h, "/api/frame/x").await;
    assert_eq!(
        rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
        ["/api/frame/x"]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_feature_call_leaves_no_request_row() {
    let up = fake("/api/frame/", json_200()).await;
    let h = features_harness(&up).await;
    let answer = call(&h, keyed("GET", "/api/frame/x")).await;
    assert_eq!(answer.status(), 200);
    answer.into_body().collect().await.unwrap();
    landed(&h).await;
    let requests =
        h.db.read_blocking(|c| {
            list_requests(
                c,
                &RequestFilter::default(),
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
        .unwrap()
        .0;
    assert_eq!(requests.len(), 0, "a feature call wrote a request row");
    up.shutdown().await;
}

#[tokio::test]
async fn the_row_names_the_account_that_served() {
    let up = fake("/api/frame/", json_200()).await;
    let h = harness_with_features(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        both_at_a(),
        KeyFeatureAccounts {
            artifacts: FeatureOverride::Account("b".into()),
            connectors: FeatureOverride::Inherit,
        },
    )
    .await;
    let answer = call(&h, keyed("GET", "/api/frame/x")).await;
    assert_eq!(answer.status(), 200);
    answer.into_body().collect().await.unwrap();
    let rows = landed(&h).await;
    assert_eq!(
        rows.iter()
            .map(|row| row.account_id.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );
    up.shutdown().await;
}

/// A call sent under `a` and answered 200, received at `received_at`.
fn sent(id: &str, received_at: i64) -> Event {
    Event::FeatureCall(FeatureCall {
        duration_ms: 40,
        end: FeatureEnd::Sent {
            account_id: "a".into(),
            error_code: None,
            feature: "artifacts",
            key_id: "desktop".into(),
            outcome: FeatureOutcome::Forwarded,
            status: Some(200),
        },
        id: id.into(),
        method: "GET".into(),
        path: "/api/frame/x".into(),
        received_at,
    })
}

#[tokio::test]
async fn old_rows_age_out() {
    const DAY: i64 = 86_400_000;
    // 2027-01-15T08:00:00Z, whose 30-day cutoff falls on 2026-12-16T08:00:00Z.
    const NOW_MS: i64 = 1_800_000_000_000;
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.bus.publish_fact(sent("older", NOW_MS - 31 * DAY)).await;
    h.bus.publish_fact(sent("younger", NOW_MS - 29 * DAY)).await;
    wait_for_async(Duration::from_secs(5), || async {
        (rows(&h).await.len() == 2).then_some(())
    })
    .await;
    let config = LedgerConfig {
        retention_days: 30,
        ..LedgerConfig::default()
    };
    run_retention_blocking(&h.db, &config, NOW_MS, &[], &PriceRevisions::shipped()).unwrap();
    assert_eq!(
        rows(&h)
            .await
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["younger"]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_call_whose_caller_left_leaves_a_row_that_reads_canceled() {
    let up = fake(
        "/api/frame/",
        Respond::SseGated {
            status: 200,
            headers: vec![],
            events: vec![stream_event()],
        },
    )
    .await;
    let h = features_harness(&up).await;
    let answer = call(&h, keyed("GET", "/api/frame/stream")).await;
    assert_eq!(answer.status(), 200);
    drop(answer);
    let rows = landed(&h).await;
    assert_eq!(end_of(&rows), ("canceled", Some(200), None));
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_forward_that_failed_leaves_a_row_with_its_code() {
    let up = fake("/api/frame/", Respond::CloseImmediately).await;
    let h = features_harness(&up).await;
    let answer = call(&h, keyed("GET", "/api/frame/x")).await;
    assert_eq!(answer.status(), 502);
    let rows = landed(&h).await;
    assert_eq!(
        end_of(&rows),
        ("failed", None, Some("upstream_unreachable"))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_answer_cut_behind_its_head_leaves_a_failed_row_with_its_status() {
    let up = fake(
        "/api/frame/",
        Respond::SseCut {
            status: 200,
            headers: vec![],
            events: vec![stream_event()],
        },
    )
    .await;
    let h = features_harness(&up).await;
    let answer = call(&h, keyed("GET", "/api/frame/stream")).await;
    assert_eq!(answer.status(), 200);
    assert!(
        answer.into_body().collect().await.is_err(),
        "the cut ends the caller's body with an error"
    );
    let rows = landed(&h).await;
    assert_eq!(end_of(&rows), ("failed", Some(200), None));
    up.shutdown().await;
}

#[tokio::test]
async fn an_answer_without_a_body_leaves_a_forwarded_row() {
    let up = fake(
        "/api/frame/",
        Respond::Json {
            status: 204,
            headers: vec![],
            body: Vec::new(),
        },
    )
    .await;
    let h = features_harness(&up).await;
    let answer = call(&h, keyed("DELETE", "/api/frame/x")).await;
    assert_eq!(answer.status(), 204);
    // A server writes a 204 without a poll of its body and drops the body, as this caller does.
    drop(answer);
    let rows = landed(&h).await;
    assert_eq!(end_of(&rows), ("forwarded", Some(204), None));
    up.shutdown().await;
}
