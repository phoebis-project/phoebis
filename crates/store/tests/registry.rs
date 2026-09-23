use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, ApiKeyRow, Identity, MemoryKey,
    MemoryRecord, MemoryStore, ModelSource, OveragePolicy, PoolRow, Secret, ServedModelWrite,
    StorageKey, Store, StoreError, Strategy, SubStrategy,
};

fn account(id: &str, kind: AccountKind) -> AccountRow {
    AccountRow {
        id: id.into(),
        kind,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Ready,
    }
}

fn pool(id: &str) -> PoolRow {
    PoolRow {
        id: id.into(),
        label: id.into(),
        enabled: true,
        strategy: Strategy::PriorityRandom,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        default_allowed_models: None,
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        prefer_subscription: false,
        api_key_sub_strategy: SubStrategy::PriorityRandom,
        cache_ttl_json: "{}".into(),
        cache_diagnostics: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn key(store: &Store, id: &str, pool_id: &str, secret: &str) -> ApiKeyRow {
    ApiKeyRow {
        id: id.into(),
        label: id.into(),
        notes: None,
        pool_id: pool_id.into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        allowed_models: None,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: store.api_key_digest(secret).to_vec(),
        prefix: phoebis_store::key_prefix(secret),
        created_at: 1,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
    }
}

#[test]
fn the_schema_is_migrated_once_and_the_version_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    // Every migration above the stored version runs again on the next open, and an `ALTER TABLE ADD COLUMN`
    // cannot run twice, so each older database below is rolled back past every column it would re-add.
    const UNDO_V21: &str = "ALTER TABLE credentials DROP COLUMN last_error;";
    const UNDO_V20: &str = "ALTER TABLE pins DROP COLUMN key_generation; ALTER TABLE pins DROP COLUMN incarnation; \
         ALTER TABLE cooldowns DROP COLUMN incarnation; ALTER TABLE overage_state DROP COLUMN incarnation; \
         ALTER TABLE refresh_attempts DROP COLUMN incarnation;";
    const UNDO_V17: &str = "ALTER TABLE api_keys DROP COLUMN retired_secret_last_used_at;";
    const UNDO_V13: &str = "ALTER TABLE pools DROP COLUMN cache_ttl_json; ALTER TABLE pools DROP COLUMN cache_diagnostics;";
    const UNDO_V12: &str = "";
    const UNDO_V11: &str = "ALTER TABLE pools DROP COLUMN prefer_subscription; ALTER TABLE pools DROP COLUMN api_key_sub_strategy; ALTER TABLE pins DROP COLUMN migrated_at;";
    const UNDO_V9: &str = "ALTER TABLE cooldowns DROP COLUMN set_at;";
    const UNDO_V8: &str = "ALTER TABLE pools DROP COLUMN select_below_utilization;";
    const UNDO_V5: &str =
        "ALTER TABLE accounts DROP COLUMN incarnation; DROP TABLE account_tombstones;";
    const UNDO_V4: &str = "ALTER TABLE accounts DROP COLUMN revision;";
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(store.schema_version().unwrap(), 21);
    drop(store);
    let again = Store::open(&path, &key).unwrap();
    assert_eq!(
        again.schema_version().unwrap(),
        21,
        "a second open runs no migration and keeps the version"
    );
    assert!(again.pools().unwrap().is_empty());
    again
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    again.insert_pool(&pool("main"), "test").unwrap();
    assert_eq!(again.pools().unwrap()[0].cache_ttl_json, "{}");
    assert!(again.pools().unwrap()[0].cache_diagnostics.is_none());
    drop(again);
    // What a database written before a cooldown recorded when it was set looks like on disk.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} INSERT INTO cooldowns (account_id, scope, until, reason, window, source_request_id) \
         VALUES ('a', 'account', 9000, 'rate_limit', 'five_hour', NULL); \
         UPDATE meta SET value = '8' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        21,
        "a store left at version 8 is carried up"
    );
    assert_eq!(
        migrated
            .load_routing()
            .unwrap()
            .cooldowns
            .pop()
            .unwrap()
            .set_at,
        0,
        "and the row it already held takes the first sample newer than the epoch as evidence"
    );
    assert_eq!(
        migrated
            .load_routing()
            .unwrap()
            .cooldowns
            .pop()
            .unwrap()
            .incarnation,
        0,
        "and names no holder, so the first reload drops it"
    );
    drop(migrated);
    // What a database written before the depletion threshold existed looks like on disk.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} UPDATE meta SET value = '7' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        21,
        "a store left at version 7 is carried up"
    );
    drop(migrated);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('pools') WHERE name = 'select_below_utilization'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "and the column it was missing is there"
    );
    drop(conn);
    // What a database written before account incarnations existed looks like on disk.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} UPDATE meta SET value = '4' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        21,
        "a store left at version 4 is carried up"
    );
    assert_eq!(
        migrated.account("a").unwrap().unwrap().incarnation,
        1,
        "and the row it already held is the first use of its id"
    );
    drop(migrated);
    // And what one written before the account revision existed looks like.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} UPDATE meta SET value = '3' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        21,
        "a store left at version 3 is carried up"
    );
    assert_eq!(
        migrated.account("a").unwrap().unwrap().revision,
        1,
        "and the row it already held starts at the first revision"
    );
    drop(migrated);
    // And what one written before the change-log index existed looks like.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "DROP INDEX registry_changes_target; {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} \
         UPDATE meta SET value = '2' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        21,
        "a store left at version 2 is carried up"
    );
    drop(migrated);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'registry_changes_target'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "and the index it was missing is there"
    );
}

/// Both writes stamp themselves from the same clock, so a delete and the create that follows it can land in
/// one millisecond. The stamps are pinned equal here rather than raced for, so the case the change log's
/// order answers and a comparison of the two clocks does not is the one this test runs every time.
#[test]
fn an_id_created_again_in_the_millisecond_it_was_deleted_in_is_a_reused_one() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-reissued"), "test")
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE api_keys SET created_at = (SELECT at FROM registry_changes WHERE target = 'back' AND kind = 'key_deleted' ORDER BY id DESC LIMIT 1) WHERE id = 'back'",
        [],
    )
    .unwrap();

    assert_eq!(
        store.load_registry().unwrap().reused_key_ids,
        vec!["back".to_string()],
        "the id carries a holder before this one whatever the two stamps read"
    );
}

#[test]
fn a_deleted_keys_id_is_created_again_at_once_on_a_fresh_store_and_on_an_upgraded_one() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-reissued"), "test")
        .unwrap();
    assert_eq!(
        store.api_key("back").unwrap().unwrap().prefix,
        phoebis_store::key_prefix("phoebis-back-reissued")
    );

    // A store that retired ids: the table is dropped by the upgrade and the id is creatable again.
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "old", "main", "phoebis-old-secret"), "test")
        .unwrap();
    store.delete_api_key("old", "test").unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_key_tombstones (id TEXT PRIMARY KEY, deleted_at INTEGER NOT NULL); \
         INSERT OR REPLACE INTO api_key_tombstones (id, deleted_at) VALUES ('old', 1); \
         ALTER TABLE api_keys DROP COLUMN retired_secret_last_used_at; \
         ALTER TABLE pins DROP COLUMN key_generation; ALTER TABLE pins DROP COLUMN incarnation; \
         ALTER TABLE cooldowns DROP COLUMN incarnation; ALTER TABLE overage_state DROP COLUMN incarnation; \
         ALTER TABLE refresh_attempts DROP COLUMN incarnation; \
         ALTER TABLE credentials DROP COLUMN last_error; \
         UPDATE meta SET value = '15' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &storage_key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 21);
    migrated
        .insert_api_key(&key(&migrated, "old", "main", "phoebis-old-reissued"), "test")
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'api_key_tombstones'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the upgrade dropped the table");
}

/// A deleted key's memory rows can never be read again, so `delete_api_key` cascades `session_memory` by
/// `key_id` the same way it cascades `pins` rather than leaving them for the retention purge.
#[test]
fn deleting_a_key_drops_its_session_memory_so_a_reissued_id_reads_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    let memory_key = MemoryKey {
        key_id: "back".into(),
        key_generation: 1_000,
        session_id: "s-1".into(),
        agent_id: "main".into(),
    };
    store
        .save(&[(
            memory_key.clone(),
            MemoryRecord {
                record_version: 1,
                payload: "{}".into(),
                last_seen: 1,
            },
        )])
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    assert_eq!(
        MemoryStore::load(&store, &memory_key).unwrap(),
        None,
        "the deleted key's memory goes with the key"
    );
}

#[test]
fn a_key_created_in_its_previous_holders_millisecond_takes_the_next_generation() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    // The first holder's create is moved a minute past the wall clock: the state the second insert sees when
    // it lands in the first's millisecond, pinned rather than raced for.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE registry_changes SET at = at + 60000 WHERE target = 'back' AND kind = 'key_created'",
        [],
    )
    .unwrap();
    let first: i64 = conn
        .query_row(
            "SELECT MAX(at) FROM registry_changes WHERE target = 'back' AND kind = 'key_created'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-reissued"), "test")
        .unwrap();
    let second = store.api_key("back").unwrap().unwrap().created_at;
    assert_eq!(second, first + 1, "one millisecond, two generations");
    assert_eq!(
        store.load_registry().unwrap().reused_key_ids,
        vec!["back".to_string()],
        "the change log still names the id as one with an earlier holder"
    );
}

#[test]
fn the_newest_catalog_change_is_what_the_account_serves_now() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .insert_account(&account("b", AccountKind::Oauth))
        .unwrap();
    assert_eq!(store.latest_catalog_change("a").unwrap(), None);
    assert!(store
        .seed_catalog("a", &[("claude-sonnet-4-5".into(), false)], 1, "test")
        .unwrap());
    assert_eq!(
        store.latest_catalog_change("a").unwrap().as_deref(),
        Some("catalog_seeded")
    );
    store
        .apply_catalog_sync("a", &[("claude-opus-4-1".into(), false)], None, 2, "test")
        .unwrap();
    assert_eq!(
        store.latest_catalog_change("a").unwrap().as_deref(),
        Some("catalog_synced"),
        "a discovery replaces the seed as what the account serves"
    );
    // A discovery that answered an empty list leaves nothing to serve, which is the one case a later seed
    // still fills.
    store.apply_catalog_sync("b", &[], None, 3, "test").unwrap();
    assert!(store
        .seed_catalog("b", &[("claude-sonnet-4-5".into(), false)], 4, "test")
        .unwrap());
    assert_eq!(
        store.latest_catalog_change("b").unwrap().as_deref(),
        Some("catalog_seeded"),
        "and a seed after a sync is what it serves in turn"
    );
    assert_eq!(
        store.latest_catalog_change("a").unwrap().as_deref(),
        Some("catalog_synced"),
        "each account reads its own log"
    );
}

#[test]
fn two_slots_cannot_bind_one_upstream_account() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .insert_account(&account("b", AccountKind::Oauth))
        .unwrap();
    let identity = Identity {
        account_uuid: "uuid-1".into(),
        email: "x@example.invalid".into(),
        organization_uuid: None,
    };
    store.set_identity("a", &identity).unwrap();
    assert!(matches!(
        store.set_identity("b", &identity),
        Err(StoreError::IdentityTaken)
    ));
    store.set_identity("a", &identity).unwrap();
}

#[test]
fn pools_members_and_keys_round_trip_and_a_pool_with_keys_cannot_be_deleted() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[("a".into(), 2)], "test")
        .unwrap();
    assert_eq!(store.pool_members("main").unwrap()[0].priority, 2);
    assert!(matches!(
        store.set_pool_members("main", &[("ghost".into(), 0)], "test"),
        Err(StoreError::AccountNotFound(_))
    ));
    let k = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&k, "test").unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().prefix,
        "01234567"
    );
    store
        .update_pool(
            &PoolRow {
                body_capture_hours: 24,
                strategy: Strategy::EarliestRefresh,
                select_below_utilization: Some(0.8),
                ..pool("main")
            },
            "test",
        )
        .unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                body_capture_hours: Some(6),
                ..k.clone()
            },
            "test",
        )
        .unwrap();
    let updated = store.pool("main").unwrap().unwrap();
    assert_eq!(
        (
            updated.body_capture_hours,
            updated.strategy,
            updated.select_below_utilization,
            store
                .api_key("desktop")
                .unwrap()
                .unwrap()
                .body_capture_hours
        ),
        (24, Strategy::EarliestRefresh, Some(0.8), Some(6))
    );
    assert_eq!(
        store
            .load_registry()
            .unwrap()
            .pools
            .iter()
            .find(|p| p.id == "main")
            .map(|p| (p.strategy, p.select_below_utilization)),
        Some((Strategy::EarliestRefresh, Some(0.8))),
        "and the load every reader starts from reads both back"
    );
    assert!(matches!(
        store.delete_pool("main", "test"),
        Err(StoreError::PoolInUse(_))
    ));
    store.revoke_api_key("desktop", 5, "test").unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().revoked_at,
        Some(5)
    );
    store.delete_api_key("desktop", "test").unwrap();
    store.delete_pool("main", "test").unwrap();
    assert!(store.pool("main").unwrap().is_none());
    let kinds: Vec<String> = store
        .registry_changes_after(0, 100)
        .unwrap()
        .into_iter()
        .map(|c| c.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "pool_created",
            "pool_members_set",
            "key_created",
            "pool_updated",
            "key_updated",
            "key_revoked",
            "key_deleted",
            "pool_deleted"
        ]
    );
}

#[test]
fn a_catalog_sync_adds_new_ids_unselected_flags_vanished_ones_and_keeps_manual_rows() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    let first = store
        .apply_catalog_sync(
            "a",
            &[
                ("claude-sonnet-5".into(), false),
                ("claude-opus-5".into(), true),
            ],
            None,
            10,
            "catalog",
        )
        .unwrap();
    assert!(first.first_sync);
    assert!(store
        .served_models("a")
        .unwrap()
        .iter()
        .all(|m| m.selected && !m.is_new));
    store
        .put_served_models(
            "a",
            &[
                ServedModelWrite {
                    upstream_id: "claude-sonnet-5".into(),
                    selected: true,
                    long_context: false,
                },
                ServedModelWrite {
                    upstream_id: "claude-opus-5".into(),
                    selected: false,
                    long_context: true,
                },
                ServedModelWrite {
                    upstream_id: "Vendor/Model-A".into(),
                    selected: true,
                    long_context: false,
                },
            ],
            "admin",
        )
        .unwrap();
    let second = store
        .apply_catalog_sync(
            "a",
            &[
                ("claude-sonnet-5".into(), false),
                ("claude-haiku-4-5".into(), false),
            ],
            None,
            20,
            "catalog",
        )
        .unwrap();
    assert_eq!(second.added, ["claude-haiku-4-5"]);
    assert_eq!(second.missing, ["claude-opus-5"]);
    let rows = store.served_models("a").unwrap();
    let by_id = |id: &str| rows.iter().find(|m| m.upstream_id == id).unwrap();
    assert!(!by_id("claude-haiku-4-5").selected && by_id("claude-haiku-4-5").is_new);
    assert!(
        by_id("claude-opus-5").missing && !by_id("claude-opus-5").selected,
        "selected is left as the operator wrote it"
    );
    assert_eq!(by_id("Vendor/Model-A").source, ModelSource::Manual);
    assert_eq!(by_id("Vendor/Model-A").canonical_id, "vendor/model-a");
    assert!(matches!(
        store.put_served_models(
            "a",
            &[
                ServedModelWrite {
                    upstream_id: "Vendor/Model-A".into(),
                    selected: true,
                    long_context: false
                },
                ServedModelWrite {
                    upstream_id: "vendor/model-a".into(),
                    selected: true,
                    long_context: false
                },
            ],
            "admin"
        ),
        Err(StoreError::CanonicalClash { .. })
    ));
    assert!(
        !store
            .seed_catalog("a", &[("claude-fable-5-1".into(), true)], 30, "catalog")
            .unwrap(),
        "a seed never overwrites a populated catalog"
    );
}

#[test]
fn an_api_key_secret_carries_a_generation_that_fences_state_verdicts() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    assert_eq!(store.api_key_credential("a").unwrap().unwrap().1, 1);
    store.set_api_key_secret("a", &Secret::new("k2")).unwrap();
    let (secret, generation) = store.api_key_credential("a").unwrap().unwrap();
    assert_eq!((secret.expose(), generation), ("k2", 2));
    assert_eq!(
        store.credential_generation("a").unwrap(),
        Some(2),
        "the kind-agnostic read answers the same generation"
    );
    assert_eq!(store.credential_generation("nobody").unwrap(), None);
    assert!(
        !store
            .set_state_if_generations("a", AccountState::ReauthRequired, 1, 1, 1)
            .unwrap(),
        "a verdict about the replaced key changes nothing"
    );
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
    assert!(store
        .set_state_if_generations("a", AccountState::ReauthRequired, 1, 1, 2)
        .unwrap());
}

/// An api-key account holding a secret at generation 1, as every fresh slot under an id does.
fn insert_ready_api_key_account(store: &Store, id: &str) {
    store
        .insert_account(&account(id, AccountKind::ApiKey))
        .unwrap();
    store.set_api_key_secret(id, &Secret::new("k1")).unwrap();
}

fn seeded_api_key_account(id: &str) -> Store {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    insert_ready_api_key_account(&store, id);
    store
}

#[test]
fn a_state_verdict_from_a_previous_incarnation_changes_nothing() {
    let store = seeded_api_key_account("a");
    let (incarnation, lifecycle, credential) = (1, 1, 1);
    store.delete_account("a", "test").unwrap();
    insert_ready_api_key_account(&store, "a");
    let moved = store
        .set_state_if_generations(
            "a",
            AccountState::FundingRequired,
            incarnation,
            lifecycle,
            credential,
        )
        .unwrap();
    assert!(!moved, "the verdict named a holder that is gone");
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
    assert!(
        store
            .set_state_if_generations("a", AccountState::FundingRequired, 2, 1, 1)
            .unwrap(),
        "the holder the tombstone numbered 2 takes its own verdicts"
    );
}

#[test]
fn an_account_update_commits_secret_fields_and_catalog_together_or_not_at_all() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k1")),
            Some(&[("claude-sonnet-5".into(), false)]),
            None,
            1,
            "admin",
        )
        .unwrap();
    assert_eq!(
        (
            store.served_models("a").unwrap().len(),
            store.api_key_credential("a").unwrap().unwrap().1
        ),
        (1, 1)
    );
    let update = AccountUpdate {
        id: "a",
        expected_revision: 1,
        lifecycle_generation: 1,
        label: "Renamed",
        secret: Some(&Secret::new("k2")),
        catalog: Some(&[("gpt-5".into(), false)]),
        catalog_error: None,
        state: AccountState::Ready,
        at: 5,
        actor: "admin",
    };
    store.commit_account_update(&update).unwrap();
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(row.label.as_str(), "Renamed");
    assert_eq!(
        store.api_key_credential("a").unwrap().unwrap().0.expose(),
        "k2"
    );
    assert_eq!(
        store.served_models("a").unwrap().len(),
        2,
        "the catalog was applied as a sync"
    );
    let stale = AccountUpdate {
        lifecycle_generation: 7,
        label: "Never",
        ..update
    };
    assert!(matches!(
        store.commit_account_update(&stale),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        store.account("a").unwrap().unwrap().label,
        "Renamed",
        "a stale update writes nothing"
    );
    // The commit above moved the revision, so the same edit verified against the row before it is now about
    // an account whose endpoint and secret nobody checked together.
    let superseded = AccountUpdate {
        label: "Never",
        ..update
    };
    assert!(matches!(
        store.commit_account_update(&superseded),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        (
            store.account("a").unwrap().unwrap().label,
            store.api_key_credential("a").unwrap().unwrap().0.expose()
        ),
        ("Renamed".to_string(), "k2"),
        "an edit verified against a superseded revision writes nothing"
    );
}

#[test]
fn every_write_that_changes_a_dispatch_input_moves_the_account_revision() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    let revision = || store.account("a").unwrap().unwrap().revision;
    assert_eq!(
        (revision(), store.account_revision("a").unwrap()),
        (1, Some(1))
    );
    store
        .update_account("a", 1, "Renamed", "admin")
        .unwrap();
    assert_eq!(revision(), 2, "a label update");
    store.set_enabled("a", false).unwrap();
    assert_eq!(revision(), 3, "an enable flip");
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    assert_eq!(revision(), 4, "a secret rotation");
    store.set_state("a", AccountState::FundingRequired).unwrap();
    assert_eq!(revision(), 5, "a state change");
    store
        .apply_catalog_sync(
            "a",
            &[("claude-sonnet-5".into(), false)],
            None,
            6,
            "catalog",
        )
        .unwrap();
    assert_eq!(
        revision(),
        5,
        "a catalog sync changes no dispatch input, so it leaves the revision alone"
    );
    assert_eq!(store.account_revision("nobody").unwrap(), None);
}

#[test]
fn a_catalog_sync_verified_against_a_superseded_revision_is_refused() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    let discovered = [("claude-sonnet-5".into(), false)];
    store
        .update_account("a", 1, "Renamed", "admin")
        .unwrap();
    assert!(matches!(
        store.apply_catalog_sync("a", &discovered, Some(1), 10, "catalog"),
        Err(StoreError::Stale)
    ));
    assert!(store.served_models("a").unwrap().is_empty());
    store
        .apply_catalog_sync("a", &discovered, Some(2), 10, "catalog")
        .unwrap();
    assert_eq!(store.served_models("a").unwrap().len(), 1);
}

#[test]
fn deleting_an_account_cascades_and_the_load_reads_everything() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[("a".into(), 0)], "test")
        .unwrap();
    store
        .apply_catalog_sync(
            "a",
            &[("claude-sonnet-5".into(), false)],
            None,
            1,
            "catalog",
        )
        .unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!(
        (
            load.accounts.len(),
            load.pools.len(),
            load.members.len(),
            load.models.len()
        ),
        (1, 1, 1, 1)
    );
    store.delete_account("a", "test").unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!(
        (load.accounts.len(), load.members.len(), load.models.len()),
        (0, 0, 0)
    );
    assert!(matches!(
        store.delete_account("a", "test"),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn a_catalog_sync_keeps_a_manual_rows_source_and_selection() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store
        .put_served_models(
            "a",
            &[ServedModelWrite {
                upstream_id: "Vendor/Model-A".into(),
                selected: false,
                long_context: false,
            }],
            "admin",
        )
        .unwrap();
    let first = store
        .apply_catalog_sync(
            "a",
            &[
                ("Vendor/Model-A".into(), true),
                ("claude-sonnet-5".into(), false),
            ],
            None,
            10,
            "catalog",
        )
        .unwrap();
    assert_eq!(first.added, ["claude-sonnet-5"]);
    let rows = store.served_models("a").unwrap();
    let by_id = |id: &str| rows.iter().find(|m| m.upstream_id == id).unwrap();
    let manual = by_id("Vendor/Model-A");
    assert_eq!(manual.source, ModelSource::Manual);
    assert!(
        !manual.selected,
        "the discovery reporting a manual row never changes its selection"
    );
    assert!(
        manual.long_context,
        "the discovery's long-context flag applies even to a manual row"
    );
    assert!(!manual.missing);
    let new_row = by_id("claude-sonnet-5");
    assert!(!new_row.selected && new_row.is_new);

    let second = store
        .apply_catalog_sync(
            "a",
            &[("claude-sonnet-5".into(), false)],
            None,
            20,
            "catalog",
        )
        .unwrap();
    assert!(
        second.missing.is_empty(),
        "a manual row omitted from a sync is never flagged missing"
    );
    assert!(
        store
            .served_models("a")
            .unwrap()
            .iter()
            .any(|m| m.upstream_id == "Vendor/Model-A"),
        "a manual row omitted from a sync is never removed"
    );
}

#[test]
fn the_newest_change_id_is_the_mirrors_starting_watermark() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    assert_eq!(store.newest_registry_change_id().unwrap(), None);
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .update_account("a", 1, "first", "admin")
        .unwrap();
    store
        .update_account("a", 2, "second", "admin")
        .unwrap();
    let newest = store.newest_registry_change_id().unwrap().unwrap();
    let changes = store.registry_changes_after(0, 100).unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        newest,
        changes.iter().map(|c| c.id).max().unwrap(),
        "the watermark is the largest id the log holds"
    );
    assert!(
        store
            .registry_changes_after(newest, 100)
            .unwrap()
            .is_empty(),
        "so a mirror starting there replays nothing"
    );
}

#[test]
fn a_pool_stores_round_robin_the_subscription_preference_and_the_sub_strategy() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let mut row = pool("rr");
    row.strategy = Strategy::RoundRobin;
    row.prefer_subscription = true;
    row.api_key_sub_strategy = SubStrategy::RoundRobin;
    store.insert_pool(&row, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert_eq!(
        (
            read.strategy,
            read.prefer_subscription,
            read.api_key_sub_strategy
        ),
        (Strategy::RoundRobin, true, SubStrategy::RoundRobin)
    );
    row.prefer_subscription = false;
    row.api_key_sub_strategy = SubStrategy::PriorityRandom;
    store.update_pool(&row, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert_eq!(
        (read.prefer_subscription, read.api_key_sub_strategy),
        (false, SubStrategy::PriorityRandom)
    );
    let change = store.registry_changes_after(0, 10).unwrap().pop().unwrap();
    assert!(
        change.diff_json.contains(r#""prefer_subscription":false"#)
            && change
                .diff_json
                .contains(r#""api_key_sub_strategy":"priority_random""#)
    );
    row.strategy = Strategy::LeastUtilized;
    store.update_pool(&row, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert!(
        !read.prefer_subscription && read.kind_ordered(),
        "the stored flag stays the operator's answer; the sample-driven strategy orders by kind regardless"
    );
}

#[test]
fn an_account_update_is_fenced_on_the_revision_it_was_built_from() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let row = account("vendor", AccountKind::ApiKey);
    store
        .create_account(
            &row,
            Some(&Secret::new("k1")),
            None,
            None,
            1,
            "admin",
        )
        .unwrap();
    let built_from = store.account_revision("vendor").unwrap().unwrap();
    store
        .update_account(
            "vendor",
            built_from,
            "Renamed",
            "admin",
        )
        .unwrap();
    assert_eq!(
        store.account("vendor").unwrap().unwrap().label.as_str(),
        "Renamed"
    );
    assert!(
        matches!(
            store.update_account(
                "vendor",
                built_from,
                "Renamed Again",
                "admin",
            ),
            Err(StoreError::Stale)
        ),
        "the revision the caller read is gone, so the edit it assembled is about a row that moved"
    );
    assert_eq!(store.account("vendor").unwrap().unwrap().label, "Renamed");
    assert!(matches!(
        store.update_account("nobody", 1, "x", "admin"),
        Err(StoreError::AccountNotFound(_))
    ));
}
