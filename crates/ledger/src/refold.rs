//! The moved-threshold re-fold. A rollup row keys the prompt-length band its attempts priced at, read against the
//! threshold in force when each request arrived, so a price file whose threshold moves (a revision that lands late
//! with another `over`, a long-context block added or removed, a date moved) leaves the rows folded before it under
//! the old band. The first start after the change folds those requests again: the old deltas out, the new in.
//!
//! The mark holds every revision's thresholds rather than a hash of the file, since taking a request's old deltas
//! back out needs the band each attempt fell in then, which only the old thresholds give. It carries `since`, the
//! first start that wrote it, because the requests folded before it landed under the migration's `standard` and
//! never used an older band: taking an older band out of their rows would subtract what was never added. A request
//! received before `since` and landed after it folds at the thresholds loaded at its landing and is never re-folded
//! when a threshold moves later, by design, since every pass reads the requests received at or after `since`. The
//! pass in progress is a row of `meta` of its own, written by every page's transaction, so a start that fails after a
//! page committed resumes after that page rather than subtracting its old deltas a second time.
//!
//! A threshold move re-bands the request rows alone. The wasted-probe counts and the probes the keepalive still holds
//! keep the band they settled or were recorded in, which leaves the waste of a probe whose prompt falls between the old
//! and the new threshold, inside the retained window, priced at the band it settled in.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::db::{
    begin_write, row_to_attempt, row_to_request, LedgerDb, ATTEMPT_SELECT, REQUEST_SELECT,
};
use crate::generations::PAGE;
use crate::pricing::{
    effective_instant, LongContext, ModelPrice, ModelRow, PriceRevisions, PriceTable,
};
use crate::record::{AttemptRecord, RequestRecord};
use crate::rollup::{deltas_of, empty_row, read_rollup, unfold, write_rollup, RollupRow};
use crate::LedgerError;

const DAY_MS: i64 = 86_400_000;
const DAY_S: i64 = 86_400;
const HOUR_S: i64 = 3_600;

/// The `meta` key holding the mark: `{"since": <ms>, "revisions": [{"effective": "<date>", "bands": {"<model>":
/// <over or null>}}]}`, `since` the instant the first mark was written (the first start after the migration) and the
/// list every revision's thresholds as the last start loaded them. Requests received before `since` landed under the
/// migration's `standard` and are never re-folded: nothing is backfilled.
pub const PRICE_BANDS_KEY: &str = "price_bands";

/// The `meta` key holding a pass in progress: `{"source": [..], "target": [..], "after": {"received_at": <ms>,
/// "id": "<request id>"}}`, written by every page's transaction and deleted by the last page's, in the same
/// transaction that writes the mark. A start that finds it resumes after `after` with the cursor's source and
/// target, whatever the file holds now, then compares the finished mark with the loaded list as usual.
pub const PRICE_BANDS_REFOLD_KEY: &str = "price_bands_refold";

/// One revision's thresholds: every model it lists, each with its long-context block's `over`, `None` for a row
/// without one. A row without the block is kept, since a dated row without it beside an undated row with it bands by
/// its own row, and leaving it out would resolve it to the undated row's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Thresholds {
    effective: String,
    bands: BTreeMap<String, Option<u64>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Mark {
    since: i64,
    revisions: Vec<Thresholds>,
}

/// The last request a committed page re-folded, in the pass's order.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct After {
    received_at: i64,
    id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    source: Vec<Thresholds>,
    target: Vec<Thresholds>,
    after: After,
}

/// Re-folds the requests whose band moved since the last start, in pages of 1,000 requests ordered by
/// `(received_at, id)`, one transaction per page carrying the page's rows and its cursor, the mark rewritten and the
/// cursor cleared in the last page's transaction. Answers how many requests moved: 0 on a first start (which writes
/// the mark with `since` and moves nothing) and on an unchanged file. Rows older than the retention cutoff and rows
/// received before `since` keep their split. Under a request retention of 0 the walk reads every request. A request
/// received before the first hour boundary at or after the retention cutoff, or under a request retention of 0 before
/// the first hour the rollup purge keeps, moves only where its hour still holds the rows it folded into, so an hour
/// the purge removed stays empty and an hour it kept for a budgeted member moves.
pub fn refold_bands(
    db: &LedgerDb,
    prices: &PriceRevisions,
    retention_days: u32,
    rollup_retention_days: u32,
    now: i64,
) -> Result<usize, LedgerError> {
    let loaded = thresholds_of(prices);
    // Each purge's own arithmetic, a zero keeping its rows for good. A nonzero request retention bounds the walk,
    // since the configuration holds a nonzero rollup retention at least as long as the request one, so the rollup
    // purge drops at most the hour the request cutoff splits. The first hour boundary at or after the request cutoff
    // guards each request before it rather than bounding the walk, since that hour's rows stand for every member under
    // a rollup retention of 0 or a longer one, and a budgeted member's under an equal one too. Under a request
    // retention of 0 the first hour the rollup purge keeps guards each request before it rather than bounding the walk,
    // since the purge keeps a budgeted member's hours from its period's start, before that hour, and `member_spends`
    // prices them by band.
    let (cutoff, guard) = match (retention_days, rollup_retention_days) {
        (0, 0) => (None, None),
        (0, days) => (None, Some(first_kept_hour(now, days))),
        (days, _) => (
            Some(now - i64::from(days) * DAY_MS),
            Some(first_kept_hour(now, days)),
        ),
    };
    let mut moved = 0;
    let (mark, cursor) = db.write(|c| {
        Ok((
            read_meta::<Mark>(c, PRICE_BANDS_KEY)?,
            read_meta::<Cursor>(c, PRICE_BANDS_REFOLD_KEY)?,
        ))
    })?;
    let Some(mut mark) = mark else {
        db.write(|c| {
            let tx = begin_write(c)?;
            write_meta(
                &tx,
                PRICE_BANDS_KEY,
                &Mark {
                    since: now,
                    revisions: loaded,
                },
            )?;
            tx.commit()?;
            Ok(())
        })?;
        return Ok(0);
    };
    if let Some(cursor) = cursor {
        moved += pass(
            db,
            mark.since,
            cutoff,
            guard,
            cursor.source,
            cursor.target.clone(),
            Some(cursor.after),
        )?;
        mark.revisions = cursor.target;
    }
    if mark.revisions != loaded {
        moved += pass(db, mark.since, cutoff, guard, mark.revisions, loaded, None)?;
    }
    Ok(moved)
}

/// One pass from `source` to `target` over the requests received at or after the later of `since`, the cutoff and the
/// first instant the two lists' band functions differ, resuming after `after` when a start left one halfway. A request
/// received before `guard` moves only where every row its old deltas name still stands.
fn pass(
    db: &LedgerDb,
    since: i64,
    cutoff: Option<i64>,
    guard: Option<i64>,
    source: Vec<Thresholds>,
    target: Vec<Thresholds>,
    mut after: Option<After>,
) -> Result<usize, LedgerError> {
    let (old, new) = (revisions_of(&source)?, revisions_of(&target)?);
    let floor = cutoff.map_or(since, |cutoff| cutoff.max(since));
    let first = first_difference(&old, &new, floor);
    let page = page_size();
    let mut moved = 0;
    let mut pages = 0;
    loop {
        let done = db.write(|c| {
            let tx = begin_write(c)?;
            let requests = match first {
                Some(first) => requests_after(&tx, first, after.as_ref(), page)?,
                None => Vec::new(),
            };
            for (request, attempts) in &requests {
                let (was, now) = (
                    deltas_of(request, attempts, &old),
                    deltas_of(request, attempts, &new),
                );
                if was == now {
                    continue;
                }
                // The purge removes a member's hour whole, so a request whose rows are gone stays out of the hour
                // rather than writing the new band's rows into it.
                if guard.is_some_and(|guard| request.received_at < guard) && !rows_stand(&tx, &was)?
                {
                    continue;
                }
                for delta in &was {
                    unfold(&tx, delta)?;
                }
                for delta in &now {
                    let mut row = read_rollup(&tx, delta)?.unwrap_or_else(|| empty_row(delta));
                    row.add(delta);
                    write_rollup(&tx, &row)?;
                }
                moved += 1;
            }
            let last = requests.len() < page;
            if last {
                tx.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    params![PRICE_BANDS_REFOLD_KEY],
                )?;
                write_meta(
                    &tx,
                    PRICE_BANDS_KEY,
                    &Mark {
                        since,
                        revisions: target.clone(),
                    },
                )?;
            } else if let Some((request, _)) = requests.last() {
                let next = After {
                    received_at: request.received_at,
                    id: request.id.clone(),
                };
                write_meta(
                    &tx,
                    PRICE_BANDS_REFOLD_KEY,
                    &Cursor {
                        source: source.clone(),
                        target: target.clone(),
                        after: next.clone(),
                    },
                )?;
                after = Some(next);
            }
            tx.commit()?;
            Ok(last)
        })?;
        pages += 1;
        fail_after_page(pages)?;
        if done {
            return Ok(moved);
        }
    }
}

/// The first hour the rollup purge keeps under a retention of `days` at `now`, in milliseconds. The purge cuts in
/// seconds and drops every hour that starts before its cutoff.
fn first_kept_hour(now: i64, days: u32) -> i64 {
    let cutoff = now / 1_000 - i64::from(days) * DAY_S;
    let hour = cutoff.div_euclid(HOUR_S) * HOUR_S;
    let first_kept = if hour < cutoff { hour + HOUR_S } else { hour };
    first_kept * 1_000
}

/// Whether the row each delta names stands.
fn rows_stand(conn: &Connection, deltas: &[RollupRow]) -> Result<bool, LedgerError> {
    for delta in deltas {
        if read_rollup(conn, delta)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every revision's thresholds, in the list's order.
fn thresholds_of(prices: &PriceRevisions) -> Vec<Thresholds> {
    prices
        .iter()
        .map(|table| Thresholds {
            effective: table.effective.clone(),
            bands: table
                .models
                .iter()
                .map(|(model, row)| {
                    (
                        model.clone(),
                        row.long_context.as_ref().map(|long| long.over),
                    )
                })
                .collect(),
        })
        .collect()
}

/// A list whose rows carry the thresholds alone, every cell zero: `deltas_of` reads a band from it and prices nothing.
fn revisions_of(list: &[Thresholds]) -> Result<PriceRevisions, LedgerError> {
    let tables = list
        .iter()
        .map(|revision| PriceTable {
            effective: revision.effective.clone(),
            models: revision
                .bands
                .iter()
                .map(|(model, over)| {
                    (
                        model.clone(),
                        ModelRow {
                            standard: ModelPrice::default(),
                            fast: None,
                            long_context: over.map(|over| LongContext {
                                over,
                                cells: ModelPrice::default(),
                            }),
                        },
                    )
                })
                .collect::<IndexMap<_, _>>(),
        })
        .collect();
    PriceRevisions::new(tables, "mark")
}

/// The first instant at or after `floor` at which the two lists answer another threshold for any model, `None` where
/// they agree from `floor` on.
fn first_difference(old: &PriceRevisions, new: &PriceRevisions, floor: i64) -> Option<i64> {
    let mut instants: Vec<i64> = old
        .iter()
        .chain(new.iter())
        .filter_map(|table| effective_instant(&table.effective))
        .filter(|instant| *instant > floor)
        .chain(std::iter::once(floor))
        .collect();
    instants.sort_unstable();
    instants.dedup();
    // Through the exact id, then the undated one, as `band_of` resolves a row: a dated row added or removed moves the
    // dated id onto or off its undated row's threshold, which a lookup by the exact id alone reads as no change.
    let threshold = |table: &PriceTable, model: &str| {
        phoebis_routing::listing::row_for(model, |key| table.models.get(key))
            .and_then(|row| row.long_context.as_ref())
            .map(|long| long.over)
    };
    let models: BTreeSet<&str> = old
        .iter()
        .chain(new.iter())
        .flat_map(|table| table.models.keys().map(String::as_str))
        .collect();
    instants.into_iter().find(|at| {
        let (was, now) = (old.in_force(*at), new.in_force(*at));
        models
            .iter()
            .any(|model| threshold(was, model) != threshold(now, model))
    })
}

/// The next page of requests received at or after `from`, past `after`, in `(received_at, id)` order, each with its
/// attempts.
fn requests_after(
    conn: &Connection,
    from: i64,
    after: Option<&After>,
    page: usize,
) -> Result<Vec<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
    let (after_at, after_id) = after.map_or((i64::MIN, ""), |a| (a.received_at, a.id.as_str()));
    let mut stmt = conn.prepare(&format!(
        "{REQUEST_SELECT} WHERE received_at >= ?1 AND (received_at, id) > (?2, ?3) ORDER BY received_at, id LIMIT ?4"
    ))?;
    let requests = stmt
        .query_map(
            params![from, after_at, after_id, page as i64],
            row_to_request,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut attempts_of = conn.prepare(&format!(
        "{ATTEMPT_SELECT} WHERE request_id = ?1 ORDER BY attempt"
    ))?;
    requests
        .into_iter()
        .map(|request| {
            let attempts = attempts_of
                .query_map(params![request.id], row_to_attempt)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((request, attempts))
        })
        .collect()
}

pub(crate) fn read_meta<T: for<'de> Deserialize<'de>>(
    conn: &Connection,
    key: &str,
) -> Result<Option<T>, LedgerError> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(text.map(|text| serde_json::from_str(&text)).transpose()?)
}

pub(crate) fn write_meta(
    tx: &Transaction<'_>,
    key: &str,
    value: &impl Serialize,
) -> Result<(), LedgerError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn page_size() -> usize {
    PAGE
}

#[cfg(test)]
pub(crate) fn page_size() -> usize {
    TEST_PAGE.get().unwrap_or(PAGE)
}

#[cfg(not(test))]
pub(crate) fn fail_after_page(_pages: usize) -> Result<(), LedgerError> {
    Ok(())
}

/// The failure a test injects once a pass has committed the pages it names, so the resume has a pass to finish.
#[cfg(test)]
pub(crate) fn fail_after_page(pages: usize) -> Result<(), LedgerError> {
    if FAIL_AFTER_PAGES.get() == Some(pages) {
        return Err(LedgerError::Io(std::io::Error::other(
            "injected failure after a committed page",
        )));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The page size a test sets in place of `PAGE`, so a pass of three requests runs three pages.
    pub(crate) static TEST_PAGE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// The pages a pass commits before it fails, which a test sets to interrupt a pass after a committed page.
    pub(crate) static FAIL_AFTER_PAGES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{Band, PriceTable, Speed};
    use crate::query::{member_spends, BudgetedMember};
    use crate::record::{AttemptRecord, RequestRecord};
    use crate::rollup::{read_rollups, RollupRow, RollupSelect, LONG_BAND, STANDARD_BAND};

    const HAIKU: &str = "claude-haiku-5-5";
    const DAY: i64 = 86_400_000;
    /// 2026-01-01 00:00 UTC, the test list's one entry.
    const NEW_YEAR: i64 = 1_767_225_600_000;

    /// The test fixture's first entry alone, its Haiku 5.5 threshold at `over`, or with no long-context block.
    fn list(over: Option<u64>) -> PriceRevisions {
        let mut table: PriceTable = crate::test_tables::api_prices().in_force(NEW_YEAR).clone();
        let row = table
            .models
            .get_mut(HAIKU)
            .expect("the fixture lists Haiku 5.5");
        match over {
            Some(over) => {
                row.long_context
                    .as_mut()
                    .expect("the fixture prices Haiku 5.5 by prompt length")
                    .over = over;
            }
            None => row.long_context = None,
        }
        PriceRevisions::new(vec![table], "test").expect("one entry")
    }

    /// A Haiku 5.5 request with one answered attempt of `prompt` uncached input tokens.
    fn haiku(id: &str, received_at: i64, prompt: u64) -> (RequestRecord, Vec<AttemptRecord>) {
        (
            RequestRecord {
                id: id.into(),
                received_at,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                model: HAIKU.into(),
                canonical_model: HAIKU.into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            vec![AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                started_at: received_at,
                outcome: "succeeded".into(),
                input_tokens: Some(prompt),
                output_tokens: Some(10),
                ..Default::default()
            }],
        )
    }

    fn land(db: &LedgerDb, request: &(RequestRecord, Vec<AttemptRecord>), prices: &PriceRevisions) {
        db.insert_request_at(&request.0, &request.1, None, prices)
            .unwrap();
    }

    fn rows(db: &LedgerDb) -> Vec<RollupRow> {
        db.read_blocking(|c| read_rollups(c, i64::MIN, i64::MAX, &RollupSelect::default()))
            .unwrap()
    }

    /// Each row's hour and band with its request and token counts.
    fn bands(db: &LedgerDb) -> Vec<(i64, String, u64, u64)> {
        rows(db)
            .into_iter()
            .map(|r| (r.hour, r.band, r.requests, r.input_tokens))
            .collect()
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

    /// The thresholds a mark or a cursor holds for Haiku 5.5 in its one entry.
    fn haiku_over(list: &serde_json::Value) -> Option<u64> {
        list[0]["bands"][HAIKU].as_u64()
    }

    /// A threshold that moves re-folds the requests the ledger still holds past the retention cutoff: a prompt of
    /// 1,500 tokens moves from `long` to `standard` when `over` goes from 1,000 to 2,000, and the request older than
    /// the cutoff keeps its split.
    #[test]
    fn a_moved_threshold_refolds_the_rows_past_the_cutoff_and_leaves_the_older_ones() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 20 * DAY);
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 7, 0, first).unwrap(),
            0,
            "the first start writes the mark"
        );
        let (older, newer) = (now - 10 * DAY, now - DAY);
        land(&db, &haiku("old", older, 1_500), &list(Some(1_000)));
        land(&db, &haiku("new", newer, 1_500), &list(Some(1_000)));
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 7, 0, now).unwrap(),
            1,
            "one request moved"
        );
        assert_eq!(
            bands(&db),
            [
                (
                    crate::rollup::hour_of(older),
                    LONG_BAND.to_string(),
                    1,
                    1_500
                ),
                (
                    crate::rollup::hour_of(newer),
                    STANDARD_BAND.to_string(),
                    1,
                    1_500
                ),
            ]
        );
        let mark = meta(&db, PRICE_BANDS_KEY).expect("the mark");
        assert_eq!(
            (mark["since"].as_i64(), haiku_over(&mark["revisions"])),
            (Some(first), Some(2_000)),
            "the mark keeps its first start and holds the loaded thresholds"
        );
    }

    const DATED: &str = "claude-haiku-5-5-20260101";

    /// `list(Some(over))` with a row of its own for `DATED`: Haiku 5.5's cells without the long-context block.
    fn with_dated(over: u64) -> PriceRevisions {
        let mut table = list(Some(over)).in_force(NEW_YEAR).clone();
        let mut row = table.models[HAIKU].clone();
        row.long_context = None;
        table.models.insert(DATED.into(), row);
        PriceRevisions::new(vec![table], "test").expect("one entry")
    }

    /// `haiku` on `DATED`.
    fn on_dated(id: &str, received_at: i64, prompt: u64) -> (RequestRecord, Vec<AttemptRecord>) {
        let (mut request, attempts) = haiku(id, received_at, prompt);
        request.model = DATED.into();
        request.canonical_model = DATED.into();
        (request, attempts)
    }

    /// A dated id the file lists without a long-context block, beside its undated id with one, bands by its own row,
    /// so a threshold that moves on the undated id leaves the dated id's request where it landed.
    #[test]
    fn a_dated_row_without_a_block_keeps_its_band_when_its_undated_rows_threshold_moves() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        assert_eq!(
            refold_bands(&db, &with_dated(1_000), 0, 0, first).unwrap(),
            0
        );
        land(
            &db,
            &on_dated("r", first + 1_000, 1_500),
            &with_dated(1_000),
        );
        let before = bands(&db);
        assert_eq!(
            before,
            [(
                crate::rollup::hour_of(first + 1_000),
                STANDARD_BAND.to_string(),
                1,
                1_500
            )]
        );
        assert_eq!(
            refold_bands(&db, &with_dated(2_000), 0, 0, now).unwrap(),
            0,
            "no request moved"
        );
        assert_eq!(bands(&db), before);
    }

    /// Removing a dated row without a long-context block hands the dated id to its undated row's threshold, which an
    /// unchanged undated row leaves where it was: the pass still re-folds the dated id's request above it.
    #[test]
    fn removing_a_dated_row_without_a_block_refolds_its_requests_under_the_undated_threshold() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        assert_eq!(
            refold_bands(&db, &with_dated(1_000), 0, 0, first).unwrap(),
            0
        );
        let received = first + 1_000;
        land(&db, &on_dated("r", received, 1_500), &with_dated(1_000));
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 0, now).unwrap(),
            1,
            "the dated id's request moved"
        );
        assert_eq!(
            bands(&db),
            [(
                crate::rollup::hour_of(received),
                LONG_BAND.to_string(),
                1,
                1_500
            )]
        );
    }

    /// Adding a dated row without a long-context block takes the dated id off its undated row's threshold, so the
    /// pass re-folds the dated id's request that landed above it.
    #[test]
    fn adding_a_dated_row_without_a_block_refolds_its_requests_out_of_the_undated_threshold() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 0, first).unwrap(),
            0
        );
        let received = first + 1_000;
        land(&db, &on_dated("r", received, 1_500), &list(Some(1_000)));
        assert_eq!(
            refold_bands(&db, &with_dated(1_000), 0, 0, now).unwrap(),
            1,
            "the dated id's request moved"
        );
        assert_eq!(
            bands(&db),
            [(
                crate::rollup::hour_of(received),
                STANDARD_BAND.to_string(),
                1,
                1_500
            )]
        );
    }

    /// A first start writes the mark with `since` at its clock and moves nothing, and the same list again moves
    /// nothing.
    #[test]
    fn a_first_start_writes_the_mark_and_an_unchanged_list_moves_nothing() {
        let db = LedgerDb::open_in_memory().unwrap();
        land(&db, &haiku("r", NEW_YEAR + DAY, 1_500), &list(Some(1_000)));
        let before = rows(&db);
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 0, NEW_YEAR + 2 * DAY).unwrap(),
            0
        );
        let mark = meta(&db, PRICE_BANDS_KEY).expect("the first start writes the mark");
        assert_eq!(
            (mark["since"].as_i64(), haiku_over(&mark["revisions"])),
            (Some(NEW_YEAR + 2 * DAY), Some(2_000))
        );
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 0, NEW_YEAR + 3 * DAY).unwrap(),
            0
        );
        assert_eq!(rows(&db), before, "nothing moved: nothing is backfilled");
    }

    /// The retention purge at `now` under the two retentions, the `budgeted` members' periods kept.
    fn purge(
        db: &LedgerDb,
        retention_days: u32,
        rollup_retention_days: u32,
        now: i64,
        budgeted: &[BudgetedMember],
    ) {
        let config = crate::LedgerConfig {
            retention_days,
            rollup_retention_days,
            ..Default::default()
        };
        crate::bodies::run_retention_blocking(db, &config, now, budgeted).unwrap();
    }

    /// A request retention of 0 keeps the requests for good while a rollup retention of seven days drops whole hours:
    /// the purge removes the hour its cutoff splits, and the re-fold writes no row back into it for the request
    /// received there after the cutoff, while the request inside the first kept hour moves. With both retentions 0
    /// nothing is purged and every request moves.
    #[test]
    fn under_a_zero_request_retention_an_hour_the_rollup_purge_removed_stays_empty_through_the_refold(
    ) {
        const HOUR: i64 = 3_600_000;
        // The purge's cutoff, seven days back from `now`, falls half an hour into `split_hour`.
        let split_hour = NEW_YEAR + 13 * DAY;
        let now = split_hour + 7 * DAY + HOUR / 2;
        let (dropped, kept) = (split_hour + 3 * HOUR / 4, split_hour + HOUR + 60_000);
        let refolded = |rollup_retention_days: u32| {
            let db = LedgerDb::open_in_memory().unwrap();
            assert_eq!(
                refold_bands(
                    &db,
                    &list(Some(1_000)),
                    0,
                    rollup_retention_days,
                    NEW_YEAR + DAY
                )
                .unwrap(),
                0,
                "the first start writes the mark"
            );
            land(&db, &haiku("dropped", dropped, 1_500), &list(Some(1_000)));
            land(&db, &haiku("kept", kept, 1_500), &list(Some(1_000)));
            purge(&db, 0, rollup_retention_days, now, &[]);
            let moved =
                refold_bands(&db, &list(Some(2_000)), 0, rollup_retention_days, now).unwrap();
            (moved, bands(&db))
        };
        let band = |received_at: i64, band: &str| {
            (
                crate::rollup::hour_of(received_at),
                band.to_string(),
                1,
                1_500,
            )
        };
        assert_eq!(
            refolded(7),
            (1, vec![band(kept, STANDARD_BAND)]),
            "the hour the purge removed gets no row back"
        );
        assert_eq!(
            refolded(0),
            (
                2,
                vec![band(dropped, STANDARD_BAND), band(kept, STANDARD_BAND)]
            ),
            "with both retentions 0 every request moves"
        );
    }

    /// Under a request retention of 0 and a rollup retention of seven days the purge keeps a monthly budgeted
    /// member's hours from the month's first while it removes another member's hour beside them. A threshold that
    /// moves re-folds the kept hour, which `member_spends` then prices at the new band, and writes no row back into
    /// the other member's.
    #[test]
    fn under_a_zero_request_retention_a_budgeted_members_kept_hours_move_with_the_threshold() {
        const HOUR: i64 = 3_600_000;
        let db = LedgerDb::open_in_memory().unwrap();
        let now = NEW_YEAR + 20 * DAY;
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 7, NEW_YEAR + DAY).unwrap(),
            0,
            "the first start writes the mark"
        );
        let day_four = NEW_YEAR + 4 * DAY + HOUR / 2;
        land(&db, &haiku("budgeted", day_four, 1_500), &list(Some(1_000)));
        let mut other = haiku("other", day_four + 1_000, 1_500);
        other.0.pool_id = Some("q".into());
        other.1[0].account_id = "b".into();
        land(&db, &other, &list(Some(1_000)));
        let budgeted = [BudgetedMember {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: phoebis_store::BudgetPeriod::Month,
            metered_only: false,
        }];
        purge(&db, 0, 7, now, &budgeted);
        let hour = crate::rollup::hour_of(day_four);
        assert_eq!(
            bands(&db),
            [(hour, LONG_BAND.to_string(), 1, 1_500)],
            "the purge kept the budgeted member's hour and removed the other member's"
        );
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 7, now).unwrap(),
            1,
            "the budgeted member's request moved"
        );
        assert_eq!(
            bands(&db),
            [(hour, STANDARD_BAND.to_string(), 1, 1_500)],
            "the kept hour moved to the new band and the removed hour got no row back"
        );
        let prices = list(Some(2_000));
        let usage = phoebis_routing::PinUsage {
            input: 1_500,
            output: 10,
            ..Default::default()
        };
        let at_band = |band: Band| {
            prices
                .in_force(now)
                .estimate_banded(HAIKU, Speed::Standard, band, usage)
                .expect("the list prices Haiku 5.5")
                .0
        };
        assert_ne!(
            at_band(Band::Standard),
            at_band(Band::LongContext),
            "the two bands price the prompt apart"
        );
        let spent = db
            .read_blocking(|c| member_spends(c, &budgeted, now, &prices))
            .unwrap();
        assert_eq!(
            spent.iter().map(|s| s.spent_usd).collect::<Vec<_>>(),
            [at_band(Band::Standard)],
            "the month's spend prices the kept hour at the new band"
        );
    }

    const HOUR: i64 = 3_600_000;
    /// The hour a seven-day cutoff back from `SPLIT_NOW` falls half an hour into.
    const SPLIT_HOUR: i64 = NEW_YEAR + 13 * DAY;
    const SPLIT_NOW: i64 = SPLIT_HOUR + 7 * DAY + HOUR / 2;
    /// A request received inside the split hour after the cutoff, and one inside the next hour.
    const IN_THE_SPLIT_HOUR: i64 = SPLIT_HOUR + 3 * HOUR / 4;
    const IN_THE_NEXT_HOUR: i64 = SPLIT_HOUR + HOUR + 60_000;

    /// The ledger under a request retention of seven days and `rollup_retention_days` as the purge at `SPLIT_NOW`
    /// leaves it, the `budgeted` members' periods kept, re-folded once the threshold moves. Answers how many requests
    /// moved and the rows' bands.
    fn refolded_past_a_seven_day_cutoff(
        rollup_retention_days: u32,
        budgeted: &[BudgetedMember],
    ) -> (usize, Vec<(i64, String, u64, u64)>) {
        let db = LedgerDb::open_in_memory().unwrap();
        assert_eq!(
            refold_bands(
                &db,
                &list(Some(1_000)),
                7,
                rollup_retention_days,
                NEW_YEAR + DAY
            )
            .unwrap(),
            0,
            "the first start writes the mark"
        );
        land(
            &db,
            &haiku("split", IN_THE_SPLIT_HOUR, 1_500),
            &list(Some(1_000)),
        );
        land(
            &db,
            &haiku("next", IN_THE_NEXT_HOUR, 1_500),
            &list(Some(1_000)),
        );
        purge(&db, 7, rollup_retention_days, SPLIT_NOW, budgeted);
        let moved =
            refold_bands(&db, &list(Some(2_000)), 7, rollup_retention_days, SPLIT_NOW).unwrap();
        (moved, bands(&db))
    }

    fn standard(received_at: i64) -> (i64, String, u64, u64) {
        (
            crate::rollup::hour_of(received_at),
            STANDARD_BAND.to_string(),
            1,
            1_500,
        )
    }

    /// Under equal request and rollup retentions the rollup purge drops the hour the request cutoff splits while the
    /// requests received after the cutoff stand: the re-fold writes no row back into that hour, and the request in the
    /// next hour moves.
    #[test]
    fn under_equal_retentions_the_hour_the_rollup_purge_dropped_at_the_request_cutoff_stays_empty_through_the_refold(
    ) {
        assert_eq!(
            refolded_past_a_seven_day_cutoff(7, &[]),
            (1, vec![standard(IN_THE_NEXT_HOUR)]),
            "the dropped hour gets no row back"
        );
    }

    /// Under equal retentions the purge keeps a monthly budgeted member's hour at the request cutoff, so the request
    /// received there after the cutoff moves with the threshold beside the one in the next hour.
    #[test]
    fn under_equal_retentions_a_budgeted_members_kept_hour_at_the_request_cutoff_moves_with_the_threshold(
    ) {
        let budgeted = [BudgetedMember {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: phoebis_store::BudgetPeriod::Month,
            metered_only: false,
        }];
        assert_eq!(
            refolded_past_a_seven_day_cutoff(7, &budgeted),
            (
                2,
                vec![standard(IN_THE_SPLIT_HOUR), standard(IN_THE_NEXT_HOUR)]
            ),
            "the kept hour moved"
        );
    }

    /// With the rollups kept for good the hour the request cutoff splits keeps its rows, so the request received there
    /// after the cutoff moves with the threshold.
    #[test]
    fn under_a_zero_rollup_retention_the_request_in_the_hour_the_request_cutoff_splits_moves_with_the_threshold(
    ) {
        assert_eq!(
            refolded_past_a_seven_day_cutoff(0, &[]),
            (
                2,
                vec![standard(IN_THE_SPLIT_HOUR), standard(IN_THE_NEXT_HOUR)]
            ),
            "the split hour's rows stand, so its request moved"
        );
    }

    /// A pass that fails after a committed page leaves its cursor and the old mark, and the next start resumes
    /// after the cursor with the cursor's own lists rather than subtracting the committed page again: the rows then
    /// equal an uninterrupted pass's.
    #[test]
    fn a_pass_that_fails_after_a_committed_page_resumes_after_its_cursor_and_sums_as_an_uninterrupted_pass_does(
    ) {
        let seed = |db: &LedgerDb| {
            assert_eq!(
                refold_bands(db, &list(Some(1_000)), 0, 0, NEW_YEAR).unwrap(),
                0
            );
            for (n, id) in ["a", "b", "c"].into_iter().enumerate() {
                land(
                    db,
                    &haiku(id, NEW_YEAR + (n as i64 + 1) * DAY, 1_500),
                    &list(Some(1_000)),
                );
            }
        };
        let whole = LedgerDb::open_in_memory().unwrap();
        seed(&whole);
        assert_eq!(
            refold_bands(&whole, &list(Some(2_000)), 0, 0, NEW_YEAR + 9 * DAY).unwrap(),
            3
        );
        let interrupted = LedgerDb::open_in_memory().unwrap();
        seed(&interrupted);
        TEST_PAGE.set(Some(1));
        FAIL_AFTER_PAGES.set(Some(1));
        let failed = refold_bands(&interrupted, &list(Some(2_000)), 0, 0, NEW_YEAR + 9 * DAY);
        FAIL_AFTER_PAGES.set(None);
        assert!(failed.is_err(), "the injected failure after the first page");
        let cursor =
            meta(&interrupted, PRICE_BANDS_REFOLD_KEY).expect("the cursor of the committed page");
        assert_eq!(
            (
                cursor["after"]["id"].as_str(),
                haiku_over(&cursor["source"]),
                haiku_over(&cursor["target"])
            ),
            (Some("a"), Some(1_000), Some(2_000))
        );
        assert_eq!(
            haiku_over(&meta(&interrupted, PRICE_BANDS_KEY).expect("the mark")["revisions"]),
            Some(1_000),
            "the mark still holds the source"
        );
        assert_eq!(
            refold_bands(&interrupted, &list(Some(2_000)), 0, 0, NEW_YEAR + 10 * DAY).unwrap(),
            2,
            "the resumed pass moves the two requests after the cursor"
        );
        TEST_PAGE.set(None);
        assert_eq!(
            meta(&interrupted, PRICE_BANDS_REFOLD_KEY),
            None,
            "the cursor is cleared"
        );
        assert_eq!(
            haiku_over(&meta(&interrupted, PRICE_BANDS_KEY).expect("the mark")["revisions"]),
            Some(2_000)
        );
        assert_eq!(rows(&interrupted), rows(&whole));
    }

    /// Requests received before the mark's `since` landed under the migration's `standard` and never used an older
    /// band, so a threshold correction after the upgrade leaves them where they are: the pass reads only requests
    /// received at or after `since`, and no token count over the table changes.
    #[test]
    fn an_upgrade_then_a_threshold_correction_leaves_the_rows_folded_before_the_mark_alone() {
        let db = LedgerDb::open_in_memory().unwrap();
        // Folded as the migration's default rows file them: under `standard`, which a list with no long-context
        // block selects for every prompt.
        land(&db, &haiku("before", NEW_YEAR + DAY, 1_500), &list(None));
        let upgrade = NEW_YEAR + 2 * DAY;
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 0, upgrade).unwrap(),
            0
        );
        land(
            &db,
            &haiku("after", NEW_YEAR + 3 * DAY, 1_500),
            &list(Some(1_000)),
        );
        let tokens = |db: &LedgerDb| -> u64 { rows(db).iter().map(|r| r.input_tokens).sum() };
        let total = tokens(&db);
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 0, NEW_YEAR + 4 * DAY).unwrap(),
            1,
            "only the request after the mark moves"
        );
        assert_eq!(tokens(&db), total, "no count was doubled or lost");
        assert_eq!(
            bands(&db),
            [
                (
                    crate::rollup::hour_of(NEW_YEAR + DAY),
                    STANDARD_BAND.to_string(),
                    1,
                    1_500
                ),
                (
                    crate::rollup::hour_of(NEW_YEAR + 3 * DAY),
                    STANDARD_BAND.to_string(),
                    1,
                    1_500
                ),
            ]
        );
    }
}
