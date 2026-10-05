//! The ledger's SQLite file: one writer connection the ledger task owns through `write`, one connection the
//! retention purge owns through `purge`, and one read-only connection the admin routes share through `read`. WAL
//! lets a read run beside either writer; the two writers take SQLite's one write lock in turn, each waiting for
//! the other's transaction under the busy timeout.

use std::path::Path;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};

use crate::query::request_by_id;
use crate::record::{
    AttemptRecord, BodyRecord, CacheMapWrite, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    QuotaSampleRecord, RequestRecord,
};
use crate::{schema, LedgerError};

/// How long a writing connection waits for the write lock while the other holds it. A slice or a batch holds it
/// for milliseconds, so the margin is for a slow disk; the registry waits as long.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct LedgerDb {
    writer: Mutex<Connection>,
    /// `None` for an in-memory ledger, whose one connection serves every role.
    reader: Option<Mutex<Connection>>,
    /// The retention purge's own connection, so a slice never holds the writer's lock or turn; `None` in memory.
    purger: Option<Mutex<Connection>>,
    /// The writes begun and ended through `write`, which `purge` reads to let a waiting write go first.
    writes: Writes,
    #[cfg(any(test, feature = "debug-hooks"))]
    fail_next_read: AtomicBool,
}

/// One `parked_records` row as the start reads it: the row's id and the serialized `ParkedRecord`.
pub(crate) struct ParkedRow {
    pub id: i64,
    pub record: String,
}

/// SQLite's busy handler retries a waiting write on a backoff that grows to 100 ms, while back-to-back slices free
/// the lock for microseconds between them: a write left to the handler alone waits out most of a pass, and past the
/// busy timeout its batch fails into the writer's retries. So a slice starts only once every write begun before it
/// has ended, and a batch waits one slice at most. The handoff works in one direction: after its wait the purge's
/// `BEGIN IMMEDIATE` competes with the next batch under SQLite's busy backoff, so a writer landing batches back to
/// back can push a slice past the 5 s busy timeout, and the pass ends logged until the next hour.
#[derive(Default)]
struct Writes {
    begun: AtomicU64,
    ended: AtomicU64,
}

/// Counts its write as ended when dropped, so a closure that panicked still lets the purge go on.
struct Ended<'a>(&'a AtomicU64);

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl LedgerDb {
    pub fn open(path: &Path) -> Result<LedgerDb, LedgerError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        if !path.exists() {
            // Captured bodies are prompts; the file is created under 0600 before SQLite can apply the umask.
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(path)?;
        }
        let mut writer = Connection::open(path)?;
        writer.busy_timeout(BUSY_TIMEOUT)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        // A power loss may lose the last transactions before a checkpoint, which the spec accepts for a
        // best-effort ledger; `FULL` would fsync on every request.
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        prepare(&mut writer, path)?;
        let reader = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // The journal mode is the file's, which the writer set; `synchronous` is the connection's own.
        let purger = Connection::open(path)?;
        purger.busy_timeout(BUSY_TIMEOUT)?;
        purger.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(LedgerDb {
            writer: Mutex::new(writer),
            reader: Some(Mutex::new(reader)),
            purger: Some(Mutex::new(purger)),
            writes: Writes::default(),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_read: AtomicBool::new(false),
        })
    }

    pub fn open_in_memory() -> Result<LedgerDb, LedgerError> {
        let mut writer = Connection::open_in_memory()?;
        prepare(&mut writer, Path::new(":memory:"))?;
        Ok(LedgerDb {
            writer: Mutex::new(writer),
            reader: None,
            purger: None,
            writes: Writes::default(),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_read: AtomicBool::new(false),
        })
    }

    pub async fn schema_version(self: &Arc<Self>) -> Result<u32, LedgerError> {
        self.read(read_version).await
    }

    /// The ledger's `meta.schema_version` through a read-only connection, taking no lock and creating nothing;
    /// `None` for an absent file or one without the table, which the next start creates or migrates.
    pub fn read_schema_version(path: &Path) -> Result<Option<u32>, LedgerError> {
        if !path.try_exists()? {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(Some(read_version(&conn)?).filter(|version| *version != 0))
    }

    /// The newest schema this build reads, which `open` refuses a file past.
    pub fn supported_schema_version() -> u32 {
        latest_version()
    }

    pub fn write<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        self.writes.begun.fetch_add(1, Ordering::SeqCst);
        let _ended = Ended(&self.writes.ended);
        // A panic inside a closure rolls its transaction back when the guard drops, so the connection is
        // consistent and the next batch lands; refusing every later write for one bug would stall the ledger for
        // good.
        let mut conn = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut conn)
    }

    /// Runs `f` under the purger connection (the writer's in memory). A poisoned guard is recovered as
    /// `read_blocking` recovers its own: a rolled-back transaction leaves the connection consistent.
    pub fn purge<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        // A write that met the previous slice's lock goes first; see `Writes`.
        let begun = self.writes.begun.load(Ordering::SeqCst);
        while self.writes.ended.load(Ordering::SeqCst) < begun {
            std::thread::sleep(Duration::from_millis(1));
        }
        let purger = self.purger.as_ref().unwrap_or(&self.writer);
        let mut conn = purger.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut conn)
    }

    /// Runs `f` on a blocking worker under the shared reader connection, so a slow read never parks the runtime
    /// thread that serves proxy traffic; the reader mutex still serialises the reads themselves.
    pub async fn read<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError> + Send + 'static,
    ) -> Result<T, LedgerError> {
        let db = Arc::clone(self);
        tokio::task::spawn_blocking(move || db.read_blocking(f))
            .await
            .map_err(|_| LedgerError::ReadCancelled)?
    }

    /// The same read on the calling thread, for the ledger task's own thread and for tests that hold no runtime
    /// worker to spare.
    pub fn read_blocking<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        if self.fail_next_read.swap(false, Ordering::SeqCst) {
            return Err(LedgerError::Io(std::io::Error::other(
                "injected ledger read failure",
            )));
        }
        // A read never opens a transaction, so a closure that panics mid-row leaves the connection exactly as
        // consistent as it was before the call; recovering the guard here, rather than propagating the
        // poisoning, keeps one panicked read from turning every later read into a `ReadCancelled`.
        match &self.reader {
            Some(reader) => f(&reader.lock().unwrap_or_else(PoisonError::into_inner)),
            None => f(&self.writer.lock().unwrap_or_else(PoisonError::into_inner)),
        }
    }

    pub fn insert_request(
        &self,
        request: &RequestRecord,
        attempts: &[AttemptRecord],
        bodies: Option<&BodyRecord>,
    ) -> Result<(), LedgerError> {
        self.write(|conn| {
            let tx = begin_write(conn)?;
            let previous = insert_request_in(&tx, request, attempts, bodies)?;
            crate::rollup::fold_request(
                &tx,
                request,
                attempts,
                previous.as_ref().map(|(r, a)| (r, a.as_slice())),
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// The next `read` or `read_blocking` answers an injected failure, so the writer's failed spend refresh has a
    /// test.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_read(&self) {
        self.fail_next_read.store(true, Ordering::SeqCst);
    }

    /// Parks one priced record the writer could not land, `parked_at` and `last_tried_at` both `at`.
    pub(crate) fn insert_parked(
        &self,
        at: i64,
        request_id: &str,
        pool_id: Option<&str>,
        account_id: Option<&str>,
        record: &str,
        error: &str,
    ) -> Result<(), LedgerError> {
        self.write(|conn| {
            conn.execute(
                "INSERT INTO parked_records (parked_at, last_tried_at, request_id, pool_id, account_id, record, error) VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6)",
                params![at, request_id, pool_id, account_id, record, error],
            )?;
            Ok(())
        })
    }

    /// Every parked row in the order it was parked, on the writer's connection, which the start runs on.
    pub(crate) fn parked_rows(&self) -> Result<Vec<ParkedRow>, LedgerError> {
        self.write(|conn| {
            let mut stmt = conn.prepare("SELECT id, record FROM parked_records ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(ParkedRow {
                        id: r.get(0)?,
                        record: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Records the failure a parked row met at this start's try.
    pub(crate) fn update_parked(&self, id: i64, error: &str, at: i64) -> Result<(), LedgerError> {
        self.write(|conn| {
            conn.execute(
                "UPDATE parked_records SET error = ?2, last_tried_at = ?3 WHERE id = ?1",
                params![id, error, at],
            )?;
            Ok(())
        })
    }

    pub fn insert_event(&self, event: &EventRecord) -> Result<(), LedgerError> {
        self.write(|conn| {
            insert_event_in(conn, event)?;
            Ok(())
        })
    }

    pub async fn request(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<Option<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
        let id = id.to_string();
        self.read(move |conn| request_by_id(conn, &id)).await
    }

    pub async fn bodies(self: &Arc<Self>, id: &str) -> Result<Option<BodyRecord>, LedgerError> {
        let id = id.to_string();
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT request_id, request_body, response_body, request_len_seen, response_len_seen, expires_at FROM bodies WHERE request_id = ?1",
                    params![id],
                    |r| Ok(BodyRecord {
                        request_id: r.get(0)?,
                        request_body: r.get(1)?,
                        response_body: r.get(2)?,
                        request_len_seen: r.get::<_, i64>(3)? as u64,
                        response_len_seen: r.get::<_, i64>(4)? as u64,
                        expires_at: r.get(5)?,
                    }),
                )
                .optional()?)
        }).await
    }

    pub async fn events(
        self: &Arc<Self>,
        from: i64,
        to: i64,
        limit: usize,
    ) -> Result<Vec<EventRecord>, LedgerError> {
        self.read(move |conn| {
            let mut stmt = conn.prepare("SELECT at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id FROM events WHERE at >= ?1 AND at < ?2 ORDER BY at, rowid LIMIT ?3")?;
            let rows = stmt.query_map(params![from, to, limit as i64], |r| {
                Ok(EventRecord { at: r.get(0)?, kind: r.get(1)?, account_id: r.get(2)?, key_id: r.get(3)?, session_id: r.get(4)?, model: r.get(5)?, actor: r.get(6)?, detail: r.get(7)?, pool_id: r.get(8)?, agent_id: r.get(9)?, request_id: r.get(10)? })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }

    pub fn insert_samples(
        &self,
        quota: &[QuotaSampleRecord],
        extra: Option<&ExtraUsageSampleRecord>,
    ) -> Result<(), LedgerError> {
        self.write(|conn| insert_samples_in(conn, quota, extra))
    }

    pub async fn quota_samples(
        self: &Arc<Self>,
        account_id: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<QuotaSampleRecord>, LedgerError> {
        let account_id = account_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn.prepare("SELECT account_id, bucket, window, utilization, reset_at, source, observed_at FROM quota_samples WHERE account_id = ?1 AND bucket >= ?2 AND bucket < ?3 ORDER BY window, bucket")?;
            let rows = stmt.query_map(params![account_id, from, to], |r| {
                Ok(QuotaSampleRecord {
                    account_id: r.get(0)?,
                    bucket: r.get(1)?,
                    window: r.get(2)?,
                    utilization: r.get(3)?,
                    reset_at: r.get(4)?,
                    source: r.get(5)?,
                    observed_at: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }

    pub async fn extra_usage_samples(
        self: &Arc<Self>,
        account_id: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
        let account_id = account_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn.prepare("SELECT account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, disabled_reason, overage_reset, source FROM extra_usage_samples WHERE account_id = ?1 AND bucket >= ?2 AND bucket < ?3 ORDER BY bucket")?;
            let rows = stmt.query_map(params![account_id, from, to], |r| {
                Ok(ExtraUsageSampleRecord {
                    account_id: r.get(0)?,
                    bucket: r.get(1)?,
                    is_enabled: r.get(2)?,
                    in_use: r.get(3)?,
                    monthly_limit: r.get(4)?,
                    used_credits: r.get(5)?,
                    utilization: r.get(6)?,
                    currency: r.get(7)?,
                    disabled_reason: r.get(8)?,
                    overage_reset: r.get(9)?,
                    source: r.get(10)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }
}

/// A transaction that takes SQLite's write lock at `BEGIN`, where the busy timeout applies. A deferred one that
/// reads before it writes is refused at once with `SQLITE_BUSY` while the other writing connection holds the lock,
/// or once that one committed past its snapshot, and its batch would be lost rather than wait.
pub(crate) fn begin_write(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

/// Deletes a parked row inside the transaction that lands its record, so the two commit or roll back together: a row
/// deleted on its own after the commit could outlive a landing the file then loses, and the next start would land the
/// record again.
pub(crate) fn delete_parked_in(tx: &Transaction<'_>, id: i64) -> Result<(), LedgerError> {
    tx.execute("DELETE FROM parked_records WHERE id = ?1", params![id])?;
    Ok(())
}

pub(crate) fn insert_samples_in(
    conn: &Connection,
    quota: &[QuotaSampleRecord],
    extra: Option<&ExtraUsageSampleRecord>,
) -> Result<(), LedgerError> {
    for q in quota {
        // A header sample is the newest truth for its bucket; a poll only fills a bucket nothing else wrote.
        let verb = if q.source == "header" {
            "INSERT OR REPLACE"
        } else {
            "INSERT OR IGNORE"
        };
        conn.execute(
            &format!(
                "{verb} INTO quota_samples (account_id, bucket, window, utilization, reset_at, source, observed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            params![q.account_id, q.bucket, q.window, q.utilization, q.reset_at, q.source, q.observed_at],
        )?;
    }
    if let Some(e) = extra {
        let verb = if e.source == "header" {
            "INSERT OR REPLACE"
        } else {
            "INSERT OR IGNORE"
        };
        conn.execute(
            &format!(
                "{verb} INTO extra_usage_samples (account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, disabled_reason, overage_reset, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                e.account_id, e.bucket, e.is_enabled, e.in_use, e.monthly_limit, e.used_credits,
                e.utilization, e.currency, e.disabled_reason, e.overage_reset, e.source,
            ],
        )?;
    }
    Ok(())
}

/// Writes the request and its attempts, answering the rows the id held before, if any: a replacement has to
/// take its earlier contribution out of every aggregate that was folded from it.
pub(crate) fn insert_request_in(
    tx: &Transaction<'_>,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    bodies: Option<&BodyRecord>,
) -> Result<Option<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
    let previous = tx
        .query_row(
            &format!("{REQUEST_SELECT} WHERE id = ?1"),
            params![request.id],
            row_to_request,
        )
        .optional()?;
    let previous = match previous {
        None => None,
        Some(row) => {
            let mut stmt = tx.prepare(&format!(
                "{ATTEMPT_SELECT} WHERE request_id = ?1 ORDER BY attempt"
            ))?;
            let old = stmt
                .query_map(params![request.id], row_to_attempt)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Some((row, old))
        }
    };
    // A kept-warm credit is recorded at completion while the writer batches, so the mark can be in the table
    // before the request's own row lands here.
    let marked: bool = tx
        .query_row(
            "SELECT 1 FROM kept_warm_marks WHERE request_id = ?1",
            params![request.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let kept_warm = request.kept_warm || marked;
    tx.execute(
        "INSERT OR REPLACE INTO requests (id, received_at, finished_at, key_id, key_generation, pool_id, session_id, agent_id, parent_agent_id, model, canonical_model, stream, kind, origin, status, outcome, attempts, request_bytes, response_bytes, ttfb_ms, ttfc_ms, duration_ms, error_code, client_drift, kept_warm, bodies_captured, user_agent, effort, thinking_type, thinking_budget, max_tokens, tool_count, system_blocks, message_count, context_edits, temperature, requested_speed, requested_service_tier, presented_retired, rotation_at, request_truncated, response_truncated, client, agent_type, compaction, context_compacted, prev_tool_ms, turn_origin, request_class, identified_by, identity_root, identity_depth, forked_from, fork_depth, auto_mode, held) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48, ?49, NULL, NULL, NULL, ?50, ?51, ?52, ?53)",
        params![
            request.id, request.received_at, request.finished_at, request.key_id, request.key_generation, request.pool_id, request.session_id, request.agent_id,
            request.parent_agent_id, request.model, request.canonical_model, request.stream, request.kind, request.origin,
            request.status.map(i64::from), request.outcome, request.attempts, request.request_bytes.map(|n| n as i64),
            request.response_bytes.map(|n| n as i64), request.ttfb_ms, request.ttfc_ms, request.duration_ms, request.error_code,
            request.client_drift, kept_warm, request.bodies_captured, request.user_agent,
            request.effort, request.thinking_type, request.thinking_budget.map(|n| n as i64), request.max_tokens.map(|n| n as i64),
            i64::from(request.tool_count), i64::from(request.system_blocks), i64::from(request.message_count),
            request.context_edits, request.temperature, request.requested_speed, request.requested_service_tier,
            request.presented_retired, request.rotation_at, request.request_truncated, request.response_truncated,
            request.client, request.agent_type, request.compaction, request.context_compacted, request.prev_tool_ms, request.turn_origin,
            request.request_class,
            request.forked_from, request.fork_depth.map(|n| n as i64), request.auto_mode, request.held,
        ],
    )?;
    tx.execute(
        "DELETE FROM attempts WHERE request_id = ?1",
        params![request.id],
    )?;
    for a in attempts {
        tx.execute(
            "INSERT INTO attempts (request_id, attempt, account_id, account_kind, pin_epoch, started_at, finished_at, ttfb_ms, ttfc_ms, upstream_status, upstream_request_id, outcome, cooldown_scope, cooldown_until, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, service_tier, cache_miss_reason, client_drift, ratelimit_5h_utilization, ratelimit_5h_reset, ratelimit_7d_utilization, ratelimit_7d_reset, ratelimit_7d_oi_utilization, ratelimit_7d_oi_reset, overage_in_use, overage_utilization, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, pricing_version, thinking_tokens, web_search_requests, web_fetch_requests, stop_reason, stop_sequence, served_model, speed, inference_geo, cleared_input_tokens, refusal_category, refusal_recommended_model, selection, upstream_message_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48, ?49)",
            params![
                a.request_id, a.attempt, a.account_id, a.account_kind, a.pin_epoch.map(|n| n as i64), a.started_at, a.finished_at,
                a.ttfb_ms, a.ttfc_ms, a.upstream_status.map(i64::from), a.upstream_request_id, a.outcome, a.cooldown_scope, a.cooldown_until,
                a.input_tokens.map(|n| n as i64), a.output_tokens.map(|n| n as i64), a.cache_read_tokens.map(|n| n as i64),
                a.cache_write_5m_tokens.map(|n| n as i64), a.cache_write_1h_tokens.map(|n| n as i64), a.service_tier, a.cache_miss_reason,
                a.client_drift, a.ratelimit_5h_utilization, a.ratelimit_5h_reset, a.ratelimit_7d_utilization, a.ratelimit_7d_reset,
                a.ratelimit_7d_oi_utilization, a.ratelimit_7d_oi_reset, a.overage_in_use, a.overage_utilization, a.estimated_usd,
                a.input_usd, a.cache_read_usd, a.cache_write_usd, a.output_usd, a.pricing_version,
                a.thinking_tokens.map(|n| n as i64), a.web_search_requests.map(|n| n as i64), a.web_fetch_requests.map(|n| n as i64),
                a.stop_reason, a.stop_sequence, a.served_model, a.speed, a.inference_geo,
                a.cleared_input_tokens.map(|n| n as i64), a.refusal_category, a.refusal_recommended_model, a.selection,
                a.upstream_message_id,
            ],
        )?;
    }
    if let Some(b) = bodies {
        tx.execute(
            "INSERT OR REPLACE INTO bodies (request_id, request_body, response_body, request_len_seen, response_len_seen, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![b.request_id, b.request_body, b.response_body, b.request_len_seen as i64, b.response_len_seen as i64, b.expires_at],
        )?;
    }
    Ok(previous)
}

pub(crate) fn insert_event_in(conn: &Connection, e: &EventRecord) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO events (at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![e.at, e.kind, e.account_id, e.key_id, e.session_id, e.model, e.actor, e.detail, e.pool_id, e.agent_id, e.request_id],
    )?;
    Ok(())
}

/// `summary` is the summary's JSON, `frame` the record's zstd frame; the caller built both inside the batch.
pub(crate) fn insert_decision_in(
    conn: &Connection,
    row: &DecisionRow,
    summary: &str,
    frame: &[u8],
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO decisions (request_id, seq, attempt, at, kind, probe, key_id, pool_id, account_id, from_account_id, session_id, agent_id, model, summary, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            row.request_id,
            row.seq,
            row.attempt,
            row.at,
            row.kind,
            row.probe,
            row.key_id,
            row.pool_id,
            row.account_id,
            row.from_account_id,
            row.session_id,
            row.agent_id,
            row.model,
            summary,
            frame
        ],
    )?;
    Ok(())
}

/// Sets `cache_map` on the decision `(request_id, seq)` names; answers the rows changed (0 when no decision row
/// exists). Keyed by the selecting decision alone, so a retry in place that completed on a later attempt still lands
/// on the decision that selected it.
pub(crate) fn update_cache_map_in(
    conn: &Connection,
    write: &CacheMapWrite,
    entries: &str,
) -> Result<usize, LedgerError> {
    Ok(conn.execute(
        "UPDATE decisions SET cache_map = ?3 WHERE request_id = ?1 AND seq = ?2",
        params![write.request_id, write.seq, entries],
    )?)
}

pub(crate) const REQUEST_SELECT: &str = "SELECT id, received_at, finished_at, key_id, key_generation, pool_id, session_id, agent_id, parent_agent_id, model, canonical_model, stream, kind, origin, status, outcome, attempts, request_bytes, response_bytes, ttfb_ms, ttfc_ms, duration_ms, error_code, client_drift, kept_warm, bodies_captured, user_agent, effort, thinking_type, thinking_budget, max_tokens, tool_count, system_blocks, message_count, context_edits, temperature, requested_speed, requested_service_tier, presented_retired, rotation_at, request_truncated, response_truncated, client, agent_type, compaction, context_compacted, prev_tool_ms, turn_origin, request_class, forked_from, fork_depth, auto_mode, held FROM requests";
pub(crate) const ATTEMPT_SELECT: &str = "SELECT request_id, attempt, account_id, account_kind, pin_epoch, started_at, finished_at, ttfb_ms, ttfc_ms, upstream_status, upstream_request_id, outcome, cooldown_scope, cooldown_until, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, service_tier, cache_miss_reason, client_drift, ratelimit_5h_utilization, ratelimit_5h_reset, ratelimit_7d_utilization, ratelimit_7d_reset, ratelimit_7d_oi_utilization, ratelimit_7d_oi_reset, overage_in_use, overage_utilization, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, pricing_version, thinking_tokens, web_search_requests, web_fetch_requests, stop_reason, stop_sequence, served_model, speed, inference_geo, cleared_input_tokens, refusal_category, refusal_recommended_model, selection, upstream_message_id FROM attempts";

pub(crate) fn row_to_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRecord> {
    Ok(RequestRecord {
        id: r.get(0)?,
        received_at: r.get(1)?,
        finished_at: r.get(2)?,
        key_id: r.get(3)?,
        key_generation: r.get(4)?,
        pool_id: r.get(5)?,
        session_id: r.get(6)?,
        agent_id: r.get(7)?,
        parent_agent_id: r.get(8)?,
        model: r.get(9)?,
        canonical_model: r.get(10)?,
        stream: r.get(11)?,
        kind: r.get(12)?,
        origin: r.get(13)?,
        status: r.get::<_, Option<i64>>(14)?.map(|n| n as u16),
        outcome: r.get(15)?,
        attempts: r.get(16)?,
        request_bytes: r.get::<_, Option<i64>>(17)?.map(|n| n as u64),
        response_bytes: r.get::<_, Option<i64>>(18)?.map(|n| n as u64),
        ttfb_ms: r.get(19)?,
        ttfc_ms: r.get(20)?,
        duration_ms: r.get(21)?,
        error_code: r.get(22)?,
        client_drift: r.get(23)?,
        kept_warm: r.get(24)?,
        bodies_captured: r.get(25)?,
        request_truncated: r.get(40)?,
        response_truncated: r.get(41)?,
        user_agent: r.get(26)?,
        effort: r.get(27)?,
        thinking_type: r.get(28)?,
        thinking_budget: r.get::<_, Option<i64>>(29)?.map(|n| n as u64),
        max_tokens: r.get::<_, Option<i64>>(30)?.map(|n| n as u64),
        tool_count: r.get::<_, i64>(31)? as u32,
        system_blocks: r.get::<_, i64>(32)? as u32,
        message_count: r.get::<_, i64>(33)? as u32,
        context_edits: r.get(34)?,
        temperature: r.get(35)?,
        requested_speed: r.get(36)?,
        requested_service_tier: r.get(37)?,
        presented_retired: r.get(38)?,
        rotation_at: r.get(39)?,
        client: r.get(42)?,
        agent_type: r.get(43)?,
        compaction: r.get(44)?,
        context_compacted: r.get(45)?,
        prev_tool_ms: r.get::<_, Option<i64>>(46)?,
        turn_origin: r.get(47)?,
        request_class: r.get(48)?,
        forked_from: r.get(49)?,
        fork_depth: r.get::<_, Option<i64>>(50)?.map(|n| n as u32),
        auto_mode: r.get(51)?,
        held: r.get(52)?,
    })
}

pub(crate) fn row_to_attempt(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRecord> {
    let tokens = |i: usize| -> rusqlite::Result<Option<u64>> {
        Ok(r.get::<_, Option<i64>>(i)?.map(|n| n as u64))
    };
    Ok(AttemptRecord {
        request_id: r.get(0)?,
        attempt: r.get(1)?,
        account_id: r.get(2)?,
        account_kind: r.get(3)?,
        pin_epoch: r.get::<_, Option<i64>>(4)?.map(|n| n as u64),
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
        ttfb_ms: r.get(7)?,
        ttfc_ms: r.get(8)?,
        upstream_status: r.get::<_, Option<i64>>(9)?.map(|n| n as u16),
        upstream_request_id: r.get(10)?,
        outcome: r.get(11)?,
        cooldown_scope: r.get(12)?,
        cooldown_until: r.get(13)?,
        input_tokens: tokens(14)?,
        output_tokens: tokens(15)?,
        cache_read_tokens: tokens(16)?,
        cache_write_5m_tokens: tokens(17)?,
        cache_write_1h_tokens: tokens(18)?,
        service_tier: r.get(19)?,
        cache_miss_reason: r.get(20)?,
        client_drift: r.get(21)?,
        ratelimit_5h_utilization: r.get(22)?,
        ratelimit_5h_reset: r.get(23)?,
        ratelimit_7d_utilization: r.get(24)?,
        ratelimit_7d_reset: r.get(25)?,
        ratelimit_7d_oi_utilization: r.get(26)?,
        ratelimit_7d_oi_reset: r.get(27)?,
        overage_in_use: r.get(28)?,
        overage_utilization: r.get(29)?,
        estimated_usd: r.get(30)?,
        input_usd: r.get(31)?,
        cache_read_usd: r.get(32)?,
        cache_write_usd: r.get(33)?,
        output_usd: r.get(34)?,
        pricing_version: r.get(35)?,
        thinking_tokens: tokens(36)?,
        web_search_requests: tokens(37)?,
        web_fetch_requests: tokens(38)?,
        stop_reason: r.get(39)?,
        stop_sequence: r.get(40)?,
        served_model: r.get(41)?,
        speed: r.get(42)?,
        inference_geo: r.get(43)?,
        cleared_input_tokens: tokens(44)?,
        refusal_category: r.get(45)?,
        refusal_recommended_model: r.get(46)?,
        selection: r.get(47)?,
        upstream_message_id: r.get(48)?,
    })
}

/// The store's rule: a file a newer build wrote is refused before the schema or a migration runs over it; then the
/// v1 schema, then every migration above the stored version, each in its own transaction.
fn prepare(conn: &mut Connection, path: &Path) -> Result<(), LedgerError> {
    let current = read_version(conn)?;
    let supported = latest_version();
    if current > supported {
        return Err(LedgerError::SchemaTooNew {
            path: path.to_path_buf(),
            found: current,
            supported,
        });
    }
    conn.execute_batch(schema::SCHEMA)?;
    if current == 0 {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
    }
    for (version, sql) in schema::MIGRATIONS {
        if *version <= current.max(1) {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("INSERT INTO meta (key, value) VALUES ('schema_version', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![version.to_string()])?;
        tx.commit()?;
    }
    Ok(())
}

fn latest_version() -> u32 {
    schema::MIGRATIONS.last().map_or(1, |(version, _)| *version)
}

/// A file that has never run `prepare` has no `meta` table yet, which reads as version 0, so the refusal of a
/// newer file runs before `schema::SCHEMA` creates the table.
pub(crate) fn read_version(conn: &Connection) -> Result<u32, LedgerError> {
    let has_meta = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_meta {
        return Ok(0);
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.and_then(|v| v.parse().ok()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::*;

    fn request(id: &str) -> RequestRecord {
        RequestRecord {
            id: id.into(),
            received_at: 1_000,
            finished_at: Some(1_500),
            key_id: Some("k".into()),
            key_generation: 0,
            pool_id: Some("p".into()),
            session_id: Some("s".into()),
            agent_id: Some("main".into()),
            parent_agent_id: None,
            model: "claude-sonnet-5[1m]".into(),
            canonical_model: "claude-sonnet-5".into(),
            stream: true,
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 2,
            request_bytes: Some(10),
            response_bytes: Some(20),
            ttfb_ms: Some(100),
            ttfc_ms: Some(150),
            duration_ms: Some(500),
            error_code: None,
            kept_warm: false,
            bodies_captured: true,
            user_agent: Some("claude-cli/2.1.300 (external, cli)".into()),
            client: Some("claude-code/cli".into()),
            agent_type: Some("Explore".into()),
            prev_tool_ms: Some(1_235),
            turn_origin: Some("human".into()),
            auto_mode: Some("server".into()),
            ..Default::default()
        }
    }

    fn attempt(id: &str, n: u32, account: &str, outcome: &str) -> AttemptRecord {
        AttemptRecord {
            request_id: id.into(),
            attempt: n,
            account_id: account.into(),
            account_kind: "oauth".into(),
            pin_epoch: Some(3),
            started_at: 1_000 + n as i64,
            finished_at: Some(1_100),
            ttfb_ms: Some(100),
            ttfc_ms: None,
            upstream_status: Some(if outcome == "succeeded" { 200 } else { 429 }),
            upstream_request_id: Some("req_up".into()),
            upstream_message_id: (outcome == "succeeded").then(|| "msg_up".into()),
            outcome: outcome.into(),
            cooldown_scope: None,
            cooldown_until: None,
            input_tokens: Some(5),
            output_tokens: Some(7),
            cache_read_tokens: None,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            service_tier: None,
            cache_miss_reason: Some("none".into()),
            client_drift: None,
            ratelimit_5h_utilization: Some(0.25),
            ratelimit_5h_reset: Some(2_000),
            ratelimit_7d_utilization: None,
            ratelimit_7d_reset: None,
            ratelimit_7d_oi_utilization: None,
            ratelimit_7d_oi_reset: None,
            overage_in_use: Some(false),
            overage_utilization: None,
            estimated_usd: Some(0.001),
            input_usd: None,
            cache_read_usd: None,
            cache_write_usd: None,
            output_usd: None,
            pricing_version: Some("2026-09-08".into()),
            thinking_tokens: Some(4),
            web_search_requests: Some(2),
            web_fetch_requests: Some(1),
            stop_reason: Some("refusal".into()),
            stop_sequence: Some("<<stop>>".into()),
            served_model: Some("claude-opus-5".into()),
            speed: Some("standard".into()),
            inference_geo: Some("us".into()),
            cleared_input_tokens: Some(1_200),
            refusal_category: Some("cyber".into()),
            refusal_recommended_model: Some("claude-opus-5".into()),
            selection: Some(
                r#"{"strategy":"pace","tier":[0,1,0],"renewed":false,"moved":null,"skipped":[]}"#
                    .into(),
            ),
        }
    }

    #[tokio::test]
    async fn the_schema_opens_in_wal_mode_with_a_version_and_rows_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("data").join("ledger.sqlite")).unwrap());
        assert_eq!(db.read_blocking(read_version).unwrap(), 29);
        let mode: String = db
            .read_blocking(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode, "wal");
        let bodies = BodyRecord {
            request_id: "r1".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(b"hi".to_vec()),
            request_len_seen: 2,
            response_len_seen: 2,
            expires_at: 9_000,
        };
        db.insert_request(
            &request("r1"),
            &[
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded"),
            ],
            Some(&bodies),
        )
        .unwrap();
        // A second write of the same request replaces rather than fails, so a retried record is not an error.
        db.insert_request(
            &request("r1"),
            &[
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded"),
            ],
            None,
        )
        .unwrap();
        let (row, attempts) = db.request("r1").await.unwrap().unwrap();
        assert_eq!(row, request("r1"));
        assert_eq!(
            attempts,
            vec![
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded")
            ]
        );
        assert_eq!(db.bodies("r1").await.unwrap().unwrap(), bodies);
        assert!(db.request("nope").await.unwrap().is_none());
        db.insert_event(&EventRecord {
            at: 5,
            kind: "ledger_gap".into(),
            detail: Some(r#"{"dropped":3}"#.into()),
            ..Default::default()
        })
        .unwrap();
        db.insert_event(&EventRecord {
            at: 7,
            kind: "cooldown_set".into(),
            account_id: Some("a".into()),
            ..Default::default()
        })
        .unwrap();
        let events = db.events(0, 10, 10).await.unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["ledger_gap", "cooldown_set"]
        );
        assert_eq!(db.events(6, 10, 10).await.unwrap().len(), 1);
        let mode = std::fs::metadata(dir.path().join("data").join("ledger.sqlite"))
            .unwrap()
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn an_in_memory_ledger_serves_reads_and_writes_from_one_connection() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        db.insert_request(&request("r1"), &[], None).unwrap();
        assert!(db.request("r1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_read_runs_off_the_runtime_thread_and_answers_on_it() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let caller = std::thread::current().id();
        let (worker, version) = db
            .read(move |c| Ok((std::thread::current().id(), read_version(c)?)))
            .await
            .unwrap();
        assert_ne!(worker, caller, "the closure ran on a blocking worker");
        assert_eq!(version, 29, "and answered what the connection holds");
    }

    #[tokio::test]
    async fn a_panicked_read_answers_read_cancelled_and_the_next_read_on_the_same_ledger_succeeds()
    {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let panicked = db
            .read(|_conn| -> Result<(), LedgerError> {
                panic!("a read closure that never means to answer");
            })
            .await;
        assert!(matches!(panicked, Err(LedgerError::ReadCancelled)));
        assert_eq!(
            db.read_blocking(read_version).unwrap(),
            29,
            "the poisoned guard recovered rather than turning every later read into ReadCancelled too"
        );
    }

    #[tokio::test]
    async fn a_panic_inside_a_write_closure_leaves_the_writer_usable() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.write(|_| -> Result<(), LedgerError> { panic!("a bug inside the closure") })
        }));
        assert!(poisoned.is_err());
        db.write(|c| {
            c.execute("INSERT INTO meta (key, value) VALUES ('probe', '1')", [])?;
            Ok(())
        })
        .expect("the next write lands");
    }

    #[test]
    fn an_older_build_refuses_a_ledger_a_newer_build_wrote_and_leaves_it_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        LedgerDb::open(&path)
            .unwrap()
            .write(|c| {
                c.execute(
                    "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let refused = LedgerDb::open(&path)
            .err()
            .expect("the newer file is refused");
        assert!(
            matches!(
                refused,
                LedgerError::SchemaTooNew {
                    found: 99,
                    supported: 29,
                    ..
                }
            ),
            "{refused}"
        );
        let message = refused.to_string();
        assert!(
            message.contains("99")
                && message.contains("restore the backup taken before the upgrade"),
            "the refusal names the version it found and the remedy: {message}"
        );
        assert!(
            !message.contains(&path.display().to_string()),
            "the path is the caller's to name, as the start's wrap does: {message}"
        );
        let stored: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "99", "no migration ran over the newer file");
    }

    #[tokio::test]
    async fn a_batch_waits_for_a_purge_slice_holding_the_write_lock_and_lands() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let slice = std::thread::spawn({
            let db = db.clone();
            move || {
                db.purge(|conn| {
                    let tx = begin_write(conn)?;
                    tx.execute("DELETE FROM bodies WHERE expires_at <= 0", [])?;
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    tx.commit()?;
                    Ok(())
                })
            }
        });
        held_rx.recv().unwrap();
        let batch = std::thread::spawn({
            let db = db.clone();
            move || db.insert_request(&request("r1"), &[attempt("r1", 1, "a", "succeeded")], None)
        });
        fake_upstream::stays_for(std::time::Duration::from_millis(200), || {
            !batch.is_finished()
        })
        .await;
        release_tx.send(()).unwrap();
        slice.join().unwrap().unwrap();
        batch
            .join()
            .unwrap()
            .expect("the batch waited for the slice's commit rather than failing at once");
        assert!(
            db.request("r1").await.unwrap().is_some(),
            "and its row is there"
        );
    }
}
