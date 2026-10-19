//! A request's guardrail matches: the column and the child table migration 39 adds, the write and the two reads, the
//! request log's filter, the retention purge, the counts per guardrail with their query plans, a blocked request's row
//! and body as the assembler lands them, and the dry run's sample pages with the body each row captured.
mod common;

use std::sync::Arc;

use common::*;
use fake_upstream::FakeUpstream;
use phoebis_bus::{
    ClientFacts, Event as BusEvent, GuardrailMatch, Kind, Origin, Outcome as BusOutcome,
    RequestComplete, RequestObserved, RequestShape, ResponseFacts,
};
use phoebis_ledger::{
    guardrail_counts, guardrail_sample_page, list_requests, request_body, request_by_id,
    run_retention, BodyRecord, GuardrailCountRow, GuardrailMatchRecord, LedgerConfig, LedgerDb,
    LiveKeys, RequestFilter, RequestRecord, SampleRow, SampleSubject, GUARDRAIL_BLOCKED_BYTES_SQL,
    GUARDRAIL_COUNTS_SQL,
};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

fn matched(guardrail_id: &str, position: u32, blocked: bool) -> GuardrailMatchRecord {
    GuardrailMatchRecord {
        guardrail_id: guardrail_id.into(),
        position,
        blocked,
    }
}

/// A request row carrying its bytes and the given matches, the blocking one, where any blocked, named on the row.
fn request(id: &str, at: i64, key: &str, matches: Vec<GuardrailMatchRecord>) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: at,
        finished_at: Some(at + 10),
        key_id: Some(key.into()),
        pool_id: Some("main".into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        request_bytes: Some(1_000),
        guardrail_id: matches
            .iter()
            .find(|m| m.blocked)
            .map(|m| m.guardrail_id.clone()),
        guardrails: matches,
        ..Default::default()
    }
}

fn child_rows(db: &LedgerDb, request_id: &str) -> Vec<(String, i64)> {
    let request_id = request_id.to_string();
    db.read_blocking(move |c| {
        let mut stmt = c.prepare(
            "SELECT guardrail_id, received_at FROM request_guardrails WHERE request_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([request_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap()
}

fn listed_ids(db: &LedgerDb, guardrails: &[&str]) -> Vec<String> {
    let filter = RequestFilter {
        guardrails: guardrails.iter().map(|g| g.to_string()).collect(),
        ..RequestFilter::default()
    };
    db.read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), None, 50))
        .unwrap()
        .0
        .into_iter()
        .map(|(r, _)| r.id)
        .collect()
}

/// The detail of each step of the statement's query plan under the given parameters.
fn plan_of(db: &LedgerDb, sql: &str, params: impl rusqlite::Params) -> Vec<String> {
    db.read_blocking(|c| {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map(params, |r| r.get::<_, String>(3))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap()
}

fn names(db: &LedgerDb, sql: &str) -> Vec<String> {
    let sql = sql.to_string();
    db.read_blocking(move |c| {
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap()
}

/// A fresh ledger and one a build at 38 wrote both open at the newest version with the column on the request row, the
/// child table and its index led by the instant, and a row from before the column reads no guardrail.
#[tokio::test]
async fn migration_39_adds_the_column_the_table_and_the_index_on_a_fresh_ledger_and_on_one_at_38() {
    let fresh = Arc::new(LedgerDb::open_in_memory().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = LedgerDb::open(&path).unwrap();
        db.insert_request(&request("before", 1_000, "k", Vec::new()), &[], None)
            .unwrap();
        // Migration 40's two request columns and migration 41's index and two columns leave too, the index before
        // the column SQLite refuses to drop while indexed, so the replay from 38 adds them again.
        db.write(|c| {
            c.execute_batch(
                "DROP INDEX requests_landed_at; ALTER TABLE requests DROP COLUMN landed_at; \
                 ALTER TABLE account_rollups_hourly DROP COLUMN wall_early_ms; \
                 ALTER TABLE requests DROP COLUMN classifier; \
                 ALTER TABLE requests DROP COLUMN client_contract; \
                 DROP INDEX request_guardrails_received; DROP TABLE request_guardrails; \
                 ALTER TABLE requests DROP COLUMN guardrail_id; \
                 UPDATE meta SET value = '38' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let migrated = Arc::new(LedgerDb::open(&path).unwrap());
    for (db, which) in [
        (&fresh, "the fresh ledger"),
        (&migrated, "the ledger at 38"),
    ] {
        assert_eq!(db.schema_version().await.unwrap(), 41, "{which}");
        assert!(
            names(db, "SELECT name FROM pragma_table_info('requests')")
                .iter()
                .any(|c| c == "guardrail_id"),
            "{which}: the request row carries the blocking guardrail"
        );
        assert_eq!(
            names(
                db,
                "SELECT name FROM pragma_table_info('request_guardrails')"
            ),
            [
                "request_id",
                "guardrail_id",
                "position",
                "blocked",
                "received_at"
            ],
            "{which}"
        );
        assert!(
            names(
                db,
                "SELECT name FROM pragma_index_list('request_guardrails')"
            )
            .iter()
            .any(|i| i == "request_guardrails_received"),
            "{which}: the child table carries the index the counts read"
        );
    }
    let (before, _) = migrated.request("before").await.unwrap().unwrap();
    assert_eq!(
        (before.guardrail_id, before.guardrails),
        (None, Vec::new()),
        "a row from before the column reads no guardrail"
    );
}

/// The row carries the blocking guardrail and the child table every match with the request's instant, and both reads
/// answer the matches in evaluation order, which here runs against the ids' order.
#[test]
fn a_request_with_two_matches_round_trips_through_the_row_and_the_child_table() {
    let db = LedgerDb::open_in_memory().unwrap();
    let written = request(
        "r1",
        5_000,
        "k",
        vec![matched("zz-watch", 0, false), matched("aa-block", 1, true)],
    );
    db.insert_request(&written, &[], None).unwrap();
    let (shown, _) = db
        .read_blocking(|c| request_by_id(c, "r1"))
        .unwrap()
        .unwrap();
    assert_eq!(shown, written);
    assert_eq!(shown.guardrail_id.as_deref(), Some("aa-block"));
    let listed = db
        .read_blocking(|c| {
            list_requests(c, &RequestFilter::default(), &LiveKeys::default(), None, 10)
        })
        .unwrap()
        .0;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].0, written,
        "the list reads the matches as the show does"
    );
    assert_eq!(
        child_rows(&db, "r1"),
        [
            ("zz-watch".to_string(), 5_000),
            ("aa-block".to_string(), 5_000)
        ],
        "each child row carries the request's instant"
    );
}

/// A record written again under its id replaces its matches: the child rows it no longer names leave with the write.
#[test]
fn a_row_written_again_replaces_its_child_rows() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(
        &request(
            "r1",
            5_000,
            "k",
            vec![matched("g1", 0, false), matched("g2", 1, true)],
        ),
        &[],
        None,
    )
    .unwrap();
    let again = request("r1", 5_000, "k", vec![matched("g3", 0, false)]);
    db.insert_request(&again, &[], None).unwrap();
    assert_eq!(child_rows(&db, "r1"), [("g3".to_string(), 5_000)]);
    let (shown, _) = db
        .read_blocking(|c| request_by_id(c, "r1"))
        .unwrap()
        .unwrap();
    assert_eq!(shown, again);
}

/// A listed id keeps the requests that guardrail matched, `-` the requests no guardrail matched, and both together
/// keep either.
#[test]
fn the_filter_keeps_the_rows_of_a_listed_id_and_the_rows_of_none() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(
        &request("r-g1", 3_000, "k", vec![matched("g1", 0, false)]),
        &[],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r-g2", 2_000, "k", vec![matched("g2", 0, true)]),
        &[],
        None,
    )
    .unwrap();
    db.insert_request(&request("r-none", 1_000, "k", Vec::new()), &[], None)
        .unwrap();
    assert_eq!(listed_ids(&db, &["g1"]), ["r-g1"]);
    assert_eq!(listed_ids(&db, &["-"]), ["r-none"]);
    assert_eq!(listed_ids(&db, &["g1", "-"]), ["r-g1", "r-none"]);
}

/// The retention pass takes a request's child rows with the request, and a request inside the window keeps its own.
#[tokio::test]
async fn the_purge_removes_the_child_rows_with_the_request() {
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_request(
        &request(
            "old",
            0,
            "k",
            vec![matched("g1", 0, false), matched("g2", 1, true)],
        ),
        &[],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("fresh", 29 * DAY_MS, "k", vec![matched("g1", 0, false)]),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(
        child_rows(&db, "old").len(),
        2,
        "the old request's matches are on record"
    );
    let config = LedgerConfig {
        retention_days: 30,
        ..LedgerConfig::default()
    };
    run_retention(db.clone(), config, 30 * DAY_MS + 1_000, Vec::new())
        .await
        .unwrap();
    assert!(db.request("old").await.unwrap().is_none());
    assert_eq!(
        names(&db, "SELECT request_id FROM request_guardrails"),
        ["fresh"],
        "no child row outlives its request"
    );
}

/// Per guardrail over the range: the matches that let the request through, the ones that blocked it, and the blocked
/// requests' bytes alone, each bound inclusive, a guardrail matched only outside the range left out.
#[test]
fn the_counts_answer_observed_blocked_and_bytes_per_guardrail_over_a_range() {
    let db = LedgerDb::open_in_memory().unwrap();
    let with_bytes =
        |id: &str, at: i64, bytes: u64, matches: Vec<GuardrailMatchRecord>| RequestRecord {
            request_bytes: Some(bytes),
            ..request(id, at, "k", matches)
        };
    for row in [
        with_bytes(
            "r1",
            11_000,
            1_000,
            vec![matched("g1", 0, false), matched("g3", 1, false)],
        ),
        with_bytes("r2", 12_000, 2_000, vec![matched("g1", 0, true)]),
        with_bytes(
            "r3",
            20_000,
            4_000,
            vec![matched("g1", 0, false), matched("g3", 1, true)],
        ),
        with_bytes("r4", 25_000, 8_000, vec![matched("g2", 0, true)]),
        with_bytes("r5", 5_000, 16_000, vec![matched("g1", 0, true)]),
    ] {
        db.insert_request(&row, &[], None).unwrap();
    }
    let counts = db
        .read_blocking(|c| guardrail_counts(c, 10_000, 20_000))
        .unwrap();
    assert_eq!(
        counts,
        [
            GuardrailCountRow {
                guardrail_id: "g1".into(),
                observed: 2,
                blocked: 1,
                blocked_bytes: 2_000,
            },
            GuardrailCountRow {
                guardrail_id: "g3".into(),
                observed: 1,
                blocked: 1,
                blocked_bytes: 4_000,
            },
        ]
    );
}

/// Both statements the counts route runs seek the child table's index led by the instant, so neither walks the
/// table or its primary key on a reload of the page.
#[test]
fn the_counts_query_and_the_bytes_query_seek_the_received_index() {
    let db = LedgerDb::open_in_memory().unwrap();
    let counts = plan_of(&db, GUARDRAIL_COUNTS_SQL, (0_i64, 0_i64));
    let bytes = plan_of(&db, GUARDRAIL_BLOCKED_BYTES_SQL, ("g1", 0_i64, 0_i64));
    for (plan, which) in [(&counts, "the counts"), (&bytes, "the bytes")] {
        assert!(
            plan.iter().any(|step| {
                step.contains("USING COVERING INDEX request_guardrails_received")
                    || step.contains("USING INDEX request_guardrails_received")
            }),
            "{which} read seeks the index led by the instant: {plan:?}"
        );
    }
}

/// A record parked by a build from before the guardrails reads with no blocking guardrail and no match.
#[test]
fn a_parked_record_from_before_reads_both_members_at_their_defaults() {
    let parked: RequestRecord = serde_json::from_str(r#"{"id":"r1","received_at":5}"#).unwrap();
    assert_eq!((parked.guardrail_id, parked.guardrails), (None, Vec::new()));
}

/// The observation carries the block and the key's capture window, so the refusal's row names the guardrail and its
/// matches beside the rejection's code, and its body lands under the window as any refusal's does.
#[tokio::test]
async fn a_blocked_request_with_a_body_keeps_its_body_row_under_the_keys_window() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (24, None),
        LedgerConfig::default(),
    )
    .await;
    let body = br#"{"model":"claude-sonnet-5","max_tokens":64,"messages":[]}"#;
    h.bus.publish(BusEvent::RequestObserved(RequestObserved {
        request_id: "blocked".into(),
        origin: Origin::Client,
        kind: Kind::Inference,
        key_id: Some("desktop".into()),
        key_generation: 0,
        pool_id: Some("main".into()),
        session_id: None,
        agent_id: None,
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: false,
        headers: vec![],
        body: bytes::Bytes::from_static(body),
        body_capture_hours: 24,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        guardrail_id: Some("g-block".into()),
        guardrails: vec![
            GuardrailMatch {
                id: "g-watch".into(),
                position: 0,
                blocked: false,
            },
            GuardrailMatch {
                id: "g-block".into(),
                position: 1,
                blocked: true,
            },
        ],
        received_at: NOW,
    }));
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        request_id: "blocked".into(),
        attempt: 0,
        account_id: None,
        outcome: BusOutcome::Rejected,
        status: 400,
        usage: None,
        response: ResponseFacts::default(),
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: Some("guardrail_blocked".into()),
        client_drift: None,
        client_contract: None,
        client_version: None,
        response_bytes: None,
        response_chunks: None,
        held: None,
        diagnostics: None,
        message_id: None,
        skipped_events: 0,
        finished_at: NOW + 5,
    }));
    let (row, attempts) = ledgered(&h, "blocked").await;
    assert_eq!(
        (
            row.outcome.as_str(),
            row.status,
            row.error_code.as_deref(),
            row.attempts,
            attempts.len()
        ),
        ("rejected", Some(400), Some("guardrail_blocked"), 0, 0)
    );
    assert_eq!(row.guardrail_id.as_deref(), Some("g-block"));
    assert_eq!(
        row.guardrails,
        [matched("g-watch", 0, false), matched("g-block", 1, true)]
    );
    assert!(row.bodies_captured);
    let stored = h.db.bodies("blocked").await.unwrap().expect("the body row");
    assert_eq!(stored.request_body, body);
    assert_eq!(stored.expires_at, NOW + 5 + 24 * 3_600_000);
    up.shutdown().await;
}

/// A row of `key` in the pool `main`, captured or not, of the given kind, with no match.
fn sampled(id: &str, at: i64, key: &str, captured: bool, kind: &str) -> RequestRecord {
    RequestRecord {
        bodies_captured: captured,
        kind: kind.into(),
        ..request(id, at, key, Vec::new())
    }
}

/// The ids of one sample page and its next cursor.
fn sample_ids(
    db: &LedgerDb,
    subject: SampleSubject<'_>,
    since: i64,
    cursor: Option<&str>,
    limit: usize,
) -> (Vec<String>, Option<String>) {
    let (rows, next) = db
        .read_blocking(|c| guardrail_sample_page(c, subject, since, cursor, limit))
        .unwrap();
    (rows.into_iter().map(|row| row.id).collect(), next)
}

/// A pool's sample reads both of its keys and a key's sample the one key, newest first, each leaving out a probe, an
/// uncaptured row, another pool's row and a row older than the window while keeping a row a few hours inside it,
/// and the cursor carries the walk across pages.
#[test]
fn the_sample_page_walks_the_newest_captured_rows_of_a_pool_or_a_key_and_skips_probes_and_uncaptured_rows(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    // The newest rows sit seconds apart, the window reaches 72 hours back, `g` sits three hours inside it and `old`
    // 1,000 hours back past it, so a bound placed where a window read in minutes puts it leaves `g` out.
    let now = 1_000 * HOUR_MS;
    let hinted = RequestRecord {
        client: Some("claude-code".into()),
        request_class: Some("auxiliary".into()),
        agent_type: Some("general-purpose".into()),
        user_agent: Some("claude-cli/2.1.300".into()),
        compaction: Some("auto".into()),
        context_compacted: Some("true".into()),
        ..sampled("a", now, "k1", true, "inference")
    };
    for row in [
        hinted,
        sampled("b", now - 1_000, "k2", true, "count_tokens"),
        sampled("c", now - 2_000, "k1", false, "inference"),
        sampled("d", now - 3_000, "k2", true, "probe"),
        sampled("e", now - 4_000, "k1", true, "inference"),
        sampled("f", now - 5_000, "k2", true, "inference"),
        sampled("g", now - 3 * HOUR_MS, "k1", true, "inference"),
        sampled("old", now - 1_000 * HOUR_MS, "k1", true, "inference"),
        RequestRecord {
            pool_id: Some("other".into()),
            ..sampled("x", now - 500, "k3", true, "inference")
        },
    ] {
        db.insert_request(&row, &[], None).unwrap();
    }
    let since = now - 72 * HOUR_MS;
    let pool = SampleSubject::Pool("main");
    assert_eq!(
        sample_ids(&db, pool, since, None, 10),
        (
            vec!["a".into(), "b".into(), "e".into(), "f".into(), "g".into()],
            None
        ),
        "both keys of the pool, newest first"
    );
    assert_eq!(
        sample_ids(&db, SampleSubject::Key("k1"), since, None, 10),
        (vec!["a".into(), "e".into(), "g".into()], None)
    );
    assert_eq!(
        sample_ids(&db, SampleSubject::Key("k2"), since, None, 10),
        (vec!["b".into(), "f".into()], None)
    );
    assert_eq!(
        sample_ids(&db, pool, 0, None, 10).0,
        ["a", "b", "e", "f", "g", "old"],
        "the window alone leaves the old row out"
    );

    let (first, next) = sample_ids(&db, pool, since, None, 2);
    let cursor = format!("{}:b", now - 1_000);
    assert_eq!(
        (first, next.as_deref()),
        (vec!["a".into(), "b".into()], Some(cursor.as_str()))
    );
    assert_eq!(
        sample_ids(&db, pool, since, next.as_deref(), 3),
        (vec!["e".into(), "f".into(), "g".into()], None),
        "a last page of exactly the limit offers no cursor"
    );

    let (rows, _) = db
        .read_blocking(|c| guardrail_sample_page(c, pool, since, None, 1))
        .unwrap();
    assert_eq!(
        rows,
        [SampleRow {
            id: "a".into(),
            received_at: now,
            kind: "inference".into(),
            canonical_model: "claude-sonnet-5".into(),
            client: Some("claude-code".into()),
            request_class: Some("auxiliary".into()),
            agent_type: Some("general-purpose".into()),
            user_agent: Some("claude-cli/2.1.300".into()),
            compaction: Some("auto".into()),
            context_compacted: Some("true".into()),
        }],
        "the row carries the facts and the five hints it keeps"
    );
}

/// The captured body comes back byte for byte, and a row whose body row has gone answers none, as does a row that
/// never had one.
#[test]
fn request_body_answers_the_captured_bytes_and_none_where_the_body_row_is_gone() {
    let db = LedgerDb::open_in_memory().unwrap();
    let body = br#"{"model":"claude-sonnet-5","messages":[]}"#;
    for id in ["kept", "aged"] {
        db.insert_request(
            &sampled(id, 5_000, "k", true, "inference"),
            &[],
            Some(&BodyRecord {
                request_id: id.into(),
                request_body: body.to_vec(),
                response_body: None,
                request_len_seen: body.len() as u64,
                response_len_seen: 0,
                expires_at: 1_000_000,
            }),
        )
        .unwrap();
    }
    db.write(|c| {
        c.execute("DELETE FROM bodies WHERE request_id = 'aged'", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        db.read_blocking(|c| request_body(c, "kept", 5_000))
            .unwrap(),
        Some(body.to_vec())
    );
    assert_eq!(
        db.read_blocking(|c| request_body(c, "aged", 5_000))
            .unwrap(),
        None,
        "the row still reads captured, and its body has aged out"
    );
    assert_eq!(
        db.read_blocking(|c| request_body(c, "never", 5_000))
            .unwrap(),
        None
    );
}

/// A body row past its own expiry answers none before any purge has removed it, while a body inside its window still
/// answers its bytes, so the dry run never reads content its key's capture window no longer covers.
#[test]
fn request_body_answers_none_for_a_body_past_its_expiry_that_no_purge_has_removed_yet() {
    let db = LedgerDb::open_in_memory().unwrap();
    let body = br#"{"model":"claude-sonnet-5","messages":[]}"#;
    let now = 10 * HOUR_MS;
    let live_expiry = now + HOUR_MS / 2;
    // Both captured under a one-hour window: one received two hours ago, one half an hour ago.
    for (id, expires_at) in [("expired", now - HOUR_MS), ("live", live_expiry)] {
        db.insert_request(
            &sampled(id, expires_at - HOUR_MS, "k", true, "inference"),
            &[],
            Some(&BodyRecord {
                request_id: id.into(),
                request_body: body.to_vec(),
                response_body: None,
                request_len_seen: body.len() as u64,
                response_len_seen: 0,
                expires_at,
            }),
        )
        .unwrap();
    }
    let stored: i64 = db
        .read_blocking(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM bodies WHERE request_id = 'expired'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(stored, 1, "no purge has removed the expired body row");
    assert_eq!(
        db.read_blocking(|c| request_body(c, "expired", now))
            .unwrap(),
        None,
        "the body's expiry passed an hour ago"
    );
    assert_eq!(
        db.read_blocking(|c| request_body(c, "live", now)).unwrap(),
        Some(body.to_vec())
    );
    assert_eq!(
        db.read_blocking(|c| request_body(c, "live", live_expiry))
            .unwrap(),
        None,
        "the expiry instant itself reads expired, as the purge deletes at it"
    );
}
