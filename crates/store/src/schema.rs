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
];

/// The newest schema version this build knows how to read. `prepare` refuses a registry stamped past it
/// rather than skip migrations it does not carry and misread the tables that follow.
pub const SCHEMA_VERSION: u32 = 22;

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
