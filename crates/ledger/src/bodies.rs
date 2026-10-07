//! Captured bodies, kept only for keys whose effective capture window is above zero, and the hourly purge
//! that enforces every retention window the ledger has, in slices on its own connection.

use std::sync::Arc;

use rusqlite::{params, params_from_iter, ToSql, Transaction};

use crate::db::begin_write;
use crate::query::{period_bounds, BudgetedMember};
use crate::task::LedgerConfig;
use crate::{LedgerDb, LedgerError};

/// A stream longer than this is truncated in the capture; the ledger row still says the body was captured,
/// and the truncation is the price of a bounded in-flight table.
pub const MAX_CAPTURED_BODY: usize = 8 << 20;

/// Rows one purge transaction deletes at most, per family: a slice holds the writer lock for milliseconds, so
/// the writer's batch waits behind a slice and never behind a whole pass, and the WAL grows by a slice's pages.
pub const PURGE_SLICE: usize = 1_000;

const DAY_MS: i64 = 86_400_000;
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

/// The pass on the calling thread: every family in slices, a family repeated while a slice came up full; the sum.
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
            purged.add(&slice);
            if family.selected(&slice) < PURGE_SLICE {
                break;
            }
        }
    }
    Ok(purged)
}

/// The bodies slice, ordered by the cutoff column the expiry index covers with `rowid` breaking ties: `ORDER BY rowid`
/// alone plans as a scan of the table whatever index the filter has, and the slice would walk every row for its
/// oldest thousand.
const PURGE_BODIES: &str = "DELETE FROM bodies WHERE rowid IN (SELECT rowid FROM bodies WHERE expires_at <= ?1 ORDER BY expires_at, rowid LIMIT ?2)";

/// The attempts of the requests `PURGE_REQUESTS` selects next in the same transaction, so a request row never
/// outlives its attempts or the other way round. Ordered as the bodies slice is, by the received index.
const PURGE_ATTEMPTS: &str = "DELETE FROM attempts WHERE request_id IN (SELECT id FROM requests WHERE received_at < ?1 ORDER BY received_at, rowid LIMIT ?2)";

const PURGE_REQUESTS: &str = "DELETE FROM requests WHERE rowid IN (SELECT rowid FROM requests WHERE received_at < ?1 ORDER BY received_at, rowid LIMIT ?2)";

/// Under the requests' cutoff and after their family, so a decision leaves in the pass that takes its request, which
/// was received before it was decided. One whose request never landed leaves at the same age by its own stamp.
const PURGE_DECISIONS: &str = "DELETE FROM decisions WHERE rowid IN (SELECT rowid FROM decisions WHERE at < ?1 ORDER BY at, rowid LIMIT ?2)";

/// One transaction under the purger over at most `PURGE_SLICE` rows of one family, selected by rowid.
fn purge_slice(
    db: &LedgerDb,
    family: Family,
    cutoffs: &Cutoffs,
    budgeted: &[BudgetedMember],
    now: i64,
) -> Result<Purged, LedgerError> {
    db.purge(|conn| {
        let tx = begin_write(conn)?;
        let slice = PURGE_SLICE as i64;
        let cut = |sql: &str, cutoff: Option<i64>| -> Result<usize, LedgerError> {
            match cutoff {
                Some(at) => Ok(tx.execute(sql, params![at, slice])?),
                None => Ok(0),
            }
        };
        let deleted = match family {
            Family::Bodies => Purged {
                bodies: tx.execute(PURGE_BODIES, params![now, slice])?,
                ..Purged::default()
            },
            Family::Requests => match cutoffs.request {
                None => Purged::default(),
                Some(at) => {
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
        Ok(deleted)
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
    }
}
