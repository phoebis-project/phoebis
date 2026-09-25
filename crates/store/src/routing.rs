//! Routing state that survives a restart: session pins, cooldowns, and the overage evidence per account.

use rusqlite::{params, Connection, TransactionBehavior};

use crate::{Store, StoreError, LOCK_POISONED};

#[derive(Clone, Debug, PartialEq)]
pub struct PinRow {
    pub key_id: String,
    pub session_id: String,
    pub model: String,
    pub account_id: String,
    pub epoch: u64,
    pub created_at: i64,
    pub last_used_at: i64,
    /// The key's `created_at` and the account's incarnation when the pin was made: both ids are reusable, and
    /// a pin is only ever the holders' it was made for.
    pub key_generation: i64,
    pub incarnation: u64,
    /// The moving average of priced burn over the pool's burn window (meter tokens on a subscription account,
    /// dollars on an api_key account) and when it was last folded; 0 and `None` for a pin never renewed with
    /// usage.
    pub burn: f64,
    pub burn_at: Option<i64>,
    pub last_input_tokens: u64,
    /// The last main, subagent or compaction turn; `last_used_at` moves on every request, this does not.
    pub last_refresh_at: i64,
    /// The cache TTL in force at the last refresh, as the relay resolved it (the policy's rewrite, else the
    /// client's own marker), so warmth is judged here without the relay's policy type; 5 minutes until reported.
    pub cache_ttl_ms: i64,
    pub shed_from: Option<String>,
    pub shed_at: Option<i64>,
    pub static_hash: Option<String>,
    pub tail_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CooldownRow {
    pub account_id: String,
    pub scope: String,
    pub until: i64,
    /// When the rejection that set this row arrived, so a usage sample can be told apart from the cached view
    /// the account had before it.
    pub set_at: i64,
    pub reason: String,
    pub window: String,
    pub source_request_id: Option<String>,
    pub incarnation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverageRow {
    pub account_id: String,
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
    pub source: String,
    pub incarnation: u64,
}

/// A folded spelling and the dated id the API listed for it; the routing state keys that spelling's rows on
/// the dated id from the moment the row exists, and the row is loaded before any request is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionRow {
    pub spelling: String,
    pub dated_id: String,
    pub resolved_at: i64,
}

/// The live fit of one window's size for one account, in meter tokens per full window.
#[derive(Clone, Debug, PartialEq)]
pub struct FitRow {
    pub account_id: String,
    pub window: String,
    pub scale: f64,
    pub confidence: f64,
    pub intervals: u32,
    /// Per-model correction factors as a JSON object, `{}` while the mix never varied.
    pub corrections_json: String,
    pub fitted_at: i64,
    pub incarnation: u64,
}

/// One landing of a resolution: the spelling row and every pin and model cooldown it moves from the spelling to
/// the dated id.
#[derive(Clone, Debug, PartialEq)]
pub struct RekeyWrite {
    pub resolution: ResolutionRow,
    /// `(key_id, session_id, model)` of each pin under the spelling.
    pub delete_pins: Vec<(String, String, String)>,
    pub upsert_pins: Vec<PinRow>,
    /// `(account_id, scope)` of each cooldown under the spelling.
    pub delete_cooldowns: Vec<(String, String)>,
    pub upsert_cooldowns: Vec<CooldownRow>,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingLoad {
    pub cooldowns: Vec<CooldownRow>,
    pub fits: Vec<FitRow>,
    pub overage: Vec<OverageRow>,
    pub pins: Vec<PinRow>,
    pub resolutions: Vec<ResolutionRow>,
}

impl Store {
    pub fn load_routing(&self) -> Result<RoutingLoad, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        let load = RoutingLoad {
            cooldowns: tx.prepare("SELECT account_id, scope, until, set_at, reason, window, source_request_id, incarnation FROM cooldowns ORDER BY account_id, scope")?
                .query_map([], |r| Ok(CooldownRow { account_id: r.get(0)?, scope: r.get(1)?, until: r.get(2)?, set_at: r.get(3)?, reason: r.get(4)?, window: r.get(5)?, source_request_id: r.get(6)?, incarnation: r.get::<_, i64>(7)? as u64 }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            overage: tx.prepare("SELECT account_id, in_use, since, reset, observed_at, source, incarnation FROM overage_state ORDER BY account_id")?
                .query_map([], |r| Ok(OverageRow { account_id: r.get(0)?, in_use: r.get(1)?, since: r.get(2)?, reset: r.get(3)?, observed_at: r.get(4)?, source: r.get(5)?, incarnation: r.get::<_, i64>(6)? as u64 }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            pins: tx.prepare("SELECT key_id, session_id, model, account_id, epoch, created_at, last_used_at, key_generation, incarnation, burn, burn_at, last_input_tokens, last_refresh_at, cache_ttl_ms, shed_from, shed_at, static_hash, tail_hash FROM pins ORDER BY key_id, session_id, model")?
                .query_map([], |r| Ok(PinRow {
                    key_id: r.get(0)?, session_id: r.get(1)?, model: r.get(2)?, account_id: r.get(3)?,
                    epoch: r.get::<_, i64>(4)? as u64, created_at: r.get(5)?, last_used_at: r.get(6)?,
                    key_generation: r.get(7)?, incarnation: r.get::<_, i64>(8)? as u64,
                    burn: r.get(9)?, burn_at: r.get(10)?, last_input_tokens: r.get::<_, i64>(11)? as u64,
                    last_refresh_at: r.get(12)?, cache_ttl_ms: r.get(13)?,
                    shed_from: r.get(14)?, shed_at: r.get(15)?, static_hash: r.get(16)?, tail_hash: r.get(17)?,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            resolutions: tx.prepare("SELECT spelling, dated_id, resolved_at FROM model_resolutions ORDER BY spelling")?
                .query_map([], |r| Ok(ResolutionRow { spelling: r.get(0)?, dated_id: r.get(1)?, resolved_at: r.get(2)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            fits: tx.prepare("SELECT account_id, window, scale, confidence, intervals, corrections_json, fitted_at, incarnation FROM plan_fits ORDER BY account_id, window")?
                .query_map([], |r| Ok(FitRow {
                    account_id: r.get(0)?, window: r.get(1)?, scale: r.get(2)?, confidence: r.get(3)?,
                    intervals: r.get::<_, i64>(4)? as u32, corrections_json: r.get(5)?,
                    fitted_at: r.get(6)?, incarnation: r.get::<_, i64>(7)? as u64,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        tx.commit()?;
        Ok(load)
    }

    pub fn upsert_pin(&self, pin: &PinRow) -> Result<(), StoreError> {
        upsert_pin_on(&self.conn.lock().expect(LOCK_POISONED), pin)
    }

    pub fn delete_pin(
        &self,
        key_id: &str,
        session_id: &str,
        model: &str,
    ) -> Result<bool, StoreError> {
        delete_pin_on(
            &self.conn.lock().expect(LOCK_POISONED),
            key_id,
            session_id,
            model,
        )
    }

    /// The later deadline always wins: a short `Retry-After` arriving after a long unified reset must not
    /// release an account the provider still refuses.
    pub fn upsert_cooldown(&self, row: &CooldownRow) -> Result<(), StoreError> {
        upsert_cooldown_on(&self.conn.lock().expect(LOCK_POISONED), row)
    }

    pub fn delete_cooldown(&self, account_id: &str, scope: &str) -> Result<bool, StoreError> {
        delete_cooldown_on(&self.conn.lock().expect(LOCK_POISONED), account_id, scope)
    }

    pub fn purge_cooldowns_before(&self, now: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn.execute("DELETE FROM cooldowns WHERE until < ?1", params![now])?)
    }

    pub fn upsert_overage(&self, row: &OverageRow) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        // A reading from another holder of the id is no timeline to be newer than.
        let written = conn.execute("INSERT INTO overage_state (account_id, in_use, since, reset, observed_at, source, incarnation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(account_id) DO UPDATE SET in_use = excluded.in_use, since = excluded.since, reset = excluded.reset, observed_at = excluded.observed_at, source = excluded.source, incarnation = excluded.incarnation \
             WHERE excluded.observed_at > overage_state.observed_at OR excluded.incarnation <> overage_state.incarnation",
            params![row.account_id, row.in_use, row.since, row.reset, row.observed_at, row.source, row.incarnation as i64])?;
        Ok(written > 0)
    }

    /// All or nothing: a crash or a failed statement part way through would leave rows under the dated id with no
    /// spelling row to find them by after a restart, or a row deleted under the spelling and never re-inserted.
    pub fn rekey_resolution(&self, rekey: &RekeyWrite) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO model_resolutions (spelling, dated_id, resolved_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(spelling) DO UPDATE SET dated_id = excluded.dated_id, resolved_at = excluded.resolved_at",
            params![
                rekey.resolution.spelling,
                rekey.resolution.dated_id,
                rekey.resolution.resolved_at
            ],
        )?;
        for (key_id, session_id, model) in &rekey.delete_pins {
            delete_pin_on(&tx, key_id, session_id, model)?;
        }
        for pin in &rekey.upsert_pins {
            upsert_pin_on(&tx, pin)?;
        }
        for (account_id, scope) in &rekey.delete_cooldowns {
            delete_cooldown_on(&tx, account_id, scope)?;
        }
        for row in &rekey.upsert_cooldowns {
            upsert_cooldown_on(&tx, row)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The persisted map on its own: the accessor seeds the catalog cache's published map at start.
    pub fn resolutions(&self) -> Result<Vec<ResolutionRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let rows = conn
            .prepare(
                "SELECT spelling, dated_id, resolved_at FROM model_resolutions ORDER BY spelling",
            )?
            .query_map([], |r| {
                Ok(ResolutionRow {
                    spelling: r.get(0)?,
                    dated_id: r.get(1)?,
                    resolved_at: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn upsert_fit(&self, row: &FitRow) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "INSERT INTO plan_fits (account_id, window, scale, confidence, intervals, corrections_json, fitted_at, incarnation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(account_id, window) DO UPDATE SET \
               scale = excluded.scale, confidence = excluded.confidence, intervals = excluded.intervals, \
               corrections_json = excluded.corrections_json, fitted_at = excluded.fitted_at, \
               incarnation = excluded.incarnation",
            params![
                row.account_id,
                row.window,
                row.scale,
                row.confidence,
                row.intervals as i64,
                row.corrections_json,
                row.fitted_at,
                row.incarnation as i64
            ],
        )?;
        Ok(())
    }

    pub fn fits(&self) -> Result<Vec<FitRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let rows = conn
            .prepare(
                "SELECT account_id, window, scale, confidence, intervals, corrections_json, fitted_at, incarnation \
                 FROM plan_fits ORDER BY account_id, window",
            )?
            .query_map([], |r| {
                Ok(FitRow {
                    account_id: r.get(0)?,
                    window: r.get(1)?,
                    scale: r.get(2)?,
                    confidence: r.get(3)?,
                    intervals: r.get::<_, i64>(4)? as u32,
                    corrections_json: r.get(5)?,
                    fitted_at: r.get(6)?,
                    incarnation: r.get::<_, i64>(7)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn upsert_pin_on(conn: &Connection, pin: &PinRow) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO pins (key_id, session_id, model, account_id, epoch, created_at, last_used_at, key_generation, incarnation, burn, burn_at, last_input_tokens, last_refresh_at, cache_ttl_ms, shed_from, shed_at, static_hash, tail_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18) \
         ON CONFLICT(key_id, session_id, model) DO UPDATE SET \
           account_id = excluded.account_id, epoch = excluded.epoch, created_at = excluded.created_at, \
           last_used_at = excluded.last_used_at, key_generation = excluded.key_generation, \
           incarnation = excluded.incarnation, burn = excluded.burn, burn_at = excluded.burn_at, \
           last_input_tokens = excluded.last_input_tokens, last_refresh_at = excluded.last_refresh_at, \
           cache_ttl_ms = excluded.cache_ttl_ms, shed_from = excluded.shed_from, shed_at = excluded.shed_at, \
           static_hash = excluded.static_hash, tail_hash = excluded.tail_hash",
        params![
            pin.key_id,
            pin.session_id,
            pin.model,
            pin.account_id,
            pin.epoch as i64,
            pin.created_at,
            pin.last_used_at,
            pin.key_generation,
            pin.incarnation as i64,
            pin.burn,
            pin.burn_at,
            pin.last_input_tokens as i64,
            pin.last_refresh_at,
            pin.cache_ttl_ms,
            pin.shed_from,
            pin.shed_at,
            pin.static_hash,
            pin.tail_hash
        ],
    )?;
    Ok(())
}

fn delete_pin_on(
    conn: &Connection,
    key_id: &str,
    session_id: &str,
    model: &str,
) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "DELETE FROM pins WHERE key_id = ?1 AND session_id = ?2 AND model = ?3",
        params![key_id, session_id, model],
    )? > 0)
}

fn upsert_cooldown_on(conn: &Connection, row: &CooldownRow) -> Result<(), StoreError> {
    conn.execute("INSERT INTO cooldowns (account_id, scope, until, set_at, reason, window, source_request_id, incarnation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(account_id, scope) DO UPDATE SET \
           set_at = MAX(cooldowns.set_at, excluded.set_at), \
           reason = CASE WHEN excluded.until > cooldowns.until THEN excluded.reason ELSE cooldowns.reason END, \
           window = CASE WHEN excluded.until > cooldowns.until THEN excluded.window ELSE cooldowns.window END, \
           source_request_id = CASE WHEN excluded.until > cooldowns.until THEN excluded.source_request_id ELSE cooldowns.source_request_id END, \
           until = MAX(cooldowns.until, excluded.until), \
           incarnation = excluded.incarnation",
        params![row.account_id, row.scope, row.until, row.set_at, row.reason, row.window, row.source_request_id, row.incarnation as i64])?;
    Ok(())
}

fn delete_cooldown_on(
    conn: &Connection,
    account_id: &str,
    scope: &str,
) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "DELETE FROM cooldowns WHERE account_id = ?1 AND scope = ?2",
        params![account_id, scope],
    )? > 0)
}
