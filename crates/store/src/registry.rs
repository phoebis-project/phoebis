//! Registry rows: pools, their members, the router's own API keys, the served models of each account, and the
//! change log every write appends to.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::Ordering;

use crate::credentials::store_api_key_secret;
use crate::{AccountRow, AccountState, Secret, Store, StoreError, LOCK_POISONED};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    EarliestRefresh,
    LeastUtilized,
    PriorityRandom,
    RoundRobin,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::EarliestRefresh => "earliest_refresh",
            Strategy::LeastUtilized => "least_utilized",
            Strategy::PriorityRandom => "priority_random",
            Strategy::RoundRobin => "round_robin",
        }
    }
}

impl FromSql for Strategy {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "earliest_refresh" => Ok(Strategy::EarliestRefresh),
            "least_utilized" => Ok(Strategy::LeastUtilized),
            "priority_random" => Ok(Strategy::PriorityRandom),
            "round_robin" => Ok(Strategy::RoundRobin),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for Strategy {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// How the api_key group orders under a sample-driven strategy, which has no sample to order an API key by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubStrategy {
    PriorityRandom,
    RoundRobin,
}

impl SubStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            SubStrategy::PriorityRandom => "priority_random",
            SubStrategy::RoundRobin => "round_robin",
        }
    }
}

impl FromSql for SubStrategy {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "priority_random" => Ok(SubStrategy::PriorityRandom),
            "round_robin" => Ok(SubStrategy::RoundRobin),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for SubStrategy {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// One entry of a model rule list: a pattern over the folded canonical id, negated when `not` is set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRule {
    pub pattern: String,
    #[serde(default)]
    pub not: bool,
}

/// How a key's own list meets its pool's: `Extend` appends the pool's entries after the key's own, `Replace`
/// leaves them out. A pool has no mode: it inherits nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RulesMode {
    #[default]
    Extend,
    Replace,
}

impl RulesMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RulesMode::Extend => "extend",
            RulesMode::Replace => "replace",
        }
    }

    pub fn parse(value: &str) -> Option<RulesMode> {
        match value {
            "extend" => Some(RulesMode::Extend),
            "replace" => Some(RulesMode::Replace),
            _ => None,
        }
    }
}

impl FromSql for RulesMode {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        RulesMode::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for RulesMode {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OveragePolicy {
    Allow,
    Never,
    PreferPlan,
}

impl OveragePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            OveragePolicy::Allow => "allow",
            OveragePolicy::Never => "never",
            OveragePolicy::PreferPlan => "prefer_plan",
        }
    }
}

impl FromSql for OveragePolicy {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "allow" => Ok(OveragePolicy::Allow),
            "never" => Ok(OveragePolicy::Never),
            "prefer_plan" => Ok(OveragePolicy::PreferPlan),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for OveragePolicy {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    Discovered,
    Manual,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Discovered => "discovered",
            ModelSource::Manual => "manual",
        }
    }
}

impl FromSql for ModelSource {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "discovered" => Ok(ModelSource::Discovered),
            "manual" => Ok(ModelSource::Manual),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for ModelSource {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Clone, Debug)]
pub struct PoolRow {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub strategy: Strategy,
    pub affinity_ttl_seconds: u64,
    pub overage_policy: OveragePolicy,
    pub migrate_above_utilization: Option<f64>,
    pub select_below_utilization: Option<f64>,
    pub default_allowed_models: Option<Vec<String>>,
    pub keepalive_json: String,
    pub body_capture_hours: u32,
    /// Order the selection set by account kind before priority; forced on under the sample-driven strategies.
    pub prefer_subscription: bool,
    pub api_key_sub_strategy: SubStrategy,
    pub cache_ttl_json: String,
    /// `None` inherits `compatibility.cache_diagnostics`; `Some` overrides it for this pool.
    pub cache_diagnostics: Option<bool>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PoolRow {
    /// The operator's stored preference, or the strategies that force it: a sample-driven strategy has no
    /// sample to order an api_key account by. One function, so the selector, the pool route, and the console
    /// cannot answer differently about the same pool.
    pub fn kind_ordered(&self) -> bool {
        self.prefer_subscription
            || matches!(
                self.strategy,
                Strategy::EarliestRefresh | Strategy::LeastUtilized
            )
    }
}

#[derive(Clone, Debug)]
pub struct PoolMember {
    pub pool_id: String,
    pub account_id: String,
    pub priority: u32,
}

#[derive(Clone, Debug)]
pub struct ApiKeyRow {
    pub id: String,
    pub label: String,
    pub notes: Option<String>,
    pub pool_id: String,
    pub enabled: bool,
    pub revoked_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub allowed_models: Option<Vec<String>>,
    pub keepalive_opt_out: bool,
    pub body_capture_hours: Option<u32>,
    pub secret_digest: Vec<u8>,
    pub prefix: String,
    pub created_at: i64,
    pub rotated_at: Option<i64>,
    pub last_used_at: Option<i64>,
    /// When the secret a rotation retired last authenticated; cleared by the next rotation.
    pub retired_secret_last_used_at: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct ServedModel {
    pub account_id: String,
    pub upstream_id: String,
    pub canonical_id: String,
    pub selected: bool,
    pub source: ModelSource,
    pub long_context: bool,
    pub missing: bool,
    pub is_new: bool,
}

#[derive(Clone, Debug)]
pub struct ServedModelWrite {
    pub upstream_id: String,
    pub selected: bool,
    pub long_context: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SyncDelta {
    pub added: Vec<String>,
    pub missing: Vec<String>,
    pub first_sync: bool,
}

#[derive(Clone, Debug)]
pub struct RegistryChange {
    pub id: u64,
    pub at: i64,
    pub actor: String,
    pub kind: String,
    pub target: String,
    pub diff_json: String,
}

/// How long a rotated key's previous secret keeps authenticating; the console's rotation dialog promises this
/// window to running sessions.
pub const ROTATION_GRACE_MS: i64 = 15 * 60_000;

/// A secret a rotation retired, accepted for its key until `expires_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetiredSecret {
    pub key_id: String,
    pub secret_digest: Vec<u8>,
    pub prefix: String,
    pub retired_at: i64,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct RegistryLoad {
    pub accounts: Vec<crate::AccountRow>,
    /// The generation of each account's credential, for accounts that hold one. The snapshot carries it so a
    /// verdict about a replaced credential can be told from a current one without reading the store.
    pub credential_generations: Vec<(String, u64)>,
    pub keys: Vec<ApiKeyRow>,
    pub members: Vec<PoolMember>,
    pub models: Vec<ServedModel>,
    pub pools: Vec<PoolRow>,
    pub retired_secrets: Vec<RetiredSecret>,
    /// Live keys whose id was deleted with an earlier key before this one was created, in the order the
    /// change log recorded it. The earlier holder's rollup rows from before the ledger carried a generation
    /// sit at generation 0 under the same id, so the startup stamp starts such a key's own rows at its
    /// creation rather than claiming every unstamped row of the id.
    pub reused_key_ids: Vec<String>,
}

/// Everything a verified edit changes. Borrowed so the admin route can stage it, run discovery, and commit.
#[derive(Clone, Copy, Debug)]
pub struct AccountUpdate<'a> {
    pub id: &'a str,
    /// The revision the discovery ran against. The commit only lands while the row still carries it, so two
    /// edits verified in parallel cannot combine into an endpoint and a secret nothing ever checked together.
    pub expected_revision: u64,
    pub lifecycle_generation: u64,
    pub label: &'a str,
    pub secret: Option<&'a Secret>,
    pub catalog: Option<&'a [(String, bool)]>,
    pub catalog_error: Option<&'a str>,
    pub state: AccountState,
    pub at: i64,
    pub actor: &'a str,
}

/// The routing key of a served model: lowercase with one trailing `[1m]` removed, the same fold the relay applies
/// to a request's `model`.
pub fn canonical_id(upstream_id: &str) -> String {
    let lowered = upstream_id.to_ascii_lowercase();
    lowered.strip_suffix("[1m]").unwrap_or(&lowered).to_string()
}

/// The display prefix of a secret: what an operator sees in a list next to a key that is never shown again.
pub fn key_prefix(secret: &str) -> String {
    secret
        .strip_prefix("phoebis-")
        .unwrap_or(secret)
        .chars()
        .take(8)
        .collect()
}

pub(crate) fn record_change(
    tx: &Transaction<'_>,
    at: i64,
    actor: &str,
    kind: &str,
    target: &str,
    diff_json: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO registry_changes (at, actor, kind, target, diff_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![at, actor, kind, target, diff_json],
    )?;
    Ok(())
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn list_json(list: &Option<Vec<String>>) -> Result<Option<String>, StoreError> {
    list.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(StoreError::from)
}

fn json_list(raw: Option<String>) -> Result<Option<Vec<String>>, StoreError> {
    raw.map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(StoreError::from)
}

impl Store {
    pub fn load_registry(&self) -> Result<RegistryLoad, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        if self.fail_next_load.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Io(std::io::Error::other(
                "injected load failure",
            )));
        }
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        let load = RegistryLoad {
            accounts: crate::accounts::all_accounts(&tx)?,
            credential_generations: tx
                .prepare(
                    "SELECT a.id, COALESCE(c.generation, k.generation) FROM accounts a \
                     LEFT JOIN credentials c ON c.account_id = a.id \
                     LEFT JOIN api_key_secrets k ON k.account_id = a.id \
                     WHERE c.generation IS NOT NULL OR k.generation IS NOT NULL",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            keys: all_keys(&tx)?,
            members: tx
                .prepare("SELECT pool_id, account_id, priority FROM pool_members ORDER BY pool_id, account_id")?
                .query_map([], |r| {
                    Ok(PoolMember { pool_id: r.get(0)?, account_id: r.get(1)?, priority: r.get::<_, i64>(2)? as u32 })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            models: tx
                .prepare(
                    "SELECT account_id, upstream_id, canonical_id, selected, source, long_context, missing, is_new \
                     FROM account_models ORDER BY account_id, upstream_id",
                )?
                .query_map([], row_to_model)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            pools: all_pools(&tx)?,
            retired_secrets: tx
                .prepare(
                    "SELECT key_id, secret_digest, prefix, retired_at, expires_at FROM api_key_retired_secrets",
                )?
                .query_map([], |r| {
                    Ok(RetiredSecret {
                        key_id: r.get(0)?,
                        secret_digest: r.get(1)?,
                        prefix: r.get(2)?,
                        retired_at: r.get(3)?,
                        expires_at: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            // The change log's own order decides, never the two clocks: a delete and the create that
            // follows it can share a millisecond, and the id is reused all the same. `registry_changes` is
            // never pruned, so the create that made the live key is always on record, and both lookups run
            // on `registry_changes_target (target, id)`.
            reused_key_ids: tx
                .prepare(
                    "SELECT k.id FROM api_keys k WHERE EXISTS ( \
                       SELECT 1 FROM registry_changes d \
                       WHERE d.target = k.id AND d.kind = 'key_deleted' \
                         AND d.id < (SELECT MAX(c.id) FROM registry_changes c \
                                     WHERE c.target = k.id AND c.kind = 'key_created')) \
                     ORDER BY k.id",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        tx.commit()?;
        Ok(load)
    }

    pub fn pools(&self) -> Result<Vec<PoolRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        all_pools(&conn)
    }

    pub fn pool(&self, id: &str) -> Result<Option<PoolRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!("{POOL_SELECT} WHERE id = ?1"),
            params![id],
            row_to_pool,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn insert_pool(&self, row: &PoolRow, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO pools (id, label, enabled, strategy, affinity_ttl_seconds, overage_policy, migrate_above_utilization, select_below_utilization, default_allowed_models_json, keepalive_json, body_capture_hours, prefer_subscription, api_key_sub_strategy, cache_ttl_json, cache_diagnostics, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
            params![
                row.id,
                row.label,
                row.enabled,
                row.strategy,
                row.affinity_ttl_seconds as i64,
                row.overage_policy,
                row.migrate_above_utilization,
                row.select_below_utilization,
                list_json(&row.default_allowed_models)?,
                row.keepalive_json,
                row.body_capture_hours as i64,
                row.prefer_subscription,
                row.api_key_sub_strategy,
                row.cache_ttl_json,
                row.cache_diagnostics,
                now
            ],
        )?;
        record_change(&tx, now, actor, "pool_created", &row.id, &pool_diff(row))?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_pool(&self, row: &PoolRow, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let updated = tx.execute(
            "UPDATE pools SET label = ?2, enabled = ?3, strategy = ?4, affinity_ttl_seconds = ?5, overage_policy = ?6, migrate_above_utilization = ?7, select_below_utilization = ?8, default_allowed_models_json = ?9, keepalive_json = ?10, body_capture_hours = ?11, prefer_subscription = ?12, api_key_sub_strategy = ?13, cache_ttl_json = ?14, cache_diagnostics = ?15, updated_at = ?16 WHERE id = ?1",
            params![
                row.id,
                row.label,
                row.enabled,
                row.strategy,
                row.affinity_ttl_seconds as i64,
                row.overage_policy,
                row.migrate_above_utilization,
                row.select_below_utilization,
                list_json(&row.default_allowed_models)?,
                row.keepalive_json,
                row.body_capture_hours as i64,
                row.prefer_subscription,
                row.api_key_sub_strategy,
                row.cache_ttl_json,
                row.cache_diagnostics,
                now
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::PoolNotFound(row.id.clone()));
        }
        record_change(&tx, now, actor, "pool_updated", &row.id, &pool_diff(row))?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_pool(&self, id: &str, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let keys: i64 = tx.query_row(
            "SELECT COUNT(*) FROM api_keys WHERE pool_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        // A key without a pool has nowhere to route, so the pool outlives its keys rather than orphaning them.
        if keys > 0 {
            return Err(StoreError::PoolInUse(id.to_string()));
        }
        if tx.execute("DELETE FROM pools WHERE id = ?1", params![id])? == 0 {
            return Err(StoreError::PoolNotFound(id.to_string()));
        }
        record_change(&tx, now_ms(), actor, "pool_deleted", id, "{}")?;
        tx.commit()?;
        Ok(())
    }

    pub fn pool_members(&self, pool_id: &str) -> Result<Vec<PoolMember>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt =
            conn.prepare("SELECT pool_id, account_id, priority FROM pool_members WHERE pool_id = ?1 ORDER BY priority, account_id")?;
        let rows = stmt.query_map(params![pool_id], |r| {
            Ok(PoolMember {
                pool_id: r.get(0)?,
                account_id: r.get(1)?,
                priority: r.get::<_, i64>(2)? as u32,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    pub fn set_pool_members(
        &self,
        pool_id: &str,
        members: &[(String, u32)],
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT COUNT(*) FROM pools WHERE id = ?1",
            params![pool_id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(StoreError::PoolNotFound(pool_id.to_string()));
        }
        for (account_id, _) in members {
            if tx.query_row(
                "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                params![account_id],
                |r| r.get::<_, i64>(0),
            )? == 0
            {
                return Err(StoreError::AccountNotFound(account_id.clone()));
            }
        }
        tx.execute(
            "DELETE FROM pool_members WHERE pool_id = ?1",
            params![pool_id],
        )?;
        for (account_id, priority) in members {
            tx.execute(
                "INSERT INTO pool_members (pool_id, account_id, priority) VALUES (?1, ?2, ?3)",
                params![pool_id, account_id, *priority as i64],
            )?;
        }
        let diff = serde_json::json!({ "members": members.iter().map(|(a, p)| serde_json::json!({ "account_id": a, "priority": p })).collect::<Vec<_>>() })
            .to_string();
        record_change(&tx, now_ms(), actor, "pool_members_set", pool_id, &diff)?;
        tx.commit()?;
        Ok(())
    }

    pub fn api_keys(&self) -> Result<Vec<ApiKeyRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        all_keys(&conn)
    }

    pub fn api_key(&self, id: &str) -> Result<Option<ApiKeyRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!("{KEY_SELECT} WHERE id = ?1"),
            params![id],
            row_to_key,
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Answers the `created_at` it stamped: a caller's own clock is not the generation, and a selector or a
    /// response built from the row would name the wrong holder after a re-create.
    pub fn insert_api_key(&self, row: &ApiKeyRow, actor: &str) -> Result<i64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT COUNT(*) FROM pools WHERE id = ?1",
            params![row.pool_id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(StoreError::PoolNotFound(row.pool_id.clone()));
        }
        // The id is reusable and the generation is this stamp, so a key created inside the millisecond its
        // previous holder was created in must not share it: the change log remembers every create under the
        // id (never pruned, served by `registry_changes_target`), and the stamp goes one past the last one
        // when the clock has not moved.
        let previous: Option<i64> = tx.query_row(
            "SELECT MAX(at) FROM registry_changes WHERE target = ?1 AND kind = 'key_created'",
            params![row.id],
            |r| r.get(0),
        )?;
        let now = previous.map_or(now_ms(), |p| now_ms().max(p + 1));
        tx.execute(
            "INSERT INTO api_keys (id, label, notes, pool_id, enabled, revoked_at, expires_at, allowed_models_json, keepalive_opt_out, body_capture_hours, secret_digest, prefix, created_at, rotated_at, last_used_at, retired_secret_last_used_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL, NULL, NULL)",
            params![
                row.id,
                row.label,
                row.notes,
                row.pool_id,
                row.enabled,
                row.revoked_at,
                row.expires_at,
                list_json(&row.allowed_models)?,
                row.keepalive_opt_out,
                row.body_capture_hours.map(|h| h as i64),
                row.secret_digest,
                row.prefix,
                now
            ],
        )?;
        record_change(&tx, now, actor, "key_created", &row.id, &key_diff(row))?;
        tx.commit()?;
        Ok(now)
    }

    pub fn update_api_key(&self, row: &ApiKeyRow, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT COUNT(*) FROM pools WHERE id = ?1",
            params![row.pool_id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(StoreError::PoolNotFound(row.pool_id.clone()));
        }
        let updated = tx.execute(
            "UPDATE api_keys SET label = ?2, notes = ?3, pool_id = ?4, enabled = ?5, expires_at = ?6, allowed_models_json = ?7, keepalive_opt_out = ?8, body_capture_hours = ?9 WHERE id = ?1",
            params![
                row.id,
                row.label,
                row.notes,
                row.pool_id,
                row.enabled,
                row.expires_at,
                list_json(&row.allowed_models)?,
                row.keepalive_opt_out,
                row.body_capture_hours.map(|h| h as i64)
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::KeyNotFound(row.id.clone()));
        }
        record_change(&tx, now_ms(), actor, "key_updated", &row.id, &key_diff(row))?;
        tx.commit()?;
        Ok(())
    }

    pub fn rotate_api_key(
        &self,
        id: &str,
        digest: &[u8],
        prefix: &str,
        at: i64,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<(Vec<u8>, String)> = tx
            .query_row(
                "SELECT secret_digest, prefix FROM api_keys WHERE id = ?1 AND revoked_at IS NULL",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((old_digest, old_prefix)) = current else {
            return Err(StoreError::KeyNotFound(id.to_string()));
        };
        tx.execute(
            "DELETE FROM api_key_retired_secrets WHERE expires_at <= ?1",
            params![at],
        )?;
        // Replace, never insert beside: the grace covers the secret this rotation retires and no earlier one.
        tx.execute(
            "INSERT OR REPLACE INTO api_key_retired_secrets (key_id, secret_digest, prefix, retired_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, old_digest, old_prefix, at, at + ROTATION_GRACE_MS],
        )?;
        // A new grace starts unpresented: a stamp under the retired secret this rotation just replaced would
        // otherwise read as if it belonged to the one this rotation creates.
        tx.execute(
            "UPDATE api_keys SET secret_digest = ?2, prefix = ?3, rotated_at = ?4, retired_secret_last_used_at = NULL WHERE id = ?1",
            params![id, digest, prefix, at],
        )?;
        record_change(
            &tx,
            at,
            actor,
            "key_rotated",
            id,
            &serde_json::json!({ "prefix": prefix, "previous_secret_until": at + ROTATION_GRACE_MS }).to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn revoke_api_key(&self, id: &str, at: i64, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM api_key_retired_secrets WHERE key_id = ?1",
            params![id],
        )?;
        let updated = tx.execute(
            "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ?2), enabled = 0 WHERE id = ?1",
            params![id, at],
        )?;
        if updated == 0 {
            return Err(StoreError::KeyNotFound(id.to_string()));
        }
        record_change(&tx, at, actor, "key_revoked", id, "{}")?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_api_key(&self, id: &str, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.execute("DELETE FROM api_keys WHERE id = ?1", params![id])? == 0 {
            return Err(StoreError::KeyNotFound(id.to_string()));
        }
        tx.execute("DELETE FROM pins WHERE key_id = ?1", params![id])?;
        // Nothing reads these rows again, since a key created under the id next takes a later generation;
        // dropping them here spares the retention purge a table of dead entries.
        tx.execute("DELETE FROM session_memory WHERE key_id = ?1", params![id])?;
        let now = now_ms();
        record_change(&tx, now, actor, "key_deleted", id, "{}")?;
        tx.commit()?;
        Ok(())
    }

    /// `last_used_at` always; `retired_secret_last_used_at` only for a retired presentation whose `rotated_at`
    /// is still the row's.
    pub fn touch_api_key(
        &self,
        id: &str,
        at: i64,
        presented_retired: bool,
        rotated_at: Option<i64>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.execute(
            "UPDATE api_keys SET last_used_at = ?2, \
             retired_secret_last_used_at = CASE WHEN ?3 AND rotated_at IS ?4 THEN ?2 ELSE retired_secret_last_used_at END \
             WHERE id = ?1",
            params![id, at, presented_retired, rotated_at],
        )?;
        Ok(())
    }

    pub fn served_models(&self, account_id: &str) -> Result<Vec<ServedModel>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare(
            "SELECT account_id, upstream_id, canonical_id, selected, source, long_context, missing, is_new \
             FROM account_models WHERE account_id = ?1 ORDER BY upstream_id",
        )?;
        let rows = stmt.query_map(params![account_id], row_to_model)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// A manual row the discovery reports keeps its source and selection, but takes the discovery's
    /// long-context flag.
    ///
    /// `expected_revision` is the revision discovery ran against, where the caller has one: a catalog
    /// fetched from an endpoint the account no longer holds answers `Stale` instead of overwriting what the
    /// replacement endpoint serves.
    pub fn apply_catalog_sync(
        &self,
        account_id: &str,
        discovered: &[(String, bool)],
        expected_revision: Option<u64>,
        at: i64,
        actor: &str,
    ) -> Result<SyncDelta, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(expected) = expected_revision {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM accounts WHERE id = ?1",
                    params![account_id],
                    |r| r.get(0),
                )
                .optional()?;
            match current {
                None => return Err(StoreError::AccountNotFound(account_id.to_string())),
                Some(current) if current as u64 != expected => return Err(StoreError::Stale),
                Some(_) => {}
            }
        }
        let delta = apply_catalog_in(&tx, account_id, discovered, at, actor)?;
        tx.commit()?;
        Ok(delta)
    }

    /// The row, its secret, and its first catalog together: a creation that verified its key against the
    /// upstream must not leave a keyless row behind when a later statement fails.
    pub fn create_account(
        &self,
        row: &AccountRow,
        secret: Option<&Secret>,
        catalog: Option<&[(String, bool)]>,
        catalog_error: Option<&str>,
        at: i64,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let identity_json = row
            .identity
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        tx.execute(
            "INSERT INTO accounts (id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, catalog_error, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?10, ?6, 1, ?7, ?8, ?9, ?9)",
            params![
                row.id,
                row.kind,
                row.label,
                row.enabled,
                identity_json,
                row.lifecycle_generation as i64,
                row.state,
                catalog_error,
                at,
                crate::accounts::next_incarnation(&tx, &row.id)? as i64,
            ],
        )?;
        if let Some(secret) = secret {
            store_api_key_secret(&tx, &self.key, &row.id, secret)?;
        }
        if let Some(catalog) = catalog {
            apply_catalog_in(&tx, &row.id, catalog, at, actor)?;
        }
        record_change(
            &tx,
            at,
            actor,
            "account_created",
            &row.id,
            &serde_json::json!({
                "kind": row.kind.as_str(),
                "label": row.label,
            })
            .to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A verified edit lands whole: discovery ran against the staged secret, and the row moving underneath
    /// (a disconnect, a deletion) means the verdict is about a slot that no longer exists.
    pub fn commit_account_update(&self, u: &AccountUpdate<'_>) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            "UPDATE accounts SET label = ?2, state = ?3, catalog_error = ?4, \
             revision = revision + 1, updated_at = ?5 \
             WHERE id = ?1 AND lifecycle_generation = ?6 AND revision = ?7",
            params![
                u.id,
                u.label,
                u.state,
                u.catalog_error,
                u.at,
                u.lifecycle_generation as i64,
                u.expected_revision as i64,
            ],
        )?;
        if updated == 0 {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                params![u.id],
                |r| r.get(0),
            )?;
            return Err(if exists > 0 {
                StoreError::Stale
            } else {
                StoreError::AccountNotFound(u.id.to_string())
            });
        }
        if let Some(secret) = u.secret {
            store_api_key_secret(&tx, &self.key, u.id, secret)?;
        }
        if let Some(catalog) = u.catalog {
            apply_catalog_in(&tx, u.id, catalog, u.at, u.actor)?;
        }
        record_change(
            &tx,
            u.at,
            u.actor,
            "account_updated",
            u.id,
            &serde_json::json!({
                "label": u.label,
                "secret": u.secret.is_some(),
                "state": u.state.as_str(),
            })
            .to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// The sync inside a caller's transaction, so a creation or a verified edit can carry its catalog.
fn apply_catalog_in(
    tx: &Transaction<'_>,
    account_id: &str,
    discovered: &[(String, bool)],
    at: i64,
    actor: &str,
) -> Result<SyncDelta, StoreError> {
    let existing: Vec<(String, ModelSource)> = tx
        .prepare("SELECT upstream_id, source FROM account_models WHERE account_id = ?1")?
        .query_map(params![account_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let first_sync = existing.is_empty();
    let mut delta = SyncDelta {
        first_sync,
        ..SyncDelta::default()
    };
    for (upstream_id, long_context) in discovered {
        let known = existing.iter().any(|(id, _)| id == upstream_id);
        if known {
            tx.execute(
                "UPDATE account_models SET long_context = ?3, missing = 0 WHERE account_id = ?1 AND upstream_id = ?2",
                params![account_id, upstream_id, long_context],
            )?;
            continue;
        }
        // A clash on canonical_id with an existing row of another spelling is the upstream's own doing; the
        // sync records the error rather than silently dropping the id.
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO account_models (account_id, upstream_id, canonical_id, selected, source, long_context, missing, is_new) \
             VALUES (?1, ?2, ?3, ?4, 'discovered', ?5, 0, ?6)",
            params![account_id, upstream_id, canonical_id(upstream_id), first_sync, long_context, !first_sync],
        )?;
        if inserted == 0 {
            return Err(StoreError::CanonicalClash {
                account_id: account_id.to_string(),
                canonical_id: canonical_id(upstream_id),
            });
        }
        delta.added.push(upstream_id.clone());
    }
    for (upstream_id, source) in &existing {
        if *source == ModelSource::Discovered && !discovered.iter().any(|(id, _)| id == upstream_id)
        {
            tx.execute(
                "UPDATE account_models SET missing = 1 WHERE account_id = ?1 AND upstream_id = ?2",
                params![account_id, upstream_id],
            )?;
            delta.missing.push(upstream_id.clone());
        }
    }
    tx.execute(
        "UPDATE accounts SET catalog_synced_at = ?2, catalog_error = NULL WHERE id = ?1",
        params![account_id, at],
    )?;
    record_change(
        tx,
        at,
        actor,
        "catalog_synced",
        account_id,
        &serde_json::json!({ "added": delta.added, "missing": delta.missing }).to_string(),
    )?;
    Ok(delta)
}

impl Store {
    /// Seeds a catalog that has nothing in it; returns whether anything was written.
    pub fn seed_catalog(
        &self,
        account_id: &str,
        ids: &[(String, bool)],
        at: i64,
        actor: &str,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM account_models WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?;
        if count > 0 {
            return Ok(false);
        }
        for (upstream_id, long_context) in ids {
            tx.execute(
                "INSERT INTO account_models (account_id, upstream_id, canonical_id, selected, source, long_context, missing, is_new) \
                 VALUES (?1, ?2, ?3, 1, 'discovered', ?4, 0, 0)",
                params![account_id, upstream_id, canonical_id(upstream_id), long_context],
            )?;
        }
        record_change(
            &tx,
            at,
            actor,
            "catalog_seeded",
            account_id,
            &serde_json::json!({ "count": ids.len() }).to_string(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn put_served_models(
        &self,
        account_id: &str,
        rows: &[ServedModelWrite],
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            if !seen.insert(canonical_id(&row.upstream_id)) {
                return Err(StoreError::CanonicalClash {
                    account_id: account_id.to_string(),
                    canonical_id: canonical_id(&row.upstream_id),
                });
            }
        }
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = ?1",
            params![account_id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(StoreError::AccountNotFound(account_id.to_string()));
        }
        let listed: Vec<&str> = rows.iter().map(|r| r.upstream_id.as_str()).collect();
        let manual: Vec<String> = tx
            .prepare("SELECT upstream_id FROM account_models WHERE account_id = ?1 AND source = 'manual'")?
            .query_map(params![account_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in manual.iter().filter(|id| !listed.contains(&id.as_str())) {
            tx.execute(
                "DELETE FROM account_models WHERE account_id = ?1 AND upstream_id = ?2",
                params![account_id, id],
            )?;
        }
        for row in rows {
            let updated = tx.execute(
                "UPDATE account_models SET selected = ?3, long_context = ?4, is_new = 0 WHERE account_id = ?1 AND upstream_id = ?2",
                params![account_id, row.upstream_id, row.selected, row.long_context],
            )?;
            if updated == 0 {
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO account_models (account_id, upstream_id, canonical_id, selected, source, long_context, missing, is_new) \
                     VALUES (?1, ?2, ?3, ?4, 'manual', ?5, 0, 0)",
                    params![account_id, row.upstream_id, canonical_id(&row.upstream_id), row.selected, row.long_context],
                )?;
                if inserted == 0 {
                    return Err(StoreError::CanonicalClash {
                        account_id: account_id.to_string(),
                        canonical_id: canonical_id(&row.upstream_id),
                    });
                }
            }
        }
        record_change(
            &tx,
            now_ms(),
            actor,
            "models_put",
            account_id,
            &serde_json::json!({ "count": rows.len() }).to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The kind of the newest catalog change the account carries, which says whether what it serves now came
    /// from a discovery or from the built-in seed. Scanning the change log from the oldest row instead would
    /// read stale history once the log outgrows any fixed limit.
    pub fn latest_catalog_change(&self, account_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            "SELECT kind FROM registry_changes WHERE target = ?1 AND kind IN ('catalog_seeded', 'catalog_synced') ORDER BY id DESC LIMIT 1",
            params![account_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Where a mirror starts: the newest change, read off the primary key rather than by loading a log
    /// nothing prunes.
    pub fn newest_registry_change_id(&self) -> Result<Option<u64>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        Ok(conn
            .query_row("SELECT MAX(id) FROM registry_changes", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
            .map(|id| id as u64))
    }

    pub fn registry_changes_after(
        &self,
        id: u64,
        limit: usize,
    ) -> Result<Vec<RegistryChange>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt = conn.prepare("SELECT id, at, actor, kind, target, diff_json FROM registry_changes WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = stmt.query_map(params![id as i64, limit as i64], |r| {
            Ok(RegistryChange {
                id: r.get::<_, i64>(0)? as u64,
                at: r.get(1)?,
                actor: r.get(2)?,
                kind: r.get(3)?,
                target: r.get(4)?,
                diff_json: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }
}

const POOL_SELECT: &str = "SELECT id, label, enabled, strategy, affinity_ttl_seconds, overage_policy, migrate_above_utilization, select_below_utilization, default_allowed_models_json, keepalive_json, body_capture_hours, created_at, updated_at, prefer_subscription, api_key_sub_strategy, cache_ttl_json, cache_diagnostics FROM pools";
const KEY_SELECT: &str = "SELECT id, label, notes, pool_id, enabled, revoked_at, expires_at, allowed_models_json, keepalive_opt_out, body_capture_hours, secret_digest, prefix, created_at, rotated_at, last_used_at, retired_secret_last_used_at FROM api_keys";

fn all_pools(conn: &rusqlite::Connection) -> Result<Vec<PoolRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("{POOL_SELECT} ORDER BY label, id"))?;
    let rows = stmt.query_map([], row_to_pool)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

fn all_keys(conn: &rusqlite::Connection) -> Result<Vec<ApiKeyRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("{KEY_SELECT} ORDER BY label, id"))?;
    let rows = stmt.query_map([], row_to_key)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

fn row_to_pool(r: &rusqlite::Row<'_>) -> rusqlite::Result<PoolRow> {
    let list: Option<String> = r.get(8)?;
    Ok(PoolRow {
        id: r.get(0)?,
        label: r.get(1)?,
        enabled: r.get(2)?,
        strategy: r.get(3)?,
        affinity_ttl_seconds: r.get::<_, i64>(4)? as u64,
        overage_policy: r.get(5)?,
        migrate_above_utilization: r.get(6)?,
        select_below_utilization: r.get(7)?,
        default_allowed_models: json_list(list).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?,
        keepalive_json: r.get(9)?,
        body_capture_hours: r.get::<_, i64>(10)? as u32,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        prefer_subscription: r.get(13)?,
        api_key_sub_strategy: r.get(14)?,
        cache_ttl_json: r.get(15)?,
        cache_diagnostics: r.get::<_, Option<bool>>(16)?,
    })
}

fn row_to_key(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyRow> {
    let list: Option<String> = r.get(7)?;
    Ok(ApiKeyRow {
        id: r.get(0)?,
        label: r.get(1)?,
        notes: r.get(2)?,
        pool_id: r.get(3)?,
        enabled: r.get(4)?,
        revoked_at: r.get(5)?,
        expires_at: r.get(6)?,
        allowed_models: json_list(list).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?,
        keepalive_opt_out: r.get(8)?,
        body_capture_hours: r.get::<_, Option<i64>>(9)?.map(|h| h as u32),
        secret_digest: r.get(10)?,
        prefix: r.get(11)?,
        created_at: r.get(12)?,
        rotated_at: r.get(13)?,
        last_used_at: r.get(14)?,
        retired_secret_last_used_at: r.get(15)?,
    })
}

fn row_to_model(r: &rusqlite::Row<'_>) -> rusqlite::Result<ServedModel> {
    Ok(ServedModel {
        account_id: r.get(0)?,
        upstream_id: r.get(1)?,
        canonical_id: r.get(2)?,
        selected: r.get(3)?,
        source: r.get(4)?,
        long_context: r.get(5)?,
        missing: r.get(6)?,
        is_new: r.get(7)?,
    })
}

fn pool_diff(row: &PoolRow) -> String {
    serde_json::json!({
        "label": row.label,
        "enabled": row.enabled,
        "strategy": row.strategy.as_str(),
        "affinity_ttl_seconds": row.affinity_ttl_seconds,
        "overage_policy": row.overage_policy.as_str(),
        "migrate_above_utilization": row.migrate_above_utilization,
        "select_below_utilization": row.select_below_utilization,
        "default_allowed_models": row.default_allowed_models,
        "body_capture_hours": row.body_capture_hours,
        "prefer_subscription": row.prefer_subscription,
        "api_key_sub_strategy": row.api_key_sub_strategy.as_str(),
        "cache_ttl": serde_json::from_str::<serde_json::Value>(&row.cache_ttl_json).unwrap_or(serde_json::Value::Null),
        "cache_diagnostics": row.cache_diagnostics,
    })
    .to_string()
}

/// Never the digest: the change log is read by the ledger and the UI.
fn key_diff(row: &ApiKeyRow) -> String {
    serde_json::json!({
        "label": row.label,
        "pool_id": row.pool_id,
        "enabled": row.enabled,
        "expires_at": row.expires_at,
        "allowed_models": row.allowed_models,
        "keepalive_opt_out": row.keepalive_opt_out,
        "body_capture_hours": row.body_capture_hours,
        "prefix": row.prefix,
    })
    .to_string()
}
