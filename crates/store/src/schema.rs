//! The registry schema at version 1: `account_tombstones`, `accounts`, `affinity`, `api_key_retired_secrets`,
//! `api_key_secrets`, `api_key_tombstones`, `api_keys`, `cooldowns`, `credentials`, `guardrail_attachments`,
//! `guardrail_tombstones`, `guardrails`, `keepalive_agents`, `keepalive_idle_returns`, `keepalive_sessions`,
//! `model_resolutions`, `overage_state`, `pins`, `pool_members`, `pool_tombstones`, `pools`, `refresh_attempts`,
//! `registry_changes`, `session_memory`, `shed_rounds`, `usage_credits` and `usage_windows`, plus `meta` for the schema
//! marker.

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, label TEXT NOT NULL, enabled INTEGER NOT NULL,
  identity_json TEXT, profile_json TEXT, state TEXT NOT NULL, lifecycle_generation INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, revision INTEGER NOT NULL DEFAULT 1,
  incarnation INTEGER NOT NULL DEFAULT 1, subscription_plan TEXT, subscription_tier TEXT, subscription_since INTEGER,
  trial_ends_at INTEGER, canceled INTEGER NOT NULL DEFAULT 0, subscription_ends_at INTEGER,
  state_cause TEXT NOT NULL DEFAULT 'unrecorded', plan_ended_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS accounts_identity_uuid
  ON accounts (json_extract(identity_json, '$.account_uuid')) WHERE identity_json IS NOT NULL;
CREATE TABLE IF NOT EXISTS account_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS credentials (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  generation INTEGER NOT NULL, lifecycle_generation INTEGER NOT NULL,
  sealed BLOB NOT NULL, nonce BLOB NOT NULL,
  issued_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, refresh_expires_at INTEGER,
  scopes TEXT NOT NULL, updated_at INTEGER NOT NULL, last_error TEXT
);
CREATE TABLE IF NOT EXISTS api_key_secrets (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  sealed BLOB NOT NULL, nonce BLOB NOT NULL, generation INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS refresh_attempts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, account_id TEXT NOT NULL, lifecycle_generation INTEGER NOT NULL,
  credential_generation INTEGER NOT NULL, started_at INTEGER NOT NULL, resolved_at INTEGER,
  outcome TEXT, detail TEXT, incarnation INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS refresh_attempts_pending ON refresh_attempts (account_id)
  WHERE resolved_at IS NULL OR outcome = 'uncertain';
CREATE INDEX IF NOT EXISTS refresh_attempts_resolved ON refresh_attempts (resolved_at, id) WHERE outcome IS NOT NULL;
CREATE INDEX IF NOT EXISTS refresh_attempts_holder_resolved ON refresh_attempts (account_id, incarnation, resolved_at, id)
  WHERE outcome IS NOT NULL;
CREATE INDEX IF NOT EXISTS refresh_attempts_account ON refresh_attempts (account_id, incarnation, id);
CREATE TABLE IF NOT EXISTS pools (
  id TEXT PRIMARY KEY, label TEXT NOT NULL, enabled INTEGER NOT NULL, strategy TEXT NOT NULL,
  affinity_ttl_seconds INTEGER NOT NULL DEFAULT 3600,
  overage_policy TEXT NOT NULL, keepalive_json TEXT NOT NULL, body_capture_hours INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, prefer_subscription INTEGER NOT NULL DEFAULT 0,
  cache_ttl_json TEXT NOT NULL DEFAULT '{}', cache_diagnostics INTEGER, model_rules_json TEXT NOT NULL DEFAULT '[]',
  shed_cap REAL NOT NULL DEFAULT 0.5, burn_average_seconds INTEGER NOT NULL DEFAULT 600,
  shed_dwell_seconds INTEGER NOT NULL DEFAULT 900, migrate_when_spent INTEGER NOT NULL DEFAULT 1,
  warm_sessions TEXT NOT NULL DEFAULT 'when_needed', retry_in_place_seconds INTEGER NOT NULL DEFAULT 10,
  in_flight_cap INTEGER NOT NULL DEFAULT 16, overage_move_grace_minutes INTEGER NOT NULL DEFAULT 30,
  wind_down_minutes INTEGER NOT NULL DEFAULT 30, affinity_mode TEXT NOT NULL DEFAULT 'always',
  subagent_placement TEXT NOT NULL DEFAULT 'by_strategy', session_placement TEXT NOT NULL DEFAULT 'by_strategy',
  min_shared_prefix_tokens INTEGER NOT NULL DEFAULT 20000, auto_mode_server INTEGER NOT NULL DEFAULT 1,
  incarnation INTEGER NOT NULL DEFAULT 1, load_shedding INTEGER NOT NULL DEFAULT 1,
  window_ceiling REAL NOT NULL DEFAULT 0.95, slow_down INTEGER NOT NULL DEFAULT 0,
  slow_down_stretch_seconds INTEGER NOT NULL DEFAULT 9000, slow_down_hold_seconds INTEGER NOT NULL DEFAULT 120,
  return_to_plan TEXT NOT NULL DEFAULT 'always', return_to_plan_dwell_seconds INTEGER NOT NULL DEFAULT 900
);
CREATE TABLE IF NOT EXISTS pool_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pool_members (
  pool_id TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  priority INTEGER NOT NULL DEFAULT 0, budget_daily_usd REAL, budget_monthly_usd REAL,
  PRIMARY KEY (pool_id, account_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS pool_members_one_pool ON pool_members (account_id);
CREATE TABLE IF NOT EXISTS api_keys (
  id TEXT PRIMARY KEY, label TEXT NOT NULL, notes TEXT, pool_id TEXT NOT NULL REFERENCES pools(id),
  enabled INTEGER NOT NULL, revoked_at INTEGER, expires_at INTEGER, keepalive_opt_out INTEGER NOT NULL DEFAULT 0,
  body_capture_hours INTEGER, secret_digest BLOB NOT NULL UNIQUE, prefix TEXT NOT NULL,
  created_at INTEGER NOT NULL, rotated_at INTEGER, last_used_at INTEGER, retired_secret_last_used_at INTEGER,
  model_rules_json TEXT NOT NULL DEFAULT '[]', model_rules_mode TEXT NOT NULL DEFAULT 'extend',
  long_context_twins INTEGER NOT NULL DEFAULT 1, auto_mode_server INTEGER, updated_at INTEGER NOT NULL DEFAULT 0,
  incarnation INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS api_key_retired_secrets (
  key_id TEXT PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
  secret_digest BLOB NOT NULL, prefix TEXT NOT NULL,
  retired_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_key_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS registry_changes (
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, actor TEXT NOT NULL, kind TEXT NOT NULL,
  target TEXT NOT NULL, diff_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS registry_changes_target ON registry_changes (target, id);
CREATE TABLE IF NOT EXISTS "pins" (
  key_id TEXT NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, model TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  epoch INTEGER NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL,
  key_generation INTEGER NOT NULL DEFAULT 0, incarnation INTEGER NOT NULL DEFAULT 0,
  burn_at INTEGER, last_input_tokens INTEGER NOT NULL DEFAULT 0, last_refresh_at INTEGER NOT NULL DEFAULT 0,
  cache_ttl_ms INTEGER NOT NULL DEFAULT 300000,
  shed_from TEXT, shed_at INTEGER, static_hash TEXT, tail_hash TEXT,
  burn_input REAL NOT NULL DEFAULT 0, burn_cache_read REAL NOT NULL DEFAULT 0, burn_write_5m REAL NOT NULL DEFAULT 0,
  burn_write_1h REAL NOT NULL DEFAULT 0, burn_output REAL NOT NULL DEFAULT 0,
  burn_banded_input REAL NOT NULL DEFAULT 0, burn_banded_cache_read REAL NOT NULL DEFAULT 0,
  burn_banded_write_5m REAL NOT NULL DEFAULT 0, burn_banded_write_1h REAL NOT NULL DEFAULT 0,
  burn_banded_output REAL NOT NULL DEFAULT 0, lane TEXT NOT NULL DEFAULT '',
  burn_fast_input REAL NOT NULL DEFAULT 0, burn_fast_cache_read REAL NOT NULL DEFAULT 0,
  burn_fast_write_5m REAL NOT NULL DEFAULT 0, burn_fast_write_1h REAL NOT NULL DEFAULT 0,
  burn_fast_output REAL NOT NULL DEFAULT 0, marked_at INTEGER, marked_on TEXT, marked_window TEXT,
  entrypoint TEXT NOT NULL DEFAULT '',
  burn_unmetered_cache_read REAL NOT NULL DEFAULT 0, burn_unmetered_input REAL NOT NULL DEFAULT 0,
  burn_unmetered_output REAL NOT NULL DEFAULT 0, burn_unmetered_write_1h REAL NOT NULL DEFAULT 0,
  burn_unmetered_write_5m REAL NOT NULL DEFAULT 0, last_classifier INTEGER NOT NULL DEFAULT 0, shed_reason TEXT,
  PRIMARY KEY (key_id, session_id, agent_id, model)
);
CREATE TABLE IF NOT EXISTS affinity (
  model TEXT NOT NULL, hash TEXT NOT NULL, chain TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, incarnation INTEGER NOT NULL,
  written_at INTEGER NOT NULL, ttl_ms INTEGER NOT NULL, prefix_tokens INTEGER NOT NULL DEFAULT 0,
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, root TEXT NOT NULL, lane TEXT NOT NULL DEFAULT '',
  depth INTEGER NOT NULL DEFAULT 0, message_id TEXT,
  PRIMARY KEY (model, hash)
);
CREATE INDEX IF NOT EXISTS affinity_written_at ON affinity (written_at);
CREATE TABLE IF NOT EXISTS cooldowns (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, scope TEXT NOT NULL,
  until INTEGER NOT NULL, reason TEXT NOT NULL, window TEXT NOT NULL, source_request_id TEXT,
  set_at INTEGER NOT NULL DEFAULT 0, incarnation INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, scope)
);
CREATE TABLE IF NOT EXISTS overage_state (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  in_use INTEGER NOT NULL, since INTEGER, reset INTEGER, observed_at INTEGER NOT NULL, source TEXT NOT NULL,
  incarnation INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS usage_windows (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  window TEXT NOT NULL,
  utilization REAL NOT NULL, reset INTEGER, observed_at INTEGER NOT NULL,
  incarnation INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, window)
);
CREATE TABLE IF NOT EXISTS usage_credits (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL, disabled_reason TEXT, utilization REAL, observed_at INTEGER NOT NULL,
  incarnation INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS shed_rounds (
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  incarnation INTEGER NOT NULL,
  round_at INTEGER NOT NULL,
  PRIMARY KEY (pool_id, account_id)
);
CREATE TABLE IF NOT EXISTS model_resolutions (
  spelling TEXT PRIMARY KEY,
  dated_id TEXT NOT NULL,
  resolved_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS "session_memory" (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id, agent_id)
);
CREATE INDEX IF NOT EXISTS session_memory_seen ON session_memory (last_seen);
CREATE TABLE IF NOT EXISTS keepalive_sessions (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id)
);
CREATE TABLE IF NOT EXISTS keepalive_agents (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL,
  body_sealed BLOB, body_nonce BLOB, headers TEXT NOT NULL, body_size INTEGER NOT NULL, dispatched_ms INTEGER,
  fast INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id, agent_id)
);
CREATE TABLE IF NOT EXISTS keepalive_idle_returns (
  pool_id TEXT NOT NULL, bucket INTEGER NOT NULL, reached INTEGER NOT NULL, returned INTEGER NOT NULL,
  PRIMARY KEY (pool_id, bucket)
);
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

/// Applied in order above the stored `schema_version`, each inside one transaction. A migration may drop or
/// rebuild a table, so an older build refuses a newer registry rather than misreading it.
pub const MIGRATIONS: &[(u32, &str)] = &[];

/// The newest schema version this build knows how to read. `prepare` refuses a registry stamped past it
/// rather than skip migrations it does not carry and misread the tables that follow.
pub const SCHEMA_VERSION: u32 = 1;
