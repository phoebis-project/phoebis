//! The ledger schema: `requests`, `attempts`, `bodies`, `quota_samples`, `extra_usage_samples`,
//! `rollups_hourly`, `account_rollups_hourly`, `cooldown_spans`, `cooldown_segments`, `kept_warm_marks`,
//! `events`, and `window_reconciliations`, plus `meta` for the schema marker.

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
  probes_skipped INTEGER NOT NULL DEFAULT 0,
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
CREATE TABLE IF NOT EXISTS kept_warm_marks (request_id TEXT PRIMARY KEY, marked_at INTEGER NOT NULL);
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
/// about the request rather than something the profile implies, and the analytics group by it. The rollup key
/// gains the agent, which SQLite cannot do to a primary key in place: the table is rebuilt, and every row
/// folded before the column existed takes the reserved id.
///
/// Version 4: cooldown time is an interval, not a sum of announcements. The fold needs the end already
/// counted for a scope, to add only what an extension moves it by.
///
/// Version 5: the due probes a scheduling gate withheld are a count of their own, and a kept-warm credit
/// leaves a mark for a request row that has not landed yet, since the engine credits at completion while the
/// writer batches.
///
/// Version 6: a key holder reads its own usage, which asks for one key's hours. The rollup key starts with
/// the hour, so that read walked every key's entries for the week and held the shared reader for as long as
/// everyone else's history was; an index that starts with the key narrows it to the rows the answer sums.
///
/// Version 7: a key's generation joins the rollup key and the request row, so a deleted key's id can be
/// created again and the new key's rows never sum with the earlier holder's; SQLite cannot alter a primary
/// key, so the table is rebuilt as version 3 rebuilt it, and the index of version 6 is rebuilt with the
/// generation between the key and the hour.
///
/// Version 8: the attempt row records the thinking and server-tool counts and what the answer said about
/// itself, and the request row records the request's shape; every column is nullable or defaults to zero, and
/// a row from before this version reads NULL or 0.
///
/// Version 9: three token sums and the request's effort join the hourly rollup; effort is a dimension, so it
/// joins the primary key and the table is rebuilt as versions 3 and 7 rebuilt it, every existing row filed
/// under `none`.
///
/// Version 10: a captured body records the length each side ran to, so a cut body says how much was dropped.
///
/// Version 11: a request says whether it presented a retired secret and under which rotation, so the count of
/// sessions still on the previous secret is scoped to the rotation whose grace is running.
///
/// Version 12: a request refused for client drift keeps the kind on its own row, not only on the alert and the
/// attempt the refusal never reached.
///
/// Version 13: a request's truncation is stored on its own row rather than recomputed from the bodies table,
/// since a capture cut by a chunk lost after the last one that arrived never raises `response_len_seen` past
/// what the row already stored, leaving the recompute no way to see the cut. No backfill: a row written before
/// this version reads the column's default, `0`, even when its bodies row is in fact short.
///
/// Version 14: a cooldown's seconds are booked to the hour each part elapses in, not to the hour an
/// announcement was recorded, so an extension crossing an hour boundary and a clear that lands mid-hour each
/// affect only the hours they actually touch. `cooldown_segments` keeps each hour's own booked contribution
/// for a clear to read back; `cooldown_spans` keeps the account's currently accounted end alone. No backfill:
/// a cooldown set before this version has no segments of its own, so a clear that lands after it takes back
/// none of the seconds already booked under the old rule.
///
/// Version 16: the request row records the client's class, its gateway hints, the summed tool durations, and
/// the turn origin, every column nullable so a row from before this version reads NULL; the client is a
/// dimension of the hourly rollup, so it joins the primary key beside the agent and the table is rebuilt as
/// versions 3, 7, and 9 rebuilt it, every existing row filed under the reserved id.
///
/// Version 17: the newest events of one kind are read on every poll of the Models view, and `events_at` made
/// that a walk of the whole history whenever the kind had fewer rows than the limit; an index that starts with
/// the kind seeks straight to them.
///
/// Version 18: the request row records the class the client named, nullable; the class is a dimension of the
/// hourly rollup, so it joins the primary key after the effort and the table is rebuilt as version 16 rebuilt
/// it, every existing row filed under `-`; the account rollup gains the rewrite tokens and the moves by reason;
/// an attempt keeps its selection trace.
///
/// Version 19: the request row records how the climb identified it, the conversation's root and, for a
/// continuation, the matched depth, and for a session that branched from another conversation, that
/// conversation's root and depth; a `(pool_id, account_id, hour)` index speeds a budgeted member's spend read.
///
/// Version 20: the instant a reading was observed, kept beside its bucket so a reconciliation compares the rise
/// between two readings with the requests completed between those same instants; and one row per completed
/// window of a subscription account: the rise its readings showed against the share the router predicted for
/// its own requests in it, resets excluded by construction (a run names one reset). A reading from before this
/// version reads NULL and bounds no run.
pub const MIGRATIONS: &[(u32, &str)] = &[
    (
        2,
        "ALTER TABLE attempts ADD COLUMN input_usd REAL;
ALTER TABLE attempts ADD COLUMN cache_read_usd REAL;
ALTER TABLE attempts ADD COLUMN cache_write_usd REAL;
ALTER TABLE attempts ADD COLUMN output_usd REAL;",
    ),
    (
        3,
        "ALTER TABLE requests ADD COLUMN user_agent TEXT;
CREATE TABLE rollups_hourly_next (
  hour INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  canonical_model TEXT NOT NULL,
  kind TEXT NOT NULL,
  user_agent TEXT NOT NULL,
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
  PRIMARY KEY (hour, key_id, pool_id, account_id, canonical_model, kind, user_agent)
);
INSERT INTO rollups_hourly_next SELECT hour, key_id, pool_id, account_id, canonical_model, kind, '-', requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;",
    ),
    (
        4,
        "CREATE TABLE IF NOT EXISTS cooldown_spans (
  account_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  hour INTEGER NOT NULL,
  until INTEGER NOT NULL,
  PRIMARY KEY (account_id, scope)
);",
    ),
    (
        5,
        "ALTER TABLE rollups_hourly ADD COLUMN probes_skipped INTEGER NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS kept_warm_marks (request_id TEXT PRIMARY KEY, marked_at INTEGER NOT NULL);",
    ),
    (
        6,
        "CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, hour);",
    ),
    (
        7,
        "ALTER TABLE requests ADD COLUMN key_generation INTEGER NOT NULL DEFAULT 0;
CREATE TABLE rollups_hourly_next (
  hour INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  key_generation INTEGER NOT NULL DEFAULT 0,
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  canonical_model TEXT NOT NULL,
  kind TEXT NOT NULL,
  user_agent TEXT NOT NULL,
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
  probes_skipped INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL,
  ttfc_hist BLOB NOT NULL,
  duration_hist BLOB NOT NULL,
  output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL,
  cache_miss_reasons_json TEXT NOT NULL,
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json) SELECT hour, key_id, 0, pool_id, account_id, canonical_model, kind, user_agent, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);",
    ),
    (
        8,
        "ALTER TABLE attempts ADD COLUMN thinking_tokens INTEGER;
ALTER TABLE attempts ADD COLUMN web_search_requests INTEGER;
ALTER TABLE attempts ADD COLUMN web_fetch_requests INTEGER;
ALTER TABLE attempts ADD COLUMN stop_reason TEXT;
ALTER TABLE attempts ADD COLUMN stop_sequence TEXT;
ALTER TABLE attempts ADD COLUMN served_model TEXT;
ALTER TABLE attempts ADD COLUMN speed TEXT;
ALTER TABLE attempts ADD COLUMN inference_geo TEXT;
ALTER TABLE attempts ADD COLUMN cleared_input_tokens INTEGER;
ALTER TABLE attempts ADD COLUMN refusal_category TEXT;
ALTER TABLE attempts ADD COLUMN refusal_recommended_model TEXT;
ALTER TABLE requests ADD COLUMN effort TEXT;
ALTER TABLE requests ADD COLUMN thinking_type TEXT;
ALTER TABLE requests ADD COLUMN thinking_budget INTEGER;
ALTER TABLE requests ADD COLUMN max_tokens INTEGER;
ALTER TABLE requests ADD COLUMN tool_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE requests ADD COLUMN system_blocks INTEGER NOT NULL DEFAULT 0;
ALTER TABLE requests ADD COLUMN message_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE requests ADD COLUMN context_edits TEXT NOT NULL DEFAULT '';
ALTER TABLE requests ADD COLUMN temperature REAL;
ALTER TABLE requests ADD COLUMN requested_speed TEXT;
ALTER TABLE requests ADD COLUMN requested_service_tier TEXT;",
    ),
    (
        9,
        "CREATE TABLE rollups_hourly_next (
  hour INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  key_generation INTEGER NOT NULL DEFAULT 0,
  pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  canonical_model TEXT NOT NULL,
  kind TEXT NOT NULL,
  user_agent TEXT NOT NULL,
  effort TEXT NOT NULL DEFAULT 'none',
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
  probes_skipped INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL,
  ttfc_hist BLOB NOT NULL,
  duration_hist BLOB NOT NULL,
  output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL,
  cache_miss_reasons_json TEXT NOT NULL,
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, effort)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, effort, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json) SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, 'none', requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, 0, 0, 0, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);",
    ),
    (
        10,
        "ALTER TABLE bodies ADD COLUMN request_len_seen INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bodies ADD COLUMN response_len_seen INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        11,
        "ALTER TABLE requests ADD COLUMN presented_retired INTEGER NOT NULL DEFAULT 0;
ALTER TABLE requests ADD COLUMN rotation_at INTEGER;",
    ),
    (
        12,
        "ALTER TABLE requests ADD COLUMN client_drift TEXT;",
    ),
    (
        13,
        "ALTER TABLE requests ADD COLUMN request_truncated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE requests ADD COLUMN response_truncated INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        14,
        "CREATE TABLE IF NOT EXISTS cooldown_segments (account_id TEXT NOT NULL, scope TEXT NOT NULL, hour INTEGER NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (account_id, scope, hour));",
    ),
    (15, ""),
    (
        16,
        "ALTER TABLE requests ADD COLUMN client TEXT;
ALTER TABLE requests ADD COLUMN agent_type TEXT;
ALTER TABLE requests ADD COLUMN compaction TEXT;
ALTER TABLE requests ADD COLUMN context_compacted TEXT;
ALTER TABLE requests ADD COLUMN prev_tool_ms INTEGER;
ALTER TABLE requests ADD COLUMN turn_origin TEXT;
CREATE TABLE rollups_hourly_next (
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
  probes_skipped INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL,
  ttfc_hist BLOB NOT NULL,
  duration_hist BLOB NOT NULL,
  output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL,
  cache_miss_reasons_json TEXT NOT NULL,
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json) SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, '-', effort, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);",
    ),
    (
        17,
        "CREATE INDEX IF NOT EXISTS events_kind_at ON events (kind, at);",
    ),
    (
        18,
        "ALTER TABLE requests ADD COLUMN request_class TEXT;
CREATE TABLE rollups_hourly_next (
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
  probes_skipped INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL,
  ttfc_hist BLOB NOT NULL,
  duration_hist BLOB NOT NULL,
  output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL,
  cache_miss_reasons_json TEXT NOT NULL,
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json) SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, '-', requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
ALTER TABLE account_rollups_hourly ADD COLUMN rewrite_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE account_rollups_hourly ADD COLUMN migrations_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE attempts ADD COLUMN selection TEXT;",
    ),
    (
        19,
        "ALTER TABLE requests ADD COLUMN identified_by TEXT;
ALTER TABLE requests ADD COLUMN identity_root TEXT;
ALTER TABLE requests ADD COLUMN identity_depth INTEGER;
ALTER TABLE requests ADD COLUMN forked_from TEXT;
ALTER TABLE requests ADD COLUMN fork_depth INTEGER;
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);",
    ),
    (
        20,
        "ALTER TABLE quota_samples ADD COLUMN observed_at INTEGER;
CREATE TABLE window_reconciliations (
  account_id TEXT NOT NULL, window TEXT NOT NULL, window_end INTEGER NOT NULL,
  first_reading_at INTEGER NOT NULL, last_reading_at INTEGER NOT NULL,
  observed_rise REAL NOT NULL, predicted_share REAL NOT NULL, ratio REAL NOT NULL,
  requests INTEGER NOT NULL, unmeasured INTEGER NOT NULL, computed_at INTEGER NOT NULL,
  PRIMARY KEY (account_id, window, window_end)
);
CREATE INDEX window_reconciliations_computed ON window_reconciliations (computed_at, account_id);",
    ),
];
