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
pub const MIGRATIONS: &[(u32, &str)] = &[(2, V2), (3, V3), (4, V4), (5, V5)];

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
