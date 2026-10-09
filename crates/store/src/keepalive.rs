//! The keepalive engine's state on disk: its sessions, its agents with their retained prompts, and the idle-age
//! counters. The engine owns the payloads' shape, and this side stores each with the version it was written under and
//! seals the prompt, which is the user's own text, as the credentials are sealed.

use rusqlite::params;

use crate::{crypto, Store, StoreError, LOCK_POISONED};

/// One session row: its identity, and the engine's versioned payload of the session's fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepaliveSessionRow {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub record_version: u32,
    pub payload: String,
    pub updated_at: i64,
}

/// One agent row: its identity, the engine's versioned payload of the agent's fields, and the retained prompt the
/// probe replays beside it. `body` is the plaintext on both sides of the store, sealed only at rest, and `None` for an
/// agent that retains nothing or whose stored body no longer opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepaliveAgentRow {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub agent_id: String,
    pub record_version: u32,
    pub payload: String,
    pub body: Option<Vec<u8>>,
    /// The header block the probe sends, as the engine's JSON.
    pub headers: String,
    pub body_size: usize,
    pub dispatched_ms: Option<i64>,
    pub fast: bool,
    pub updated_at: i64,
}

/// One idle-age bucket of one pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleReturnsRow {
    pub pool_id: String,
    pub bucket: u32,
    pub reached: u64,
    pub returned: u64,
}

/// What one flush writes. A removed session takes every agent row filed under it, so a session the engine dropped
/// leaves nothing behind for a load to restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepaliveBatch {
    pub sessions: Vec<KeepaliveSessionRow>,
    pub agents: Vec<KeepaliveAgentRow>,
    /// `(key_id, key_generation, session_id)`.
    pub removed_sessions: Vec<(String, i64, String)>,
    /// `(key_id, key_generation, session_id, agent_id)`.
    pub removed_agents: Vec<(String, i64, String, String)>,
    pub counters: Vec<IdleReturnsRow>,
}

impl KeepaliveBatch {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.agents.is_empty()
            && self.removed_sessions.is_empty()
            && self.removed_agents.is_empty()
            && self.counters.is_empty()
    }
}

/// The identity tuple as length-prefixed text, each component `<byte length>:<bytes>` in this order: `key_id`,
/// `key_generation` in decimal, `session_id`, `agent_id`, the literal `keepalive`. A colon inside an id cannot move
/// a boundary (`3:s:x1:y` against `1:s3:x:y`), so a sealed body moved to any other row fails to open. `crypto::seal`
/// takes the string as it takes the credentials' AAD.
pub fn keepalive_aad(
    key_id: &str,
    key_generation: i64,
    session_id: &str,
    agent_id: &str,
) -> String {
    let generation = key_generation.to_string();
    [
        key_id,
        generation.as_str(),
        session_id,
        agent_id,
        "keepalive",
    ]
    .iter()
    .map(|part| format!("{}:{part}", part.len()))
    .collect()
}

impl Store {
    /// One transaction of upserts and deletes, each body sealed under the storage key with the AAD `keepalive_aad`.
    /// The removals land first, and the engine never names one row in both halves of a batch.
    pub fn save_keepalive(&self, batch: &KeepaliveBatch) -> Result<(), StoreError> {
        // Sealed before the connection is taken, so a large batch holds no other writer behind the cipher.
        let mut sealed = Vec::with_capacity(batch.agents.len());
        for row in &batch.agents {
            sealed.push(match &row.body {
                Some(body) => {
                    let aad = keepalive_aad(
                        &row.key_id,
                        row.key_generation,
                        &row.session_id,
                        &row.agent_id,
                    );
                    let (nonce, blob) = crypto::seal(&self.key, &aad, body)?;
                    (Some(blob), Some(nonce))
                }
                None => (None, None),
            });
        }
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (key_id, key_generation, session_id) in &batch.removed_sessions {
            tx.execute(
                "DELETE FROM keepalive_sessions WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3",
                params![key_id, key_generation, session_id],
            )?;
            tx.execute(
                "DELETE FROM keepalive_agents WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3",
                params![key_id, key_generation, session_id],
            )?;
        }
        for (key_id, key_generation, session_id, agent_id) in &batch.removed_agents {
            tx.execute(
                "DELETE FROM keepalive_agents \
                 WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND agent_id = ?4",
                params![key_id, key_generation, session_id, agent_id],
            )?;
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO keepalive_sessions \
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
                "INSERT INTO keepalive_agents \
                 (key_id, key_generation, session_id, agent_id, record_version, payload, body_sealed, body_nonce, \
                  headers, body_size, dispatched_ms, fast, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                 ON CONFLICT(key_id, key_generation, session_id, agent_id) DO UPDATE SET \
                 record_version = excluded.record_version, payload = excluded.payload, \
                 body_sealed = excluded.body_sealed, body_nonce = excluded.body_nonce, headers = excluded.headers, \
                 body_size = excluded.body_size, dispatched_ms = excluded.dispatched_ms, fast = excluded.fast, \
                 updated_at = excluded.updated_at",
            )?;
            for (row, (blob, nonce)) in batch.agents.iter().zip(&sealed) {
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
                    row.body_size as i64,
                    row.dispatched_ms,
                    row.fast,
                    row.updated_at
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO keepalive_idle_returns (pool_id, bucket, reached, returned) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(pool_id, bucket) DO UPDATE SET \
                 reached = excluded.reached, returned = excluded.returned",
            )?;
            for row in &batch.counters {
                stmt.execute(params![
                    row.pool_id,
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
    #[allow(clippy::type_complexity)]
    pub fn load_keepalive(
        &self,
    ) -> Result<
        (
            Vec<KeepaliveSessionRow>,
            Vec<KeepaliveAgentRow>,
            Vec<IdleReturnsRow>,
        ),
        StoreError,
    > {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let sessions = conn
            .prepare(
                "SELECT key_id, key_generation, session_id, record_version, payload, updated_at \
                 FROM keepalive_sessions ORDER BY key_id, key_generation, session_id",
            )?
            .query_map([], |r| {
                Ok(KeepaliveSessionRow {
                    key_id: r.get(0)?,
                    key_generation: r.get(1)?,
                    session_id: r.get(2)?,
                    record_version: r.get(3)?,
                    payload: r.get(4)?,
                    updated_at: r.get(5)?,
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
                "SELECT pool_id, bucket, reached, returned FROM keepalive_idle_returns ORDER BY pool_id, bucket",
            )?
            .query_map([], |r| {
                Ok(IdleReturnsRow {
                    pool_id: r.get(0)?,
                    bucket: r.get(1)?,
                    reached: r.get::<_, i64>(2)?.max(0) as u64,
                    returned: r.get::<_, i64>(3)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(conn);
        let agents = stored
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
        Ok((sessions, agents, counters))
    }
}
