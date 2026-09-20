//! Alert rules, the events they open, and every delivery attempt, in the registry so the UI edits rules where it
//! edits pools and keys. The webhook secret is sealed like an API-key secret and never read back whole by a
//! route; `has_secret` is what the UI learns.

use rusqlite::{params, OptionalExtension};
use url::Url;

use crate::registry::record_change;
use crate::{crypto, Secret, Store, StoreError, LOCK_POISONED};

#[derive(Clone, Debug, PartialEq)]
pub struct AlertRuleRow {
    pub id: String,
    pub kind: String,
    pub scope_type: String,
    pub scope_id: Option<String>,
    pub threshold: Option<f64>,
    pub window: Option<String>,
    pub webhook_url: String,
    pub has_secret: bool,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlertEventRow {
    pub id: String,
    pub rule_id: String,
    pub kind: String,
    pub subject: String,
    pub state: String,
    pub opened_at: i64,
    pub resolved_at: Option<i64>,
    pub detail_json: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlertDeliveryRow {
    pub id: u64,
    pub event_id: String,
    pub state: String,
    pub attempt: u32,
    pub at: i64,
    pub status: Option<u16>,
    pub error: Option<String>,
}

fn secret_aad(rule_id: &str) -> String {
    format!("{rule_id}:alert_secret")
}

const RULE_COLUMNS: &str = "id, kind, scope_type, scope_id, threshold, window, webhook_url, secret_sealed IS NOT NULL, enabled, created_at, updated_at";
const EVENT_COLUMNS: &str =
    "id, rule_id, kind, subject, state, opened_at, resolved_at, detail_json";

fn row_to_rule(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlertRuleRow> {
    Ok(AlertRuleRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        scope_type: r.get(2)?,
        scope_id: r.get(3)?,
        threshold: r.get(4)?,
        window: r.get(5)?,
        webhook_url: r.get(6)?,
        has_secret: r.get(7)?,
        enabled: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlertEventRow> {
    Ok(AlertEventRow {
        id: r.get(0)?,
        rule_id: r.get(1)?,
        kind: r.get(2)?,
        subject: r.get(3)?,
        state: r.get(4)?,
        opened_at: r.get(5)?,
        resolved_at: r.get(6)?,
        detail_json: r.get(7)?,
    })
}

/// Where a rule posts, with the port when it names one. A webhook URL may carry a credential in its userinfo,
/// its path or its query, and the viewer role reads both a registry change and the rules route, so both stop
/// at the destination and go no further. A URL that does not parse is answered as absent rather than whole.
pub fn webhook_host(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

fn rule_diff(row: &AlertRuleRow) -> String {
    serde_json::json!({
        "kind": row.kind,
        "scope_type": row.scope_type,
        "scope_id": row.scope_id,
        "threshold": row.threshold,
        "window": row.window,
        "webhook_host": webhook_host(&row.webhook_url),
        "has_secret": row.has_secret,
        "enabled": row.enabled,
    })
    .to_string()
}

impl Store {
    pub fn alert_rules(&self) -> Result<Vec<AlertRuleRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!(
            "SELECT {RULE_COLUMNS} FROM alert_rules ORDER BY webhook_url, id"
        ))?;
        let rows = stmt.query_map([], row_to_rule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    pub fn alert_rule(&self, id: &str) -> Result<Option<AlertRuleRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!("SELECT {RULE_COLUMNS} FROM alert_rules WHERE id = ?1"),
            params![id],
            row_to_rule,
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// `secret`: `None` leaves the stored secret alone, `Some(None)` clears it, `Some(Some(s))` seals `s`.
    pub fn upsert_alert_rule(
        &self,
        row: &AlertRuleRow,
        secret: Option<Option<&Secret>>,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM alert_rules WHERE id = ?1)",
            params![row.id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO alert_rules (id, kind, scope_type, scope_id, threshold, window, webhook_url, enabled, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, scope_type = excluded.scope_type, scope_id = excluded.scope_id, threshold = excluded.threshold, window = excluded.window, webhook_url = excluded.webhook_url, enabled = excluded.enabled, updated_at = excluded.updated_at",
            params![row.id, row.kind, row.scope_type, row.scope_id, row.threshold, row.window, row.webhook_url, row.enabled, row.created_at, row.updated_at],
        )?;
        match secret {
            None => {}
            Some(None) => {
                tx.execute(
                    "UPDATE alert_rules SET secret_sealed = NULL, secret_nonce = NULL WHERE id = ?1",
                    params![row.id],
                )?;
            }
            Some(Some(secret)) => {
                let (nonce, sealed) =
                    crypto::seal(&self.key, &secret_aad(&row.id), secret.expose().as_bytes())?;
                tx.execute(
                    "UPDATE alert_rules SET secret_sealed = ?2, secret_nonce = ?3 WHERE id = ?1",
                    params![row.id, sealed, nonce],
                )?;
            }
        }
        let has_secret: bool = tx.query_row(
            "SELECT secret_sealed IS NOT NULL FROM alert_rules WHERE id = ?1",
            params![row.id],
            |r| r.get(0),
        )?;
        record_change(
            &tx,
            row.updated_at,
            actor,
            if existed {
                "alert_rule_updated"
            } else {
                "alert_rule_created"
            },
            &row.id,
            &rule_diff(&AlertRuleRow {
                has_secret,
                ..row.clone()
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_alert_rule(&self, id: &str, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.execute("DELETE FROM alert_rules WHERE id = ?1", params![id])? == 0 {
            return Err(StoreError::AlertRuleNotFound(id.into()));
        }
        record_change(
            &tx,
            jiff::Timestamp::now().as_millisecond(),
            actor,
            "alert_rule_deleted",
            id,
            "{}",
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn alert_rule_secret(&self, id: &str) -> Result<Option<Secret>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        // A rule with no secret and a missing rule both answer `None`, so the NULL halves are filtered here
        // rather than unpacked below.
        let row: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row(
                "SELECT secret_sealed, secret_nonce FROM alert_rules \
                 WHERE id = ?1 AND secret_sealed IS NOT NULL AND secret_nonce IS NOT NULL",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((sealed, nonce)) = row else {
            return Ok(None);
        };
        let plaintext = crypto::open(&self.key, &secret_aad(id), &nonce, &sealed)?;
        Ok(Some(Secret::new(
            &String::from_utf8(plaintext).map_err(|_| StoreError::Crypto)?,
        )))
    }

    /// Newest first, optionally narrowed to one state.
    pub fn alert_events(
        &self,
        state: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AlertEventRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM alert_events WHERE (?1 IS NULL OR state = ?1) ORDER BY opened_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![state, limit as i64], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    pub fn alert_event(&self, id: &str) -> Result<Option<AlertEventRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!("SELECT {EVENT_COLUMNS} FROM alert_events WHERE id = ?1"),
            params![id],
            row_to_event,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn open_alert_event(
        &self,
        rule_id: &str,
        subject: &str,
    ) -> Result<Option<AlertEventRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM alert_events WHERE rule_id = ?1 AND subject = ?2 AND state = 'open'"
            ),
            params![rule_id, subject],
            row_to_event,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn open_alert_events(&self) -> Result<Vec<AlertEventRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM alert_events WHERE state = 'open' ORDER BY opened_at, id"
        ))?;
        let rows = stmt.query_map([], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// `true` when the row landed, `false` when another open event already holds the rule and subject, so two
    /// concurrent openers cannot both believe they opened it. Every other conflict is an error, a reused `id`
    /// above all: an opener that read its own earlier row as someone else's would drop the alert in silence.
    pub fn insert_alert_event(&self, row: &AlertEventRow) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        // The unique open index is the check: the insert either lands or reports the conflict, in one statement.
        // The target repeats that index's predicate, which is how SQLite names a partial index, so only the
        // rule-and-subject clash is swallowed.
        let landed = conn.execute(
            "INSERT INTO alert_events (id, rule_id, kind, subject, state, opened_at, resolved_at, detail_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(rule_id, subject) WHERE state = 'open' DO NOTHING",
            params![row.id, row.rule_id, row.kind, row.subject, row.state, row.opened_at, row.resolved_at, row.detail_json],
        )?;
        Ok(landed > 0)
    }

    /// `false` when the event is already resolved: what a resolved event's delivery carried is what it said,
    /// and a later writer may not rewrite it.
    pub fn update_alert_event_detail(
        &self,
        id: &str,
        detail_json: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute(
            "UPDATE alert_events SET detail_json = ?2 WHERE id = ?1 AND state = 'open'",
            params![id, detail_json],
        )? > 0)
    }

    /// `false` when the event is already resolved, so a second resolver does not move the timestamp.
    pub fn resolve_alert_event(&self, id: &str, at: i64) -> Result<bool, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        if self
            .fail_next_alert_resolve
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Io(std::io::Error::other(
                "injected resolve failure",
            )));
        }
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute(
            "UPDATE alert_events SET state = 'resolved', resolved_at = ?2 WHERE id = ?1 AND state = 'open'",
            params![id, at],
        )? > 0)
    }

    pub fn insert_alert_delivery(&self, row: &AlertDeliveryRow) -> Result<u64, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "INSERT INTO alert_deliveries (event_id, state, attempt, at, status, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![row.event_id, row.state, row.attempt, row.at, row.status, row.error],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn alert_deliveries(&self, event_id: &str) -> Result<Vec<AlertDeliveryRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(
            "SELECT id, event_id, state, attempt, at, status, error FROM alert_deliveries WHERE event_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![event_id], |r| {
            Ok(AlertDeliveryRow {
                id: r.get::<_, i64>(0)? as u64,
                event_id: r.get(1)?,
                state: r.get(2)?,
                attempt: r.get::<_, i64>(3)? as u32,
                at: r.get(4)?,
                status: r.get::<_, Option<i64>>(5)?.map(|s| s as u16),
                error: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// Resolved events older than the mark go, deliveries with them; an open event is never purged. The
    /// question an `ask_user_question` detail carries is prompt content, so it follows the body it was
    /// extracted from: past that body's own `question_expires_at`, or past `questions_before` for a detail
    /// that carries no deadline, the key is nulled and the event stays. Answers how many events were deleted.
    pub fn purge_alert_events_before(
        &self,
        resolved_before: i64,
        questions_before: i64,
        now: i64,
    ) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let purged = conn.execute(
            "DELETE FROM alert_events WHERE state = 'resolved' AND resolved_at < ?1",
            params![resolved_before],
        )?;
        // `json_valid` guards the json calls: a detail this process did not write would otherwise abort the
        // statement and take the purge with it.
        conn.execute(
            "UPDATE alert_events SET detail_json = json_set(detail_json, '$.question', NULL) \
             WHERE kind = 'ask_user_question' AND json_valid(detail_json) \
               AND json_extract(detail_json, '$.question') IS NOT NULL \
               AND CASE WHEN json_extract(detail_json, '$.question_expires_at') IS NULL \
                        THEN opened_at < ?1 \
                        ELSE json_extract(detail_json, '$.question_expires_at') <= ?2 END",
            params![questions_before, now],
        )?;
        Ok(purged)
    }

    /// Every BLOB and TEXT column of the named table as bytes, for tests that assert plaintext is absent.
    /// The name picks a literal statement rather than being interpolated, so a caller cannot steer the query.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_dump_table(&self, table: &str) -> Result<Vec<Vec<u8>>, StoreError> {
        let sql = match table {
            "alert_rules" => "SELECT webhook_url, secret_sealed, secret_nonce FROM alert_rules",
            _ => return Ok(Vec::new()),
        };
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(sql)?;
        let count = stmt.column_count();
        let rows = stmt.query_map([], |r| {
            (0..count)
                .map(|i| {
                    r.get::<_, Option<Vec<u8>>>(i)
                        .or_else(|_| r.get::<_, String>(i).map(|s| Some(s.into_bytes())))
                        .map(|v| v.unwrap_or_default())
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect())
    }
}
