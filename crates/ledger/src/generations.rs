//! The one-time stamp of live keys' generations onto rows written before the rollup key carried one.

use rusqlite::{params, OptionalExtension};

use crate::query::hour_ceil;
use crate::rollup::{
    delete_rollup, read_rollup, read_rollups_of_key_generation, write_rollup, RollupRow,
};
use crate::{LedgerDb, LedgerError};

/// The `meta` key that says the generation-0 rows were stamped once.
pub const GENERATIONS_RECONCILED_KEY: &str = "key_generations_reconciled";

/// How many generation-0 rows one transaction holds in memory: the fold reads a page, moves it, and reads the
/// next from where it left off, so a key with a year of hourly rows never sits in one `Vec`.
const PAGE: usize = 1_000;

/// One live key as the startup stamp sees it: the id, its `created_at`, and where its own rows start. `floor`
/// is `Some(created_at)` for an id in `RegistryLoad.reused_key_ids` (an earlier holder's rows sit at generation
/// 0 under the same id, so only rows at or after `hour_ceil(created_at)` are this key's) and `None` for an id
/// whose holder is its first (every generation-0 row is this key's).
#[derive(Clone, Debug)]
pub struct LiveKeyLifetime {
    pub id: String,
    pub created_at: i64,
    pub floor: Option<i64>,
}

/// Stamps each live key's generation onto the generation-0 rollup and request rows of its own lifetime, once
/// per ledger. Answers how many rollup rows moved; 0 on every later start.
///
/// The ledger cannot read the store, so the migration that added the column left every row at 0 and the binary
/// brings the live keys here once both handles are open. Runs before the ledger task writes anything, so the
/// primary key may already hold a row under the live generation (folded rather than overwritten, see below).
/// One transaction per key and the `meta` mark after the last: a start that fails between keys re-runs the
/// remaining keys next time, and a re-run of a finished key finds no generation-0 row of its lifetime to move,
/// so nothing folds twice.
pub fn reconcile_key_generations(
    db: &LedgerDb,
    keys: &[LiveKeyLifetime],
) -> Result<usize, LedgerError> {
    db.write(|conn| {
        let done: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![GENERATIONS_RECONCILED_KEY],
                |r| r.get(0),
            )
            .optional()?;
        if done.is_some() {
            return Ok(0);
        }
        let mut moved = 0;
        for key in keys {
            // An id an earlier holder used before ids were retired carries that holder's rows at generation 0
            // too; the current holder's own rows start at its first whole hour, the creation hour left out
            // whole as the usage floor left it, since nothing in a row says which holder filed it.
            let from_hour = key.floor.map(hour_ceil);
            let tx = conn.transaction()?;
            let mut after_rowid = 0;
            loop {
                let page =
                    read_rollups_of_key_generation(&tx, &key.id, 0, from_hour, after_rowid, PAGE)?;
                let Some((last_rowid, _)) = page.last() else {
                    break;
                };
                after_rowid = *last_rowid;
                for (_, row) in &page {
                    // Folded rather than updated in place: the new binary may already have filed a row under
                    // the live generation with the same other parts, and the primary key admits no second.
                    let stamped = RollupRow {
                        key_generation: key.created_at,
                        ..row.clone()
                    };
                    let target = match read_rollup(&tx, &stamped)? {
                        Some(mut existing) => {
                            existing.add(row);
                            existing
                        }
                        None => stamped,
                    };
                    write_rollup(&tx, &target)?;
                    delete_rollup(&tx, row)?;
                    moved += 1;
                }
            }
            tx.execute(
                "UPDATE requests SET key_generation = ?2 WHERE key_id = ?1 AND key_generation = 0 AND received_at >= ?3",
                params![key.id, key.created_at, from_hour.map_or(i64::MIN, |h| h * 1_000)],
            )?;
            tx.commit()?;
        }
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '1')",
            params![GENERATIONS_RECONCILED_KEY],
        )?;
        Ok(moved)
    })
}
