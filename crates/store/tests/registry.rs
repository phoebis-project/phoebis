use phoebis_store::{
    answers_stamp_token, stamp_token, AccountKind, AccountRow, AccountState, AccountUpdate,
    Affinity, AffinityChain, ApiKeyRow, CanaryRead, Failover, Identity, MemberWrite,
    MemoryKey, MemoryRecord, MemoryStore, ModelRule, NewBundle, Overlap, PinBurn, PinRow, PoolRow,
    RulesMode, Secret, Shedding, StateCause, StorageKey, Store, StoreError, Strategy,
    SubagentPlacement, SubscriptionRow,
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
// version-38 store is faithful. One line with the marker last. The trailing `\n` ends the marker's comment.
const UNDO_V39: &str = "UPDATE pools SET cache_ttl_json = json_set(cache_ttl_json, '$.plan.keepalive', json_extract(cache_ttl_json, '$.plan.main'), '$.metered.keepalive', json_extract(cache_ttl_json, '$.metered.main')) WHERE json_valid(cache_ttl_json) AND json_type(cache_ttl_json, '$.plan') = 'object'; -- rollback\n";
// The reverse of `V40`: the kind prefix stripped from a session key. The replays run it over a memory table they
// never seed.
const UNDO_V40: &str =
    "UPDATE session_memory SET session_id = substr(session_id, 9) WHERE session_id LIKE 'session:%';";
// The reverse of `V41`: the one budget back with its period, the monthly preferred when a member carries both, so
// a seeded version-40 store is faithful. One line with the marker last. The trailing `\n` ends the marker's comment.
const UNDO_V41: &str = "ALTER TABLE pool_members ADD COLUMN budget_usd REAL; ALTER TABLE pool_members ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'month'; UPDATE pool_members SET budget_usd = COALESCE(budget_monthly_usd, budget_daily_usd), budget_period = CASE WHEN budget_monthly_usd IS NULL AND budget_daily_usd IS NOT NULL THEN 'day' ELSE 'month' END; ALTER TABLE pool_members DROP COLUMN budget_monthly_usd; ALTER TABLE pool_members DROP COLUMN budget_daily_usd; -- rollback\n";
const UNDO_V42: &str = "DROP INDEX IF EXISTS alert_deliveries_event; CREATE INDEX alert_deliveries_event ON alert_deliveries (event_id, id);";
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
// The reverse of `V50`: the table `V28` created, as `V33` and `V38` left it, so the chains below replay `UNDO_V38` and
// `UNDO_V33` over it unchanged. One line with the marker last. The trailing `\n` ends the marker's comment.
const UNDO_V50: &str = "CREATE TABLE keepalive_budgets (day TEXT NOT NULL, scope TEXT NOT NULL, id TEXT NOT NULL, usd REAL NOT NULL DEFAULT 0, five_hour_share REAL NOT NULL DEFAULT 0, PRIMARY KEY (day, scope, id)); -- rollback\n";
// The reverse of `V49`: the keepalive engine's three tables.
const UNDO_V49: &str =
    "DROP TABLE keepalive_idle_returns; DROP TABLE keepalive_agents; DROP TABLE keepalive_sessions;";
// The reverse of `V48`, one statement per line. The key tombstones go too, so a replay that reaches `V14` creates
// that version's table of the same name rather than meeting this one's columns.
const UNDO_V48: &str = r#"
ALTER TABLE pools DROP COLUMN incarnation;
ALTER TABLE api_keys DROP COLUMN incarnation;
DROP TABLE pool_tombstones;
DROP TABLE api_key_tombstones;
"#;
// The reverse of `V47`. One line with the marker last, and the trailing `\n` ends the marker's comment.
const UNDO_V47: &str = "DROP INDEX pool_members_one_pool; -- rollback\n";
// The reverse of `V46`, one statement per line.
const UNDO_V46: &str = r#"
DROP TABLE shed_rounds;
ALTER TABLE affinity DROP COLUMN depth;
ALTER TABLE accounts DROP COLUMN state_cause;
"#;
// The reverse of `V45`. One line with the marker last, since it restores a dropped name. The trailing `\n` ends the
// marker's comment.
const UNDO_V45: &str =
    "ALTER TABLE pools RENAME COLUMN shed_ahead_of_pace TO migrate_above_utilization; -- rollback\n";
// The reverse of `V44`, one statement per line. The request and response links it deleted are not restored, since
// no older version needs them; the index goes before `UNDO_V20` drops the column it names.
const UNDO_V44: &str = r#"
DROP INDEX refresh_attempts_account;
ALTER TABLE api_keys DROP COLUMN updated_at;
"#;
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
        state_cause: StateCause::Operator,
    }
}

fn pool(id: &str) -> PoolRow {
    PoolRow {
        strategy: Strategy::Priority,
        prefer_subscription: false,
        created_at: 1,
        updated_at: 1,
        ..PoolRow::for_tests(id)
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
        incarnation: 1,
        created_at: 1,
        updated_at: 0,
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

/// A label edit through the one account write, fenced on the revision the caller read. The incarnation and the
/// generation are the row's own at call time, so only the revision half of the admin route's fence can refuse here.
fn rename(
    store: &Store,
    id: &str,
    revision: u64,
    label: &str,
    actor: &str,
) -> Result<(), StoreError> {
    let (incarnation, generation) = store
        .account(id)?
        .map_or((0, 0), |row| (row.incarnation, row.lifecycle_generation));
    store.commit_account_update(&AccountUpdate {
        id,
        expected_revision: revision,
        incarnation,
        lifecycle_generation: generation,
        label,
        secret: None,
        state: None,
        at: jiff::Timestamp::now().as_millisecond(),
        actor,
        enabled: None,
        cancellation: None,
    })
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
    assert_eq!(store.schema_version().unwrap(), 54);
    drop(store);
    let again = Store::open(&path, &key).unwrap();
    assert_eq!(
        again.schema_version().unwrap(),
        54,
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} INSERT INTO cooldowns (account_id, scope, until, reason, window, source_request_id) \
         VALUES ('a', 'account', 9000, 'rate_limit', 'five_hour', NULL); \
         UPDATE meta SET value = '8' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        54,
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} UPDATE meta SET value = '7' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        54,
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
        0,
        "and the column it was missing came back on the way up, since V54 drops it and a drop of a missing column fails"
    );
    drop(conn);
    // What a database written before account incarnations existed looks like on disk.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} UPDATE meta SET value = '4' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        54,
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} UPDATE meta SET value = '3' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        54,
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} DROP INDEX registry_changes_target; {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} {UNDO_V23} {UNDO_V22} {UNDO_V21} {UNDO_V20} {UNDO_V17} {UNDO_V13} {UNDO_V12} {UNDO_V11} {UNDO_V9} {UNDO_V8} {UNDO_V5} {UNDO_V4} \
         UPDATE meta SET value = '2' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        54,
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} UPDATE meta SET value = '24' WHERE key = 'schema_version'; {SEED_PRE_V25}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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
    assert_eq!(
        legacy.shedding,
        Shedding {
            load_shedding: false,
            burn_average_seconds: 600,
            ..Shedding::default()
        },
        "a pool from before the columns keeps the burn window's column default, and its unset weekly lead reads the \
         switch off"
    );
    assert_eq!(
        legacy.failover,
        Failover {
            retry_in_place_seconds: 10,
            ..Failover::default()
        },
        "a pool from before the column keeps the column's own default, as an existing pool keeps its stored bound"
    );
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} UPDATE meta SET value = '28' WHERE key = 'schema_version'; {SEED_PRE_V29}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);

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

#[test]
fn a_pool_inserted_without_a_placement_reads_by_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .insert_pool(
            &PoolRow {
                affinity: Affinity::default(),
                ..PoolRow::for_tests("built")
            },
            "test",
        )
        .unwrap();
    assert_eq!(
        store
            .pool("built")
            .unwrap()
            .unwrap()
            .affinity
            .subagent_placement,
        SubagentPlacement::ByStrategy,
        "the row builder's default"
    );
    drop(store);
    // Every store insert names the column, so only a raw row reads the column's own default.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO pools (id, label, enabled, strategy, overage_policy, keepalive_json, cache_ttl_json, cache_diagnostics, created_at, updated_at) VALUES ('raw', 'raw', 1, 'priority', 'prefer_plan', '{}', '{}', 0, 1, 1);",
    )
    .unwrap();
    drop(conn);
    let reopened = Store::open(&path, &key).unwrap();
    let raw = reopened.pool("raw").unwrap().unwrap();
    assert_eq!(
        raw.affinity.subagent_placement,
        SubagentPlacement::ByStrategy,
        "the column's default"
    );
    assert_eq!(
        raw.affinity.subagent_placement,
        Affinity::default().subagent_placement,
        "the column's default and the row builder's agree"
    );
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} UPDATE meta SET value = '38' WHERE key = 'schema_version'; {SEED_PRE_V39}"
    ))
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(store.schema_version().unwrap(), 54);
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} UPDATE meta SET value = '40' WHERE key = 'schema_version'; {SEED_PRE_V41}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43_KEY}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &storage_key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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
    store
        .insert_pool(
            &PoolRow {
                cache_diagnostics: true,
                ..pool("on")
            },
            "test",
        )
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43_DIAGNOSTICS}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    let pools = migrated.pools().expect("an unset switch reads as a bool");
    assert_eq!(
        pools
            .iter()
            .map(|p| (p.id.as_str(), p.cache_diagnostics))
            .collect::<Vec<_>>(),
        [("main", false), ("on", true)],
        "the unset switch reads off and a set one keeps its state"
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} UPDATE meta SET value = '42' WHERE key = 'schema_version'; {SEED_PRE_V43_TABLES}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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

/// A message link beside a request link and a response link under the key `k` and the account `a`, planted at
/// version 43.
const SEED_PRE_V44: &str = r#"
INSERT INTO affinity (model, hash, chain, account_id, incarnation, written_at, ttl_ms, prefix_tokens, key_id, key_generation, root, lane) VALUES ('m', 'b1', 'message', 'a', 1, 1, 0, 0, 'k', 1, 'b1', '');
INSERT INTO affinity (model, hash, chain, account_id, incarnation, written_at, ttl_ms, prefix_tokens, key_id, key_generation, root, lane) VALUES ('m', 'msg_1', 'response', 'a', 1, 1, 0, 0, 'k', 1, 'b1', '');
INSERT INTO affinity (model, hash, chain, account_id, incarnation, written_at, ttl_ms, prefix_tokens, key_id, key_generation, root, lane) VALUES ('m', 'req_1', 'request', 'a', 1, 1, 0, 0, 'k', 1, 'b1', '');
"#;

#[test]
fn a_store_left_at_43_is_carried_to_44_with_the_key_column_the_index_and_no_parent_chain_rows() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "phoebis-k-secret"), "test")
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} UPDATE meta SET value = '43' WHERE key = 'schema_version'; {SEED_PRE_V44}"
    ))
    .unwrap();
    drop(conn);
    let wall_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let before = wall_ms();
    let migrated = Store::open(&path, &storage_key).unwrap();
    let after = wall_ms();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    assert_eq!(
        migrated
            .load_routing()
            .expect("every stored chain parses")
            .affinity
            .iter()
            .map(|row| (row.hash.as_str(), row.chain))
            .collect::<Vec<_>>(),
        [("b1", AffinityChain::Message)],
        "the request and response links leave and the message link stays"
    );
    drop(migrated);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let stamped: Vec<i64> = conn
        .prepare("SELECT updated_at FROM api_keys")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // The migration stamps whole seconds of the wall clock in milliseconds.
    assert_eq!(stamped.len(), 1);
    assert!(
        stamped
            .iter()
            .all(|at| (before - before % 1_000..=after).contains(at)),
        "every key carries the migration's instant: {stamped:?} outside {before}..={after}"
    );
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_index_info('refresh_attempts_account') ORDER BY seqno")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        columns,
        ["account_id", "incarnation", "id"],
        "the index a holder's newest attempt is read through"
    );
}

#[test]
fn a_pool_cap_written_under_the_old_column_reads_through_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} UPDATE pools SET shed_share = 0.25 WHERE id = 'main'; \
         UPDATE meta SET value = '53' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    assert_eq!(
        migrated.pool("main").unwrap().unwrap().shedding.shed_cap,
        0.25,
        "the value written under the old column is read through the new one"
    );
}

#[test]
fn a_registry_holding_an_account_in_two_pools_refuses_to_migrate_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("p1"), "test").unwrap();
    store.insert_pool(&pool("p2"), "test").unwrap();
    store
        .set_pool_members("p1", &[member("a", 0)], "test")
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} UPDATE meta SET value = '46' WHERE key = 'schema_version'; \
         INSERT INTO pool_members (pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd) \
         VALUES ('p2', 'a', 0, NULL, NULL);"
    ))
    .unwrap();
    drop(conn);
    let overlap = Overlap {
        account_id: "a".into(),
        pool_ids: vec!["p1".into(), "p2".into()],
    };
    let Err(refused) = Store::open(&path, &key) else {
        panic!("a registry with an account in two pools opened");
    };
    assert!(
        matches!(&refused, StoreError::AccountsInPools(overlaps) if overlaps == std::slice::from_ref(&overlap)),
        "{refused:?}"
    );
    assert_eq!(
        refused.to_string(),
        "account a is a member of pools p1 and p2, and an account belongs to one pool"
    );
    assert_eq!(
        Store::read_schema_version(&path).unwrap(),
        Some(46),
        "the refused open migrated nothing"
    );
    assert_eq!(
        Store::read_overlapping_memberships(&path).unwrap(),
        [overlap],
        "the read-only read finds the same overlap"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("DELETE FROM pool_members WHERE pool_id = 'p2'", [])
        .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
}

#[test]
fn migration_46_adds_the_shed_rounds_the_affinity_depth_and_the_state_cause_on_a_version_45_registry(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} UPDATE meta SET value = '45' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    drop(migrated);
    let conn = rusqlite::Connection::open(&path).unwrap();
    // `(name, type, notnull, dflt_value)` of each column, in the table's order.
    let columns = |table: &str| -> Vec<(String, String, bool, Option<String>)> {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    // The table V46 created, as the newest migration leaves it: one row per pool and account with the round's time.
    assert_eq!(
        columns("shed_rounds")
            .into_iter()
            .map(|(name, ..)| name)
            .collect::<Vec<_>>(),
        ["pool_id", "account_id", "incarnation", "round_at"]
    );
    assert_eq!(
        columns("affinity")
            .into_iter()
            .find(|(name, ..)| name == "depth"),
        Some((
            "depth".to_string(),
            "INTEGER".to_string(),
            true,
            Some("0".to_string())
        ))
    );
    assert_eq!(
        columns("accounts")
            .into_iter()
            .find(|(name, ..)| name == "state_cause"),
        Some((
            "state_cause".to_string(),
            "TEXT".to_string(),
            true,
            Some("'unrecorded'".to_string())
        ))
    );
    let cause: String = conn
        .query_row("SELECT state_cause FROM accounts WHERE id = 'a'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        cause, "unrecorded",
        "an account stored before the column reads the default"
    );
}

fn has_budgets_table(path: &std::path::Path) -> bool {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'keepalive_budgets'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn migration_50_drops_the_budgets_table_and_the_undo_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    drop(Store::open(&path, &key).unwrap());
    assert!(
        !has_budgets_table(&path),
        "a fresh store is prepared without the table"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} UPDATE meta SET value = '49' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    assert!(
        has_budgets_table(&path),
        "the undo restores the table a version-49 store holds"
    );
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    drop(migrated);
    assert!(!has_budgets_table(&path), "V50 dropped the table");
}

const FAST_COLUMNS: [&str; 5] = [
    "burn_fast_cache_read",
    "burn_fast_input",
    "burn_fast_output",
    "burn_fast_write_1h",
    "burn_fast_write_5m",
];

#[test]
fn a_store_left_at_50_is_carried_to_52_and_a_pins_fast_burn_reads_back_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} UPDATE meta SET value = '50' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let rewound = columns_of(&path, "pins");
    assert!(
        FAST_COLUMNS.iter().all(|c| !rewound.contains(*c)),
        "the undo leaves the pins of a version-50 store without the fast columns: {rewound:?}"
    );
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    let fast = PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        agent_id: "main".into(),
        model: "claude-opus-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        key_generation: 1,
        incarnation: 1,
        burn: PinBurn::default(),
        burn_at: Some(5),
        burn_banded: PinBurn::default(),
        burn_fast: PinBurn {
            cache_read: 0.5,
            input: 1.5,
            output: 2.5,
            write_1h: 3.5,
            write_5m: 4.5,
        },
        last_input_tokens: 1_000,
        last_refresh_at: 1,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
        lane: String::new(),
        marked_at: None,
        marked_on: None,
        marked_window: None,
    };
    migrated.upsert_pin(&fast).unwrap();
    assert_eq!(
        migrated.load_routing().unwrap().pins,
        vec![fast],
        "the fast copy reads back with every type as written"
    );
    drop(migrated);
    let columns = columns_of(&path, "pins");
    assert!(
        FAST_COLUMNS.iter().all(|c| columns.contains(*c)),
        "V52 added the five fast columns: {columns:?}"
    );
}

#[test]
fn migration_53_moves_the_marks_onto_the_pins_and_keeps_one_round_row_per_pool_and_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    drop(store);
    let marks = ["marked_at", "marked_on", "marked_window"];
    let pins = columns_of(&path, "pins");
    assert!(
        marks.iter().all(|c| pins.contains(*c)),
        "V53 added the three mark columns to the pins: {pins:?}"
    );
    assert_eq!(
        columns_of(&path, "shed_rounds"),
        ["account_id", "incarnation", "pool_id", "round_at"]
            .into_iter()
            .map(String::from)
            .collect(),
        "V53 rebuilt the rounds table with one row per pool and account"
    );
    // A registry at 52 holding a round row of the old shape: V53 drops the row with the table.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} UPDATE meta SET value = '52' WHERE key = 'schema_version';"
    ))
    .unwrap();
    conn.execute_batch(
        "INSERT INTO shed_rounds (pool_id, account_id, incarnation, key_id, session_id, agent_id, model, epoch, round_at, marked) \
         VALUES ('p', 'a', 1, 'k', 's1', 'main', 'claude-sonnet-5', 1, 1000, 1);",
    )
    .unwrap();
    drop(conn);
    let rewound = columns_of(&path, "pins");
    assert!(
        marks.iter().all(|c| !rewound.contains(*c)),
        "the undo leaves the pins of a version-52 store without the mark columns: {rewound:?}"
    );
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    assert!(
        migrated.load_routing().unwrap().shed_rounds.is_empty(),
        "the round written under 52 is gone with its table"
    );
    drop(migrated);
    let pins = columns_of(&path, "pins");
    assert!(marks.iter().all(|c| pins.contains(*c)), "{pins:?}");
    assert_eq!(
        columns_of(&path, "shed_rounds"),
        ["account_id", "incarnation", "pool_id", "round_at"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

#[test]
fn migration_54_renames_the_pool_fields_and_clamps_a_value_outside_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store.insert_pool(&pool("low"), "test").unwrap();
    store.insert_pool(&pool("lead"), "test").unwrap();
    store.insert_pool(&pool("week"), "test").unwrap();
    drop(store);
    // A registry at 53: `low` holds a cap and a burn window under the new floors and no weekly lead, `lead` a lead
    // that is a number, `week` a burn window of a week, past the new ceiling.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} UPDATE pools SET shed_share = 0.05, burn_window_seconds = 120, shed_ahead_of_pace = NULL WHERE id = 'low'; \
         UPDATE pools SET shed_ahead_of_pace = 0.2 WHERE id = 'lead'; \
         UPDATE pools SET burn_window_seconds = 604800 WHERE id = 'week'; \
         UPDATE meta SET value = '53' WHERE key = 'schema_version';"
    ))
    .unwrap();
    let logged = |conn: &rusqlite::Connection| {
        conn.query_row("SELECT COUNT(*) FROM registry_changes", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let before = logged(&conn);
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    drop(migrated);
    let columns = columns_of(&path, "pools");
    assert!(
        [
            "burn_average_seconds",
            "load_shedding",
            "shed_cap",
            "shed_dwell_seconds",
            "window_ceiling"
        ]
        .iter()
        .all(|c| columns.contains(*c)),
        "V54 names the pool fields by their meaning: {columns:?}"
    );
    assert!(
        [
            "burn_window_seconds",
            "pace_floor",
            "select_below_utilization",
            "shed_ahead_of_pace",
            "shed_share",
            "source_dwell_seconds"
        ]
        .iter()
        .all(|c| !columns.contains(*c)),
        "and no old name is left: {columns:?}"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let row = |id: &str| {
        conn.query_row(
            "SELECT shed_cap, burn_average_seconds, shed_dwell_seconds, window_ceiling, load_shedding FROM pools \
             WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, f64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(
        row("low"),
        (0.1, 600, 900, 0.95, 0),
        "the cap and the average clamp to their floors, the ceiling takes its default and a null lead reads off"
    );
    assert_eq!(row("lead").4, 1, "a lead that was a number reads on");
    assert_eq!(
        row("week").1,
        7_200,
        "a week-long burn window clamps to the average's ceiling"
    );
    assert_eq!(
        logged(&conn),
        before + 2,
        "the clamp writes one change row per clamped pool and the switch's mapping none"
    );
    let mut statement = conn
        .prepare(
            "SELECT actor, kind, target, diff_json FROM registry_changes WHERE actor = 'migration' ORDER BY target",
        )
        .unwrap();
    let changes: Vec<(String, String, String, serde_json::Value)> = statement
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                serde_json::from_str(&r.get::<_, String>(3)?).unwrap(),
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        changes,
        [
            (
                "migration".to_string(),
                "pool_updated".to_string(),
                "low".to_string(),
                serde_json::json!({
                    "burn_average_seconds": 600,
                    "shed_cap": 0.1,
                    "shed_dwell_seconds": 900
                })
            ),
            (
                "migration".to_string(),
                "pool_updated".to_string(),
                "week".to_string(),
                serde_json::json!({
                    "burn_average_seconds": 7_200,
                    "shed_cap": 0.5,
                    "shed_dwell_seconds": 900
                })
            ),
        ],
        "each row is the partial image of the three bounded fields after the clamp"
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
        "marked_at",
        "marked_on",
        "marked_window",
        "burn_banded_cache_read",
        "burn_banded_input",
        "burn_banded_output",
        "burn_banded_write_1h",
        "burn_banded_write_5m",
        "burn_cache_read",
        "burn_fast_cache_read",
        "burn_fast_input",
        "burn_fast_output",
        "burn_fast_write_1h",
        "burn_fast_write_5m",
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} UPDATE meta SET value = '23' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(
        migrated.schema_version().unwrap(),
        54,
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} UPDATE meta SET value = '31' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    assert!(
        !has_windows_table(&path),
        "the roll-back left the store at 31"
    );
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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

/// A pin with both old burn figures, planted at version 32 after the roll-back.
const HELD_AT_32: &str = "INSERT INTO pins (key_id, session_id, agent_id, model, account_id, epoch, created_at, last_used_at, burn, burn_at, burn_usd) VALUES ('k', 's', 'main', 'claude-opus-5', 'a', 1, 1, 1, 120.0, 5, 0.75); -- rollback\n";

#[test]
fn a_store_left_at_32_is_carried_to_33_with_the_typed_burn_restarted() {
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} UPDATE meta SET value = '32' WHERE key = 'schema_version'; {HELD_AT_32}"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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
    drop(migrated);
    let columns = columns_of(&path, "pins");
    assert!(
        [
            "burn_input",
            "burn_cache_read",
            "burn_write_5m",
            "burn_write_1h",
            "burn_output",
            "burn_banded_input",
            "burn_banded_cache_read",
            "burn_banded_write_5m",
            "burn_banded_write_1h",
            "burn_banded_output"
        ]
        .iter()
        .all(|c| columns.contains(*c))
            && !columns.contains("burn")
            && !columns.contains("burn_usd"),
        "the pin's burn is typed, the old averages' columns gone: {columns:?}"
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} UPDATE meta SET value = '33' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    assert!(has_fit_table(&path), "the roll-back left the store at 33");
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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
    store
        .update_api_key(&read, read.updated_at, "test")
        .unwrap();
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
fn a_key_created_with_its_own_auto_mode_setting_reopens_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let key_store = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key_store).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    for (id, secret, setting) in [
        ("desktop", "phoebis-desktop-secret", true),
        ("laptop", "phoebis-laptop-secret", false),
    ] {
        let mut row = key(&store, id, "main", secret);
        row.auto_mode_server = Some(setting);
        store.insert_api_key(&row, "test").unwrap();
    }
    drop(store);
    let store = Store::open(&path, &key_store).unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().auto_mode_server,
        Some(true)
    );
    assert_eq!(
        store.api_key("laptop").unwrap().unwrap().auto_mode_server,
        Some(false),
        "an override set at the create is stored as set, on or off"
    );
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} UPDATE meta SET value = '34' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    let migrated = Store::open(&path, &key_store).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
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

fn keepalive_tables(path: &std::path::Path) -> std::collections::BTreeSet<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut statement = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'keepalive_%'")
        .unwrap();
    statement
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The one upgrade-path check of `V49`'s tables: `V38`'s own change leaves no end state once `V50` drops its table.
#[test]
fn a_store_left_at_37_is_carried_to_the_engines_three_keepalive_tables_alone() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} UPDATE meta SET value = '37' WHERE key = 'schema_version';"
    ))
    .unwrap();
    drop(conn);
    assert_eq!(
        keepalive_tables(&path).len(),
        1,
        "the roll-back left the store at 37, with the day budgets alone: {:?}",
        keepalive_tables(&path)
    );
    let migrated = Store::open(&path, &key).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), 54);
    drop(migrated);
    assert_eq!(
        keepalive_tables(&path),
        [
            "keepalive_agents",
            "keepalive_idle_returns",
            "keepalive_sessions"
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        "the chain from 37 ends on the engine's own tables, the day's probe spend dropped"
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
        "{UNDO_V54} {UNDO_V53} {UNDO_V52} {UNDO_V51} {UNDO_V50} {UNDO_V49} {UNDO_V48} {UNDO_V47} {UNDO_V46} {UNDO_V45} {UNDO_V44} {UNDO_V43} {UNDO_V42} {UNDO_V41} {UNDO_V40} {UNDO_V39} {UNDO_V38} {UNDO_V37} {UNDO_V36} {UNDO_V35} {UNDO_V34} {UNDO_V33} {UNDO_V32} {UNDO_V31} {UNDO_V30} {UNDO_V29} {UNDO_V28} {UNDO_V27} {UNDO_V26} {UNDO_V25} {UNDO_V24} \
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
    assert_eq!(migrated.schema_version().unwrap(), 54);
    migrated
        .insert_api_key(
            &key(&migrated, "old", "main", "phoebis-old-reissued"),
            "test",
        )
        .unwrap();
    // The name holds the incarnations a deleted id leaves behind, which never retire an id.
    assert_eq!(
        columns_of(&path, "api_key_tombstones"),
        ["id", "incarnation"].map(String::from).into(),
        "the upgrade dropped the retirement table"
    );
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
                window_ceiling: 0.8,
                model_rules: pool_rules.clone(),
                ..pool("main")
            },
            store.pool("main").unwrap().unwrap().updated_at,
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
            store.api_key("desktop").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    let updated = store.pool("main").unwrap().unwrap();
    let updated_key = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(
        (
            updated.body_capture_hours,
            updated.strategy,
            updated.window_ceiling,
            updated.model_rules.clone(),
            updated_key.body_capture_hours,
            updated_key.model_rules.clone(),
            updated_key.model_rules_mode,
            updated_key.long_context_twins,
        ),
        (
            24,
            Strategy::Pace,
            0.8,
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
            .map(|p| (p.strategy, p.window_ceiling)),
        Some((Strategy::Pace, 0.8)),
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
fn a_new_pool_waits_out_a_throttle_of_up_to_a_minute_by_default() {
    assert_eq!(Failover::default().retry_in_place_seconds, 60);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("router.sqlite"), &StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let pools = store.pools().unwrap();
    let main = pools.iter().find(|p| p.id == "main").unwrap();
    assert_eq!(main.failover.retry_in_place_seconds, 60);
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
    let before = store.account("a").unwrap().unwrap().revision;
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                1
            )
            .unwrap(),
        None,
        "a verdict about the replaced key changes nothing"
    );
    let row = store.account("a").unwrap().unwrap();
    assert_eq!((row.state, row.revision), (AccountState::Ready, before));
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                2
            )
            .unwrap(),
        Some((before + 1, AccountState::Ready)),
        "the moved row's revision after the write and the state it held before"
    );
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                2
            )
            .unwrap(),
        Some((before + 2, AccountState::ReauthRequired)),
        "a repeated verdict lands under its fences and names the state it found unchanged"
    );
}

#[test]
fn each_state_writer_stores_its_word_beside_the_state() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let read = |id: &str| {
        let row = store.account(id).unwrap().unwrap();
        (row.state, row.state_cause)
    };
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    assert_eq!(read("a"), (AccountState::Ready, StateCause::Operator));
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    assert_eq!(read("a"), (AccountState::Ready, StateCause::Paired));
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                2
            )
            .unwrap(),
        None
    );
    assert_eq!(
        read("a"),
        (AccountState::Ready, StateCause::Paired),
        "a write its fences refuse leaves the state and its word"
    );
    assert!(store
        .set_state_if_generations(
            "a",
            AccountState::ReauthRequired,
            StateCause::AuthFailed,
            1,
            1,
            1
        )
        .unwrap()
        .is_some());
    assert_eq!(
        read("a"),
        (AccountState::ReauthRequired, StateCause::AuthFailed)
    );
    let row = store.account("a").unwrap().unwrap();
    store
        .commit_account_update(&AccountUpdate {
            id: "a",
            expected_revision: row.revision,
            incarnation: row.incarnation,
            lifecycle_generation: row.lifecycle_generation,
            label: "a",
            secret: None,
            state: Some((AccountState::Ready, StateCause::Operator)),
            at: 1,
            actor: "admin",
            enabled: None,
            cancellation: None,
        })
        .unwrap();
    assert_eq!(read("a"), (AccountState::Ready, StateCause::Operator));
    store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    let revision = store.account("a").unwrap().unwrap().revision;
    rename(&store, "a", revision, "renamed", "admin").unwrap();
    assert_eq!(
        read("a"),
        (AccountState::Ready, StateCause::Paired),
        "an edit that stages no state leaves the word a pairing stored"
    );
    store.bump_lifecycle("a", StateCause::Operator).unwrap();
    assert_eq!(
        read("a"),
        (AccountState::Disconnected, StateCause::Operator)
    );
    store
        .create_account(
            &AccountRow {
                state: AccountState::Unpaired,
                state_cause: StateCause::Paired,
                ..account("b", AccountKind::Oauth)
            },
            None,
            1,
            "admin",
        )
        .unwrap();
    assert_eq!(
        read("b"),
        (AccountState::Unpaired, StateCause::Paired),
        "the creation writes the row's own word"
    );
}

#[test]
fn set_enabled_leaves_the_state_cause() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    for enabled in [false, true] {
        store.set_enabled("a", enabled).unwrap();
        let row = store.account("a").unwrap().unwrap();
        assert_eq!(
            (row.enabled, row.state, row.state_cause),
            (enabled, AccountState::Ready, StateCause::Paired)
        );
    }
}

#[test]
fn a_row_written_without_the_column_s_word_reads_unrecorded() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO accounts (id, kind, label, enabled, state, created_at, updated_at) \
         VALUES ('a', 'oauth', 'a', 1, 'ready', 1, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().state_cause,
        StateCause::Unrecorded,
        "the migration's default is the word a row from before the column reads"
    );
}

#[test]
fn a_stored_word_outside_the_list_fails_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE accounts SET state_cause = 'nonsense' WHERE id = 'a'",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert!(store.account("a").is_err());
}

#[test]
fn two_pool_edits_from_one_read_answer_stale_on_the_second() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let read = store.pool("main").unwrap().unwrap();
    store
        .update_pool(
            &PoolRow {
                label: "First".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert!(matches!(
        store.update_pool(
            &PoolRow {
                label: "Second".into(),
                ..read.clone()
            },
            read.updated_at,
            "test"
        ),
        Err(StoreError::Stale)
    ));
    let first = store.pool("main").unwrap().unwrap();
    assert_eq!(first.label, "First", "the stale edit wrote nothing");
    assert!(
        first.updated_at > read.updated_at,
        "a write inside the stamp's own millisecond still moves it"
    );
    store
        .set_pool_members("main", &[member("a", 0)], "test")
        .unwrap();
    assert!(
        matches!(
            store.update_pool_with_members(
                &PoolRow {
                    label: "Third".into(),
                    ..first.clone()
                },
                first.updated_at,
                Some(&[]),
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "a member write moves the stamp, so an edit read before it cannot land over it"
    );
    assert_eq!(
        (
            store.pool("main").unwrap().unwrap().label,
            store.pool_members("main").unwrap().len()
        ),
        ("First".to_string(), 1),
        "neither half of the stale edit landed"
    );
    assert!(matches!(
        store.update_pool(&pool("ghost"), 1, "test"),
        Err(StoreError::PoolNotFound(_))
    ));
}

/// Accounts `a` and `b`, pools `p1` holding `a` and `p2` holding no one.
fn a_in_p1(store: &Store) {
    for id in ["a", "b"] {
        store
            .insert_account(&account(id, AccountKind::ApiKey))
            .unwrap();
    }
    store.insert_pool(&pool("p1"), "test").unwrap();
    store.insert_pool(&pool("p2"), "test").unwrap();
    store
        .set_pool_members("p1", &[member("a", 0)], "test")
        .unwrap();
}

#[test]
fn a_member_write_naming_an_account_in_another_pool_is_refused_and_writes_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    a_in_p1(&store);
    let before = store.pool("p2").unwrap().unwrap();
    let changes = store.registry_changes_after(0, 100).unwrap();
    let in_p1 = |result: Result<i64, StoreError>| match result {
        Err(StoreError::AccountInPool {
            account_id,
            pool_id,
        }) => (account_id, pool_id) == ("a".to_string(), "p1".to_string()),
        _ => false,
    };
    assert!(
        in_p1(store.set_pool_members("p2", &[member("a", 0)], "test")),
        "the member list's own write"
    );
    assert!(
        in_p1(
            store
                .insert_pool_with_members(&pool("p3"), Some(&[member("a", 0)]), "test")
                .map(|row| row.updated_at)
        ),
        "a create carrying members"
    );
    assert!(
        in_p1(store.update_pool_with_members(
            &PoolRow {
                label: "Renamed".into(),
                ..before.clone()
            },
            before.updated_at,
            Some(&[member("a", 0), member("b", 1)]),
            "test"
        )),
        "an edit carrying members"
    );
    let after = store.pool("p2").unwrap().unwrap();
    assert_eq!(
        (after.label, after.updated_at),
        (before.label, before.updated_at),
        "the refused edit moved neither the settings nor the stamp"
    );
    assert!(store.pool_members("p2").unwrap().is_empty());
    assert!(
        store.pool("p3").unwrap().is_none(),
        "the refused create left no row"
    );
    assert_eq!(
        store
            .pool_members("p1")
            .unwrap()
            .iter()
            .map(|m| m.account_id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(
        store.registry_changes_after(0, 100).unwrap().len(),
        changes.len(),
        "no write recorded a change, a pool_members_set row included"
    );
}

#[test]
fn a_write_that_keeps_an_account_in_its_own_pool_lands() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    a_in_p1(&store);
    store
        .set_pool_members("p1", &[member("a", 2)], "test")
        .unwrap();
    assert_eq!(store.pool_members("p1").unwrap()[0].priority, 2);
    let read = store.pool("p1").unwrap().unwrap();
    store
        .update_pool_with_members(&read, read.updated_at, Some(&[member("a", 5)]), "test")
        .unwrap();
    assert_eq!(store.pool_members("p1").unwrap()[0].priority, 5);
}

#[test]
fn the_index_refuses_a_direct_insert_the_check_would_have_caught() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    a_in_p1(&store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let refused = conn
        .execute(
            "INSERT INTO pool_members (pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd) \
             VALUES ('p2', 'a', 0, NULL, NULL)",
            [],
        )
        .unwrap_err();
    assert!(
        matches!(
            &refused,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
        ),
        "{refused:?}"
    );
}

#[test]
fn two_key_edits_from_one_read_answer_stale_on_the_second() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-0123456789abcdef"),
            "test",
        )
        .unwrap();
    let read = store.api_key("desktop").unwrap().unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                label: "First".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert!(matches!(
        store.update_api_key(
            &ApiKeyRow {
                label: "Second".into(),
                ..read.clone()
            },
            read.updated_at,
            "test"
        ),
        Err(StoreError::Stale)
    ));
    let first = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(first.label, "First", "the stale edit wrote nothing");
    assert!(
        first.updated_at > read.updated_at,
        "a write inside the stamp's own millisecond still moves it"
    );
    store.revoke_api_key("desktop", 5, "test").unwrap();
    assert!(
        matches!(
            store.update_api_key(&first, first.updated_at, "test"),
            Err(StoreError::Stale)
        ),
        "an edit read before the revocation cannot switch the revoked key back on"
    );
    assert!(!store.api_key("desktop").unwrap().unwrap().enabled);
    assert!(matches!(
        store.update_api_key(&key(&store, "ghost", "main", "phoebis-ghost"), 0, "test"),
        Err(StoreError::KeyNotFound(_))
    ));
}

/// The replacement's stamp is pinned to its predecessor's rather than raced for, so the two rows differ in their
/// incarnation alone and the token is what tells them apart.
#[test]
fn a_pool_deleted_and_created_again_inside_one_millisecond_refuses_the_predecessors_token() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    let first = store.insert_pool(&pool("main"), "test").unwrap();
    assert_eq!(
        first.incarnation, 1,
        "a new id starts at the first incarnation"
    );
    let token = stamp_token(first.incarnation, first.updated_at);
    store.delete_pool("main", "test").unwrap();
    let second = store.insert_pool(&pool("main"), "test").unwrap();
    assert_eq!(
        second.incarnation, 2,
        "the insert answers the incarnation it wrote"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE pools SET updated_at = ?1 WHERE id = 'main'",
        [first.updated_at],
    )
    .unwrap();
    drop(conn);
    let read = store.pool("main").unwrap().unwrap();
    assert_eq!((read.incarnation, read.updated_at), (2, first.updated_at));
    assert_ne!(stamp_token(read.incarnation, read.updated_at), token);
    assert!(
        !answers_stamp_token(read.incarnation, read.updated_at, token),
        "the replacement never answers its predecessor's token"
    );
    assert!(
        matches!(
            store.update_pool(
                &PoolRow {
                    label: "Stale".into(),
                    ..first.clone()
                },
                first.updated_at,
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "an edit read before the deletion carries the predecessor's incarnation"
    );
    assert_eq!(store.pool("main").unwrap().unwrap().label, "main");
    store
        .update_pool(
            &PoolRow {
                label: "Fresh".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(store.pool("main").unwrap().unwrap().label, "Fresh");
}

#[test]
fn a_key_deleted_and_created_again_inside_one_millisecond_refuses_the_predecessors_token() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let first = store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-desktop-one"),
            "test",
        )
        .unwrap();
    assert_eq!(
        first.incarnation, 1,
        "a new id starts at the first incarnation"
    );
    let token = stamp_token(first.incarnation, first.updated_at);
    store.delete_api_key("desktop", "test").unwrap();
    let second = store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-desktop-two"),
            "test",
        )
        .unwrap();
    assert_eq!(
        second.incarnation, 2,
        "the insert answers the incarnation it wrote"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE api_keys SET updated_at = ?1 WHERE id = 'desktop'",
        [first.updated_at],
    )
    .unwrap();
    drop(conn);
    let read = store.api_key("desktop").unwrap().unwrap();
    assert_eq!((read.incarnation, read.updated_at), (2, first.updated_at));
    assert_ne!(stamp_token(read.incarnation, read.updated_at), token);
    assert!(
        !answers_stamp_token(read.incarnation, read.updated_at, token),
        "the replacement never answers its predecessor's token"
    );
    assert!(
        matches!(
            store.update_api_key(
                &ApiKeyRow {
                    label: "Stale".into(),
                    ..first.clone()
                },
                first.updated_at,
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "an edit read before the deletion carries the predecessor's incarnation"
    );
    assert_eq!(store.api_key("desktop").unwrap().unwrap().label, "desktop");
    store
        .update_api_key(
            &ApiKeyRow {
                label: "Fresh".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(store.api_key("desktop").unwrap().unwrap().label, "Fresh");
}

#[test]
fn a_stale_account_update_carrying_enabled_and_a_cancellation_writes_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let read = store.account("a").unwrap().unwrap();
    rename(&store, "a", read.revision, "Moved", "admin").unwrap();
    let stale = AccountUpdate {
        id: "a",
        expected_revision: read.revision,
        incarnation: read.incarnation,
        lifecycle_generation: read.lifecycle_generation,
        label: "Stale",
        secret: None,
        state: None,
        at: 5,
        actor: "admin",
        enabled: Some(false),
        cancellation: Some((true, Some(9_000))),
    };
    assert!(matches!(
        store.commit_account_update(&stale),
        Err(StoreError::Stale)
    ));
    let row = store.account("a").unwrap().unwrap();
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (
            row.label.as_str(),
            row.enabled,
            term.cancelled,
            term.ends_at
        ),
        ("Moved", true, false, None),
        "no staged field of the stale edit landed"
    );
    store
        .commit_account_update(&AccountUpdate {
            expected_revision: row.revision,
            ..stale
        })
        .unwrap();
    let landed = store.account("a").unwrap().unwrap();
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (
            landed.label.as_str(),
            landed.enabled,
            landed.state,
            term.cancelled,
            term.ends_at,
            landed.revision
        ),
        (
            "Stale",
            false,
            AccountState::Ready,
            true,
            Some(9_000),
            row.revision + 1
        ),
        "the same edit built from the moved row lands every staged field in one revision"
    );
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
            StateCause::FundingRefused,
            incarnation,
            lifecycle,
            credential,
        )
        .unwrap();
    assert_eq!(moved, None, "the verdict named a holder that is gone");
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(row.state, AccountState::Ready);
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::FundingRequired,
                StateCause::FundingRefused,
                2,
                1,
                1
            )
            .unwrap(),
        Some((row.revision + 1, AccountState::Ready)),
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
        incarnation: 1,
        lifecycle_generation: 1,
        label: "Renamed",
        secret: Some(&Secret::new("k2")),
        state: Some((AccountState::Ready, StateCause::Operator)),
        at: 5,
        actor: "admin",
        enabled: None,
        cancellation: None,
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
fn a_recreated_account_never_answers_an_edit_token_its_predecessor_answered() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(&account("a", AccountKind::Oauth), None, 1, "admin")
        .unwrap();
    let mut answered = Vec::new();
    for label in ["One", "Two"] {
        let row = store.account("a").unwrap().unwrap();
        answered.push(row.edit_token());
        rename(&store, "a", row.revision, label, "admin").unwrap();
    }
    answered.push(store.account("a").unwrap().unwrap().edit_token());
    store.delete_account("a", "admin").unwrap();
    store
        .create_account(&account("a", AccountKind::Oauth), None, 2, "admin")
        .unwrap();
    for label in ["One", "Two", "Three"] {
        let row = store.account("a").unwrap().unwrap();
        assert!(
            !answered.contains(&row.edit_token()),
            "revision {} of the replacement answers a token its predecessor answered",
            row.revision
        );
        rename(&store, "a", row.revision, label, "admin").unwrap();
    }
}

/// The bundle a pairing's confirm hands the store, with tokens that are obviously not real.
fn paired_bundle() -> NewBundle {
    NewBundle {
        access_token: Secret::new("at-test-not-a-token"),
        refresh_token: Some(Secret::new("rt-test-not-a-token")),
        issued_at: 1_799_996_400_000,
        expires_at: 1_800_000_000_000,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    }
}

/// The row a confirm builds for a pairing started before its row existed: bound to `account_uuid` and born paired.
fn paired_row(id: &str, account_uuid: &str) -> AccountRow {
    AccountRow {
        identity: Some(Identity {
            account_uuid: account_uuid.into(),
            email: format!("{id}@example.invalid"),
            organization_uuid: None,
        }),
        state_cause: StateCause::Paired,
        ..account(id, AccountKind::Oauth)
    }
}

#[test]
fn a_paired_account_is_born_ready_with_its_credential_attempt_and_change_in_one_write() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let subscription = SubscriptionRow {
        plan: Some("max".into()),
        tier: Some("default_claude_max_5x".into()),
        since: Some(1_773_482_400_000),
        trial_ends_at: None,
    };
    let profile = r#"{"account":{"uuid":"00000000-0000-4000-8000-000000000001"}}"#;
    let generation = store
        .create_paired_account(
            &paired_row("fresh", "00000000-0000-4000-8000-000000000001"),
            &paired_bundle(),
            profile,
            &subscription,
            5,
            "admin",
        )
        .unwrap();
    assert_eq!(generation, 1);
    let row = store.account("fresh").unwrap().unwrap();
    assert_eq!(
        (
            row.state,
            row.state_cause,
            row.incarnation,
            row.lifecycle_generation,
            row.revision
        ),
        (AccountState::Ready, StateCause::Paired, 1, 1, 1)
    );
    assert_eq!(
        row.identity.as_ref().map(|i| i.account_uuid.as_str()),
        Some("00000000-0000-4000-8000-000000000001")
    );
    let credential = store.credentials("fresh").unwrap().unwrap();
    assert_eq!(
        (credential.generation, credential.lifecycle_generation),
        (1, 1)
    );
    assert_eq!(credential.access_token.expose(), "at-test-not-a-token");
    let attempts = store
        .newest_resolved_refresh_attempts("fresh", 1, 10)
        .unwrap();
    assert_eq!(attempts.len(), 1, "the pairing's own resolved attempt");
    assert_eq!(
        (
            attempts[0].outcome.as_str(),
            attempts[0].detail.as_deref(),
            attempts[0].credential_generation,
            attempts[0].resolved_at
        ),
        ("committed", Some("pairing"), 1, 5)
    );
    let changes = store.registry_changes_after(0, 10).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        (
            changes[0].kind.as_str(),
            changes[0].target.as_str(),
            changes[0].actor.as_str(),
            changes[0].at
        ),
        ("account_created", "fresh", "admin", 5)
    );
    assert_eq!(
        store.profile_json("fresh").unwrap().as_deref(),
        Some(profile),
        "the profile lands in the same write"
    );
    let term = store.subscription_term("fresh").unwrap().unwrap();
    assert_eq!(
        (term.since, term.trial_ends_at),
        (Some(1_773_482_400_000), None)
    );

    let unchanged = |store: &Store| {
        (
            store.accounts().unwrap(),
            store.credentials("fresh").unwrap().map(|c| c.generation),
            store
                .newest_resolved_refresh_attempts("fresh", 1, 10)
                .unwrap()
                .len(),
            store.registry_changes_after(0, 10).unwrap().len(),
        )
    };
    let before = unchanged(&store);
    let same_id = store.create_paired_account(
        &paired_row("fresh", "00000000-0000-4000-8000-000000000009"),
        &paired_bundle(),
        "{}",
        &subscription,
        6,
        "admin",
    );
    assert!(
        matches!(same_id, Err(StoreError::Sqlite(_))),
        "a taken id answers the constraint itself: {same_id:?}"
    );
    let same_identity = store.create_paired_account(
        &paired_row("second", "00000000-0000-4000-8000-000000000001"),
        &paired_bundle(),
        "{}",
        &subscription,
        7,
        "admin",
    );
    assert!(
        matches!(same_identity, Err(StoreError::IdentityTaken)),
        "an identity another row holds is named: {same_identity:?}"
    );
    assert!(store.account("second").unwrap().is_none());
    assert!(store.credentials("second").unwrap().is_none());
    assert_eq!(
        unchanged(&store),
        before,
        "neither refused write left anything behind"
    );
}

#[test]
fn an_edit_token_too_wide_for_a_console_number_is_refused_by_its_own_row() {
    let row = AccountRow {
        incarnation: 1 << 21,
        ..account("a", AccountKind::Oauth)
    };
    assert!(
        row.edit_token() < 1 << 53,
        "the token stays an integer a console number holds exactly"
    );
    assert!(
        !row.answers_edit_token(row.edit_token()),
        "a token that cannot name the row's incarnation fences nothing onto it"
    );
    // A pool's or a key's token packs the incarnation above the stamp's 42 bits, which hold every stamp to 2109.
    let widest = (1 << 42) - 1;
    assert!(
        stamp_token((1 << 11) - 1, widest) < 1 << 53 && stamp_token(1 << 11, widest) < 1 << 53,
        "the stamp token stays an integer a console number holds exactly"
    );
    assert!(answers_stamp_token(
        (1 << 11) - 1,
        widest,
        stamp_token((1 << 11) - 1, widest)
    ));
    assert!(
        !answers_stamp_token(1 << 11, widest, stamp_token(1 << 11, widest)),
        "a stamp token that cannot name the row's incarnation fences nothing onto it"
    );
}

#[test]
fn an_account_update_read_from_a_deleted_account_is_stale_on_the_one_that_took_its_id() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k1")),
            1,
            "admin",
        )
        .unwrap();
    let read = store.account("a").unwrap().unwrap();
    store.delete_account("a", "admin").unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k2")),
            2,
            "admin",
        )
        .unwrap();
    let replacement = store.account("a").unwrap().unwrap();
    assert_eq!(
        (replacement.revision, replacement.lifecycle_generation),
        (read.revision, read.lifecycle_generation),
        "the replacement restarts both counters the read carried"
    );
    let stale = AccountUpdate {
        id: "a",
        expected_revision: read.revision,
        incarnation: read.incarnation,
        lifecycle_generation: read.lifecycle_generation,
        label: "Stale",
        secret: Some(&Secret::new("k3")),
        state: Some((AccountState::Ready, StateCause::Operator)),
        at: 5,
        actor: "admin",
        enabled: Some(false),
        cancellation: None,
    };
    assert!(matches!(
        store.commit_account_update(&stale),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        (
            store.account("a").unwrap().unwrap(),
            store
                .api_key_credential("a")
                .unwrap()
                .unwrap()
                .0
                .expose()
                .to_string()
        ),
        (replacement.clone(), "k2".to_string()),
        "no staged field of the edit read from the deleted account landed"
    );
    store
        .commit_account_update(&AccountUpdate {
            incarnation: replacement.incarnation,
            ..stale
        })
        .unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().label,
        "Stale",
        "the same edit built from the replacement lands"
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
    rename(&store, "a", 1, "Renamed", "admin").unwrap();
    assert_eq!(revision(), 2, "a label update");
    store.set_enabled("a", false).unwrap();
    assert_eq!(revision(), 3, "an enable flip");
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    assert_eq!(revision(), 4, "a secret rotation");
    store
        .set_state("a", AccountState::FundingRequired, StateCause::Operator)
        .unwrap();
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
    rename(&store, "a", 1, "first", "admin").unwrap();
    rename(&store, "a", 2, "second", "admin").unwrap();
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
fn every_key_write_stamps_updated_at_and_a_use_leaves_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store.insert_pool(&pool("p"), "test").unwrap();
    let created_at = store
        .insert_api_key(&key(&store, "k", "p", "secret-one"), "test")
        .unwrap()
        .created_at;
    let stamped = || store.api_key("k").unwrap().unwrap().updated_at;
    assert_eq!(stamped(), created_at, "the create dates the row");
    // Cleared beside the store between the writes, so each write has to stamp the row itself.
    let raw = rusqlite::Connection::open(&path).unwrap();
    let clear = || {
        raw.execute("UPDATE api_keys SET updated_at = 0", [])
            .unwrap();
    };
    clear();
    store.touch_api_key("k", 5, false, None).unwrap();
    assert_eq!(stamped(), 0, "a use is no change to the row");
    let row = store.api_key("k").unwrap().unwrap();
    let read_at = row.updated_at;
    let answered = store
        .update_api_key(
            &ApiKeyRow {
                label: "renamed".into(),
                ..row
            },
            read_at,
            "test",
        )
        .unwrap();
    assert_ne!(answered, 0);
    assert_eq!(stamped(), answered, "an edit answers the stamp it wrote");
    clear();
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("secret-two"),
            &phoebis_store::key_prefix("secret-two"),
            7,
            "test",
        )
        .unwrap();
    assert_ne!(stamped(), 0, "a rotation stamps the row");
    clear();
    store.revoke_api_key("k", 9, "test").unwrap();
    assert_ne!(stamped(), 0, "a revocation stamps the row");
}

#[test]
fn two_key_updates_inside_one_millisecond_read_two_different_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store.insert_pool(&pool("p"), "test").unwrap();
    let created_at = store
        .insert_api_key(&key(&store, "k", "p", "secret-one"), "test")
        .unwrap()
        .created_at;
    // A stored stamp an hour past the clock is the state several writes inside one millisecond leave: the clock
    // has not moved past the newest stamp, so each write has to step past it to read as a change.
    let ahead = created_at + 3_600_000;
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE api_keys SET updated_at = ?1", [ahead])
        .unwrap();
    let read = store.api_key("k").unwrap().unwrap();
    let first = store
        .update_api_key(
            &ApiKeyRow {
                label: "first".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    let moved = store.api_key("k").unwrap().unwrap();
    let second = store
        .update_api_key(
            &ApiKeyRow {
                label: "second".into(),
                ..moved.clone()
            },
            moved.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(
        (first, second),
        (ahead + 1, ahead + 2),
        "each update steps one past the stamp it fenced on"
    );
    assert_eq!(store.api_key("k").unwrap().unwrap().updated_at, second);
    assert!(
        matches!(
            store.update_api_key(&moved, moved.updated_at, "test"),
            Err(StoreError::Stale)
        ),
        "an edit read between the two cannot pass the fence"
    );
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("secret-two"),
            &phoebis_store::key_prefix("secret-two"),
            7,
            "test",
        )
        .unwrap();
    assert_eq!(
        store.api_key("k").unwrap().unwrap().updated_at,
        ahead + 3,
        "a rotation steps the same way"
    );
    store.revoke_api_key("k", 9, "test").unwrap();
    assert_eq!(
        store.api_key("k").unwrap().unwrap().updated_at,
        ahead + 4,
        "and so does a revocation"
    );
}

#[test]
fn the_history_purge_cuts_old_resolved_attempts_and_changes_and_keeps_what_their_readers_need() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let cutoff = 1_000_000;
    {
        let mut raw = rusqlite::Connection::open(&path).unwrap();
        let tx = raw.transaction().unwrap();
        let attempt = |account_id: &str, resolved_at: Option<i64>, outcome: Option<&str>| {
            tx.execute(
                "INSERT INTO refresh_attempts (account_id, incarnation, lifecycle_generation, \
                 credential_generation, started_at, resolved_at, outcome) VALUES (?1, 1, 1, 1, ?2, ?3, ?4)",
                rusqlite::params![account_id, resolved_at.unwrap_or(10), resolved_at, outcome],
            )
            .unwrap();
        };
        for i in 0..2_500 {
            let outcome = if i % 2 == 0 { "failed" } else { "committed" };
            attempt("a", Some(1_000 + i), Some(outcome));
        }
        // Still open: the startup quarantine reads it, whatever its age.
        attempt("a", Some(500), Some("uncertain"));
        attempt("c", None, None);
        // A holder no refresh touched since: its newest two are what the attention reader and the account
        // view read.
        for resolved_at in [100, 200, 300] {
            attempt("b", Some(resolved_at), Some("failed"));
        }
        // Two uncertain attempts a later sweep settled after the holder's newest one: the newest two by settling
        // are the older pair, and the account view's last attempt reads the newest by id.
        for resolved_at in [400, 500, 150] {
            attempt("d", Some(resolved_at), Some("failed"));
        }
        let change = |at: i64, kind: &str, target: &str| {
            tx.execute(
                "INSERT INTO registry_changes (at, actor, kind, target, diff_json) VALUES (?1, 'test', ?2, ?3, '{}')",
                rusqlite::params![at, kind, target],
            )
            .unwrap();
        };
        for i in 0..1_500 {
            change(1_000 + i, "pool_updated", "p");
        }
        // A reused id: the newest create and delete tell the ledger's stamp and the next create's generation
        // that the id had an earlier holder.
        change(10, "key_created", "gone");
        change(11, "key_deleted", "gone");
        change(12, "key_created", "gone");
        change(13, "key_deleted", "gone");
        tx.commit().unwrap();
    }
    for _ in 0..2 {
        let id = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
        store
            .resolve_refresh_attempt(id, 1, phoebis_store::AttemptOutcome::Failed("x".into()))
            .unwrap();
    }
    let watermark = store.newest_registry_change_id().unwrap().unwrap();
    store.insert_pool(&pool("p"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "k", "p", "secret"), "test")
        .unwrap();
    let fresh: Vec<u64> = store
        .registry_changes_after(watermark, 100)
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(fresh.len(), 2);

    let purged = store.purge_history(cutoff).unwrap();
    assert_eq!(
        purged,
        phoebis_store::PurgedHistory {
            attempts: 2_500 + 1,
            changes: 1_500 + 2,
        }
    );
    let raw = rusqlite::Connection::open(&path).unwrap();
    let attempts: Vec<(String, Option<i64>, Option<String>)> = raw
        .prepare("SELECT account_id, resolved_at, outcome FROM refresh_attempts WHERE resolved_at IS NULL OR resolved_at < ?1 ORDER BY id")
        .unwrap()
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        attempts,
        [
            ("a".into(), Some(500), Some("uncertain".into())),
            ("c".into(), None, None),
            ("b".into(), Some(200), Some("failed".into())),
            ("b".into(), Some(300), Some("failed".into())),
            ("d".into(), Some(400), Some("failed".into())),
            ("d".into(), Some(500), Some("failed".into())),
            ("d".into(), Some(150), Some("failed".into())),
        ],
        "the open rows, each untouched holder's newest two and its newest by id stay"
    );
    assert_eq!(
        store
            .last_refresh_attempt("d", 1)
            .unwrap()
            .and_then(|a| a.resolved_at),
        Some(150)
    );
    assert_eq!(
        store
            .newest_resolved_refresh_attempts("a", 1, 10)
            .unwrap()
            .len(),
        3,
        "the fresh attempts stay beside the open one"
    );
    let kept: Vec<(i64, String)> = raw
        .prepare("SELECT at, kind FROM registry_changes WHERE at < ?1 ORDER BY id")
        .unwrap()
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        kept,
        [(12, "key_created".into()), (13, "key_deleted".into())],
        "the reused id keeps its newest create and delete"
    );
    assert_eq!(
        store
            .registry_changes_after(watermark, 100)
            .unwrap()
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        fresh,
        "a mirror's watermark still reads the fresh changes"
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
    store.update_pool(&row, read.updated_at, "test").unwrap();
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
    rename(&store, "vendor", built_from, "Renamed", "admin").unwrap();
    assert_eq!(
        store.account("vendor").unwrap().unwrap().label.as_str(),
        "Renamed"
    );
    assert!(
        matches!(
            rename(&store, "vendor", built_from, "Renamed Again", "admin"),
            Err(StoreError::Stale)
        ),
        "the revision the caller read is gone, so the edit it assembled is about a row that moved"
    );
    assert_eq!(store.account("vendor").unwrap().unwrap().label, "Renamed");
    assert!(matches!(
        rename(&store, "nobody", 1, "x", "admin"),
        Err(StoreError::AccountNotFound(_))
    ));
}

/// Runs the key-loss recovery as `docs/site/operations.md` prints it, read from the page so the two cannot drift.
#[test]
fn the_key_loss_recovery_opens_the_registry_under_a_new_key_with_its_accounts_and_pools_intact() {
    let page = include_str!("../../../docs/site/operations.md");
    let mut sequences = page.split("<<'SQL'\n").skip(1);
    let sequence = sequences
        .next()
        .and_then(|rest| rest.split("\nSQL\n").next())
        .expect("the page prints the recovery sequence as a heredoc");
    assert!(
        sequences.next().is_none(),
        "the page prints one recovery sequence"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let lost = StorageKey::generate();
    let store = Store::open(&path, &lost).unwrap();
    store
        .insert_account(&account("claude", AccountKind::Oauth))
        .unwrap();
    store
        .commit_credentials(
            "claude",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("at-test-not-a-token"),
                refresh_token: Some(Secret::new("rt-test-not-a-token")),
                issued_at: 1_799_996_400_000,
                expires_at: 1_800_000_000_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    store.open_refresh_attempt("claude", 1, 1, 1).unwrap();
    store
        .create_account(
            &account("vendor", AccountKind::ApiKey),
            Some(&Secret::new("sk-ant-test-not-a-key")),
            1,
            "admin",
        )
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[member("claude", 1), member("vendor", 2)], "test")
        .unwrap();
    let desktop = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&desktop, "test").unwrap();
    drop(store);

    let new = StorageKey::generate();
    assert!(matches!(
        Store::open(&path, &new),
        Err(StoreError::KeyMismatch { .. })
    ));
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(sequence)
        .unwrap();
    assert_eq!(
        Store::read_canary(&path, &new).unwrap(),
        CanaryRead::Absent,
        "nothing sealed is left to prove a key against"
    );

    let store = Store::open(&path, &new).unwrap();
    assert_eq!(
        Store::read_canary(&path, &new).unwrap(),
        CanaryRead::Matches,
        "the start under the new key sealed a new canary"
    );
    assert!(matches!(
        Store::read_canary(&path, &lost),
        Err(StoreError::KeyMismatch { .. })
    ));
    let states: Vec<(String, AccountState)> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|row| (row.id, row.state))
        .collect();
    assert_eq!(
        states,
        vec![
            ("claude".to_string(), AccountState::Disconnected),
            ("vendor".to_string(), AccountState::Unpaired),
        ],
        "each account rests where the Disconnect action leaves it"
    );
    assert!(store.credentials("claude").unwrap().is_none());
    assert!(store.api_key_secret("vendor").unwrap().is_none());
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    assert!(store.pool("main").unwrap().is_some());
    let members: Vec<String> = store
        .pool_members("main")
        .unwrap()
        .into_iter()
        .map(|member| member.account_id)
        .collect();
    assert_eq!(members, vec!["claude".to_string(), "vendor".to_string()]);
    assert_eq!(store.api_key("desktop").unwrap().unwrap().pool_id, "main");
}
