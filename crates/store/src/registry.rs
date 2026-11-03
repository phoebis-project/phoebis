//! Registry rows: pools, their members, the router's own API keys, the served models of each account, and the
//! change log every write appends to.

use phoebis_guardrails::{Condition, GuardrailAction, GuardrailMode};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::Ordering;

use crate::credentials::store_api_key_secret;
use crate::{
    AccountKind, AccountRow, AccountState, NewBundle, Secret, StateCause, Store, StoreError,
    SubscriptionRow, LOCK_POISONED,
};

text_enum! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum Strategy {
        #[default]
        Pace => "pace",
        Priority => "priority",
        RoundRobin => "round_robin",
    }
}

text_enum! {
    /// Which pins a shedding round may move once the cold ones are moved.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum WarmSessions {
        Always => "always",
        Never => "never",
        #[default]
        WhenNeeded => "when_needed",
    }
}

text_enum! {
    /// When a session on an API-key member returns to a subscription member.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum ReturnToPlan {
        #[default]
        Always => "always",
        Never => "never",
        WhenCold => "when_cold",
    }
}

/// The load-shedding knobs of a pool. `Default` is the balanced preset with the switch on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shedding {
    /// Off, no shedding round runs. The move off a spent account keeps `migrate_when_spent` as its own toggle.
    pub load_shedding: bool,
    pub shed_cap: f64,
    pub burn_average_seconds: u64,
    pub shed_dwell_seconds: u64,
    pub migrate_when_spent: bool,
    pub warm_sessions: WarmSessions,
}

impl Default for Shedding {
    fn default() -> Self {
        Shedding {
            load_shedding: true,
            shed_cap: 0.5,
            burn_average_seconds: 1800,
            shed_dwell_seconds: 900,
            migrate_when_spent: true,
            warm_sessions: WarmSessions::WhenNeeded,
        }
    }
}

/// The slow-down knobs of a pool. `Default` is the switch off at the shipped figures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlowDown {
    pub slow_down: bool,
    pub slow_down_stretch_seconds: u32,
    pub slow_down_hold_seconds: u32,
}

impl Default for SlowDown {
    fn default() -> Self {
        SlowDown {
            slow_down: false,
            slow_down_stretch_seconds: 9000,
            slow_down_hold_seconds: 120,
        }
    }
}

/// The failover, overage-grace, wind-down and return-to-plan knobs of a pool. `Default` is the shipped preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Failover {
    pub retry_in_place_seconds: u64,
    pub in_flight_cap: u32,
    pub overage_move_grace_minutes: u32,
    pub wind_down_minutes: u32,
    pub return_to_plan: ReturnToPlan,
    pub return_to_plan_dwell_seconds: u32,
}

impl Default for Failover {
    fn default() -> Self {
        Failover {
            retry_in_place_seconds: 60,
            in_flight_cap: 16,
            overage_move_grace_minutes: 30,
            wind_down_minutes: 30,
            return_to_plan: ReturnToPlan::Always,
            return_to_plan_dwell_seconds: 900,
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

text_enum! {
    /// How a key's own list meets its pool's: `Extend` appends the pool's entries after the key's own, `Replace`
    /// leaves them out. A pool has no mode: it inherits nothing.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum RulesMode {
        #[default]
        Extend => "extend",
        Replace => "replace",
    }
}

text_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum OveragePolicy {
        Allow => "allow",
        Never => "never",
        PreferPlan => "prefer_plan",
    }
}

text_enum! {
    /// Whether a session or a subagent cohort holds its pin to one account regardless of cache evidence, or only
    /// while the evidence still confirms a warm prefix there.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum AffinityMode {
        #[default]
        Always => "always",
        WhenCached => "when_cached",
    }
}

text_enum! {
    /// Where a subagent of a pinned session lands without a pin of its own: by its cohort's own placement rules, by
    /// the pool's ordinary selection strategy, or beside the session's main agent.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum SubagentPlacement {
        ByCohort => "by_cohort",
        #[default]
        ByStrategy => "by_strategy",
        WithMain => "with_main",
    }
}

text_enum! {
    /// Where a fresh session with no pin of its own lands: by the pool's ordinary selection strategy, or on whichever
    /// account holds the warmest matching prefix.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum SessionPlacement {
        #[default]
        ByStrategy => "by_strategy",
        ByWarmPrefix => "by_warm_prefix",
    }
}

text_enum! {
    /// The period a member's spend budget resets over.
    #[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
    pub enum BudgetPeriod {
        Day => "day",
        #[default]
        Month => "month",
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
    /// One fraction for every plan window: the shedding trigger's ceiling and the runway ceiling of every move and
    /// placement.
    pub window_ceiling: f64,
    pub model_rules: Vec<ModelRule>,
    pub keepalive_json: String,
    pub body_capture_hours: u32,
    /// Order the selection set by account kind before priority; the operator's answer alone.
    pub prefer_subscription: bool,
    pub shedding: Shedding,
    pub failover: Failover,
    pub slow_down: SlowDown,
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
            // The routing crate's `DEFAULT_WINDOW_CEILING`, which the store cannot name.
            window_ceiling: 0.95,
            model_rules: Vec::new(),
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: true,
            shedding: Shedding::default(),
            failover: Failover::default(),
            slow_down: SlowDown::default(),
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

/// A named list of conditions a request is checked against before any account is selected, defined once and
/// attached to the pools and keys it guards.
#[derive(Clone, Debug, PartialEq)]
pub struct GuardrailRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub mode: GuardrailMode,
    pub action: GuardrailAction,
    /// Every condition must hold for the guardrail to match.
    pub conditions: Vec<Condition>,
    /// The preset the row was created from with its version, so the console can tell an older copy from the
    /// version the binary ships.
    pub preset_id: Option<String>,
    pub preset_version: Option<u32>,
    /// How many times this id has held a guardrail. A deletion leaves the count behind in a tombstone and a
    /// guardrail created again under the id starts one above it, so an edit read from the deleted row cannot land
    /// on its replacement.
    pub incarnation: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What a guardrail is attached to. One table holds both kinds under a kind word, since a pool's list and a key's
/// list are one shape with one writer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AttachmentSubject {
    Key(String),
    Pool(String),
}

impl AttachmentSubject {
    /// The word the attachment table stores for the subject's kind.
    pub fn kind(&self) -> &'static str {
        match self {
            AttachmentSubject::Key(_) => "key",
            AttachmentSubject::Pool(_) => "pool",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            AttachmentSubject::Key(id) | AttachmentSubject::Pool(id) => id,
        }
    }
}

/// One guardrail on one subject, at its place in that subject's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardrailAttachment {
    pub guardrail_id: String,
    pub subject: AttachmentSubject,
    pub position: u32,
}

/// The most guardrails one pool's list holds and one key runs (its pool's and its own, one on both counted once), so
/// one request's walk stays bounded and a key created in any pool inherits at most this many.
pub const GUARDRAIL_BOUND: usize = 64;

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
/// and the end the operator saved with it (set only while the flag is on), and the class the stored profile names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionTerm {
    pub account_id: String,
    pub since: Option<i64>,
    pub trial_ends_at: Option<i64>,
    pub canceled: bool,
    pub ends_at: Option<i64>,
    /// The profile's rate-limit tier, which prices the router's own use of the account's windows where `plan` is one
    /// the cost table prices.
    pub tier: Option<String>,
    /// The class of a usage-based Enterprise seat: the tier `default_claude_zero`, or the stored document's
    /// `organization.seat_tier` `enterprise_usage_based`. Derived at every load from the stored tier and document,
    /// never stored, false for an `api_key` account and for a seat that keeps plan windows.
    pub usage_based: bool,
    /// The plan the stored profile resolves to, one word: `free`, `pro`, `max_5x`, `max_20x`, `max`, `team`,
    /// `enterprise` or `usage_based`, `None` while no stored document and no tier the table names stand. Derived at
    /// every load by `plan_of` beside `usage_based`, never stored. The console labels `max_20x` Ultra, `max_5x` Max and
    /// `max` Max (tier unknown).
    pub plan: Option<String>,
    /// When a profile read found the account's organization holding no plan, epoch milliseconds, `None` while a plan
    /// stands. The engine reads it as an end that has passed.
    pub plan_ended_at: Option<i64>,
}

/// The one derivation of the class, shared by the registry load and `Store::subscription_term`.
pub fn usage_based_of(tier: Option<&str>, profile_json: Option<&str>) -> bool {
    tier == Some("default_claude_zero")
        || profile_json
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .is_some_and(|document| {
                document["organization"]["seat_tier"] == "enterprise_usage_based"
            })
}

/// The one resolution of the plan, shared by the registry load and `Store::subscription_term`: the usage-based class
/// above both, then the document's `organization.organization_type`, then the rate-limit tier alone.
pub fn plan_of(
    organization_type: Option<&str>,
    tier: Option<&str>,
    usage_based: bool,
) -> Option<&'static str> {
    if usage_based {
        return Some("usage_based");
    }
    let by_tier = match tier {
        Some("default_claude_ai") => Some("pro"),
        Some("default_claude_max_5x") => Some("max_5x"),
        Some("default_claude_max_20x") => Some("max_20x"),
        Some("default_claude_zero") => Some("usage_based"),
        _ => None,
    };
    match organization_type {
        Some("claude_free") => Some("free"),
        Some("claude_pro") => Some("pro"),
        Some("claude_team") => Some("team"),
        Some("claude_enterprise") => Some("enterprise"),
        Some("claude_max") => Some(match tier {
            Some("default_claude_max_5x") => "max_5x",
            Some("default_claude_max_20x") => "max_20x",
            _ => "max",
        }),
        _ => by_tier,
    }
}

/// The stored document's `organization.organization_type`, `None` without a document or the member.
pub(crate) fn organization_type_of(profile_json: Option<&str>) -> Option<String> {
    profile_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|document| {
            document["organization"]["organization_type"]
                .as_str()
                .map(String::from)
        })
}

#[derive(Clone, Debug, Default)]
pub struct RegistryLoad {
    pub accounts: Vec<crate::AccountRow>,
    /// The generation of each account's credential, for accounts that hold one. The snapshot carries it so a
    /// verdict about a replaced credential can be told from a current one without reading the store.
    pub credential_generations: Vec<(String, u64)>,
    /// Ordered by kind, subject and position, so each subject's list reads in the order it runs.
    pub guardrail_attachments: Vec<GuardrailAttachment>,
    pub guardrails: Vec<GuardrailRow>,
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

fn row_exists(conn: &rusqlite::Connection, table: &str, id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE id = ?1"),
        params![id],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _)
        if failure.code == rusqlite::ErrorCode::ConstraintViolation)
}

/// A create's refused insert as its caller reads it: `IdTaken` when a constraint refused the row and a row stands
/// under its id, since the route's own check reads outside the transaction and another create can land between.
fn create_failure(
    tx: &Transaction<'_>,
    table: &str,
    id: &str,
    error: rusqlite::Error,
) -> StoreError {
    if !is_constraint(&error) {
        return error.into();
    }
    match row_exists(tx, table, id) {
        Ok(true) => StoreError::IdTaken(id.to_string()),
        Ok(false) => error.into(),
        Err(probe) => probe,
    }
}

/// The stamp a fenced row takes on its next write: one past `previous` when the clock has not moved, so a write
/// inside the millisecond of the stamp it fenced on still moves it, and an edit read before it cannot pass.
fn stamp_past(now: i64, previous: i64) -> i64 {
    now.max(previous.saturating_add(1))
}

/// Moves a fenced row's stamp past its current one, so a save opened before this write presents a token the row
/// no longer carries and is refused as stale. `at` is the writing side's own stamp.
fn stamp_subject(tx: &Transaction<'_>, table: &str, id: &str, at: i64) -> Result<(), StoreError> {
    tx.execute(
        &format!("UPDATE {table} SET updated_at = MAX(?2, updated_at + 1) WHERE id = ?1"),
        params![id, at],
    )?;
    Ok(())
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
            guardrail_attachments: all_guardrail_attachments(&tx)?,
            guardrails: all_guardrails(&tx)?,
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
                    "SELECT id, subscription_since, trial_ends_at, canceled, subscription_ends_at, \
                     subscription_tier, kind, profile_json, plan_ended_at FROM accounts ORDER BY id",
                )?
                .query_map([], |r| {
                    let tier: Option<String> = r.get(5)?;
                    let profile_json: Option<String> = r.get(7)?;
                    let usage_based = r.get::<_, AccountKind>(6)? == AccountKind::Oauth
                        && usage_based_of(tier.as_deref(), profile_json.as_deref());
                    let plan = plan_of(
                        organization_type_of(profile_json.as_deref()).as_deref(),
                        tier.as_deref(),
                        usage_based,
                    )
                    .map(String::from);
                    Ok(SubscriptionTerm {
                        account_id: r.get(0)?,
                        since: r.get(1)?,
                        trial_ends_at: r.get(2)?,
                        canceled: r.get(3)?,
                        ends_at: r.get(4)?,
                        tier,
                        usage_based,
                        plan,
                        plan_ended_at: r.get(8)?,
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
        let now = self.now_ms();
        let incarnation = crate::accounts::next_incarnation_in(&tx, "pool_tombstones", &row.id)?;
        tx.execute(
            "INSERT INTO pools (id, label, enabled, strategy, affinity_ttl_seconds, overage_policy, window_ceiling, model_rules_json, keepalive_json, body_capture_hours, prefer_subscription, shed_cap, burn_average_seconds, shed_dwell_seconds, load_shedding, migrate_when_spent, warm_sessions, retry_in_place_seconds, in_flight_cap, overage_move_grace_minutes, wind_down_minutes, cache_ttl_json, cache_diagnostics, affinity_mode, subagent_placement, session_placement, min_shared_prefix_tokens, auto_mode_server, slow_down, slow_down_stretch_seconds, slow_down_hold_seconds, return_to_plan, return_to_plan_dwell_seconds, created_at, updated_at, incarnation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?34, ?35)",
            params![
                row.id,
                row.label,
                row.enabled,
                row.strategy,
                row.affinity_ttl_seconds as i64,
                row.overage_policy,
                row.window_ceiling,
                rules_json(&row.model_rules)?,
                row.keepalive_json,
                row.body_capture_hours as i64,
                row.prefer_subscription,
                row.shedding.shed_cap,
                row.shedding.burn_average_seconds as i64,
                row.shedding.shed_dwell_seconds as i64,
                row.shedding.load_shedding,
                row.shedding.migrate_when_spent,
                row.shedding.warm_sessions,
                row.failover.retry_in_place_seconds as i64,
                row.failover.in_flight_cap as i64,
                row.failover.overage_move_grace_minutes as i64,
                row.failover.wind_down_minutes as i64,
                row.cache_ttl_json,
                row.cache_diagnostics,
                row.affinity.mode,
                row.affinity.subagent_placement,
                row.affinity.session_placement,
                row.affinity.min_shared_prefix_tokens as i64,
                row.auto_mode_server,
                row.slow_down.slow_down,
                row.slow_down.slow_down_stretch_seconds as i64,
                row.slow_down.slow_down_hold_seconds as i64,
                row.failover.return_to_plan,
                row.failover.return_to_plan_dwell_seconds as i64,
                now,
                incarnation as i64
            ],
        )
        .map_err(|error| create_failure(&tx, "pools", &row.id, error))?;
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
    /// stamp written, which the next fenced edit reads, or the stamp read where the save changes nothing, since the
    /// stamp moves only with a write.
    pub fn update_pool_with_members(
        &self,
        row: &PoolRow,
        expected_updated_at: i64,
        members: Option<&[MemberWrite]>,
        actor: &str,
    ) -> Result<i64, StoreError> {
        self.update_pool_with_members_and_guardrails(row, expected_updated_at, members, None, actor)
    }

    /// The pool write with the pool's guardrail list, replaced whole in the given order when `Some`. A write moves the
    /// pool's stamp, and so does the stamp of every guardrail whose membership in the list changed, so a guardrail save
    /// opened before the change is refused rather than restoring a detached pool. A save that changes no field, no
    /// policy, no member and no guardrail writes nothing and answers the stamp read, since the stamp moves only with a
    /// write.
    pub fn update_pool_with_members_and_guardrails(
        &self,
        row: &PoolRow,
        expected_updated_at: i64,
        members: Option<&[MemberWrite]>,
        guardrails: Option<&[String]>,
        actor: &str,
    ) -> Result<i64, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.park_on_pool_write_hold();
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let before = tx
            .query_row(
                &format!("{POOL_SELECT} WHERE id = ?1"),
                params![row.id],
                row_to_pool,
            )
            .optional()?
            .ok_or_else(|| StoreError::PoolNotFound(row.id.clone()))?;
        let held = match guardrails {
            Some(ids) => {
                let keys = pool_key_ids(&tx, &row.id)?
                    .into_iter()
                    .map(|key_id| {
                        let own = subject_guardrails(&tx, "key", &key_id)?;
                        Ok((key_id, own))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?;
                check_guardrail_bound(&row.id, ids, &keys)?;
                subject_guardrails(&tx, "pool", &row.id)?
            }
            None => Vec::new(),
        };
        let pairs = pool_pairs(&before, row);
        let diff = match guardrails {
            Some(ids) => with_guardrails_change(pairs, &held, ids)?,
            None => pairs,
        };
        let pairs_changed = diff != "{}";
        // The policy's members ride the pairs. The compare reads both texts parsed, so a key order or a whitespace
        // alone writes nothing. Two texts that both fail to parse count as no change, and no route writes such a text.
        let policy_changed = parsed(&before.keepalive_json) != parsed(&row.keepalive_json);
        let members_changed = match members {
            Some(members) => stored_members(&tx, &row.id)? != members,
            None => false,
        };
        if !pairs_changed && !policy_changed && !members_changed {
            // The `UPDATE`'s fence with no write: a moved revision is a write the stale comparison reads, so a save
            // that changes nothing answers the stamp it read.
            if before.updated_at != expected_updated_at || before.incarnation != row.incarnation {
                return Err(StoreError::Stale);
            }
            return Ok(before.updated_at);
        }
        let at = stamp_past(self.now_ms(), expected_updated_at);
        let updated = tx.execute(
            "UPDATE pools SET label = ?2, enabled = ?3, strategy = ?4, affinity_ttl_seconds = ?5, overage_policy = ?6, window_ceiling = ?7, model_rules_json = ?8, keepalive_json = ?9, body_capture_hours = ?10, prefer_subscription = ?11, shed_cap = ?12, burn_average_seconds = ?13, shed_dwell_seconds = ?14, load_shedding = ?15, migrate_when_spent = ?16, warm_sessions = ?17, retry_in_place_seconds = ?18, in_flight_cap = ?19, overage_move_grace_minutes = ?20, wind_down_minutes = ?21, cache_ttl_json = ?22, cache_diagnostics = ?23, affinity_mode = ?24, subagent_placement = ?25, session_placement = ?26, min_shared_prefix_tokens = ?27, auto_mode_server = ?28, slow_down = ?29, slow_down_stretch_seconds = ?30, slow_down_hold_seconds = ?31, return_to_plan = ?32, return_to_plan_dwell_seconds = ?33, updated_at = ?34 WHERE id = ?1 AND updated_at = ?35 AND incarnation = ?36",
            params![
                row.id,
                row.label,
                row.enabled,
                row.strategy,
                row.affinity_ttl_seconds as i64,
                row.overage_policy,
                row.window_ceiling,
                rules_json(&row.model_rules)?,
                row.keepalive_json,
                row.body_capture_hours as i64,
                row.prefer_subscription,
                row.shedding.shed_cap,
                row.shedding.burn_average_seconds as i64,
                row.shedding.shed_dwell_seconds as i64,
                row.shedding.load_shedding,
                row.shedding.migrate_when_spent,
                row.shedding.warm_sessions,
                row.failover.retry_in_place_seconds as i64,
                row.failover.in_flight_cap as i64,
                row.failover.overage_move_grace_minutes as i64,
                row.failover.wind_down_minutes as i64,
                row.cache_ttl_json,
                row.cache_diagnostics,
                row.affinity.mode,
                row.affinity.subagent_placement,
                row.affinity.session_placement,
                row.affinity.min_shared_prefix_tokens as i64,
                row.auto_mode_server,
                row.slow_down.slow_down,
                row.slow_down.slow_down_stretch_seconds as i64,
                row.slow_down.slow_down_hold_seconds as i64,
                row.failover.return_to_plan,
                row.failover.return_to_plan_dwell_seconds as i64,
                at,
                expected_updated_at,
                row.incarnation as i64
            ],
        )?;
        // The row stands, read above, so a fence that matched nothing means another write moved it.
        if updated == 0 {
            return Err(StoreError::Stale);
        }
        // A save that changed the member list alone records that list's row and no empty pool edit beside it.
        if pairs_changed || policy_changed {
            record_change(&tx, at, actor, "pool_updated", &row.id, &diff)?;
        }
        if let Some(members) = members {
            set_pool_members_in(&tx, &row.id, members, at, actor)?;
        }
        if let Some(ids) = guardrails {
            set_subject_guardrails_in(&tx, &AttachmentSubject::Pool(row.id.clone()), ids, at)?;
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
            "DELETE FROM guardrail_attachments WHERE subject_kind = ?1 AND subject_id = ?2",
            params!["pool", id],
        )?;
        tx.execute(
            "INSERT INTO pool_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(pool_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
        )?;
        record_change(&tx, self.now_ms(), actor, "pool_deleted", id, "{}")?;
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

    /// Answers the pool's `updated_at`: the stamp written where the list changed, and the stamp read where the list
    /// equals the stored one, since the stamp moves only with a write.
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
        let at = stamp_past(self.now_ms(), current);
        if !set_pool_members_in(&tx, pool_id, members, at, actor)? {
            return Ok(current);
        }
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
        let now = previous.map_or(self.now_ms(), |p| self.now_ms().max(p + 1));
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
        )
        .map_err(|error| create_failure(&tx, "api_keys", &row.id, error))?;
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
        self.update_api_key_with_guardrails(row, expected_updated_at, None, actor)
    }

    /// The key write with the key's own guardrail list, replaced whole in the given order when `Some`. The key's stamp
    /// moves, and so does the stamp of every guardrail whose membership in the list changed, so a guardrail save opened
    /// before the change is refused rather than restoring a detached key.
    pub fn update_api_key_with_guardrails(
        &self,
        row: &ApiKeyRow,
        expected_updated_at: i64,
        guardrails: Option<&[String]>,
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
        let key_row = format!("{KEY_SELECT} WHERE id = ?1");
        let before = tx
            .query_row(&key_row, params![row.id], row_to_key)
            .optional()?
            .ok_or_else(|| StoreError::KeyNotFound(row.id.clone()))?;
        let held = subject_guardrails(&tx, "key", &row.id)?;
        // The pool the row names, the new one on a move, so a key moving into a pool cannot run more than the bound.
        let pool_list = subject_guardrails(&tx, "pool", &row.pool_id)?;
        let own = guardrails.map_or_else(|| held.clone(), <[String]>::to_vec);
        check_guardrail_bound(&row.pool_id, &pool_list, &[(row.id.clone(), own)])?;
        let at = stamp_past(self.now_ms(), expected_updated_at);
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
        // The row stands, read above, so a fence that matched nothing means another write moved it.
        if updated == 0 {
            return Err(StoreError::Stale);
        }
        // Read back rather than taken from `row`: the edit leaves the display prefix, which the pairs name too.
        let after = tx.query_row(&key_row, params![row.id], row_to_key)?;
        let pairs = key_pairs(&before, &after);
        let diff = match guardrails {
            Some(ids) => with_guardrails_change(pairs, &held, ids)?,
            None => pairs,
        };
        record_change(&tx, at, actor, "key_updated", &row.id, &diff)?;
        if let Some(ids) = guardrails {
            set_subject_guardrails_in(&tx, &AttachmentSubject::Key(row.id.clone()), ids, at)?;
        }
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
            params![id, digest, prefix, at, self.now_ms()],
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
            params![id, at, self.now_ms()],
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
            "DELETE FROM guardrail_attachments WHERE subject_kind = ?1 AND subject_id = ?2",
            params!["key", id],
        )?;
        tx.execute(
            "INSERT INTO api_key_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(api_key_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
        )?;
        tx.execute("DELETE FROM pins WHERE key_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM session_first_seen WHERE key_id = ?1",
            params![id],
        )?;
        // Nothing reads these rows again, since a key created under the id next takes a later generation;
        // dropping them here spares the retention purge a table of dead entries.
        tx.execute("DELETE FROM session_memory WHERE key_id = ?1", params![id])?;
        let now = self.now_ms();
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
        )
        .map_err(|error| create_failure(&tx, "accounts", &row.id, error))?;
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

    /// A row born paired: the account with its identity, state and profile, the credential bundle at its first
    /// generation, the pairing's resolved attempt and the change record, in one transaction, so a confirm that
    /// cannot write the row leaves no credential and one that cannot write the credential leaves no row. A
    /// constraint violation on the row is `IdTaken` when a row holds `row.id`, and `IdentityTaken` when none does.
    pub fn create_paired_account(
        &self,
        row: &AccountRow,
        bundle: &NewBundle,
        profile_json: &str,
        subscription: &SubscriptionRow,
        at: i64,
        actor: &str,
    ) -> Result<u64, StoreError> {
        self.fail_injected_commit()?;
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let identity_json = row
            .identity
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let incarnation = crate::accounts::next_incarnation(&tx, &row.id)?;
        let inserted = tx.execute(
            "INSERT INTO accounts (id, kind, label, enabled, identity_json, incarnation, lifecycle_generation, revision, \
             state, state_cause, profile_json, subscription_plan, subscription_tier, subscription_since, trial_ends_at, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
            params![
                row.id,
                row.kind,
                row.label,
                row.enabled,
                identity_json,
                incarnation as i64,
                row.lifecycle_generation as i64,
                row.state,
                row.state_cause,
                profile_json,
                subscription.plan,
                subscription.tier,
                subscription.since,
                subscription.trial_ends_at,
                at,
            ],
        );
        if let Err(error) = inserted {
            return Err(match create_failure(&tx, "accounts", &row.id, error) {
                // The primary key or the identity index, and with no row under the id it is the identity's.
                StoreError::Sqlite(error) if is_constraint(&error) => StoreError::IdentityTaken,
                other => other,
            });
        }
        let generation = self.commit_credentials_in(
            &tx,
            &row.id,
            incarnation,
            row.lifecycle_generation,
            None,
            None,
            bundle,
            at,
        )?;
        tx.execute(
            "INSERT INTO refresh_attempts (account_id, incarnation, lifecycle_generation, credential_generation, \
             started_at, resolved_at, outcome, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'committed', 'pairing')",
            params![
                row.id,
                incarnation as i64,
                row.lifecycle_generation as i64,
                generation as i64,
                at
            ],
        )?;
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
        Ok(generation)
    }

    /// An edit lands whole, in one revision: every staged field or none. Discovery ran against the staged secret,
    /// and the row moving underneath (another edit, a disconnect, a deletion, a replacement under the same id) means
    /// the edit is about a row that no longer exists as it was read.
    pub fn commit_account_update(&self, u: &AccountUpdate<'_>) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let absent = || StoreError::AccountNotFound(u.id.to_string());
        let before = crate::accounts::account_in(&tx, u.id)?.ok_or_else(absent)?;
        let term = crate::accounts::subscription_term_in(&tx, u.id)?.ok_or_else(absent)?;
        let canceled = u.cancellation.map(|(canceled, _)| canceled);
        let ends_at = u.cancellation.and_then(|(_, ends_at)| ends_at);
        let (state, cause) = u.state.unzip();
        // The end goes with the flag, as `set_canceled` writes them: dropped the moment the flag is off.
        let updated = tx.execute(
            "UPDATE accounts SET label = ?2, state = COALESCE(?3, state), \
             state_cause = COALESCE(?11, state_cause), enabled = COALESCE(?7, enabled), \
             canceled = COALESCE(?8, canceled), \
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
                canceled,
                ends_at,
                u.incarnation as i64,
                cause,
            ],
        )?;
        // The row stands, read above, so a fence that matched nothing means another write moved it.
        if updated == 0 {
            return Err(StoreError::Stale);
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
            &account_pairs(&before, u, &term),
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

impl Store {
    pub fn guardrails(&self) -> Result<Vec<GuardrailRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        all_guardrails(&conn)
    }

    pub fn guardrail(&self, id: &str) -> Result<Option<GuardrailRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        conn.query_row(
            &format!("{GUARDRAIL_SELECT} WHERE id = ?1"),
            params![id],
            row_to_guardrail,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn guardrail_attachments(&self) -> Result<Vec<GuardrailAttachment>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        all_guardrail_attachments(&conn)
    }

    /// The row with its attachments under one `BEGIN IMMEDIATE`, answering the committed row with its incarnation. Each
    /// pool and key it attaches to takes a stamp past its own, so a save of theirs opened before the create presents a
    /// token the row no longer carries.
    pub fn insert_guardrail(
        &self,
        row: &GuardrailRow,
        pools: &[String],
        keys: &[String],
        actor: &str,
    ) -> Result<GuardrailRow, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        check_guardrail_side_bound(&tx, &row.id, Some(pools), Some(keys))?;
        let now = self.now_ms();
        let incarnation =
            crate::accounts::next_incarnation_in(&tx, "guardrail_tombstones", &row.id)?;
        tx.execute(
            "INSERT INTO guardrails (id, name, description, enabled, mode, action, conditions_json, preset_id, preset_version, incarnation, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                row.id,
                row.name,
                row.description,
                row.enabled,
                row.mode.as_str(),
                row.action.as_str(),
                serde_json::to_string(&row.conditions)?,
                row.preset_id,
                row.preset_version,
                incarnation as i64,
                now
            ],
        )?;
        set_guardrail_subjects_in(&tx, &row.id, "pool", pools, now)?;
        set_guardrail_subjects_in(&tx, &row.id, "key", keys, now)?;
        // Read back rather than taken from the caller, so the lists sit in the order the update logs them.
        let diff = guardrail_diff(
            row,
            &guardrail_subjects(&tx, &row.id, "pool")?,
            &guardrail_subjects(&tx, &row.id, "key")?,
        );
        record_change(&tx, now, actor, "guardrail_created", &row.id, &diff)?;
        let written = tx.query_row(
            &format!("{GUARDRAIL_SELECT} WHERE id = ?1"),
            params![row.id],
            row_to_guardrail,
        )?;
        tx.commit()?;
        Ok(written)
    }

    /// Fenced on `expected_updated_at` and `row.incarnation`. `None` leaves that kind's attachments as they are.
    /// Answers the stamp written, which the next fenced edit reads. Each pool and key the write attaches or detaches
    /// takes a stamp past its own, so a save of theirs opened before it presents a token the row no longer carries.
    pub fn update_guardrail(
        &self,
        row: &GuardrailRow,
        expected_updated_at: i64,
        pools: Option<&[String]>,
        keys: Option<&[String]>,
        actor: &str,
    ) -> Result<i64, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        check_guardrail_side_bound(&tx, &row.id, pools, keys)?;
        let before = tx
            .query_row(
                &format!("{GUARDRAIL_SELECT} WHERE id = ?1"),
                params![row.id],
                row_to_guardrail,
            )
            .optional()?
            .ok_or_else(|| StoreError::GuardrailNotFound(row.id.clone()))?;
        let before = guardrail_fields(
            &before,
            &guardrail_subjects(&tx, &row.id, "pool")?,
            &guardrail_subjects(&tx, &row.id, "key")?,
        );
        let at = stamp_past(self.now_ms(), expected_updated_at);
        let updated = tx.execute(
            "UPDATE guardrails SET name = ?2, description = ?3, enabled = ?4, mode = ?5, action = ?6, conditions_json = ?7, preset_id = ?8, preset_version = ?9, updated_at = ?10 WHERE id = ?1 AND updated_at = ?11 AND incarnation = ?12",
            params![
                row.id,
                row.name,
                row.description,
                row.enabled,
                row.mode.as_str(),
                row.action.as_str(),
                serde_json::to_string(&row.conditions)?,
                row.preset_id,
                row.preset_version,
                at,
                expected_updated_at,
                row.incarnation as i64
            ],
        )?;
        if updated == 0 {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM guardrails WHERE id = ?1",
                params![row.id],
                |r| r.get(0),
            )?;
            return Err(if exists > 0 {
                StoreError::Stale
            } else {
                StoreError::GuardrailNotFound(row.id.clone())
            });
        }
        if let Some(pools) = pools {
            set_guardrail_subjects_in(&tx, &row.id, "pool", pools, at)?;
        }
        if let Some(keys) = keys {
            set_guardrail_subjects_in(&tx, &row.id, "key", keys, at)?;
        }
        let after = guardrail_fields(
            row,
            &guardrail_subjects(&tx, &row.id, "pool")?,
            &guardrail_subjects(&tx, &row.id, "key")?,
        );
        let diff = serde_json::Value::Object(changed(&before, &after)).to_string();
        record_change(&tx, at, actor, "guardrail_updated", &row.id, &diff)?;
        tx.commit()?;
        Ok(at)
    }

    /// The attachments go with the row through the foreign key's cascade, and the tombstone keeps the incarnation. Each
    /// pool and key it was attached to, read before the cascade, takes a stamp past its own, so a save of theirs opened
    /// before the delete presents a token the row no longer carries.
    pub fn delete_guardrail(&self, id: &str, actor: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let incarnation: Option<i64> = tx
            .query_row(
                "SELECT incarnation FROM guardrails WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let incarnation =
            incarnation.ok_or_else(|| StoreError::GuardrailNotFound(id.to_string()))?;
        let now = self.now_ms();
        let subjects = [
            ("pools", guardrail_subjects(&tx, id, "pool")?),
            ("api_keys", guardrail_subjects(&tx, id, "key")?),
        ];
        tx.execute("DELETE FROM guardrails WHERE id = ?1", params![id])?;
        tx.execute(
            "INSERT INTO guardrail_tombstones (id, incarnation) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET incarnation = max(guardrail_tombstones.incarnation, excluded.incarnation)",
            params![id, incarnation],
        )?;
        for (table, subject_ids) in &subjects {
            for subject_id in subject_ids {
                stamp_subject(&tx, table, subject_id, now)?;
            }
        }
        record_change(&tx, now, actor, "guardrail_deleted", id, "{}")?;
        tx.commit()?;
        Ok(())
    }
}

/// The subject's list replaced whole, positions 0 to n-1, every id checked against `guardrails`. The pool and key
/// dialogs show the subject's whole order, so their list is the order. Each guardrail the write attaches or detaches
/// takes a stamp past its own, so a guardrail save opened before it presents a token the row no longer carries, and a
/// reorder alone changes no guardrail's membership and stamps none.
fn set_subject_guardrails_in(
    tx: &Transaction<'_>,
    subject: &AttachmentSubject,
    ids: &[String],
    at: i64,
) -> Result<(), StoreError> {
    for id in ids {
        if tx.query_row(
            "SELECT COUNT(*) FROM guardrails WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(StoreError::GuardrailNotFound(id.clone()));
        }
    }
    let held = subject_guardrails(tx, subject.kind(), subject.id())?;
    tx.execute(
        "DELETE FROM guardrail_attachments WHERE subject_kind = ?1 AND subject_id = ?2",
        params![subject.kind(), subject.id()],
    )?;
    for (position, id) in ids.iter().enumerate() {
        tx.execute(
            "INSERT INTO guardrail_attachments (guardrail_id, subject_kind, subject_id, position) VALUES (?1, ?2, ?3, ?4)",
            params![id, subject.kind(), subject.id(), position as i64],
        )?;
    }
    let detached = held.iter().filter(|id| !ids.contains(id));
    let attached = ids.iter().filter(|id| !held.contains(id));
    for id in detached.chain(attached) {
        stamp_subject(tx, "guardrails", id, at)?;
    }
    Ok(())
}

/// The guardrail's subjects of one kind: a new id appended at the end of that subject's order, a held id keeping its
/// position, an id left out detached, every id checked against `pools` or `api_keys`. The guardrail dialog cannot see
/// each subject's order, so it must not reorder it. Each subject attached or detached takes a stamp past its own, so a
/// pool or key save opened before it presents a token the row no longer carries.
fn set_guardrail_subjects_in(
    tx: &Transaction<'_>,
    guardrail_id: &str,
    kind: &str,
    ids: &[String],
    at: i64,
) -> Result<(), StoreError> {
    let (table, missing): (&str, fn(String) -> StoreError) = if kind == "pool" {
        ("pools", StoreError::PoolNotFound)
    } else {
        ("api_keys", StoreError::KeyNotFound)
    };
    for id in ids {
        if tx.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE id = ?1"),
            params![id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            return Err(missing(id.clone()));
        }
    }
    let held = guardrail_subjects(tx, guardrail_id, kind)?;
    for subject_id in held.iter().filter(|held| !ids.contains(held)) {
        tx.execute(
            "DELETE FROM guardrail_attachments WHERE guardrail_id = ?1 AND subject_kind = ?2 AND subject_id = ?3",
            params![guardrail_id, kind, subject_id],
        )?;
        stamp_subject(tx, table, subject_id, at)?;
    }
    for subject_id in ids.iter().filter(|id| !held.contains(id)) {
        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM guardrail_attachments WHERE subject_kind = ?1 AND subject_id = ?2",
            params![kind, subject_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO guardrail_attachments (guardrail_id, subject_kind, subject_id, position) VALUES (?1, ?2, ?3, ?4)",
            params![guardrail_id, kind, subject_id, position],
        )?;
        stamp_subject(tx, table, subject_id, at)?;
    }
    Ok(())
}

/// The ids a guardrail is attached to under one kind, by id, so the change log names them in one order whatever order
/// they were attached in.
fn guardrail_subjects(
    tx: &Transaction<'_>,
    guardrail_id: &str,
    kind: &str,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT subject_id FROM guardrail_attachments WHERE guardrail_id = ?1 AND subject_kind = ?2 ORDER BY subject_id",
    )?;
    let rows = stmt.query_map(params![guardrail_id, kind], |r| r.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

/// One subject's guardrails in the order it runs them.
fn subject_guardrails(
    tx: &Transaction<'_>,
    kind: &str,
    subject_id: &str,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT guardrail_id FROM guardrail_attachments WHERE subject_kind = ?1 AND subject_id = ?2 ORDER BY position",
    )?;
    let rows = stmt.query_map(params![kind, subject_id], |r| r.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

/// By id, so a refusal names the same first key over the bound on every run.
fn pool_key_ids(tx: &Transaction<'_>, pool_id: &str) -> Result<Vec<String>, StoreError> {
    let mut stmt = tx.prepare("SELECT id FROM api_keys WHERE pool_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![pool_id], |r| r.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

/// The bound of a write from the guardrail's side, over the lists as the write would leave them: each pool it names
/// takes the guardrail at the end of its list and each key it names in its own, and every key of a named pool and
/// every named key is counted against its pool's list. `None` leaves that kind's lists as they stand.
fn check_guardrail_side_bound(
    tx: &Transaction<'_>,
    guardrail_id: &str,
    pools: Option<&[String]>,
    keys: Option<&[String]>,
) -> Result<(), StoreError> {
    let named = |list: Option<&[String]>, id: &str| {
        list.is_some_and(|list| list.iter().any(|named| named == id))
    };
    let mut affected: std::collections::BTreeSet<String> =
        pools.unwrap_or_default().iter().cloned().collect();
    for key_id in keys.unwrap_or_default() {
        // A key that does not exist adds no pool here, and the subject write refuses it by name.
        let pool_id: Option<String> = tx
            .query_row(
                "SELECT pool_id FROM api_keys WHERE id = ?1",
                params![key_id],
                |r| r.get(0),
            )
            .optional()?;
        affected.extend(pool_id);
    }
    for pool_id in &affected {
        let pool_named = named(pools, pool_id);
        let mut pool_list = subject_guardrails(tx, "pool", pool_id)?;
        if pools.is_some() {
            place(&mut pool_list, guardrail_id, pool_named);
        }
        let mut key_lists = Vec::new();
        for key_id in pool_key_ids(tx, pool_id)? {
            let key_named = named(keys, &key_id);
            if !pool_named && !key_named {
                continue;
            }
            let mut own = subject_guardrails(tx, "key", &key_id)?;
            if keys.is_some() {
                place(&mut own, guardrail_id, key_named);
            }
            key_lists.push((key_id, own));
        }
        check_guardrail_bound(pool_id, &pool_list, &key_lists)?;
    }
    Ok(())
}

/// A subject's list after a write from the guardrail's side: the guardrail appended when newly attached, dropped when
/// detached, a held place kept.
fn place(list: &mut Vec<String>, guardrail_id: &str, attached: bool) {
    let held = list.iter().any(|id| id == guardrail_id);
    if attached && !held {
        list.push(guardrail_id.to_string());
    } else if !attached && held {
        list.retain(|id| id != guardrail_id);
    }
}

/// The bound over the lists as the write would leave them, read inside the transaction before any row changes:
/// `pool_list` is the pool's own list, `keys` each key of the pool with its own list (a key moving in among them).
/// Answers `PoolGuardrailBound` for the pool, then `KeyGuardrailBound` for the first key whose union is over the bound.
/// The count runs under the same lock and transaction as the rows it guards, so two writes cannot each read 63 and
/// commit 65 together.
fn check_guardrail_bound(
    pool_id: &str,
    pool_list: &[String],
    keys: &[(String, Vec<String>)],
) -> Result<(), StoreError> {
    let pool: std::collections::HashSet<&str> = pool_list.iter().map(String::as_str).collect();
    if pool.len() > GUARDRAIL_BOUND {
        return Err(StoreError::PoolGuardrailBound(pool_id.to_string()));
    }
    for (key_id, own) in keys {
        let union: std::collections::HashSet<&str> = own
            .iter()
            .map(String::as_str)
            .chain(pool.iter().copied())
            .collect();
        if union.len() > GUARDRAIL_BOUND {
            return Err(StoreError::KeyGuardrailBound(key_id.clone()));
        }
    }
    Ok(())
}

/// A pool or key diff with the subject's list as a `from` and `to` pair, present only when the list changed, so the
/// member reads like a changed field.
fn with_guardrails_change(
    diff: String,
    from: &[String],
    to: &[String],
) -> Result<String, StoreError> {
    if from == to {
        return Ok(diff);
    }
    let mut value: serde_json::Value = serde_json::from_str(&diff)?;
    if let Some(members) = value.as_object_mut() {
        members.insert(
            "guardrails".into(),
            serde_json::json!({ "from": from, "to": to }),
        );
    }
    Ok(value.to_string())
}

/// The stored member rows of a pool in the order the write inserted them, the shape `MemberWrite` compares against.
fn stored_members(tx: &Transaction<'_>, pool_id: &str) -> Result<Vec<MemberWrite>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT account_id, priority, budget_daily_usd, budget_monthly_usd FROM pool_members WHERE pool_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![pool_id], |r| {
        Ok(MemberWrite {
            account_id: r.get(0)?,
            priority: r.get::<_, i64>(1)? as u32,
            budget_daily_usd: r.get(2)?,
            budget_monthly_usd: r.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

/// The member half of a pool write on the caller's transaction. Writes the list where it differs from the stored
/// rows in any member, position or column, and answers whether it wrote. An equal list moves no stamp and records
/// nothing. A write checks every named account, replaces the list whole and moves the pool's `updated_at` to `at`,
/// so an edit fenced on the stamp it read before this write is refused rather than landing over it. The compare
/// reads the stored rows in `rowid` order, since they carry no position column and `rowid` keeps the request's
/// order.
fn set_pool_members_in(
    tx: &Transaction<'_>,
    pool_id: &str,
    members: &[MemberWrite],
    at: i64,
    actor: &str,
) -> Result<bool, StoreError> {
    if stored_members(tx, pool_id)? == members {
        // A moved revision is a write the stale comparison reads, so a list that changes nothing keeps the stamp.
        return Ok(false);
    }
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
    Ok(true)
}

const POOL_SELECT: &str = "SELECT id, label, enabled, strategy, affinity_ttl_seconds, overage_policy, window_ceiling, model_rules_json, keepalive_json, body_capture_hours, created_at, updated_at, prefer_subscription, shed_cap, burn_average_seconds, shed_dwell_seconds, load_shedding, migrate_when_spent, warm_sessions, retry_in_place_seconds, in_flight_cap, overage_move_grace_minutes, wind_down_minutes, cache_ttl_json, cache_diagnostics, affinity_mode, subagent_placement, session_placement, min_shared_prefix_tokens, auto_mode_server, incarnation, slow_down, slow_down_stretch_seconds, slow_down_hold_seconds, return_to_plan, return_to_plan_dwell_seconds FROM pools";
const GUARDRAIL_SELECT: &str = "SELECT id, name, description, enabled, mode, action, conditions_json, preset_id, preset_version, incarnation, created_at, updated_at FROM guardrails";
const KEY_SELECT: &str = "SELECT id, label, notes, pool_id, enabled, revoked_at, expires_at, keepalive_opt_out, body_capture_hours, secret_digest, prefix, created_at, rotated_at, last_used_at, retired_secret_last_used_at, model_rules_json, model_rules_mode, long_context_twins, auto_mode_server, updated_at, incarnation FROM api_keys";

fn all_pools(conn: &rusqlite::Connection) -> Result<Vec<PoolRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("{POOL_SELECT} ORDER BY label, id"))?;
    let rows = stmt.query_map([], row_to_pool)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

pub(crate) fn all_guardrails(conn: &rusqlite::Connection) -> Result<Vec<GuardrailRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("{GUARDRAIL_SELECT} ORDER BY id"))?;
    let rows = stmt.query_map([], row_to_guardrail)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

fn all_guardrail_attachments(
    conn: &rusqlite::Connection,
) -> Result<Vec<GuardrailAttachment>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT guardrail_id, subject_kind, subject_id, position FROM guardrail_attachments ORDER BY subject_kind, subject_id, position",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(1)?;
        let subject_id: String = r.get(2)?;
        let subject = match kind.as_str() {
            "key" => AttachmentSubject::Key(subject_id),
            "pool" => AttachmentSubject::Pool(subject_id),
            _ => return Err(unknown_word(1, &kind)),
        };
        Ok(GuardrailAttachment {
            guardrail_id: r.get(0)?,
            subject,
            position: r.get::<_, i64>(3)? as u32,
        })
    })?;
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
    let rules: String = r.get(7)?;
    Ok(PoolRow {
        id: r.get(0)?,
        label: r.get(1)?,
        enabled: r.get(2)?,
        strategy: r.get(3)?,
        affinity_ttl_seconds: r.get::<_, i64>(4)? as u64,
        overage_policy: r.get(5)?,
        window_ceiling: r.get(6)?,
        model_rules: json_rules(rules).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?,
        keepalive_json: r.get(8)?,
        body_capture_hours: r.get::<_, i64>(9)? as u32,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        prefer_subscription: r.get(12)?,
        shedding: Shedding {
            shed_cap: r.get(13)?,
            burn_average_seconds: r.get::<_, i64>(14)? as u64,
            shed_dwell_seconds: r.get::<_, i64>(15)? as u64,
            load_shedding: r.get(16)?,
            migrate_when_spent: r.get(17)?,
            warm_sessions: r.get(18)?,
        },
        failover: Failover {
            retry_in_place_seconds: r.get::<_, i64>(19)? as u64,
            in_flight_cap: r.get::<_, i64>(20)? as u32,
            overage_move_grace_minutes: r.get::<_, i64>(21)? as u32,
            wind_down_minutes: r.get::<_, i64>(22)? as u32,
            return_to_plan: r.get(34)?,
            return_to_plan_dwell_seconds: r.get::<_, i64>(35)? as u32,
        },
        slow_down: SlowDown {
            slow_down: r.get(31)?,
            slow_down_stretch_seconds: r.get::<_, i64>(32)? as u32,
            slow_down_hold_seconds: r.get::<_, i64>(33)? as u32,
        },
        cache_ttl_json: r.get(23)?,
        cache_diagnostics: r.get::<_, bool>(24)?,
        affinity: Affinity {
            mode: r.get(25)?,
            subagent_placement: r.get(26)?,
            session_placement: r.get(27)?,
            min_shared_prefix_tokens: r.get::<_, i64>(28)? as u64,
        },
        auto_mode_server: r.get(29)?,
        incarnation: r.get::<_, i64>(30)? as u64,
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

fn row_to_guardrail(r: &rusqlite::Row<'_>) -> rusqlite::Result<GuardrailRow> {
    let mode: String = r.get(4)?;
    let action: String = r.get(5)?;
    let conditions: String = r.get(6)?;
    Ok(GuardrailRow {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        enabled: r.get(3)?,
        mode: GuardrailMode::parse(&mode).ok_or_else(|| unknown_word(4, &mode))?,
        action: GuardrailAction::parse(&action).ok_or_else(|| unknown_word(5, &action))?,
        conditions: serde_json::from_str::<Vec<Condition>>(&conditions).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
        preset_id: r.get(7)?,
        preset_version: r.get(8)?,
        incarnation: r.get::<_, i64>(9)? as u64,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

/// A stored word this build does not spell fails the read of its row, as a malformed rule list does, rather than
/// load a row whose meaning the build cannot know.
fn unknown_word(column: usize, word: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        format!("unknown word {word:?}").into(),
    )
}

fn pool_diff(row: &PoolRow) -> String {
    pool_fields(row).to_string()
}

/// The pairs of the fields whose value differs between `before` and `after`, over the fields `pool_diff` names,
/// `{}` when none does.
fn pool_pairs(before: &PoolRow, after: &PoolRow) -> String {
    serde_json::Value::Object(changed(&pool_fields(before), &pool_fields(after))).to_string()
}

/// The pool's fields the change rows name, with each member of the stored keepalive policy under
/// `keepalive.<member>`, so a policy edit records the member it moved.
fn pool_fields(row: &PoolRow) -> serde_json::Value {
    let mut fields = serde_json::json!({
        "label": row.label,
        "enabled": row.enabled,
        "strategy": row.strategy.as_str(),
        "affinity_ttl_seconds": row.affinity_ttl_seconds,
        "overage_policy": row.overage_policy.as_str(),
        "window_ceiling": row.window_ceiling,
        "model_rules": row.model_rules,
        "body_capture_hours": row.body_capture_hours,
        "prefer_subscription": row.prefer_subscription,
        "shed_cap": row.shedding.shed_cap,
        "burn_average_seconds": row.shedding.burn_average_seconds,
        "shed_dwell_seconds": row.shedding.shed_dwell_seconds,
        "load_shedding": row.shedding.load_shedding,
        "migrate_when_spent": row.shedding.migrate_when_spent,
        "warm_sessions": row.shedding.warm_sessions.as_str(),
        "slow_down": row.slow_down.slow_down,
        "slow_down_stretch_seconds": row.slow_down.slow_down_stretch_seconds,
        "slow_down_hold_seconds": row.slow_down.slow_down_hold_seconds,
        "retry_in_place_seconds": row.failover.retry_in_place_seconds,
        "in_flight_cap": row.failover.in_flight_cap,
        "overage_move_grace_minutes": row.failover.overage_move_grace_minutes,
        "wind_down_minutes": row.failover.wind_down_minutes,
        "return_to_plan": row.failover.return_to_plan.as_str(),
        "return_to_plan_dwell_seconds": row.failover.return_to_plan_dwell_seconds,
        "cache_ttl": serde_json::from_str::<serde_json::Value>(&row.cache_ttl_json).unwrap_or(serde_json::Value::Null),
        "cache_diagnostics": row.cache_diagnostics,
        "affinity_mode": row.affinity.mode.as_str(),
        "subagent_placement": row.affinity.subagent_placement.as_str(),
        "session_placement": row.affinity.session_placement.as_str(),
        "min_shared_prefix_tokens": row.affinity.min_shared_prefix_tokens,
        "auto_mode_server": row.auto_mode_server,
    });
    if let (serde_json::Value::Object(named), Some(serde_json::Value::Object(policy))) =
        (&mut fields, parsed(&row.keepalive_json))
    {
        for (member, value) in policy {
            named.insert(format!("keepalive.{member}"), value);
        }
    }
    fields
}

fn parsed(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

fn key_diff(row: &ApiKeyRow) -> String {
    key_fields(row).to_string()
}

fn key_pairs(before: &ApiKeyRow, after: &ApiKeyRow) -> String {
    serde_json::Value::Object(changed(&key_fields(before), &key_fields(after))).to_string()
}

/// Never the digest: the change log is read by the ledger and the UI.
fn key_fields(row: &ApiKeyRow) -> serde_json::Value {
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
}

/// The account's pairs over the five editable fields, `secret_replaced` beside them when a secret was staged. The
/// row holds the label, the state and the switch, and the term row the flag and the end.
fn account_pairs(before: &AccountRow, u: &AccountUpdate<'_>, term: &SubscriptionTerm) -> String {
    // As the update's `CASE` writes them: the end goes with the flag, dropped the moment the flag is off.
    let (canceled, ends_at) = match u.cancellation {
        None => (term.canceled, term.ends_at),
        Some((true, ends_at)) => (true, ends_at),
        Some((false, _)) => (false, None),
    };
    let old = serde_json::json!({
        "label": before.label,
        "enabled": before.enabled,
        "state": before.state.as_str(),
        "canceled": term.canceled,
        "subscription_ends_at": term.ends_at,
    });
    let new = serde_json::json!({
        "label": u.label,
        "enabled": u.enabled.unwrap_or(before.enabled),
        "state": u.state.map_or(before.state, |(state, _)| state).as_str(),
        "canceled": canceled,
        "subscription_ends_at": ends_at,
    });
    let mut pairs = changed(&old, &new);
    // A secret's value never reaches the log, which the ledger and the UI read.
    if u.secret.is_some() {
        pairs.insert("secret_replaced".into(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(pairs).to_string()
}

/// Each member whose value differs between two snapshots of one row, as `{"from": old, "to": new}`: `before`'s
/// members, then the members of `after` that `before` lacks, so a save that gives a policy member the row did not
/// hold a value names it. A member one side lacks reads null there, so a null beside a missing member is no change.
fn changed(
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return serde_json::Map::new();
    };
    let null = &serde_json::Value::Null;
    let added = after
        .iter()
        .filter(|(field, _)| !before.contains_key(*field))
        .map(|(field, to)| (field, null, to));
    before
        .iter()
        .map(|(field, from)| (field, from, after.get(field).unwrap_or(null)))
        .chain(added)
        .filter(|(_, from, to)| from != to)
        .map(|(field, from, to)| (field.clone(), serde_json::json!({ "from": from, "to": to })))
        .collect()
}

fn guardrail_diff(row: &GuardrailRow, pools: &[String], keys: &[String]) -> String {
    guardrail_fields(row, pools, keys).to_string()
}

fn guardrail_fields(row: &GuardrailRow, pools: &[String], keys: &[String]) -> serde_json::Value {
    serde_json::json!({
        "name": row.name,
        "description": row.description,
        "enabled": row.enabled,
        "mode": row.mode.as_str(),
        "action": row.action.as_str(),
        "conditions": row.conditions,
        "preset_id": row.preset_id,
        "preset_version": row.preset_version,
        "pools": pools,
        "keys": keys,
    })
}
