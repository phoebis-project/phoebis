//! The relay's session memory on disk: what the relay last saw for a key, session, and agent, kept past a
//! restart and past the in-process cap. The relay owns the payload's shape; this side stores and returns it
//! with the version it was written under.

use rusqlite::{params, OptionalExtension};

use crate::{Store, StoreError, LOCK_POISONED};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemoryKey {
    pub key_id: String,
    /// The holder's `created_at`. An id is reusable, so entries of the key that held it before are another
    /// holder's and must never be read as the current one's.
    pub key_generation: i64,
    /// The conversation under the prefix `session:<id>`, the client's own session id.
    pub session_id: String,
    pub agent_id: String,
}

impl MemoryKey {
    /// The key of a conversation the client named by its own session id.
    pub fn for_session(
        key_id: &str,
        key_generation: i64,
        session_id: &str,
        agent_id: &str,
    ) -> MemoryKey {
        MemoryKey {
            key_id: key_id.to_string(),
            key_generation,
            session_id: format!("session:{session_id}"),
            agent_id: agent_id.to_string(),
        }
    }

    /// The same key under another agent: the parent a subagent inherits from shares the key, the session,
    /// and the generation.
    pub fn with_agent(&self, agent: &str) -> MemoryKey {
        MemoryKey {
            agent_id: agent.to_string(),
            ..self.clone()
        }
    }
}

/// One entry as tier 2 holds it: the shape version, the JSON document, and when the entry was last seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRecord {
    pub record_version: u32,
    pub payload: String,
    pub last_seen: i64,
}

/// The relay reaches tier 2 through this alone, so the relay names no SQLite type and a test can hand it a
/// map.
pub trait MemoryStore: Send + Sync {
    fn load(&self, key: &MemoryKey) -> Result<Option<MemoryRecord>, StoreError>;
    /// Every record lands or none does; a record already present is replaced.
    fn save(&self, records: &[(MemoryKey, MemoryRecord)]) -> Result<(), StoreError>;
    /// Rows last seen before the mark go; answers how many.
    fn purge(&self, older_than: i64) -> Result<usize, StoreError>;
}

impl MemoryStore for Store {
    fn load(&self, key: &MemoryKey) -> Result<Option<MemoryRecord>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT record_version, payload, last_seen FROM session_memory \
             WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 AND agent_id = ?4",
            params![key.key_id, key.key_generation, key.session_id, key.agent_id],
            |r| {
                Ok(MemoryRecord {
                    record_version: r.get::<_, i64>(0)? as u32,
                    payload: r.get(1)?,
                    last_seen: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    fn save(&self, records: &[(MemoryKey, MemoryRecord)]) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO session_memory \
                 (key_id, key_generation, session_id, agent_id, record_version, payload, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(key_id, key_generation, session_id, agent_id) DO UPDATE SET \
                 record_version = excluded.record_version, payload = excluded.payload, \
                 last_seen = excluded.last_seen",
            )?;
            for (key, record) in records {
                stmt.execute(params![
                    key.key_id,
                    key.key_generation,
                    key.session_id,
                    key.agent_id,
                    record.record_version,
                    record.payload,
                    record.last_seen
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn purge(&self, older_than: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "DELETE FROM session_memory WHERE last_seen < ?1",
            params![older_than],
        )
        .map_err(StoreError::from)
    }
}
