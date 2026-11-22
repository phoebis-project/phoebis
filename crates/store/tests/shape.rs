//! The tables, indexes and foreign keys of a fresh registry, held to `fixtures/registry_shape.txt`, so the baseline
//! `schema::SCHEMA` creates and every step after it change the shape only together with the fixture.
use phoebis_store::{StorageKey, Store};
use rusqlite::{Connection, OpenFlags};

/// The wall clock, for a case that reads no stamp.
fn wall() -> phoebis_store::Clock {
    std::sync::Arc::new(|| jiff::Timestamp::now().as_millisecond())
}

/// One space for every whitespace run and none beside `,`, `(` or `)`: a column an `ALTER TABLE` added sits in the
/// stored text after a line break and a comma, where a creating statement writes `, name TYPE`.
fn normalize(sql: &str) -> String {
    let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(collapsed.len());
    for c in collapsed.chars() {
        match c {
            ' ' if out.ends_with([',', '(', ')']) => {}
            ',' | '(' | ')' => {
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Every table, index, trigger and view by its stored text, and every foreign key, one sorted line each. The names
/// SQLite owns stay out: `sqlite_sequence` follows from the first `AUTOINCREMENT` table and the autoindexes from the
/// table text.
fn shape(conn: &Connection) -> String {
    let objects: Vec<(String, String, String)> = conn
        .prepare("SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut lines = Vec::new();
    for (kind, name, sql) in &objects {
        lines.push(format!("{kind} {name} {}", normalize(sql)));
        if kind != "table" {
            continue;
        }
        let keys: Vec<String> = conn
            .prepare(
                "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete \
                 FROM pragma_foreign_key_list(?1)",
            )
            .unwrap()
            .query_map([name], |row| {
                Ok(format!(
                    "fk {name} {} {} {} {} {} {} {}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?
                        .unwrap_or_else(|| "NULL".into()),
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        lines.extend(keys);
    }
    lines.sort();
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[test]
fn a_fresh_registry_has_the_shape_the_fixture_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &StorageKey::generate(), wall()).unwrap());
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let shape = shape(&conn);
    assert!(
        shape
            .lines()
            .any(|line| line == "fk credentials 0 0 accounts account_id id NO ACTION CASCADE"),
        "an account's credentials leave with the account row:\n{shape}"
    );
    assert_eq!(shape, include_str!("fixtures/registry_shape.txt"));
}
