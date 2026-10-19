//! The registry schema: `accounts`, `api_key_secrets`, `credentials`, and `refresh_attempts`, plus `meta`
//! for the schema marker.

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, label TEXT NOT NULL, enabled INTEGER NOT NULL,
  identity_json TEXT, profile_json TEXT, state TEXT NOT NULL,
  lifecycle_generation INTEGER NOT NULL DEFAULT 1,
  catalog_synced_at INTEGER, catalog_error TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS credentials (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  generation INTEGER NOT NULL, lifecycle_generation INTEGER NOT NULL,
  sealed BLOB NOT NULL, nonce BLOB NOT NULL,
  issued_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, refresh_expires_at INTEGER,
  scopes TEXT NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_key_secrets (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  sealed BLOB NOT NULL, nonce BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS refresh_attempts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, account_id TEXT NOT NULL, lifecycle_generation INTEGER NOT NULL,
  credential_generation INTEGER NOT NULL, started_at INTEGER NOT NULL, resolved_at INTEGER,
  outcome TEXT, detail TEXT
);
CREATE INDEX IF NOT EXISTS refresh_attempts_pending ON refresh_attempts (account_id) WHERE resolved_at IS NULL OR outcome = 'uncertain';
"#;

/// Applied in order above the stored `schema_version`, each inside one transaction. A migration may drop or
/// rebuild a table, so an older build refuses a newer registry rather than misreading it.
pub const MIGRATIONS: &[(u32, &str)] = &[
    (2, V2),
    (3, V3),
    (4, V4),
    (5, V5),
    (6, V6),
    (7, V7),
    (8, V8),
    (9, V9),
    (10, V10),
    (11, V11),
    (12, V12),
    (13, V13),
    (14, V14),
    (15, V15),
    (16, V16),
    (17, V17),
    (18, V18),
    (19, V19),
    (20, V20),
    (21, V21),
    (22, V22),
    (23, V23),
    (24, V24),
    (25, V25),
    (26, V26),
    (27, V27),
    (28, V28),
    (29, V29),
    (30, V30),
    (31, V31),
    (32, V32),
    (33, V33),
    (34, V34),
    (35, V35),
    (36, V36),
    (37, V37),
    (38, V38),
    (39, V39),
    (40, V40),
    (41, V41),
    (42, V42),
    (43, V43),
    (44, V44),
    (45, V45),
    (46, V46),
    (47, V47),
    (48, V48),
    (49, V49),
    (50, V50),
    (51, V51),
    (52, V52),
    (53, V53),
    (54, V54),
    (55, V55),
    (56, V56),
    (57, V57),
    (58, V58),
];

/// The newest schema version this build knows how to read. `prepare` refuses a registry stamped past it
/// rather than skip migrations it does not carry and misread the tables that follow.
pub const SCHEMA_VERSION: u32 = 58;

/// The version that bounds an account to one pool; a file below it is read for overlaps before it migrates.
pub const ONE_POOL_VERSION: u32 = 47;

const V2: &str = r#"
CREATE TABLE IF NOT EXISTS account_models (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, upstream_id TEXT NOT NULL, canonical_id TEXT NOT NULL,
  selected INTEGER NOT NULL DEFAULT 1, source TEXT NOT NULL,
  long_context INTEGER NOT NULL DEFAULT 0, missing INTEGER NOT NULL DEFAULT 0, is_new INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, upstream_id),
  UNIQUE (account_id, canonical_id)
);
CREATE TABLE IF NOT EXISTS pools (
  id TEXT PRIMARY KEY, label TEXT NOT NULL, enabled INTEGER NOT NULL, strategy TEXT NOT NULL,
  affinity_ttl_seconds INTEGER NOT NULL DEFAULT 3600,
  overage_policy TEXT NOT NULL, migrate_above_utilization REAL, default_allowed_models_json TEXT,
  keepalive_json TEXT NOT NULL, body_capture_hours INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pool_members (
  pool_id TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  priority INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (pool_id, account_id)
);
CREATE TABLE IF NOT EXISTS api_keys (
  id TEXT PRIMARY KEY, label TEXT NOT NULL, notes TEXT, pool_id TEXT NOT NULL REFERENCES pools(id), enabled INTEGER NOT NULL,
  revoked_at INTEGER, expires_at INTEGER, allowed_models_json TEXT, keepalive_opt_out INTEGER NOT NULL DEFAULT 0,
  body_capture_hours INTEGER,
  secret_digest BLOB NOT NULL UNIQUE, prefix TEXT NOT NULL,
  created_at INTEGER NOT NULL, rotated_at INTEGER, last_used_at INTEGER
);
CREATE TABLE IF NOT EXISTS registry_changes (
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, actor TEXT NOT NULL, kind TEXT NOT NULL,
  target TEXT NOT NULL, diff_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pins (
  key_id TEXT NOT NULL, session_id TEXT NOT NULL, model TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  epoch INTEGER NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, session_id, model)
);
CREATE TABLE IF NOT EXISTS cooldowns (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, scope TEXT NOT NULL,
  until INTEGER NOT NULL, reason TEXT NOT NULL, window TEXT NOT NULL, source_request_id TEXT,
  PRIMARY KEY (account_id, scope)
);
CREATE TABLE IF NOT EXISTS overage_state (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  in_use INTEGER NOT NULL, since INTEGER, reset INTEGER, observed_at INTEGER NOT NULL, source TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS accounts_identity_uuid
  ON accounts (json_extract(identity_json, '$.account_uuid')) WHERE identity_json IS NOT NULL;
ALTER TABLE api_key_secrets ADD COLUMN generation INTEGER NOT NULL DEFAULT 1;
"#;

const V3: &str = r#"
CREATE INDEX IF NOT EXISTS registry_changes_target ON registry_changes (target, id);
"#;

// Rows written before this column existed start at 1, which is the same value a fresh row takes, so a
// verdict formed against a pre-migration row is only stale once something actually changes the account.
const V4: &str = r#"
ALTER TABLE accounts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
"#;

// An id is reusable, so deleting a slot has to leave behind the one thing its replacement cannot inherit:
// how many times that id has been used. The tombstone outlives the row it names, which is why it is its own
// table rather than a column, and the cascade that empties every other table of the account leaves it alone.
const V5: &str = r#"
ALTER TABLE accounts ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 1;
CREATE TABLE IF NOT EXISTS account_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
"#;

// Imported price tables are registry state: they travel with the accounts they price, and an operator's
// import is a registry change like any other.
const V6: &str = r#"
CREATE TABLE IF NOT EXISTS pricing_versions (
  version TEXT PRIMARY KEY, effective TEXT NOT NULL, source TEXT NOT NULL, table_json TEXT NOT NULL,
  imported_at INTEGER NOT NULL
);
"#;

// The event mirror pages settled attempts by `(resolved_at, id)` every few seconds; the pending index covers
// the rows that query excludes.
const V7: &str = r#"
CREATE INDEX IF NOT EXISTS refresh_attempts_resolved ON refresh_attempts (resolved_at, id) WHERE outcome IS NOT NULL;
"#;

// Null is off: a pool written before the depletion threshold existed keeps selecting from every eligible
// account, which is what it did.
const V8: &str = r#"
ALTER TABLE pools ADD COLUMN select_below_utilization REAL;
"#;

const V9: &str = r#"
ALTER TABLE cooldowns ADD COLUMN set_at INTEGER NOT NULL DEFAULT 0;
"#;

// Alert rules are registry rows edited in the UI, so they live beside pools and keys; events and deliveries
// cascade with their rule because an event without its rule has no subject to show.
//
// The two `state` vocabularies differ by design and the checks hold them apart: an event is `open` or
// `resolved`, while a delivery names the transition it carried, `opened` or `resolved`. Without the check an
// event could be stored in a state that escapes both the partial open index and the purge, leaving a row
// nothing ever closes.
const V10: &str = r#"
CREATE TABLE IF NOT EXISTS alert_rules (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, scope_type TEXT NOT NULL, scope_id TEXT,
  threshold REAL, window TEXT, webhook_url TEXT NOT NULL, secret_sealed BLOB, secret_nonce BLOB,
  enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS alert_events (
  id TEXT PRIMARY KEY, rule_id TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, subject TEXT NOT NULL, state TEXT NOT NULL CHECK (state IN ('open', 'resolved')),
  opened_at INTEGER NOT NULL, resolved_at INTEGER, detail_json TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS alert_events_open ON alert_events (rule_id, subject) WHERE state = 'open';
CREATE TABLE IF NOT EXISTS alert_deliveries (
  id INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT NOT NULL REFERENCES alert_events(id) ON DELETE CASCADE,
  state TEXT NOT NULL CHECK (state IN ('opened', 'resolved')),
  attempt INTEGER NOT NULL, at INTEGER NOT NULL, status INTEGER, error TEXT
);
CREATE INDEX IF NOT EXISTS alert_deliveries_event ON alert_deliveries (event_id, id);
"#;

// Round robin and the kind-ordered selection are per-pool choices, but the stored flag only governs
// `priority_random`: a pre-existing `least_utilized` or `earliest_refresh` pool takes the kind order at this
// version regardless, per the spec; only a `priority_random` pool keeps the priority-then-strategy order it had.
// A pin's migration time is what the dwell reads, null for a pin that never moved for utilization.
const V11: &str = r#"
ALTER TABLE pools ADD COLUMN prefer_subscription INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pools ADD COLUMN api_key_sub_strategy TEXT NOT NULL DEFAULT 'priority_random';
ALTER TABLE pins ADD COLUMN migrated_at INTEGER;
"#;

const V12: &str = r#"
"#;

// The policy is one JSON object rather than a column per field: a second override strategy joins as one more
// defaulted member of the object, read by every build that knows it and ignored by none. `cache_diagnostics`
// is nullable because null is the value that inherits the bootstrap option.
const V13: &str = r#"
ALTER TABLE pools ADD COLUMN cache_ttl_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE pools ADD COLUMN cache_diagnostics INTEGER;
"#;

// A rotation retires the previous secret rather than deleting it: the console promises running sessions a
// window to switch, and a session presenting the retired secret inside it is the operator's own. One row per
// key, so a second rotation inside the window ends the older secret at once; the grace is a bounded overlap of
// two secrets, never a chain.
const V14: &str = r#"
CREATE TABLE IF NOT EXISTS api_key_retired_secrets (
  key_id TEXT PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
  secret_digest BLOB NOT NULL, prefix TEXT NOT NULL,
  retired_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_key_tombstones (
  id TEXT PRIMARY KEY, deleted_at INTEGER NOT NULL
);
"#;

// V15 backfilled the retirement V14 introduced from the change log; V16 drops the table, so this statement is
// kept for the version chain alone and inserts into a table V16 removes.
const V15: &str = r#"
INSERT OR IGNORE INTO api_key_tombstones (id, deleted_at)
SELECT target, MAX(at) FROM registry_changes
WHERE kind = 'key_deleted' AND target NOT IN (SELECT id FROM api_keys)
GROUP BY target;
"#;

// The generation every ledger row now carries is what keeps a re-created id from reading an earlier holder's
// usage, so the retirement that stood in for it goes with its table.
const V16: &str = r#"
DROP TABLE IF EXISTS api_key_tombstones;
"#;

// A key says when its retired secret last authenticated, so the console can count the sessions still on it.
const V17: &str = r#"
ALTER TABLE api_keys ADD COLUMN retired_secret_last_used_at INTEGER;
"#;

// A lifecycle delivery hangs off no event row, so the event reference is optional and the row names its rule,
// kind, subject, and event itself; the state check gains the value such a delivery posts under. Rows from before
// this read an empty rule, kind, and subject.
const V18: &str = r#"
CREATE TABLE alert_deliveries_rebuilt (
  id INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT REFERENCES alert_events(id) ON DELETE CASCADE,
  rule_id TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL DEFAULT '', subject TEXT NOT NULL DEFAULT '',
  event TEXT, detail_json TEXT,
  state TEXT NOT NULL CHECK (state IN ('event', 'opened', 'resolved')),
  attempt INTEGER NOT NULL, at INTEGER NOT NULL, status INTEGER, error TEXT
);
INSERT INTO alert_deliveries_rebuilt (id, event_id, state, attempt, at, status, error)
  SELECT id, event_id, state, attempt, at, status, error FROM alert_deliveries;
DROP TABLE alert_deliveries;
ALTER TABLE alert_deliveries_rebuilt RENAME TO alert_deliveries;
CREATE INDEX IF NOT EXISTS alert_deliveries_event ON alert_deliveries (event_id, id);
CREATE INDEX IF NOT EXISTS alert_deliveries_lifecycle ON alert_deliveries (kind, id) WHERE event_id IS NULL;
"#;

// The relay's session memory on disk: one row per key generation, session, and agent, the payload a JSON
// document the relay owns and the version the shape it was written in, so the store never reads inside it and
// a later shape reads older rows by their version. The generation is in the key because an id is reusable and
// what one holder saw is never the next holder's. `last_seen` is what the retention purge measures from.
const V19: &str = r#"
CREATE TABLE IF NOT EXISTS session_memory (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id, agent_id)
);
CREATE INDEX IF NOT EXISTS session_memory_seen ON session_memory (last_seen);
"#;

// An id is reusable, so routing state and refresh attempts name the holder they were recorded for: a pin its
// key generation and account incarnation, a cooldown, an overage reading and an attempt the incarnation. A row
// from before the columns names no live holder (0 matches nothing) and leaves at the first reload, which is the
// safe reading for state whose holder cannot be told.
const V20: &str = r#"
ALTER TABLE pins ADD COLUMN key_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cooldowns ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE overage_state ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE refresh_attempts ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 0;
"#;

// A credential failure that never opens a refresh attempt (a backoff, a quarantine) has
// nowhere else to leave its reason; a row from before this column reads no error, which is the safe reading
// for a failure that predates the column.
const V21: &str = r#"
ALTER TABLE credentials ADD COLUMN last_error TEXT;
"#;

const V22: &str = r#"
"#;

// Both whitelist columns are dropped rather than renamed, so no old list is ever read as a rule list; neither sits
// in an index, a key, or a constraint. A fresh database replays the chain, so `V2` keeps the two columns this drops.
const V23: &str = r#"
ALTER TABLE pools DROP COLUMN default_allowed_models_json;
ALTER TABLE pools ADD COLUMN model_rules_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE api_keys DROP COLUMN allowed_models_json;
ALTER TABLE api_keys ADD COLUMN model_rules_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE api_keys ADD COLUMN model_rules_mode TEXT NOT NULL DEFAULT 'extend';
ALTER TABLE api_keys ADD COLUMN long_context_twins INTEGER NOT NULL DEFAULT 1;
CREATE TABLE model_resolutions (
    spelling TEXT PRIMARY KEY,
    dated_id TEXT NOT NULL,
    resolved_at INTEGER NOT NULL
);
DROP TABLE account_models;
ALTER TABLE accounts DROP COLUMN catalog_synced_at;
ALTER TABLE accounts DROP COLUMN catalog_error;
"#;

// The two dates travel as epoch milliseconds, the same shape every other timestamp column of this table
// holds. A row from before this migration reads all four as null until its next poll or pairing confirm.
const V24: &str = r#"
ALTER TABLE accounts ADD COLUMN subscription_plan TEXT;
ALTER TABLE accounts ADD COLUMN subscription_tier TEXT;
ALTER TABLE accounts ADD COLUMN subscription_since INTEGER;
ALTER TABLE accounts ADD COLUMN trial_ends_at INTEGER;
"#;

// The strategy set is three names: `pace` takes over both sample-driven strategies now that every account carries
// a sample, `priority` the random pick within a priority. The kind order is a free toggle every pool starts with
// on, so the sub-strategy that ordered the api_key tier under a strategy with no sample for it has nothing left to
// order. The per-pin migration dwell gives way to the source account's dwell, so `migrated_at` leaves. A pin from
// before the burn columns reads no burn and its last use as its last refresh; a key with no cap and an account
// nobody cancelled read as such, and an end is stored only beside the flag. `plan_fits` is the live fit of a
// window's size per account and window kind.
const V25: &str = r#"
UPDATE pools SET strategy = 'pace' WHERE strategy IN ('earliest_refresh', 'least_utilized');
UPDATE pools SET strategy = 'priority' WHERE strategy = 'priority_random';
UPDATE pools SET prefer_subscription = 1;
ALTER TABLE pools DROP COLUMN api_key_sub_strategy;
ALTER TABLE pools ADD COLUMN shed_share REAL NOT NULL DEFAULT 0.5;
ALTER TABLE pools ADD COLUMN burn_window_seconds INTEGER NOT NULL DEFAULT 600;
ALTER TABLE pools ADD COLUMN source_dwell_seconds INTEGER NOT NULL DEFAULT 900;
ALTER TABLE pools ADD COLUMN migrate_when_spent INTEGER NOT NULL DEFAULT 1;
ALTER TABLE pools ADD COLUMN warm_sessions TEXT NOT NULL DEFAULT 'when_needed';
ALTER TABLE pools ADD COLUMN retry_in_place_seconds INTEGER NOT NULL DEFAULT 10;
ALTER TABLE pools ADD COLUMN in_flight_cap INTEGER NOT NULL DEFAULT 16;
ALTER TABLE pools ADD COLUMN pace_floor REAL NOT NULL DEFAULT 0.05;
ALTER TABLE pools ADD COLUMN overage_move_grace_minutes INTEGER NOT NULL DEFAULT 30;
ALTER TABLE pools ADD COLUMN wind_down_minutes INTEGER NOT NULL DEFAULT 30;
ALTER TABLE pins DROP COLUMN migrated_at;
ALTER TABLE pins ADD COLUMN burn REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_at INTEGER;
ALTER TABLE pins ADD COLUMN last_input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN last_refresh_at INTEGER NOT NULL DEFAULT 0;
UPDATE pins SET last_refresh_at = last_used_at;
ALTER TABLE pins ADD COLUMN cache_ttl_ms INTEGER NOT NULL DEFAULT 300000;
ALTER TABLE pins ADD COLUMN shed_from TEXT;
ALTER TABLE pins ADD COLUMN shed_at INTEGER;
ALTER TABLE pins ADD COLUMN static_hash TEXT;
ALTER TABLE pins ADD COLUMN tail_hash TEXT;
ALTER TABLE api_keys ADD COLUMN in_flight_cap INTEGER;
ALTER TABLE accounts ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN subscription_ends_at INTEGER;
CREATE TABLE plan_fits (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, window TEXT NOT NULL,
  scale REAL NOT NULL, confidence REAL NOT NULL, intervals INTEGER NOT NULL,
  corrections_json TEXT NOT NULL DEFAULT '{}', fitted_at INTEGER NOT NULL, incarnation INTEGER NOT NULL,
  PRIMARY KEY (account_id, window)
);
"#;

const V26: &str = r#"
ALTER TABLE pins ADD COLUMN burn_usd REAL NOT NULL DEFAULT 0;
"#;

// A fit's carried run and its band sums are kept beside the scale, so a restart or a quiet weekend continues them
// rather than starting over.
const V27: &str = r#"
ALTER TABLE plan_fits ADD COLUMN band_factor REAL;
ALTER TABLE plan_fits ADD COLUMN carry_weighted REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN carry_inside REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN carry_outside REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN carry_delta REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN carry_since INTEGER;
ALTER TABLE plan_fits ADD COLUMN carry_utilization REAL;
ALTER TABLE plan_fits ADD COLUMN carry_reset INTEGER;
ALTER TABLE plan_fits ADD COLUMN band_inside_tokens REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN band_inside_delta REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN band_outside_tokens REAL NOT NULL DEFAULT 0;
ALTER TABLE plan_fits ADD COLUMN band_outside_delta REAL NOT NULL DEFAULT 0;
"#;

// The keepalive's daily probe budgets as each probe settled them, so a restart resumes both caps where they stood;
// the rollups carry no funding to rebuild them from. `scope` is `pool` for the dollar figure and `account` for the
// meter one; `day` is the UTC date, and the day roll deletes the days before it.
const V28: &str = r#"
CREATE TABLE keepalive_budgets (
  day TEXT NOT NULL, scope TEXT NOT NULL, id TEXT NOT NULL,
  usd REAL NOT NULL DEFAULT 0, meter_tokens REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (day, scope, id)
);
"#;

// A pin's agent joins its key so a subagent of a session pins apart from its main turn; every row the chain
// wrote reads 'main', which is what every request before this migration was. The primary key cannot be altered
// in place, so the table is rebuilt rather than columned onto in place. The cache TTL text is reshaped from one
// rule set into two, `plan` and `metered`, so a metered account's TTL no longer rides the same object as a plan
// account's; the override for an account in overage leaves with its five-minute default carried onto the
// metered set. The affinity table is the placement index that lets a fresh session or a subagent cohort land
// where its prefix is warm.
const V29: &str = r#"
CREATE TABLE pins_next (
  key_id TEXT NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, model TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  epoch INTEGER NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL,
  key_generation INTEGER NOT NULL DEFAULT 0, incarnation INTEGER NOT NULL DEFAULT 0,
  burn REAL NOT NULL DEFAULT 0, burn_at INTEGER, burn_usd REAL NOT NULL DEFAULT 0,
  last_input_tokens INTEGER NOT NULL DEFAULT 0, last_refresh_at INTEGER NOT NULL DEFAULT 0,
  cache_ttl_ms INTEGER NOT NULL DEFAULT 300000,
  shed_from TEXT, shed_at INTEGER, static_hash TEXT, tail_hash TEXT,
  PRIMARY KEY (key_id, session_id, agent_id, model)
);
INSERT INTO pins_next (key_id, session_id, agent_id, model, account_id, epoch, created_at, last_used_at,
  key_generation, incarnation, burn, burn_at, burn_usd, last_input_tokens, last_refresh_at, cache_ttl_ms,
  shed_from, shed_at, static_hash, tail_hash)
SELECT key_id, session_id, 'main', model, account_id, epoch, created_at, last_used_at,
  key_generation, incarnation, burn, burn_at, burn_usd, last_input_tokens, last_refresh_at, cache_ttl_ms,
  shed_from, shed_at, static_hash, tail_hash
FROM pins;
DROP TABLE pins;
ALTER TABLE pins_next RENAME TO pins;
ALTER TABLE pools ADD COLUMN affinity_mode TEXT NOT NULL DEFAULT 'always';
ALTER TABLE pools ADD COLUMN subagent_placement TEXT NOT NULL DEFAULT 'by_strategy';
ALTER TABLE pools ADD COLUMN session_placement TEXT NOT NULL DEFAULT 'by_strategy';
ALTER TABLE pools ADD COLUMN min_shared_prefix_tokens INTEGER NOT NULL DEFAULT 20000;
ALTER TABLE pools ADD COLUMN quota_headers TEXT NOT NULL DEFAULT 'pass';
UPDATE pools SET cache_ttl_json = json_object(
  'enabled', json(CASE WHEN json_extract(cache_ttl_json, '$.enabled') THEN 'true' ELSE 'false' END),
  'in_overage', CASE json_type(cache_ttl_json, '$.in_overage')
                  WHEN 'null' THEN json('null')
                  WHEN 'text' THEN json_extract(cache_ttl_json, '$.in_overage')
                  ELSE '5m' END,
  'main', COALESCE(json_extract(cache_ttl_json, '$.main'), '1h'),
  'models', json(COALESCE(json_extract(cache_ttl_json, '$.models'), '[]')),
  'subagent', COALESCE(json_extract(cache_ttl_json, '$.subagent'), '1h'))
WHERE json_valid(cache_ttl_json) AND json_type(cache_ttl_json) = 'object' AND cache_ttl_json <> '{}';
UPDATE pools SET cache_ttl_json = json_object(
  'enabled', json(CASE WHEN json_extract(cache_ttl_json, '$.enabled') THEN 'true' ELSE 'false' END),
  'inject_when_absent', json('false'),
  'metered', CASE WHEN json_type(cache_ttl_json, '$.in_overage') = 'null' THEN json_object(
      'keepalive', json_extract(cache_ttl_json, '$.main'),
      'main', json_extract(cache_ttl_json, '$.main'),
      'models', json(json_extract(cache_ttl_json, '$.models')),
      'subagent', json_extract(cache_ttl_json, '$.subagent'))
    ELSE json_object(
      'keepalive', json_extract(cache_ttl_json, '$.in_overage'),
      'main', json_extract(cache_ttl_json, '$.in_overage'),
      'models', json('[]'),
      'subagent', json_extract(cache_ttl_json, '$.in_overage')) END,
  'plan', json_object(
    'keepalive', json_extract(cache_ttl_json, '$.main'),
    'main', json_extract(cache_ttl_json, '$.main'),
    'models', json(json_extract(cache_ttl_json, '$.models')),
    'subagent', json_extract(cache_ttl_json, '$.subagent')))
WHERE json_valid(cache_ttl_json) AND json_type(cache_ttl_json) = 'object' AND cache_ttl_json <> '{}';
ALTER TABLE pool_members ADD COLUMN budget_usd REAL;
ALTER TABLE pool_members ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'month';
ALTER TABLE api_keys ADD COLUMN affinity_source TEXT NOT NULL DEFAULT 'auto';
CREATE TABLE affinity (
  model TEXT NOT NULL, hash TEXT NOT NULL, chain TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, incarnation INTEGER NOT NULL,
  written_at INTEGER NOT NULL, ttl_ms INTEGER NOT NULL, prefix_tokens INTEGER NOT NULL DEFAULT 0,
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, root TEXT NOT NULL,
  PRIMARY KEY (model, hash)
);
CREATE INDEX affinity_written_at ON affinity (written_at);
"#;

// The import that wrote this table is gone, and an attempt's pricing_version names the table in force by its
// effective date, so no reader is left for the rows.
const V30: &str = "DROP TABLE IF EXISTS pricing_versions;";

// A lifecycle deliveries page orders by `at` then `id`, and a page narrowed to a kind seeks the kind first, so
// each reads a partial index in its own order rather than sorting every lifecycle row. The `(kind, id)` index
// ordered by `id`, which no reader asks for.
const V31: &str = r#"
DROP INDEX IF EXISTS alert_deliveries_lifecycle;
CREATE INDEX alert_deliveries_lifecycle_at ON alert_deliveries (at, id) WHERE event_id IS NULL;
CREATE INDEX alert_deliveries_lifecycle_kind ON alert_deliveries (kind, at, id) WHERE event_id IS NULL;
"#;

// The newest reading of each of an OAuth account's plan windows, as the routing state last held it, so a restart
// or a startup poll the endpoint refuses routes from the last known windows; the api_key buckets refill within
// a minute and are not kept. Deleted with the account.
const V32: &str = r#"
CREATE TABLE usage_windows (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  window TEXT NOT NULL,
  utilization REAL NOT NULL, reset INTEGER, observed_at INTEGER NOT NULL,
  incarnation INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, window)
);
"#;

// A pin's burn is typed tokens per second, plain and weighed by the peak band at each renewal, priced when
// read; the keepalive's plan-side day budget is the share of the account's 5-hour window its probes consumed.
// Both sums restart at zero here: nothing has been released, and the old columns held another unit.
const V33: &str = r#"
ALTER TABLE pins DROP COLUMN burn;
ALTER TABLE pins DROP COLUMN burn_usd;
ALTER TABLE pins ADD COLUMN burn_input REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_cache_read REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_write_5m REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_write_1h REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_output REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_banded_input REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_banded_cache_read REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_banded_write_5m REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_banded_write_1h REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_banded_output REAL NOT NULL DEFAULT 0;
ALTER TABLE keepalive_budgets DROP COLUMN meter_tokens;
ALTER TABLE keepalive_budgets ADD COLUMN five_hour_share REAL NOT NULL DEFAULT 0;
ALTER TABLE keepalive_budgets ADD COLUMN five_hour_share_unmeasured INTEGER NOT NULL DEFAULT 0;
"#;

// Nothing read these rows once the meter's unit became the window share.
const V34: &str = "DROP TABLE IF EXISTS plan_fits;";

// The pool column carries the default so every existing pool reopens on; the key column is nullable because
// null is the value that inherits the pool's.
const V35: &str = r#"
ALTER TABLE pools ADD COLUMN auto_mode_server INTEGER NOT NULL DEFAULT 1;
ALTER TABLE api_keys ADD COLUMN auto_mode_server INTEGER;
"#;

// The cache lane a pin's last request and an entry's writer wrote to: a filter beside each table's key (a pin's
// `(key_id, session_id, agent_id, model)`, an entry's `(model, hash)`), so neither table is rebuilt. A row from
// before reads `''`, which adopts the lane of the next request that reads it.
const V36: &str = "ALTER TABLE pins ADD COLUMN lane TEXT NOT NULL DEFAULT ''; \
ALTER TABLE affinity ADD COLUMN lane TEXT NOT NULL DEFAULT '';";

// The newest extra-usage state the poll read for an OAuth account, as the routing state last held it, so a restart
// judges fast eligibility on the credits the last poll saw rather than admitting fast requests an account refuses.
// Kept and dropped as the usage windows are: under the holder's incarnation, deleted with the account.
const V37: &str = r#"
CREATE TABLE usage_credits (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL, disabled_reason TEXT, utilization REAL, observed_at INTEGER NOT NULL,
  incarnation INTEGER NOT NULL
);
"#;

// The share a plan-funded probe settles in is measured or not priced at all, so the day's row carries no flag. No
// code evaluates, patches or resolves an alert of the retired drift kind, and the console would show one under
// another kind's label, so its deliveries, events and rules go with it.
const V38: &str = r#"
ALTER TABLE keepalive_budgets DROP COLUMN five_hour_share_unmeasured;
DELETE FROM alert_deliveries WHERE kind = 'meter_drift'
  OR event_id IN (SELECT id FROM alert_events WHERE kind = 'meter_drift');
DELETE FROM alert_events WHERE kind = 'meter_drift';
DELETE FROM alert_rules WHERE kind = 'meter_drift';
"#;

// The rule sets' `keepalive` TTL left with the probe reading its agent's branch; a stored key would make the reader
// refuse the whole policy as unknown and read it as off, so the key goes from every stored policy.
const V39: &str = r#"
UPDATE pools SET cache_ttl_json = json_remove(cache_ttl_json, '$.plan.keepalive', '$.metered.keepalive')
WHERE json_valid(cache_ttl_json) AND json_type(cache_ttl_json) = 'object';
"#;

// Every row written before this version was keyed on the client's own session id.
// Rebuilt rather than updated in place: `session_id` is in the primary key, and a
// bare `s1` prefixed beside a stored `session:s1` would collide mid-update.
const V40: &str = r#"
CREATE TABLE session_memory_next (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id, agent_id)
);
INSERT INTO session_memory_next (key_id, key_generation, session_id, agent_id, record_version, payload, last_seen)
  SELECT key_id, key_generation, 'session:' || session_id, agent_id, record_version, payload, last_seen
  FROM session_memory;
DROP TABLE session_memory;
ALTER TABLE session_memory_next RENAME TO session_memory;
CREATE INDEX session_memory_seen ON session_memory (last_seen);
"#;

// A member's daily and monthly budgets stand side by side; the one budget with a period moves into the column its
// period names.
const V41: &str = r#"
ALTER TABLE pool_members ADD COLUMN budget_daily_usd REAL;
ALTER TABLE pool_members ADD COLUMN budget_monthly_usd REAL;
UPDATE pool_members SET budget_daily_usd = budget_usd WHERE budget_usd IS NOT NULL AND budget_period = 'day';
UPDATE pool_members SET budget_monthly_usd = budget_usd WHERE budget_usd IS NOT NULL AND budget_period = 'month';
ALTER TABLE pool_members DROP COLUMN budget_usd;
ALTER TABLE pool_members DROP COLUMN budget_period;
"#;

// The event index held every lifecycle row under a null event id, and the planner, without statistics, costed
// `event_id IS NULL` there at a few rows and sorted them all; partial, it holds the rows an event lookup and the
// cascade read and nothing a lifecycle page asks for.
const V42: &str = r#"
DROP INDEX IF EXISTS alert_deliveries_event;
CREATE INDEX alert_deliveries_event ON alert_deliveries (event_id, id) WHERE event_id IS NOT NULL;
"#;

// The relay hides the rate-limit header families on every committed answer and keeps them on a rate limit, so no
// pool chooses; the switch's column goes with the choice. A pool's diagnostics switch has no option left to inherit,
// so an unset one is stored off and every read finds a bool. A key bounds none of its requests and every key climbs
// the whole conversation climb, so the key's cap and its affinity source go the same way. Nothing configures, delivers or
// records an alert, so its three tables go, the deliveries first since they reference the events and the events the
// rules. The attention reader reads every account's newest settled attempts on each scrape, and the resolved index
// leads with the time, so the holder's own index keeps that read off a table the history purge cuts only at the
// retention, keeping each holder's newest two.
const V43: &str = r#"
ALTER TABLE pools DROP COLUMN quota_headers;
UPDATE pools SET cache_diagnostics = 0 WHERE cache_diagnostics IS NULL;
ALTER TABLE api_keys DROP COLUMN in_flight_cap;
ALTER TABLE api_keys DROP COLUMN affinity_source;
DROP TABLE alert_deliveries;
DROP TABLE alert_events;
DROP TABLE alert_rules;
CREATE INDEX refresh_attempts_holder_resolved ON refresh_attempts (account_id, incarnation, resolved_at, id)
  WHERE outcome IS NOT NULL;
"#;

// A holder's newest attempt is its highest id under its account and incarnation, which neither refresh index serves:
// one is partial on the pending attempts, the other on the settled ones. A key's `updated_at` dates its last change,
// and a key stored before this version takes the migration's instant, the earliest a reader can date it to. Only a
// session id or a message boundary identifies a conversation, so the request and response links leave and no loader
// meets a chain it does not know.
const V44: &str = r#"
CREATE INDEX refresh_attempts_account ON refresh_attempts (account_id, incarnation, id);
ALTER TABLE api_keys ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
UPDATE api_keys SET updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000;
DELETE FROM affinity WHERE chain IN ('request', 'response');
"#;

// The field names the rule it sets, a window running ahead of its pace by this much; the creating statement keeps
// its frozen text, so the rename runs on a fresh file as on an old one.
const V45: &str = r#"
ALTER TABLE pools RENAME COLUMN migrate_above_utilization TO shed_ahead_of_pace;
"#;

// One row per pin of a pool's and account's latest shedding round, marked or not, so the round itself (and the
// dwell it arms) is on disk even when it marked nothing. `round_at` is the same on every row of one round. `epoch`
// is the pin's at the round and `incarnation` the account's: a mark is read back only for the pin it was made on,
// never for a replacement pin under the key or a replaced account. A pin write that removes the pin or replaces it
// under another epoch clears the mark in its own transaction (`marked` 0, the row and its `round_at` kept for the
// dwell gate). A deleted account takes its rows with it, as the pins' cascade does. The affinity table gains the
// ordinal of each entry's boundary in its chain, which the routing map records at the write and the purge reports.
// The accounts table gains the word for who moved its state, so the routing log's transition row reads the cause off
// the registry rather than inferring it from the path a change reached the mirror by. A row from before the column
// reads `unrecorded`.
const V46: &str = r#"
CREATE TABLE IF NOT EXISTS shed_rounds (
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  incarnation INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  model TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  round_at INTEGER NOT NULL,
  marked INTEGER NOT NULL,
  PRIMARY KEY (pool_id, account_id, key_id, session_id, agent_id, model)
);
ALTER TABLE affinity ADD COLUMN depth INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN state_cause TEXT NOT NULL DEFAULT 'unrecorded';
"#;

// An account's one membership, enforced below the member write's own check.
const V47: &str = "CREATE UNIQUE INDEX pool_members_one_pool ON pool_members (account_id);";

// A pool or key created again under a deleted id must not answer its predecessor's edit token; the tombstones
// outlive the rows as the accounts' do.
const V48: &str = r#"
ALTER TABLE pools ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 1;
ALTER TABLE api_keys ADD COLUMN incarnation INTEGER NOT NULL DEFAULT 1;
CREATE TABLE IF NOT EXISTS pool_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS api_key_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
"#;

// The keepalive engine's state, so a restart keeps its sessions, its agents with their retained prompts and the
// idle-age counters. An agent row carries one versioned payload of the agent's fields rather than a column per field,
// so a field the engine reshapes moves the payload's version and no column. The prompt sits beside it sealed under the
// storage key, with the header block, the size and the dispatch TTL the probe replays it with.
const V49: &str = r#"
CREATE TABLE keepalive_sessions (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id)
);
CREATE TABLE keepalive_agents (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL,
  body_sealed BLOB, body_nonce BLOB, headers TEXT NOT NULL, body_size INTEGER NOT NULL, dispatched_ms INTEGER, fast INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id, agent_id)
);
CREATE TABLE keepalive_idle_returns (
  pool_id TEXT NOT NULL, bucket INTEGER NOT NULL, reached INTEGER NOT NULL, returned INTEGER NOT NULL,
  PRIMARY KEY (pool_id, bucket)
);
"#;

// Nothing reads the day budgets once the keepalive has no daily probe cap.
const V50: &str = "DROP TABLE keepalive_budgets;";

// The upstream id of the newest answer at a message boundary, so the relay's cache diagnostics member names the
// answer a request continues rather than the session's last one. Nullable: a static entry never carries one, nor
// does a row from before the column.
const V51: &str = "ALTER TABLE affinity ADD COLUMN message_id TEXT;";

// A fast pin's burn beside its standard and banded copies, so a restart reads it as saved.
const V52: &str = r#"
ALTER TABLE pins ADD COLUMN burn_fast_input REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_fast_cache_read REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_fast_write_5m REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_fast_write_1h REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_fast_output REAL NOT NULL DEFAULT 0;
"#;

// A shedding mark lives on the pin it was made on: when it was made, the account it was made on and the window whose
// trigger made it. The rounds table keeps one row per pool and account with the round's time alone, which the dwell
// between rounds reads. The old rounds are dropped with the table: nothing released depends on them.
const V53: &str = r#"
ALTER TABLE pins ADD COLUMN marked_at INTEGER;
ALTER TABLE pins ADD COLUMN marked_on TEXT;
ALTER TABLE pins ADD COLUMN marked_window TEXT;
DROP TABLE shed_rounds;
CREATE TABLE shed_rounds (
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  incarnation INTEGER NOT NULL,
  round_at INTEGER NOT NULL,
  PRIMARY KEY (pool_id, account_id)
);
"#;

// The shedding fields take the names of their meaning. The switch keeps the stored intent of the weekly lead it
// replaces, a null lead being the operator's off, and changes no value, so it logs nothing. A stored value outside its
// new bound is clamped and the clamp logged as a pool update, so the registry panel shows what the migration moved.
const V54: &str = r#"
ALTER TABLE pools RENAME COLUMN shed_share TO shed_cap;
ALTER TABLE pools RENAME COLUMN burn_window_seconds TO burn_average_seconds;
ALTER TABLE pools RENAME COLUMN source_dwell_seconds TO shed_dwell_seconds;
ALTER TABLE pools ADD COLUMN load_shedding INTEGER NOT NULL DEFAULT 1;
UPDATE pools SET load_shedding = CASE WHEN shed_ahead_of_pace IS NULL THEN 0 ELSE 1 END;
ALTER TABLE pools DROP COLUMN shed_ahead_of_pace;
ALTER TABLE pools DROP COLUMN select_below_utilization;
ALTER TABLE pools DROP COLUMN pace_floor;
ALTER TABLE pools ADD COLUMN window_ceiling REAL NOT NULL DEFAULT 0.95;
INSERT INTO registry_changes (at, actor, kind, target, diff_json)
  SELECT CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'migration', 'pool_updated', id,
         json_object('burn_average_seconds', MIN(MAX(burn_average_seconds, 600), 7200),
                     'shed_cap', MIN(MAX(shed_cap, 0.1), 1.0),
                     'shed_dwell_seconds', MIN(MAX(shed_dwell_seconds, 300), 7200))
  FROM pools
  WHERE burn_average_seconds NOT BETWEEN 600 AND 7200 OR shed_cap NOT BETWEEN 0.1 AND 1.0
     OR shed_dwell_seconds NOT BETWEEN 300 AND 7200;
UPDATE pools SET burn_average_seconds = MIN(MAX(burn_average_seconds, 600), 7200),
                 shed_cap = MIN(MAX(shed_cap, 0.1), 1.0),
                 shed_dwell_seconds = MIN(MAX(shed_dwell_seconds, 300), 7200);
"#;

// The return dwell starts at each pool's shed dwell, the value that timed the return before this column, so no pool's
// return timing moves at the migration. The copy lies inside the new bound and changes no value's meaning, so nothing
// is logged.
const V55: &str = r#"
ALTER TABLE pools ADD COLUMN slow_down INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pools ADD COLUMN slow_down_stretch_seconds INTEGER NOT NULL DEFAULT 9000;
ALTER TABLE pools ADD COLUMN slow_down_hold_seconds INTEGER NOT NULL DEFAULT 120;
ALTER TABLE pools ADD COLUMN return_to_plan TEXT NOT NULL DEFAULT 'always';
ALTER TABLE pools ADD COLUMN return_to_plan_dwell_seconds INTEGER NOT NULL DEFAULT 900;
UPDATE pools SET return_to_plan_dwell_seconds = shed_dwell_seconds;
"#;

// The entrypoint of the request that placed the pin, the regime its figures price under: a column beside the row's
// key, so the table is not rebuilt, and a row from before reads `''`, the base regime, until its next request stamps
// it.
const V56: &str = "ALTER TABLE pins ADD COLUMN entrypoint TEXT NOT NULL DEFAULT '';";

// A guardrail is a named list of conditions a request is checked against before any account is selected, attached to
// pools and keys in a position per subject. One attachment table with a kind column rather than two, since the pool's
// and the key's lists are one shape with one writer, and a subject column cannot reference two tables, so the pool and
// key deletes remove their rows themselves. The tombstones outlive the rows as the pools' do.
const V57: &str = r#"
CREATE TABLE IF NOT EXISTS guardrails (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  enabled INTEGER NOT NULL DEFAULT 1, mode TEXT NOT NULL DEFAULT 'observe', action TEXT NOT NULL DEFAULT 'block',
  conditions_json TEXT NOT NULL, preset_id TEXT, preset_version INTEGER,
  incarnation INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS guardrail_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS guardrail_attachments (
  guardrail_id TEXT NOT NULL REFERENCES guardrails(id) ON DELETE CASCADE,
  subject_kind TEXT NOT NULL, subject_id TEXT NOT NULL, position INTEGER NOT NULL,
  PRIMARY KEY (guardrail_id, subject_kind, subject_id)
);
CREATE INDEX IF NOT EXISTS guardrail_attachments_subject ON guardrail_attachments (subject_kind, subject_id, position);
"#;

// The pin's unmetered burn: the usage of the client's own classifier calls, priced in dollars alone and never under
// a window share, as five columns beside the fast copy's, and whether a classifier call renewed the pin last, whose
// stored context then prices no rewrite. Zero and unset on every standing pin, which the defaults give.
const V58: &str = r#"
ALTER TABLE pins ADD COLUMN burn_unmetered_cache_read REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_unmetered_input REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_unmetered_output REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_unmetered_write_1h REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN burn_unmetered_write_5m REAL NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN last_classifier INTEGER NOT NULL DEFAULT 0;
"#;
