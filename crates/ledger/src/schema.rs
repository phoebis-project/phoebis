//! The ledger schema at version 1: `account_rollups_hourly`, `attempts`, `bodies`, `cooldown_segments`,
//! `cooldown_spans`, `decisions`, `events`, `extra_usage_samples`, `kept_warm_marks`, `parked_records`,
//! `quota_samples`, `request_guardrails`, `requests`, `rollups_hourly`, `routing_conditions` and
//! `window_reconciliations`, plus `meta` for the schema marker. `MIGRATIONS` steps it to version 6.

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- key_generation keeps the rows of a key created again under a deleted id from summing with its predecessor's.
-- presented_retired and rotation_at scope the count of sessions still on a previous secret to the rotation whose grace
-- is running. response_truncated sits on the row rather than being recomputed from bodies, since a capture cut by a
-- chunk lost after the last one that arrived never raises response_len_seen. Every open runs this baseline, so it keeps
-- request_truncated, which no insert writes, and the migration to version 4 drops it from a fresh file as from a
-- stamped one. held keeps the holds a request waited through, since a hold that ends in an exhaustion dispatches
-- nothing an attempt row could carry, and client_drift keeps a refused request's drift kind for the same reason.
-- guardrail_id names the guardrail that blocked the request, null on most rows. landed_at is the instant the ledger
-- wrote the row, strictly increasing across the writer's rows, so a poll that passes back its newest stamp reads
-- exactly the rows written since. refused flags a refusal the router issued before it asked any account, for the
-- client's or a policy's cause.
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
-- A list read filters on the landing stamp.
CREATE INDEX IF NOT EXISTS requests_landed_at ON requests (landed_at);
CREATE INDEX IF NOT EXISTS requests_received ON requests (received_at);
CREATE INDEX IF NOT EXISTS requests_session ON requests (session_id, received_at);
-- An attempt keeps counts and no dollar figure, since a reader prices them at the revision in force when the request
-- arrived. tier is the tier the router prices the serving account by at the landing, usage_based its class, so a
-- usage-based seat's attempts file under a funding of their own, and upstream_message_id joins a client transcript.
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
-- request_len_seen and response_len_seen record the length each side ran to, so a cut body says how much was dropped.
CREATE TABLE IF NOT EXISTS bodies (
  request_id TEXT PRIMARY KEY,
  request_body BLOB NOT NULL,
  response_body BLOB,
  expires_at INTEGER NOT NULL,
  request_len_seen INTEGER NOT NULL DEFAULT 0,
  response_len_seen INTEGER NOT NULL DEFAULT 0
);
-- The retention purge cuts captured bodies in slices of the oldest expiries.
CREATE INDEX IF NOT EXISTS bodies_expires ON bodies (expires_at);
-- Every routing decision is its own row beside the attempt rows, one compressed record per decision keyed by the
-- request and the decision's place in it.
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
-- pool_id, agent_id and request_id are columns with their indexes, so a merged list filters events and decisions by the
-- same keys without reading detail.
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
-- The newest events of one kind seek through this index rather than walking events_at.
CREATE INDEX IF NOT EXISTS events_kind_at ON events (kind, at);
CREATE INDEX IF NOT EXISTS events_request ON events (request_id);
CREATE INDEX IF NOT EXISTS events_session ON events (session_id, at);
-- observed_at is the instant a reading was observed, kept beside its bucket so a reconciliation compares the rise
-- between two readings with the requests completed between those same instants.
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
-- Keyed by the source too, so a head and a poll of one five-minute bucket both stand. utilization is a fraction
-- whichever source wrote it, the five spend columns hold the poll's spend object, and the table keeps its rowid, since
-- the purge deletes by rowid.
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
-- An hourly row keeps counts and no dollar figure, since a reader prices them at the revision in force at the row's
-- hour. Every dimension a reader groups or prices by is a key part: key_generation keeps a key created again under a
-- deleted id apart from its predecessor, funding lets a subscription member's budget bound its metered spend alone,
-- band and tier name the cells and windows the counts price at, and classifier files a classifier call's usage apart,
-- so the share readers price it at no plan share. probes_skipped counts the due probes a scheduling gate withheld,
-- incomplete the committed answers whose body ended early, the two kept-warm token columns what a kept-warm credit read
-- under each TTL, the slow-down columns the waits of the row's requests, client_canceled the requests whose client left
-- (a column of the migration to version 4), in requests and apart from failed and errors_json,
-- input_transformations_json (a column of the same migration) the requests per key of the blocks their answer dropped,
-- once per key whatever the blocks, beside the reserved keys moved, none and reported, uncacheable (a column of the
-- same migration) the uncached requests that are no token count and whose body marked no cache, a part of
-- uncached_requests the read adds a token count's row to, and refusals_json the refusals by error code apart from every
-- other figure, so a reader that sums the row sees none of them. Every open runs this baseline, so it keeps
-- probe_migrated, which no build wrote above zero, and the migration to version 6 drops it from a fresh file as from a
-- stamped one.
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
-- A key holder reads its own usage, one key's hours, without walking every key's entries.
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
-- Speeds a budgeted member's spend read.
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);
-- wall_early_ms is the longest time a wall on the account arrived before its window's reset under a stretch round, so
-- the figure outlives the trailing day the wall_reached condition covers.
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
-- Cooldown time folds as an interval: the fold needs the end already counted for a scope to add only what an extension
-- moves it by, and this table keeps the account's accounted end alone.
CREATE TABLE IF NOT EXISTS cooldown_spans (
  account_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  hour INTEGER NOT NULL,
  until INTEGER NOT NULL,
  PRIMARY KEY (account_id, scope)
);
-- A cooldown's seconds are booked to the hour each part elapses in, and this table keeps each hour's booked
-- contribution for a clear to read back.
CREATE TABLE IF NOT EXISTS cooldown_segments (
  account_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  hour INTEGER NOT NULL,
  seconds INTEGER NOT NULL,
  PRIMARY KEY (account_id, scope, hour)
);
-- A kept-warm credit leaves a mark for a request row that has not landed yet, since the engine credits at completion
-- while the writer batches.
CREATE TABLE IF NOT EXISTS kept_warm_marks (
  request_id TEXT PRIMARY KEY,
  marked_at INTEGER NOT NULL
);
-- A priced record the writer could not land keeps its raw facts, the error that refused it, its pool and account and
-- when it was parked and last tried, so the next start can try it again rather than lose the money it carries.
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
-- One row per guardrail that matched a request, the request's instant copied so the index answers a range without a
-- join.
CREATE TABLE IF NOT EXISTS request_guardrails (
  request_id TEXT NOT NULL,
  guardrail_id TEXT NOT NULL,
  position INTEGER NOT NULL,
  blocked INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  PRIMARY KEY (request_id, guardrail_id)
);
CREATE INDEX IF NOT EXISTS request_guardrails_received ON request_guardrails (received_at, guardrail_id, blocked);
-- The routing conditions an hourly fold reads off the trailing day of events, one row per kind and subject, so a scrape
-- or a console poll reads a lookup rather than scanning the events. The fold writes the table whole, so it holds what
-- the last fold found.
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
-- One row per completed 5-hour, 7-day and Fable window of an account: the rise its readings showed against the share
-- the router priced for its own plan-funded attempts between those readings. The hourly fold reads the newest of each
-- window for the meter's drift. Every open runs this baseline, so it keeps the `estimated` count the migration to
-- version 2 drops, which a fresh file then loses as a stamped one does.
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

/// Step 3 carries the fallback credit's digests and the serving attempt's facts onto the rows, so the request list
/// reads a column where it ran a subquery per row. A row written before the step reads null until the backfill reaches
/// it, in a start's slice or in a pass of the purge task. The partial index holds the rows with attempts whose serving
/// account is null, so a slice seeks the newest of them without reading the filled rows, and a request no attempt
/// served, whose account stays null, never enters it. The rare row whose trail lost every attempt stays in it, and the
/// backfill's reads skip it.
const STEP_3: &str = "ALTER TABLE attempts ADD COLUMN issued_digest TEXT; \
    ALTER TABLE requests ADD COLUMN fallback_digest TEXT; \
    ALTER TABLE requests ADD COLUMN serving_account_id TEXT; \
    ALTER TABLE requests ADD COLUMN reached_upstream INTEGER; \
    ALTER TABLE requests ADD COLUMN outcome_class TEXT; \
    ALTER TABLE requests ADD COLUMN speed TEXT; \
    ALTER TABLE requests ADD COLUMN cache_miss_reason TEXT; \
    ALTER TABLE requests ADD COLUMN stop_reason TEXT; \
    ALTER TABLE requests ADD COLUMN refusal_category TEXT; \
    ALTER TABLE requests ADD COLUMN input_tokens INTEGER; \
    ALTER TABLE requests ADD COLUMN output_tokens INTEGER; \
    ALTER TABLE requests ADD COLUMN cache_read_tokens INTEGER; \
    ALTER TABLE requests ADD COLUMN cache_write_tokens INTEGER; \
    CREATE INDEX requests_serving_pending ON requests (received_at) \
        WHERE serving_account_id IS NULL AND attempts > 0";

/// Step 4 drops `requests.request_truncated`, a column no insert wrote and no read took. The two
/// `input_transformations` columns hold the blocks an answer dropped, per attempt and for the serving attempt on the
/// request row. `events.from_account_id` holds the account a pin move left, filled on the stored moves from their
/// detail, and a stored pin or request event with no pool takes its request's pool or the pool of its key's newest
/// decision at or before it. `rollups_hourly.client_canceled` counts a client's cancels apart from `failed`, and each
/// stored row that is no probe's moves its errors map's `client_gone` count there, off `failed` and out of the map. The
/// stored account hours that hold nothing are deleted, judged by the eleven columns `AccountRollupRow::holds_nothing`
/// reads, so the rows a cleared cooldown emptied before the step leave as `unfold_account` deletes them after it.
/// `rollups_hourly.input_transformations_json` counts the requests per key of the blocks their answer dropped, and a
/// stored hour reads the empty map, since no attempt stored before the step carries the member.
/// `attempts.previous_input_tokens` holds the size the selection handed on, `requests.cache_marker` the selection's
/// marker fact and `rollups_hourly.uncacheable` the uncached requests whose body marked no cache, each with no fill,
/// since no stored row holds the fact. `requests.thinking_diagnostics_source` holds who sent the thinking-binding beta,
/// null on a row stored before the step. `session_sums` holds each session's request and token counts under every part
/// a price reads and `session_facts` the picks no sum carries, both left empty for the purge task's fill, with
/// `session_sums_received_hour` for the purge's cut and a moved threshold.
const STEP_4: &str = "ALTER TABLE requests DROP COLUMN request_truncated; \
    ALTER TABLE attempts ADD COLUMN input_transformations TEXT; \
    ALTER TABLE requests ADD COLUMN input_transformations TEXT; \
    ALTER TABLE events ADD COLUMN from_account_id TEXT; \
    UPDATE events SET from_account_id = json_extract(detail, '$.from_account') WHERE kind = 'pin_migrated'; \
    UPDATE events SET pool_id = (SELECT r.pool_id FROM requests r WHERE r.id = events.request_id) \
        WHERE pool_id IS NULL AND request_id IS NOT NULL \
        AND kind IN ('model_permitted', 'model_rejected', 'stream_interrupted'); \
    UPDATE events SET pool_id = (SELECT d.pool_id FROM decisions d \
        WHERE d.key_id = events.key_id AND d.at <= events.at AND d.pool_id IS NOT NULL ORDER BY d.at DESC LIMIT 1) \
        WHERE pool_id IS NULL AND key_id IS NOT NULL \
        AND kind IN ('mark_cleared', 'pin_created', 'pin_expired', 'pin_migrated', 'warm_prefix_expired'); \
    ALTER TABLE rollups_hourly ADD COLUMN client_canceled INTEGER NOT NULL DEFAULT 0; \
    UPDATE rollups_hourly \
        SET client_canceled = json_extract(errors_json, '$.client_gone'), \
            failed = MAX(failed - json_extract(errors_json, '$.client_gone'), 0), \
            errors_json = json_remove(errors_json, '$.client_gone') \
        WHERE kind <> 'probe' AND json_extract(errors_json, '$.client_gone') IS NOT NULL; \
    DELETE FROM account_rollups_hourly \
        WHERE utilization_5h_max IS NULL AND utilization_7d_max IS NULL AND utilization_7d_oi_max IS NULL \
        AND overage_seconds = 0 AND cooldown_seconds = 0 AND pins_created = 0 AND pins_migrated_in = 0 \
        AND pins_migrated_out = 0 AND rewrite_tokens = 0 AND wall_early_ms = 0 AND migrations_json = '{}'; \
    ALTER TABLE rollups_hourly ADD COLUMN input_transformations_json TEXT NOT NULL DEFAULT '{}'; \
    ALTER TABLE attempts ADD COLUMN previous_input_tokens INTEGER; \
    ALTER TABLE requests ADD COLUMN cache_marker INTEGER; \
    ALTER TABLE rollups_hourly ADD COLUMN uncacheable INTEGER NOT NULL DEFAULT 0; \
    ALTER TABLE requests ADD COLUMN thinking_diagnostics_source TEXT; \
    CREATE TABLE session_sums ( \
        key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, \
        probe INTEGER NOT NULL, account_id TEXT NOT NULL, canonical_model TEXT NOT NULL, speed TEXT NOT NULL, \
        funding TEXT NOT NULL, band TEXT NOT NULL, tier TEXT NOT NULL, client TEXT NOT NULL, classifier TEXT NOT NULL, \
        request_class TEXT NOT NULL, received_hour INTEGER NOT NULL, completed_hour INTEGER NOT NULL, \
        requests INTEGER NOT NULL, input_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL, \
        cache_write_5m_tokens INTEGER NOT NULL, cache_write_1h_tokens INTEGER NOT NULL, \
        output_tokens INTEGER NOT NULL, \
        PRIMARY KEY (key_id, key_generation, session_id, agent_id, probe, account_id, canonical_model, speed, funding, \
                     band, tier, client, classifier, request_class, received_hour, completed_hour) \
    ) WITHOUT ROWID; \
    CREATE TABLE session_facts ( \
        key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, \
        last_request_at INTEGER, last_inference_at INTEGER, served_at INTEGER, served_id TEXT, user_agent TEXT, \
        first_request_at INTEGER, newest_at INTEGER, newest_id TEXT, newest_model TEXT, newest_agent_type TEXT, \
        newest_account_id TEXT, probe_at INTEGER, probe_id TEXT, probe_model TEXT, \
        PRIMARY KEY (key_id, key_generation, session_id, agent_id) \
    ) WITHOUT ROWID; \
    CREATE INDEX session_sums_received_hour ON session_sums (received_hour)";

/// Step 5 adds the feature calls, one row per call sent upstream under an account. The list reads newest first with the
/// id breaking a tie, so the first index serves its order and its cursor, and the second a key's calls.
const STEP_5: &str = "CREATE TABLE feature_calls (
  id TEXT PRIMARY KEY,
  received_at INTEGER NOT NULL,
  key_id TEXT NOT NULL,
  feature TEXT NOT NULL,
  account_id TEXT NOT NULL,
  method TEXT NOT NULL,
  path TEXT NOT NULL,
  status INTEGER,
  outcome TEXT NOT NULL,
  error_code TEXT,
  duration_ms INTEGER NOT NULL
);
CREATE INDEX feature_calls_received ON feature_calls (received_at, id);
CREATE INDEX feature_calls_key ON feature_calls (key_id, received_at);";

/// Step 6 drops the migrated probe count of an hour, which no build wrote above zero. `attempts.entry_expired` and
/// `attempts.agent_released` hold the two facts the selection hands on beside the size, null on an attempt stored
/// before the step. A session's own facts row gains the members of its newest client request, filled from its request
/// rows, and an index on its newest receipt, which the Sessions poll seeks. The cursor of the session totals' fill
/// leaves, so the first start walks the retained requests from the top and marks each session that holds no mark.
const STEP_6: &str = "\
    ALTER TABLE rollups_hourly DROP COLUMN probe_migrated; \
    ALTER TABLE attempts ADD COLUMN entry_expired INTEGER; \
    ALTER TABLE attempts ADD COLUMN agent_released INTEGER; \
    ALTER TABLE session_facts ADD COLUMN last_request_id TEXT; \
    ALTER TABLE session_facts ADD COLUMN last_request_model TEXT; \
    ALTER TABLE session_facts ADD COLUMN last_request_account_id TEXT; \
    ALTER TABLE session_facts ADD COLUMN last_request_user_agent TEXT; \
    ALTER TABLE session_facts ADD COLUMN last_request_retired INTEGER; \
    ALTER TABLE session_facts ADD COLUMN last_request_rotation_at INTEGER; \
    CREATE INDEX session_facts_last_request ON session_facts (last_request_at); \
    UPDATE session_facts SET (last_request_id, last_request_model, last_request_account_id, \
        last_request_user_agent, last_request_retired, last_request_rotation_at) = \
        (SELECT r.id, r.canonical_model, \
            (SELECT a.account_id FROM attempts a WHERE a.request_id = r.id ORDER BY a.attempt DESC LIMIT 1), \
            r.user_agent, r.presented_retired, r.rotation_at \
         FROM requests r INDEXED BY requests_session \
         WHERE r.session_id = session_facts.session_id AND r.key_id = session_facts.key_id \
         AND r.key_generation = session_facts.key_generation AND r.kind <> 'probe' \
         ORDER BY r.received_at DESC, r.id DESC LIMIT 1) \
        WHERE agent_id = ''; \
    DELETE FROM meta WHERE key = 'session_totals_fill'";

/// Applied in order above the stored `schema_version`, each inside one transaction. A migration may drop or rebuild a
/// table, so an older build refuses a newer ledger rather than misreading it.
pub const MIGRATIONS: &[(u32, &str)] = &[
    (
        2,
        "ALTER TABLE window_reconciliations DROP COLUMN estimated",
    ),
    (3, STEP_3),
    (4, STEP_4),
    (5, STEP_5),
    (6, STEP_6),
];
