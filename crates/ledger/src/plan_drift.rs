//! The pinned plans under statistics shaped like a live file's: one key holding nearly every row, many sessions
//! holding few, a few accounts. Every statement whose plan a test pins without statistics runs here against an
//! analyzed file, so a plan the statistics move is caught before a live file moves it. The session identities read, the
//! top sessions read and the extra usage reads are the exceptions: each of their table references names its index,
//! seeks the primary key on its leading column or reads by rowid, so no statistics move their plans. Each keeps its own
//! test in `query.rs`, and the session identities test's planted statistics catch a dropped hint.
//!
//! The file is seeded and analyzed once per process and each test opens its own copy of the bytes, since the seed is
//! most of a second's writes and every test reads the same file. A copy per test keeps the tests independent of each
//! other's reads and opens.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use phoebis_store::AccountKind;
use rusqlite::{params, params_from_iter};

use crate::bodies::{
    EXPIRED_BODY_BYTES, PURGE_ATTEMPTS, PURGE_BODIES, PURGE_DECISIONS, PURGE_REQUESTS,
    PURGE_REQUEST_GUARDRAILS,
};
use crate::db::{begin_write, insert_decision_in, ANALYSIS_LIMIT, LANDING_STAMP_SQL};
use crate::query::{
    decision_rows_statement, decision_statement, list_requests_statement,
    session_attempts_statement, LiveKeys, RequestFilter, RoutingLogFilter,
    GUARDRAIL_BLOCKED_BYTES_SQL, GUARDRAIL_COUNTS_SQL, GUARDRAIL_SAMPLE_CURSOR_SQL,
    GUARDRAIL_SAMPLE_KEY_SQL, GUARDRAIL_SAMPLE_ORDER_SQL, GUARDRAIL_SAMPLE_POOL_SQL,
    HOURLY_WINDOW_SAMPLES_SQL, LATEST_QUOTA_SQL, RECENT_EVENTS_OF_KIND,
};
use crate::record::{
    AttemptRecord, BodyRecord, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    GuardrailMatchRecord, QuotaSampleRecord, RequestRecord, SampleSource,
};
use crate::rollup::{key_range_statement, select_statement, RollupSelect};
use crate::LedgerDb;

/// 2026-01-01 00:00 UTC in milliseconds, the first request's arrival.
const FROM: i64 = 1_767_225_600_000;
const DAY: i64 = 86_400_000;
const TO: i64 = FROM + DAY;
const REQUESTS: i64 = 2_000;
/// The generation every seeded key carries, which a live key's selector resolves to.
const KEY_GENERATION: i64 = 1;
const ACCOUNTS: [&str; 3] = ["a1", "a2", "a3"];
const POOLS: [&str; 2] = ["p1", "p2"];
const MODEL: &str = "claude-sonnet-5";

/// `sqlite_stat1`'s rows as (`tbl`, `idx`, `stat`), ordered by all three.
type Statistics = Vec<(String, Option<String>, String)>;

/// The seeded and analyzed file's bytes beside the statistics the analysis wrote. The initializer's directory goes
/// when it returns or unwinds, so the shared seed leaves nothing behind.
static SEEDED: OnceLock<(Vec<u8>, Statistics)> = OnceLock::new();

/// A ledger of its own on a copy of the seeded file. Its `sqlite_stat1` rows, read through a pooled reader, must equal
/// the seed's: that proves the copy carries the seed's statistics in full and that the reader the tests plan on sees
/// them, so no test plans on an unanalyzed file.
fn seeded() -> (tempfile::TempDir, Arc<LedgerDb>) {
    let (bytes, analyzed_statistics) = SEEDED.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let db = LedgerDb::open(&path).unwrap();
        seed(&db);
        analyzed(&db);
        let kept = statistics(&db);
        // A copy carries the main file alone, so every page the seed wrote leaves the WAL first.
        let busy: i64 = db
            .write(|c| Ok(c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(busy, 0, "the checkpoint moved every page into the file");
        drop(db);
        (std::fs::read(&path).unwrap(), kept)
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    std::fs::write(&path, bytes).unwrap();
    let db = Arc::new(LedgerDb::open(&path).unwrap());
    assert_eq!(
        &statistics(&db),
        analyzed_statistics,
        "the copy's statistics, read through a pooled reader, differ from the seed's"
    );
    (dir, db)
}

/// `sqlite_stat1` through a pooled reader.
fn statistics(db: &LedgerDb) -> Statistics {
    db.read_blocking(|c| Ok(crate::db::stat1_rows(c)?)).unwrap()
}

/// 2,000 requests over 400 sessions of five, 1,990 of them under the key `main` and one under each of ten other
/// keys, their attempts over three accounts, under two pools, with a decision per request, a `pin_migrated` event
/// per ten requests, a captured body per ten requests with its expiry spread as the arrivals are, a guardrail row per
/// hundred, and a day of quota and extra-usage samples for the three accounts.
fn seed(db: &LedgerDb) {
    // Parsed once, since `insert_request` parses the shipped list on every call and the seed lands 2,000 requests.
    let prices = crate::test_tables::api_prices();
    let template = decision();
    let mut decisions = Vec::new();
    for i in 0..REQUESTS {
        let id = format!("r{i:04}");
        let received_at = FROM + i * 40_000;
        let key = if i < 1_990 {
            "main".to_string()
        } else {
            format!("k{}", i - 1_990)
        };
        let session = format!("s{:03}", i / 5);
        let account = ACCOUNTS[(i % 3) as usize];
        let pool = POOLS[(i % 2) as usize];
        let captured = i % 10 == 0;
        let request = RequestRecord {
            id: id.clone(),
            received_at,
            finished_at: Some(received_at + 1_000),
            key_id: Some(key.clone()),
            key_generation: KEY_GENERATION,
            pool_id: Some(pool.into()),
            session_id: Some(session.clone()),
            agent_id: Some("main".into()),
            model: MODEL.into(),
            canonical_model: MODEL.into(),
            stream: true,
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            bodies_captured: captured,
            guardrails: if i % 100 == 0 {
                vec![GuardrailMatchRecord {
                    guardrail_id: "g1".into(),
                    position: 0,
                    blocked: true,
                }]
            } else {
                Vec::new()
            },
            ..RequestRecord::default()
        };
        let attempt = AttemptRecord {
            request_id: id.clone(),
            attempt: 1,
            account_id: account.into(),
            account_kind: AccountKind::Oauth,
            started_at: received_at + 10,
            finished_at: Some(received_at + 900),
            upstream_status: Some(200),
            outcome: "succeeded".into(),
            input_tokens: Some(100),
            output_tokens: Some(10),
            ..AttemptRecord::default()
        };
        let body = captured.then(|| BodyRecord {
            request_id: id.clone(),
            request_body: b"{}".to_vec(),
            response_body: Some(b"{}".to_vec()),
            request_len_seen: 2,
            response_len_seen: 2,
            expires_at: received_at + 7 * DAY,
        });
        db.insert_request_at(&request, &[attempt], body.as_ref(), &prices)
            .unwrap();
        if i % 10 == 0 {
            db.insert_event(&EventRecord {
                at: received_at + 1,
                kind: "pin_migrated".into(),
                account_id: Some(account.into()),
                key_id: Some(key.clone()),
                session_id: Some(session.clone()),
                pool_id: Some(pool.into()),
                request_id: Some(id.clone()),
                ..EventRecord::default()
            })
            .unwrap();
        }
        decisions.push(DecisionRow {
            request_id: id,
            seq: 1,
            attempt: 1,
            at: received_at,
            kind: "placed".into(),
            probe: false,
            key_id: Some(key),
            pool_id: Some(pool.into()),
            account_id: Some(account.into()),
            from_account_id: None,
            session_id: Some(session),
            agent_id: Some("main".into()),
            model: Some(MODEL.into()),
            ..template.clone()
        });
    }
    db.write(|conn| {
        let tx = begin_write(conn)?;
        for row in &decisions {
            insert_decision_in(&tx, row, "{}", &[])?;
        }
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    for account in ACCOUNTS {
        for bucket in (FROM / 1_000..TO / 1_000).step_by(300) {
            let quota = ["five_hour", "seven_day"].map(|window| QuotaSampleRecord {
                account_id: account.into(),
                bucket,
                window: window.into(),
                utilization: Some(0.5),
                reset_at: Some(bucket + 3_600),
                source: SampleSource::Poll,
                observed_at: Some(bucket * 1_000),
            });
            let extra = ExtraUsageSampleRecord {
                account_id: account.into(),
                bucket,
                is_enabled: true,
                utilization: Some(0.1),
                source: SampleSource::Poll,
                ..ExtraUsageSampleRecord::default()
            };
            db.insert_samples(&quota, Some(&extra)).unwrap();
        }
    }
}

/// A decision row whose record and summary the seed's rows share. The plans read the columns alone, which each seeded
/// row sets for itself.
fn decision() -> DecisionRow {
    use phoebis_routing::{
        ClientClass, DecisionRecord, RefusalFacts, RefusedRequest, RegistrySnapshot,
    };
    let snapshot =
        RegistrySnapshot::from_load(phoebis_store::RegistryLoad::default(), [0; 32], 0, 1);
    let (record, summary) = DecisionRecord::refused(
        &RefusedRequest {
            request_id: "r".into(),
            attempt: 1,
            seq: 1,
            at: FROM,
            key_id: "main".into(),
            key_generation: KEY_GENERATION,
            agent_id: "main".into(),
            parent_agent_id: None,
            session_id: None,
            request_class: None,
            canonical_model: MODEL.into(),
            lane: Default::default(),
            flags: Default::default(),
            chain: Default::default(),
            attempted: Vec::new(),
            holds: Vec::new(),
            client: ClientClass::ClaudeCode,
            client_version: None,
        },
        &snapshot,
        &RefusalFacts {
            kind: "key",
            code: "unknown",
            reason: "the key is unknown".into(),
        },
    );
    DecisionRow::of(summary, record, FROM)
}

/// `PRAGMA analysis_limit` at `ANALYSIS_LIMIT` and `ANALYZE` on the purger connection, the open's limit. Before it
/// returns it proves the readers see the new rows through two control plans of `CONTROL_SQL`: planted statistics that
/// favor the key index, explained through a reader, then the analysis, then the same statement explained again, and
/// the two plans differ, the second naming `requests_session`. Each control follows a bump of the pool's statistics
/// generation through `debug_bump_statistics_generation`: a pooled reader loads its schema and `sqlite_stat1` at its
/// open, where the `cache_size` pragma loads the schema, and a re-analysis of a table that has statistics moves no
/// schema cookie, so without the bump a control would plan on the statistics its reader loaded at its open. The tests
/// below need no bump, since each plans on a fresh ledger over a copy of the analyzed file, whose readers load the
/// seeded statistics at their open.
fn analyzed(db: &LedgerDb) {
    db.purge(|c| {
        c.execute("DELETE FROM sqlite_stat1", [])?;
        c.execute(
            "INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES \
             ('requests', 'requests_key', '2000 1 1'), ('requests', 'requests_session', '2000 1000 1')",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    db.debug_bump_statistics_generation();
    let planted = plan(db, CONTROL_SQL, params!["main", "s007"]);
    assert!(
        seeks(&planted, "requests_key", &["key_id=?"]),
        "the planted statistics reach a pooled reader: {planted:?}"
    );
    db.purge(|c| {
        c.pragma_update(None, "analysis_limit", ANALYSIS_LIMIT)?;
        c.execute_batch("ANALYZE")?;
        Ok(())
    })
    .unwrap();
    db.debug_bump_statistics_generation();
    let analyzed = plan(db, CONTROL_SQL, params!["main", "s007"]);
    assert!(
        seeks(&analyzed, "requests_session", &["session_id=?"]) && analyzed != planted,
        "the seeded statistics reach a pooled reader: {planted:?} then {analyzed:?}"
    );
}

/// The two-index lookup the statistics tests of `db.rs` plan, spelled again here since that module's tests are
/// private to it. Under the planted rows the key index looks selective and the planner takes it, and under the seeded
/// skew (one key holding nearly every row, five rows per session) it takes the session index. The planted rows make
/// the first plan independent of how the planner breaks a tie between two indexes that look alike without
/// statistics.
const CONTROL_SQL: &str =
    "SELECT id FROM requests WHERE key_id = ?1 AND session_id = ?2 ORDER BY received_at LIMIT 1";

/// The plan through a pooled reader, which plans on the statistics it loaded at its open. A test's reader opened on the
/// analyzed copy, and `analyzed` bumps the generation before each control, so `lend` reopens the reader first.
fn plan(db: &LedgerDb, sql: &str, params: impl rusqlite::Params) -> Vec<String> {
    db.read_blocking(|c| {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let steps = stmt
            .query_map(params, |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(steps)
    })
    .unwrap()
}

/// Whether a step searches through `index` binding every one of `terms` as the plan prints them. A covering seek
/// prints `USING COVERING INDEX`, so the index is matched as the word after `INDEX` wherever it sits.
fn seeks(plan: &[String], index: &str, terms: &[&str]) -> bool {
    plan.iter().any(|step| {
        let words: Vec<&str> = step.split_whitespace().collect();
        step.starts_with("SEARCH ")
            && words.windows(2).any(|pair| pair == ["INDEX", index])
            && terms.iter().all(|term| step.contains(term))
    })
}

/// Whether a step walks a whole table under any of `names`, the tables and the aliases the statement spells.
fn walks(plan: &[String], names: &[&str]) -> bool {
    plan.iter().any(|step| {
        names.iter().any(|name| {
            *step == format!("SCAN {name}") || step.starts_with(&format!("SCAN {name} "))
        })
    })
}

#[test]
fn the_session_attempts_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let (sql, binds) = session_attempts_statement(FROM, TO, &["s007".into(), "s008".into()]);
    let steps = plan(&db, &sql, params_from_iter(binds));
    assert!(
        seeks(
            &steps,
            "requests_session",
            &["session_id=?", "received_at>?", "received_at<?"]
        ) && seeks(&steps, "sqlite_autoindex_attempts_1", &["request_id=?"])
            && !walks(&steps, &["attempts", "a", "requests", "r"]),
        "{steps:?}"
    );
}

#[test]
fn the_newest_events_of_a_kind_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let steps = plan(&db, RECENT_EVENTS_OF_KIND, params!["pin_migrated", 50]);
    assert!(
        seeks(&steps, "events_kind_at", &["kind=?"]) && !walks(&steps, &["events"]),
        "{steps:?}"
    );
}

/// A dry run's sample page for `subject` under `base`, on the first page and on a page past its cursor, composed as
/// `guardrail_sample_page` composes them.
fn sample_pages(db: &LedgerDb, base: &str, subject: &str) -> [Vec<String>; 2] {
    [
        plan(
            db,
            &format!("{base}{GUARDRAIL_SAMPLE_ORDER_SQL}3"),
            params![subject, FROM, 51],
        ),
        plan(
            db,
            &format!("{base}{GUARDRAIL_SAMPLE_CURSOR_SQL}{GUARDRAIL_SAMPLE_ORDER_SQL}5"),
            params![subject, FROM, TO, "r1000", 51],
        ),
    ]
}

#[test]
fn the_pool_sample_page_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    for steps in sample_pages(&db, GUARDRAIL_SAMPLE_POOL_SQL, "p1") {
        assert!(
            seeks(&steps, "requests_received", &["received_at>?"])
                && !walks(&steps, &["requests", "r"]),
            "{steps:?}"
        );
    }
}

#[test]
fn the_key_sample_page_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    for steps in sample_pages(&db, GUARDRAIL_SAMPLE_KEY_SQL, "main") {
        assert!(
            seeks(&steps, "requests_key", &["key_id=?"]) && !walks(&steps, &["requests", "r"]),
            "{steps:?}"
        );
    }
}

/// The hours of the seeded day in seconds, as the rollup reads bind them.
const FROM_HOUR: i64 = FROM / 1_000;
const TO_HOUR: i64 = TO / 1_000;

#[test]
fn the_pool_and_account_rollup_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let select = RollupSelect {
        pools: vec!["p1".into()],
        accounts: vec!["a1".into(), "a2".into()],
        ..RollupSelect::default()
    };
    let (sql, binds) = select_statement(FROM_HOUR, TO_HOUR, &select);
    let steps = plan(&db, &sql, params_from_iter(binds));
    assert!(
        seeks(
            &steps,
            "rollups_hourly_member",
            &["pool_id=?", "account_id=?"]
        ) && !walks(&steps, &["rollups_hourly"]),
        "{steps:?}"
    );
}

#[test]
fn the_pool_only_rollup_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let select = RollupSelect {
        pools: vec!["p1".into()],
        ..RollupSelect::default()
    };
    let (sql, binds) = select_statement(FROM_HOUR, TO_HOUR, &select);
    let steps = plan(&db, &sql, params_from_iter(binds));
    assert!(
        seeks(
            &steps,
            "sqlite_autoindex_rollups_hourly_1",
            &["hour>?", "hour<?"]
        ) && !walks(&steps, &["rollups_hourly"]),
        "{steps:?}"
    );
}

#[test]
fn the_key_range_rollup_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let steps = plan(
        &db,
        &key_range_statement(),
        params!["main", KEY_GENERATION, FROM_HOUR, TO_HOUR],
    );
    assert!(
        seeks(&steps, "rollups_hourly_key", &["key_id=?"]) && !walks(&steps, &["rollups_hourly"]),
        "{steps:?}"
    );
}

/// The purge's cutoff and slice size.
const PURGE_BINDS: (i64, i64) = (TO, 1_000);

#[test]
fn the_bodies_purge_slice_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    for sql in [PURGE_BODIES, EXPIRED_BODY_BYTES] {
        let steps = plan(&db, sql, PURGE_BINDS);
        assert!(
            seeks(&steps, "bodies_expires", &["expires_at<?"])
                && !walks(&steps, &["bodies", "b", "requests", "r"]),
            "{sql}: {steps:?}"
        );
    }
}

#[test]
fn the_request_family_purge_slices_keep_their_plans_under_statistics() {
    let (_dir, db) = seeded();
    for sql in [PURGE_ATTEMPTS, PURGE_REQUEST_GUARDRAILS, PURGE_REQUESTS] {
        let steps = plan(&db, sql, PURGE_BINDS);
        assert!(
            seeks(&steps, "requests_received", &["received_at<?"])
                && !walks(&steps, &["attempts", "request_guardrails", "requests"]),
            "{sql}: {steps:?}"
        );
    }
}

#[test]
fn the_decisions_purge_slice_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let steps = plan(&db, PURGE_DECISIONS, PURGE_BINDS);
    assert!(
        seeks(&steps, "decisions_at", &["at<?"]) && !walks(&steps, &["decisions"]),
        "{steps:?}"
    );
}

#[test]
fn the_guardrail_counts_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    for steps in [
        plan(&db, GUARDRAIL_COUNTS_SQL, params![FROM, TO]),
        plan(&db, GUARDRAIL_BLOCKED_BYTES_SQL, params!["g1", FROM, TO]),
    ] {
        assert!(
            seeks(&steps, "request_guardrails_received", &["received_at>?"])
                && !walks(&steps, &["request_guardrails", "g", "requests", "r"]),
            "{steps:?}"
        );
    }
}

#[test]
fn the_quota_sample_reads_keep_their_plans_under_statistics() {
    let (_dir, db) = seeded();
    let since = FROM / 1_000;
    for steps in [
        plan(&db, LATEST_QUOTA_SQL, params![since]),
        plan(
            &db,
            HOURLY_WINDOW_SAMPLES_SQL,
            params![since, TO / 1_000, since, TO / 1_000],
        ),
    ] {
        assert!(
            seeks(
                &steps,
                "quota_samples_account",
                &["account_id=?", "window=?", "bucket>?"]
            ) && !walks(&steps, &["quota_samples", "q", "s"]),
            "{steps:?}"
        );
    }
}

#[test]
fn the_request_list_by_key_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let filter = RequestFilter {
        keys: vec!["main".into()],
        ..RequestFilter::default()
    };
    let live = LiveKeys(BTreeMap::from([("main".into(), KEY_GENERATION)]));
    let (sql, args) = list_requests_statement(&filter, &live, None, 50).unwrap();
    let steps = plan(&db, &sql, params_from_iter(args.iter().map(|a| a.as_ref())));
    assert!(
        seeks(&steps, "requests_key", &["key_id=?"]) && !walks(&steps, &["requests"]),
        "{steps:?}"
    );
}

#[test]
fn the_request_list_by_session_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let filter = RequestFilter {
        session: Some("s007".into()),
        ..RequestFilter::default()
    };
    let (sql, args) = list_requests_statement(&filter, &LiveKeys::default(), None, 50).unwrap();
    let steps = plan(&db, &sql, params_from_iter(args.iter().map(|a| a.as_ref())));
    assert!(
        seeks(&steps, "requests_session", &["session_id=?"]) && !walks(&steps, &["requests"]),
        "{steps:?}"
    );
}

/// Either index serves the session's decisions: the session's own, or the range's where the `LIKE` arm of the session
/// filter's `OR` keeps the session index off.
#[test]
fn the_routing_log_by_session_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let filter = RoutingLogFilter {
        from: FROM,
        to: TO,
        session: Some("s007".into()),
        ..RoutingLogFilter::default()
    };
    let statement = decision_statement(&filter).expect("no kind is named, so decisions are read");
    let (sql, args) = decision_rows_statement(statement, None, 51);
    let steps = plan(&db, &sql, params_from_iter(args.iter().map(|a| a.as_ref())));
    assert!(
        (seeks(&steps, "decisions_at", &["at>?"])
            || seeks(&steps, "decisions_session", &["session_id=?"]))
            && !walks(
                &steps,
                &["attempts", "a", "x", "decisions", "d", "n", "requests", "r"]
            ),
        "{steps:?}"
    );
}

#[test]
fn the_landing_stamp_read_keeps_its_plan_under_statistics() {
    let (_dir, db) = seeded();
    let steps = plan(&db, LANDING_STAMP_SQL, []);
    assert!(
        seeks(&steps, "requests_landed_at", &[]) && !walks(&steps, &["requests"]),
        "{steps:?}"
    );
}
