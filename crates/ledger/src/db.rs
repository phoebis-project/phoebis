//! The ledger's SQLite file: one writer connection the ledger task owns through `write`, one read-only
//! connection the admin routes share through `read`, WAL so the two never wait on each other.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};

use crate::query::request_by_id;
use crate::record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord,
    RequestRecord,
};
use crate::{schema, LedgerError};

const LOCK_POISONED: &str = "ledger connection mutex poisoned by a prior panic";

pub struct LedgerDb {
    writer: Mutex<Connection>,
    /// `None` for an in-memory ledger, whose one connection serves both roles.
    reader: Option<Mutex<Connection>>,
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
        writer.pragma_update(None, "journal_mode", "WAL")?;
        // A crash loses at most the last transactions before a checkpoint, which the spec accepts for a
        // best-effort ledger; `FULL` would fsync on every request.
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        prepare(&mut writer)?;
        let reader = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(LedgerDb {
            writer: Mutex::new(writer),
            reader: Some(Mutex::new(reader)),
        })
    }

    pub fn open_in_memory() -> Result<LedgerDb, LedgerError> {
        let mut writer = Connection::open_in_memory()?;
        prepare(&mut writer)?;
        Ok(LedgerDb {
            writer: Mutex::new(writer),
            reader: None,
        })
    }

    pub fn schema_version(&self) -> Result<u32, LedgerError> {
        self.read(read_version)
    }

    pub fn write<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        let mut conn = self.writer.lock().expect(LOCK_POISONED);
        f(&mut conn)
    }

    pub fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        match &self.reader {
            Some(reader) => f(&reader.lock().expect(LOCK_POISONED)),
            None => f(&self.writer.lock().expect(LOCK_POISONED)),
        }
    }

    pub fn insert_request(
        &self,
        request: &RequestRecord,
        attempts: &[AttemptRecord],
        bodies: Option<&BodyRecord>,
    ) -> Result<(), LedgerError> {
        self.write(|conn| {
            let tx = conn.transaction()?;
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

    pub fn insert_event(&self, event: &EventRecord) -> Result<(), LedgerError> {
        self.write(|conn| {
            insert_event_in(conn, event)?;
            Ok(())
        })
    }

    pub fn request(
        &self,
        id: &str,
    ) -> Result<Option<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
        self.read(|conn| request_by_id(conn, id))
    }

    pub fn bodies(&self, id: &str) -> Result<Option<BodyRecord>, LedgerError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT request_id, request_body, response_body, expires_at FROM bodies WHERE request_id = ?1",
                    params![id],
                    |r| Ok(BodyRecord { request_id: r.get(0)?, request_body: r.get(1)?, response_body: r.get(2)?, expires_at: r.get(3)? }),
                )
                .optional()?)
        })
    }

    pub fn events(
        &self,
        from: i64,
        to: i64,
        limit: usize,
    ) -> Result<Vec<EventRecord>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT at, kind, account_id, key_id, session_id, model, actor, detail FROM events WHERE at >= ?1 AND at < ?2 ORDER BY at, rowid LIMIT ?3")?;
            let rows = stmt.query_map(params![from, to, limit as i64], |r| {
                Ok(EventRecord { at: r.get(0)?, kind: r.get(1)?, account_id: r.get(2)?, key_id: r.get(3)?, session_id: r.get(4)?, model: r.get(5)?, actor: r.get(6)?, detail: r.get(7)? })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn insert_samples(
        &self,
        quota: &[QuotaSampleRecord],
        extra: Option<&ExtraUsageSampleRecord>,
    ) -> Result<(), LedgerError> {
        self.write(|conn| insert_samples_in(conn, quota, extra))
    }

    pub fn quota_samples(
        &self,
        account_id: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<QuotaSampleRecord>, LedgerError> {
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT account_id, bucket, window, utilization, reset_at, source FROM quota_samples WHERE account_id = ?1 AND bucket >= ?2 AND bucket < ?3 ORDER BY window, bucket")?;
            let rows = stmt.query_map(params![account_id, from, to], |r| {
                Ok(QuotaSampleRecord {
                    account_id: r.get(0)?,
                    bucket: r.get(1)?,
                    window: r.get(2)?,
                    utilization: r.get(3)?,
                    reset_at: r.get(4)?,
                    source: r.get(5)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn extra_usage_samples(
        &self,
        account_id: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
        self.read(|conn| {
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
        })
    }
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
                "{verb} INTO quota_samples (account_id, bucket, window, utilization, reset_at, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            params![q.account_id, q.bucket, q.window, q.utilization, q.reset_at, q.source],
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
    tx.execute(
        "INSERT OR REPLACE INTO requests (id, received_at, finished_at, key_id, pool_id, session_id, agent_id, parent_agent_id, model, canonical_model, stream, kind, origin, status, outcome, attempts, request_bytes, response_bytes, ttfb_ms, ttfc_ms, duration_ms, error_code, kept_warm, bodies_captured) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        params![
            request.id, request.received_at, request.finished_at, request.key_id, request.pool_id, request.session_id, request.agent_id,
            request.parent_agent_id, request.model, request.canonical_model, request.stream, request.kind, request.origin,
            request.status.map(i64::from), request.outcome, request.attempts, request.request_bytes.map(|n| n as i64),
            request.response_bytes.map(|n| n as i64), request.ttfb_ms, request.ttfc_ms, request.duration_ms, request.error_code,
            request.kept_warm, request.bodies_captured,
        ],
    )?;
    tx.execute(
        "DELETE FROM attempts WHERE request_id = ?1",
        params![request.id],
    )?;
    for a in attempts {
        tx.execute(
            "INSERT INTO attempts (request_id, attempt, account_id, account_kind, pin_epoch, started_at, finished_at, ttfb_ms, ttfc_ms, upstream_status, upstream_request_id, outcome, cooldown_scope, cooldown_until, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, service_tier, cache_miss_reason, client_drift, ratelimit_5h_utilization, ratelimit_5h_reset, ratelimit_7d_utilization, ratelimit_7d_reset, ratelimit_7d_oi_utilization, ratelimit_7d_oi_reset, overage_in_use, overage_utilization, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, pricing_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36)",
            params![
                a.request_id, a.attempt, a.account_id, a.account_kind, a.pin_epoch.map(|n| n as i64), a.started_at, a.finished_at,
                a.ttfb_ms, a.ttfc_ms, a.upstream_status.map(i64::from), a.upstream_request_id, a.outcome, a.cooldown_scope, a.cooldown_until,
                a.input_tokens.map(|n| n as i64), a.output_tokens.map(|n| n as i64), a.cache_read_tokens.map(|n| n as i64),
                a.cache_write_5m_tokens.map(|n| n as i64), a.cache_write_1h_tokens.map(|n| n as i64), a.service_tier, a.cache_miss_reason,
                a.client_drift, a.ratelimit_5h_utilization, a.ratelimit_5h_reset, a.ratelimit_7d_utilization, a.ratelimit_7d_reset,
                a.ratelimit_7d_oi_utilization, a.ratelimit_7d_oi_reset, a.overage_in_use, a.overage_utilization, a.estimated_usd,
                a.input_usd, a.cache_read_usd, a.cache_write_usd, a.output_usd, a.pricing_version,
            ],
        )?;
    }
    if let Some(b) = bodies {
        tx.execute(
            "INSERT OR REPLACE INTO bodies (request_id, request_body, response_body, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![b.request_id, b.request_body, b.response_body, b.expires_at],
        )?;
    }
    Ok(previous)
}

pub(crate) fn insert_event_in(conn: &Connection, e: &EventRecord) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO events (at, kind, account_id, key_id, session_id, model, actor, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![e.at, e.kind, e.account_id, e.key_id, e.session_id, e.model, e.actor, e.detail],
    )?;
    Ok(())
}

pub(crate) const REQUEST_SELECT: &str = "SELECT id, received_at, finished_at, key_id, pool_id, session_id, agent_id, parent_agent_id, model, canonical_model, stream, kind, origin, status, outcome, attempts, request_bytes, response_bytes, ttfb_ms, ttfc_ms, duration_ms, error_code, kept_warm, bodies_captured FROM requests";
pub(crate) const ATTEMPT_SELECT: &str = "SELECT request_id, attempt, account_id, account_kind, pin_epoch, started_at, finished_at, ttfb_ms, ttfc_ms, upstream_status, upstream_request_id, outcome, cooldown_scope, cooldown_until, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, service_tier, cache_miss_reason, client_drift, ratelimit_5h_utilization, ratelimit_5h_reset, ratelimit_7d_utilization, ratelimit_7d_reset, ratelimit_7d_oi_utilization, ratelimit_7d_oi_reset, overage_in_use, overage_utilization, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, pricing_version FROM attempts";

pub(crate) fn row_to_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRecord> {
    Ok(RequestRecord {
        id: r.get(0)?,
        received_at: r.get(1)?,
        finished_at: r.get(2)?,
        key_id: r.get(3)?,
        pool_id: r.get(4)?,
        session_id: r.get(5)?,
        agent_id: r.get(6)?,
        parent_agent_id: r.get(7)?,
        model: r.get(8)?,
        canonical_model: r.get(9)?,
        stream: r.get(10)?,
        kind: r.get(11)?,
        origin: r.get(12)?,
        status: r.get::<_, Option<i64>>(13)?.map(|n| n as u16),
        outcome: r.get(14)?,
        attempts: r.get(15)?,
        request_bytes: r.get::<_, Option<i64>>(16)?.map(|n| n as u64),
        response_bytes: r.get::<_, Option<i64>>(17)?.map(|n| n as u64),
        ttfb_ms: r.get(18)?,
        ttfc_ms: r.get(19)?,
        duration_ms: r.get(20)?,
        error_code: r.get(21)?,
        kept_warm: r.get(22)?,
        bodies_captured: r.get(23)?,
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
    })
}

/// The store's rule: the v1 schema, then every migration above the stored version, each in its own transaction.
fn prepare(conn: &mut Connection) -> Result<(), LedgerError> {
    conn.execute_batch(schema::SCHEMA)?;
    let current = read_version(conn)?;
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

fn read_version(conn: &Connection) -> Result<u32, LedgerError> {
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
        }
    }

    #[test]
    fn the_schema_opens_in_wal_mode_with_a_version_and_rows_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("data").join("ledger.sqlite")).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        let mode: String = db
            .read(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode, "wal");
        let bodies = BodyRecord {
            request_id: "r1".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(b"hi".to_vec()),
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
        let (row, attempts) = db.request("r1").unwrap().unwrap();
        assert_eq!(row, request("r1"));
        assert_eq!(
            attempts,
            vec![
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded")
            ]
        );
        assert_eq!(db.bodies("r1").unwrap().unwrap(), bodies);
        assert!(db.request("nope").unwrap().is_none());
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
        let events = db.events(0, 10, 10).unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["ledger_gap", "cooldown_set"]
        );
        assert_eq!(db.events(6, 10, 10).unwrap().len(), 1);
        let mode = std::fs::metadata(dir.path().join("data").join("ledger.sqlite"))
            .unwrap()
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
    }

    #[test]
    fn an_in_memory_ledger_serves_reads_and_writes_from_one_connection() {
        let db = LedgerDb::open_in_memory().unwrap();
        db.insert_request(&request("r1"), &[], None).unwrap();
        assert!(db.request("r1").unwrap().is_some());
    }
}
