//! The test tables: a subscription cost model and a price file in round numbers, so a test works its expected
//! figure out by hand from the file rather than reading a shipped value back. Embedded, so a test in another crate
//! reads them without a path. Each carries two revisions, the second a factor of two from the first, so a test
//! crosses a boundary without writing a file.

use crate::{CostRevisions, PlanTables, PriceRevisions};

pub const SUBSCRIPTION_COSTS: &str = include_str!("../tests/fixtures/subscription_costs.yaml");
pub const API_PRICES: &str = include_str!("../tests/fixtures/api_prices.yaml");

/// A cost model in round numbers for the share cases: one Fable input token weighs 10 units, `test_tier` holds 100
/// units on the 5-hour window and 1000 on the week, `narrow_tier` sizes the 5-hour window alone, and the Fable week
/// is half the 7-day window.
pub const SHARE_COSTS: &str = include_str!("../tests/fixtures/share_costs.yaml");

/// An instant inside the fixtures' first entry, 2026-01-15 12:00 UTC, for a test that wants the first entry's figures.
pub const JANUARY_2026: i64 = 1_768_478_400_000;

/// An instant inside the fixtures' second entries and out of band, Sunday 2026-02-15 12:00 UTC.
pub const FEBRUARY_2026: i64 = 1_771_156_800_000;

pub fn subscription_costs() -> PlanTables {
    PlanTables {
        revisions: CostRevisions::from_yaml(SUBSCRIPTION_COSTS)
            .expect("the test subscription cost model parses"),
        path: None,
        source: "test".to_string(),
    }
}

/// A test's own cost revisions as the tables a reader takes.
pub fn tables(revisions: CostRevisions) -> PlanTables {
    PlanTables {
        revisions,
        path: None,
        source: "test".to_string(),
    }
}

pub fn api_prices() -> PriceRevisions {
    PriceRevisions::from_yaml(API_PRICES, "test").expect("the test price file parses")
}

/// The cost fixture's first entry as a file of its own, for a test that reads at an instant past the second entry's
/// date and wants the first entry's figures, or writes them with one edited.
pub fn first_entry_costs_text() -> String {
    let (first, _) = SUBSCRIPTION_COSTS
        .split_once("  - effective: 2026-02-01\n")
        .expect("the cost fixture carries a second entry");
    first.to_string()
}

/// `first_entry_costs_text` loaded.
pub fn first_entry_costs() -> CostRevisions {
    CostRevisions::from_yaml(&first_entry_costs_text()).expect("the first cost entry parses alone")
}

/// The steps of `EXPLAIN QUERY PLAN` for `sql` under `binds` on `db`.
#[cfg(test)]
fn explain(db: &crate::LedgerDb, sql: &str, binds: Vec<rusqlite::types::Value>) -> Vec<String> {
    db.read_blocking(|c| {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .unwrap()
}

/// The plan of `sql` under `binds` on an empty ledger.
#[cfg(test)]
pub(crate) fn plan_of(sql: &str, binds: Vec<rusqlite::types::Value>) -> Vec<String> {
    explain(&crate::LedgerDb::open_in_memory().unwrap(), sql, binds)
}

/// `plan_of` on a ledger whose `sqlite_stat1` holds `rows` as (table, index, stat).
#[cfg(test)]
pub(crate) fn plan_under(
    sql: &str,
    binds: Vec<rusqlite::types::Value>,
    rows: &[(&str, &str, &str)],
) -> Vec<String> {
    let db = crate::LedgerDb::open_in_memory().unwrap();
    db.purge(|c| {
        for (tbl, idx, stat) in rows {
            c.execute(
                "INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES (?1, ?2, ?3)",
                rusqlite::params![tbl, idx, stat],
            )?;
        }
        // A connection keeps the statistics it loaded with its schema, and a row written by hand moves no schema
        // cookie, so the planner reads the new rows only once `ANALYZE sqlite_schema` reloads them.
        c.execute_batch("ANALYZE sqlite_schema")?;
        Ok(())
    })
    .unwrap();
    explain(&db, sql, binds)
}
