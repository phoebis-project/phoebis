//! Registry rows: pools, their members, the router's own API keys, the served models of each account, and the
//! change log every write appends to.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::Ordering;

use crate::credentials::store_api_key_secret;
use crate::{AccountRow, AccountState, Secret, StateCause, Store, StoreError, LOCK_POISONED};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strategy {
    #[default]
    Pace,
    Priority,
    RoundRobin,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Pace => "pace",
            Strategy::Priority => "priority",
            Strategy::RoundRobin => "round_robin",
        }
    }
}

impl FromSql for Strategy {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "pace" => Ok(Strategy::Pace),
            "priority" => Ok(Strategy::Priority),
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

/// Which pins a shedding round may move once the cold ones are moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WarmSessions {
    Always,
    Never,
    #[default]
    WhenNeeded,
}

impl WarmSessions {
    pub fn as_str(self) -> &'static str {
        match self {
            WarmSessions::Always => "always",
            WarmSessions::Never => "never",
            WarmSessions::WhenNeeded => "when_needed",
        }
    }

    pub fn parse(value: &str) -> Option<WarmSessions> {
        match value {
            "always" => Some(WarmSessions::Always),
            "never" => Some(WarmSessions::Never),
            "when_needed" => Some(WarmSessions::WhenNeeded),
            _ => None,
        }
    }
}

impl FromSql for WarmSessions {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        WarmSessions::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for WarmSessions {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// The load-shedding knobs of a pool; `Default` is the balanced preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shedding {
    pub shed_share: f64,
    pub burn_window_seconds: u64,
    pub source_dwell_seconds: u64,
    pub migrate_when_spent: bool,
    pub warm_sessions: WarmSessions,
}

impl Default for Shedding {
    fn default() -> Self {
        Shedding {
            shed_share: 0.5,
            burn_window_seconds: 600,
            source_dwell_seconds: 900,
            migrate_when_spent: true,
            warm_sessions: WarmSessions::WhenNeeded,
        }
    }
}

/// The failover, overage-grace and wind-down knobs of a pool; `Default` is the shipped preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Failover {
    pub retry_in_place_seconds: u64,
    pub in_flight_cap: u32,
    pub pace_floor: f64,
    pub overage_move_grace_minutes: u32,
    pub wind_down_minutes: u32,
}

impl Default for Failover {
    fn default() -> Self {
        Failover {
            retry_in_place_seconds: 60,
            in_flight_cap: 16,
            pace_floor: 0.05,
            overage_move_grace_minutes: 30,
            wind_down_minutes: 30,
        }
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

/// Whether a session or a subagent cohort holds its pin to one account regardless of cache evidence, or only
/// while the evidence still confirms a warm prefix there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AffinityMode {
    #[default]
    Always,
    WhenCached,
}

impl AffinityMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AffinityMode::Always => "always",
            AffinityMode::WhenCached => "when_cached",
        }
    }

    pub fn parse(value: &str) -> Option<AffinityMode> {
        match value {
            "always" => Some(AffinityMode::Always),
            "when_cached" => Some(AffinityMode::WhenCached),
            _ => None,
        }
    }
}

impl FromSql for AffinityMode {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        AffinityMode::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for AffinityMode {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// Where a subagent of a pinned session lands without a pin of its own: by its cohort's own placement rules, by
/// the pool's ordinary selection strategy, or beside the session's main agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubagentPlacement {
    ByCohort,
    #[default]
    ByStrategy,
    WithMain,
}

impl SubagentPlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            SubagentPlacement::ByCohort => "by_cohort",
            SubagentPlacement::ByStrategy => "by_strategy",
            SubagentPlacement::WithMain => "with_main",
        }
    }

    pub fn parse(value: &str) -> Option<SubagentPlacement> {
        match value {
            "by_cohort" => Some(SubagentPlacement::ByCohort),
            "by_strategy" => Some(SubagentPlacement::ByStrategy),
            "with_main" => Some(SubagentPlacement::WithMain),
            _ => None,
        }
    }
}

impl FromSql for SubagentPlacement {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        SubagentPlacement::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for SubagentPlacement {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// Where a fresh session with no pin of its own lands: by the pool's ordinary selection strategy, or on whichever
/// account holds the warmest matching prefix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionPlacement {
    #[default]
    ByStrategy,
    ByWarmPrefix,
}

impl SessionPlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionPlacement::ByStrategy => "by_strategy",
            SessionPlacement::ByWarmPrefix => "by_warm_prefix",
        }
    }

    pub fn parse(value: &str) -> Option<SessionPlacement> {
        match value {
            "by_strategy" => Some(SessionPlacement::ByStrategy),
            "by_warm_prefix" => Some(SessionPlacement::ByWarmPrefix),
            _ => None,
        }
    }
}

impl FromSql for SessionPlacement {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        SessionPlacement::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for SessionPlacement {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// The period a member's spend budget resets over.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub enum BudgetPeriod {
    Day,
    #[default]
    Month,
}

impl BudgetPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetPeriod::Day => "day",
            BudgetPeriod::Month => "month",
        }
    }

    pub fn parse(value: &str) -> Option<BudgetPeriod> {
        match value {
            "day" => Some(BudgetPeriod::Day),
            "month" => Some(BudgetPeriod::Month),
            _ => None,
        }
    }
}

impl FromSql for BudgetPeriod {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        BudgetPeriod::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

impl ToSql for BudgetPeriod {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

/// The pin and placement knobs of a pool beside `affinity_ttl_seconds`; `Default` is the shipped preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Affinity {
    pub mode: AffinityMode,
    pub subagent_placement: SubagentPlacement,
    pub session_placement: SessionPlacement,
    pub min_shared_prefix_tokens: u64,
}

impl Default for Affinity {
    fn default() -> Self {
        Affinity {
            mode: AffinityMode::Always,
            subagent_placement: SubagentPlacement::ByStrategy,
            session_placement: SessionPlacement::ByStrategy,
            min_shared_prefix_tokens: 20_000,
        }
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
    pub shed_ahead_of_pace: Option<f64>,
    pub select_below_utilization: Option<f64>,
    pub model_rules: Vec<ModelRule>,
    pub keepalive_json: String,
    pub body_capture_hours: u32,
    /// Order the selection set by account kind before priority; the operator's answer alone.
    pub prefer_subscription: bool,
    pub shedding: Shedding,
    pub failover: Failover,
    pub cache_ttl_json: String,
    pub cache_diagnostics: bool,
    /// Whether the client's server-side auto mode checks travel; off, the relay drops them so the client
    /// falls back to its own classifier requests.
    pub auto_mode_server: bool,
    pub affinity: Affinity,
    /// How many times this id has held a pool. A deletion leaves the count behind in a tombstone and a pool created
    /// again under the id starts one above it, so an edit read from the deleted pool cannot land on its replacement.
    pub incarnation: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PoolRow {
    /// One function, so the selector, the pool route, and the console cannot answer differently about the
    /// same pool.
    pub fn kind_ordered(&self) -> bool {
        self.prefer_subscription
    }

    /// The shipped defaults with `id` and `label` set: a test overrides the fields its case is about, so a field
    /// added to or removed from the row touches this one site.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(id: &str) -> PoolRow {
        PoolRow {
            id: id.into(),
            label: id.into(),
            enabled: true,
            strategy: Strategy::Pace,
            affinity_ttl_seconds: 3600,
            overage_policy: OveragePolicy::PreferPlan,
            shed_ahead_of_pace: None,
            select_below_utilization: None,
            model_rules: Vec::new(),
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: true,
            shedding: Shedding::default(),
            failover: Failover::default(),
            cache_ttl_json: "{}".into(),
            cache_diagnostics: false,
            auto_mode_server: true,
            affinity: Affinity::default(),
            incarnation: 1,
            created_at: 0,
            updated_at: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PoolMember {
    pub pool_id: String,
    pub account_id: String,
    pub priority: u32,
    /// `None` is no budget over that period; a member may carry one, both, or neither.
    pub budget_daily_usd: Option<f64>,
    pub budget_monthly_usd: Option<f64>,
}

impl PoolMember {
    /// The budget over `period`, if the member carries one.
    pub fn budget(&self, period: BudgetPeriod) -> Option<f64> {
        match period {
            BudgetPeriod::Day => self.budget_daily_usd,
            BudgetPeriod::Month => self.budget_monthly_usd,
        }
    }

    /// Every budget the member carries, with its period, `Day` before `Month`.
    pub fn budgets(&self) -> impl Iterator<Item = (BudgetPeriod, f64)> + '_ {
        [BudgetPeriod::Day, BudgetPeriod::Month]
            .into_iter()
            .filter_map(|period| self.budget(period).map(|budget| (period, budget)))
    }

    pub fn budgeted(&self) -> bool {
        self.budgets().next().is_some()
    }
}

/// What `set_pool_members` writes for one member: its priority beside its own account and spend budgets, so a
/// caller cannot set one without the others.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberWrite {
    pub account_id: String,
    pub priority: u32,
    pub budget_daily_usd: Option<f64>,
    pub budget_monthly_usd: Option<f64>,
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
    pub model_rules: Vec<ModelRule>,
    pub model_rules_mode: RulesMode,
    /// Whether `GET /v1/models` under this key lists an `id[1m]` twin beside every long-context model.
    pub long_context_twins: bool,
    pub keepalive_opt_out: bool,
    pub body_capture_hours: Option<u32>,
    pub secret_digest: Vec<u8>,
    pub prefix: String,
    /// How many times this id has held a key. A deletion leaves the count behind in a tombstone and a key created
    /// again under the id starts one above it, so an edit read from the deleted key cannot land on its replacement.
    pub incarnation: u64,
    pub created_at: i64,
    /// The row's last write: the create, an edit, a rotation or a revocation, never a use's `last_used_at`. The
    /// create stamps `created_at`; every later write stamps the clock or one past the stored stamp, whichever is
    /// later, so two writes inside one millisecond never share a stamp and an edit fenced on it is exact.
    pub updated_at: i64,
    pub rotated_at: Option<i64>,
    pub last_used_at: Option<i64>,
    /// When the secret a rotation retired last authenticated; cleared by the next rotation.
    pub retired_secret_last_used_at: Option<i64>,
    /// `None` inherits the pool's `auto_mode_server`; `Some` overrides it for this key.
    pub auto_mode_server: Option<bool>,
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

/// What the engine knows about an account's paid term: the profile's start and trial end, the operator's flag
/// and the end the operator saved with it (set only while the flag is on).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionTerm {
    pub account_id: String,
    pub since: Option<i64>,
    pub trial_ends_at: Option<i64>,
    pub cancelled: bool,
    pub ends_at: Option<i64>,
    /// The profile's rate-limit tier, which prices the router's own use of the account's windows.
    pub tier: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RegistryLoad {
    pub accounts: Vec<crate::AccountRow>,
    /// The generation of each account's credential, for accounts that hold one. The snapshot carries it so a
    /// verdict about a replaced credential can be told from a current one without reading the store.
    pub credential_generations: Vec<(String, u64)>,
    pub keys: Vec<ApiKeyRow>,
    pub members: Vec<PoolMember>,
    pub pools: Vec<PoolRow>,
    pub retired_secrets: Vec<RetiredSecret>,
    /// Live keys whose id was deleted with an earlier key before this one was created, in the order the
    /// change log recorded it. The earlier holder's rollup rows from before the ledger carried a generation
    /// sit at generation 0 under the same id, so the startup stamp starts such a key's own rows at its
    /// creation rather than claiming every unstamped row of the id.
    pub reused_key_ids: Vec<String>,
    pub terms: Vec<SubscriptionTerm>,
}

/// Everything an account edit changes, staged so the admin route can run discovery and then commit every field in
/// one transaction.
#[derive(Clone, Copy, Debug)]
pub struct AccountUpdate<'a> {
    pub id: &'a str,
    /// The revision the edit was built from. The commit only lands while the row still carries it, so two edits
    /// built from one read cannot both land, and an endpoint and a secret nothing checked together never combine.
    pub expected_revision: u64,
    /// The incarnation the edit was built from. A slot deleted and created again under the same id restarts its
    /// revision and lifecycle generation at 1, so only this tells the replacement from the row the edit read.
    pub incarnation: u64,
    pub lifecycle_generation: u64,
    pub label: &'a str,
    pub secret: Option<&'a Secret>,
    /// The staged state with who moves it. `None` leaves both the state and its word.
    pub state: Option<(AccountState, StateCause)>,
    pub at: i64,
    pub actor: &'a str,
    /// The staged `enabled` flag; `None` leaves it.
    pub enabled: Option<bool>,
    /// The staged cancellation, the flag with its end; `None` leaves both.
    pub cancellation: Option<(bool, Option<i64>)>,
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

/// The stamp a fenced row takes on its next write: one past `previous` when the clock has not moved, so a write
/// inside the millisecond of the stamp it fenced on still moves it, and an edit read before it cannot pass.
fn stamp_past(previous: i64) -> i64 {
    now_ms().max(previous.saturating_add(1))
}

fn rules_json(rules: &[ModelRule]) -> Result<String, StoreError> {
    serde_json::to_string(rules).map_err(StoreError::from)
}

fn json_rules(raw: String) -> Result<Vec<ModelRule>, StoreError> {
    serde_json::from_str(&raw).map_err(StoreError::from)
}

impl Store {
    pub fn load_registry(&self) -> Result<RegistryLoad, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.enter_load();
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
                .prepare("SELECT pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd FROM pool_members ORDER BY pool_id, account_id")?
                .query_map([], |r| {
                    Ok(PoolMember {
                        pool_id: r.get(0)?,
                        account_id: r.get(1)?,
                        priority: r.get::<_, i64>(2)? as u32,
                        budget_daily_usd: r.get(3)?,
                        budget_monthly_usd: r.get(4)?,
                    })
                })?
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
            // follows it can share a millisecond, and the id is reused all the same. The history purge keeps
            // each id's newest create and delete whatever their age, so the create that made the live key and
            // the delete before it stay on record, and both lookups run on `registry_changes_target (target, id)`.
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
            terms: tx
                .prepare(
                    "SELECT id, subscription_since, trial_ends_at, cancelled, subscription_ends_at, \
                     subscription_tier FROM accounts ORDER BY id",
                )?
                .query_map([], |r| {
                    Ok(SubscriptionTerm {
                        account_id: r.get(0)?,
                        since: r.get(1)?,
                        trial_ends_at: r.get(2)?,
                        cancelled: r.get(3)?,
                        ends_at: r.get(4)?,
                        tier: r.get(5)?,
                    })
                })?
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

    pub fn insert_pool(&self, row: &PoolRow, actor: &str) -> Result<PoolRow, StoreError> {
        self.insert_pool_with_members(row, None, actor)
    }

    /// The pool row and its members under one `BEGIN IMMEDIATE`: a refused member (a duplicate, an unknown
    /// account, an account in another pool) leaves no row. Answers the committed row: the incarnation is taken inside
    /// the transaction, so the caller's row cannot know it, and the stamps are the store's own.
    pub fn insert_pool_with_members(
        &self,
        row: &PoolRow,
        members: Option<&[MemberWrite]>,
        actor: &str,
    ) -> Result<PoolRow, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let incarnation = crate::accounts::next_incarnation_in(&tx, "pool_tombstones", &row.id)?;
        tx.execute(
            "INSERT INTO pools (id, label, enabled, strategy, affinity_ttl_seconds, overage_policy, shed_ahead_of_pace, select_below_utilization, model_rules_json, keepalive_json, body_capture_hours, prefer_subscription, shed_share, burn_window_seconds, source_dwell_seconds, migrate_when_spent, warm_sessions, retry_in_place_seconds, in_flight_cap, pace_floor, overage_move_grace_minutes, wind_down_minutes, cache_ttl_json, cache_diagnostics, affinity_mode, subagent_placement, session_placement, min_shared_prefix_tokens, auto_mode_server, created_at, updated_at, incarnation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?30, ?31)",
            params![
                row.id,
                row.label,
                row.enabled,
                row.strategy,
                row.affinity_ttl_seconds as i64,
                row.overage_policy,
                row.shed_ahead_of_pace,
                row.select_below_utilization,
                rules_json(&row.model_rules)?,
                row.keepalive_json,
                row.body_capture_hours as i64,
                row.prefer_subscription,
                row.shedding.shed_share,
                row.shedding.burn_window_seconds as i64,
                row.shedding.source_dwell_seconds as i64,
                row.shedding.migrate_when_spent,
                row.shedding.warm_sessions,
                row.failover.retry_in_place_seconds as i64,
                row.failover.in_flight_cap as i64,
                row.failover.pace_floor,
                row.failover.overage_move_grace_minutes as i64,
                row.failover.wind_down_minutes as i64,
                row.cache_ttl_json,
                row.cache_diagnostics,
                row.affinity.mode,
                row.affinity.subagent_placement,
                row.affinity.session_placement,
                row.affinity.min_shared_prefix_tokens as i64,
                row.auto_mode_server,
                now,
                incarnation as i64
            ],
        )?;
        record_change(&tx, now, actor, "pool_created", &row.id, &pool_diff(row))?;
        if let Some(members) = members {
            set_pool_members_in(&tx, &row.id, members, now, actor)?;
        }
        let written = tx.query_row(
            &format!("{POOL_SELECT} WHERE id = ?1"),
            params![row.id],
            row_to_pool,
        )?;
        tx.commit()?;
        Ok(written)
    }

    /// The policy write fenced on the `updated_at` the caller read: `Stale` when another write moved the row since.
    pub fn update_pool(
        &self,
        row: &PoolRow,
        expected_updated_at: i64,
        actor: &str,
    ) -> Result<i64, StoreError> {
        self.update_pool_with_members(row, expected_updated_at, None, actor)
    }

    /// The policy and the members under one transaction, fenced on the `updated_at` the handler read and on `row`'s
    /// incarnation, so an edit read from a deleted pool never lands on one created again under its id. Answers the
    /// stamp written, which the next fenced edit reads.
    pub fn update_pool_with_members(
        &self,
        row: &PoolRow,
        expected_updated_at: i64,
        members: Option<&[MemberWrite]>,
        actor: &str,
    ) -> Result<i64, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.park_on_pool_write_hold();
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let at = stamp_past(expected_updated_at);
        let updated = tx.execute(
            "UPDATE pools SET label = ?2, enabled = ?3, strategy = ?4, affinity_ttl_seconds = ?5, overage_policy = ?6, shed_ahead_of_pace = ?7, select_below_utilization = ?8, model_rules_json = ?9, keepalive_json = ?10, body_capture_hours = ?11, prefer_subscription = ?12, shed_share = ?13, burn_window_seconds = ?14, source_dwell_seconds = ?15, migrate_when_spent = ?16, warm_sessions = ?17, retry_in_place_seconds = ?18, in_flight_cap = ?19, pace_floor = ?20, overage_move_grace_minutes = ?21, wind_down_minutes = ?22, cache_ttl_json = ?23, cache_diagnostics = ?24, affinity_mode = ?25, subagent_placement = ?26, session_placement = ?27, min_shared_prefix_tokens = ?28, auto_mode_server = ?29, updated_at = ?30 WHERE id = ?1 AND updated_at = ?31 AND incarnation = ?32",
            params![
                row.id,
                row.label,
                row.enabled,
                row.strategy,
                row.affinity_ttl_seconds as i64,
                row.overage_policy,
                row.shed_ahead_of_pace,
                row.select_below_utilization,
                rules_json(&row.model_rules)?,
                row.keepalive_json,
                row.body_capture_hours as i64,
                row.prefer_subscription,
                row.shedding.shed_share,
                row.shedding.burn_window_seconds as i64,
                row.shedding.source_dwell_seconds as i64,
                row.shedding.migrate_when_spent,
                row.shedding.warm_sessions,
                row.failover.retry_in_place_seconds as i64,
                row.failover.in_flight_cap as i64,
                row.failover.pace_floor,
                row.failover.overage_move_grace_minutes as i64,
                row.failover.wind_down_minutes as i64,
                row.cache_ttl_json,
                row.cache_diagnostics,
                row.affinity.mode,
                row.affinity.subagent_placement,
                row.affinity.session_placement,
                row.affinity.min_shared_prefix_tokens as i64,
                row.auto_mode_server,
                at,
                expected_updated_at,
                row.incarnation as i64
            ],
        )?;
        if updated == 0 {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM pools WHERE id = ?1",
                params![row.id],
                |r| r.get(0),
            )?;
            return Err(if exists > 0 {
                StoreError::Stale
            } else {
                StoreError::PoolNotFound(row.id.clone())
            });
        }
        record_change(&tx, at, actor, "pool_updated", &row.id, &pool_diff(row))?;
        if let Some(members) = members {
            set_pool_members_in(&tx, &row.id, members, at, actor)?;
        }
        tx.commit()?;
        Ok(at)
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
        let incarnation: Option<i64> = tx
            .query_row(
                "SELECT incarnation FROM pools WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let incarnation = incarnation.ok_or_else(|| StoreError::PoolNotFound(id.to_string()))?;
        tx.execute("DELETE FROM pools WHERE id = ?1", params![id])?;
        tx.execute(
            "INSERT INTO pool_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(pool_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
        )?;
        record_change(&tx, now_ms(), actor, "pool_deleted", id, "{}")?;
        tx.commit()?;
        Ok(())
    }

    pub fn pool_members(&self, pool_id: &str) -> Result<Vec<PoolMember>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        let mut stmt =
            conn.prepare("SELECT pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd FROM pool_members WHERE pool_id = ?1 ORDER BY priority, account_id")?;
        let rows = stmt.query_map(params![pool_id], |r| {
            Ok(PoolMember {
                pool_id: r.get(0)?,
                account_id: r.get(1)?,
                priority: r.get::<_, i64>(2)? as u32,
                budget_daily_usd: r.get(3)?,
                budget_monthly_usd: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// Answers the pool's new `updated_at`.
    pub fn set_pool_members(
        &self,
        pool_id: &str,
        members: &[MemberWrite],
        actor: &str,
    ) -> Result<i64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT updated_at FROM pools WHERE id = ?1",
                params![pool_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::PoolNotFound(pool_id.to_string()));
        };
        let at = stamp_past(current);
        set_pool_members_in(&tx, pool_id, members, at, actor)?;
        tx.commit()?;
        Ok(at)
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

    /// Answers the committed row, with the `created_at` it stamped (the row's `updated_at` too) and the incarnation it
    /// took: a caller's own clock is not the generation, and a selector or a response built from the caller's row
    /// would name the wrong holder after a re-create.
    pub fn insert_api_key(&self, row: &ApiKeyRow, actor: &str) -> Result<ApiKeyRow, StoreError> {
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
        // previous holder was created in must not share it: the change log keeps the newest create under the
        // id through the history purge (served by `registry_changes_target`), and the stamp goes one past it
        // when the clock has not moved.
        let previous: Option<i64> = tx.query_row(
            "SELECT MAX(at) FROM registry_changes WHERE target = ?1 AND kind = 'key_created'",
            params![row.id],
            |r| r.get(0),
        )?;
        let now = previous.map_or(now_ms(), |p| now_ms().max(p + 1));
        let incarnation = crate::accounts::next_incarnation_in(&tx, "api_key_tombstones", &row.id)?;
        tx.execute(
            "INSERT INTO api_keys (id, label, notes, pool_id, enabled, revoked_at, expires_at, model_rules_json, model_rules_mode, long_context_twins, keepalive_opt_out, body_capture_hours, secret_digest, prefix, auto_mode_server, created_at, updated_at, rotated_at, last_used_at, retired_secret_last_used_at, incarnation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16, NULL, NULL, NULL, ?17)",
            params![
                row.id,
                row.label,
                row.notes,
                row.pool_id,
                row.enabled,
                row.revoked_at,
                row.expires_at,
                rules_json(&row.model_rules)?,
                row.model_rules_mode,
                row.long_context_twins,
                row.keepalive_opt_out,
                row.body_capture_hours.map(|h| h as i64),
                row.secret_digest,
                row.prefix,
                row.auto_mode_server,
                now,
                incarnation as i64
            ],
        )?;
        record_change(&tx, now, actor, "key_created", &row.id, &key_diff(row))?;
        let written = tx.query_row(
            &format!("{KEY_SELECT} WHERE id = ?1"),
            params![row.id],
            row_to_key,
        )?;
        tx.commit()?;
        Ok(written)
    }

    /// The edit fenced on the `updated_at` the caller read and on `row`'s incarnation: `Stale` when another write moved
    /// the row since, or the key was deleted and created again. Answers the `updated_at` it stamped, which a response
    /// built from `row` carries.
    pub fn update_api_key(
        &self,
        row: &ApiKeyRow,
        expected_updated_at: i64,
        actor: &str,
    ) -> Result<i64, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.park_on_key_write_hold();
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
        let at = stamp_past(expected_updated_at);
        let updated = tx.execute(
            "UPDATE api_keys SET label = ?2, notes = ?3, pool_id = ?4, enabled = ?5, expires_at = ?6, model_rules_json = ?7, model_rules_mode = ?8, long_context_twins = ?9, keepalive_opt_out = ?10, body_capture_hours = ?11, auto_mode_server = ?12, updated_at = ?13 WHERE id = ?1 AND updated_at = ?14 AND incarnation = ?15",
            params![
                row.id,
                row.label,
                row.notes,
                row.pool_id,
                row.enabled,
                row.expires_at,
                rules_json(&row.model_rules)?,
                row.model_rules_mode,
                row.long_context_twins,
                row.keepalive_opt_out,
                row.body_capture_hours.map(|h| h as i64),
                row.auto_mode_server,
                at,
                expected_updated_at,
                row.incarnation as i64
            ],
        )?;
        if updated == 0 {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM api_keys WHERE id = ?1",
                params![row.id],
                |r| r.get(0),
            )?;
            return Err(if exists > 0 {
                StoreError::Stale
            } else {
                StoreError::KeyNotFound(row.id.clone())
            });
        }
        record_change(&tx, at, actor, "key_updated", &row.id, &key_diff(row))?;
        tx.commit()?;
        Ok(at)
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
            "UPDATE api_keys SET secret_digest = ?2, prefix = ?3, rotated_at = ?4, retired_secret_last_used_at = NULL, updated_at = MAX(?5, updated_at + 1) WHERE id = ?1",
            params![id, digest, prefix, at, now_ms()],
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
        // The stamp moves too, so an edit read before the revocation cannot write `enabled` back on.
        let updated = tx.execute(
            "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ?2), enabled = 0, \
             updated_at = MAX(?3, updated_at + 1) WHERE id = ?1",
            params![id, at, now_ms()],
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
        let incarnation: Option<i64> = tx
            .query_row(
                "SELECT incarnation FROM api_keys WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let incarnation = incarnation.ok_or_else(|| StoreError::KeyNotFound(id.to_string()))?;
        tx.execute("DELETE FROM api_keys WHERE id = ?1", params![id])?;
        tx.execute(
            "INSERT INTO api_key_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(api_key_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
        )?;
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

    /// The row and its secret together: a creation that verified its key against the upstream must not leave
    /// a keyless row behind when the secret write fails.
    pub fn create_account(
        &self,
        row: &AccountRow,
        secret: Option<&Secret>,
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
            "INSERT INTO accounts (id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, state, state_cause, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?9, ?6, 1, ?7, ?10, ?8, ?8)",
            params![
                row.id,
                row.kind,
                row.label,
                row.enabled,
                identity_json,
                row.lifecycle_generation as i64,
                row.state,
                at,
                crate::accounts::next_incarnation(&tx, &row.id)? as i64,
                row.state_cause,
            ],
        )?;
        if let Some(secret) = secret {
            store_api_key_secret(&tx, &self.key, &row.id, secret)?;
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

    /// An edit lands whole, in one revision: every staged field or none. Discovery ran against the staged secret,
    /// and the row moving underneath (another edit, a disconnect, a deletion, a replacement under the same id) means
    /// the edit is about a row that no longer exists as it was read.
    pub fn commit_account_update(&self, u: &AccountUpdate<'_>) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let cancelled = u.cancellation.map(|(cancelled, _)| cancelled);
        let ends_at = u.cancellation.and_then(|(_, ends_at)| ends_at);
        let (state, cause) = u.state.unzip();
        // The end goes with the flag, as `set_cancelled` writes them: dropped the moment the flag is off.
        let updated = tx.execute(
            "UPDATE accounts SET label = ?2, state = COALESCE(?3, state), \
             state_cause = COALESCE(?11, state_cause), enabled = COALESCE(?7, enabled), \
             cancelled = COALESCE(?8, cancelled), \
             subscription_ends_at = CASE WHEN ?8 IS NULL THEN subscription_ends_at WHEN ?8 THEN ?9 ELSE NULL END, \
             revision = revision + 1, updated_at = ?4 \
             WHERE id = ?1 AND incarnation = ?10 AND lifecycle_generation = ?5 AND revision = ?6",
            params![
                u.id,
                u.label,
                state,
                u.at,
                u.lifecycle_generation as i64,
                u.expected_revision as i64,
                u.enabled,
                cancelled,
                ends_at,
                u.incarnation as i64,
                cause,
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
        record_change(
            &tx,
            u.at,
            u.actor,
            "account_updated",
            u.id,
            &serde_json::json!({
                "label": u.label,
                "secret": u.secret.is_some(),
                "state": state.map(AccountState::as_str),
                "enabled": u.enabled,
                "cancelled": cancelled,
                "subscription_ends_at": ends_at,
            })
            .to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Where a mirror starts: the newest change, read off the primary key rather than by loading a log the
    /// history purge cuts only at the retention.
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

/// The member half of a pool write on the caller's transaction: every named account checked, the list replaced
/// whole, and the pool's `updated_at` moved to `at`, so an edit fenced on the stamp it read before this write is
/// refused rather than landing over it.
fn set_pool_members_in(
    tx: &Transaction<'_>,
    pool_id: &str,
    members: &[MemberWrite],
    at: i64,
    actor: &str,
) -> Result<(), StoreError> {
    if tx.execute(
        "UPDATE pools SET updated_at = ?2 WHERE id = ?1",
        params![pool_id, at],
    )? == 0
    {
        return Err(StoreError::PoolNotFound(pool_id.to_string()));
    }
    for member in members {
        if tx.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = ?1",
            params![member.account_id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(StoreError::AccountNotFound(member.account_id.clone()));
        }
    }
    // An account belongs to one pool. A member of the pool being written is fine, since the list replaces it.
    for member in members {
        let held: Option<String> = tx
            .query_row(
                "SELECT pool_id FROM pool_members WHERE account_id = ?1 AND pool_id != ?2",
                params![member.account_id, pool_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(held) = held {
            return Err(StoreError::AccountInPool {
                account_id: member.account_id.clone(),
                pool_id: held,
            });
        }
    }
    tx.execute(
        "DELETE FROM pool_members WHERE pool_id = ?1",
        params![pool_id],
    )?;
    for member in members {
        tx.execute(
            "INSERT INTO pool_members (pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![pool_id, member.account_id, member.priority as i64, member.budget_daily_usd, member.budget_monthly_usd],
        )?;
    }
    let diff = serde_json::json!({ "members": members.iter().map(|m| serde_json::json!({
        "account_id": m.account_id, "priority": m.priority,
        "budget_daily_usd": m.budget_daily_usd, "budget_monthly_usd": m.budget_monthly_usd,
    })).collect::<Vec<_>>() })
    .to_string();
    record_change(tx, at, actor, "pool_members_set", pool_id, &diff)?;
    Ok(())
}

const POOL_SELECT: &str = "SELECT id, label, enabled, strategy, affinity_ttl_seconds, overage_policy, shed_ahead_of_pace, select_below_utilization, model_rules_json, keepalive_json, body_capture_hours, created_at, updated_at, prefer_subscription, shed_share, burn_window_seconds, source_dwell_seconds, migrate_when_spent, warm_sessions, retry_in_place_seconds, in_flight_cap, pace_floor, overage_move_grace_minutes, wind_down_minutes, cache_ttl_json, cache_diagnostics, affinity_mode, subagent_placement, session_placement, min_shared_prefix_tokens, auto_mode_server, incarnation FROM pools";
const KEY_SELECT: &str = "SELECT id, label, notes, pool_id, enabled, revoked_at, expires_at, keepalive_opt_out, body_capture_hours, secret_digest, prefix, created_at, rotated_at, last_used_at, retired_secret_last_used_at, model_rules_json, model_rules_mode, long_context_twins, auto_mode_server, updated_at, incarnation FROM api_keys";

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
    let rules: String = r.get(8)?;
    Ok(PoolRow {
        id: r.get(0)?,
        label: r.get(1)?,
        enabled: r.get(2)?,
        strategy: r.get(3)?,
        affinity_ttl_seconds: r.get::<_, i64>(4)? as u64,
        overage_policy: r.get(5)?,
        shed_ahead_of_pace: r.get(6)?,
        select_below_utilization: r.get(7)?,
        model_rules: json_rules(rules).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?,
        keepalive_json: r.get(9)?,
        body_capture_hours: r.get::<_, i64>(10)? as u32,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        prefer_subscription: r.get(13)?,
        shedding: Shedding {
            shed_share: r.get(14)?,
            burn_window_seconds: r.get::<_, i64>(15)? as u64,
            source_dwell_seconds: r.get::<_, i64>(16)? as u64,
            migrate_when_spent: r.get(17)?,
            warm_sessions: r.get(18)?,
        },
        failover: Failover {
            retry_in_place_seconds: r.get::<_, i64>(19)? as u64,
            in_flight_cap: r.get::<_, i64>(20)? as u32,
            pace_floor: r.get(21)?,
            overage_move_grace_minutes: r.get::<_, i64>(22)? as u32,
            wind_down_minutes: r.get::<_, i64>(23)? as u32,
        },
        cache_ttl_json: r.get(24)?,
        cache_diagnostics: r.get::<_, bool>(25)?,
        affinity: Affinity {
            mode: r.get(26)?,
            subagent_placement: r.get(27)?,
            session_placement: r.get(28)?,
            min_shared_prefix_tokens: r.get::<_, i64>(29)? as u64,
        },
        auto_mode_server: r.get(30)?,
        incarnation: r.get::<_, i64>(31)? as u64,
    })
}

fn row_to_key(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyRow> {
    let rules: String = r.get(15)?;
    Ok(ApiKeyRow {
        id: r.get(0)?,
        label: r.get(1)?,
        notes: r.get(2)?,
        pool_id: r.get(3)?,
        enabled: r.get(4)?,
        revoked_at: r.get(5)?,
        expires_at: r.get(6)?,
        model_rules: json_rules(rules).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(15, rusqlite::types::Type::Text, Box::new(e))
        })?,
        model_rules_mode: r.get(16)?,
        long_context_twins: r.get(17)?,
        keepalive_opt_out: r.get(7)?,
        body_capture_hours: r.get::<_, Option<i64>>(8)?.map(|h| h as u32),
        secret_digest: r.get(9)?,
        prefix: r.get(10)?,
        incarnation: r.get::<_, i64>(20)? as u64,
        created_at: r.get(11)?,
        updated_at: r.get(19)?,
        rotated_at: r.get(12)?,
        last_used_at: r.get(13)?,
        retired_secret_last_used_at: r.get(14)?,
        auto_mode_server: r.get::<_, Option<bool>>(18)?,
    })
}

fn pool_diff(row: &PoolRow) -> String {
    serde_json::json!({
        "label": row.label,
        "enabled": row.enabled,
        "strategy": row.strategy.as_str(),
        "affinity_ttl_seconds": row.affinity_ttl_seconds,
        "overage_policy": row.overage_policy.as_str(),
        "shed_ahead_of_pace": row.shed_ahead_of_pace,
        "select_below_utilization": row.select_below_utilization,
        "model_rules": row.model_rules,
        "body_capture_hours": row.body_capture_hours,
        "prefer_subscription": row.prefer_subscription,
        "shed_share": row.shedding.shed_share,
        "burn_window_seconds": row.shedding.burn_window_seconds,
        "source_dwell_seconds": row.shedding.source_dwell_seconds,
        "migrate_when_spent": row.shedding.migrate_when_spent,
        "warm_sessions": row.shedding.warm_sessions.as_str(),
        "retry_in_place_seconds": row.failover.retry_in_place_seconds,
        "in_flight_cap": row.failover.in_flight_cap,
        "pace_floor": row.failover.pace_floor,
        "overage_move_grace_minutes": row.failover.overage_move_grace_minutes,
        "wind_down_minutes": row.failover.wind_down_minutes,
        "cache_ttl": serde_json::from_str::<serde_json::Value>(&row.cache_ttl_json).unwrap_or(serde_json::Value::Null),
        "cache_diagnostics": row.cache_diagnostics,
        "affinity_mode": row.affinity.mode.as_str(),
        "subagent_placement": row.affinity.subagent_placement.as_str(),
        "session_placement": row.affinity.session_placement.as_str(),
        "min_shared_prefix_tokens": row.affinity.min_shared_prefix_tokens,
        "auto_mode_server": row.auto_mode_server,
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
        "model_rules": row.model_rules,
        "model_rules_mode": row.model_rules_mode.as_str(),
        "long_context_twins": row.long_context_twins,
        "keepalive_opt_out": row.keepalive_opt_out,
        "body_capture_hours": row.body_capture_hours,
        "prefix": row.prefix,
        "auto_mode_server": row.auto_mode_server,
    })
    .to_string()
}
