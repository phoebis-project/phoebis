//! Captured bodies, kept only for keys whose effective capture window is above zero, and the hourly purge
//! that enforces every retention window the ledger has, in slices on its own connection.

use std::sync::Arc;

use phoebis_routing::DAY_MS;
use rusqlite::{params, params_from_iter, ToSql, Transaction};

use crate::db::begin_write;
use crate::query::{period_bounds, BudgetedMember};
use crate::task::LedgerConfig;
use crate::{LedgerDb, LedgerError};

/// A stream longer than this is truncated in the capture; the ledger row still says the body was captured,
/// and the truncation is the price of a bounded in-flight table.
pub const MAX_CAPTURED_BODY: usize = 8 << 20;

/// Rows one purge transaction deletes at most, per family, a bodies slice bounded in bytes beside them: a slice holds
/// the writer lock for a fraction of the busy timeout, so the writer's batch waits behind one slice at most and never
/// behind a whole pass, and the WAL grows by a slice's pages.
pub const PURGE_SLICE: usize = 1_000;

/// About the bytes one bodies slice frees, the stored response counted at its length: the bytes a slice frees hold the
/// writer's lock, and 32 MiB holds it well under `BUSY_TIMEOUT` at the slowest disk read, where an hourly pass of 1
/// to 3 MB bodies held it 11 to 52 s. A body above the budget goes alone in a slice of its own, since a slice always
/// takes its first body: up to `proxy.max_request_bytes` and the 8 MiB response cap, 40 MiB at the defaults, still
/// well under the busy timeout.
pub const PURGE_SLICE_BYTES: usize = 32 << 20;

const DAY_S: i64 = 86_400;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub attempts: usize,
    pub bodies: usize,
    pub decisions: usize,
    pub events: usize,
    pub extra_usage_samples: usize,
    pub quota_samples: usize,
    pub requests: usize,
    pub rollups: usize,
}

impl Purged {
    fn add(&mut self, slice: &Purged) {
        self.attempts += slice.attempts;
        self.bodies += slice.bodies;
        self.decisions += slice.decisions;
        self.events += slice.events;
        self.extra_usage_samples += slice.extra_usage_samples;
        self.quota_samples += slice.quota_samples;
        self.requests += slice.requests;
        self.rollups += slice.rollups;
    }
}

/// A slice's answer: the counts, and whether the family has more rows to purge in this pass.
pub(crate) struct Slice {
    pub purged: Purged,
    pub more: bool,
}

/// The families the purge walks, in the order a pass cuts them.
#[derive(Clone, Copy)]
enum Family {
    Bodies,
    Requests,
    KeptWarmMarks,
    Events,
    Decisions,
    QuotaSamples,
    ExtraUsageSamples,
    Rollups,
    AccountRollups,
}

impl Family {
    const ALL: [Family; 9] = [
        Family::Bodies,
        Family::Requests,
        Family::KeptWarmMarks,
        Family::Events,
        Family::Decisions,
        Family::QuotaSamples,
        Family::ExtraUsageSamples,
        Family::Rollups,
        Family::AccountRollups,
    ];

    /// The rows a slice of this family selected, out of the counts it answered; a full slice means another may
    /// follow.
    fn selected(self, slice: &Purged) -> usize {
        match self {
            Family::Bodies => slice.bodies,
            Family::Requests | Family::KeptWarmMarks => slice.requests,
            Family::Events => slice.events,
            Family::Decisions => slice.decisions,
            Family::QuotaSamples => slice.quota_samples,
            Family::ExtraUsageSamples => slice.extra_usage_samples,
            Family::Rollups | Family::AccountRollups => slice.rollups,
        }
    }
}

/// Every family's cutoff, computed once per pass. Zero is "keep forever": the cutoff is `None` and the purge of
/// that family is skipped rather than run against a cutoff at the epoch.
#[derive(Clone, Copy)]
struct Cutoffs {
    request: Option<i64>,
    rollup: Option<i64>,
    sample: Option<i64>,
}

/// The requests' cutoff, `None` at a zero `retention_days`; the registry's history purge reads the same one.
pub(crate) fn request_cutoff(config: &LedgerConfig, now: i64) -> Option<i64> {
    (config.retention_days != 0).then(|| now - config.retention_days as i64 * DAY_MS)
}

impl Cutoffs {
    fn of(config: &LedgerConfig, now: i64) -> Cutoffs {
        let request = request_cutoff(config, now);
        let rollup = (config.rollup_retention_days != 0)
            .then(|| now / 1_000 - config.rollup_retention_days as i64 * DAY_S);
        // The samples' buckets are seconds while the requests' timestamps are milliseconds; one window, two units.
        let sample = request.map(|at| at / 1_000);
        Cutoffs {
            request,
            rollup,
            sample,
        }
    }
}

/// `run_retention_blocking` on a blocking worker. The writer still waits for one slice at most, since each slice is
/// its own transaction under the purger's lock. `budgeted` is every budgeted period of every pool member with a spend
/// budget, as `member_spends` takes them.
pub async fn run_retention(
    db: Arc<LedgerDb>,
    config: LedgerConfig,
    now: i64,
    budgeted: Vec<BudgetedMember>,
) -> Result<Purged, LedgerError> {
    tokio::task::spawn_blocking(move || run_retention_blocking(&db, &config, now, &budgeted))
        .await
        .map_err(|e| LedgerError::Io(std::io::Error::other(e)))?
}

/// The pass on the calling thread: every family in slices, a family repeated while its slice says more rows wait, and
/// the sum.
pub fn run_retention_blocking(
    db: &LedgerDb,
    config: &LedgerConfig,
    now: i64,
    budgeted: &[BudgetedMember],
) -> Result<Purged, LedgerError> {
    let cutoffs = Cutoffs::of(config, now);
    let mut purged = Purged::default();
    for family in Family::ALL {
        loop {
            let slice = purge_slice(db, family, &cutoffs, budgeted, now)?;
            purged.add(&slice.purged);
            if !slice.more {
                break;
            }
        }
    }
    Ok(purged)
}

/// The bodies slice, ordered by the cutoff column the expiry index covers with `rowid` breaking ties: `ORDER BY rowid`
/// alone plans as a scan of the table whatever index the filter has, and the slice would walk every row for its
/// oldest thousand.
pub(crate) const PURGE_BODIES: &str = "DELETE FROM bodies WHERE rowid IN (SELECT rowid FROM bodies WHERE expires_at <= ?1 ORDER BY expires_at, rowid LIMIT ?2)";

/// The bodies `PURGE_BODIES` takes next, in its order, with their request rows' request counts and the stored
/// responses' lengths. The join reads the bodies record's first column and the request row by its key, and SQLite
/// answers `length()` of a blob from the record's header, so no blob is read: the record's own `request_len_seen` sits
/// behind both blobs, and reading it would walk each row's overflow pages under the lock the budget exists to shorten.
/// The request row's `response_bytes` counts what the caller received, while the bodies row keeps what the bus
/// delivered up to the capture's bound, so a stored response counts at its own length.
pub(crate) const EXPIRED_BODY_BYTES: &str = "SELECT r.id IS NOT NULL, r.request_bytes, length(b.response_body) FROM bodies b LEFT JOIN requests r ON r.id = b.request_id WHERE b.expires_at <= ?1 ORDER BY b.expires_at, b.rowid LIMIT ?2";

/// What one body frees: the request as its row counts it, received whole since the capture never cuts it, at
/// `MAX_CAPTURED_BODY` where the count is null, and the stored response at its length, 0 where none was stored. A body
/// whose request row is gone counts the whole budget, since no smaller figure bounds a request blob that was never
/// cut.
fn body_bytes(request_row: bool, request: Option<i64>, response: Option<i64>) -> usize {
    if !request_row {
        return PURGE_SLICE_BYTES;
    }
    let request = request
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(MAX_CAPTURED_BODY);
    let response = response
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(0);
    request.saturating_add(response)
}

/// How many of the bodies `PURGE_BODIES` takes next fit the byte budget, at least one, and whether a body read past
/// them was left outside it.
fn bodies_within_budget(tx: &Transaction<'_>, now: i64) -> Result<(usize, bool), LedgerError> {
    let mut stmt = tx.prepare(EXPIRED_BODY_BYTES)?;
    let mut rows = stmt.query(params![now, PURGE_SLICE as i64])?;
    let (mut within, mut bytes) = (0, 0usize);
    while let Some(row) = rows.next()? {
        bytes = bytes.saturating_add(body_bytes(row.get(0)?, row.get(1)?, row.get(2)?));
        if within > 0 && bytes > PURGE_SLICE_BYTES {
            return Ok((within, true));
        }
        within += 1;
    }
    Ok((within, false))
}

/// The attempts of the requests `PURGE_REQUESTS` selects next in the same transaction, so a request row never
/// outlives its attempts or the other way round. Ordered as the bodies slice is, by the received index.
pub(crate) const PURGE_ATTEMPTS: &str = "DELETE FROM attempts WHERE request_id IN (SELECT id FROM requests WHERE received_at < ?1 ORDER BY received_at, rowid LIMIT ?2)";

/// The guardrail matches of the requests `PURGE_REQUESTS` selects next in the same transaction, so no match outlives
/// its request. The same slice as the attempts'. The primary key is named: once statistics count few matches, SQLite
/// prices a walk of every match below a lookup per request, and a table grown past its statistics would then be walked
/// whole on every slice.
pub(crate) const PURGE_REQUEST_GUARDRAILS: &str = "DELETE FROM request_guardrails INDEXED BY sqlite_autoindex_request_guardrails_1 WHERE request_id IN (SELECT id FROM requests WHERE received_at < ?1 ORDER BY received_at, rowid LIMIT ?2)";

pub(crate) const PURGE_REQUESTS: &str = "DELETE FROM requests WHERE rowid IN (SELECT rowid FROM requests WHERE received_at < ?1 ORDER BY received_at, rowid LIMIT ?2)";

/// Under the requests' cutoff and after their family, so a decision leaves in the pass that takes its request, which
/// was received before it was decided. One whose request never landed leaves at the same age by its own stamp.
pub(crate) const PURGE_DECISIONS: &str = "DELETE FROM decisions WHERE rowid IN (SELECT rowid FROM decisions WHERE at < ?1 ORDER BY at, rowid LIMIT ?2)";

/// One transaction under the purger over at most `PURGE_SLICE` rows of one family, selected by rowid, a bodies slice
/// freeing about `PURGE_SLICE_BYTES` with the stored response counted at its length, a body above it alone in its
/// slice, so the writer waits behind one slice at most and the slice holds the lock for a fraction of the busy timeout.
fn purge_slice(
    db: &LedgerDb,
    family: Family,
    cutoffs: &Cutoffs,
    budgeted: &[BudgetedMember],
    now: i64,
) -> Result<Slice, LedgerError> {
    db.purge(|conn| {
        let tx = begin_write(conn)?;
        let slice = PURGE_SLICE as i64;
        let cut = |sql: &str, cutoff: Option<i64>| -> Result<usize, LedgerError> {
            match cutoff {
                Some(at) => Ok(tx.execute(sql, params![at, slice])?),
                None => Ok(0),
            }
        };
        // A bodies slice that stopped at the byte budget left rows it read, so its family repeats short of a full
        // slice.
        let mut over_budget = false;
        let deleted = match family {
            Family::Bodies => {
                let (within, left_outside) = bodies_within_budget(&tx, now)?;
                over_budget = left_outside;
                Purged {
                    bodies: tx.execute(PURGE_BODIES, params![now, within as i64])?,
                    ..Purged::default()
                }
            }
            Family::Requests => match cutoffs.request {
                None => Purged::default(),
                Some(at) => {
                    tx.execute(PURGE_REQUEST_GUARDRAILS, params![at, slice])?;
                    let attempts = tx.execute(PURGE_ATTEMPTS, params![at, slice])?;
                    let requests = tx.execute(PURGE_REQUESTS, params![at, slice])?;
                    Purged {
                        attempts,
                        requests,
                        ..Purged::default()
                    }
                }
            },
            // No index leads with the marks' or the samples' cutoff column, so their slices take the first
            // matches the table yields: an `ORDER BY` would read and sort every match under the write lock, once
            // per slice. The rollup tables' primary keys lead with the hour, so theirs come oldest first anyway.
            Family::KeptWarmMarks => Purged {
                requests: cut(
                    "DELETE FROM kept_warm_marks WHERE rowid IN (SELECT rowid FROM kept_warm_marks WHERE marked_at < ?1 LIMIT ?2)",
                    cutoffs.request,
                )?,
                ..Purged::default()
            },
            Family::Events => Purged {
                events: cut(
                    "DELETE FROM events WHERE rowid IN (SELECT rowid FROM events WHERE at < ?1 ORDER BY at, rowid LIMIT ?2)",
                    cutoffs.request,
                )?,
                ..Purged::default()
            },
            Family::Decisions => Purged {
                decisions: cut(PURGE_DECISIONS, cutoffs.request)?,
                ..Purged::default()
            },
            Family::QuotaSamples => Purged {
                quota_samples: cut(
                    "DELETE FROM quota_samples WHERE rowid IN (SELECT rowid FROM quota_samples WHERE bucket < ?1 LIMIT ?2)",
                    cutoffs.sample,
                )?,
                ..Purged::default()
            },
            Family::ExtraUsageSamples => Purged {
                extra_usage_samples: cut(
                    "DELETE FROM extra_usage_samples WHERE rowid IN (SELECT rowid FROM extra_usage_samples WHERE bucket < ?1 LIMIT ?2)",
                    cutoffs.sample,
                )?,
                ..Purged::default()
            },
            Family::Rollups => Purged {
                rollups: purge_hourly_rollups(&tx, cutoffs.rollup, budgeted, now, PURGE_SLICE)?,
                ..Purged::default()
            },
            Family::AccountRollups => Purged {
                rollups: cut(
                    "DELETE FROM account_rollups_hourly WHERE rowid IN (SELECT rowid FROM account_rollups_hourly WHERE hour < ?1 LIMIT ?2)",
                    cutoffs.rollup,
                )?,
                ..Purged::default()
            },
        };
        tx.commit()?;
        Ok(Slice {
            more: over_budget || family.selected(&deleted) >= PURGE_SLICE,
            purged: deleted,
        })
    })
}

/// A budget sums its member's hourly rows over the whole active period, so a retention shorter than the period
/// would reopen an exhausted member before the period ends: a budgeted member's rows from its period's start on
/// stay, and the first purge past the period's end takes them; a member with both budgets keeps its rows from the
/// earlier start. Deletes at most `slice` rows, the clause inside the slice's selection so no slice takes a kept
/// row.
fn purge_hourly_rollups(
    tx: &Transaction<'_>,
    cutoff: Option<i64>,
    budgeted: &[BudgetedMember],
    now: i64,
    slice: usize,
) -> Result<usize, LedgerError> {
    let Some(cutoff) = cutoff else {
        return Ok(0);
    };
    if budgeted.is_empty() {
        return Ok(tx.execute(
            "DELETE FROM rollups_hourly WHERE rowid IN (SELECT rowid FROM rollups_hourly WHERE hour < ?1 LIMIT ?2)",
            params![cutoff, slice as i64],
        )?);
    }
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(cutoff), Box::new(slice as i64)];
    let values: Vec<String> = budgeted
        .iter()
        .map(|m| {
            args.push(Box::new(m.pool_id.clone()));
            args.push(Box::new(m.account_id.clone()));
            args.push(Box::new(period_bounds(m.period, now).0 / 1_000));
            let n = args.len();
            format!("(?{}, ?{}, ?{n})", n - 2, n - 1)
        })
        .collect();
    let sql = format!(
        "WITH budgeted(pool_id, account_id, since) AS (VALUES {}) \
         DELETE FROM rollups_hourly WHERE rowid IN (SELECT rowid FROM rollups_hourly WHERE hour < ?1 \
         AND NOT EXISTS (SELECT 1 FROM budgeted b WHERE b.pool_id = rollups_hourly.pool_id \
         AND b.account_id = rollups_hourly.account_id AND rollups_hourly.hour >= b.since) LIMIT ?2)",
        values.join(", ")
    );
    Ok(tx.execute(&sql, params_from_iter(args.iter().map(|a| a.as_ref())))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan lines SQLite reads for `sql` with both parameters bound.
    fn plan(db: &LedgerDb, sql: &str) -> Vec<String> {
        db.read_blocking(|c| {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let lines = stmt
                .query_map(params![0, 1_000], |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(lines)
        })
        .unwrap()
    }

    #[test]
    fn each_slice_seeks_its_cutoff_index_and_neither_walks_nor_sorts_a_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        for (sql, index) in [
            (PURGE_BODIES, "INDEX bodies_expires"),
            (EXPIRED_BODY_BYTES, "INDEX bodies_expires"),
            (PURGE_ATTEMPTS, "INDEX requests_received"),
            (PURGE_REQUESTS, "INDEX requests_received"),
            (PURGE_DECISIONS, "INDEX decisions_at"),
        ] {
            let lines = plan(&db, sql);
            assert!(
                lines.iter().any(|l| l.contains(index)),
                "{sql} seeks {index}: {lines:?}"
            );
            assert!(
                !lines
                    .iter()
                    .any(|l| l.contains("SCAN") || l.contains("TEMP B-TREE")),
                "{sql}: {lines:?}"
            );
        }
        let lines = plan(&db, EXPIRED_BODY_BYTES);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("SEARCH r USING INDEX sqlite_autoindex_requests_1")),
            "the byte read finds each request row by its key: {lines:?}"
        );
    }

    const NOW: i64 = 1_000;

    fn ledger() -> (tempfile::TempDir, LedgerDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        (dir, db)
    }

    /// What every seeded request row carries as `response_bytes`: a compressed count, which the bound never reads.
    const COMPRESSED: i64 = 1_024;

    /// One seeded body: its request id, its request row's `request_bytes` (`None` for no row, `Some(None)` for a null
    /// count) and the length of its stored response blob (`None` for none stored).
    type Seeded<'a> = (&'a str, Option<Option<usize>>, Option<usize>);

    /// Bodies expired before `NOW` whatever their number, oldest first, each written as its `Seeded` names it. The
    /// request blob holds two bytes whatever its row counts, since the bound reads the row's count for it.
    fn expired(db: &LedgerDb, bodies: &[Seeded<'_>]) {
        db.write(|c| {
            let tx = c.transaction()?;
            for (at, (id, request, response)) in bodies.iter().enumerate() {
                if let Some(request) = request {
                    tx.execute(
                        "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, \
                         outcome, attempts, request_bytes, response_bytes, bodies_captured) \
                         VALUES (?1, 0, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1, ?2, ?3, 1)",
                        params![id, request.map(|bytes| bytes as i64), COMPRESSED],
                    )?;
                }
                tx.execute(
                    "INSERT INTO bodies (request_id, request_body, response_body, expires_at) \
                     VALUES (?1, x'7b7d', CASE WHEN ?2 IS NULL THEN NULL ELSE zeroblob(?2) END, ?3)",
                    params![
                        id,
                        response.map(|bytes| bytes as i64),
                        NOW - (bodies.len() - at) as i64
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    }

    fn stored(db: &LedgerDb) -> Vec<String> {
        db.read_blocking(|c| {
            let mut stmt = c.prepare("SELECT request_id FROM bodies ORDER BY expires_at, rowid")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(ids)
        })
        .unwrap()
    }

    /// One bodies slice at `NOW`: the bodies it took, oldest first, and whether it says more wait.
    fn take(db: &LedgerDb) -> (Vec<String>, bool) {
        let before = stored(db);
        let cutoffs = Cutoffs::of(&LedgerConfig::default(), NOW);
        let slice = purge_slice(db, Family::Bodies, &cutoffs, &[], NOW).unwrap();
        let after = stored(db);
        let taken: Vec<String> = before
            .into_iter()
            .filter(|id| !after.contains(id))
            .collect();
        assert_eq!(
            slice.purged.bodies,
            taken.len(),
            "the count is the bodies it took"
        );
        (taken, slice.more)
    }

    fn owned(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_bodies_slice_takes_the_longest_prefix_within_the_byte_budget_and_the_next_slice_the_rest()
    {
        let (_dir, db) = ledger();
        let quarter = PURGE_SLICE_BYTES / 4;
        // The first three fill the budget to the byte, so the fourth waits however small it is.
        expired(
            &db,
            &[
                ("half", Some(Some(2 * quarter)), None),
                ("quarter-1", Some(Some(quarter)), None),
                ("quarter-2", Some(Some(quarter)), None),
                ("byte-1", Some(Some(1)), None),
                ("byte-2", Some(Some(1)), None),
            ],
        );
        assert_eq!(
            take(&db),
            (owned(&["half", "quarter-1", "quarter-2"]), true),
            "the prefix within the budget, with rows left outside it"
        );
        assert_eq!(
            take(&db),
            (owned(&["byte-1", "byte-2"]), false),
            "the rest, and none left"
        );
    }

    #[test]
    fn a_bodies_slice_takes_purge_slice_rows_and_no_more_however_few_their_bytes() {
        let (_dir, db) = ledger();
        let ids: Vec<String> = (0..PURGE_SLICE + 5).map(|i| format!("b{i:04}")).collect();
        let bodies: Vec<_> = ids
            .iter()
            .map(|id| (id.as_str(), Some(Some(3)), Some(3)))
            .collect();
        expired(&db, &bodies);
        assert_eq!(take(&db), (ids[..PURGE_SLICE].to_vec(), true));
        assert_eq!(stored(&db), ids[PURGE_SLICE..]);
    }

    #[test]
    fn a_body_above_the_byte_budget_leaves_alone_in_a_slice_of_its_own() {
        let (_dir, db) = ledger();
        expired(
            &db,
            &[
                ("small-1", Some(Some(1)), None),
                ("large", Some(Some(PURGE_SLICE_BYTES + 1)), None),
                ("small-2", Some(Some(1)), None),
                ("small-3", Some(Some(1)), None),
            ],
        );
        assert_eq!(take(&db), (owned(&["small-1"]), true));
        assert_eq!(take(&db), (owned(&["large"]), true));
        assert_eq!(take(&db), (owned(&["small-2", "small-3"]), false));
    }

    #[test]
    fn a_body_whose_request_row_is_gone_leaves_alone_in_a_slice_of_its_own_and_is_still_taken() {
        let (_dir, db) = ledger();
        expired(
            &db,
            &[
                ("small-1", Some(Some(1)), None),
                ("orphan", None, None),
                ("small-2", Some(Some(1)), None),
            ],
        );
        assert_eq!(take(&db), (owned(&["small-1"]), true));
        assert_eq!(take(&db), (owned(&["orphan"]), true));
        assert_eq!(take(&db), (owned(&["small-2"]), false));
    }

    #[test]
    fn a_response_counts_at_its_stored_length_whatever_its_request_row_says_the_caller_received() {
        let (_dir, db) = ledger();
        // Each body frees 1 KiB of request and a response stored at the cap, which its request row counts at the
        // compressed 1 KiB: three take 24 MiB and 3 KiB of the budget, and a fourth would pass it.
        let ids = ["full-1", "full-2", "full-3", "full-4", "full-5"];
        let bodies: Vec<_> = ids
            .iter()
            .map(|id| (*id, Some(Some(1_024)), Some(MAX_CAPTURED_BODY)))
            .collect();
        expired(&db, &bodies);
        assert_eq!(take(&db), (owned(&ids[..3]), true));
        assert_eq!(take(&db), (owned(&ids[3..]), false));
    }

    #[test]
    fn a_null_request_count_counts_the_capture_cap_and_a_response_never_stored_counts_nothing() {
        // The null count fills the budget to the byte with the fill, so the tail waits for the next slice.
        let (_dir, db) = ledger();
        expired(
            &db,
            &[
                ("null-request", Some(None), None),
                (
                    "fill",
                    Some(Some(PURGE_SLICE_BYTES - MAX_CAPTURED_BODY)),
                    None,
                ),
                ("tail", Some(Some(1)), None),
            ],
        );
        assert_eq!(take(&db), (owned(&["null-request", "fill"]), true));
        assert_eq!(take(&db), (owned(&["tail"]), false));
        // Two requests that fill the budget to the byte go in one slice, since neither stored a response.
        let (_dir, db) = ledger();
        expired(
            &db,
            &[
                ("no-response", Some(Some(PURGE_SLICE_BYTES - 1)), None),
                ("byte", Some(Some(1)), None),
            ],
        );
        assert_eq!(take(&db), (owned(&["no-response", "byte"]), false));
    }
}
