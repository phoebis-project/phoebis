//! Routing state that survives a restart: session pins, cooldowns, and the overage evidence per account.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, TransactionBehavior};

use crate::{Store, StoreError, LOCK_POISONED};

/// The identity a pin is held under: one conversation of one agent under one key, per model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PinKey {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PinRow {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
    pub account_id: String,
    pub epoch: u64,
    pub created_at: i64,
    pub last_used_at: i64,
    /// The key's `created_at` and the account's incarnation when the pin was made: both ids are reusable, and
    /// a pin is only ever the holders' it was made for.
    pub key_generation: i64,
    pub incarnation: u64,
    /// The moving average of the pin's usage priced in meter tokens over the pool's burn window, and when it
    /// and `burn_usd` were last folded; 0 and `None` for a pin never renewed with usage. Both averages fold on
    /// every client renewal, so no average ever mixes units, and a reader takes the one the account's funding
    /// selects through the routing crate's `BurnUnit`.
    pub burn: f64,
    pub burn_at: Option<i64>,
    /// The same usage as `burn`, priced in dollars and folded at the same `burn_at`.
    pub burn_usd: f64,
    pub last_input_tokens: u64,
    /// The last main, subagent or compaction turn, or a keepalive probe's renewal; `last_used_at` moves on every
    /// request, this does not.
    pub last_refresh_at: i64,
    /// The cache TTL in force at the last refresh, as the relay resolved it (the policy's rewrite, else the
    /// client's own marker), so warmth is judged here without the relay's policy type; 5 minutes until reported.
    pub cache_ttl_ms: i64,
    pub shed_from: Option<String>,
    pub shed_at: Option<i64>,
    pub static_hash: Option<String>,
    pub tail_hash: Option<String>,
}

impl PinRow {
    pub fn key(&self) -> PinKey {
        PinKey {
            key_id: self.key_id.clone(),
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            model: self.model.clone(),
        }
    }
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

/// The live fit of one window's size for one account, in meter tokens per full window. The carried run and the
/// band sums are kept beside the scale, so a restart continues them rather than starting them over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FitRow {
    pub account_id: String,
    pub window: String,
    pub scale: f64,
    pub confidence: f64,
    pub intervals: u32,
    /// Per-model correction factors as a JSON object, `{}` while the mix never varied.
    pub corrections_json: String,
    /// The last fold; a row written for the carry alone keeps it.
    pub fitted_at: i64,
    pub incarnation: u64,
    /// The band factor the fit observes (the row's factor times the out-of-band scale over the in-band one),
    /// `None` until both sides have folded.
    pub band_factor: Option<f64>,
    /// Meter tokens booked since the last fold, each weighted by the band its charge landed in.
    pub carry_weighted: f64,
    /// The same tokens unweighted, split by whether the charge landed inside the band or outside it.
    pub carry_inside: f64,
    pub carry_outside: f64,
    /// How far the utilization has risen since the last fold.
    pub carry_delta: f64,
    /// When the carried run began, and its latest reading with the reset that reading named.
    pub carry_since: Option<i64>,
    pub carry_utilization: Option<f64>,
    pub carry_reset: Option<i64>,
    /// Unweighted meter tokens and utilization delta summed over the folds wholly inside the band, and over
    /// those wholly outside it.
    pub band_inside_tokens: f64,
    pub band_inside_delta: f64,
    pub band_outside_tokens: f64,
    pub band_outside_delta: f64,
}

impl FitRow {
    /// Every number finite and the scale positive: a row that fails this can seed no denominator, so the
    /// fitter starts its window over and the router serves it to no reader.
    pub fn sound(&self) -> bool {
        [
            self.scale,
            self.confidence,
            self.carry_weighted,
            self.carry_inside,
            self.carry_outside,
            self.carry_delta,
            self.band_inside_tokens,
            self.band_inside_delta,
            self.band_outside_tokens,
            self.band_outside_delta,
        ]
        .into_iter()
        .chain(self.carry_utilization)
        .chain(self.band_factor)
        .all(f64::is_finite)
            && self.scale > 0.0
    }
}

const FIT_COLUMNS: &str = "account_id, window, scale, confidence, intervals, corrections_json, fitted_at, \
     incarnation, band_factor, carry_weighted, carry_inside, carry_outside, carry_delta, carry_since, \
     carry_utilization, carry_reset, band_inside_tokens, band_inside_delta, band_outside_tokens, band_outside_delta";

fn fit_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<FitRow> {
    Ok(FitRow {
        account_id: r.get(0)?,
        window: r.get(1)?,
        scale: r.get(2)?,
        confidence: r.get(3)?,
        intervals: r.get::<_, i64>(4)? as u32,
        corrections_json: r.get(5)?,
        fitted_at: r.get(6)?,
        incarnation: r.get::<_, i64>(7)? as u64,
        band_factor: r.get(8)?,
        carry_weighted: r.get(9)?,
        carry_inside: r.get(10)?,
        carry_outside: r.get(11)?,
        carry_delta: r.get(12)?,
        carry_since: r.get(13)?,
        carry_utilization: r.get(14)?,
        carry_reset: r.get(15)?,
        band_inside_tokens: r.get(16)?,
        band_inside_delta: r.get(17)?,
        band_outside_tokens: r.get(18)?,
        band_outside_delta: r.get(19)?,
    })
}

/// One landing of a resolution: the spelling row and every pin and model cooldown it moves from the spelling to
/// the dated id.
#[derive(Clone, Debug, PartialEq)]
pub struct RekeyWrite {
    pub resolution: ResolutionRow,
    pub delete_pins: Vec<PinKey>,
    pub upsert_pins: Vec<PinRow>,
    /// `(account_id, scope)` of each cooldown under the spelling.
    pub delete_cooldowns: Vec<(String, String)>,
    pub upsert_cooldowns: Vec<CooldownRow>,
}

/// What a hash in the `affinity` table is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffinityChain {
    Message,
    Request,
    Response,
    Static,
}

impl AffinityChain {
    pub fn as_str(self) -> &'static str {
        match self {
            AffinityChain::Message => "message",
            AffinityChain::Request => "request",
            AffinityChain::Response => "response",
            AffinityChain::Static => "static",
        }
    }

    pub fn parse(value: &str) -> Option<AffinityChain> {
        match value {
            "message" => Some(AffinityChain::Message),
            "request" => Some(AffinityChain::Request),
            "response" => Some(AffinityChain::Response),
            "static" => Some(AffinityChain::Static),
            _ => None,
        }
    }
}

impl FromSql for AffinityChain {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        AffinityChain::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for AffinityChain {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// One boundary or parent id: which account the conversation it names last ran on, whose conversation it belongs
/// to, and, when the cache confirmed it, how warm it is. The links (the account, the key, the root) are written
/// for every completed response; the warmth is written only with cache evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityRow {
    pub model: String,
    pub hash: String,
    pub chain: AffinityChain,
    /// The account and its incarnation, as the routing state's other account-bound rows carry them: a row
    /// under another incarnation is a replaced account's and is dropped.
    pub account_id: String,
    pub incarnation: u64,
    /// `written_at` is the last report's time (the purge reads it). `ttl_ms` is the lifetime of the marker the
    /// writing request carried on this boundary's tier, as sent upstream; 0 for a link the cache never
    /// confirmed, which never places. `prefix_tokens` is the promised prefix: for a `static` row the relay's
    /// estimate of the blocks the boundary covers (canonical bytes over four), for a `message`, `response` or
    /// `request` row the cached tokens the upstream reported for the writing request.
    pub written_at: i64,
    pub ttl_ms: i64,
    pub prefix_tokens: u64,
    pub key_id: String,
    pub key_generation: i64,
    /// The conversation the writing request belonged to: its resolved session under an identity, its own first
    /// message boundary for a request with none.
    pub root: String,
}

const AFFINITY_COLUMNS: &str =
    "model, hash, chain, account_id, incarnation, written_at, ttl_ms, prefix_tokens, key_id, key_generation, root";

fn affinity_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<AffinityRow> {
    Ok(AffinityRow {
        model: r.get(0)?,
        hash: r.get(1)?,
        chain: r.get(2)?,
        account_id: r.get(3)?,
        incarnation: r.get::<_, i64>(4)? as u64,
        written_at: r.get(5)?,
        ttl_ms: r.get(6)?,
        prefix_tokens: r.get::<_, i64>(7)? as u64,
        key_id: r.get(8)?,
        key_generation: r.get(9)?,
        root: r.get(10)?,
    })
}

#[derive(Clone, Debug, Default)]
pub struct RoutingLoad {
    pub affinity: Vec<AffinityRow>,
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
            pins: tx.prepare("SELECT key_id, session_id, agent_id, model, account_id, epoch, created_at, last_used_at, key_generation, incarnation, burn, burn_at, last_input_tokens, last_refresh_at, cache_ttl_ms, shed_from, shed_at, static_hash, tail_hash, burn_usd FROM pins ORDER BY key_id, session_id, agent_id, model")?
                .query_map([], |r| Ok(PinRow {
                    key_id: r.get(0)?, session_id: r.get(1)?, agent_id: r.get(2)?, model: r.get(3)?, account_id: r.get(4)?,
                    epoch: r.get::<_, i64>(5)? as u64, created_at: r.get(6)?, last_used_at: r.get(7)?,
                    key_generation: r.get(8)?, incarnation: r.get::<_, i64>(9)? as u64,
                    burn: r.get(10)?, burn_at: r.get(11)?, burn_usd: r.get(19)?, last_input_tokens: r.get::<_, i64>(12)? as u64,
                    last_refresh_at: r.get(13)?, cache_ttl_ms: r.get(14)?,
                    shed_from: r.get(15)?, shed_at: r.get(16)?, static_hash: r.get(17)?, tail_hash: r.get(18)?,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            resolutions: tx.prepare("SELECT spelling, dated_id, resolved_at FROM model_resolutions ORDER BY spelling")?
                .query_map([], |r| Ok(ResolutionRow { spelling: r.get(0)?, dated_id: r.get(1)?, resolved_at: r.get(2)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            fits: tx.prepare(&format!("SELECT {FIT_COLUMNS} FROM plan_fits ORDER BY account_id, window"))?
                .query_map([], fit_of)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            affinity: tx.prepare(&format!("SELECT {AFFINITY_COLUMNS} FROM affinity ORDER BY model, hash"))?
                .query_map([], affinity_of)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        tx.commit()?;
        Ok(load)
    }

    pub fn upsert_pin(&self, pin: &PinRow) -> Result<(), StoreError> {
        upsert_pin_on(&self.conn.lock().expect(LOCK_POISONED), pin)
    }

    pub fn delete_pin(&self, key: &PinKey) -> Result<bool, StoreError> {
        delete_pin_on(&self.conn.lock().expect(LOCK_POISONED), key)
    }

    /// Every row lands or none does; a row under an existing (model, hash) replaces it.
    pub fn upsert_affinity(&self, rows: &[AffinityRow]) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for row in rows {
            tx.execute(
                &format!(
                    "INSERT INTO affinity ({AFFINITY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                     ON CONFLICT(model, hash) DO UPDATE SET \
                       chain = excluded.chain, account_id = excluded.account_id, incarnation = excluded.incarnation, \
                       written_at = excluded.written_at, ttl_ms = excluded.ttl_ms, prefix_tokens = excluded.prefix_tokens, \
                       key_id = excluded.key_id, key_generation = excluded.key_generation, root = excluded.root"
                ),
                params![
                    row.model,
                    row.hash,
                    row.chain,
                    row.account_id,
                    row.incarnation as i64,
                    row.written_at,
                    row.ttl_ms,
                    row.prefix_tokens as i64,
                    row.key_id,
                    row.key_generation,
                    row.root
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_affinity(&self, keys: &[(String, String)]) -> Result<usize, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut deleted = 0;
        for (model, hash) in keys {
            deleted += conn.execute(
                "DELETE FROM affinity WHERE model = ?1 AND hash = ?2",
                params![model, hash],
            )?;
        }
        Ok(deleted)
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
        for key in &rekey.delete_pins {
            delete_pin_on(&tx, key)?;
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
            &format!(
                "INSERT INTO plan_fits ({FIT_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20) \
                 ON CONFLICT(account_id, window) DO UPDATE SET \
                   scale = excluded.scale, confidence = excluded.confidence, intervals = excluded.intervals, \
                   corrections_json = excluded.corrections_json, fitted_at = excluded.fitted_at, \
                   incarnation = excluded.incarnation, band_factor = excluded.band_factor, \
                   carry_weighted = excluded.carry_weighted, carry_inside = excluded.carry_inside, \
                   carry_outside = excluded.carry_outside, carry_delta = excluded.carry_delta, \
                   carry_since = excluded.carry_since, carry_utilization = excluded.carry_utilization, \
                   carry_reset = excluded.carry_reset, band_inside_tokens = excluded.band_inside_tokens, \
                   band_inside_delta = excluded.band_inside_delta, \
                   band_outside_tokens = excluded.band_outside_tokens, \
                   band_outside_delta = excluded.band_outside_delta"
            ),
            params![
                row.account_id,
                row.window,
                row.scale,
                row.confidence,
                row.intervals as i64,
                row.corrections_json,
                row.fitted_at,
                row.incarnation as i64,
                row.band_factor,
                row.carry_weighted,
                row.carry_inside,
                row.carry_outside,
                row.carry_delta,
                row.carry_since,
                row.carry_utilization,
                row.carry_reset,
                row.band_inside_tokens,
                row.band_inside_delta,
                row.band_outside_tokens,
                row.band_outside_delta
            ],
        )?;
        Ok(())
    }

    pub fn fits(&self) -> Result<Vec<FitRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let rows = conn
            .prepare(&format!(
                "SELECT {FIT_COLUMNS} FROM plan_fits ORDER BY account_id, window"
            ))?
            .query_map([], fit_of)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn upsert_pin_on(conn: &Connection, pin: &PinRow) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO pins (key_id, session_id, agent_id, model, account_id, epoch, created_at, last_used_at, key_generation, incarnation, burn, burn_at, last_input_tokens, last_refresh_at, cache_ttl_ms, shed_from, shed_at, static_hash, tail_hash, burn_usd) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20) \
         ON CONFLICT(key_id, session_id, agent_id, model) DO UPDATE SET \
           account_id = excluded.account_id, epoch = excluded.epoch, created_at = excluded.created_at, \
           last_used_at = excluded.last_used_at, key_generation = excluded.key_generation, \
           incarnation = excluded.incarnation, burn = excluded.burn, burn_at = excluded.burn_at, \
           last_input_tokens = excluded.last_input_tokens, last_refresh_at = excluded.last_refresh_at, \
           cache_ttl_ms = excluded.cache_ttl_ms, shed_from = excluded.shed_from, shed_at = excluded.shed_at, \
           static_hash = excluded.static_hash, tail_hash = excluded.tail_hash, burn_usd = excluded.burn_usd",
        params![
            pin.key_id,
            pin.session_id,
            pin.agent_id,
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
            pin.tail_hash,
            pin.burn_usd
        ],
    )?;
    Ok(())
}

fn delete_pin_on(conn: &Connection, key: &PinKey) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "DELETE FROM pins WHERE key_id = ?1 AND session_id = ?2 AND agent_id = ?3 AND model = ?4",
        params![key.key_id, key.session_id, key.agent_id, key.model],
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
