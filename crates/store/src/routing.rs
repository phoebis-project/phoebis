//! Routing state that survives a restart: session pins, cooldowns, and the overage evidence per account.

use rusqlite::params;

use crate::{Store, StoreError, LOCK_POISONED};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRow {
    pub key_id: String,
    pub session_id: String,
    pub model: String,
    pub account_id: String,
    pub epoch: u64,
    pub created_at: i64,
    pub last_used_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CooldownRow {
    pub account_id: String,
    pub scope: String,
    pub until: i64,
    pub reason: String,
    pub window: String,
    pub source_request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverageRow {
    pub account_id: String,
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingLoad {
    pub cooldowns: Vec<CooldownRow>,
    pub overage: Vec<OverageRow>,
    pub pins: Vec<PinRow>,
}

impl Store {
    pub fn load_routing(&self) -> Result<RoutingLoad, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        let load = RoutingLoad {
            cooldowns: tx.prepare("SELECT account_id, scope, until, reason, window, source_request_id FROM cooldowns ORDER BY account_id, scope")?
                .query_map([], |r| Ok(CooldownRow { account_id: r.get(0)?, scope: r.get(1)?, until: r.get(2)?, reason: r.get(3)?, window: r.get(4)?, source_request_id: r.get(5)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            overage: tx.prepare("SELECT account_id, in_use, since, reset, observed_at, source FROM overage_state ORDER BY account_id")?
                .query_map([], |r| Ok(OverageRow { account_id: r.get(0)?, in_use: r.get(1)?, since: r.get(2)?, reset: r.get(3)?, observed_at: r.get(4)?, source: r.get(5)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            pins: tx.prepare("SELECT key_id, session_id, model, account_id, epoch, created_at, last_used_at FROM pins ORDER BY key_id, session_id, model")?
                .query_map([], |r| Ok(PinRow { key_id: r.get(0)?, session_id: r.get(1)?, model: r.get(2)?, account_id: r.get(3)?, epoch: r.get::<_, i64>(4)? as u64, created_at: r.get(5)?, last_used_at: r.get(6)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        tx.commit()?;
        Ok(load)
    }

    pub fn upsert_pin(&self, pin: &PinRow) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute("INSERT INTO pins (key_id, session_id, model, account_id, epoch, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(key_id, session_id, model) DO UPDATE SET account_id = excluded.account_id, epoch = excluded.epoch, created_at = excluded.created_at, last_used_at = excluded.last_used_at",
            params![pin.key_id, pin.session_id, pin.model, pin.account_id, pin.epoch as i64, pin.created_at, pin.last_used_at])?;
        Ok(())
    }

    pub fn delete_pin(
        &self,
        key_id: &str,
        session_id: &str,
        model: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute(
            "DELETE FROM pins WHERE key_id = ?1 AND session_id = ?2 AND model = ?3",
            params![key_id, session_id, model],
        )? > 0)
    }

    pub fn delete_pins(&self, keys: &[(String, String, String)]) -> Result<usize, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        let mut removed = 0;
        for (key_id, session_id, model) in keys {
            removed += tx.execute(
                "DELETE FROM pins WHERE key_id = ?1 AND session_id = ?2 AND model = ?3",
                params![key_id, session_id, model],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// The later deadline always wins: a short `Retry-After` arriving after a long unified reset must not
    /// release an account the provider still refuses.
    pub fn upsert_cooldown(&self, row: &CooldownRow) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute("INSERT INTO cooldowns (account_id, scope, until, reason, window, source_request_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(account_id, scope) DO UPDATE SET \
               reason = CASE WHEN excluded.until > cooldowns.until THEN excluded.reason ELSE cooldowns.reason END, \
               window = CASE WHEN excluded.until > cooldowns.until THEN excluded.window ELSE cooldowns.window END, \
               source_request_id = CASE WHEN excluded.until > cooldowns.until THEN excluded.source_request_id ELSE cooldowns.source_request_id END, \
               until = MAX(cooldowns.until, excluded.until)",
            params![row.account_id, row.scope, row.until, row.reason, row.window, row.source_request_id])?;
        Ok(())
    }

    pub fn delete_cooldown(&self, account_id: &str, scope: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute(
            "DELETE FROM cooldowns WHERE account_id = ?1 AND scope = ?2",
            params![account_id, scope],
        )? > 0)
    }

    pub fn purge_cooldowns_before(&self, now: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute("DELETE FROM cooldowns WHERE until < ?1", params![now])?)
    }

    pub fn upsert_overage(&self, row: &OverageRow) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let written = conn.execute("INSERT INTO overage_state (account_id, in_use, since, reset, observed_at, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(account_id) DO UPDATE SET in_use = excluded.in_use, since = excluded.since, reset = excluded.reset, observed_at = excluded.observed_at, source = excluded.source \
             WHERE excluded.observed_at > overage_state.observed_at",
            params![row.account_id, row.in_use, row.since, row.reset, row.observed_at, row.source])?;
        Ok(written > 0)
    }
}
