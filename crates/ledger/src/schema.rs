//! The ledger schema: `requests`, `attempts`, `bodies`, `quota_samples`, `extra_usage_samples`,
//! `rollups_hourly`, `account_rollups_hourly`, `cooldown_spans`, `cooldown_segments`, `kept_warm_marks`, `events`,
//! and `parked_records`, plus `meta` for the schema marker. `SCHEMA` is version 1; `MIGRATIONS` builds the rest.

/// Frozen: the fresh path is the upgrade path from version 1. A column folded in here and added by a migration would
/// fail the migration with a duplicate column on a fresh file, one dropped by a migration would fail it with a missing
/// one, and two DDL sources would have to stay aligned.
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
///
/// Version 21: the request row records its auto-mode standing (as admission read it: a server-side check asked for
/// under an on setting, the `safeguards` member carried under an off setting, or a refusal for the body's shape
/// before any dispatch); a row from before reads NULL.
///
/// Version 22: the speed the answer reported is a dimension of the hourly rollup, so it joins the primary key after
/// the class and the table is rebuilt as version 18 rebuilt it, every existing row filed under `standard`; the two
/// indexes are recreated on the new table.
///
/// Version 23: the per-window reconciliation's table leaves; the readings keep `observed_at`.
///
/// Version 24: who paid for an attempt is a dimension of the member's spend: a subscription member's budget bounds
/// its metered spend alone, its overage and its fast answers, and a sum that cannot tell plan-funded rows from
/// metered ones would count the plan's traffic against it. `funding` joins the primary key after the speed and the
/// table is rebuilt as version 22 rebuilt it; a row from before files under `fast` when its speed says fast and
/// under `plan` otherwise, since the overage reading was never kept on a rollup row.
///
/// Version 25: the holds a request waited through, kept on the request itself: a hold that ends in an exhaustion
/// dispatches nothing, so no attempt row could carry it. A row from before reads NULL.
///
/// Version 26: a committed answer whose body ended before its end counts in its own rollup column, so a cut is
/// neither a success nor a failure in the charts; and the attempt row carries the `id` of the message the answer
/// named, the join to a client transcript. A row from before reads 0 and NULL.
///
/// Version 27: the retention purge cuts captured bodies in slices of the oldest expiries, which without an index
/// on `expires_at` read the whole table for every slice; and a priced record the writer could not land keeps its
/// raw facts, the error that refused it, its pool and account and when it was parked and last tried, so the next
/// start can try it again rather than lose the money it carries.
///
/// Version 29: every routing decision is its own row beside the attempt rows, one compressed record per decision keyed
/// by the request and the decision's place in it. The events gain the pool, the agent and the request as columns
/// with their indexes, so a merged list filters both tables by the same keys without reading `detail`.
///
/// Version 32: the request row records who put the cache diagnostics member on the body the upstream compared, the
/// client or the router, so the drawer tells a client's own comparison from the relay's insertion. A row from before
/// reads NULL.
///
/// Version 33: an attempt keeps its counts and no dollar figure, since a reader prices the counts at the revision in
/// force when the request arrived, so the five dollar columns and the stamp of the table that priced them leave. The
/// attempt records the serving account's tier as the registry held it at the landing. SQLite drops a column in place,
/// so the table is not rebuilt. No backfill: a row from before reads a NULL tier.
///
/// Version 34: an hourly rollup row keeps counts and no dollar figure, since a reader prices the counts at the
/// revision in force at the row's hour, so the eight dollar columns leave and a wasted probe keeps its five token
/// counts in place of its dollars. The prompt-length band the row's attempts priced at and the serving account's tier
/// join the primary key after the funding, so a row's counts price at the cells and the windows its key names. The
/// table is rebuilt as version 24 rebuilt it. No backfill: a row from before files under `standard` and `-` with no
/// wasted count and prices from its own counts.
///
/// Version 35: the routing conditions an hourly fold reads off the trailing day of events, one row per kind and
/// subject, so a scrape or a console poll reads a lookup rather than scanning the events. The fold writes the table
/// whole, so it holds what the last fold found and nothing older.
///
/// Version 36: an hourly rollup row counts the slow-down's waits of its requests, the milliseconds summed and the
/// requests that waited, read off each request's `held` waits under `slow_down`, so the held time reads per bucket by
/// pool and account without the request rows. No backfill: a row from before reads 0.
///
/// Version 37: a kept-warm credit counts the tokens it read under the TTL its request's observation settled on, so the
/// one count becomes the five-minute count in place and the hour count joins at zero. A row from before keeps its
/// whole count under five minutes, since its split was never recorded. The table is not rebuilt.
///
/// Version 38: one row per completed 5-hour, 7-day and Fable window of an account, the rise its readings showed against
/// the share the router priced for its own plan-funded attempts between those readings, each attempt under its own
/// regime, and how many of them priced at a model tier's estimate. The hourly fold writes the rows and reads the
/// newest of each window for the meter's drift, so a cost model whose figures stray from the upstream's readings is
/// seen. The table starts empty, and the first fold reconciles every completed window of the trailing 30 days whose
/// readings and requests the ledger still holds.
///
/// Version 39: the guardrail that blocked a request on its row and one child row per guardrail that matched it, with
/// the request's instant copied so the index answers the range without a join. Most rows carry neither, so the column
/// is nullable and the table starts empty.
///
/// Version 40: the client's classifier standing and the contract item on the request row, both nullable since most
/// rows carry neither, and the classifier as the sixteenth key part of the hourly rollups after the tier, so a
/// classifier call's usage files on rows of its own and the share readers price it at no plan share. The rollup
/// table is rebuilt once with `-` in the new position, as the band and the tier were added.
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
    (
        21,
        "ALTER TABLE requests ADD COLUMN auto_mode TEXT;",
    ),
    (
        22,
        "CREATE TABLE rollups_hourly_next (
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
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json) SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, 'standard', requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);",
    ),
    (23, "DROP TABLE window_reconciliations;"),
    (
        24,
        "CREATE TABLE rollups_hourly_next (
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
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json) SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, CASE WHEN speed = 'fast' THEN 'fast' ELSE 'plan' END, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);",
    ),
    (25, "ALTER TABLE requests ADD COLUMN held TEXT;"),
    (
        26,
        "ALTER TABLE rollups_hourly ADD COLUMN incomplete INTEGER NOT NULL DEFAULT 0;
ALTER TABLE attempts ADD COLUMN upstream_message_id TEXT;",
    ),
    (
        27,
        "CREATE INDEX IF NOT EXISTS bodies_expires ON bodies (expires_at);
CREATE TABLE parked_records (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  parked_at INTEGER NOT NULL,
  last_tried_at INTEGER NOT NULL,
  request_id TEXT NOT NULL,
  pool_id TEXT,
  account_id TEXT,
  record TEXT NOT NULL,
  error TEXT NOT NULL
);",
    ),
    // Before 28 the head wrote `5h`, `7d` and `7d_oi` and the poll the endpoint's keys plus `fable`. A poll row
    // whose old-spelled partner holds the same account, bucket and window goes first, so the rename meets no
    // primary key twice: the head's row wins as "head replaces" would have had it, and the `limits` reading
    // wins the Fable week.
    (
        28,
        "DELETE FROM quota_samples WHERE source = 'poll'
  AND window IN ('five_hour', 'seven_day', 'seven_day_overage_included', 'seven_day_fable') AND EXISTS (
  SELECT 1 FROM quota_samples o WHERE o.account_id = quota_samples.account_id AND o.bucket = quota_samples.bucket
  AND o.window = CASE quota_samples.window WHEN 'five_hour' THEN '5h' WHEN 'seven_day' THEN '7d'
    WHEN 'seven_day_overage_included' THEN '7d_oi' ELSE 'fable' END);
UPDATE quota_samples SET window = CASE window WHEN '5h' THEN 'five_hour' WHEN '7d' THEN 'seven_day'
  WHEN '7d_oi' THEN 'seven_day_overage_included' WHEN 'fable' THEN 'seven_day_fable' ELSE window END;",
    ),
    (
        29,
        "CREATE TABLE decisions (
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
CREATE INDEX decisions_at ON decisions (at);
CREATE INDEX decisions_account ON decisions (account_id, at);
CREATE INDEX decisions_pool ON decisions (pool_id, at);
CREATE INDEX decisions_session ON decisions (session_id, at);
CREATE INDEX decisions_key ON decisions (key_id, at);
ALTER TABLE events ADD COLUMN pool_id TEXT;
ALTER TABLE events ADD COLUMN agent_id TEXT;
ALTER TABLE events ADD COLUMN request_id TEXT;
CREATE INDEX events_account ON events (account_id, at);
CREATE INDEX events_session ON events (session_id, at);
CREATE INDEX events_request ON events (request_id);",
    ),
    (30, "ALTER TABLE rollups_hourly ADD COLUMN probes_held INTEGER NOT NULL DEFAULT 0;"),
    (
        31,
        "ALTER TABLE attempts ADD COLUMN written_at INTEGER; \
         ALTER TABLE requests ADD COLUMN stream_end_at INTEGER;",
    ),
    (32, "ALTER TABLE requests ADD COLUMN diagnostics_source TEXT;"),
    (
        33,
        "ALTER TABLE attempts DROP COLUMN estimated_usd; ALTER TABLE attempts DROP COLUMN input_usd; \
         ALTER TABLE attempts DROP COLUMN cache_read_usd; ALTER TABLE attempts DROP COLUMN cache_write_usd; \
         ALTER TABLE attempts DROP COLUMN output_usd; ALTER TABLE attempts DROP COLUMN pricing_version; \
         ALTER TABLE attempts ADD COLUMN tier TEXT;",
    ),
    (
        34,
        "CREATE TABLE rollups_hourly_next (
  hour INTEGER NOT NULL, key_id TEXT NOT NULL, key_generation INTEGER NOT NULL DEFAULT 0, pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL, canonical_model TEXT NOT NULL, kind TEXT NOT NULL, user_agent TEXT NOT NULL,
  client TEXT NOT NULL DEFAULT '-', effort TEXT NOT NULL DEFAULT 'none', request_class TEXT NOT NULL DEFAULT '-',
  speed TEXT NOT NULL DEFAULT 'standard', funding TEXT NOT NULL DEFAULT 'plan',
  band TEXT NOT NULL DEFAULT 'standard', tier TEXT NOT NULL DEFAULT '-',
  requests INTEGER NOT NULL, succeeded INTEGER NOT NULL, rate_limited INTEGER NOT NULL, failed INTEGER NOT NULL,
  exhausted INTEGER NOT NULL, rejected INTEGER NOT NULL, attempts INTEGER NOT NULL, failovers INTEGER NOT NULL,
  cache_hits INTEGER NOT NULL, cache_misses INTEGER NOT NULL, uncached_requests INTEGER NOT NULL,
  input_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL, cache_write_5m_tokens INTEGER NOT NULL,
  cache_write_1h_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, thinking_tokens INTEGER NOT NULL DEFAULT 0,
  web_search_requests INTEGER NOT NULL DEFAULT 0, web_fetch_requests INTEGER NOT NULL DEFAULT 0,
  kept_warm_requests INTEGER NOT NULL, kept_warm_tokens INTEGER NOT NULL, probe_migrated INTEGER NOT NULL,
  wasted_probes INTEGER NOT NULL, wasted_probe_reasons_json TEXT NOT NULL, probes_skipped INTEGER NOT NULL DEFAULT 0,
  wasted_input_tokens INTEGER NOT NULL DEFAULT 0, wasted_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_cache_write_5m_tokens INTEGER NOT NULL DEFAULT 0, wasted_cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_output_tokens INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL, ttfc_hist BLOB NOT NULL, duration_hist BLOB NOT NULL, output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL, cache_miss_reasons_json TEXT NOT NULL, incomplete INTEGER NOT NULL DEFAULT 0,
  probes_held INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding, band, tier)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding, band, tier, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, kept_warm_requests, kept_warm_tokens, probe_migrated, wasted_probes, wasted_probe_reasons_json, probes_skipped, wasted_input_tokens, wasted_cache_read_tokens, wasted_cache_write_5m_tokens, wasted_cache_write_1h_tokens, wasted_output_tokens, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json, incomplete, probes_held) SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding, 'standard', '-', requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, kept_warm_requests, kept_warm_tokens, probe_migrated, wasted_probes, wasted_probe_reasons_json, probes_skipped, 0, 0, 0, 0, 0, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json, incomplete, probes_held FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);",
    ),
    (
        35,
        "CREATE TABLE routing_conditions (
  kind TEXT NOT NULL, subject_kind TEXT NOT NULL, subject_id TEXT NOT NULL, pool_id TEXT,
  since INTEGER NOT NULL, count INTEGER NOT NULL, detail_json TEXT NOT NULL, computed_at INTEGER NOT NULL,
  PRIMARY KEY (kind, subject_kind, subject_id)
);",
    ),
    (
        36,
        "ALTER TABLE rollups_hourly ADD COLUMN slow_down_held_ms INTEGER NOT NULL DEFAULT 0; \
         ALTER TABLE rollups_hourly ADD COLUMN slow_down_held INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        37,
        "ALTER TABLE rollups_hourly RENAME COLUMN kept_warm_tokens TO kept_warm_5m_tokens;
ALTER TABLE rollups_hourly ADD COLUMN kept_warm_1h_tokens INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        38,
        "CREATE TABLE window_reconciliations (
  account_id TEXT NOT NULL, window TEXT NOT NULL, window_end INTEGER NOT NULL,
  first_reading_at INTEGER NOT NULL, last_reading_at INTEGER NOT NULL,
  observed_rise REAL NOT NULL, predicted_share REAL NOT NULL, ratio REAL NOT NULL,
  requests INTEGER NOT NULL, estimated INTEGER NOT NULL, computed_at INTEGER NOT NULL,
  PRIMARY KEY (account_id, window, window_end)
);",
    ),
    (
        39,
        "ALTER TABLE requests ADD COLUMN guardrail_id TEXT;
CREATE TABLE request_guardrails (request_id TEXT NOT NULL, guardrail_id TEXT NOT NULL, position INTEGER NOT NULL, blocked INTEGER NOT NULL, received_at INTEGER NOT NULL, PRIMARY KEY (request_id, guardrail_id));
CREATE INDEX request_guardrails_received ON request_guardrails (received_at, guardrail_id, blocked);",
    ),
    (
        40,
        "ALTER TABLE requests ADD COLUMN classifier TEXT;
ALTER TABLE requests ADD COLUMN client_contract TEXT;
CREATE TABLE rollups_hourly_next (
  hour INTEGER NOT NULL, key_id TEXT NOT NULL, key_generation INTEGER NOT NULL DEFAULT 0, pool_id TEXT NOT NULL,
  account_id TEXT NOT NULL, canonical_model TEXT NOT NULL, kind TEXT NOT NULL, user_agent TEXT NOT NULL,
  client TEXT NOT NULL DEFAULT '-', effort TEXT NOT NULL DEFAULT 'none', request_class TEXT NOT NULL DEFAULT '-',
  speed TEXT NOT NULL DEFAULT 'standard', funding TEXT NOT NULL DEFAULT 'plan',
  band TEXT NOT NULL DEFAULT 'standard', tier TEXT NOT NULL DEFAULT '-', classifier TEXT NOT NULL DEFAULT '-',
  requests INTEGER NOT NULL, succeeded INTEGER NOT NULL, rate_limited INTEGER NOT NULL, failed INTEGER NOT NULL,
  exhausted INTEGER NOT NULL, rejected INTEGER NOT NULL, attempts INTEGER NOT NULL, failovers INTEGER NOT NULL,
  cache_hits INTEGER NOT NULL, cache_misses INTEGER NOT NULL, uncached_requests INTEGER NOT NULL,
  input_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL, cache_write_5m_tokens INTEGER NOT NULL,
  cache_write_1h_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, thinking_tokens INTEGER NOT NULL DEFAULT 0,
  web_search_requests INTEGER NOT NULL DEFAULT 0, web_fetch_requests INTEGER NOT NULL DEFAULT 0,
  kept_warm_requests INTEGER NOT NULL, kept_warm_5m_tokens INTEGER NOT NULL, probe_migrated INTEGER NOT NULL,
  wasted_probes INTEGER NOT NULL, wasted_probe_reasons_json TEXT NOT NULL, probes_skipped INTEGER NOT NULL DEFAULT 0,
  wasted_input_tokens INTEGER NOT NULL DEFAULT 0, wasted_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_cache_write_5m_tokens INTEGER NOT NULL DEFAULT 0, wasted_cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0,
  wasted_output_tokens INTEGER NOT NULL DEFAULT 0,
  ttfb_hist BLOB NOT NULL, ttfc_hist BLOB NOT NULL, duration_hist BLOB NOT NULL, output_tps_hist BLOB NOT NULL,
  errors_json TEXT NOT NULL, cache_miss_reasons_json TEXT NOT NULL, incomplete INTEGER NOT NULL DEFAULT 0,
  probes_held INTEGER NOT NULL DEFAULT 0, slow_down_held_ms INTEGER NOT NULL DEFAULT 0,
  slow_down_held INTEGER NOT NULL DEFAULT 0, kept_warm_1h_tokens INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort,
    request_class, speed, funding, band, tier, classifier)
);
INSERT INTO rollups_hourly_next (hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent,
  client, effort, request_class, speed, funding, band, tier, classifier, requests, succeeded, rate_limited, failed,
  exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens,
  cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens,
  web_search_requests, web_fetch_requests, kept_warm_requests, kept_warm_5m_tokens, probe_migrated, wasted_probes,
  wasted_probe_reasons_json, probes_skipped, wasted_input_tokens, wasted_cache_read_tokens,
  wasted_cache_write_5m_tokens, wasted_cache_write_1h_tokens, wasted_output_tokens, ttfb_hist, ttfc_hist,
  duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json, incomplete, probes_held, slow_down_held_ms,
  slow_down_held, kept_warm_1h_tokens)
SELECT hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort,
  request_class, speed, funding, band, tier, '-', requests, succeeded, rate_limited, failed, exhausted, rejected,
  attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens,
  cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests,
  web_fetch_requests, kept_warm_requests, kept_warm_5m_tokens, probe_migrated, wasted_probes,
  wasted_probe_reasons_json, probes_skipped, wasted_input_tokens, wasted_cache_read_tokens,
  wasted_cache_write_5m_tokens, wasted_cache_write_1h_tokens, wasted_output_tokens, ttfb_hist, ttfc_hist,
  duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json, incomplete, probes_held, slow_down_held_ms,
  slow_down_held, kept_warm_1h_tokens
FROM rollups_hourly;
DROP TABLE rollups_hourly;
ALTER TABLE rollups_hourly_next RENAME TO rollups_hourly;
CREATE INDEX IF NOT EXISTS rollups_hourly_key ON rollups_hourly (key_id, key_generation, hour);
CREATE INDEX IF NOT EXISTS rollups_hourly_member ON rollups_hourly (pool_id, account_id, hour);",
    ),
];
