//! A pool with capture stores both bodies for a key without an override; a key override of 0 stores nothing
//! while its pool captures; a key override of 6 on a pool at 0 stores for six hours; the rows vanish after
//! the retention hours.
mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_ledger::{run_retention, LedgerConfig, Purged, MAX_CAPTURED_BODY, PURGE_SLICE};
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

/// One `content_block_delta` past the cap, so the capture cuts the response and keeps the length it saw.
fn streamed_past_the_cap() -> Respond {
    let text = "a".repeat(MAX_CAPTURED_BODY + 1_024);
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
                data: format!(
                    r#"{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{text}"}}}}"#
                ),
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
    assert!(row.bodies_captured && !row.request_truncated);
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
    assert_eq!(
        bodies.request_len_seen,
        bodies.request_body.len() as u64,
        "the request body is never cut, so the seen length is what was stored"
    );
    assert_eq!(bodies.expires_at, row.finished_at.unwrap() + 24 * 3_600_000);
    up.shutdown().await;
}

#[tokio::test]
async fn a_capture_past_the_cap_says_how_much_it_dropped() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: streamed_past_the_cap(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (24, None),
        LedgerConfig::default(),
    )
    .await;
    let (id, _) = one_request(&h).await;
    let (row, _) = ledgered(&h, &id).await;
    assert!(
        row.response_truncated && !row.request_truncated,
        "the response ran past the cap; the request body never does"
    );
    let bodies = h.db.bodies(&id).await.unwrap().expect("captured");
    assert_eq!(
        bodies.response_body.as_ref().map(Vec::len),
        Some(MAX_CAPTURED_BODY),
        "the stored body is cut at the cap"
    );
    assert!(
        bodies.response_len_seen > MAX_CAPTURED_BODY as u64,
        "the seen length is the full count, past the cap"
    );
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
        six.db.clone(),
        LedgerConfig::default(),
        row.finished_at.unwrap() + 6 * 3_600_000 - 1,
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(purged, Purged::default());
    let purged = run_retention(
        six.db.clone(),
        LedgerConfig::default(),
        row.finished_at.unwrap() + 6 * 3_600_000,
        Vec::new(),
    )
    .await
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
async fn retention_purges_requests_events_and_samples_by_the_raw_window_and_rollups_by_theirs() {
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
            // Older than thirty days and younger than `retention_days`: the samples follow the requests' window.
            phoebis_ledger::QuotaSampleRecord {
                account_id: "a".into(),
                bucket: 30 * 86_400,
                window: "5h".into(),
                source: "poll".into(),
                ..Default::default()
            },
        ],
        None,
    )
    .unwrap();
    // One second past the 90-day cutoff, so the row received at 0 and the sample at bucket 0 are older than the
    // window, and the day-89 row and the day-30 sample are not.
    let now = 90 * day + 1_000;
    let purged = run_retention(h.db.clone(), LedgerConfig::default(), now, Vec::new())
        .await
        .unwrap();
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
        h.db.read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .unwrap();
    assert_eq!(rollups.len(), 2, "rollups outlive their raw rows");
    let purged = run_retention(h.db.clone(), LedgerConfig::default(), 401 * day, Vec::new())
        .await
        .unwrap();
    assert_eq!(
        purged.rollups, 1,
        "the hour-zero rollup is past 400 days; the other is not"
    );
    up.shutdown().await;
}

/// Zero on either key means forever: the purge of that family is skipped, the other families run as before. The
/// samples are raw rows, so a zero `retention_days` keeps them too.
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
    h.db.insert_samples(
        &[phoebis_ledger::QuotaSampleRecord {
            account_id: "a".into(),
            bucket: 0,
            window: "5h".into(),
            source: "poll".into(),
            ..Default::default()
        }],
        Some(&phoebis_ledger::ExtraUsageSampleRecord {
            account_id: "a".into(),
            bucket: 0,
            source: "poll".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    let forever_raw = LedgerConfig {
        retention_days: 0,
        rollup_retention_days: 400,
        ..LedgerConfig::default()
    };
    let purged = run_retention(h.db.clone(), forever_raw, 402 * day, Vec::new())
        .await
        .unwrap();
    assert_eq!(
        (
            purged.requests,
            purged.attempts,
            purged.events,
            purged.quota_samples,
            purged.extra_usage_samples
        ),
        (0, 0, 0, 0, 0),
        "nothing raw goes under 0, the samples included"
    );
    assert_eq!(purged.rollups, 1, "the hour's rollup row is past 400 days");
    h.db.insert_request(&old("r2"), &[attempt("r2")], None)
        .unwrap();
    let forever_rollups = LedgerConfig {
        retention_days: 1,
        rollup_retention_days: 0,
        ..LedgerConfig::default()
    };
    let purged = run_retention(h.db.clone(), forever_rollups, 402 * day, Vec::new())
        .await
        .unwrap();
    assert_eq!(
        (purged.requests, purged.attempts),
        (2, 2),
        "both raw rows go under a one-day window"
    );
    assert_eq!(purged.rollups, 0, "nothing in the rollups goes under 0");
    up.shutdown().await;
}

fn ms(y: i16, m: i8, d: i8, h: i8, mi: i8) -> i64 {
    jiff::civil::date(y, m, d)
        .at(h, mi, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap()
        .timestamp()
        .as_millisecond()
}

/// Folds one request for `account` in pool `p` at `at`, then copies its hourly row into `rows` rows of that hour
/// under distinct models; the request itself leaves again, so only the rollup rows stay behind.
fn plant_rollup_rows(db: &phoebis_ledger::LedgerDb, account: &str, at: i64, rows: usize) {
    let id = format!("template-{account}-{at}");
    db.insert_request(
        &phoebis_ledger::RequestRecord {
            id: id.clone(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            model: "m".into(),
            canonical_model: "m".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[phoebis_ledger::AttemptRecord {
            request_id: id.clone(),
            attempt: 1,
            account_id: account.into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(1),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    db.write(|c| {
        let columns: String = c.query_row(
            "SELECT group_concat(name, ', ') FROM pragma_table_info('rollups_hourly')",
            [],
            |r| r.get(0),
        )?;
        let copied = columns.replace("canonical_model", "canonical_model || '-' || i");
        c.execute(
            &format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1) \
                 INSERT INTO rollups_hourly ({columns}) SELECT {copied} FROM rollups_hourly, n \
                 WHERE account_id = ?2 AND hour = ?3 AND canonical_model = 'm'"
            ),
            rusqlite::params![rows as i64 - 1, account, phoebis_ledger::hour_of(at)],
        )?;
        c.execute("DELETE FROM attempts WHERE request_id = ?1", [&id])?;
        c.execute("DELETE FROM requests WHERE id = ?1", [&id])?;
        Ok(())
    })
    .unwrap();
}

fn rollup_rows_of(db: &phoebis_ledger::LedgerDb, account: &str) -> i64 {
    db.read_blocking(|c| {
        Ok(c.query_row(
            "SELECT count(*) FROM rollups_hourly WHERE account_id = ?1",
            [account],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

/// The sums of a pass that ran in slices: three slices of requests with their attempts, and a rollup cut that
/// crosses a slice boundary while a budgeted member's rows inside its period stay on both sides of it.
#[tokio::test]
async fn a_sliced_pass_sums_its_slices_and_keeps_a_budgeted_members_period_across_a_slice_boundary()
{
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let now = ms(2026, 1, 20, 12, 0);
    let config = LedgerConfig {
        retention_days: 5,
        rollup_retention_days: 5,
        ..LedgerConfig::default()
    };
    h.db.write(|c| {
        c.execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 2499) \
             INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
             SELECT 'old-' || i, {old} + i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1 FROM n; \
             WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 2499) \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome) \
             SELECT 'old-' || i, 1, 'a', 'oauth', {old} + i, 'succeeded' FROM n; \
             WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 9) \
             INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
             SELECT 'fresh-' || i, {fresh} + i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 0 FROM n;",
            old = ms(2026, 1, 2, 0, 0),
            fresh = ms(2026, 1, 19, 0, 0),
        ))?;
        Ok(())
    })
    .unwrap();
    // `a` budgets by the month, so its rows from January 1 on stay though they are older than the five-day
    // cutoff; its December rows and every row of the unbudgeted `b` go.
    plant_rollup_rows(&h.db, "a", ms(2026, 1, 10, 0, 30), 1_200);
    plant_rollup_rows(&h.db, "a", ms(2025, 12, 20, 0, 30), 300);
    plant_rollup_rows(&h.db, "b", ms(2026, 1, 10, 0, 30), 1_200);
    let budgeted = vec![phoebis_ledger::BudgetedMember {
        pool_id: "p".into(),
        account_id: "a".into(),
        period: phoebis_store::BudgetPeriod::Month,
        metered_only: false,
    }];
    let purged = run_retention(h.db.clone(), config, now, budgeted)
        .await
        .unwrap();
    assert_eq!(
        purged,
        Purged {
            attempts: 2_500,
            requests: 2_500,
            rollups: 1_500,
            ..Purged::default()
        }
    );
    let left: i64 =
        h.db.read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(left, 10, "the fresh rows stay");
    assert_eq!(
        (rollup_rows_of(&h.db, "a"), rollup_rows_of(&h.db, "b")),
        (1_200, 0),
        "the budgeted member's period stays whole across the slices that cut around it"
    );
    up.shutdown().await;
}

/// The plan lines SQLite reads for `sql` with `args` bound.
fn plan(db: &phoebis_ledger::LedgerDb, sql: &str, args: [i64; 2]) -> Vec<String> {
    db.read_blocking(|c| {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let lines = stmt
            .query_map(args, |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(lines)
    })
    .unwrap()
}

#[tokio::test]
async fn the_bodies_slice_reads_the_expiry_index_rather_than_the_table() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let lines = plan(
        &h.db,
        "SELECT rowid FROM bodies WHERE expires_at <= ?1 ORDER BY expires_at, rowid LIMIT ?2",
        [0, 1_000],
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("SEARCH bodies USING") && l.contains("INDEX bodies_expires")),
        "{lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("SCAN") || l.contains("TEMP B-TREE")),
        "{lines:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_requests_slice_reads_the_received_index_rather_than_the_table() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let lines = plan(
        &h.db,
        "SELECT rowid FROM requests WHERE received_at < ?1 ORDER BY received_at, rowid LIMIT ?2",
        [0, 1_000],
    );
    assert!(
        lines.iter().any(
            |l| l.starts_with("SEARCH requests USING") && l.contains("INDEX requests_received")
        ),
        "{lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("SCAN") || l.contains("TEMP B-TREE")),
        "{lines:?}"
    );
    up.shutdown().await;
}

/// A pass of a hundred slices with one batch landing through the writer while it runs: the batch waits behind
/// the slice that holds the lock, not behind the pass, so the rows cut while it waited are a slice or two of the
/// hundred.
#[tokio::test]
async fn a_batch_waits_for_a_slice_of_a_long_purge_pass_rather_than_the_whole_pass() {
    const OLD: usize = 100 * PURGE_SLICE;
    const DAY: i64 = 86_400_000;
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(
        phoebis_ledger::LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap(),
    );
    db.write(|c| {
        c.execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {last}) \
             INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
             SELECT 'old-' || i, i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1 FROM n; \
             WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {last}) \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome) \
             SELECT 'old-' || i, 1, 'a', 'oauth', i, 'succeeded' FROM n;",
            last = OLD - 1
        ))?;
        Ok(())
    })
    .unwrap();
    let old = |db: &phoebis_ledger::LedgerDb| -> usize {
        db.read_blocking(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM requests WHERE received_at < ?1",
                [DAY],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap() as usize
    };
    let config = LedgerConfig {
        retention_days: 1,
        ..LedgerConfig::default()
    };
    let pass = tokio::spawn(run_retention(db.clone(), config, 2 * DAY, Vec::new()));
    wait_for_async(Duration::from_secs(30), || async {
        (old(&db) < OLD).then_some(())
    })
    .await;
    // Both counts are read on the batch's own thread, so the rows between them are the ones the purge cut while
    // the batch waited for the lock.
    let (before, after) = tokio::task::spawn_blocking({
        let db = db.clone();
        move || {
            let before = old(&db);
            db.insert_request(
                &phoebis_ledger::RequestRecord {
                    id: "landed".into(),
                    received_at: 2 * DAY,
                    model: "m".into(),
                    canonical_model: "m".into(),
                    kind: "inference".into(),
                    origin: "client".into(),
                    outcome: "succeeded".into(),
                    ..Default::default()
                },
                &[],
                None,
            )
            .expect("the batch lands");
            (before, old(&db))
        }
    })
    .await
    .unwrap();
    assert!(
        after > 0,
        "the batch landed before the pass ended, not after it"
    );
    assert!(
        before - after <= 4 * PURGE_SLICE,
        "the batch waited for a slice or two, not for the {before} rows the pass still had to cut: {} went \
         while it waited",
        before - after
    );
    let purged = pass.await.unwrap().unwrap();
    assert_eq!((purged.requests, purged.attempts), (OLD, OLD));
    assert!(db.request("landed").await.unwrap().is_some());
}
