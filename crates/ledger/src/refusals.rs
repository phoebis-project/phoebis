//! The codes that flag a request row as refused, and the ledger's reading of a row against them. Each code names a
//! refusal the router issues before it asks any account, for the client's or a policy's cause.
//!
//! The codes that stay in the figures and off the list are `account_canceled`, `accounts_unavailable`,
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
const DAY_S: i64 = 86_400;
const HOUR_S: i64 = 3_600;

/// The `meta` key holding the mark `{"done_at": <ms>}` once the stored refusals were moved into the map.
pub const REFUSALS_BACKFILL_KEY: &str = "refusals_backfill";
/// The `meta` key holding a pass in progress, `{"received_at": <ms>, "id": "<request id>"}`, the last request the
/// last committed page moved, written by every page's transaction and deleted by the last page's.
pub const REFUSALS_BACKFILL_CURSOR_KEY: &str = "refusals_backfill_cursor";

macro_rules! refusal_codes {
    ($first:literal $(, $rest:literal)* $(,)?) => {
        /// The codes that flag a rejected request with no attempt as refused, alphabetical.
        pub const REFUSAL_CODES: &[&str] = &[$first $(, $rest)*];
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
/// `REFUSAL_CODES` and an attempt count of zero, the count the row stores. An `auth_unknown` or a `model_not_allowed`
/// answered at a reselect after an attempt stays a rejected request, also where the bus lost that attempt's selection
/// event and the trail holds no row, and a probe the registry refused stays on its probe row.
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
///
/// A nonzero `retention_days` walks from the request cutoff, and a refusal received before the first hour boundary at
/// or after that cutoff moves only where a row of its hour still holds it: the rollup purge drops whole hours, so under
/// an equal rollup retention the hour that cutoff splits is gone, while under a rollup retention of 0 or a longer one
/// its rows stand. `retention_days` 0 keeps the requests for good, so the walk is bounded by the rollup retention
/// instead, the purge's own cutoff rounded up to the first hour the purge keeps. Either way a refusal whose hour the
/// rollup purge removed stays out of the map and writes no row into that hour. Both 0 walks from the start.
pub fn refold_refusals(
    db: &LedgerDb,
    prices: &PriceRevisions,
    retention_days: u32,
    rollup_retention_days: u32,
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
    // Each purge's own arithmetic, a zero keeping its rows for good. A nonzero request retention bounds the walk, and
    // since the configuration holds a nonzero rollup retention at least as long as the request one, the rollup purge
    // drops at most the hour the request cutoff splits. The first hour boundary at or after that cutoff guards each
    // refusal before it rather than bounding the walk, since that hour's rows stand under a rollup retention of 0 or a
    // longer one. Under a request retention of 0 the first hour the rollup purge keeps bounds the walk, since a
    // refusal's row names no account and no budget keeps it before that hour.
    let (from, guard) = match (retention_days, rollup_retention_days) {
        (0, 0) => (i64::MIN, None),
        (0, days) => (first_kept_hour(now, days), None),
        (days, _) => (
            now - i64::from(days) * DAY_MS,
            Some(first_kept_hour(now, days)),
        ),
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
                let was = deltas_of(&before, &[], prices);
                // The purge removes an hour whole, so a refusal whose rows are gone stays out of the hour rather than
                // writing a map-only row into it.
                if guard.is_some_and(|guard| request.received_at < guard)
                    && !rows_hold(&tx, request, &was)?
                {
                    continue;
                }
                for delta in &was {
                    unfold_from_holding_row(&tx, request, delta)?;
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

/// The first hour the rollup purge keeps under a retention of `days` at `now`, in milliseconds. The purge cuts in
/// seconds and drops every hour that starts before its cutoff.
fn first_kept_hour(now: i64, days: u32) -> i64 {
    let cutoff = now / 1_000 - i64::from(days) * DAY_S;
    let hour = cutoff.div_euclid(HOUR_S) * HOUR_S;
    let first_kept = if hour < cutoff { hour + HOUR_S } else { hour };
    first_kept * 1_000
}

/// Takes a stored refusal's old `delta` out of the row `holding_row` finds, with `unfold`'s arithmetic. Where no row
/// holds it, the pass books the refusal in the map anyway and names the request in the log.
pub(crate) fn unfold_from_holding_row(
    conn: &Connection,
    request: &RequestRecord,
    delta: &RollupRow,
) -> Result<(), LedgerError> {
    let Some(mut row) = holding_row(conn, request, delta)? else {
        tracing::warn!(
            request = %request.id,
            hour = %phoebis_store::utc_stamp(delta.hour * 1_000),
            "no hourly row holds the stored refusal's figures, so they stay where the older fold booked them"
        );
        return Ok(());
    };
    row.subtract(delta);
    if row.holds_nothing() {
        delete_rollup(conn, &row)
    } else {
        write_rollup(conn, &row)
    }
}

/// Whether a row of each delta's hour holds the stored refusal, as `holding_row` finds it.
fn rows_hold(
    conn: &Connection,
    request: &RequestRecord,
    deltas: &[RollupRow],
) -> Result<bool, LedgerError> {
    for delta in deltas {
        if holding_row(conn, request, delta)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The row that holds a stored refusal's old `delta`, `None` where no row of its hour does. The row is found rather
/// than keyed by today's fold, since the build that stored it may have keyed it otherwise: the raw `User-Agent`,
/// client and class before those dimensions were folded, and `standard` on every row stored before the speed joined
/// the key, where today's fold keys a refusal that asked for `fast` under `fast`. Within one hour every row is of one
/// build's fold, so today's exact key wins wherever it holds the refusal.
fn holding_row(
    conn: &Connection,
    request: &RequestRecord,
    delta: &RollupRow,
) -> Result<Option<RollupRow>, LedgerError> {
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
    Ok(at.map(|at| holding.swap_remove(at)))
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
        assert_eq!(refold_refusals(&db, &api_prices(), 0, 0, now).unwrap(), 3);
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
            refold_refusals(&db, &api_prices(), 0, 0, now + DAY_MS).unwrap(),
            0,
            "the mark stands"
        );
        assert_eq!(rows(&db), after, "the second pass moved nothing");
    }

    /// A request retention of 0 keeps the requests for good while a rollup retention of one day drops whole hours:
    /// the pass starts at the first hour the purge keeps, so a refusal two days old and one received after the
    /// purge's cutoff inside the hour the cutoff splits are not walked and write no row into their purged hours, while
    /// a refusal inside the kept hours moves into the map.
    #[test]
    fn under_a_zero_request_retention_the_pass_starts_at_the_first_hour_the_rollup_purge_keeps() {
        const HOUR_MS: i64 = 3_600_000;
        let db = LedgerDb::open_in_memory().unwrap();
        // The purge's cutoff, a day back from `now`, falls half an hour into `split_hour`.
        let split_hour = JANUARY_2026 + 2 * DAY_MS;
        let now = split_hour + DAY_MS + HOUR_MS / 2;
        let old = refusal("old", now - 2 * DAY_MS);
        let split = refusal("split", split_hour + 3 * HOUR_MS / 4);
        let kept = refusal("kept", split_hour + HOUR_MS + 60_000);
        for request in [&old, &split, &kept] {
            land(&db, request, &[]);
        }
        flag(&db, &["old", "split", "kept"]);
        let config = crate::LedgerConfig {
            retention_days: 0,
            rollup_retention_days: 1,
            ..crate::LedgerConfig::default()
        };
        crate::bodies::run_retention_blocking(&db, &config, now, &[]).unwrap();
        let kept_hour = crate::rollup::hour_of(kept.received_at);
        assert_eq!(
            rows(&db).iter().map(|r| r.hour).collect::<Vec<_>>(),
            [kept_hour],
            "the purge removed the two older hours and kept the last"
        );
        assert_eq!(
            refold_refusals(&db, &api_prices(), 0, 1, now).unwrap(),
            1,
            "the refusal in the kept hour alone"
        );
        assert_eq!(
            rows(&db)
                .into_iter()
                .map(|r| (r.hour, r.requests, r.refusals))
                .collect::<Vec<_>>(),
            [(
                kept_hour,
                0,
                BTreeMap::from([("guardrail_blocked".to_string(), 1)])
            )],
            "no row in a purged hour, and the kept hour's refusal in the map alone"
        );
    }

    /// The hour a seven-day cutoff back from the cases' `now` falls half an hour into.
    const SPLIT_HOUR: i64 = JANUARY_2026 + 13 * DAY_MS;

    /// A rollup row's hour, rejected count and refusals map.
    type HourFigures = (i64, u64, BTreeMap<String, u64>);

    /// The ledger under a request retention of seven days and `rollup_retention_days` as the purge at `now` leaves it,
    /// with a refusal received after the request cutoff inside `SPLIT_HOUR` and one inside the next hour. Answers the
    /// pass's count and the rows' figures by hour.
    fn refolded_past_a_seven_day_cutoff(rollup_retention_days: u32) -> (usize, Vec<HourFigures>) {
        const HOUR_MS: i64 = 3_600_000;
        let db = LedgerDb::open_in_memory().unwrap();
        let now = SPLIT_HOUR + 7 * DAY_MS + HOUR_MS / 2;
        let split = refusal("split", SPLIT_HOUR + 3 * HOUR_MS / 4);
        let kept = refusal("kept", SPLIT_HOUR + HOUR_MS + 60_000);
        for request in [&split, &kept] {
            land(&db, request, &[]);
        }
        flag(&db, &["split", "kept"]);
        let config = crate::LedgerConfig {
            retention_days: 7,
            rollup_retention_days,
            ..crate::LedgerConfig::default()
        };
        crate::bodies::run_retention_blocking(&db, &config, now, &[]).unwrap();
        let moved = refold_refusals(&db, &api_prices(), 7, rollup_retention_days, now).unwrap();
        let rows = rows(&db)
            .into_iter()
            .map(|r| (r.hour, r.rejected, r.refusals))
            .collect();
        (moved, rows)
    }

    /// Under equal request and rollup retentions the rollup purge drops the hour the request cutoff splits while the
    /// requests received after the cutoff stand. The guard finds no row of that hour holding the refusal received
    /// there, so it writes no row into the hour, and the one in the next hour moves into the map.
    #[test]
    fn under_equal_retentions_the_refusal_in_the_hour_the_rollup_purge_dropped_writes_no_row_into_it(
    ) {
        let split_hour = crate::rollup::hour_of(SPLIT_HOUR);
        let blocked = BTreeMap::from([("guardrail_blocked".to_string(), 1)]);
        assert_eq!(
            refolded_past_a_seven_day_cutoff(7),
            (1, vec![(split_hour + 3_600, 0, blocked)]),
            "the refusal in the next hour alone moved, and no row stands in the dropped hour"
        );
    }

    /// Both refusals moved into the map, the split hour's and the next hour's, each hour's rejected count back to 0.
    fn both_moved() -> (usize, Vec<HourFigures>) {
        let split_hour = crate::rollup::hour_of(SPLIT_HOUR);
        let blocked = BTreeMap::from([("guardrail_blocked".to_string(), 1)]);
        (
            2,
            vec![
                (split_hour, 0, blocked.clone()),
                (split_hour + 3_600, 0, blocked),
            ],
        )
    }

    /// With the rollups kept for good the hour the request cutoff splits keeps its rows, so the refusal received there
    /// after the cutoff moves into the map beside the next hour's.
    #[test]
    fn under_a_zero_rollup_retention_the_refusal_in_the_hour_the_request_cutoff_splits_moves_into_the_map(
    ) {
        assert_eq!(
            refolded_past_a_seven_day_cutoff(0),
            both_moved(),
            "the split hour's rows stand, so its refusal moved"
        );
    }

    /// A rollup retention longer than the request one keeps the hour the request cutoff splits, so the refusal received
    /// there after the cutoff moves into the map beside the next hour's.
    #[test]
    fn under_a_longer_rollup_retention_the_refusal_in_the_hour_the_request_cutoff_splits_moves_into_the_map(
    ) {
        assert_eq!(
            refolded_past_a_seven_day_cutoff(30),
            both_moved(),
            "the split hour's rows stand, so its refusal moved"
        );
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
        assert_eq!(
            refold_refusals(&whole, &api_prices(), 0, 0, now).unwrap(),
            3
        );
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
        let failed = refold_refusals(&interrupted, &api_prices(), 0, 0, now);
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
        let failed = refold_refusals(&interrupted, &api_prices(), 0, 0, now);
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
            refold_refusals(&interrupted, &api_prices(), 0, 0, now).unwrap(),
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
        let failed = refold_refusals(&db, &api_prices(), 0, 0, now);
        FAIL_AFTER_PAGES.set(None);
        assert!(failed.is_err(), "the injected failure after the first page");
        assert_eq!(
            older(&db),
            Some((1, 1, blocked(1))),
            "the first page took the first refusal out of the row that holds it"
        );
        assert_eq!(
            refold_refusals(&db, &api_prices(), 0, 0, now).unwrap(),
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
        assert_eq!(refold_refusals(&db, &api_prices(), 0, 0, now).unwrap(), 0);
        assert_eq!(
            meta(&db, REFUSALS_BACKFILL_KEY).expect("the mark")["done_at"].as_i64(),
            Some(now)
        );
        assert_eq!(meta(&db, REFUSALS_BACKFILL_CURSOR_KEY), None);
        assert_eq!(rows(&db), before);
    }
}
