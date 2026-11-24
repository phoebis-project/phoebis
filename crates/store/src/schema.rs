//! The registry schema at version 1: `account_tombstones`, `accounts`, `affinity`, `api_key_retired_secrets`,
//! `api_key_secrets`, `api_key_tombstones`, `api_keys`, `cooldowns`, `credentials`, `guardrail_attachments`,
//! `guardrail_tombstones`, `guardrails`, `keepalive_agents`, `keepalive_idle_returns`, `keepalive_sessions`,
//! `model_resolutions`, `overage_state`, `pins`, `pool_members`, `pool_tombstones`, `pools`, `refresh_attempts`,
//! `registry_changes`, `session_memory`, `shed_rounds`, `usage_credits` and `usage_windows`, plus `meta` for the schema
//! marker. Version 3 adds `usage_window_sets`. Version 7 adds the limit resets' three columns to `accounts`. Version 8
//! adds `accounts.state_changed_at`, `model_week_hours`, `pins.thinking_sums`, `pools.fable_reservation_shedding`,
//! `pools.thinking_diagnostics`, `pools.weekly_ceiling` and `thinking_bindings`, renames `pools.window_ceiling` to
//! `five_hour_ceiling` and drops `session_first_seen`. Version 10 adds `lapsed_agents`.

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- Every instant here is epoch milliseconds. state_cause names who moved the account's state, so the routing log reads
-- the cause off the registry rather than inferring it from the path a change took. A subscription end is stored only
-- beside the canceled flag, and plan_ended_at records a profile read that found the organization holding no plan, read
-- like the operator's end.
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
-- An id is reusable, so a deleted account leaves behind the one thing its successor cannot inherit, how many times the
-- id was used. The tombstone outlives the row it names, which is why it is a table of its own with no reference to
-- accounts, and the cascades that empty the account's other tables leave it alone.
CREATE TABLE IF NOT EXISTS account_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
-- last_error keeps the reason of a credential failure that opens no refresh attempt (a backoff, a quarantine), which
-- has nowhere else to leave it.
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
-- An attempt names the incarnation of the account it was recorded for, since an id is reusable, and 0 matches no live
-- holder.
CREATE TABLE IF NOT EXISTS refresh_attempts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, account_id TEXT NOT NULL, lifecycle_generation INTEGER NOT NULL,
  credential_generation INTEGER NOT NULL, started_at INTEGER NOT NULL, resolved_at INTEGER,
  outcome TEXT, detail TEXT, incarnation INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS refresh_attempts_pending ON refresh_attempts (account_id)
  WHERE resolved_at IS NULL OR outcome = 'uncertain';
-- The event mirror pages settled attempts by (resolved_at, id) every few seconds, and the pending index covers the rows
-- that read excludes.
CREATE INDEX IF NOT EXISTS refresh_attempts_resolved ON refresh_attempts (resolved_at, id) WHERE outcome IS NOT NULL;
-- The attention reader reads every account's newest settled attempts on each scrape, and the resolved index leads with
-- the time, so the holder's own index keeps that read off a table the history purge cuts only at the retention.
CREATE INDEX IF NOT EXISTS refresh_attempts_holder_resolved
  ON refresh_attempts (account_id, incarnation, resolved_at, id) WHERE outcome IS NOT NULL;
-- A holder's newest attempt is its highest id under its account and incarnation, which neither partial index serves.
CREATE INDEX IF NOT EXISTS refresh_attempts_account ON refresh_attempts (account_id, incarnation, id);
-- cache_ttl_json is one JSON object rather than a column per field, so another override joins as one more defaulted
-- member. It holds the plan and metered rule sets apart, and its reader refuses a member it does not know and reads the
-- whole policy as off. The incarnation, which pool_tombstones keeps past a delete, stops a pool created again under a
-- deleted id from answering its predecessor's edit token.
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
-- Outlives the pool it names, as the accounts' tombstones do.
CREATE TABLE IF NOT EXISTS pool_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pool_members (
  pool_id TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  priority INTEGER NOT NULL DEFAULT 0, budget_daily_usd REAL, budget_monthly_usd REAL,
  PRIMARY KEY (pool_id, account_id)
);
-- An account's one membership, enforced below the member write's own check.
CREATE UNIQUE INDEX IF NOT EXISTS pool_members_one_pool ON pool_members (account_id);
-- The incarnation, which api_key_tombstones keeps past a delete, stops a key created again under a deleted id from
-- answering its predecessor's edit token. updated_at dates the key's last change, retired_secret_last_used_at says when
-- the retired secret last authenticated, so the console can count the sessions still on it, and a null auto_mode_server
-- inherits the pool's.
CREATE TABLE IF NOT EXISTS api_keys (
  id TEXT PRIMARY KEY, label TEXT NOT NULL, notes TEXT, pool_id TEXT NOT NULL REFERENCES pools(id),
  enabled INTEGER NOT NULL, revoked_at INTEGER, expires_at INTEGER, keepalive_opt_out INTEGER NOT NULL DEFAULT 0,
  body_capture_hours INTEGER, secret_digest BLOB NOT NULL UNIQUE, prefix TEXT NOT NULL,
  created_at INTEGER NOT NULL, rotated_at INTEGER, last_used_at INTEGER, retired_secret_last_used_at INTEGER,
  model_rules_json TEXT NOT NULL DEFAULT '[]', model_rules_mode TEXT NOT NULL DEFAULT 'extend',
  long_context_twins INTEGER NOT NULL DEFAULT 1, auto_mode_server INTEGER, updated_at INTEGER NOT NULL DEFAULT 0,
  incarnation INTEGER NOT NULL DEFAULT 1
);
-- A rotation retires the previous secret rather than deleting it: the console promises running sessions a window to
-- switch, and a session presenting the retired secret inside it is the operator's own. One row per key, so a second
-- rotation inside the window ends the older secret at once and the grace is a bounded overlap of two secrets.
CREATE TABLE IF NOT EXISTS api_key_retired_secrets (
  key_id TEXT PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
  secret_digest BLOB NOT NULL, prefix TEXT NOT NULL,
  retired_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
-- Outlives the key it names, as the accounts' tombstones do.
CREATE TABLE IF NOT EXISTS api_key_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS registry_changes (
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, actor TEXT NOT NULL, kind TEXT NOT NULL,
  target TEXT NOT NULL, diff_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS registry_changes_target ON registry_changes (target, id);
-- A pin is keyed by its agent too, so a subagent of a session pins apart from its main turn, and it names its key
-- generation and account incarnation, since an id is reusable, with 0 matching no live holder. The burn columns are
-- typed tokens per second, the banded copy weighed by the peak band at each renewal and the fast copy kept beside them,
-- all priced when read. The unmetered copy is the client's own classifier calls, priced in dollars alone and never
-- under a window share, and last_classifier says a classifier call renewed the pin last, whose stored context then
-- prices no rewrite. lane is the cache lane the last request wrote to, a filter beside the key, and '' adopts the next
-- request's lane. entrypoint is the regime the figures price under, '' the base one. A shedding mark (marked_at,
-- marked_on, marked_window) lives on the pin it was made on, and shed_reason says why the stamp was written, a round's
-- move or the failover's word.
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
-- The placement index that lets a fresh session or a subagent cohort land where its prefix is warm. depth is the
-- ordinal of the entry's boundary in its chain, which the routing map records at the write and the purge reports.
-- message_id is the upstream id of the newest answer at a message boundary, so the relay's cache diagnostics member
-- names the answer a request continues rather than the session's last one, and a static entry carries none. lane is the
-- writer's cache lane, read as on the pins.
CREATE TABLE IF NOT EXISTS affinity (
  model TEXT NOT NULL, hash TEXT NOT NULL, chain TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, incarnation INTEGER NOT NULL,
  written_at INTEGER NOT NULL, ttl_ms INTEGER NOT NULL, prefix_tokens INTEGER NOT NULL DEFAULT 0,
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, root TEXT NOT NULL, lane TEXT NOT NULL DEFAULT '',
  depth INTEGER NOT NULL DEFAULT 0, message_id TEXT,
  PRIMARY KEY (model, hash)
);
CREATE INDEX IF NOT EXISTS affinity_written_at ON affinity (written_at);
-- A cooldown names the incarnation it was set for, since an id is reusable, and 0 matches no live holder.
CREATE TABLE IF NOT EXISTS cooldowns (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, scope TEXT NOT NULL,
  until INTEGER NOT NULL, reason TEXT NOT NULL, window TEXT NOT NULL, source_request_id TEXT,
  set_at INTEGER NOT NULL DEFAULT 0, incarnation INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, scope)
);
-- An overage reading names the incarnation it was read for, as a cooldown does.
CREATE TABLE IF NOT EXISTS overage_state (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  in_use INTEGER NOT NULL, since INTEGER, reset INTEGER, observed_at INTEGER NOT NULL, source TEXT NOT NULL,
  incarnation INTEGER NOT NULL DEFAULT 0
);
-- The newest reading of each of an OAuth account's plan windows, as the routing state last held it, so a restart or a
-- startup poll the endpoint refuses routes from the last known windows. The api_key buckets refill within a minute and
-- are not kept. Deleted with the account.
CREATE TABLE IF NOT EXISTS usage_windows (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  window TEXT NOT NULL,
  utilization REAL NOT NULL, reset INTEGER, observed_at INTEGER NOT NULL,
  incarnation INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account_id, window)
);
-- The newest extra-usage state the poll read for an OAuth account, so a restart judges fast eligibility on the credits
-- the last poll saw rather than admitting fast requests an account refuses. Kept and dropped as the usage windows are,
-- under the holder's incarnation and deleted with the account.
CREATE TABLE IF NOT EXISTS usage_credits (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL, disabled_reason TEXT, utilization REAL, observed_at INTEGER NOT NULL,
  incarnation INTEGER NOT NULL
);
-- One row per pool and account with the latest round's time alone, which the dwell between rounds reads. The marks live
-- on the pins.
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
-- The relay's session memory: one row per key generation, session and agent, the payload a JSON document the relay owns
-- and record_version the shape it was written in, so the store never reads inside it and a later shape reads older rows
-- by their version. The generation is in the key because an id is reusable and what one holder saw is never the next
-- holder's. session_id holds the conversation under a kind prefix, `session:` for the client's own id.
CREATE TABLE IF NOT EXISTS "session_memory" (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id, agent_id)
);
-- last_seen is what the retention purge measures from.
CREATE INDEX IF NOT EXISTS session_memory_seen ON session_memory (last_seen);
-- The keepalive engine's state, so a restart keeps its sessions, its agents with their retained prompts and the
-- idle-age counters.
CREATE TABLE IF NOT EXISTS keepalive_sessions (
  key_id TEXT NOT NULL, key_generation INTEGER NOT NULL, session_id TEXT NOT NULL,
  record_version INTEGER NOT NULL, payload TEXT NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, key_generation, session_id)
);
-- One versioned payload of the agent's fields rather than a column per field, so a field the engine reshapes moves the
-- payload's version and no column. The prompt sits beside it sealed under the storage key, with the header block, the
-- size and the dispatch TTL the probe replays it with.
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
-- A named list of conditions a request is checked against before any account is selected.
CREATE TABLE IF NOT EXISTS guardrails (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  enabled INTEGER NOT NULL DEFAULT 1, mode TEXT NOT NULL DEFAULT 'observe', action TEXT NOT NULL DEFAULT 'block',
  conditions_json TEXT NOT NULL, preset_id TEXT, preset_version INTEGER,
  incarnation INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
-- Outlives the guardrail it names, as the pools' tombstones do.
CREATE TABLE IF NOT EXISTS guardrail_tombstones (id TEXT PRIMARY KEY, incarnation INTEGER NOT NULL);
-- Attaches a guardrail to pools and keys in a position per subject. One table with a kind column rather than two, since
-- both lists are one shape with one writer, and a subject column cannot reference two tables, so the pool and key
-- deletes remove their rows themselves.
CREATE TABLE IF NOT EXISTS guardrail_attachments (
  guardrail_id TEXT NOT NULL REFERENCES guardrails(id) ON DELETE CASCADE,
  subject_kind TEXT NOT NULL, subject_id TEXT NOT NULL, position INTEGER NOT NULL,
  PRIMARY KEY (guardrail_id, subject_kind, subject_id)
);
CREATE INDEX IF NOT EXISTS guardrail_attachments_subject ON guardrail_attachments (subject_kind, subject_id, position);
"#;

/// The pins carry the auxiliary-only mark, so a restart reads it from the row rather than deriving it from two
/// stamps, and the effort of the pin's last turn, which the Sessions route answers.
/// `session_first_seen` keeps when each agent of a session first placed a pin, a creation time a failover or a move
/// leaves alone, until step 8 drops the table. The idle-return counters are kept per UTC day so the overview can answer
/// a range, and the all-time rows leave with the old table, since a count with no day cannot be placed on one. The
/// newest reading of each window keeps the plan and the tier it was taken under, so a reset prices a past week at that
/// week's own tier.
const V2: &str = r#"
ALTER TABLE pins ADD COLUMN auxiliary_only INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN effort TEXT NOT NULL DEFAULT '';
CREATE TABLE session_first_seen (
  key_id TEXT NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, first_seen_at INTEGER NOT NULL,
  PRIMARY KEY (key_id, session_id, agent_id)
);
DROP TABLE keepalive_idle_returns;
CREATE TABLE keepalive_idle_returns (
  pool_id TEXT NOT NULL, day INTEGER NOT NULL, bucket INTEGER NOT NULL, reached INTEGER NOT NULL,
  returned INTEGER NOT NULL,
  PRIMARY KEY (pool_id, day, bucket)
);
ALTER TABLE usage_windows ADD COLUMN plan TEXT;
ALTER TABLE usage_windows ADD COLUMN tier TEXT;
"#;

/// The shared window set of an OAuth account as its newest clean poll settled it, so a restart prices no window the
/// account lost before the next poll answers.
const V3: &str = r#"
CREATE TABLE usage_window_sets (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  windows TEXT NOT NULL, observed_at INTEGER NOT NULL, incarnation INTEGER NOT NULL
);
"#;

/// A settled attempt's class as its settlement recorded it, which the ledger's refresh rows name beside the detail
/// text rather than reading it back out of that text.
const V4: &str = r#"
ALTER TABLE refresh_attempts ADD COLUMN cause TEXT;
"#;

/// The companion's per-key permissions replace the keepalive opt-out: a migrated opt-out is a set without `keepalive`.
const V5: &str = r#"
ALTER TABLE api_keys ADD COLUMN companion_permissions_json TEXT NOT NULL DEFAULT '["allowance","keepalive"]';
UPDATE api_keys SET companion_permissions_json = '["allowance"]' WHERE keepalive_opt_out = 1;
ALTER TABLE api_keys DROP COLUMN keepalive_opt_out;
"#;

/// The plan word the exchange built from two profile flags had no reader: the term resolves its plan from the
/// stored document and the tier. A pin carries its agent's end, so a restart keeps an ended agent's pin out of the
/// figures. No classifier call renews a pin, so the flag for one leaves, and the classifier copy names the model
/// its calls ran on, which prices it. A flagged pin's count is that call's small prompt, which the router would now
/// trust as the pin's own, so the step zeroes it and the pin's next move sizes its rewrite by the request's body, as
/// the flag had it. A saved keepalive agent whose cause is `released` leaves, since no build writes or reads that cause
/// any more.
const V6: &str = r#"
ALTER TABLE accounts DROP COLUMN subscription_plan;
ALTER TABLE pins ADD COLUMN ended INTEGER NOT NULL DEFAULT 0;
UPDATE pins SET last_input_tokens = 0 WHERE last_classifier = 1;
ALTER TABLE pins DROP COLUMN last_classifier;
ALTER TABLE pins ADD COLUMN burn_unmetered_model TEXT;
DELETE FROM keepalive_agents WHERE json_extract(payload, '$.reason') = 'released';
"#;

/// An account's limit reset block as the usage service last read it, with the instant of that read, so the cards
/// and the conditions read a block across a restart. The epoch orders the block's writers: a claim's fold and a
/// pairing advance it, and a read that started before either writes nothing.
const V7: &str = r#"
ALTER TABLE accounts ADD COLUMN limit_resets_json TEXT;
ALTER TABLE accounts ADD COLUMN limit_resets_read_at INTEGER;
ALTER TABLE accounts ADD COLUMN limit_resets_epoch INTEGER NOT NULL DEFAULT 0;
"#;

/// Step 8: the record of the share the router's own requests priced on a model week per clock hour. The pool's
/// switch for the thinking-binding beta is not null with a default, since the pool's read takes a boolean and a pool
/// stored before the step would read null. The pool's one ceiling becomes the 5-hour window's, and the weekly windows
/// read 1.0 on a row at the old default 0.95 and the row's own fraction on any other, since an operator who lowered
/// the one fraction held a weekly reserve that way. A pin stored before the step holds no thinking sum and the router
/// has learned no binding, so each move prices the session's whole last prompt until an answer says more. The
/// first-seen table has no reader left, since the Sessions route reads a session's first request from the ledger.
/// The pool's option to shed on the Fable reservation starts off, as a pool stored before the step ran without it.
/// An account stored before the step dates its state from the row's last write, the nearest instant the registry holds.
const V8: &str = r#"
CREATE TABLE model_week_hours (
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    incarnation INTEGER NOT NULL,
    window      TEXT NOT NULL,
    hour        INTEGER NOT NULL,
    share       REAL NOT NULL,
    PRIMARY KEY (account_id, window, hour)
);
ALTER TABLE pools ADD COLUMN thinking_diagnostics INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pools RENAME COLUMN window_ceiling TO five_hour_ceiling;
ALTER TABLE pools ADD COLUMN weekly_ceiling REAL NOT NULL DEFAULT 1.0;
UPDATE pools SET weekly_ceiling = five_hour_ceiling WHERE five_hour_ceiling <> 0.95;
ALTER TABLE pins ADD COLUMN thinking_sums TEXT NOT NULL DEFAULT '{}';
CREATE TABLE thinking_bindings (
    model       TEXT NOT NULL,
    account_a   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    account_b   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    bound       INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (model, account_a, account_b)
);
DROP TABLE session_first_seen;
ALTER TABLE pools ADD COLUMN fable_reservation_shedding INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN state_changed_at INTEGER NOT NULL DEFAULT 0;
UPDATE accounts SET state_changed_at = updated_at;
"#;

/// A pool's feature accounts and a key's overrides of them, each one JSON object keyed by the feature word, so a
/// further feature joins as one more member. A missing word is none on a pool and inherit on a key, where a null
/// is none.
const V9: &str = r#"
ALTER TABLE pools ADD COLUMN feature_accounts_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE api_keys ADD COLUMN feature_accounts_json TEXT NOT NULL DEFAULT '{}';
"#;

/// A pin whose session holds account state, which stays on its account until a portable turn of the conversation, or
/// until a request bound by web search results alone leaves it because its account cannot serve in time. A shedding
/// mark names the test that fired its round, null on a mark made before the column, which reads as a projection
/// round's. An account cooldown of a usage-based seat keeps the spend share at its refusal, null on each other row and
/// on a seat's row set before the column, which lifts as a share of 1.0. A lapsed agent's record keeps the burn and the
/// thinking sums of a pin the purge took as expired, with whether its session held account state, and the table starts
/// empty. A pin whose compaction call answered whole carries a mark until the agent's next conversation request, and a
/// lapsed agent's record keeps that mark.
const V10: &str = r#"
ALTER TABLE pins ADD COLUMN account_bound INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pins ADD COLUMN marked_source TEXT;
ALTER TABLE cooldowns ADD COLUMN spend_share REAL;
CREATE TABLE lapsed_agents (
  key_id TEXT NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, model TEXT NOT NULL,
  key_generation INTEGER NOT NULL, account_id TEXT NOT NULL, incarnation INTEGER NOT NULL,
  last_used_at INTEGER NOT NULL, last_input_tokens INTEGER NOT NULL,
  burn_input REAL NOT NULL, burn_cache_read REAL NOT NULL, burn_write_5m REAL NOT NULL,
  burn_write_1h REAL NOT NULL, burn_output REAL NOT NULL,
  burn_banded_input REAL NOT NULL, burn_banded_cache_read REAL NOT NULL, burn_banded_write_5m REAL NOT NULL,
  burn_banded_write_1h REAL NOT NULL, burn_banded_output REAL NOT NULL,
  burn_fast_input REAL NOT NULL, burn_fast_cache_read REAL NOT NULL, burn_fast_write_5m REAL NOT NULL,
  burn_fast_write_1h REAL NOT NULL, burn_fast_output REAL NOT NULL,
  burn_unmetered_cache_read REAL NOT NULL, burn_unmetered_input REAL NOT NULL, burn_unmetered_output REAL NOT NULL,
  burn_unmetered_write_1h REAL NOT NULL, burn_unmetered_write_5m REAL NOT NULL, burn_unmetered_model TEXT,
  speed REAL NOT NULL, thinking_sums TEXT NOT NULL, account_bound INTEGER NOT NULL,
  PRIMARY KEY (key_id, session_id, agent_id, model)
);
ALTER TABLE pins ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE lapsed_agents ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0;
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
];

/// The newest schema version this build knows how to read. `prepare` refuses a registry stamped past it
/// rather than skip migrations it does not carry and misread the tables that follow.
pub const SCHEMA_VERSION: u32 = crate::migrations::latest_version(MIGRATIONS);
