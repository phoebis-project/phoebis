//! Captured bodies, kept only for keys whose effective capture window is above zero, and the hourly purge
//! that enforces every retention window the ledger has.

use phoebis_store::BudgetPeriod;
use rusqlite::{params, params_from_iter, ToSql};

use crate::query::period_bounds;
use crate::task::LedgerConfig;
use crate::{LedgerDb, LedgerError};

/// A stream longer than this is truncated in the capture; the ledger row still says the body was captured,
/// and the truncation is the price of a bounded in-flight table.
pub const MAX_CAPTURED_BODY: usize = 8 << 20;

const DAY_MS: i64 = 86_400_000;
const DAY_S: i64 = 86_400;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub attempts: usize,
    pub bodies: usize,
    pub events: usize,
    pub extra_usage_samples: usize,
    pub quota_samples: usize,
    pub reconciliations: usize,
    pub requests: usize,
    pub rollups: usize,
}

/// `budgeted` is every pool member with a spend budget, as `member_spends` takes them.
pub fn run_retention(
    db: &LedgerDb,
    config: &LedgerConfig,
    now: i64,
    budgeted: &[(String, String, BudgetPeriod)],
) -> Result<Purged, LedgerError> {
    db.write(|conn| {
        let tx = conn.transaction()?;
        // Zero is "keep forever": the cutoff is `None` and the purge of that family is skipped rather than run
        // against a cutoff at the epoch.
        let request_cutoff = (config.retention_days != 0).then(|| now - config.retention_days as i64 * DAY_MS);
        let rollup_cutoff = (config.rollup_retention_days != 0)
            .then(|| now / 1_000 - config.rollup_retention_days as i64 * DAY_S);
        let sample_cutoff = now / 1_000 - config.sample_retention_days as i64 * DAY_S;
        let purge = |sql: &str, cutoff: Option<i64>| -> Result<usize, LedgerError> {
            match cutoff {
                Some(at) => Ok(tx.execute(sql, params![at])?),
                None => Ok(0),
            }
        };
        let purged = Purged {
            bodies: tx.execute("DELETE FROM bodies WHERE expires_at <= ?1", params![now])?,
            attempts: purge(
                "DELETE FROM attempts WHERE request_id IN (SELECT id FROM requests WHERE received_at < ?1)",
                request_cutoff,
            )?,
            requests: purge("DELETE FROM requests WHERE received_at < ?1", request_cutoff)?
                + purge("DELETE FROM kept_warm_marks WHERE marked_at < ?1", request_cutoff)?,
            events: purge("DELETE FROM events WHERE at < ?1", request_cutoff)?,
            quota_samples: tx.execute("DELETE FROM quota_samples WHERE bucket < ?1", params![sample_cutoff])?,
            extra_usage_samples: tx
                .execute("DELETE FROM extra_usage_samples WHERE bucket < ?1", params![sample_cutoff])?,
            rollups: purge_hourly_rollups(&tx, rollup_cutoff, budgeted, now)?
                + purge("DELETE FROM account_rollups_hourly WHERE hour < ?1", rollup_cutoff)?,
            // Kept as long as the rollups, the ledger's other per-window aggregate: the requests they were
            // computed from may be gone sooner than the analytics read them.
            reconciliations: purge(
                "DELETE FROM window_reconciliations WHERE computed_at < ?1",
                rollup_cutoff.map(|at| at * 1_000),
            )?,
        };
        tx.commit()?;
        Ok(purged)
    })
}

/// A budget sums its member's hourly rows over the whole active period, so a retention shorter than the period
/// would reopen an exhausted member before the period ends: a budgeted member's rows from its period's start on
/// stay, and the first purge past the period's end takes them.
fn purge_hourly_rollups(
    tx: &rusqlite::Transaction<'_>,
    cutoff: Option<i64>,
    budgeted: &[(String, String, BudgetPeriod)],
    now: i64,
) -> Result<usize, LedgerError> {
    let Some(cutoff) = cutoff else {
        return Ok(0);
    };
    if budgeted.is_empty() {
        return Ok(tx.execute(
            "DELETE FROM rollups_hourly WHERE hour < ?1",
            params![cutoff],
        )?);
    }
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(cutoff)];
    let values: Vec<String> = budgeted
        .iter()
        .map(|(pool_id, account_id, period)| {
            args.push(Box::new(pool_id.clone()));
            args.push(Box::new(account_id.clone()));
            args.push(Box::new(period_bounds(*period, now).0 / 1_000));
            let n = args.len();
            format!("(?{}, ?{}, ?{n})", n - 2, n - 1)
        })
        .collect();
    let sql = format!(
        "WITH budgeted(pool_id, account_id, since) AS (VALUES {}) \
         DELETE FROM rollups_hourly WHERE hour < ?1 AND NOT EXISTS (SELECT 1 FROM budgeted b \
         WHERE b.pool_id = rollups_hourly.pool_id AND b.account_id = rollups_hourly.account_id \
         AND rollups_hourly.hour >= b.since)",
        values.join(", ")
    );
    Ok(tx.execute(&sql, params_from_iter(args.iter().map(|a| a.as_ref())))?)
}
