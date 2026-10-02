use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, Affinity, ApiKeyRow,
    Failover, Identity, MemberWrite, MemoryKey, MemoryRecord, MemoryStore, ModelRule,
    OveragePolicy, PinBurn, PoolRow, RulesMode, Secret, Shedding, StorageKey, Store, StoreError,
    Strategy, SubscriptionRow,
};

// Read by both the roll-back chains below and the standalone direct-open test further down.
const UNDO_V24: &str = "ALTER TABLE accounts DROP COLUMN subscription_plan; \
     ALTER TABLE accounts DROP COLUMN subscription_tier; \
     ALTER TABLE accounts DROP COLUMN subscription_since; \
     ALTER TABLE accounts DROP COLUMN trial_ends_at;";
// The exact reverse of `V25`, one statement per line so the marker on a line that restores an old name or
// column ends at that line alone rather than commenting out the roll-back chain's next constant.
const UNDO_V25: &str = r#"
DROP TABLE plan_fits; -- rollback
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
const UNDO_V26: &str = "ALTER TABLE pins DROP COLUMN burn_usd;";
const UNDO_V27: &str = r#"
ALTER TABLE plan_fits DROP COLUMN band_outside_delta; -- rollback
ALTER TABLE plan_fits DROP COLUMN band_outside_tokens; -- rollback
ALTER TABLE plan_fits DROP COLUMN band_inside_delta; -- rollback
ALTER TABLE plan_fits DROP COLUMN band_inside_tokens; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_reset; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_utilization; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_since; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_delta; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_outside; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_inside; -- rollback
ALTER TABLE plan_fits DROP COLUMN carry_weighted; -- rollback
ALTER TABLE plan_fits DROP COLUMN band_factor; -- rollback
"#;
const UNDO_V28: &str = "DROP TABLE keepalive_budgets;";
// The exact reverse of `V29`: the affinity table and its index, the key and member columns it added, the pool's
// five placement columns, then the pin rebuilt back to its three-column key. A pin under another agent has no
// home in that key, so it is dropped rather than collided into one; a seed that needs it back plants it after.
const UNDO_V29: &str = r#"
DROP INDEX affinity_written_at;
DROP TABLE affinity;
ALTER TABLE api_keys DROP COLUMN affinity_source;
ALTER TABLE pool_members DROP COLUMN budget_period; -- rollback
ALTER TABLE pool_members DROP COLUMN budget_usd; -- rollback
ALTER TABLE pools DROP COLUMN quota_headers;
ALTER TABLE pools DROP COLUMN min_shared_prefix_tokens;
ALTER TABLE pools DROP COLUMN session_placement;
ALTER TABLE pools DROP COLUMN subagent_placement;
ALTER TABLE pools DROP COLUMN affinity_mode;
DELETE FROM pins WHERE agent_id <> 'main';
CREATE TABLE pins_prev (
  key_id TEXT NOT NULL, session_id TEXT NOT NULL, model TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  epoch INTEGER NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL,
  key_generation INTEGER NOT NULL DEFAULT 0, incarnation INTEGER NOT NULL DEFAULT 0,
  burn REAL NOT NULL DEFAULT 0, burn_at INTEGER, burn_usd REAL NOT NULL DEFAULT 0,
  last_input_tokens INTEGER NOT NULL DEFAULT 0, last_refresh_at INTEGER NOT NULL DEFAULT 0,
  cache_ttl_ms INTEGER NOT NULL DEFAULT 300000,
  shed_from TEXT, shed_at INTEGER, static_hash TEXT, tail_hash TEXT,
  PRIMARY KEY (key_id, session_id, model)
);
INSERT INTO pins_prev (key_id, session_id, model, account_id, epoch, created_at, last_used_at,
  key_generation, incarnation, burn, burn_at, burn_usd, last_input_tokens, last_refresh_at, cache_ttl_ms,
  shed_from, shed_at, static_hash, tail_hash)
SELECT key_id, session_id, model, account_id, epoch, created_at, last_used_at,
  key_generation, incarnation, burn, burn_at, burn_usd, last_input_tokens, last_refresh_at, cache_ttl_ms,
  shed_from, shed_at, static_hash, tail_hash
FROM pins;
DROP TABLE pins;
ALTER TABLE pins_prev RENAME TO pins;
"#;
// The exact reverse of `V30`: `V30` is `DROP TABLE IF EXISTS`, so the forward migration would run again with
// or without this undo; what recreating the table buys is a faithful version-29 store for a test that seeds
// one directly. The trailing `\n` ends the marker's comment at this constant's own close, as the comment above
// `UNDO_V11` explains.
const UNDO_V30: &str = "CREATE TABLE pricing_versions (version TEXT PRIMARY KEY, effective TEXT NOT NULL, source TEXT NOT NULL, table_json TEXT NOT NULL, imported_at INTEGER NOT NULL); -- rollback\n";
const UNDO_V31: &str = "DROP INDEX alert_deliveries_lifecycle_at; DROP INDEX alert_deliveries_lifecycle_kind; \
     CREATE INDEX alert_deliveries_lifecycle ON alert_deliveries (kind, id) WHERE event_id IS NULL;";
const UNDO_V32: &str = "DROP TABLE usage_windows;";
// The reverse of `V33`, the three dropped columns re-added last so the marker that ends the string covers the
// line that restores them; the trailing `\n` ends the marker's comment at this constant's own close.
const UNDO_V33: &str = "ALTER TABLE keepalive_budgets DROP COLUMN five_hour_share_unmeasured; -- rollback\n\
     ALTER TABLE keepalive_budgets DROP COLUMN five_hour_share; \
     ALTER TABLE pins DROP COLUMN burn_banded_output; \
     ALTER TABLE pins DROP COLUMN burn_banded_write_1h; \
     ALTER TABLE pins DROP COLUMN burn_banded_write_5m; \
     ALTER TABLE pins DROP COLUMN burn_banded_cache_read; \
     ALTER TABLE pins DROP COLUMN burn_banded_input; \
     ALTER TABLE pins DROP COLUMN burn_output; \
     ALTER TABLE pins DROP COLUMN burn_write_1h; \
     ALTER TABLE pins DROP COLUMN burn_write_5m; \
     ALTER TABLE pins DROP COLUMN burn_cache_read; \
     ALTER TABLE pins DROP COLUMN burn_input; \
     ALTER TABLE pins ADD COLUMN burn_usd REAL NOT NULL DEFAULT 0; ALTER TABLE pins ADD COLUMN burn REAL NOT NULL DEFAULT 0; ALTER TABLE keepalive_budgets ADD COLUMN meter_tokens REAL NOT NULL DEFAULT 0; -- rollback\n";
// The reverse of `V34`: the table with the eight columns of `V25` and the twelve of `V27`, so the chains below
// replay `UNDO_V27` and `UNDO_V25` over it unchanged; one line, so the marker that ends it covers the whole
// statement, and the trailing `\n` ends the marker's comment at this constant's own close.
const UNDO_V34: &str = "CREATE TABLE plan_fits (account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, window TEXT NOT NULL, scale REAL NOT NULL, confidence REAL NOT NULL, intervals INTEGER NOT NULL, corrections_json TEXT NOT NULL DEFAULT '{}', fitted_at INTEGER NOT NULL, incarnation INTEGER NOT NULL, band_factor REAL, carry_weighted REAL NOT NULL DEFAULT 0, carry_inside REAL NOT NULL DEFAULT 0, carry_outside REAL NOT NULL DEFAULT 0, carry_delta REAL NOT NULL DEFAULT 0, carry_since INTEGER, carry_utilization REAL, carry_reset INTEGER, band_inside_tokens REAL NOT NULL DEFAULT 0, band_inside_delta REAL NOT NULL DEFAULT 0, band_outside_tokens REAL NOT NULL DEFAULT 0, band_outside_delta REAL NOT NULL DEFAULT 0, PRIMARY KEY (account_id, window)); -- rollback\n";

const UNDO_V35: &str = "ALTER TABLE pools DROP COLUMN auto_mode_server; ALTER TABLE api_keys DROP COLUMN auto_mode_server;";
const UNDO_V36: &str = "ALTER TABLE pins DROP COLUMN lane; ALTER TABLE affinity DROP COLUMN lane;";
const UNDO_V37: &str = "DROP TABLE usage_credits;";
// The reverse of `V38`'s column drop, so `UNDO_V33` can drop the column again on the replay; the rows `V38` deleted
// are not restored, since no older version needs them. The trailing `\n` ends the marker's comment.
const UNDO_V38: &str = "ALTER TABLE keepalive_budgets ADD COLUMN five_hour_share_unmeasured INTEGER NOT NULL DEFAULT 0; -- rollback\n";
// The reverse of `V39`: each set's dropped key back at the set's `main`, the value `V29` wrote it with, so a seeded
// version-38 store is faithful. One line with the marker last; the trailing `\n` ends the marker's comment.
const UNDO_V39: &str = "UPDATE pools SET cache_ttl_json = json_set(cache_ttl_json, '$.plan.keepalive', json_extract(cache_ttl_json, '$.plan.main'), '$.metered.keepalive', json_extract(cache_ttl_json, '$.metered.main')) WHERE json_valid(cache_ttl_json) AND json_type(cache_ttl_json, '$.plan') = 'object'; -- rollback\n";
// The reverse of `V40`: the kind prefix stripped from a session key. The replays run it over a memory table they
// never seed.
const UNDO_V40: &str =
    "UPDATE session_memory SET session_id = substr(session_id, 9) WHERE session_id LIKE 'session:%';";
// The reverse of `V41`: the one budget back with its period, the monthly preferred when a member carries both, so
// a seeded version-40 store is faithful. One line with the marker last; the trailing `\n` ends the marker's comment.
const UNDO_V41: &str = "ALTER TABLE pool_members ADD COLUMN budget_usd REAL; ALTER TABLE pool_members ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'month'; UPDATE pool_members SET budget_usd = COALESCE(budget_monthly_usd, budget_daily_usd), budget_period = CASE WHEN budget_monthly_usd IS NULL AND budget_daily_usd IS NOT NULL THEN 'day' ELSE 'month' END; ALTER TABLE pool_members DROP COLUMN budget_monthly_usd; ALTER TABLE pool_members DROP COLUMN budget_daily_usd; -- rollback\n";
const UNDO_V42: &str = "DROP INDEX IF EXISTS alert_deliveries_event; CREATE INDEX alert_deliveries_event ON alert_deliveries (event_id, id);";
// The reverse of `V43`, one statement per line, the marker on each line that restores a dropped name. Storing an
// unset diagnostics switch off needs no reverse, since its statement runs on any state.
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
        cache_diagnostics: false,
        auto_mode_server: true,
        affinity: Affinity::default(),
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
        auto_mode_server: None,
    }
}

fn member(account_id: &str, priority: u32) -> MemberWrite {
    MemberWrite {
        account_id: account_id.into(),
        priority,
        budget_daily_usd: None,
        budget_monthly_usd: None,
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
    assert_eq!(store.schema_version().unwrap(), 43);
    drop(store);
    let again = Store::open(&path, &key).unwrap();
    assert_eq!(
        again.schema_version().unwrap(),
        43,
        "a second open runs no migration and keeps the version"
    );
    assert!(again.pools().unwrap().is_empty());
    again
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    again.insert_pool(&pool("main"), "test").unwrap();
    assert_eq!(again.pools().unwrap()[0].cache_ttl_json, "{}");
    assert!(!again.pools().unwrap()[0].cache_diagnostics);
    drop(again);
    // What a database written before a cooldown recorded when it was set looks like on disk.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} INSERT INTO cooldowns (account_id, scope, until, reason, window, source_request_id) \
         VALUES ('a', 'account', 9000, 'rate_limit', 'five_hour', NULL); \
         UPDATE meta SET value = '8' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        43,
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} UPDATE meta SET value = '7' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        43,
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} UPDATE meta SET value = '4' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        43,
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} UPDATE meta SET value = '3' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        43,
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} DROP INDEX registry_changes_target; {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} \
         UPDATE meta SET value = '2' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        43,
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
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pricing_versions'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "V30 dropped the table the roll-back recreated on the same carried-up open"
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} UPDATE meta SET value = '24' WHERE key = 'schema_version'; {SEED_PRE_V25}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
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
    assert_eq!(pin.burn, PinBurn::default());
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

/// The pre-migration shape of five pools' `cache_ttl_json`, a pin under the three-column key, a member and a
/// key: a seed that restores the dropped shape plays the roll-back constant's role, so the sweep's marker
/// covers it the same way.
const SEED_PRE_V29: &str = r#"
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, created_at, updated_at) VALUES ('full', 'full', 1, 'priority', 'prefer_plan', '{}', '{"enabled":true,"in_overage":"5m","main":"1h","subagent":"1h","models":[{"model":"claude-haiku-4-5-20251001","main":"1h","subagent":"5m"}]}', 1, 1); -- rollback
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, created_at, updated_at) VALUES ('null-override', 'null-override', 1, 'priority', 'prefer_plan', '{}', '{"enabled":true,"in_overage":null,"main":"1h","subagent":"1h","models":[{"model":"claude-haiku-4-5-20251001","main":"1h","subagent":"5m"}]}', 1, 1); -- rollback
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, created_at, updated_at) VALUES ('partial-main', 'partial-main', 1, 'priority', 'prefer_plan', '{}', '{"enabled":true,"main":"1h"}', 1, 1); -- rollback
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, created_at, updated_at) VALUES ('partial-override', 'partial-override', 1, 'priority', 'prefer_plan', '{}', '{"in_overage":"1h"}', 1, 1); -- rollback
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, created_at, updated_at) VALUES ('empty', 'empty', 1, 'priority', 'prefer_plan', '{}', '{}', 1, 1); -- rollback
INSERT INTO pool_members (pool_id, account_id, priority) VALUES ('full', 'a', 0); -- rollback
INSERT INTO api_keys (id, label, pool_id, enabled, secret_digest, prefix, created_at) VALUES ('k', 'k', 'full', 1, x'00', '00000000', 1); -- rollback
INSERT INTO pins (key_id, session_id, model, account_id, epoch, created_at, last_used_at) VALUES ('k', 's', 'm', 'a', 1, 1, 1); -- rollback
"#;

#[test]
fn a_pin_a_pool_a_member_and_a_key_from_before_the_affinity_columns_read_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} UPDATE meta SET value = '28' WHERE key = 'schema_version'; {SEED_PRE_V29}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);

    let pin = migrated
        .load_routing()
        .unwrap()
        .pins
        .into_iter()
        .find(|p| p.key_id == "k")
        .unwrap();
    assert_eq!(pin.agent_id, "main");

    let pools = migrated.pools().unwrap();
    for pool in &pools {
        assert_eq!(pool.affinity, Affinity::default(), "{}", pool.id);
        assert!(
            !pool.cache_ttl_json.contains("in_overage"), // -- rollback
            "{}: the old key does not survive the rewrite",
            pool.id
        );
    }
    let policy = |id: &str| {
        phoebis_relay::CacheTtlPolicy::from_json(
            &pools.iter().find(|p| p.id == id).unwrap().cache_ttl_json,
        )
    };
    let haiku = phoebis_relay::ModelTtl {
        model: "claude-haiku-4-5-20251001".into(),
        main: phoebis_relay::CacheTtl::OneHour,
        subagent: phoebis_relay::CacheTtl::FiveMinutes,
    };

    let full = policy("full");
    assert!(full.enabled);
    assert_eq!(
        full.metered,
        phoebis_relay::TtlRules {
            main: phoebis_relay::CacheTtl::FiveMinutes,
            models: Vec::new(),
            subagent: phoebis_relay::CacheTtl::FiveMinutes,
        },
        "the override's TTL throughout, no per-model rule"
    );
    assert!(
        pools
            .iter()
            .find(|p| p.id == "full")
            .unwrap()
            .cache_ttl_json
            .contains(r#""metered":{"main":"5m","models":[],"subagent":"5m"}"#),
        "the migration itself wrote the metered set, not a reader's default standing in for a missing one"
    );
    assert_eq!(
        full.plan,
        phoebis_relay::TtlRules {
            main: phoebis_relay::CacheTtl::OneHour,
            models: vec![haiku.clone()],
            subagent: phoebis_relay::CacheTtl::OneHour,
        },
        "the old main, subagent and models"
    );

    let null_override = policy("null-override");
    assert!(null_override.enabled);
    assert_eq!(
        null_override.metered, null_override.plan,
        "a null override reads no override, so metered equals plan"
    );
    assert_eq!(null_override.plan.models, vec![haiku.clone()]);

    let partial_main = policy("partial-main");
    assert!(partial_main.enabled);
    assert_eq!(partial_main.plan, phoebis_relay::TtlRules::plan_default());
    assert_eq!(
        partial_main.metered,
        phoebis_relay::TtlRules::metered_default(),
        "the omitted override was the old five-minute default"
    );

    let partial_override = policy("partial-override");
    assert_eq!(
        partial_override.plan,
        phoebis_relay::TtlRules::plan_default()
    );
    assert_eq!(
        partial_override.metered,
        phoebis_relay::TtlRules {
            main: phoebis_relay::CacheTtl::OneHour,
            models: Vec::new(),
            subagent: phoebis_relay::CacheTtl::OneHour,
        },
        "the sole override names an hour, so metered reads it throughout"
    );

    let empty = policy("empty");
    assert_eq!(empty, phoebis_relay::CacheTtlPolicy::default());
    assert_eq!(
        pools
            .iter()
            .find(|p| p.id == "empty")
            .unwrap()
            .cache_ttl_json,
        "{}",
        "a row holding the default is left alone"
    );

    let member = migrated.pool_members("full").unwrap().pop().unwrap();
    assert_eq!(
        (member.budget_daily_usd, member.budget_monthly_usd),
        (None, None)
    );

    let key = migrated.api_key("k").unwrap().unwrap();
}

/// A version-38 pool's `cache_ttl_json` as the app wrote it then, the `keepalive` key in both sets: a seed that
/// restores the dropped shape plays the roll-back constant's role, so the sweep's marker covers it the same way.
const SEED_PRE_V39: &str = r#"
INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, created_at, updated_at) VALUES ('warm', 'warm', 1, 'priority', 'prefer_plan', '{}', '{"enabled":true,"inject_when_absent":false,"metered":{"keepalive":"1h","main":"1h","models":[],"subagent":"1h"},"plan":{"keepalive":"5m","main":"5m","models":[],"subagent":"5m"}}', 1, 1); -- rollback
"#;

#[test]
fn a_stored_policy_with_the_retired_keepalive_key_reads_as_set_after_the_migration() {
    // The reader refuses an unknown field, so without the migration the whole policy would read as off.
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} UPDATE meta SET value = '38' WHERE key = 'schema_version'; {SEED_PRE_V39}"
    ))
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(store.schema_version().unwrap(), 43);
    let policy = phoebis_relay::CacheTtlPolicy::from_json(
        &store.pool("warm").unwrap().unwrap().cache_ttl_json,
    );
    assert!(policy.enabled, "the policy survives the key's removal");
    assert_eq!(
        policy.plan,
        phoebis_relay::TtlRules {
            main: phoebis_relay::CacheTtl::FiveMinutes,
            models: Vec::new(),
            subagent: phoebis_relay::CacheTtl::FiveMinutes,
        },
        "the plan set keeps its stored branches, not the defaults"
    );
    assert_eq!(
        policy.metered,
        phoebis_relay::TtlRules {
            main: phoebis_relay::CacheTtl::OneHour,
            models: Vec::new(),
            subagent: phoebis_relay::CacheTtl::OneHour,
        },
        "the metered set keeps its stored branches, not the defaults"
    );
}

/// Two version-40 members, each with the one budget and its period that version carried: a seed that restores the
/// dropped shape plays the roll-back constant's role, so the sweep's marker covers it the same way.
const SEED_PRE_V41: &str = r#"
INSERT INTO pool_members (pool_id, account_id, priority, budget_usd, budget_period) VALUES ('main', 'a', 0, 20.0, 'day'); -- rollback
INSERT INTO pool_members (pool_id, account_id, priority, budget_usd, budget_period) VALUES ('main', 'b', 1, 50.0, 'month'); -- rollback
"#;

#[test]
fn a_member_budget_with_its_period_lands_in_the_column_the_period_names() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store
        .insert_account(&account("b", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} UPDATE meta SET value = '40' WHERE key = 'schema_version'; {SEED_PRE_V41}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    let members = migrated.pool_members("main").unwrap();
    let budgets_of = |id: &str| {
        let m = members.iter().find(|m| m.account_id == id).unwrap();
        (m.budget_daily_usd, m.budget_monthly_usd)
    };
    assert_eq!(
        budgets_of("a"),
        (Some(20.0), None),
        "a day's budget lands in the daily column alone"
    );
    assert_eq!(
        budgets_of("b"),
        (None, Some(50.0)),
        "a month's budget lands in the monthly column alone"
    );
    drop(migrated);
    assert_eq!(
        columns_of(&path, "pool_members"),
        [
            "account_id",
            "budget_daily_usd",
            "budget_monthly_usd",
            "pool_id",
            "priority"
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        "the one budget and its period leave the table"
    );
}

/// A pool that chose to hide the header families, planted at version 42.
const SEED_PRE_V43: &str = r#"
UPDATE pools SET quota_headers = 'hide' WHERE id = 'main'; -- rollback
"#;

#[test]
fn a_store_left_at_42_is_carried_to_43_without_the_pools_header_switch() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    assert_eq!(
        migrated
            .pools()
            .unwrap()
            .iter()
            .map(|p| (p.id.as_str(), p.label.as_str()))
            .collect::<Vec<_>>(),
        [("main", "main")],
        "the pool reads back"
    );
    drop(migrated);
    let columns = columns_of(&path, "pools");
    assert!(!columns.contains("quota_headers"), "{columns:?}"); // removed name
}

/// A key that capped its requests and fixed its source, planted at version 42.
const SEED_PRE_V43_KEY: &str = r#"
UPDATE api_keys SET in_flight_cap = 3, affinity_source = 'content' WHERE id = 'k'; -- rollback
"#;

#[test]
fn a_store_left_at_42_is_carried_to_43_without_the_keys_cap_or_fixed_source() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "phoebis-k-secret"), "test")
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43_KEY}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &storage_key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    let read = migrated.api_key("k").unwrap().expect("the key reads back");
    assert_eq!((read.label.as_str(), read.pool_id.as_str()), ("k", "main"));
    drop(migrated);
    let columns = columns_of(&path, "api_keys");
    assert!(!columns.contains("in_flight_cap"), "{columns:?}"); // removed name
    assert!(!columns.contains("affinity_source"), "{columns:?}"); // removed name
}

/// A pool whose diagnostics switch was left unset, planted at version 42.
const SEED_PRE_V43_DIAGNOSTICS: &str = r#"
UPDATE pools SET cache_diagnostics = NULL WHERE id = 'main'; -- rollback
"#;

#[test]
fn a_store_left_at_42_is_carried_to_43_with_every_unset_diagnostics_switch_off() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43_DIAGNOSTICS}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    let pools = migrated.pools().expect("an unset switch reads as a bool");
    assert_eq!(
        pools
            .iter()
            .map(|p| (p.id.as_str(), p.cache_diagnostics))
            .collect::<Vec<_>>(),
        [("main", false)],
        "the unset switch reads off"
    );
}

/// A rule, its open event and the event's delivery, planted at version 42.
const SEED_PRE_V43_TABLES: &str = r#"
INSERT INTO alert_rules (id, kind, scope_type, webhook_url, created_at, updated_at) VALUES ('r', 'pool_exhausted', 'all', 'http://127.0.0.1:9/hook', 1, 1); -- rollback
INSERT INTO alert_events (id, rule_id, kind, subject, state, opened_at) VALUES ('e', 'r', 'pool_exhausted', 'main', 'open', 2); -- rollback
INSERT INTO alert_deliveries (event_id, rule_id, kind, subject, state, attempt, at) VALUES ('e', 'r', 'pool_exhausted', 'main', 'opened', 1, 3); -- rollback
"#;

/// Every table and index the store's schema names, so a test can see what a migration left.
const SCHEMA_OBJECTS: &str =
    "SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name";

#[test]
fn a_store_left_at_42_is_carried_to_43_without_the_rule_event_and_delivery_tables() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43_TABLES}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    drop(migrated);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let objects: Vec<String> = conn
        .prepare(SCHEMA_OBJECTS)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let dropped = ["alert_deliveries", "alert_events", "alert_rules"]; // removed name
    assert!(
        dropped.iter().all(|t| !objects.contains(&t.to_string())),
        "the three tables are gone: {objects:?}"
    );
    assert!(
        !objects.iter().any(|n| n.starts_with("alert_")), // removed name
        "and their indexes with them: {objects:?}"
    );
    assert!(
        objects.contains(&"refresh_attempts_holder_resolved".to_string()),
        "the holder's own refresh index is in place: {objects:?}"
    );
}

#[test]
fn the_pins_columns_after_the_rebuild_are_the_old_set_plus_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    Store::open(&path, &StorageKey::generate()).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let columns: std::collections::BTreeSet<String> = conn
        .prepare("SELECT name FROM pragma_table_info('pins')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut expected: std::collections::BTreeSet<String> = [
        "key_id",
        "session_id",
        "model",
        "account_id",
        "epoch",
        "created_at",
        "last_used_at",
        "key_generation",
        "incarnation",
        "burn_at",
        "last_input_tokens",
        "last_refresh_at",
        "cache_ttl_ms",
        "shed_from",
        "shed_at",
        "static_hash",
        "tail_hash",
        "lane",
        "burn_banded_cache_read",
        "burn_banded_input",
        "burn_banded_output",
        "burn_banded_write_1h",
        "burn_banded_write_5m",
        "burn_cache_read",
        "burn_input",
        "burn_output",
        "burn_write_1h",
        "burn_write_5m",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    expected.insert("agent_id".into());
    assert_eq!(columns, expected);
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} UPDATE meta SET value = '23' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        43,
        "the direct 23 to 28 open runs every migration between"
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

fn has_windows_table(path: &std::path::Path) -> bool {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'usage_windows'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn a_fresh_store_carries_the_windows_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    Store::open(&path, &StorageKey::generate()).unwrap();
    assert!(has_windows_table(&path));
}

#[test]
fn a_store_left_at_31_is_carried_to_32_with_the_windows_table() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} UPDATE meta SET value = '31' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    assert!(
        !has_windows_table(&path),
        "the roll-back left the store at 31"
    );
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    drop(migrated);
    assert!(has_windows_table(&path), "V32 created the table");
}

fn columns_of(path: &std::path::Path, table: &str) -> std::collections::BTreeSet<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .unwrap();
    statement
        .query_map([table], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A pin with both old burn figures and a day's plan-side budget, planted at version 32 after the roll-back.
const HELD_AT_32: &str = "INSERT INTO pins (key_id, session_id, agent_id, model, account_id, epoch, created_at, last_used_at, burn, burn_at, burn_usd) VALUES ('k', 's', 'main', 'claude-opus-5', 'a', 1, 1, 1, 120.0, 5, 0.75); \
     INSERT INTO keepalive_budgets (day, scope, id, usd, meter_tokens) VALUES ('2026-09-29', 'account', 'a', 0.0, 520.0); -- rollback\n";

#[test]
fn a_store_left_at_32_is_carried_to_33_with_the_typed_burn_restarted_and_the_share_budget() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} UPDATE meta SET value = '32' WHERE key = 'schema_version'; {HELD_AT_32}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    let pin = migrated.load_routing().unwrap().pins.pop().unwrap();
    assert_eq!(
        (pin.burn, pin.burn_at, pin.burn_banded),
        (PinBurn::default(), Some(5), PinBurn::default()),
        "both copies restart at zero, since the old averages held another unit"
    );
    assert_eq!(
        pin.lane, "",
        "a pin from before the lane column names none, so its next request stamps its own"
    );
    let budgets = migrated.keepalive_budgets("2026-09-29").unwrap();
    assert_eq!(
        budgets
            .iter()
            .map(|b| b.five_hour_share)
            .collect::<Vec<_>>(),
        vec![0.0],
        "the day's row keeps its place and its share starts at zero"
    );
    drop(migrated);
    assert_eq!(
        columns_of(&path, "keepalive_budgets"),
        ["day", "five_hour_share", "id", "scope", "usd"]
            .into_iter()
            .map(String::from)
            .collect(),
        "the budget's plan side is the share alone"
    );
}

fn has_fit_table(path: &std::path::Path) -> bool {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'plan_fits'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn a_store_left_at_33_is_carried_to_34_without_the_fit_table() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} UPDATE meta SET value = '33' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    assert!(has_fit_table(&path), "the roll-back left the store at 33");
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    drop(migrated);
    assert!(!has_fit_table(&path), "V34 dropped the table");
}

#[test]
fn a_pool_reopens_on_and_a_key_reopens_inheriting_until_set_off() {
    let dir = tempfile::tempdir().unwrap();
    let key_store = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key_store).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let desktop = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&desktop, "test").unwrap();
    drop(store);
    let store = Store::open(&path, &key_store).unwrap();
    assert!(store.pool("main").unwrap().unwrap().auto_mode_server);
    let mut read = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(read.auto_mode_server, None, "a new key inherits the pool's");
    read.auto_mode_server = Some(false);
    store.update_api_key(&read, "test").unwrap();
    drop(store);
    let store = Store::open(&path, &key_store).unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().auto_mode_server,
        Some(false)
    );
    let change = store
        .registry_changes_after(0, 100)
        .unwrap()
        .into_iter()
        .rfind(|c| c.kind == "key_updated")
        .unwrap();
    let diff: serde_json::Value = serde_json::from_str(&change.diff_json).unwrap();
    assert_eq!(diff["auto_mode_server"], serde_json::json!(false));
}

#[test]
fn a_registry_from_before_the_auto_mode_columns_reopens_every_pool_on_and_every_key_inheriting() {
    let dir = tempfile::tempdir().unwrap();
    let key_store = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key_store).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let desktop = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&desktop, "test").unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} UPDATE meta SET value = '34' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key_store).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    assert!(migrated.pool("main").unwrap().unwrap().auto_mode_server);
    assert_eq!(
        migrated
            .api_key("desktop")
            .unwrap()
            .unwrap()
            .auto_mode_server,
        None
    );
}

/// Planted at version 37: a day's budget row with its share flagged. One line, so the marker that ends it covers the
/// statement.
const HELD_AT_37: &str = "INSERT INTO keepalive_budgets (day, scope, id, usd, five_hour_share, five_hour_share_unmeasured) VALUES ('2026-09-30', 'account', 'a', 0.0, 0.25, 1); -- rollback\n";

#[test]
fn a_store_left_at_37_is_carried_to_38_without_the_budget_flag() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} UPDATE meta SET value = '37' WHERE key = 'schema_version'; {HELD_AT_37}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 43);
    let budgets = migrated.keepalive_budgets("2026-09-30").unwrap();
    assert_eq!(
        budgets
            .iter()
            .map(|b| b.five_hour_share)
            .collect::<Vec<_>>(),
        vec![0.25],
        "the day's row keeps its share"
    );
    drop(migrated);
    assert_eq!(
        columns_of(&path, "keepalive_budgets"),
        ["day", "five_hour_share", "id", "scope", "usd"]
            .into_iter()
            .map(String::from)
            .collect(),
        "the budget's flag column is dropped"
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
        .insert_api_key(
            &key(&store, "back", "main", "phoebis-back-reissued"),
            "test",
        )
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
        .insert_api_key(
            &key(&store, "back", "main", "phoebis-back-reissued"),
            "test",
        )
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
        "{UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} \
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
    assert_eq!(migrated.schema_version().unwrap(), 43);
    migrated
        .insert_api_key(
            &key(&migrated, "old", "main", "phoebis-old-reissued"),
            "test",
        )
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
        .insert_api_key(
            &key(&store, "back", "main", "phoebis-back-reissued"),
            "test",
        )
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
        .set_pool_members("main", &[member("a", 2)], "test")
        .unwrap();
    assert_eq!(store.pool_members("main").unwrap()[0].priority, 2);
    assert!(matches!(
        store.set_pool_members("main", &[member("ghost", 0)], "test"),
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
        .set_pool_members("main", &[member("a", 0)], "test")
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
