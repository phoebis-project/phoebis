//! The ledger schema at version 1: `account_rollups_hourly`, `attempts`, `bodies`, `cooldown_segments`,
//! `cooldown_spans`, `decisions`, `events`, `extra_usage_samples`, `kept_warm_marks`, `parked_records`,
//! `quota_samples`, `request_guardrails`, `requests`, `rollups_hourly`, `routing_conditions` and
//! `window_reconciliations`, plus `meta` for the schema marker.

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
  bodies_captured INTEGER NOT NULL DEFAULT 0,
  user_agent TEXT,
  key_generation INTEGER NOT NULL DEFAULT 0,
  effort TEXT,
  thinking_type TEXT,
  thinking_budget INTEGER,
  max_tokens INTEGER,
  tool_count INTEGER NOT NULL DEFAULT 0,
  system_blocks INTEGER NOT NULL DEFAULT 0,
  message_count INTEGER NOT NULL DEFAULT 0,
  context_edits TEXT NOT NULL DEFAULT '',
  temperature REAL,
  requested_speed TEXT,
  requested_service_tier TEXT,
  presented_retired INTEGER NOT NULL DEFAULT 0,
  rotation_at INTEGER,
  client_drift TEXT,
  request_truncated INTEGER NOT NULL DEFAULT 0,
  response_truncated INTEGER NOT NULL DEFAULT 0,
  client TEXT,
  agent_type TEXT,
  compaction TEXT,
  context_compacted TEXT,
  prev_tool_ms INTEGER,
  turn_origin TEXT,
  request_class TEXT,
  identified_by TEXT,
  identity_root TEXT,
  identity_depth INTEGER,
  forked_from TEXT,
  fork_depth INTEGER,
  auto_mode TEXT,
  held TEXT,
  stream_end_at INTEGER,
  diagnostics_source TEXT,
  guardrail_id TEXT,
  classifier TEXT,
  client_contract TEXT,
  landed_at INTEGER,
  refused INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS requests_key ON requests (key_id, received_at);
CREATE INDEX IF NOT EXISTS requests_landed_at ON requests (landed_at);
CREATE INDEX IF NOT EXISTS requests_received ON requests (received_at);
CREATE INDEX IF NOT EXISTS requests_session ON requests (session_id, received_at);
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
  thinking_tokens INTEGER,
  web_search_requests INTEGER,
  web_fetch_requests INTEGER,
  stop_reason TEXT,
  stop_sequence TEXT,
  served_model TEXT,
  speed TEXT,
  inference_geo TEXT,
  cleared_input_tokens INTEGER,
  refusal_category TEXT,
  refusal_recommended_model TEXT,
  selection TEXT,
  upstream_message_id TEXT,
  written_at INTEGER,
  tier TEXT,
  usage_based INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (request_id, attempt)
);
CREATE INDEX IF NOT EXISTS attempts_account ON attempts (account_id, started_at);
CREATE TABLE IF NOT EXISTS bodies (
  request_id TEXT PRIMARY KEY,
  request_body BLOB NOT NULL,
  response_body BLOB,
  expires_at INTEGER NOT NULL,
  request_len_seen INTEGER NOT NULL DEFAULT 0,
  response_len_seen INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS bodies_expires ON bodies (expires_at);
CREATE TABLE IF NOT EXISTS decisions (
  request_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  attempt INTEGER NOT NULL,
  at INTEGER NOT NULL,
  kind TEXT NOT NULL,
  probe INTEGER NOT NULL DEFAULT 0,
  key_id TEXT,
  pool_id TEXT,
  account_id TEXT,
  from_account_id TEXT,
  session_id TEXT,
  agent_id TEXT,
  model TEXT,
  summary TEXT NOT NULL,
  record BLOB NOT NULL,
  cache_map TEXT,
  PRIMARY KEY (request_id, seq)
);
CREATE INDEX IF NOT EXISTS decisions_account ON decisions (account_id, at);
CREATE INDEX IF NOT EXISTS decisions_at ON decisions (at);
CREATE INDEX IF NOT EXISTS decisions_key ON decisions (key_id, at);
CREATE INDEX IF NOT EXISTS decisions_pool ON decisions (pool_id, at);
CREATE INDEX IF NOT EXISTS decisions_session ON decisions (session_id, at);
CREATE TABLE IF NOT EXISTS events (
  at INTEGER NOT NULL,
  kind TEXT NOT NULL,
  account_id TEXT,
  key_id TEXT,
  session_id TEXT,
  model TEXT,
  actor TEXT,
  detail TEXT,
  pool_id TEXT,
  agent_id TEXT,
  request_id TEXT
);
CREATE INDEX IF NOT EXISTS events_account ON events (account_id, at);
CREATE INDEX IF NOT EXISTS events_at ON events (at);
CREATE INDEX IF NOT EXISTS events_kind_at ON events (kind, at);
CREATE INDEX IF NOT EXISTS events_request ON events (request_id);
CREATE INDEX IF NOT EXISTS events_session ON events (session_id, at);
CREATE TABLE IF NOT EXISTS quota_samples (
  account_id TEXT NOT NULL,
  bucket INTEGER NOT NULL,
  window TEXT NOT NULL,
  utilization REAL,
  reset_at INTEGER,
  source TEXT NOT NULL,
  observed_at INTEGER,
  PRIMARY KEY (account_id, bucket, window)
);
CREATE INDEX IF NOT EXISTS quota_samples_account ON quota_samples (account_id, window, bucket);
CREATE TABLE IF NOT EXISTS "extra_usage_samples" (
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
  spend_limit_minor INTEGER,
  spend_used_minor INTEGER,
  spend_percent REAL,
  spend_currency TEXT,
  spend_exponent INTEGER,
  PRIMARY KEY (account_id, bucket, source)
);
CREATE TABLE IF NOT EXISTS "rollups_hourly" (
  hour INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  key_generation INTEGER NOT NULL DEFAULT 0,
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  canonical_model TEXT NOT NULL,
  kind TEXT NOT NULL,
  user_agent TEXT NOT NULL,
  client TEXT NOT NULL DEFAULT '-',
  effort TEXT NOT NULL DEFAULT 'none',
  request_class TEXT NOT NULL DEFAULT '-',
  speed TEXT NOT NULL DEFAULT 'standard',
  funding TEXT NOT NULL DEFAULT 'plan',
  band TEXT NOT NULL DEFAULT 'standard',
  tier TEXT NOT NULL DEFAULT '-',
  classifier TEXT NOT NULL DEFAULT '-',
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
  thinking_tokens INTEGER NOT NULL DEFAULT 0,
  web_search_requests INTEGER NOT NULL DEFAULT 0,
  web_fetch_requests INTEGER NOT NULL DEFAULT 0,
  kept_warm_requests INTEGER NOT NULL,
  kept_warm_5m_tokens INTEGER NOT NULL,
  probe_migrated INTEGER NOT NULL,
  wasted_probes INTEGER NOT NULL,
  wasted_probe_reasons_json TEXT NOT NULL,
  probes_skipped INTEGER NOT NULL DEFAULT 0,
  wasted_input_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_cache_write_5m_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_output_tokens INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL,
  ttfc_hist BLOB NOT NULL,
  duration_hist BLOB NOT NULL,
  output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL,
  cache_miss_reasons_json TEXT NOT NULL,
  incomplete INTEGER NOT NULL DEFAULT 0,
  probes_held INTEGER NOT NULL DEFAULT 0,
  slow_down_held_ms INTEGER NOT NULL DEFAULT 0,
  slow_down_held INTEGER NOT NULL DEFAULT 0,
  kept_warm_1h_tokens INTEGER NOT NULL DEFAULT 0,
  refusals_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort,
    request_class, speed, funding, band, tier, classifier)
);
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);
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
  rewrite_tokens INTEGER NOT NULL DEFAULT 0,
  migrations_json TEXT NOT NULL DEFAULT '{}',
  wall_early_ms INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (hour, account_id)
);
CREATE TABLE IF NOT EXISTS cooldown_spans (
  account_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  hour INTEGER NOT NULL,
  until INTEGER NOT NULL,
  PRIMARY KEY (account_id, scope)
);
CREATE TABLE IF NOT EXISTS cooldown_segments (
  account_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  hour INTEGER NOT NULL,
  seconds INTEGER NOT NULL,
  PRIMARY KEY (account_id, scope, hour)
);
CREATE TABLE IF NOT EXISTS kept_warm_marks (
  request_id TEXT PRIMARY KEY,
  marked_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS parked_records (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  parked_at INTEGER NOT NULL,
  last_tried_at INTEGER NOT NULL,
  request_id TEXT NOT NULL,
  pool_id TEXT,
  account_id TEXT,
  record TEXT NOT NULL,
  error TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS request_guardrails (
  request_id TEXT NOT NULL,
  guardrail_id TEXT NOT NULL,
  position INTEGER NOT NULL,
  blocked INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  PRIMARY KEY (request_id, guardrail_id)
);
CREATE INDEX IF NOT EXISTS request_guardrails_received ON request_guardrails (received_at, guardrail_id, blocked);
CREATE TABLE IF NOT EXISTS routing_conditions (
  kind TEXT NOT NULL,
  subject_kind TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  pool_id TEXT,
  since INTEGER NOT NULL,
  count INTEGER NOT NULL,
  detail_json TEXT NOT NULL,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (kind, subject_kind, subject_id)
);
CREATE TABLE IF NOT EXISTS window_reconciliations (
  account_id TEXT NOT NULL,
  window TEXT NOT NULL,
  window_end INTEGER NOT NULL,
  first_reading_at INTEGER NOT NULL,
  last_reading_at INTEGER NOT NULL,
  observed_rise REAL NOT NULL,
  predicted_share REAL NOT NULL,
  ratio REAL NOT NULL,
  requests INTEGER NOT NULL,
  estimated INTEGER NOT NULL,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (account_id, window, window_end)
);
"#;

/// Applied in order above the stored `schema_version`, each inside one transaction. A migration may drop or rebuild a
/// table, so an older build refuses a newer ledger rather than misreading it.
pub const MIGRATIONS: &[(u32, &str)] = &[];
