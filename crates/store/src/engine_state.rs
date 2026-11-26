//! The engine's state on disk: the session engine's rows of its sessions and of their agents, the cache warming's row
//! of each agent with the retained prompt beside it, the facts that a companion reports of a session and of its
//! agents, and the idle-age counters. Each component owns its payloads' shape, and this side stores each payload with
//! the version it was written under and seals the prompt and the reported facts, which are the user's own text, as the
//! credentials are sealed.

use rusqlite::params;

use crate::{crypto, Store, StoreError, LOCK_POISONED};

text_enum! {
    /// The kind of one stored set of reported facts, the word of its row and of the registration's list.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub enum FactsKind {
        Agents => "agents",
        Project => "project",
        Title => "title",
    }
}

/// The key of one stored set. `agent_id` is empty for the two session kinds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FactsKey {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub kind: FactsKind,
    pub agent_id: String,
}

/// One stored set to write: the value in clear, which the save seals, and the instant of its session's last report.
#[derive(Debug, PartialEq)]
pub struct FactsRow {
    pub key: FactsKey,
    pub record_version: u32,
    pub value: Vec<u8>,
    /// The instant of the report that filed the set, which a save moves to the session's last report where its batch
    /// carries that report.
    pub seen_at: i64,
}

/// One stored set as a read answers it. `value` is `None` for a row that does not open.
#[derive(Debug)]
pub struct StoredFacts {
    pub key: FactsKey,
    pub record_version: u32,
    pub value: Option<Vec<u8>>,
}

/// One session row: its identity, and the session engine's versioned payload of the session's fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionSessionRow {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub record_version: u32,
    pub payload: String,
    pub updated_at: i64,
}

/// One agent row of the session engine: its identity, and the session engine's versioned payload of the agent's
/// fields. The cache warming's row of the same agent stands in `KeepaliveAgentRow`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionAgentRow {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub agent_id: String,
    pub record_version: u32,
    pub payload: String,
    pub updated_at: i64,
}

/// What a flush does with the stored recipe body: nothing (the body is as saved), clear it (the recipe ended), or
/// seal a new one. A body is sealed only when it changed since the last acknowledged save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyWrite {
    Keep,
    Clear,
    Seal(Vec<u8>),
}

/// One agent row of the cache warming: its identity, the warming's versioned payload of its record of the agent, and
/// the retained prompt the probe replays beside it, sealed only at rest. `body` is the plaintext a load opened, `None`
/// for an agent that retains nothing or whose stored body no longer opens. A save reads `body_write` alone, and a load
/// answers it as `Keep`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepaliveAgentRow {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub agent_id: String,
    pub record_version: u32,
    pub payload: String,
    pub body: Option<Vec<u8>>,
    pub body_write: BodyWrite,
    /// The header block the probe sends, as the engine's JSON.
    pub headers: String,
    pub body_size: usize,
    pub dispatched_ms: Option<i64>,
    pub fast: bool,
    pub updated_at: i64,
}

/// One idle-age bucket of one pool on one UTC day, the day as the engine keys its counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleReturnsRow {
    pub pool_id: String,
    pub day: i64,
    pub bucket: u32,
    pub reached: u64,
    pub returned: u64,
}

/// What one flush writes. A removed session takes every agent row filed under it in both agent tables, and a removed
/// agent takes its row of each, so a session or an agent the engine dropped leaves nothing behind for a load to
/// restore. A removed session's stored sets of reported facts stay, since each leaves by a removal of its own, by the
/// delete of the sets' key or by the retention pass's cut.
#[derive(Debug, Default, PartialEq)]
pub struct EngineBatch {
    pub sessions: Vec<CompanionSessionRow>,
    /// `((key_id, key_generation, session_id), last_report_at)`: the instant of each session's last report, which the
    /// save stamps on each stored set of the session.
    pub session_reports: Vec<((String, i64, String), i64)>,
    pub agents: Vec<CompanionAgentRow>,
    pub warming_agents: Vec<KeepaliveAgentRow>,
    /// `(key_id, key_generation, session_id)`.
    pub removed_sessions: Vec<(String, i64, String)>,
    /// `(key_id, key_generation, session_id, agent_id)`.
    pub removed_agents: Vec<(String, i64, String, String)>,
    pub counters: Vec<IdleReturnsRow>,
    /// The counters' rows of a day before this one leave in the same transaction, `None` keeping every day. A batch
    /// that carries the purge alone is not empty, so a flush with no other row still reaches the delete.
    pub purge_counters_before: Option<i64>,
    pub facts: Vec<FactsRow>,
    pub removed_facts: Vec<FactsKey>,
    /// `(key_id, key_generation, session_id, kind)`: each stored set of the kind under the session leaves, an agent's
    /// set of an agent that the engine holds no record of among them.
    pub removed_fact_kinds: Vec<(String, i64, String, FactsKind)>,
}

impl EngineBatch {
    pub fn is_empty(&self) -> bool {
        *self == EngineBatch::default()
    }
}

/// Every row a load answers, each list in its table's key order.
pub struct EngineState {
    pub sessions: Vec<CompanionSessionRow>,
    pub agents: Vec<CompanionAgentRow>,
    pub warming_agents: Vec<KeepaliveAgentRow>,
    pub counters: Vec<IdleReturnsRow>,
}

/// Each part as `<byte length>:<bytes>`, in order. A colon inside a part cannot move a boundary (`3:s:x1:y` against
/// `1:s3:x:y`), so a value sealed under one row's parts fails to open under any other row's.
fn length_prefixed(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| format!("{}:{part}", part.len()))
        .collect()
}

/// The identity tuple as length-prefixed text, each component `<byte length>:<bytes>` in this order: `key_id`,
/// `key_generation` in decimal, `session_id`, `agent_id`, the literal `keepalive`. `crypto::seal` takes the string as
/// it takes the credentials' AAD.
pub fn keepalive_aad(
    key_id: &str,
    key_generation: i64,
    session_id: &str,
    agent_id: &str,
) -> String {
    length_prefixed(&[
        key_id,
        &key_generation.to_string(),
        session_id,
        agent_id,
        "keepalive",
    ])
}

/// A stored set's key as length-prefixed text, each component `<byte length>:<bytes>` as `keepalive_aad` writes
/// them, in this order: `key_id`, `key_generation` in decimal, `session_id`, the kind's word, `agent_id`, the literal
/// `facts`. A sealed set moved to another row of the table, or into a recipe body's column, fails to open.
pub(crate) fn facts_aad(key: &FactsKey) -> String {
    length_prefixed(&[
        &key.key_id,
        &key.key_generation.to_string(),
        &key.session_id,
        key.kind.as_str(),
        &key.agent_id,
        "facts",
    ])
}

impl Store {
    /// One transaction of upserts and deletes, each new body sealed under the storage key with the AAD `keepalive_aad`
    /// and each set of reported facts with `facts_aad`. The removals land first, the leaves of a kind ahead of the
    /// leaves of a row. A batch names a set in both halves where the engine filed it behind the leave of its kind, and
    /// such a set stands, since the leaves run ahead of the writes. The engine names no other row in both halves of a
    /// batch. A removed session deletes no set of reported facts. A warming row that keeps its body leaves the stored
    /// blob and nonce as they are, and one that clears it leaves neither. Each entry of `session_reports` stamps its
    /// instant on each stored set of its session, a set that the same batch writes among them. A set is written only
    /// under a key that the registry holds with the set's generation, and a set that this condition refuses fails no
    /// save.
    pub fn save_engine_state(&self, batch: &EngineBatch) -> Result<(), StoreError> {
        // Sealed before the connection is taken, so a large batch holds no other writer behind the cipher.
        let mut sealed_facts = Vec::with_capacity(batch.facts.len());
        for row in &batch.facts {
            sealed_facts.push(crypto::seal(&self.key, &facts_aad(&row.key), &row.value)?);
        }
        let mut sealed = Vec::with_capacity(batch.warming_agents.len());
        for row in &batch.warming_agents {
            sealed.push(match &row.body_write {
                BodyWrite::Seal(body) => {
                    let aad = keepalive_aad(
                        &row.key_id,
                        row.key_generation,
                        &row.session_id,
                        &row.agent_id,
                    );
                    let (nonce, blob) = crypto::seal(&self.key, &aad, body)?;
                    (Some(blob), Some(nonce))
                }
                BodyWrite::Keep | BodyWrite::Clear => (None, None),
            });
        }
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut sessions = tx.prepare_cached(
                "DELETE FROM companion_sessions WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3",
            )?;
            let mut agents = tx.prepare_cached(
                "DELETE FROM companion_agents WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3",
            )?;
            let mut warming_agents = tx.prepare_cached(
                "DELETE FROM keepalive_agents WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3",
            )?;
            for (key_id, key_generation, session_id) in &batch.removed_sessions {
                sessions.execute(params![key_id, key_generation, session_id])?;
                agents.execute(params![key_id, key_generation, session_id])?;
                warming_agents.execute(params![key_id, key_generation, session_id])?;
            }
        }
        {
            let mut agents = tx.prepare_cached(
                "DELETE FROM companion_agents \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND agent_id = ?4",
            )?;
            let mut warming_agents = tx.prepare_cached(
                "DELETE FROM keepalive_agents \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND agent_id = ?4",
            )?;
            for (key_id, key_generation, session_id, agent_id) in &batch.removed_agents {
                agents.execute(params![key_id, key_generation, session_id, agent_id])?;
                warming_agents.execute(params![key_id, key_generation, session_id, agent_id])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "DELETE FROM companion_facts \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND kind = ?4",
            )?;
            for (key_id, key_generation, session_id, kind) in &batch.removed_fact_kinds {
                stmt.execute(params![key_id, key_generation, session_id, kind])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "DELETE FROM companion_facts \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND kind = ?4 AND agent_id = ?5",
            )?;
            for key in &batch.removed_facts {
                stmt.execute(params![
                    key.key_id,
                    key.key_generation,
                    key.session_id,
                    key.kind,
                    key.agent_id
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO companion_sessions \
                 (key_id, key_generation, session_id, record_version, payload, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(key_id, key_generation, session_id) DO UPDATE SET \
                 record_version = excluded.record_version, payload = excluded.payload, \
                 updated_at = excluded.updated_at",
            )?;
            for row in &batch.sessions {
                stmt.execute(params![
                    row.key_id,
                    row.key_generation,
                    row.session_id,
                    row.record_version,
                    row.payload,
                    row.updated_at
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO companion_agents \
                 (key_id, key_generation, session_id, agent_id, record_version, payload, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(key_id, key_generation, session_id, agent_id) DO UPDATE SET \
                 record_version = excluded.record_version, payload = excluded.payload, \
                 updated_at = excluded.updated_at",
            )?;
            for row in &batch.agents {
                stmt.execute(params![
                    row.key_id,
                    row.key_generation,
                    row.session_id,
                    row.agent_id,
                    row.record_version,
                    row.payload,
                    row.updated_at
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO keepalive_agents \
                 (key_id, key_generation, session_id, agent_id, record_version, payload, body_sealed, body_nonce, \
                  headers, body_size, dispatched_ms, fast, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                 ON CONFLICT(key_id, key_generation, session_id, agent_id) DO UPDATE SET \
                 record_version = excluded.record_version, payload = excluded.payload, \
                 body_sealed = CASE WHEN ?14 THEN keepalive_agents.body_sealed ELSE excluded.body_sealed END, \
                 body_nonce = CASE WHEN ?14 THEN keepalive_agents.body_nonce ELSE excluded.body_nonce END, \
                 headers = excluded.headers, \
                 body_size = excluded.body_size, dispatched_ms = excluded.dispatched_ms, fast = excluded.fast, \
                 updated_at = excluded.updated_at",
            )?;
            for (row, (blob, nonce)) in batch.warming_agents.iter().zip(&sealed) {
                stmt.execute(params![
                    row.key_id,
                    row.key_generation,
                    row.session_id,
                    row.agent_id,
                    row.record_version,
                    row.payload,
                    blob,
                    nonce,
                    row.headers,
                    // A cleared body leaves no bytes to size, whatever the row carried.
                    if row.body_write == BodyWrite::Clear {
                        0
                    } else {
                        row.body_size as i64
                    },
                    row.dispatched_ms,
                    row.fast,
                    row.updated_at,
                    row.body_write == BodyWrite::Keep
                ])?;
            }
        }
        {
            // A save whose batch was read ahead of a key's delete would write the deleted generation's set behind it,
            // so the condition reads the registry inside this transaction.
            let mut stmt = tx.prepare_cached(
                "INSERT INTO companion_facts \
                 (key_id, key_generation, session_id, kind, agent_id, record_version, facts_sealed, facts_nonce, \
                  seen_at) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9 \
                 WHERE EXISTS (SELECT 1 FROM api_keys WHERE id = ?1 AND created_at = ?2) \
                 ON CONFLICT(key_id, key_generation, session_id, kind, agent_id) DO UPDATE SET \
                 record_version = excluded.record_version, facts_sealed = excluded.facts_sealed, \
                 facts_nonce = excluded.facts_nonce, seen_at = excluded.seen_at",
            )?;
            for (row, (nonce, blob)) in batch.facts.iter().zip(&sealed_facts) {
                stmt.execute(params![
                    row.key.key_id,
                    row.key.key_generation,
                    row.key.session_id,
                    row.key.kind,
                    row.key.agent_id,
                    row.record_version,
                    blob,
                    nonce,
                    row.seen_at
                ])?;
            }
        }
        // Behind the sets' upserts, since an upsert writes its own instant over the stamp.
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE companion_facts SET seen_at = ?4 \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND seen_at <> ?4",
            )?;
            for ((key_id, key_generation, session_id), seen_at) in &batch.session_reports {
                stmt.execute(params![key_id, key_generation, session_id, seen_at])?;
            }
        }
        if let Some(day) = batch.purge_counters_before {
            tx.execute(
                "DELETE FROM keepalive_idle_returns WHERE day < ?1",
                params![day],
            )?;
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO keepalive_idle_returns (pool_id, day, bucket, reached, returned) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(pool_id, day, bucket) DO UPDATE SET \
                 reached = excluded.reached, returned = excluded.returned",
            )?;
            for row in &batch.counters {
                stmt.execute(params![
                    row.pool_id,
                    row.day,
                    row.bucket,
                    row.reached as i64,
                    row.returned as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every row. A body that fails to open comes back as `body: None`, and the caller drops that recipe.
    pub fn load_engine_state(&self) -> Result<EngineState, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let sessions = conn
            .prepare(
                "SELECT key_id, key_generation, session_id, record_version, payload, updated_at \
                 FROM companion_sessions ORDER BY key_id, key_generation, session_id",
            )?
            .query_map([], |r| {
                Ok(CompanionSessionRow {
                    key_id: r.get(0)?,
                    key_generation: r.get(1)?,
                    session_id: r.get(2)?,
                    record_version: r.get(3)?,
                    payload: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let agents = conn
            .prepare(
                "SELECT key_id, key_generation, session_id, agent_id, record_version, payload, updated_at \
                 FROM companion_agents ORDER BY key_id, key_generation, session_id, agent_id",
            )?
            .query_map([], |r| {
                Ok(CompanionAgentRow {
                    key_id: r.get(0)?,
                    key_generation: r.get(1)?,
                    session_id: r.get(2)?,
                    agent_id: r.get(3)?,
                    record_version: r.get(4)?,
                    payload: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        type Stored = (KeepaliveAgentRow, Option<Vec<u8>>, Option<Vec<u8>>);
        let stored: Vec<Stored> = conn
            .prepare(
                "SELECT key_id, key_generation, session_id, agent_id, record_version, payload, body_sealed, \
                 body_nonce, headers, body_size, dispatched_ms, fast, updated_at \
                 FROM keepalive_agents ORDER BY key_id, key_generation, session_id, agent_id",
            )?
            .query_map([], |r| {
                Ok((
                    KeepaliveAgentRow {
                        key_id: r.get(0)?,
                        key_generation: r.get(1)?,
                        session_id: r.get(2)?,
                        agent_id: r.get(3)?,
                        record_version: r.get(4)?,
                        payload: r.get(5)?,
                        body: None,
                        body_write: BodyWrite::Keep,
                        headers: r.get(8)?,
                        body_size: r.get::<_, i64>(9)?.max(0) as usize,
                        dispatched_ms: r.get(10)?,
                        fast: r.get(11)?,
                        updated_at: r.get(12)?,
                    },
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let counters = conn
            .prepare(
                "SELECT pool_id, day, bucket, reached, returned FROM keepalive_idle_returns \
                 ORDER BY pool_id, day, bucket",
            )?
            .query_map([], |r| {
                Ok(IdleReturnsRow {
                    pool_id: r.get(0)?,
                    day: r.get(1)?,
                    bucket: r.get(2)?,
                    reached: r.get::<_, i64>(3)?.max(0) as u64,
                    returned: r.get::<_, i64>(4)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(conn);
        let warming_agents = stored
            .into_iter()
            .map(|(mut row, blob, nonce)| {
                if let (Some(blob), Some(nonce)) = (blob, nonce) {
                    let aad = keepalive_aad(
                        &row.key_id,
                        row.key_generation,
                        &row.session_id,
                        &row.agent_id,
                    );
                    row.body = crypto::open(&self.key, &aad, &nonce, &blob).ok();
                }
                row
            })
            .collect();
        Ok(EngineState {
            sessions,
            agents,
            warming_agents,
            counters,
        })
    }

    /// The stored sets of `sessions`, each `(key_id, key_generation, session_id)`, in the order of the table's key. A
    /// set that does not open under its row's key answers `value: None`, and a stored kind word outside `FactsKind`
    /// fails the read.
    pub fn load_companion_facts(
        &self,
        sessions: &[(String, i64, String)],
    ) -> Result<Vec<StoredFacts>, StoreError> {
        let mut sessions: Vec<&(String, i64, String)> = sessions.iter().collect();
        sessions.sort();
        sessions.dedup();
        type Sealed = (FactsKey, u32, Vec<u8>, Vec<u8>);
        let mut stored: Vec<Sealed> = Vec::new();
        {
            let conn = self.conn.lock().expect(LOCK_POISONED);
            let mut statement = conn.prepare_cached(
                "SELECT kind, agent_id, record_version, facts_sealed, facts_nonce FROM companion_facts \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 ORDER BY kind, agent_id",
            )?;
            for (key_id, key_generation, session_id) in sessions {
                let rows =
                    statement.query_map(params![key_id, key_generation, session_id], |r| {
                        Ok((
                            FactsKey {
                                key_id: key_id.clone(),
                                key_generation: *key_generation,
                                session_id: session_id.clone(),
                                kind: r.get(0)?,
                                agent_id: r.get(1)?,
                            },
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                        ))
                    })?;
                for row in rows {
                    stored.push(row?);
                }
            }
        }
        Ok(stored
            .into_iter()
            .map(|(key, record_version, blob, nonce)| {
                let value = crypto::open(&self.key, &facts_aad(&key), &nonce, &blob).ok();
                StoredFacts {
                    key,
                    record_version,
                    value,
                }
            })
            .collect())
    }
}
