//! Captured bodies, kept only for keys whose effective capture window is above zero, and the hourly purge
//! that enforces every retention window the ledger has.

use rusqlite::params;

use crate::task::LedgerConfig;
use crate::{LedgerDb, LedgerError};

/// A stream longer than this is truncated in the capture; the ledger row still says the body was captured,
/// and the truncation is the price of a bounded in-flight table.
pub const MAX_CAPTURED_BODY: usize = 8 << 20;

pub const ROLLUP_RETENTION_DAYS: u32 = 400;

const DAY_MS: i64 = 86_400_000;
const DAY_S: i64 = 86_400;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub attempts: usize,
    pub bodies: usize,
    pub events: usize,
    pub extra_usage_samples: usize,
    pub quota_samples: usize,
    pub requests: usize,
    pub rollups: usize,
}

pub fn run_retention(
    db: &LedgerDb,
    config: &LedgerConfig,
    now: i64,
) -> Result<Purged, LedgerError> {
    db.write(|conn| {
        let tx = conn.transaction()?;
        let request_cutoff = now - config.retention_days as i64 * DAY_MS;
        let sample_cutoff = now / 1_000 - config.sample_retention_days as i64 * DAY_S;
        let rollup_cutoff = now / 1_000 - ROLLUP_RETENTION_DAYS as i64 * DAY_S;
        let purged = Purged {
            bodies: tx.execute("DELETE FROM bodies WHERE expires_at <= ?1", params![now])?,
            attempts: tx.execute(
                "DELETE FROM attempts WHERE request_id IN (SELECT id FROM requests WHERE received_at < ?1)",
                params![request_cutoff],
            )?,
            requests: tx.execute("DELETE FROM requests WHERE received_at < ?1", params![request_cutoff])?
                + tx.execute("DELETE FROM kept_warm_marks WHERE marked_at < ?1", params![request_cutoff])?,
            events: tx.execute("DELETE FROM events WHERE at < ?1", params![request_cutoff])?,
            quota_samples: tx.execute("DELETE FROM quota_samples WHERE bucket < ?1", params![sample_cutoff])?,
            extra_usage_samples: tx
                .execute("DELETE FROM extra_usage_samples WHERE bucket < ?1", params![sample_cutoff])?,
            rollups: tx.execute("DELETE FROM rollups_hourly WHERE hour < ?1", params![rollup_cutoff])?
                + tx.execute(
                    "DELETE FROM account_rollups_hourly WHERE hour < ?1",
                    params![rollup_cutoff],
                )?,
        };
        tx.commit()?;
        Ok(purged)
    })
}
