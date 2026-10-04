//! Tier 2 of the relay's session memory: rows land and reload by key, a replace wins, the purge reads
//! `last_seen`, and an on-disk store keeps the rows across a reopen.
use phoebis_store::{MemoryKey, MemoryRecord, MemoryStore, StorageKey, Store};

fn key(agent: &str) -> MemoryKey {
    MemoryKey::for_session("desktop", 1_000, "s1", agent)
}

fn record(version: u32, payload: &str, last_seen: i64) -> MemoryRecord {
    MemoryRecord {
        record_version: version,
        payload: payload.into(),
        last_seen,
    }
}

#[test]
fn records_save_load_by_key_replace_and_purge_by_last_seen() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    assert_eq!(store.load(&key("main")).unwrap(), None);
    store
        .save(&[
            (key("main"), record(1, r#"{"prompt_id":"p1"}"#, 1_000)),
            (key("sub-1"), record(1, r#"{"prompt_id":"p2"}"#, 2_000)),
        ])
        .unwrap();
    assert_eq!(
        store.load(&key("main")).unwrap(),
        Some(record(1, r#"{"prompt_id":"p1"}"#, 1_000))
    );
    store
        .save(&[(key("main"), record(2, r#"{"prompt_id":"p3","x":1}"#, 3_000))])
        .unwrap();
    assert_eq!(
        store.load(&key("main")).unwrap(),
        Some(record(2, r#"{"prompt_id":"p3","x":1}"#, 3_000)),
        "a save replaces the row and keeps the version it was written under"
    );
    assert_eq!(
        store.purge(2_500).unwrap(),
        1,
        "the sub-agent's row, last seen at 2000"
    );
    assert_eq!(store.load(&key("sub-1")).unwrap(), None);
    assert!(store.load(&key("main")).unwrap().is_some());
}

#[test]
fn rows_outlive_a_reopen_of_the_on_disk_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    {
        let store = Store::open(&path, &storage_key).unwrap();
        store
            .save(&[(key("main"), record(1, "{}", 1_000))])
            .unwrap();
    }
    let store = Store::open(&path, &storage_key).unwrap();
    assert_eq!(
        store.load(&key("main")).unwrap(),
        Some(record(1, "{}", 1_000))
    );
    assert_eq!(store.schema_version().unwrap(), 44);
}

// The reverse of the registry's member budgets migration, so a store stamped back below it can run it again. One
// line with the marker last; the trailing `\n` ends the marker's comment.
const UNDO_V41: &str = "ALTER TABLE pool_members ADD COLUMN budget_usd REAL; ALTER TABLE pool_members ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'month'; UPDATE pool_members SET budget_usd = COALESCE(budget_monthly_usd, budget_daily_usd), budget_period = CASE WHEN budget_monthly_usd IS NULL AND budget_daily_usd IS NOT NULL THEN 'day' ELSE 'month' END; ALTER TABLE pool_members DROP COLUMN budget_monthly_usd; ALTER TABLE pool_members DROP COLUMN budget_daily_usd; -- rollback\n";
// The reverse of the registry's version 44, one statement per line; the request and response links it deleted are
// not restored.
const UNDO_V44: &str = r#"
DROP INDEX refresh_attempts_account;
ALTER TABLE api_keys DROP COLUMN updated_at;
"#;
// The reverse of the registry's version 43, so a store stamped back below it can run it again; the marker ends each
// line that restores a dropped name.
const UNDO_V43: &str = r#"
ALTER TABLE pools ADD COLUMN quota_headers TEXT NOT NULL DEFAULT 'pass'; -- rollback
ALTER TABLE api_keys ADD COLUMN in_flight_cap INTEGER; -- rollback
ALTER TABLE api_keys ADD COLUMN affinity_source TEXT NOT NULL DEFAULT 'auto'; -- rollback
CREATE TABLE alert_rules (id TEXT PRIMARY KEY, kind TEXT NOT NULL, scope_type TEXT NOT NULL, scope_id TEXT, threshold REAL, window TEXT, webhook_url TEXT NOT NULL, secret_sealed BLOB, secret_nonce BLOB, enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL); -- rollback
CREATE TABLE alert_events (id TEXT PRIMARY KEY, rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE, kind TEXT NOT NULL, subject TEXT NOT NULL, state TEXT NOT NULL CHECK (state IN ('open', 'resolved')), opened_at INTEGER NOT NULL, resolved_at INTEGER, detail_json TEXT); -- rollback
CREATE UNIQUE INDEX alert_events_open ON alert_events (rule_id, subject) WHERE state = 'open'; -- rollback
CREATE TABLE alert_deliveries (id INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT REFERENCES alert_events(id) ON DELETE CASCADE, rule_id TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL DEFAULT '', subject TEXT NOT NULL DEFAULT '', event TEXT, detail_json TEXT, state TEXT NOT NULL CHECK (state IN ('event', 'opened', 'resolved')), attempt INTEGER NOT NULL, at INTEGER NOT NULL, status INTEGER, error TEXT); -- rollback
CREATE INDEX alert_deliveries_event ON alert_deliveries (event_id, id) WHERE event_id IS NOT NULL; -- rollback
CREATE INDEX alert_deliveries_lifecycle_at ON alert_deliveries (at, id) WHERE event_id IS NULL; -- rollback
CREATE INDEX alert_deliveries_lifecycle_kind ON alert_deliveries (kind, at, id) WHERE event_id IS NULL; -- rollback
DROP INDEX refresh_attempts_holder_resolved;
"#;

#[test]
fn rows_keyed_s1_and_session_s1_both_survive_the_kind_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage = StorageKey::generate();
    drop(Store::open(&path, &storage).unwrap());
    // Two version-39 rows under one key, generation and agent: a session `s1` and a session whose own id is
    // `session:s1`, both valid ids, each with its own payload.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!("{UNDO_V44} {UNDO_V43} {UNDO_V41}"))
        .unwrap();
    conn.execute_batch(
        "INSERT INTO session_memory (key_id, key_generation, session_id, agent_id, record_version, payload, last_seen) \
         VALUES ('desktop', 1000, 's1', 'main', 1, '{\"prompt_id\":\"p1\"}', 1000); \
         INSERT INTO session_memory (key_id, key_generation, session_id, agent_id, record_version, payload, last_seen) \
         VALUES ('desktop', 1000, 'session:s1', 'main', 1, '{\"prompt_id\":\"p2\"}', 1000); \
         UPDATE meta SET value = '39' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &storage).expect("the migration completes and the store opens");
    assert_eq!(store.schema_version().unwrap(), 44);
    let payload = |session: &str| {
        store
            .load(&MemoryKey::for_session("desktop", 1_000, session, "main"))
            .unwrap()
            .map(|r| r.payload)
    };
    assert_eq!(
        payload("s1"),
        Some(r#"{"prompt_id":"p1"}"#.into()),
        "the bare id reads back under its prefix"
    );
    assert_eq!(
        payload("session:s1"),
        Some(r#"{"prompt_id":"p2"}"#.into()),
        "and its neighbour keeps its own row"
    );
}
