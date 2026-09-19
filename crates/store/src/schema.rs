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

/// Applied in order above the stored `schema_version`, each inside one transaction. Every statement is
/// additive so a downgrade can still read the older tables.
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
];

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
