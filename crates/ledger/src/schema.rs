//! The ledger schema: `requests`, `attempts`, `bodies`, `quota_samples`, `extra_usage_samples`,
//! `rollups_hourly`, `account_rollups_hourly`, and `events`, plus `meta` for the schema marker.

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS requests (
  id TEXT PRIMARY KEY,
  received_at INTEGER NOT NULL,
  finished_at INTEGER,
  key_id TEXT,
  pool_id TEXT,
  session_id TEXT,
  agent_id TEXT,
  parent_agent_id TEXT,
  model TEXT NOT NULL,
  canonical_model TEXT NOT NULL,
  stream INTEGER NOT NULL,
  kind TEXT NOT NULL,
  origin TEXT NOT NULL,
  status INTEGER,
  outcome TEXT NOT NULL,
  attempts INTEGER NOT NULL,
  request_bytes INTEGER,
  response_bytes INTEGER,
  ttfb_ms INTEGER,
  ttfc_ms INTEGER,
  duration_ms INTEGER,
  error_code TEXT,
  kept_warm INTEGER NOT NULL DEFAULT 0,
  bodies_captured INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS attempts (
  request_id TEXT NOT NULL,
  attempt INTEGER NOT NULL,
  account_id TEXT NOT NULL,
  account_kind TEXT NOT NULL,
  pin_epoch INTEGER,
  started_at INTEGER NOT NULL,
  finished_at INTEGER,
  ttfb_ms INTEGER,
  ttfc_ms INTEGER,
  upstream_status INTEGER,
  upstream_request_id TEXT,
  outcome TEXT NOT NULL,
  cooldown_scope TEXT,
  cooldown_until INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cache_read_tokens INTEGER,
  cache_write_5m_tokens INTEGER,
  cache_write_1h_tokens INTEGER,
  service_tier TEXT,
  cache_miss_reason TEXT,
  client_drift TEXT,
  ratelimit_5h_utilization REAL,
  ratelimit_5h_reset INTEGER,
  ratelimit_7d_utilization REAL,
  ratelimit_7d_reset INTEGER,
  ratelimit_7d_oi_utilization REAL,
  ratelimit_7d_oi_reset INTEGER,
  overage_in_use INTEGER,
  overage_utilization REAL,
  estimated_usd REAL,
  pricing_version TEXT,
  PRIMARY KEY (request_id, attempt)
);
CREATE TABLE IF NOT EXISTS bodies (
  request_id TEXT PRIMARY KEY,
  request_body BLOB NOT NULL,
  response_body BLOB,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS quota_samples (
  account_id TEXT NOT NULL,
  bucket INTEGER NOT NULL,
  window TEXT NOT NULL,
  utilization REAL,
  reset_at INTEGER,
  source TEXT NOT NULL,
  PRIMARY KEY (account_id, bucket, window)
);
CREATE TABLE IF NOT EXISTS extra_usage_samples (
  account_id TEXT NOT NULL,
  bucket INTEGER NOT NULL,
  is_enabled INTEGER NOT NULL,
  in_use INTEGER,
  monthly_limit INTEGER,
  used_credits INTEGER,
  utilization REAL,
  currency TEXT,
  disabled_reason TEXT,
  overage_reset INTEGER,
  source TEXT NOT NULL,
  PRIMARY KEY (account_id, bucket)
);
CREATE TABLE IF NOT EXISTS rollups_hourly (
  hour INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  canonical_model TEXT NOT NULL,
  kind TEXT NOT NULL,
  requests INTEGER NOT NULL,
  succeeded INTEGER NOT NULL,
  rate_limited INTEGER NOT NULL,
  failed INTEGER NOT NULL,
  exhausted INTEGER NOT NULL,
  rejected INTEGER NOT NULL,
  attempts INTEGER NOT NULL,
  failovers INTEGER NOT NULL,
  cache_hits INTEGER NOT NULL,
  cache_misses INTEGER NOT NULL,
  uncached_requests INTEGER NOT NULL,
  input_tokens INTEGER NOT NULL,
  cache_read_tokens INTEGER NOT NULL,
  cache_write_5m_tokens INTEGER NOT NULL,
  cache_write_1h_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  estimated_usd REAL NOT NULL,
  input_usd REAL NOT NULL,
  cache_read_usd REAL NOT NULL,
  cache_write_usd REAL NOT NULL,
  output_usd REAL NOT NULL,
  kept_warm_requests INTEGER NOT NULL,
  kept_warm_tokens INTEGER NOT NULL,
  saved_usd_gross REAL NOT NULL,
  probe_cost_usd REAL NOT NULL,
  probe_migrated INTEGER NOT NULL,
  wasted_probes INTEGER NOT NULL,
  wasted_probe_usd REAL NOT NULL,
  wasted_probe_reasons_json TEXT NOT NULL,
  ttfb_hist BLOB NOT NULL,
  ttfc_hist BLOB NOT NULL,
  duration_hist BLOB NOT NULL,
  output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL,
  cache_miss_reasons_json TEXT NOT NULL,
  PRIMARY KEY (hour, key_id, pool_id, account_id, canonical_model, kind)
);
CREATE TABLE IF NOT EXISTS account_rollups_hourly (
  hour INTEGER NOT NULL,
  account_id TEXT NOT NULL,
  utilization_5h_max REAL,
  utilization_7d_max REAL,
  utilization_7d_oi_max REAL,
  overage_seconds INTEGER NOT NULL,
  cooldown_seconds INTEGER NOT NULL,
  pins_created INTEGER NOT NULL,
  pins_migrated_in INTEGER NOT NULL,
  pins_migrated_out INTEGER NOT NULL,
  PRIMARY KEY (hour, account_id)
);
CREATE TABLE IF NOT EXISTS events (
  at INTEGER NOT NULL,
  kind TEXT NOT NULL,
  account_id TEXT,
  key_id TEXT,
  session_id TEXT,
  model TEXT,
  actor TEXT,
  detail TEXT
);
CREATE INDEX IF NOT EXISTS requests_received ON requests (received_at);
CREATE INDEX IF NOT EXISTS requests_session ON requests (session_id, received_at);
CREATE INDEX IF NOT EXISTS requests_key ON requests (key_id, received_at);
CREATE INDEX IF NOT EXISTS attempts_account ON attempts (account_id, started_at);
CREATE INDEX IF NOT EXISTS quota_samples_account ON quota_samples (account_id, window, bucket);
CREATE INDEX IF NOT EXISTS events_at ON events (at);
"#;

/// Version 2: the split a request was folded with has to be readable from its own rows, or a replace cannot
/// take back what was actually added; version 1 only ever stored `estimated_usd`, the row-level total.
///
/// Version 3: every client version is relayed on its own headers, so which client made a request is a fact
/// about the request rather than something the profile implies.
pub const MIGRATIONS: &[(u32, &str)] = &[
    (
        2,
        "ALTER TABLE attempts ADD COLUMN input_usd REAL;
ALTER TABLE attempts ADD COLUMN cache_read_usd REAL;
ALTER TABLE attempts ADD COLUMN cache_write_usd REAL;
ALTER TABLE attempts ADD COLUMN output_usd REAL;",
    ),
    (3, "ALTER TABLE requests ADD COLUMN user_agent TEXT;"),
];
