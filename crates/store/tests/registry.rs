use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, ApiKeyRow, Failover, Identity, MemoryKey,
    MemoryRecord, MemoryStore, ModelRule, OveragePolicy, PoolRow, RulesMode, Secret, Shedding,
    StorageKey, Store, StoreError, Strategy, SubscriptionRow,
};

// Read by both the roll-back chains below and the standalone direct-open test further down.
const UNDO_V24: &str = "ALTER TABLE accounts DROP COLUMN subscription_plan; \
     ALTER TABLE accounts DROP COLUMN subscription_tier; \
     ALTER TABLE accounts DROP COLUMN subscription_since; \
     ALTER TABLE accounts DROP COLUMN trial_ends_at;";
// The exact reverse of `V25`, one statement per line so the marker on a line that restores an old name or
// column ends at that line alone rather than commenting out the roll-back chain's next constant.
const UNDO_V25: &str = r#"
DROP TABLE plan_fits;
ALTER TABLE accounts DROP COLUMN subscription_ends_at;
ALTER TABLE accounts DROP COLUMN cancelled;
ALTER TABLE api_keys DROP COLUMN in_flight_cap;
ALTER TABLE pins DROP COLUMN tail_hash;
ALTER TABLE pins DROP COLUMN static_hash;
ALTER TABLE pins DROP COLUMN shed_at;
ALTER TABLE pins DROP COLUMN shed_from;
ALTER TABLE pins DROP COLUMN cache_ttl_ms;
ALTER TABLE pins DROP COLUMN last_refresh_at;
ALTER TABLE pins DROP COLUMN last_input_tokens;
ALTER TABLE pins DROP COLUMN burn_at;
ALTER TABLE pins DROP COLUMN burn;
ALTER TABLE pins ADD COLUMN migrated_at INTEGER; -- rollback
ALTER TABLE pools DROP COLUMN wind_down_minutes;
ALTER TABLE pools DROP COLUMN overage_move_grace_minutes;
ALTER TABLE pools DROP COLUMN pace_floor;
ALTER TABLE pools DROP COLUMN in_flight_cap;
ALTER TABLE pools DROP COLUMN retry_in_place_seconds;
ALTER TABLE pools DROP COLUMN warm_sessions;
ALTER TABLE pools DROP COLUMN migrate_when_spent;
ALTER TABLE pools DROP COLUMN source_dwell_seconds;
ALTER TABLE pools DROP COLUMN burn_window_seconds;
ALTER TABLE pools DROP COLUMN shed_share;
ALTER TABLE pools ADD COLUMN api_key_sub_strategy TEXT NOT NULL DEFAULT 'priority_random'; -- rollback
UPDATE pools SET strategy = 'least_utilized' WHERE strategy = 'pace'; -- rollback
UPDATE pools SET strategy = 'priority_random' WHERE strategy = 'priority'; -- rollback
"#;

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
        strategy: Strategy::Priority,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        model_rules: Vec::new(),
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        prefer_subscription: false,
        shedding: Shedding::default(),
        failover: Failover::default(),
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
        model_rules: Vec::new(),
        model_rules_mode: RulesMode::Extend,
        long_context_twins: true,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: store.api_key_digest(secret).to_vec(),
        prefix: phoebis_store::key_prefix(secret),
        created_at: 1,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
        in_flight_cap: None,
    }
}

#[test]
fn the_schema_is_migrated_once_and_the_version_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    // Every migration above the stored version runs again on the next open, and an `ALTER TABLE ADD COLUMN`
    // cannot run twice, so each older database below is rolled back past every column it would re-add.
    const UNDO_V23: &str = "DROP TABLE model_resolutions; \
         ALTER TABLE pools ADD COLUMN default_allowed_models_json TEXT; \
         ALTER TABLE pools DROP COLUMN model_rules_json; \
         ALTER TABLE api_keys ADD COLUMN allowed_models_json TEXT; \
         ALTER TABLE api_keys DROP COLUMN model_rules_json; \
         ALTER TABLE api_keys DROP COLUMN model_rules_mode; \
         ALTER TABLE api_keys DROP COLUMN long_context_twins; \
         CREATE TABLE account_models ( \
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, upstream_id TEXT NOT NULL, canonical_id TEXT NOT NULL, \
           selected INTEGER NOT NULL DEFAULT 1, source TEXT NOT NULL, \
           long_context INTEGER NOT NULL DEFAULT 0, missing INTEGER NOT NULL DEFAULT 0, is_new INTEGER NOT NULL DEFAULT 0, \
           PRIMARY KEY (account_id, upstream_id), \
           UNIQUE (account_id, canonical_id) \
         ); \
         ALTER TABLE accounts ADD COLUMN catalog_synced_at INTEGER; \
         ALTER TABLE accounts ADD COLUMN catalog_error TEXT;";
    const UNDO_V22: &str = "";
    const UNDO_V21: &str = "ALTER TABLE credentials DROP COLUMN last_error;";
    const UNDO_V20: &str = "ALTER TABLE pins DROP COLUMN key_generation; ALTER TABLE pins DROP COLUMN incarnation; \
         ALTER TABLE cooldowns DROP COLUMN incarnation; ALTER TABLE overage_state DROP COLUMN incarnation; \
         ALTER TABLE refresh_attempts DROP COLUMN incarnation;";
    const UNDO_V17: &str = "ALTER TABLE api_keys DROP COLUMN retired_secret_last_used_at;";
    const UNDO_V13: &str = "ALTER TABLE pools DROP COLUMN cache_ttl_json; ALTER TABLE pools DROP COLUMN cache_diagnostics;";
    const UNDO_V12: &str = "";
    // The trailing `\n` (a real newline, unlike the `\`-continued lines above) ends the marker's comment at
    // this constant's own close, so the roll-back chain's next constant is not swallowed with it.
    const UNDO_V11: &str = "ALTER TABLE pools DROP COLUMN prefer_subscription; ALTER TABLE pools DROP COLUMN api_key_sub_strategy; ALTER TABLE pins DROP COLUMN migrated_at; -- rollback\n";
    const UNDO_V9: &str = "ALTER TABLE cooldowns DROP COLUMN set_at;";
    const UNDO_V8: &str = "ALTER TABLE pools DROP COLUMN select_below_utilization;";
    const UNDO_V5: &str =
        "ALTER TABLE accounts DROP COLUMN incarnation; DROP TABLE account_tombstones;";
    const UNDO_V4: &str = "ALTER TABLE accounts DROP COLUMN revision;";
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(store.schema_version().unwrap(), 25);
    drop(store);
    let again = Store::open(&path, &key).unwrap();
    assert_eq!(
        again.schema_version().unwrap(),
        25,
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
        "{UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} INSERT INTO cooldowns (account_id, scope, until, reason, window, source_request_id) \
         VALUES ('a', 'account', 9000, 'rate_limit', 'five_hour', NULL); \
         UPDATE meta SET value = '8' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        25,
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
        "{UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} UPDATE meta SET value = '7' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        25,
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
        "{UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} UPDATE meta SET value = '4' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        25,
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
        "{UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} UPDATE meta SET value = '3' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        25,
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
        "DROP INDEX registry_changes_target; {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} \
         UPDATE meta SET value = '2' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        25,
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

/// The pre-migration shape of two pools and a pin, seeded through the columns and strategy names `V25` maps
/// away: a seed that restores the dropped shape plays the roll-back constant's role, so the sweep's marker
/// covers it the same way.
const SEED_PRE_V25: &str = r#"
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, prefer_subscription, created_at, updated_at) VALUES ('legacy', 'legacy', 1, 'least_utilized', 'prefer_plan', '{}', 0, 1, 1); -- rollback
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, prefer_subscription, created_at, updated_at) VALUES ('random', 'random', 1, 'priority_random', 'prefer_plan', '{}', 0, 1, 1); -- rollback
INSERT INTO pins (key_id, session_id, model, account_id, epoch, created_at, last_used_at, migrated_at) VALUES ('k', 's', 'm', 'legacy', 1, 1, 7, 5); -- rollback
"#;

#[test]
fn a_pool_and_a_pin_from_before_the_engine_columns_read_the_mapped_strategy_and_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    // The pin's account_id is a live foreign key, so the account exists before the rollback plants rows under it.
    store
        .insert_account(&account("legacy", AccountKind::Oauth))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V25} UPDATE meta SET value = '24' WHERE key = 'schema_version'; {SEED_PRE_V25}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 25);
    let pools = migrated.pools().unwrap();
    let legacy = pools.iter().find(|p| p.id == "legacy").unwrap();
    let random = pools.iter().find(|p| p.id == "random").unwrap();
    assert_eq!(
        legacy.strategy,
        Strategy::Pace,
        "the sample-driven strategy maps to the single one that replaces it"
    );
    assert_eq!(
        random.strategy,
        Strategy::Priority,
        "the random pick keeps its rule under its new name"
    );
    assert!(
        legacy.prefer_subscription && random.prefer_subscription,
        "the migration turns the free toggle on for every existing pool"
    );
    assert_eq!(legacy.shedding, Shedding::default());
    assert_eq!(legacy.failover, Failover::default());
    let pin = migrated
        .load_routing()
        .unwrap()
        .pins
        .into_iter()
        .find(|p| p.key_id == "k")
        .unwrap();
    assert_eq!(pin.burn, 0.0);
    assert_eq!(pin.burn_at, None);
    assert_eq!(
        pin.last_refresh_at, 7,
        "a pin from before the burn columns reads its last use as its last refresh"
    );
    assert_eq!(
        (pin.shed_from, pin.shed_at, pin.static_hash, pin.tail_hash),
        (None, None, None, None)
    );
}

#[test]
fn the_subscription_is_stored_with_the_profile_and_reads_back_with_nulls_before_the_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    assert_eq!(
        store.subscription("a").unwrap().unwrap(),
        SubscriptionRow::default(),
        "a row never written answers every field None"
    );
    drop(store);
    // What a database written before the subscription columns existed looks like on disk, opened directly at
    // the version this migration reads from rather than rolled back through every later one.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V25} {UNDO_V24} UPDATE meta SET value = '23' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        25,
        "the direct 23 to 25 open runs every migration between"
    );
    assert_eq!(
        migrated.subscription("a").unwrap().unwrap(),
        SubscriptionRow::default(),
        "the four columns the open just added hold nulls"
    );
    let row = SubscriptionRow {
        plan: Some("max".into()),
        tier: Some("default_claude_max_5x".into()),
        since: Some(1_773_482_400_000),
        trial_ends_at: None,
    };
    migrated.set_profile("a", "{}", &row).unwrap();
    assert_eq!(migrated.subscription("a").unwrap().unwrap(), row);
    assert_eq!(migrated.profile_json("a").unwrap().as_deref(), Some("{}"));
}

#[test]
fn the_cancelled_flag_and_its_end_are_the_operators_alone_and_survive_a_profile_write() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let revision = || store.account("a").unwrap().unwrap().revision;
    let before = revision();
    store.set_cancelled("a", true, Some(9_000)).unwrap();
    assert!(revision() > before, "the write bumps the revision");
    store
        .set_profile(
            "a",
            "{}",
            &SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_5x".into()),
                since: Some(1_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (term.cancelled, term.ends_at, term.since),
        (true, Some(9_000), Some(1_000)),
        "the flag and its end are the operator's alone; the start is the profile's"
    );
    let after_profile = revision();
    store.set_cancelled("a", false, Some(9_000)).unwrap();
    assert!(
        revision() > after_profile,
        "clearing the flag bumps the revision too"
    );
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (term.cancelled, term.ends_at),
        (false, None),
        "the end is dropped the moment the flag goes off, whatever end was passed"
    );
}

#[test]
fn a_fresh_store_carries_the_rule_columns_and_neither_whitelist_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    Store::open(&path, &StorageKey::generate()).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('pools') WHERE name = 'model_rules_json'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "a fresh store carries the pool's rule column"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('api_keys') \
             WHERE name IN ('model_rules_json', 'model_rules_mode', 'long_context_twins')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3,
        "a fresh store carries the key's three rule columns"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('pools') WHERE name = 'default_allowed_models_json'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "the pool's whitelist column is gone"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('api_keys') WHERE name = 'allowed_models_json'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "the key's whitelist column is gone"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('model_resolutions') \
             WHERE name IN ('spelling', 'dated_id', 'resolved_at')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3,
        "a fresh store carries the resolution table's three columns"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'account_models'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "a fresh store carries no catalog table"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('accounts') \
             WHERE name IN ('catalog_synced_at', 'catalog_error')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "a fresh store carries neither catalog column"
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
    conn.execute_batch(&format!(
        "{UNDO_V25} {UNDO_V24} \
         DROP TABLE model_resolutions; \
         ALTER TABLE pools ADD COLUMN default_allowed_models_json TEXT; \
         ALTER TABLE pools DROP COLUMN model_rules_json; \
         ALTER TABLE api_keys ADD COLUMN allowed_models_json TEXT; \
         ALTER TABLE api_keys DROP COLUMN model_rules_json; \
         ALTER TABLE api_keys DROP COLUMN model_rules_mode; \
         ALTER TABLE api_keys DROP COLUMN long_context_twins; \
         CREATE TABLE account_models ( \
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, upstream_id TEXT NOT NULL, canonical_id TEXT NOT NULL, \
           selected INTEGER NOT NULL DEFAULT 1, source TEXT NOT NULL, \
           long_context INTEGER NOT NULL DEFAULT 0, missing INTEGER NOT NULL DEFAULT 0, is_new INTEGER NOT NULL DEFAULT 0, \
           PRIMARY KEY (account_id, upstream_id), \
           UNIQUE (account_id, canonical_id) \
         ); \
         ALTER TABLE accounts ADD COLUMN catalog_synced_at INTEGER; \
         ALTER TABLE accounts ADD COLUMN catalog_error TEXT; \
         CREATE TABLE IF NOT EXISTS api_key_tombstones (id TEXT PRIMARY KEY, deleted_at INTEGER NOT NULL); \
         INSERT OR REPLACE INTO api_key_tombstones (id, deleted_at) VALUES ('old', 1); \
         ALTER TABLE api_keys DROP COLUMN retired_secret_last_used_at; \
         ALTER TABLE pins DROP COLUMN key_generation; ALTER TABLE pins DROP COLUMN incarnation; \
         ALTER TABLE cooldowns DROP COLUMN incarnation; ALTER TABLE overage_state DROP COLUMN incarnation; \
         ALTER TABLE refresh_attempts DROP COLUMN incarnation; \
         ALTER TABLE credentials DROP COLUMN last_error; \
         UPDATE meta SET value = '15' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &storage_key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 25);
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
    let pool_rules = vec![
        ModelRule {
            pattern: "claude-opus-5".into(),
            not: false,
        },
        ModelRule {
            pattern: "*".into(),
            not: true,
        },
    ];
    let key_rules = vec![ModelRule {
        pattern: "claude-sonnet-5".into(),
        not: false,
    }];
    store
        .update_pool(
            &PoolRow {
                body_capture_hours: 24,
                strategy: Strategy::Pace,
                select_below_utilization: Some(0.8),
                model_rules: pool_rules.clone(),
                ..pool("main")
            },
            "test",
        )
        .unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                body_capture_hours: Some(6),
                model_rules: key_rules.clone(),
                model_rules_mode: RulesMode::Replace,
                long_context_twins: false,
                ..k.clone()
            },
            "test",
        )
        .unwrap();
    let updated = store.pool("main").unwrap().unwrap();
    let updated_key = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(
        (
            updated.body_capture_hours,
            updated.strategy,
            updated.select_below_utilization,
            updated.model_rules.clone(),
            updated_key.body_capture_hours,
            updated_key.model_rules.clone(),
            updated_key.model_rules_mode,
            updated_key.long_context_twins,
        ),
        (
            24,
            Strategy::Pace,
            Some(0.8),
            pool_rules,
            Some(6),
            key_rules,
            RulesMode::Replace,
            false,
        )
    );
    assert_eq!(
        store
            .load_registry()
            .unwrap()
            .pools
            .iter()
            .find(|p| p.id == "main")
            .map(|p| (p.strategy, p.select_below_utilization)),
        Some((Strategy::Pace, Some(0.8))),
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
fn an_account_update_commits_its_secret_and_label_together_or_not_at_all() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k1")),
            1,
            "admin",
        )
        .unwrap();
    assert_eq!(store.api_key_credential("a").unwrap().unwrap().1, 1);
    let update = AccountUpdate {
        id: "a",
        expected_revision: 1,
        lifecycle_generation: 1,
        label: "Renamed",
        secret: Some(&Secret::new("k2")),
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
    store.update_account("a", 1, "Renamed", "admin").unwrap();
    assert_eq!(revision(), 2, "a label update");
    store.set_enabled("a", false).unwrap();
    assert_eq!(revision(), 3, "an enable flip");
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    assert_eq!(revision(), 4, "a secret rotation");
    store.set_state("a", AccountState::FundingRequired).unwrap();
    assert_eq!(revision(), 5, "a state change");
    assert_eq!(store.account_revision("nobody").unwrap(), None);
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
    let load = store.load_registry().unwrap();
    assert_eq!(
        (load.accounts.len(), load.pools.len(), load.members.len()),
        (1, 1, 1)
    );
    store.delete_account("a", "test").unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!((load.accounts.len(), load.members.len()), (0, 0));
    assert!(matches!(
        store.delete_account("a", "test"),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn the_newest_change_id_is_the_mirrors_starting_watermark() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    assert_eq!(store.newest_registry_change_id().unwrap(), None);
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store.update_account("a", 1, "first", "admin").unwrap();
    store.update_account("a", 2, "second", "admin").unwrap();
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
fn a_pool_stores_round_robin_and_the_subscription_preference_is_the_operators_alone() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let mut row = pool("rr");
    row.strategy = Strategy::RoundRobin;
    row.prefer_subscription = true;
    store.insert_pool(&row, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert_eq!(
        (read.strategy, read.prefer_subscription),
        (Strategy::RoundRobin, true)
    );
    row.prefer_subscription = false;
    store.update_pool(&row, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert!(
        !read.prefer_subscription && !read.kind_ordered(),
        "the flag is the operator's answer alone; no strategy overrides it"
    );
    let change = store.registry_changes_after(0, 10).unwrap().pop().unwrap();
    assert!(change.diff_json.contains(r#""prefer_subscription":false"#));
}

#[test]
fn an_account_update_is_fenced_on_the_revision_it_was_built_from() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let row = account("vendor", AccountKind::ApiKey);
    store
        .create_account(&row, Some(&Secret::new("k1")), 1, "admin")
        .unwrap();
    let built_from = store.account_revision("vendor").unwrap().unwrap();
    store
        .update_account("vendor", built_from, "Renamed", "admin")
        .unwrap();
    assert_eq!(
        store.account("vendor").unwrap().unwrap().label.as_str(),
        "Renamed"
    );
    assert!(
        matches!(
            store.update_account("vendor", built_from, "Renamed Again", "admin"),
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
