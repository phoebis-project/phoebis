//! A pool with capture stores both bodies for a key without an override; a key override of 0 stores nothing
//! while its pool captures; a key override of 6 on a pool at 0 stores for six hours; the rows vanish after
//! the retention hours.
mod common;
use std::time::Duration;

use phoebis_ledger::{run_retention, LedgerConfig, Purged};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn request_id(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .unwrap()
        .request_id
        .clone()
}

fn streamed() -> Respond {
    Respond::Sse {
        status: 200,
        headers: vec![],
        events: vec![
            SseEvent {
                event: "message_start".into(),
                data: r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#.into(),
            },
            SseEvent {
                event: "content_block_delta".into(),
                data:
                    r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#
                        .into(),
            },
            SseEvent {
                event: "message_stop".into(),
                data: r#"{"type":"message_stop"}"#.into(),
            },
        ],
        delay_between: Duration::from_millis(1),
    }
}

async fn one_request(h: &Harness) -> (String, Vec<u8>) {
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (id, body)
}

#[tokio::test]
async fn a_pool_with_capture_stores_both_bodies_for_a_key_without_an_override() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: streamed(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (24, None),
        LedgerConfig::default(),
    )
    .await;
    let (id, body) = one_request(&h).await;
    let (row, _) = ledgered(&h, &id).await;
    assert!(row.bodies_captured);
    let bodies = h.db.bodies(&id).await.unwrap().expect("captured");
    assert!(
        bodies
            .request_body
            .starts_with(br#"{"model":"claude-sonnet-4-5""#),
        "the request body as received"
    );
    assert_eq!(
        bodies.response_body.as_deref(),
        Some(body.as_slice()),
        "the response text, assembled from the chunks"
    );
    assert_eq!(bodies.expires_at, row.finished_at.unwrap() + 24 * 3_600_000);
    up.shutdown().await;
}

#[tokio::test]
async fn a_key_override_of_zero_stores_nothing_and_an_override_of_six_captures_on_a_pool_at_zero() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: streamed(),
    }])
    .await;
    let off = harness(
        &up,
        &[oauth("a", A, 0)],
        (24, Some(0)),
        LedgerConfig::default(),
    )
    .await;
    let (id, _) = one_request(&off).await;
    let (row, _) = ledgered(&off, &id).await;
    assert!(!row.bodies_captured);
    assert!(
        off.db.bodies(&id).await.unwrap().is_none(),
        "a key override of 0 stores nothing while its pool captures"
    );
    let six = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, Some(6)),
        LedgerConfig::default(),
    )
    .await;
    let (id, _) = one_request(&six).await;
    let (row, _) = ledgered(&six, &id).await;
    let bodies = six
        .db
        .bodies(&id)
        .await
        .unwrap()
        .expect("captured under the key's own hours");
    assert_eq!(bodies.expires_at, row.finished_at.unwrap() + 6 * 3_600_000);
    // Retention: the rows vanish once the hours have passed, and nothing else does.
    let purged = run_retention(
        &six.db,
        &LedgerConfig::default(),
        row.finished_at.unwrap() + 6 * 3_600_000 - 1,
    )
    .unwrap();
    assert_eq!(purged, Purged::default());
    let purged = run_retention(
        &six.db,
        &LedgerConfig::default(),
        row.finished_at.unwrap() + 6 * 3_600_000,
    )
    .unwrap();
    assert_eq!(
        purged,
        Purged {
            bodies: 1,
            ..Purged::default()
        }
    );
    assert!(six.db.bodies(&id).await.unwrap().is_none());
    assert!(
        six.db.request(&id).await.unwrap().is_some(),
        "the request row outlives its bodies"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn retention_purges_requests_events_and_samples_by_their_own_windows_and_rollups_by_theirs() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let day = 86_400_000i64;
    let old = phoebis_ledger::RequestRecord {
        id: "old".into(),
        received_at: 0,
        model: "m".into(),
        canonical_model: "m".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        ..Default::default()
    };
    let fresh = phoebis_ledger::RequestRecord {
        id: "fresh".into(),
        received_at: 89 * day,
        ..old.clone()
    };
    h.db.insert_request(
        &old,
        &[phoebis_ledger::AttemptRecord {
            request_id: "old".into(),
            attempt: 1,
            account_id: "a".into(),
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    h.db.insert_request(&fresh, &[], None).unwrap();
    h.db.insert_event(&phoebis_ledger::EventRecord {
        at: 0,
        kind: "pin_created".into(),
        ..Default::default()
    })
    .unwrap();
    h.db.insert_event(&phoebis_ledger::EventRecord {
        at: 89 * day,
        kind: "pin_created".into(),
        ..Default::default()
    })
    .unwrap();
    h.db.insert_samples(
        &[
            phoebis_ledger::QuotaSampleRecord {
                account_id: "a".into(),
                bucket: 0,
                window: "5h".into(),
                source: "poll".into(),
                ..Default::default()
            },
            phoebis_ledger::QuotaSampleRecord {
                account_id: "a".into(),
                bucket: 89 * 86_400,
                window: "5h".into(),
                source: "poll".into(),
                ..Default::default()
            },
        ],
        None,
    )
    .unwrap();
    // One past the 90-day cutoff, so the row received at 0 is older than the window and the one at day 89 is not.
    let now = 90 * day + 1;
    let purged = run_retention(&h.db, &LedgerConfig::default(), now).unwrap();
    assert_eq!(
        purged,
        Purged {
            attempts: 1,
            events: 1,
            quota_samples: 1,
            requests: 1,
            ..Purged::default()
        }
    );
    assert!(
        h.db.request("old").await.unwrap().is_none()
            && h.db.request("fresh").await.unwrap().is_some()
    );
    assert_eq!(h.db.events(0, i64::MAX, 10).await.unwrap().len(), 1);
    assert_eq!(h.db.quota_samples("a", 0, i64::MAX).await.unwrap().len(), 1);
    let rollups =
        h.db.read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
            .unwrap();
    assert_eq!(rollups.len(), 2, "rollups outlive their raw rows");
    let purged = run_retention(&h.db, &LedgerConfig::default(), 401 * day).unwrap();
    assert_eq!(
        purged.rollups, 1,
        "the hour-zero rollup is past 400 days; the other is not"
    );
    up.shutdown().await;
}

/// Zero on either key means forever: the purge of that family is skipped, the other families run as before.
#[tokio::test]
async fn a_zero_retention_keeps_the_raw_rows_and_a_zero_rollup_retention_keeps_the_rollups() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let day = 86_400_000i64;
    let old = |id: &str| phoebis_ledger::RequestRecord {
        id: id.into(),
        received_at: day,
        model: "m".into(),
        canonical_model: "m".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        ..Default::default()
    };
    let attempt = |id: &str| phoebis_ledger::AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        outcome: "succeeded".into(),
        ..Default::default()
    };
    h.db.insert_request(&old("r1"), &[attempt("r1")], None)
        .unwrap();
    let forever_raw = LedgerConfig {
        retention_days: 0,
        rollup_retention_days: 400,
        ..LedgerConfig::default()
    };
    let purged = run_retention(&h.db, &forever_raw, 402 * day).unwrap();
    assert_eq!(
        (purged.requests, purged.attempts, purged.events),
        (0, 0, 0),
        "nothing raw goes under 0"
    );
    assert_eq!(purged.rollups, 1, "the hour's rollup row is past 400 days");
    h.db.insert_request(&old("r2"), &[attempt("r2")], None)
        .unwrap();
    let forever_rollups = LedgerConfig {
        retention_days: 1,
        rollup_retention_days: 0,
        ..LedgerConfig::default()
    };
    let purged = run_retention(&h.db, &forever_rollups, 402 * day).unwrap();
    assert_eq!(
        (purged.requests, purged.attempts),
        (2, 2),
        "both raw rows go under a one-day window"
    );
    assert_eq!(purged.rollups, 0, "nothing in the rollups goes under 0");
    up.shutdown().await;
}
