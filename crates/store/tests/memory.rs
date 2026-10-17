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
    assert_eq!(store.schema_version().unwrap(), 57);
}

// The reverse of the registry's member budgets migration, so a store stamped back below it can run it again. One
// line with the marker last. The trailing `\n` ends the marker's comment.
const UNDO_V41: &str = "ALTER TABLE pool_members ADD COLUMN budget_usd REAL; ALTER TABLE pool_members ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'month'; UPDATE pool_members SET budget_usd = COALESCE(budget_monthly_usd, budget_daily_usd), budget_period = CASE WHEN budget_monthly_usd IS NULL AND budget_daily_usd IS NOT NULL THEN 'day' ELSE 'month' END; ALTER TABLE pool_members DROP COLUMN budget_monthly_usd; ALTER TABLE pool_members DROP COLUMN budget_daily_usd; -- rollback\n";
// The reverse of the registry's version 56: the pins lose `entrypoint`. One line with the marker last. The trailing
// `\n` ends the marker's comment.
const UNDO_V56: &str = "ALTER TABLE pins DROP COLUMN entrypoint; -- rollback\n";
// The reverse of the registry's version 55: the pools lose `slow_down`, `slow_down_stretch_seconds`,
// `slow_down_hold_seconds`, `return_to_plan` and `return_to_plan_dwell_seconds`. One line with the marker last. The
// trailing `\n` ends the marker's comment.
const UNDO_V55: &str = "ALTER TABLE pools DROP COLUMN slow_down; ALTER TABLE pools DROP COLUMN slow_down_stretch_seconds; ALTER TABLE pools DROP COLUMN slow_down_hold_seconds; ALTER TABLE pools DROP COLUMN return_to_plan; ALTER TABLE pools DROP COLUMN return_to_plan_dwell_seconds; -- rollback\n";
// The reverse of the registry's version 54: the pools' three renamed columns take their old names back, the three
// dropped ones return with their old types and defaults, and the ceiling and the switch leave, so the chains below
// replay the older undos over the old names. One line with the marker last. The trailing `\n` ends the marker's
// comment.
const UNDO_V54: &str = "ALTER TABLE pools DROP COLUMN window_ceiling; ALTER TABLE pools DROP COLUMN load_shedding; ALTER TABLE pools ADD COLUMN shed_ahead_of_pace REAL; ALTER TABLE pools ADD COLUMN select_below_utilization REAL; ALTER TABLE pools ADD COLUMN pace_floor REAL NOT NULL DEFAULT 0.05; ALTER TABLE pools RENAME COLUMN shed_cap TO shed_share; ALTER TABLE pools RENAME COLUMN burn_average_seconds TO burn_window_seconds; ALTER TABLE pools RENAME COLUMN shed_dwell_seconds TO source_dwell_seconds; -- rollback\n";
// The reverse of the registry's version 53: the three mark columns leave the pins and the rounds table is rebuilt as
// version 46 left it, so the chains below replay `UNDO_V46` over it unchanged. One line with the marker last. The
// trailing `\n` ends the marker's comment.
const UNDO_V53: &str = "ALTER TABLE pins DROP COLUMN marked_at; ALTER TABLE pins DROP COLUMN marked_on; ALTER TABLE pins DROP COLUMN marked_window; DROP TABLE shed_rounds; CREATE TABLE shed_rounds (pool_id TEXT NOT NULL, account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, incarnation INTEGER NOT NULL, key_id TEXT NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, model TEXT NOT NULL, epoch INTEGER NOT NULL, round_at INTEGER NOT NULL, marked INTEGER NOT NULL, PRIMARY KEY (pool_id, account_id, key_id, session_id, agent_id, model)); -- rollback\n";
// The reverse of the registry's version 52: the five fast burn columns. One line with the marker last. The trailing
// `\n` ends the marker's comment.
const UNDO_V52: &str = "ALTER TABLE pins DROP COLUMN burn_fast_input; ALTER TABLE pins DROP COLUMN burn_fast_cache_read; ALTER TABLE pins DROP COLUMN burn_fast_write_5m; ALTER TABLE pins DROP COLUMN burn_fast_write_1h; ALTER TABLE pins DROP COLUMN burn_fast_output; -- rollback\n";
// The reverse of `V51`: the affinity rows lose the answer id, which no version before it stored.
const UNDO_V51: &str = "ALTER TABLE affinity DROP COLUMN message_id;";
// The reverse of the registry's version 50: the keepalive's day budgets table as the versions before it left it. One
// line with the marker last. The trailing `\n` ends the marker's comment.
const UNDO_V50: &str = "CREATE TABLE keepalive_budgets (day TEXT NOT NULL, scope TEXT NOT NULL, id TEXT NOT NULL, usd REAL NOT NULL DEFAULT 0, five_hour_share REAL NOT NULL DEFAULT 0, PRIMARY KEY (day, scope, id)); -- rollback\n";
// The reverse of the registry's version 49: the keepalive engine's three tables.
const UNDO_V49: &str =
    "DROP TABLE keepalive_idle_returns; DROP TABLE keepalive_agents; DROP TABLE keepalive_sessions;";
// The reverse of the registry's version 48, so a store stamped back below it can run it again.
const UNDO_V48: &str = r#"
ALTER TABLE pools DROP COLUMN incarnation;
ALTER TABLE api_keys DROP COLUMN incarnation;
DROP TABLE pool_tombstones;
DROP TABLE api_key_tombstones;
"#;
// The reverse of the registry's version 47, so a store stamped back below it can run it again. One line with the
// marker last, and the trailing `\n` ends the marker's comment.
const UNDO_V47: &str = "DROP INDEX pool_members_one_pool; -- rollback\n";
// The reverse of the registry's version 46, one statement per line.
const UNDO_V46: &str = r#"
DROP TABLE shed_rounds;
ALTER TABLE affinity DROP COLUMN depth;
ALTER TABLE accounts DROP COLUMN state_cause;
"#;
// The reverse of the registry's version 45, so a store stamped back below it can run it again. One line with the
// marker last. The trailing `\n` ends the marker's comment.
const UNDO_V45: &str =
    "ALTER TABLE pools RENAME COLUMN shed_ahead_of_pace TO migrate_above_utilization; -- rollback\n";
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
    conn.execute_batch(&format!(
        "{UNDO_V56} {UNDO_V55} {UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V41}"
    ))
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
    assert_eq!(store.schema_version().unwrap(), 57);
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
        "and its neighbor keeps its own row"
    );
}
