//! The codes that flag a request row as refused, and the ledger's reading of a row against them. Each code names a
//! refusal the router issues before it asks any account, for the client's or a policy's cause.
//!
//! The codes that stay in the figures and off the list are `account_cancelled`, `accounts_unavailable`,
//! `body_budget_exhausted`, `key_body_reads`, `ledger_behind` and `router_unavailable`, since the router's own standing
//! caused them, and `accounts_exhausted`, `model_rejected` and every upstream code, since an account was asked.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::db::{begin_write, row_to_request, LedgerDb, REQUEST_SELECT};
use crate::pricing::PriceRevisions;
use crate::record::RequestRecord;
use crate::refold::{fail_after_page, page_size, read_meta, write_meta};
use crate::rollup::{
    delete_rollup, deltas_of, empty_row, read_rollup, read_rollups, write_rollup, KeySelector,
    RollupRow, RollupSelect,
};
use crate::LedgerError;

const DAY_MS: i64 = 86_400_000;

/// The `meta` key holding the mark `{"done_at": <ms>}` once the stored refusals were moved into the map.
pub const REFUSALS_BACKFILL_KEY: &str = "refusals_backfill";
/// The `meta` key holding a pass in progress, `{"received_at": <ms>, "id": "<request id>"}`, the last request the
/// last committed page moved, written by every page's transaction and deleted by the last page's.
pub const REFUSALS_BACKFILL_CURSOR_KEY: &str = "refusals_backfill_cursor";

/// One token list feeds the slice and the step's `IN (..)` text, so `is_refusal` and the step's backfill cannot
/// name different codes.
macro_rules! refusal_codes {
    ($first:literal $(, $rest:literal)* $(,)?) => {
        /// The codes that flag a rejected request with no attempt as refused, alphabetical, the same list the step's
        /// backfill SQL is generated from.
        pub const REFUSAL_CODES: &[&str] = &[$first $(, $rest)*];
        pub(crate) const REFUSED_STEP_SQL: &str = concat!(
            "ALTER TABLE requests ADD COLUMN refused INTEGER NOT NULL DEFAULT 0;\n",
            "ALTER TABLE rollups_hourly ADD COLUMN refusals_json TEXT NOT NULL DEFAULT '{}';\n",
            "UPDATE requests SET refused = 1 WHERE kind <> 'probe' AND outcome = 'rejected' AND error_code IN (",
            "'", $first, "'" $(, ", '", $rest, "'")*,
            ") AND requests.attempts = 0",
            " AND NOT EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id);"
        );
    };
}
refusal_codes!(
    "auth_ambiguous",
    "auth_malformed",
    "auth_missing",
    "auth_unknown",
    "body_shape",
    "client_contract",
    "client_drift",
    "client_not_allowed",
    "guardrail_blocked",
    "invalid_body",
    "invalid_model",
    "method_not_allowed",
    "missing_model",
    "model_not_allowed",
    "not_found",
    "request_incomplete",
    "request_timeout",
    "request_too_large",
    "unsupported_content_encoding",
);

/// The ledger's reading of a row's own facts: a kind other than `probe`, the outcome `rejected`, a code of
/// `REFUSAL_CODES` and an attempt count of zero, the count the row stores. A `client_not_allowed`,
/// `model_not_allowed` or `auth_unknown` answered at a reselect after an attempt stays a rejected request, also where
/// the bus lost that attempt's selection event and the trail holds no row, and a probe the registry refused stays on
/// its probe row.
pub fn is_refusal(kind: &str, outcome: &str, error_code: Option<&str>, attempts: u32) -> bool {
    kind != "probe"
        && outcome == "rejected"
        && attempts == 0
        && error_code.is_some_and(|code| REFUSAL_CODES.contains(&code))
}

#[derive(Debug, Serialize, Deserialize)]
struct Mark {
    done_at: i64,
}

/// The last request a committed page moved, in the pass's `(received_at, id)` order.
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    received_at: i64,
    id: String,
}

/// Moves each flagged request's old contribution (one request, its rejected, its uncached request, its errors entry
/// and its duration bucket) out of the hour's row that holds it and its refusals entry in, over the rows the
/// retention still holds, in pages of `page_size()` with a cursor, once. Answers how many requests moved: 0 when the
/// mark stands.
///
/// It runs at start, after the step flagged the stored rows and before the ledger task lands anything, so every
/// flagged row a page meets was folded before the flag existed. A row landed later folds under the map at once and
/// meets no page, since the mark stands by then. The cursor is the refold's rule: a start that fails after a page
/// committed resumes after that page rather than taking its contribution out a second time.
pub fn refold_refusals(
    db: &LedgerDb,
    prices: &PriceRevisions,
    retention_days: u32,
    now: i64,
) -> Result<usize, LedgerError> {
    let (mark, mut after) = db.write(|c| {
        Ok((
            read_meta::<Mark>(c, REFUSALS_BACKFILL_KEY)?,
            read_meta::<Cursor>(c, REFUSALS_BACKFILL_CURSOR_KEY)?,
        ))
    })?;
    if mark.is_some() {
        return Ok(0);
    }
    // The retention purge's own arithmetic: a zero keeps the requests for good, so nothing is cut off.
    let from = if retention_days == 0 {
        i64::MIN
    } else {
        now - i64::from(retention_days) * DAY_MS
    };
    let page = page_size();
    let mut moved = 0;
    let mut pages = 0;
    loop {
        let done = db.write(|c| {
            let tx = begin_write(c)?;
            let requests = refused_after(&tx, from, after.as_ref(), page)?;
            for request in &requests {
                // The fold before the flag booked the refusal as a rejected request, so that is what comes out.
                let before = RequestRecord {
                    refused: false,
                    ..request.clone()
                };
                for delta in deltas_of(&before, &[], prices) {
                    unfold_from_holding_row(&tx, request, &delta)?;
                }
                for delta in deltas_of(request, &[], prices) {
                    let mut row = read_rollup(&tx, &delta)?.unwrap_or_else(|| empty_row(&delta));
                    row.add(&delta);
                    write_rollup(&tx, &row)?;
                }
                moved += 1;
            }
            let last = requests.len() < page;
            if last {
                tx.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    params![REFUSALS_BACKFILL_CURSOR_KEY],
                )?;
                write_meta(&tx, REFUSALS_BACKFILL_KEY, &Mark { done_at: now })?;
            } else if let Some(request) = requests.last() {
                let next = Cursor {
                    received_at: request.received_at,
                    id: request.id.clone(),
                };
                write_meta(&tx, REFUSALS_BACKFILL_CURSOR_KEY, &next)?;
                after = Some(next);
            }
            tx.commit()?;
            Ok(last)
        })?;
        pages += 1;
        fail_after_page(pages)?;
        if done {
            break;
        }
    }
    tracing::info!(
        requests = moved,
        pages,
        "moved the stored refusals out of their hours' request figures into the refusals map"
    );
    Ok(moved)
}

/// Takes a stored refusal's old `delta` out of the row that holds it, with `unfold`'s arithmetic. The row is found
/// rather than keyed by today's fold, since the build that stored it may have keyed it otherwise: the raw
/// `User-Agent`, client and class before those dimensions were folded, and `standard` on every row stored before the
/// speed joined the key, where today's fold keys a refusal that asked for `fast` under `fast`. Within one hour every
/// row is of one build's fold, so today's exact key wins wherever it holds the refusal. Where no row holds it, the
/// pass books the refusal in the map anyway and names the request in the log.
pub(crate) fn unfold_from_holding_row(
    conn: &Connection,
    request: &RequestRecord,
    delta: &RollupRow,
) -> Result<(), LedgerError> {
    let code = request.error_code.as_deref().unwrap_or_default();
    let select = RollupSelect {
        pools: vec![delta.pool_id.clone()],
        accounts: vec![delta.account_id.clone()],
        kinds: vec![delta.kind.clone()],
        models: vec![delta.canonical_model.clone()],
        keys: vec![KeySelector {
            key_id: delta.key_id.clone(),
            key_generation: None,
        }],
    };
    let mut holding: Vec<RollupRow> = read_rollups(conn, delta.hour, delta.hour + 3_600, &select)?
        .into_iter()
        .filter(|row| row.rejected > 0 && row.errors.get(code).is_some_and(|n| *n > 0))
        .collect();
    // `empty_row` keeps a row's key alone, so two of them are equal exactly when the keys are.
    let at = holding
        .iter()
        .position(|row| empty_row(row) == empty_row(delta))
        .or_else(|| {
            holding
                .iter()
                .position(|row| row.key_generation == delta.key_generation)
        })
        .or((!holding.is_empty()).then_some(0));
    let Some(at) = at else {
        tracing::warn!(
            request = %request.id,
            hour = delta.hour,
            "no hourly row holds the stored refusal's figures, so they stay where the older fold booked them"
        );
        return Ok(());
    };
    let mut row = holding.swap_remove(at);
    row.subtract(delta);
    if row.holds_nothing() {
        delete_rollup(conn, &row)
    } else {
        write_rollup(conn, &row)
    }
}

/// The next page of flagged requests received at or after `from`, past `after`, in `(received_at, id)` order. A
/// flagged row has no attempt row, since the flag requires none, so the page reads none.
fn refused_after(
    conn: &Connection,
    from: i64,
    after: Option<&Cursor>,
    page: usize,
) -> Result<Vec<RequestRecord>, LedgerError> {
    let (after_at, after_id) = after.map_or((i64::MIN, ""), |a| (a.received_at, a.id.as_str()));
    let mut stmt = conn.prepare(&format!(
        "{REQUEST_SELECT} WHERE refused = 1 AND received_at >= ?1 AND (received_at, id) > (?2, ?3) ORDER BY received_at, id LIMIT ?4"
    ))?;
    let requests = stmt
        .query_map(
            params![from, after_at, after_id, page as i64],
            row_to_request,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(requests)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::record::AttemptRecord;
    use crate::refold::{FAIL_AFTER_PAGES, TEST_PAGE};
    use crate::rollup::{read_rollups, RollupRow, RollupSelect};
    use crate::test_tables::{api_prices, JANUARY_2026};

    const HAIKU: &str = "claude-haiku-5-5";

    #[test]
    fn is_refusal_flags_a_rejected_client_request_with_a_listed_code_and_no_attempt_and_nothing_else(
    ) {
        let cases: [(&str, &str, Option<&str>, u32, bool); 7] = [
            ("inference", "rejected", Some("guardrail_blocked"), 0, true),
            ("count_tokens", "rejected", Some("auth_missing"), 0, true),
            ("probe", "rejected", Some("auth_unknown"), 0, false),
            (
                "inference",
                "rejected",
                Some("client_not_allowed"),
                1,
                false,
            ),
            (
                "inference",
                "rejected",
                Some("accounts_unavailable"),
                0,
                false,
            ),
            ("inference", "failed", Some("guardrail_blocked"), 0, false),
            ("inference", "rejected", None, 0, false),
        ];
        for (kind, outcome, code, attempts, want) in cases {
            assert_eq!(
                is_refusal(kind, outcome, code, attempts),
                want,
                "a {kind} request {outcome} with {code:?} after {attempts} attempts"
            );
        }
    }

    #[test]
    fn the_steps_backfill_names_every_refusal_code_and_no_other() {
        for code in REFUSAL_CODES {
            assert!(
                REFUSED_STEP_SQL.contains(&format!("'{code}'")),
                "the backfill names {code}: {REFUSED_STEP_SQL}"
            );
        }
        let (_, after) = REFUSED_STEP_SQL
            .split_once("IN (")
            .expect("the backfill lists its codes");
        let (list, _) = after.split_once(')').expect("the list closes");
        let quoted = list
            .split(", ")
            .filter(|item| item.len() > 2 && item.starts_with('\'') && item.ends_with('\''))
            .count();
        assert_eq!(
            (quoted, list.split(", ").count()),
            (REFUSAL_CODES.len(), REFUSAL_CODES.len()),
            "one quoted item per code and nothing else: {list}"
        );
        assert!(
            REFUSAL_CODES.windows(2).all(|pair| pair[0] < pair[1]),
            "the list is strictly ascending: {REFUSAL_CODES:?}"
        );
        assert_eq!(REFUSAL_CODES.len(), 19, "the rule names nineteen codes");
        assert!(
            REFUSED_STEP_SQL.starts_with(
                "ALTER TABLE requests ADD COLUMN refused INTEGER NOT NULL DEFAULT 0;\n\
                 ALTER TABLE rollups_hourly ADD COLUMN refusals_json TEXT NOT NULL DEFAULT '{}';\n"
            ),
            "the step adds both columns first: {REFUSED_STEP_SQL}"
        );
        for clause in [
            "kind <> 'probe'",
            "requests.attempts = 0",
            "NOT EXISTS (SELECT 1 FROM attempts",
        ] {
            assert!(
                REFUSED_STEP_SQL.contains(clause),
                "the backfill carries {clause}: {REFUSED_STEP_SQL}"
            );
        }
    }

    /// A request the router refused before any account, as a ledger from before the flag landed it: unflagged, so
    /// the fold booked it as a rejected request.
    fn refusal(id: &str, received_at: i64) -> RequestRecord {
        RequestRecord {
            id: id.into(),
            received_at,
            finished_at: Some(received_at + 4),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            model: HAIKU.into(),
            canonical_model: HAIKU.into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(403),
            outcome: "rejected".into(),
            duration_ms: Some(4),
            error_code: Some("guardrail_blocked".into()),
            ..Default::default()
        }
    }

    /// A request one attempt served with 100 uncached input tokens.
    fn served(id: &str, received_at: i64) -> (RequestRecord, Vec<AttemptRecord>) {
        (
            RequestRecord {
                id: id.into(),
                received_at,
                finished_at: Some(received_at + 900),
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                model: HAIKU.into(),
                canonical_model: HAIKU.into(),
                kind: "inference".into(),
                origin: "client".into(),
                status: Some(200),
                outcome: "succeeded".into(),
                attempts: 1,
                duration_ms: Some(900),
                ..Default::default()
            },
            vec![AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                started_at: received_at,
                outcome: "succeeded".into(),
                input_tokens: Some(100),
                output_tokens: Some(10),
                ..Default::default()
            }],
        )
    }

    fn land(db: &LedgerDb, request: &RequestRecord, attempts: &[AttemptRecord]) {
        db.insert_request_at(request, attempts, None, &api_prices())
            .unwrap();
    }

    /// Sets the flag on the named rows alone, as the step's backfill does on the rows it matches.
    fn flag(db: &LedgerDb, ids: &[&str]) {
        db.write(|c| {
            let marks = vec!["?"; ids.len()].join(", ");
            c.execute(
                &format!("UPDATE requests SET refused = 1 WHERE id IN ({marks})"),
                rusqlite::params_from_iter(ids),
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn rows(db: &LedgerDb) -> Vec<RollupRow> {
        db.read_blocking(|c| read_rollups(c, i64::MIN, i64::MAX, &RollupSelect::default()))
            .unwrap()
    }

    fn meta(db: &LedgerDb, key: &str) -> Option<serde_json::Value> {
        db.read_blocking(|c| {
            Ok(rusqlite::OptionalExtension::optional(c.query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            ))?)
        })
        .unwrap()
        .map(|text| serde_json::from_str(&text).expect("the meta value is JSON"))
    }

    /// Three refusals folded under the old arithmetic beside a served request: the pass takes each one's request,
    /// rejected, uncached request, errors entry and duration bucket out of the hour and books it in the map, the
    /// served request keeps its figures, and the mark makes a second pass move nothing.
    #[test]
    fn the_pass_moves_each_flagged_requests_old_contribution_into_the_map_once() {
        let db = LedgerDb::open_in_memory().unwrap();
        for (n, id) in ["r1", "r2", "r3"].into_iter().enumerate() {
            land(&db, &refusal(id, JANUARY_2026 + n as i64), &[]);
        }
        let (request, attempts) = served("s1", JANUARY_2026 + 10);
        land(&db, &request, &attempts);
        flag(&db, &["r1", "r2", "r3"]);
        let sum = |rows: &[RollupRow], figure: fn(&RollupRow) -> u64| -> u64 {
            rows.iter().map(figure).sum()
        };
        let before = rows(&db);
        assert_eq!(
            (sum(&before, |r| r.requests), sum(&before, |r| r.rejected)),
            (4, 3),
            "the old arithmetic booked the refusals as rejected requests"
        );
        let now = JANUARY_2026 + DAY_MS;
        assert_eq!(refold_refusals(&db, &api_prices(), 0, now).unwrap(), 3);
        let after = rows(&db);
        assert_eq!(
            (
                sum(&after, |r| r.requests),
                sum(&after, |r| r.rejected),
                sum(&after, |r| r.uncached_requests),
                sum(&after, |r| r.duration_hist.count()),
            ),
            (1, 0, 1, 1),
            "the served request's figures alone"
        );
        assert!(
            after
                .iter()
                .all(|r| !r.errors.contains_key("guardrail_blocked")),
            "no errors entry for the code: {after:?}"
        );
        let refusals: Vec<_> = after
            .iter()
            .filter(|r| !r.refusals.is_empty())
            .map(|r| (r.requests, r.refusals.clone()))
            .collect();
        assert_eq!(
            refusals,
            [(0, BTreeMap::from([("guardrail_blocked".to_string(), 3)]))],
            "one row holds the three refusals in the map alone"
        );
        assert_eq!(
            meta(&db, REFUSALS_BACKFILL_KEY).expect("the mark")["done_at"].as_i64(),
            Some(now)
        );
        assert_eq!(meta(&db, REFUSALS_BACKFILL_CURSOR_KEY), None);
        assert_eq!(
            refold_refusals(&db, &api_prices(), 0, now + DAY_MS).unwrap(),
            0,
            "the mark stands"
        );
        assert_eq!(rows(&db), after, "the second pass moved nothing");
    }

    /// A pass of three flagged requests at a page of one commits a cursor after each page, and a pass interrupted
    /// after a committed page resumes after its cursor: the rows then equal an uninterrupted pass's, so no
    /// contribution came out twice.
    #[test]
    fn the_pass_commits_a_cursor_after_each_page_and_a_resumed_pass_subtracts_no_contribution_twice(
    ) {
        let seed = |db: &LedgerDb| {
            for (n, id) in ["r1", "r2", "r3"].into_iter().enumerate() {
                land(db, &refusal(id, JANUARY_2026 + n as i64 * 1_000), &[]);
            }
            flag(db, &["r1", "r2", "r3"]);
        };
        let now = JANUARY_2026 + DAY_MS;
        let whole = LedgerDb::open_in_memory().unwrap();
        seed(&whole);
        assert_eq!(refold_refusals(&whole, &api_prices(), 0, now).unwrap(), 3);
        let interrupted = LedgerDb::open_in_memory().unwrap();
        seed(&interrupted);
        let cursor = |db: &LedgerDb| {
            meta(db, REFUSALS_BACKFILL_CURSOR_KEY).map(|c| {
                (
                    c["received_at"].as_i64(),
                    c["id"].as_str().map(String::from),
                )
            })
        };
        TEST_PAGE.set(Some(1));
        FAIL_AFTER_PAGES.set(Some(1));
        let failed = refold_refusals(&interrupted, &api_prices(), 0, now);
        FAIL_AFTER_PAGES.set(None);
        assert!(failed.is_err(), "the injected failure after the first page");
        assert_eq!(
            cursor(&interrupted),
            Some((Some(JANUARY_2026), Some("r1".to_string()))),
            "the cursor after page 1"
        );
        assert_eq!(
            meta(&interrupted, REFUSALS_BACKFILL_KEY),
            None,
            "no mark yet"
        );
        FAIL_AFTER_PAGES.set(Some(1));
        let failed = refold_refusals(&interrupted, &api_prices(), 0, now);
        FAIL_AFTER_PAGES.set(None);
        assert!(
            failed.is_err(),
            "the injected failure after the resumed pass's first page"
        );
        assert_eq!(
            cursor(&interrupted),
            Some((Some(JANUARY_2026 + 1_000), Some("r2".to_string()))),
            "the cursor after page 2"
        );
        assert_eq!(
            refold_refusals(&interrupted, &api_prices(), 0, now).unwrap(),
            1,
            "the third pass moves the request after the cursor"
        );
        TEST_PAGE.set(None);
        assert_eq!(cursor(&interrupted), None, "the cursor is cleared");
        assert_eq!(
            meta(&interrupted, REFUSALS_BACKFILL_KEY).expect("the mark")["done_at"].as_i64(),
            Some(now)
        );
        assert_eq!(rows(&interrupted), rows(&whole));
    }

    /// Two refusals an older build booked in one row keyed by the raw `User-Agent` and `standard` speed: a pass
    /// interrupted after its first page took the first one's figures out of that row, and the resumed pass takes the
    /// second one's alone, so the row empties and today's key holds both in the map.
    #[test]
    fn a_resumed_pass_takes_each_refusal_once_out_of_the_row_an_older_fold_keyed_it_under() {
        const RAW_AGENT: &str = "claude-cli/2.1.302 (external, cli)";
        let db = LedgerDb::open_in_memory().unwrap();
        for (n, id) in ["r1", "r2"].into_iter().enumerate() {
            let request = RequestRecord {
                user_agent: Some(RAW_AGENT.into()),
                requested_speed: Some("fast".into()),
                ..refusal(id, JANUARY_2026 + n as i64)
            };
            land(&db, &request, &[]);
        }
        db.write(|c| {
            c.execute(
                "UPDATE rollups_hourly SET user_agent = ?1, speed = 'standard'",
                [RAW_AGENT],
            )?;
            Ok(())
        })
        .unwrap();
        flag(&db, &["r1", "r2"]);
        let older = |db: &LedgerDb| {
            rows(db)
                .into_iter()
                .find(|r| r.user_agent == RAW_AGENT)
                .map(|r| (r.requests, r.rejected, r.errors))
        };
        let blocked = |n: u64| BTreeMap::from([("guardrail_blocked".to_string(), n)]);
        let now = JANUARY_2026 + DAY_MS;
        TEST_PAGE.set(Some(1));
        FAIL_AFTER_PAGES.set(Some(1));
        let failed = refold_refusals(&db, &api_prices(), 0, now);
        FAIL_AFTER_PAGES.set(None);
        assert!(failed.is_err(), "the injected failure after the first page");
        assert_eq!(
            older(&db),
            Some((1, 1, blocked(1))),
            "the first page took the first refusal out of the row that holds it"
        );
        assert_eq!(
            refold_refusals(&db, &api_prices(), 0, now).unwrap(),
            1,
            "the resumed pass moves the second refusal alone"
        );
        TEST_PAGE.set(None);
        assert_eq!(older(&db), None, "the row held nothing else, so it went");
        assert_eq!(
            rows(&db)
                .into_iter()
                .map(|r| (r.user_agent, r.speed, r.requests, r.refusals))
                .collect::<Vec<_>>(),
            [(
                "claude-cli/2.1.302".to_string(),
                "fast".to_string(),
                0,
                blocked(2)
            )],
            "today's key holds both refusals in the map alone"
        );
        assert_eq!(
            meta(&db, REFUSALS_BACKFILL_KEY).expect("the mark")["done_at"].as_i64(),
            Some(now)
        );
        assert_eq!(meta(&db, REFUSALS_BACKFILL_CURSOR_KEY), None);
    }

    #[test]
    fn a_ledger_with_no_flagged_row_writes_the_mark_and_moves_nothing() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (request, attempts) = served("s1", JANUARY_2026);
        land(&db, &request, &attempts);
        let before = rows(&db);
        let now = JANUARY_2026 + DAY_MS;
        assert_eq!(refold_refusals(&db, &api_prices(), 0, now).unwrap(), 0);
        assert_eq!(
            meta(&db, REFUSALS_BACKFILL_KEY).expect("the mark")["done_at"].as_i64(),
            Some(now)
        );
        assert_eq!(meta(&db, REFUSALS_BACKFILL_CURSOR_KEY), None);
        assert_eq!(rows(&db), before);
    }
}
