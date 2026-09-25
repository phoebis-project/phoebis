//! Tier 2 of the relay's session memory: rows land and reload by key, a replace wins, the purge reads
//! `last_seen`, and an on-disk store keeps the rows across a reopen.
use phoebis_store::{MemoryKey, MemoryRecord, MemoryStore, StorageKey, Store};

fn key(agent: &str) -> MemoryKey {
    MemoryKey {
        key_id: "desktop".into(),
        key_generation: 1_000,
        session_id: "s1".into(),
        agent_id: agent.into(),
    }
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
    assert_eq!(store.schema_version().unwrap(), 27);
}
