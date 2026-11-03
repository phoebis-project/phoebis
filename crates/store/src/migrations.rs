//! The schema runner the registry and the ledger share, so a change to how a step commits, or to the crash case of
//! the version row, lands in one place for both files.

use rusqlite::{params, Connection, OptionalExtension};

/// Runs `baseline`, the schema at version 1, then applies every step above `current` (read as at least 1) in order,
/// each inside its own transaction with the version it reaches, so a crash or a failing step leaves the version at
/// the last step that committed. The caller refuses a file a newer build wrote before it calls this.
pub fn apply(
    conn: &mut Connection,
    baseline: &str,
    current: u32,
    steps: &[(u32, &str)],
) -> rusqlite::Result<()> {
    // A fresh file carries its version row as a stepped one does, so an `UPDATE` of the marker finds a row to change.
    // A version read that takes a `meta` without its row for the baseline would read a file a crash left between the
    // two as prepared, and the file would never get its row: the two share one transaction, and any open that finds
    // the row absent writes it. A row already there is kept, and an open that finds it writes nothing, so it lands
    // beside a connection holding the write lock.
    let tx = conn.transaction()?;
    tx.execute_batch(baseline)?;
    let marked = tx
        .query_row(
            "SELECT 1 FROM meta WHERE key = 'schema_version'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !marked {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
    }
    tx.commit()?;
    let current = current.max(1);
    for (version, sql) in steps {
        if *version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![version.to_string()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// The stored `schema_version`. A connection that has never run `apply` has no `meta` table yet, which reads as
/// version 0, so the caller's refusal of a newer file runs before the baseline creates the table. A `meta` without
/// the row, or with a value that does not parse, reads as `without_row`, which each caller picks for its own file.
pub fn stored_version(conn: &Connection, without_row: u32) -> rusqlite::Result<u32> {
    if !has_table(conn, "meta")? {
        return Ok(0);
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.and_then(|v| v.parse().ok()).unwrap_or(without_row))
}

pub(crate) fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SCHEMA;

    /// A step that creates a table and one that alters it, so a step applied twice or out of order fails.
    const CHAIN: &[(u32, &str)] = &[
        (2, "CREATE TABLE chain_probe (id INTEGER PRIMARY KEY);"),
        (
            3,
            "ALTER TABLE chain_probe ADD COLUMN word TEXT NOT NULL DEFAULT 'x';",
        ),
    ];

    /// The baseline with no step above it, so the chain tests start at version 1 whatever the build's own chain holds.
    fn prepared_in_memory() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, SCHEMA, 0, &[]).unwrap();
        conn
    }

    fn word_columns(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('chain_probe') WHERE name = 'word'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_chain_of_two_steps_applies_in_order_and_records_the_last_version() {
        let mut conn = prepared_in_memory();
        assert_eq!(stored_version(&conn, 1).unwrap(), 1);
        apply(&mut conn, SCHEMA, 1, CHAIN).unwrap();
        assert_eq!(stored_version(&conn, 1).unwrap(), 3);
        assert_eq!(word_columns(&conn), 1);
    }

    #[test]
    fn a_chain_resumes_above_the_stored_version_and_repeats_nothing() {
        let mut conn = prepared_in_memory();
        apply(&mut conn, SCHEMA, 1, &CHAIN[..1]).unwrap();
        assert_eq!(stored_version(&conn, 1).unwrap(), 2);
        apply(&mut conn, SCHEMA, 2, CHAIN).unwrap();
        assert_eq!(stored_version(&conn, 1).unwrap(), 3);
        assert_eq!(word_columns(&conn), 1);
        apply(&mut conn, SCHEMA, 3, CHAIN).unwrap();
        assert_eq!(stored_version(&conn, 1).unwrap(), 3);
        assert_eq!(word_columns(&conn), 1);
    }

    #[test]
    fn a_failing_step_leaves_the_version_at_the_last_step_that_landed() {
        let mut conn = prepared_in_memory();
        let error: rusqlite::Error = apply(
            &mut conn,
            SCHEMA,
            1,
            &[(2, CHAIN[0].1), (3, "THIS IS NOT SQL;")],
        )
        .unwrap_err();
        assert_eq!(stored_version(&conn, 1).unwrap(), 2, "{error}");
        assert!(has_table(&conn, "chain_probe").unwrap());
    }

    /// `meta` alone, from the baseline's own `CREATE TABLE` for it.
    fn meta_alone() -> Connection {
        let ddl = SCHEMA
            .lines()
            .find(|line| line.starts_with("CREATE TABLE IF NOT EXISTS meta "))
            .expect("the baseline creates meta");
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        conn
    }

    fn raw_markers(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT value FROM meta WHERE key = 'schema_version'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn an_open_that_finds_meta_without_its_marker_writes_the_version_row() {
        // What a first open that died between the baseline and its marker left behind.
        let mut conn = meta_alone();
        apply(&mut conn, SCHEMA, 1, &[]).unwrap();
        assert_eq!(raw_markers(&conn), vec!["1".to_string()]);
    }

    #[test]
    fn an_open_that_finds_its_marker_keeps_it_and_adds_no_second_row() {
        let mut conn = meta_alone();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
        apply(&mut conn, SCHEMA, 1, &[]).unwrap();
        assert_eq!(raw_markers(&conn), vec!["1".to_string()]);
    }
}
