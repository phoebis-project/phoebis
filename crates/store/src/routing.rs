//! Routing state that survives a restart: session pins, cooldowns, and the overage evidence per account.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

use crate::accounts::set_state_if_generations_on;
use crate::registry::json_column;
use crate::{AccountState, Clock, StateCause, Store, StoreError, LOCK_POISONED};

/// The identity a pin is held under: one conversation of one agent under one key, per model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PinKey {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
}

impl PinKey {
    pub fn new(key_id: &str, session_id: &str, agent_id: &str, model: &str) -> PinKey {
        PinKey {
            key_id: key_id.to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            model: model.to_string(),
        }
    }

    /// The four fields borrowed, in field order, as `PinRow::key_parts` lends a row's.
    pub fn parts(&self) -> (&str, &str, &str, &str) {
        (&self.key_id, &self.session_id, &self.agent_id, &self.model)
    }
}

/// Tokens per second of each type, a pin's burn averaged over the pool's burn window and decayed from `burn_at`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PinBurn {
    pub cache_read: f64,
    pub input: f64,
    pub output: f64,
    pub write_1h: f64,
    pub write_5m: f64,
}

/// Where a shedding round's move or a failover's landing took a pin from, and when: written whole, and cleared whole
/// by every other move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShedStamp {
    pub from: String,
    pub at: i64,
    /// Why the stamp was written: `round` for a shedding round's move, the failover's reason word (`cooldown`,
    /// `fast_lane`, `mark`) for a failover's landing. Null on a row stamped before the column, which reads as shed.
    pub reason: Option<String>,
}

/// The round's mark: when it was made, the account it was made on, the window whose trigger made it and the test that
/// fired the round. It stands while `on` is the pin's account, and nothing but a move, a release or a heal clears it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinMark {
    pub at: i64,
    pub on: String,
    pub window: String,
    /// The word of the test that fired the mark's round, as `TriggerSource::as_str` spells it. `None` on a mark stored
    /// before the column, which is a projection round's.
    pub source: Option<String>,
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
    /// The pin's usage as typed tokens per second over the pool's burn window, and when it was last folded. A
    /// reader prices it on the account the pin sits on when it reads, so no average carries a unit. Zero and
    /// `None` for a pin never renewed with usage.
    pub burn: PinBurn,
    pub burn_at: Option<i64>,
    /// The same tokens weighed by the peak band at each renewal: what the 5-hour meter charged, which a
    /// subscription account's figure reads. The dollar figure reads `burn`.
    pub burn_banded: PinBurn,
    /// The pin's fast usage as typed tokens per second over the same window, decayed from `burn_at` as `burn` is,
    /// and zero for a pin never renewed fast.
    pub burn_fast: PinBurn,
    /// The usage of the client's own classifier calls, typed tokens per second over the same window, decayed from
    /// `burn_at` as `burn` is. Priced in dollars alone, so a plan account's projection never reads it.
    pub burn_unmetered: PinBurn,
    /// The canonical model of the classifier call that folded into `burn_unmetered` last, which prices the copy.
    /// `None` on a row no classifier call folded into, whose copy prices at the pin's model.
    pub burn_unmetered_model: Option<String>,
    pub last_input_tokens: u64,
    /// The last main, subagent or compaction turn, or a keepalive probe's renewal. `last_used_at` moves on every
    /// request, while this does not.
    pub last_refresh_at: i64,
    /// The cache TTL in force at the last refresh, as the relay resolved it (the policy's rewrite, else the
    /// client's own marker), so warmth is judged here without the relay's policy type. It is 5 minutes until reported.
    pub cache_ttl_ms: i64,
    pub shed: Option<ShedStamp>,
    pub static_hash: Option<String>,
    pub tail_hash: Option<String>,
    /// The lane of the request that created the pin, stamped again by each request that refreshed or colded it,
    /// or `""` on a pin written before pins carried a lane, which adopts the next request's lane.
    pub lane: String,
    /// The entrypoint of the request that placed the pin, stamped again by each client renewal. `None` for none and
    /// on a pin written before pins carried one, which adopts the next request's, and the base regime prices `None`.
    /// The column spells none as the empty text.
    pub entrypoint: Option<String>,
    pub mark: Option<PinMark>,
    /// Only auxiliary requests have used the pin since its placement: written true at such a placement, cleared by
    /// the first refreshing turn that renews the pin, so a restart reads the flag rather than deriving it.
    pub auxiliary_only: bool,
    /// The effective effort of the pin's last turn, the request that placed it or the last one that refreshed or colded
    /// it, the word as the client sent it, `None` for none and on a row written before the column, which spells none as
    /// the empty text. A token count, an auxiliary call and a probe keep it.
    pub effort: Option<String>,
    /// The pin's agent ended and its next request has not arrived: a subagent's turn answered with `end_turn`, or an
    /// agent the companion released. The pool's keep leaves such a pin out of every figure until that request.
    pub ended: bool,
    /// The thinking tokens each account produced for the session on the pin's model, by account id.
    pub thinking_sums: BTreeMap<String, ThinkingSum>,
    /// The pin's session holds account state: its newest request that carried the conversation read non-portable. A
    /// request that carries no conversation follows such a pin, and no round plans its move.
    pub account_bound: bool,
}

/// One account's entry in a pin's thinking sums. An id is reusable, and a holder created under it later produced none
/// of these blocks, so the entry names the incarnation it was summed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThinkingSum {
    pub incarnation: u64,
    pub tokens: u64,
}

impl PinRow {
    pub fn key(&self) -> PinKey {
        PinKey::new(&self.key_id, &self.session_id, &self.agent_id, &self.model)
    }

    /// The key's fields borrowed, in `PinKey`'s field order: a compare or a sort of pins reads these, since `key`
    /// clones four strings for every pin it walks.
    pub fn key_parts(&self) -> (&str, &str, &str, &str) {
        (&self.key_id, &self.session_id, &self.agent_id, &self.model)
    }
}

/// What the router keeps of an agent whose pin the purge took as expired, for a placement of that agent to read in
/// place of a fresh session's figures.
#[derive(Clone, Debug, PartialEq)]
pub struct LapsedAgentRow {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
    pub key_generation: i64,
    /// The account the pin stood on, with the incarnation it stood under.
    pub account_id: String,
    pub incarnation: u64,
    pub last_used_at: i64,
    pub last_input_tokens: u64,
    /// The pin's burn members as its last fold left them.
    pub burn: PinBurn,
    pub burn_banded: PinBurn,
    pub burn_fast: PinBurn,
    pub burn_unmetered: PinBurn,
    pub burn_unmetered_model: Option<String>,
    /// The slow-down speed in force on the account when the pin was taken, 1 where none was.
    pub speed: f64,
    pub thinking_sums: BTreeMap<String, ThinkingSum>,
    /// The pin's session held account state when the pin was taken, as the pin's own flag read.
    pub account_bound: bool,
}

impl LapsedAgentRow {
    pub fn key(&self) -> PinKey {
        PinKey::new(&self.key_id, &self.session_id, &self.agent_id, &self.model)
    }
}

/// What a cooldown row holds off: the whole account, the fast lane's rate limit, the fast lane's usage-credits
/// refusal, or one model by its canonical id. The column, the admin field and the clear route carry it as the text
/// `account`, `fast`, `fast_credits` or `model:<canonical>`.
///
/// The variants and the model ids order as their stored texts do, so a sort by scope keeps the order the text gave.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CooldownScope {
    Account,
    Fast,
    FastCredits,
    Model(String),
}

impl CooldownScope {
    /// The scope a stored text names, `None` for text no row holds. Any `model:` suffix names a model, the empty
    /// one included.
    pub fn parse(text: &str) -> Option<CooldownScope> {
        match text {
            "account" => Some(CooldownScope::Account),
            "fast" => Some(CooldownScope::Fast),
            "fast_credits" => Some(CooldownScope::FastCredits),
            _ => text
                .strip_prefix("model:")
                .map(|model| CooldownScope::Model(model.to_string())),
        }
    }

    /// The model a `model:`-scoped row cools, `None` for an account-wide or a fast-lane row.
    pub fn model(&self) -> Option<&str> {
        match self {
            CooldownScope::Model(model) => Some(model),
            CooldownScope::Account | CooldownScope::Fast | CooldownScope::FastCredits => None,
        }
    }
}

impl fmt::Display for CooldownScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CooldownScope::Account => f.write_str("account"),
            CooldownScope::Fast => f.write_str("fast"),
            CooldownScope::FastCredits => f.write_str("fast_credits"),
            CooldownScope::Model(model) => write!(f, "model:{model}"),
        }
    }
}

impl FromSql for CooldownScope {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let word = value.as_str()?;
        CooldownScope::parse(word)
            .ok_or_else(|| FromSqlError::Other(crate::unknown_word_error(word)))
    }
}

impl ToSql for CooldownScope {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.to_string().into())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CooldownRow {
    pub account_id: String,
    pub scope: CooldownScope,
    pub until: i64,
    /// When the rejection that set this row arrived, so a usage sample can be told apart from the cached view
    /// the account had before it.
    pub set_at: i64,
    pub reason: String,
    pub window: String,
    pub source_request_id: Option<String>,
    pub incarnation: u64,
    /// The spend share that the router held when it dated the row, 1.0 where it held none, on an account row of a
    /// usage-based seat. `None` on each other row.
    pub spend_share: Option<f64>,
}

/// Where an account's overage evidence came from: a response head or a usage sample. Stored as `head` or `sample`,
/// and any text but `sample` reads as `Head`, so no stored word fails the routing load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceSource {
    Head,
    Sample,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::Head => "head",
            EvidenceSource::Sample => "sample",
        }
    }
}

impl FromSql for EvidenceSource {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(match value.as_str()? {
            "sample" => EvidenceSource::Sample,
            _ => EvidenceSource::Head,
        })
    }
}

impl ToSql for EvidenceSource {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverageRow {
    pub account_id: String,
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
    pub source: EvidenceSource,
    pub incarnation: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowRow {
    pub account_id: String,
    pub incarnation: u64,
    pub observed_at: i64,
    /// The resolved plan word the account held when the reading was taken, `None` on a row written before the
    /// column, so a reset priced from the reading reads the plan of its week.
    pub plan: Option<String>,
    pub reset: Option<i64>,
    /// The stored rate-limit tier the account held when the reading was taken, `None` on a row written before the
    /// column, so a reset priced from the reading reads the tier of its week.
    pub tier: Option<String>,
    pub utilization: f64,
    /// `five_hour`, `seven_day` or `seven_day_fable`.
    pub window: String,
}

/// The shared window set of an OAuth account as its newest clean poll settled it.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowSetRow {
    pub account_id: String,
    pub incarnation: u64,
    /// The `observed_at` of the poll that settled the set, restored as the sample's `polled_at`.
    pub observed_at: i64,
    /// The set's ids comma-joined in the usage endpoint's spelling, `five_hour,seven_day` for both.
    pub windows: String,
}

/// The poll's newest extra-usage state of an OAuth account: what the fast eligibility reads.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditsRow {
    pub account_id: String,
    pub incarnation: u64,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    /// A fraction of the monthly limit, as the plan windows read.
    pub utilization: Option<f64>,
    pub observed_at: i64,
}

/// A folded spelling and the dated id the API listed for it. The routing state keys that spelling's rows on
/// the dated id from the moment the row exists, and the row is loaded before any request is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionRow {
    pub spelling: String,
    pub dated_id: String,
    pub resolved_at: i64,
}

/// One landing of a resolution: the spelling row and every pin, model cooldown, pair's fact and lapsed agent's record
/// that it moves from the spelling to the dated id.
#[derive(Clone, Debug, PartialEq)]
pub struct RekeyWrite {
    pub resolution: ResolutionRow,
    pub delete_pins: Vec<PinKey>,
    pub upsert_pins: Vec<PinRow>,
    /// `(account_id, scope)` of each cooldown under the spelling.
    pub delete_cooldowns: Vec<(String, CooldownScope)>,
    pub upsert_cooldowns: Vec<CooldownRow>,
    /// `(model, account_a, account_b)` of each pair's fact under the spelling.
    pub delete_bindings: Vec<(String, String, String)>,
    /// Each fact that stands under the dated id behind the merge, written under the holders that it names.
    pub upsert_bindings: Vec<ThinkingBindingRow>,
    pub delete_lapsed_agents: Vec<PinKey>,
    pub upsert_lapsed_agents: Vec<LapsedAgentRow>,
}

text_enum! {
    /// What a hash in the `affinity` table is.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum AffinityChain {
        Message => "message",
        Static => "static",
    }
}

/// One boundary: which account the conversation it names last ran on, whose conversation it belongs
/// to, and, when the cache confirmed it, how warm it is. The links (the account, the key, the root) are written
/// for every completed response. The warmth is written only with cache evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityRow {
    pub model: String,
    pub hash: String,
    pub chain: AffinityChain,
    /// The account and its incarnation, as the routing state's other account-bound rows carry them: a row
    /// under another incarnation is a replaced account's and is dropped.
    pub account_id: String,
    pub incarnation: u64,
    /// `written_at` is the last report's time (the purge reads it). `ttl_ms` is the lifetime remaining as of
    /// `written_at`: a report with cache evidence sets it to the marker's full lifetime as sent upstream, and an
    /// unconfirmed refresh shortens it to what remained, so the boundary's warm deadline never moves. It is 0 for a
    /// link the cache never confirmed, which never places. `prefix_tokens` is the promised prefix: for a
    /// `static` row the relay's estimate of the blocks the boundary covers (canonical bytes over four), for a
    /// `message` row the cached tokens the upstream reported for the writing request.
    pub written_at: i64,
    pub ttl_ms: i64,
    pub prefix_tokens: u64,
    pub key_id: String,
    pub key_generation: i64,
    /// The conversation the writing request belonged to: its resolved session under an identity, its own first
    /// message boundary for a request with none.
    pub root: String,
    /// The cache lane of the writing request: a filter beside the `(model, hash)` key, never part of it.
    pub lane: String,
    /// The boundary's place in its chain, counted from 1, as the report that wrote the entry listed it. A row
    /// stored before the registry kept it reads 0.
    pub depth: u32,
    /// The upstream id of the newest answer produced at this boundary on the account that holds the row, which the
    /// relay's cache diagnostics member names for a request that continues the conversation here. `None` on a
    /// static row, on a row whose answer was not remembered, and on a row stored before the registry kept it.
    pub message_id: Option<String>,
}

const AFFINITY_COLUMNS: &str =
    "model, hash, chain, account_id, incarnation, written_at, ttl_ms, prefix_tokens, key_id, key_generation, root, lane, depth, message_id";

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
        lane: r.get(11)?,
        depth: r.get(12)?,
        message_id: r.get(13)?,
    })
}

/// One pool's latest round on an account: the stamp the dwell between rounds reads. The marks live on the pins, and
/// the account's incarnation fences the row to the holder the round ran on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShedRoundRow {
    pub pool_id: String,
    pub account_id: String,
    pub incarnation: u64,
    pub round_at: i64,
}

/// The share the router's own requests priced on one model week of an account in one clock hour, fenced by the
/// account's incarnation as the window rows are.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelWeekHourRow {
    pub account_id: String,
    pub incarnation: u64,
    /// The model week, such as `seven_day_fable`.
    pub window: String,
    /// The instant in milliseconds over 3,600,000, floored.
    pub hour: i64,
    pub share: f64,
}

/// What a move of one model's session between two accounts showed of its thinking blocks: `bound` where the account
/// moved to dropped the blocks the other produced. The pair is unordered, so `account_a` holds the smaller id.
/// `incarnation_a` and `incarnation_b` name the holders of the two ids the fact was learned under. The table stores
/// neither, so a loaded row takes its accounts' current holders: the cascade deletes a row with either account, and
/// the write lands only under the holders the row names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingBindingRow {
    pub model: String,
    pub account_a: String,
    pub incarnation_a: u64,
    pub account_b: String,
    pub incarnation_b: u64,
    pub bound: bool,
    pub observed_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingLoad {
    pub affinity: Vec<AffinityRow>,
    pub cooldowns: Vec<CooldownRow>,
    pub credits: Vec<CreditsRow>,
    pub lapsed_agents: Vec<LapsedAgentRow>,
    pub model_week_hours: Vec<ModelWeekHourRow>,
    pub overage: Vec<OverageRow>,
    pub pins: Vec<PinRow>,
    pub resolutions: Vec<ResolutionRow>,
    pub shed_rounds: Vec<ShedRoundRow>,
    pub thinking_bindings: Vec<ThinkingBindingRow>,
    pub window_sets: Vec<WindowSetRow>,
    pub windows: Vec<WindowRow>,
}

impl Store {
    pub fn load_routing(&self) -> Result<RoutingLoad, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        let tx = conn.transaction()?;
        let load = RoutingLoad {
            cooldowns: tx.prepare("SELECT account_id, scope, until, set_at, reason, window, source_request_id, incarnation, spend_share FROM cooldowns ORDER BY account_id, scope")?
                .query_map([], |r| Ok(CooldownRow { account_id: r.get(0)?, scope: r.get(1)?, until: r.get(2)?, set_at: r.get(3)?, reason: r.get(4)?, window: r.get(5)?, source_request_id: r.get(6)?, incarnation: r.get::<_, i64>(7)? as u64, spend_share: r.get(8)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            // As with the windows, a previous holder's state is never loaded.
            credits: tx.prepare("SELECT c.account_id, c.incarnation, c.enabled, c.disabled_reason, c.utilization, c.observed_at \
                 FROM usage_credits c JOIN accounts ON accounts.id = c.account_id AND accounts.incarnation = c.incarnation \
                 ORDER BY c.account_id")?
                .query_map([], |r| Ok(CreditsRow { account_id: r.get(0)?, incarnation: r.get::<_, i64>(1)? as u64, enabled: r.get(2)?, disabled_reason: r.get(3)?, utilization: r.get(4)?, observed_at: r.get(5)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            lapsed_agents: tx.prepare(&format!("SELECT {LAPSED_AGENT_COLUMNS} FROM lapsed_agents ORDER BY key_id, session_id, agent_id, model"))?
                .query_map([], |r| Ok(LapsedAgentRow {
                    key_id: r.get("key_id")?, session_id: r.get("session_id")?, agent_id: r.get("agent_id")?,
                    model: r.get("model")?, key_generation: r.get("key_generation")?,
                    account_id: r.get("account_id")?, incarnation: r.get::<_, i64>("incarnation")? as u64,
                    last_used_at: r.get("last_used_at")?,
                    last_input_tokens: r.get::<_, i64>("last_input_tokens")? as u64,
                    burn: burn_from(r, BURN_COLUMNS)?, burn_banded: burn_from(r, BURN_BANDED_COLUMNS)?,
                    burn_fast: burn_from(r, BURN_FAST_COLUMNS)?, burn_unmetered: burn_from(r, BURN_UNMETERED_COLUMNS)?,
                    burn_unmetered_model: r.get("burn_unmetered_model")?,
                    speed: r.get("speed")?,
                    thinking_sums: json_column(r, r.as_ref().column_index("thinking_sums")?)?,
                    account_bound: r.get("account_bound")?,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            // As with the windows, a previous holder's hours are never loaded.
            model_week_hours: tx.prepare("SELECT h.account_id, h.incarnation, h.window, h.hour, h.share \
                 FROM model_week_hours h JOIN accounts ON accounts.id = h.account_id AND accounts.incarnation = h.incarnation \
                 ORDER BY h.account_id, h.window, h.hour")?
                .query_map([], |r| Ok(ModelWeekHourRow { account_id: r.get(0)?, incarnation: r.get::<_, i64>(1)? as u64, window: r.get(2)?, hour: r.get(3)?, share: r.get(4)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            overage: tx.prepare("SELECT account_id, in_use, since, reset, observed_at, source, incarnation FROM overage_state ORDER BY account_id")?
                .query_map([], |r| Ok(OverageRow { account_id: r.get(0)?, in_use: r.get(1)?, since: r.get(2)?, reset: r.get(3)?, observed_at: r.get(4)?, source: r.get(5)?, incarnation: r.get::<_, i64>(6)? as u64 }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            pins: tx.prepare(&format!("SELECT {PIN_COLUMNS} FROM pins ORDER BY key_id, session_id, agent_id, model"))?
                .query_map([], |r| Ok(PinRow {
                    key_id: r.get("key_id")?, session_id: r.get("session_id")?, agent_id: r.get("agent_id")?,
                    model: r.get("model")?, account_id: r.get("account_id")?,
                    epoch: r.get::<_, i64>("epoch")? as u64, created_at: r.get("created_at")?,
                    last_used_at: r.get("last_used_at")?,
                    key_generation: r.get("key_generation")?, incarnation: r.get::<_, i64>("incarnation")? as u64,
                    burn_at: r.get("burn_at")?, last_input_tokens: r.get::<_, i64>("last_input_tokens")? as u64,
                    last_refresh_at: r.get("last_refresh_at")?, cache_ttl_ms: r.get("cache_ttl_ms")?,
                    shed: shed_from(r)?, static_hash: r.get("static_hash")?, tail_hash: r.get("tail_hash")?,
                    lane: r.get("lane")?, burn: burn_from(r, BURN_COLUMNS)?,
                    burn_banded: burn_from(r, BURN_BANDED_COLUMNS)?,
                    burn_fast: burn_from(r, BURN_FAST_COLUMNS)?, burn_unmetered: burn_from(r, BURN_UNMETERED_COLUMNS)?,
                    burn_unmetered_model: r.get("burn_unmetered_model")?,
                    mark: mark_from(r)?,
                    entrypoint: none_if_empty(r.get("entrypoint")?),
                    auxiliary_only: r.get("auxiliary_only")?, effort: none_if_empty(r.get("effort")?),
                    ended: r.get("ended")?,
                    thinking_sums: json_column(r, r.as_ref().column_index("thinking_sums")?)?,
                    account_bound: r.get("account_bound")?,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            resolutions: all_resolutions(&tx)?,
            affinity: tx.prepare(&format!("SELECT {AFFINITY_COLUMNS} FROM affinity ORDER BY model, hash"))?
                .query_map([], affinity_of)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            // As with the windows, a previous holder's round is never loaded.
            shed_rounds: tx.prepare("SELECT r.pool_id, r.account_id, r.incarnation, r.round_at \
                 FROM shed_rounds r JOIN accounts ON accounts.id = r.account_id AND accounts.incarnation = r.incarnation \
                 ORDER BY r.pool_id, r.account_id")?
                .query_map([], |r| Ok(ShedRoundRow {
                    pool_id: r.get(0)?, account_id: r.get(1)?, incarnation: r.get::<_, i64>(2)? as u64,
                    round_at: r.get(3)?,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            thinking_bindings: tx.prepare("SELECT t.model, t.account_a, a.incarnation, t.account_b, b.incarnation, t.bound, t.observed_at \
                 FROM thinking_bindings t JOIN accounts a ON a.id = t.account_a JOIN accounts b ON b.id = t.account_b \
                 ORDER BY t.model, t.account_a, t.account_b")?
                .query_map([], |r| Ok(ThinkingBindingRow {
                    model: r.get(0)?, account_a: r.get(1)?, incarnation_a: r.get::<_, i64>(2)? as u64,
                    account_b: r.get(3)?, incarnation_b: r.get::<_, i64>(4)? as u64, bound: r.get(5)?,
                    observed_at: r.get(6)?,
                }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            // As with the windows, a previous holder's set is never loaded.
            window_sets: tx.prepare("SELECT s.account_id, s.incarnation, s.observed_at, s.windows \
                 FROM usage_window_sets s \
                 JOIN accounts ON accounts.id = s.account_id AND accounts.incarnation = s.incarnation \
                 ORDER BY s.account_id")?
                .query_map([], |r| Ok(WindowSetRow { account_id: r.get(0)?, incarnation: r.get::<_, i64>(1)? as u64, observed_at: r.get(2)?, windows: r.get(3)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            // A row of a previous holder of the id is never loaded: the replacement starts with no windows.
            windows: tx.prepare("SELECT w.account_id, w.incarnation, w.observed_at, w.reset, w.utilization, w.window, w.plan, w.tier \
                 FROM usage_windows w JOIN accounts ON accounts.id = w.account_id AND accounts.incarnation = w.incarnation \
                 ORDER BY w.account_id, w.window")?
                .query_map([], |r| Ok(WindowRow { account_id: r.get(0)?, incarnation: r.get::<_, i64>(1)? as u64, observed_at: r.get(2)?, plan: r.get(6)?, reset: r.get(3)?, tier: r.get(7)?, utilization: r.get(4)?, window: r.get(5)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        tx.commit()?;
        Ok(load)
    }

    /// One transaction for a batch of routing writes: the mutex taken once, `BEGIN IMMEDIATE`, every write of
    /// the closure against it, one commit. An error rolls the whole batch back and is the caller's to replay.
    pub fn write_routing<T>(
        &self,
        f: impl FnOnce(&mut RoutingTx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.park_on_routing_hold();
        let clock = self.clock.clone();
        self.routing_transaction(|tx| f(&mut RoutingTx { clock, tx }))
    }

    /// One `BEGIN IMMEDIATE` transaction under `routing_commit`, which an error from `f` rolls back.
    fn routing_transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.routing_commit(|conn| in_transaction(conn, f))
    }

    /// One routing commit under the lock, at `synchronous = NORMAL` while every other write of the connection runs
    /// at `FULL`: the routing state alone skips the sync, since the next requests rebuild what a power loss takes of
    /// it, and an operator's cooldown clear a power loss undoes is made again. Counted while the lock is still held,
    /// so a test that sees the rows sees the count too.
    pub(crate) fn routing_commit<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut conn = self.conn.lock().expect(LOCK_POISONED);
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let result = f(&mut conn);
        #[cfg(test)]
        crate::ROUTING_COMMIT_SAW.set(
            conn.pragma_query_value(None, "synchronous", |row| row.get(0))
                .ok(),
        );
        // The write has landed or failed by now and its answer is the caller's, but a connection left at `NORMAL`
        // would relax the operator's next write too, so the failure is an error line.
        if let Err(error) = conn.pragma_update(None, "synchronous", "FULL") {
            tracing::error!(%error, "the registry connection stays at synchronous = NORMAL after a routing write");
        }
        let value = result?;
        #[cfg(any(test, feature = "debug-hooks"))]
        self.hooks
            .routing_commits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(value)
    }

    pub fn purge_cooldowns_before(&self, now: i64) -> Result<usize, StoreError> {
        self.routing_transaction(|tx| {
            Ok(tx.execute("DELETE FROM cooldowns WHERE until < ?1", params![now])?)
        })
    }

    /// Drops every window, window set and credits row whose incarnation is not its account's. The router's purge
    /// tick calls it.
    pub fn purge_stale_windows(&self) -> Result<usize, StoreError> {
        self.routing_transaction(|tx| purge_stale_windows_on(tx))
    }

    /// All or nothing: a crash or a failed statement part way through would leave rows under the dated id with no
    /// spelling row to find them by after a restart, or a row deleted under the spelling and never re-inserted.
    pub fn rekey_resolution(&self, rekey: &RekeyWrite) -> Result<(), StoreError> {
        self.routing_transaction(|tx| rekey_resolution_on(tx, rekey))
    }

    /// The persisted map on its own: the accessor seeds the catalog cache's published map at start.
    pub fn resolutions(&self) -> Result<Vec<ResolutionRow>, StoreError> {
        let conn = self.conn.lock().expect(LOCK_POISONED);
        all_resolutions(&conn)
    }
}

/// Seams for the tests that write one row at a time, each its own commit, where the router's writes travel in
/// `write_routing` batches.
#[cfg(any(test, feature = "test-support"))]
impl Store {
    pub fn upsert_pin(&self, pin: &PinRow) -> Result<(), StoreError> {
        self.routing_transaction(|tx| upsert_pin_on(tx, pin))
    }

    /// One transaction, as `upsert_pin` takes.
    pub fn delete_pin(&self, key: &PinKey) -> Result<bool, StoreError> {
        self.routing_transaction(|tx| delete_pin_on(tx, key))
    }

    /// Every row lands or none does. A row under an existing (model, hash) replaces it.
    pub fn upsert_affinity(&self, rows: &[AffinityRow]) -> Result<(), StoreError> {
        self.routing_transaction(|tx| upsert_affinity_on(tx, rows))
    }

    /// One transaction for the whole batch, as `upsert_affinity` takes, so a large purge costs one commit.
    pub fn delete_affinity(&self, keys: &[(String, String)]) -> Result<usize, StoreError> {
        self.routing_transaction(|tx| delete_affinity_on(tx, keys))
    }

    /// The later deadline always wins: a short `Retry-After` arriving after a long unified reset must not
    /// release an account the provider still refuses.
    pub fn upsert_cooldown(&self, row: &CooldownRow) -> Result<(), StoreError> {
        self.routing_commit(|conn| upsert_cooldown_on(conn, row))
    }

    pub fn delete_cooldown(
        &self,
        account_id: &str,
        scope: &CooldownScope,
    ) -> Result<bool, StoreError> {
        self.routing_commit(|conn| delete_cooldown_on(conn, account_id, scope))
    }

    pub fn upsert_overage(&self, row: &OverageRow) -> Result<bool, StoreError> {
        self.routing_commit(|conn| upsert_overage_on(conn, row))
    }

    /// Written only while `row.incarnation` is the account's current one, and only when newer than the stored
    /// row or when that row is a previous holder's: one statement, so a write queued for a holder the id has since
    /// left lands nowhere. Answers whether a row was written.
    pub fn upsert_window(&self, row: &WindowRow) -> Result<bool, StoreError> {
        self.routing_commit(|conn| upsert_window_on(conn, row))
    }

    /// Written as `upsert_window` writes a window: for the account's current incarnation, and only when newer than the
    /// stored row or when that row is a previous holder's. Answers whether a row was written.
    pub fn upsert_window_set(&self, row: &WindowSetRow) -> Result<bool, StoreError> {
        self.routing_commit(|conn| upsert_window_set_on(conn, row))
    }

    /// Removes one window's row under the account's current incarnation. Answers whether a row left.
    pub fn delete_window(
        &self,
        account_id: &str,
        window: &str,
        incarnation: u64,
    ) -> Result<bool, StoreError> {
        self.routing_commit(|conn| delete_window_on(conn, account_id, window, incarnation))
    }

    /// Written as `upsert_window` writes a window: only for the account's current incarnation, and only when
    /// newer than the stored row or when that row is a previous holder's. Answers whether a row was written.
    pub fn upsert_credits(&self, row: &CreditsRow) -> Result<bool, StoreError> {
        self.routing_commit(|conn| upsert_credits_on(conn, row))
    }

    /// Writes the pool's round on the account, replacing the one before it. `incarnation` is the account's at the
    /// round.
    pub fn replace_shed_round(
        &self,
        pool_id: &str,
        account_id: &str,
        incarnation: u64,
        round_at: i64,
    ) -> Result<(), StoreError> {
        self.routing_transaction(|tx| {
            replace_shed_round_on(tx, pool_id, account_id, incarnation, round_at)
        })
    }

    pub fn purge_shed_rounds_before(&self, before: i64) -> Result<usize, StoreError> {
        self.routing_transaction(|tx| purge_shed_rounds_before_on(tx, before))
    }
}

fn all_resolutions(conn: &Connection) -> Result<Vec<ResolutionRow>, StoreError> {
    let rows = conn
        .prepare("SELECT spelling, dated_id, resolved_at FROM model_resolutions ORDER BY spelling")?
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

/// The routing writes on one open transaction. Each is the inner form of the public method of the same name.
pub struct RoutingTx<'a> {
    pub(crate) clock: Clock,
    pub(crate) tx: &'a Transaction<'a>,
}

impl RoutingTx<'_> {
    pub fn delete_affinity(&mut self, keys: &[(String, String)]) -> Result<usize, StoreError> {
        delete_affinity_on(self.tx, keys)
    }

    pub fn delete_cooldown(
        &mut self,
        account_id: &str,
        scope: &CooldownScope,
    ) -> Result<bool, StoreError> {
        delete_cooldown_on(self.tx, account_id, scope)
    }

    pub fn delete_lapsed_agent(&mut self, key: &PinKey) -> Result<(), StoreError> {
        delete_lapsed_agent_on(self.tx, key)
    }

    pub fn delete_pin(&mut self, key: &PinKey) -> Result<bool, StoreError> {
        delete_pin_on(self.tx, key)
    }

    /// Drops every model week hour numbered under `before`. Only the router's batches write the hours, so `Store` has
    /// no single-write form.
    pub fn purge_model_week_hours_before(&mut self, before: i64) -> Result<usize, StoreError> {
        Ok(self
            .tx
            .prepare_cached("DELETE FROM model_week_hours WHERE hour < ?1")?
            .execute(params![before])?)
    }

    pub fn purge_shed_rounds_before(&mut self, before: i64) -> Result<usize, StoreError> {
        purge_shed_rounds_before_on(self.tx, before)
    }

    pub fn purge_stale_windows(&mut self) -> Result<usize, StoreError> {
        purge_stale_windows_on(self.tx)
    }

    pub fn rekey_resolution(&mut self, rekey: &RekeyWrite) -> Result<(), StoreError> {
        rekey_resolution_on(self.tx, rekey)
    }

    pub fn replace_shed_round(
        &mut self,
        pool_id: &str,
        account_id: &str,
        incarnation: u64,
        round_at: i64,
    ) -> Result<(), StoreError> {
        replace_shed_round_on(self.tx, pool_id, account_id, incarnation, round_at)
    }

    /// Writes the pair's fact in place of the one it held, only while both ids' holders carry the incarnations the row
    /// names. Only from the router's batches. Answers whether a row was written.
    pub fn replace_thinking_binding(
        &mut self,
        row: &ThinkingBindingRow,
    ) -> Result<bool, StoreError> {
        replace_thinking_binding_on(self.tx, row)
    }

    pub fn set_state_if_generations(
        &mut self,
        id: &str,
        state: AccountState,
        cause: StateCause,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> Result<Option<(u64, AccountState)>, StoreError> {
        set_state_if_generations_on(
            self.tx,
            (self.clock)(),
            id,
            state,
            cause,
            incarnation,
            lifecycle_generation,
            credential_generation,
        )
    }

    pub fn upsert_affinity(&mut self, rows: &[AffinityRow]) -> Result<(), StoreError> {
        upsert_affinity_on(self.tx, rows)
    }

    pub fn upsert_cooldown(&mut self, row: &CooldownRow) -> Result<(), StoreError> {
        upsert_cooldown_on(self.tx, row)
    }

    pub fn upsert_credits(&mut self, row: &CreditsRow) -> Result<bool, StoreError> {
        upsert_credits_on(self.tx, row)
    }

    /// Writes the record in place of the one its key held. Only the router's batches write a record, so `Store` has no
    /// single-write form.
    pub fn upsert_lapsed_agent(&mut self, row: &LapsedAgentRow) -> Result<(), StoreError> {
        upsert_lapsed_agent_on(self.tx, row)
    }

    /// Writes the hour's sum as the routing state holds it, so a write the backlog kept in place of an earlier one for
    /// the same hour still lands the whole sum. Only for the account's current incarnation, a previous holder's row
    /// replaced, and only from the router's batches. Answers whether a row was written.
    pub fn upsert_model_week_hour(&mut self, row: &ModelWeekHourRow) -> Result<bool, StoreError> {
        // The insert-select is the incarnation check, as `upsert_window_on`'s is.
        let written = self
            .tx
            .prepare_cached(
                "INSERT INTO model_week_hours (account_id, incarnation, window, hour, share) \
                 SELECT ?1, ?2, ?3, ?4, ?5 FROM accounts WHERE id = ?1 AND incarnation = ?2 \
                 ON CONFLICT(account_id, window, hour) DO UPDATE SET \
                   incarnation = excluded.incarnation, share = excluded.share",
            )?
            .execute(params![
                row.account_id,
                row.incarnation as i64,
                row.window,
                row.hour,
                row.share
            ])?;
        Ok(written > 0)
    }

    pub fn upsert_overage(&mut self, row: &OverageRow) -> Result<bool, StoreError> {
        upsert_overage_on(self.tx, row)
    }

    pub fn upsert_pin(&mut self, pin: &PinRow) -> Result<(), StoreError> {
        upsert_pin_on(self.tx, pin)
    }

    pub fn upsert_window(&mut self, row: &WindowRow) -> Result<bool, StoreError> {
        upsert_window_on(self.tx, row)
    }

    pub fn upsert_window_set(&mut self, row: &WindowSetRow) -> Result<bool, StoreError> {
        upsert_window_set_on(self.tx, row)
    }

    pub fn delete_window(
        &mut self,
        account_id: &str,
        window: &str,
        incarnation: u64,
    ) -> Result<bool, StoreError> {
        delete_window_on(self.tx, account_id, window, incarnation)
    }
}

fn in_transaction<T>(
    conn: &mut Connection,
    f: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = f(&tx)?;
    tx.commit()?;
    Ok(value)
}

// The upsert binds the pin's columns in this one order, so `upsert_pin_on` lists its values in it. The load reads each
// column by its name.
const PIN_COLUMNS: &str = "key_id, session_id, agent_id, model, account_id, epoch, created_at, last_used_at, \
     key_generation, incarnation, burn_at, last_input_tokens, last_refresh_at, cache_ttl_ms, shed_from, shed_at, \
     static_hash, tail_hash, lane, \
     burn_cache_read, burn_input, burn_output, burn_write_1h, burn_write_5m, \
     burn_banded_cache_read, burn_banded_input, burn_banded_output, burn_banded_write_1h, burn_banded_write_5m, \
     burn_fast_cache_read, burn_fast_input, burn_fast_output, burn_fast_write_1h, burn_fast_write_5m, \
     burn_unmetered_cache_read, burn_unmetered_input, burn_unmetered_output, burn_unmetered_write_1h, \
     burn_unmetered_write_5m, burn_unmetered_model, \
     marked_at, marked_on, marked_window, entrypoint, shed_reason, auxiliary_only, effort, ended, thinking_sums, \
     account_bound, marked_source";

// The upsert binds a lapsed agent's columns in this one order, so `upsert_lapsed_agent_on` lists its values in it. The
// burn columns carry the pins' names, so the load reads both tables through the lists below.
const LAPSED_AGENT_COLUMNS: &str = "key_id, session_id, agent_id, model, key_generation, account_id, incarnation, \
     last_used_at, last_input_tokens, \
     burn_cache_read, burn_input, burn_output, burn_write_1h, burn_write_5m, \
     burn_banded_cache_read, burn_banded_input, burn_banded_output, burn_banded_write_1h, burn_banded_write_5m, \
     burn_fast_cache_read, burn_fast_input, burn_fast_output, burn_fast_write_1h, burn_fast_write_5m, \
     burn_unmetered_cache_read, burn_unmetered_input, burn_unmetered_output, burn_unmetered_write_1h, \
     burn_unmetered_write_5m, burn_unmetered_model, speed, thinking_sums, account_bound";

// Each burn copy's columns, in `PinBurn`'s field order: `burn_from` takes them by position.
const BURN_COLUMNS: [&str; 5] = [
    "burn_cache_read",
    "burn_input",
    "burn_output",
    "burn_write_1h",
    "burn_write_5m",
];
const BURN_BANDED_COLUMNS: [&str; 5] = [
    "burn_banded_cache_read",
    "burn_banded_input",
    "burn_banded_output",
    "burn_banded_write_1h",
    "burn_banded_write_5m",
];
const BURN_FAST_COLUMNS: [&str; 5] = [
    "burn_fast_cache_read",
    "burn_fast_input",
    "burn_fast_output",
    "burn_fast_write_1h",
    "burn_fast_write_5m",
];
const BURN_UNMETERED_COLUMNS: [&str; 5] = [
    "burn_unmetered_cache_read",
    "burn_unmetered_input",
    "burn_unmetered_output",
    "burn_unmetered_write_1h",
    "burn_unmetered_write_5m",
];

fn burn_from(
    r: &rusqlite::Row<'_>,
    [cache_read, input, output, write_1h, write_5m]: [&str; 5],
) -> rusqlite::Result<PinBurn> {
    Ok(PinBurn {
        cache_read: r.get(cache_read)?,
        input: r.get(input)?,
        output: r.get(output)?,
        write_1h: r.get(write_1h)?,
        write_5m: r.get(write_5m)?,
    })
}

/// The shed stamp of a row whose `shed_from` and `shed_at` columns are both set: every write sets or clears the two
/// together.
fn shed_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<ShedStamp>> {
    Ok(match (r.get("shed_from")?, r.get("shed_at")?) {
        (Some(from), Some(at)) => Some(ShedStamp {
            from,
            at,
            reason: r.get("shed_reason")?,
        }),
        _ => None,
    })
}

/// The mark of a row whose three mark columns are all set: every write sets or clears them together.
fn mark_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<PinMark>> {
    Ok(
        match (
            r.get("marked_at")?,
            r.get("marked_on")?,
            r.get("marked_window")?,
        ) {
            (Some(at), Some(on), Some(window)) => Some(PinMark {
                at,
                on,
                window,
                source: r.get("marked_source")?,
            }),
            _ => None,
        },
    )
}

/// The upsert of `columns` into `table`, bound `?1` to `?N` in the columns' order. On a conflict over `key`, every
/// other column takes the inserted value, so a column added to the list is renewed with the rest.
fn upsert_sql(table: &str, columns: &str, key: &[&str]) -> String {
    let names: Vec<&str> = columns.split(',').map(str::trim).collect();
    let values = (1..=names.len())
        .map(|bind| format!("?{bind}"))
        .collect::<Vec<_>>()
        .join(", ");
    let set = names
        .iter()
        .filter(|name| !key.contains(name))
        .map(|name| format!("{name} = excluded.{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {table} ({columns}) VALUES ({values}) ON CONFLICT({}) DO UPDATE SET {set}",
        key.join(", ")
    )
}

static UPSERT_PIN: LazyLock<String> = LazyLock::new(|| {
    upsert_sql(
        "pins",
        PIN_COLUMNS,
        &["key_id", "session_id", "agent_id", "model"],
    )
});

fn upsert_pin_on(conn: &Connection, pin: &PinRow) -> Result<(), StoreError> {
    conn.prepare_cached(&UPSERT_PIN)?.execute(params![
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
        pin.burn_at,
        pin.last_input_tokens as i64,
        pin.last_refresh_at,
        pin.cache_ttl_ms,
        pin.shed.as_ref().map(|shed| &shed.from),
        pin.shed.as_ref().map(|shed| shed.at),
        pin.static_hash,
        pin.tail_hash,
        pin.lane,
        pin.burn.cache_read,
        pin.burn.input,
        pin.burn.output,
        pin.burn.write_1h,
        pin.burn.write_5m,
        pin.burn_banded.cache_read,
        pin.burn_banded.input,
        pin.burn_banded.output,
        pin.burn_banded.write_1h,
        pin.burn_banded.write_5m,
        pin.burn_fast.cache_read,
        pin.burn_fast.input,
        pin.burn_fast.output,
        pin.burn_fast.write_1h,
        pin.burn_fast.write_5m,
        pin.burn_unmetered.cache_read,
        pin.burn_unmetered.input,
        pin.burn_unmetered.output,
        pin.burn_unmetered.write_1h,
        pin.burn_unmetered.write_5m,
        pin.burn_unmetered_model,
        pin.mark.as_ref().map(|mark| mark.at),
        pin.mark.as_ref().map(|mark| &mark.on),
        pin.mark.as_ref().map(|mark| &mark.window),
        pin.entrypoint.as_deref().unwrap_or_default(),
        pin.shed.as_ref().and_then(|shed| shed.reason.as_ref()),
        pin.auxiliary_only,
        pin.effort.as_deref().unwrap_or_default(),
        pin.ended,
        serde_json::to_string(&pin.thinking_sums)?,
        pin.account_bound,
        pin.mark.as_ref().and_then(|mark| mark.source.as_ref())
    ])?;
    Ok(())
}

/// An optional pin column read back: the write stores `None` as the empty text, since the column is `NOT NULL`.
fn none_if_empty(text: String) -> Option<String> {
    Some(text).filter(|text| !text.is_empty())
}

/// The pin's row alone, whether it leaves for good or for a replacement.
fn delete_pin_on(conn: &Connection, key: &PinKey) -> Result<bool, StoreError> {
    let mut stmt = conn.prepare_cached(
        "DELETE FROM pins WHERE key_id = ?1 AND session_id = ?2 AND agent_id = ?3 AND model = ?4",
    )?;
    Ok(stmt.execute(params![key.key_id, key.session_id, key.agent_id, key.model])? > 0)
}

static UPSERT_LAPSED_AGENT: LazyLock<String> = LazyLock::new(|| {
    upsert_sql(
        "lapsed_agents",
        LAPSED_AGENT_COLUMNS,
        &["key_id", "session_id", "agent_id", "model"],
    )
});

fn upsert_lapsed_agent_on(conn: &Connection, row: &LapsedAgentRow) -> Result<(), StoreError> {
    conn.prepare_cached(&UPSERT_LAPSED_AGENT)?.execute(params![
        row.key_id,
        row.session_id,
        row.agent_id,
        row.model,
        row.key_generation,
        row.account_id,
        row.incarnation as i64,
        row.last_used_at,
        row.last_input_tokens as i64,
        row.burn.cache_read,
        row.burn.input,
        row.burn.output,
        row.burn.write_1h,
        row.burn.write_5m,
        row.burn_banded.cache_read,
        row.burn_banded.input,
        row.burn_banded.output,
        row.burn_banded.write_1h,
        row.burn_banded.write_5m,
        row.burn_fast.cache_read,
        row.burn_fast.input,
        row.burn_fast.output,
        row.burn_fast.write_1h,
        row.burn_fast.write_5m,
        row.burn_unmetered.cache_read,
        row.burn_unmetered.input,
        row.burn_unmetered.output,
        row.burn_unmetered.write_1h,
        row.burn_unmetered.write_5m,
        row.burn_unmetered_model,
        row.speed,
        serde_json::to_string(&row.thinking_sums)?,
        row.account_bound
    ])?;
    Ok(())
}

fn delete_lapsed_agent_on(conn: &Connection, key: &PinKey) -> Result<(), StoreError> {
    conn.prepare_cached(
        "DELETE FROM lapsed_agents WHERE key_id = ?1 AND session_id = ?2 AND agent_id = ?3 AND model = ?4",
    )?
    .execute(params![key.key_id, key.session_id, key.agent_id, key.model])?;
    Ok(())
}

/// Writes the pair's fact in place of the one it held, only while both ids' holders carry the incarnations the row
/// names, and answers whether a row was written.
fn replace_thinking_binding_on(
    conn: &Connection,
    row: &ThinkingBindingRow,
) -> Result<bool, StoreError> {
    // A pair whose account left between the observation and the write lands nowhere, rather than failing the
    // batch on the reference the cascade would have cleared, and so does one whose id a new holder took: the
    // table names no incarnation, so a row that landed would read as the new holder's.
    let written = conn
        .prepare_cached(
            "INSERT OR REPLACE INTO thinking_bindings (model, account_a, account_b, bound, observed_at) \
             SELECT ?1, ?2, ?3, ?4, ?5 \
             WHERE EXISTS (SELECT 1 FROM accounts WHERE id = ?2 AND incarnation = ?6) \
             AND EXISTS (SELECT 1 FROM accounts WHERE id = ?3 AND incarnation = ?7)",
        )?
        .execute(params![
            row.model,
            row.account_a,
            row.account_b,
            row.bound,
            row.observed_at,
            row.incarnation_a as i64,
            row.incarnation_b as i64
        ])?;
    Ok(written > 0)
}

fn upsert_cooldown_on(conn: &Connection, row: &CooldownRow) -> Result<(), StoreError> {
    conn.prepare_cached("INSERT INTO cooldowns (account_id, scope, until, set_at, reason, window, source_request_id, incarnation, spend_share) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(account_id, scope) DO UPDATE SET \
           set_at = MAX(cooldowns.set_at, excluded.set_at), \
           spend_share = CASE WHEN excluded.set_at > cooldowns.set_at THEN excluded.spend_share ELSE cooldowns.spend_share END, \
           reason = CASE WHEN excluded.until > cooldowns.until THEN excluded.reason ELSE cooldowns.reason END, \
           window = CASE WHEN excluded.until > cooldowns.until THEN excluded.window ELSE cooldowns.window END, \
           source_request_id = CASE WHEN excluded.until > cooldowns.until THEN excluded.source_request_id ELSE cooldowns.source_request_id END, \
           until = MAX(cooldowns.until, excluded.until), \
           incarnation = excluded.incarnation")?.execute(params![row.account_id, row.scope, row.until, row.set_at, row.reason, row.window, row.source_request_id, row.incarnation as i64, row.spend_share])?;
    Ok(())
}

fn delete_cooldown_on(
    conn: &Connection,
    account_id: &str,
    scope: &CooldownScope,
) -> Result<bool, StoreError> {
    Ok(conn
        .prepare_cached("DELETE FROM cooldowns WHERE account_id = ?1 AND scope = ?2")?
        .execute(params![account_id, scope])?
        > 0)
}

static UPSERT_AFFINITY: LazyLock<String> =
    LazyLock::new(|| upsert_sql("affinity", AFFINITY_COLUMNS, &["model", "hash"]));

fn upsert_affinity_on(conn: &Connection, rows: &[AffinityRow]) -> Result<(), StoreError> {
    let mut stmt = conn.prepare_cached(&UPSERT_AFFINITY)?;
    for row in rows {
        stmt.execute(params![
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
            row.root,
            row.lane,
            row.depth,
            row.message_id
        ])?;
    }
    Ok(())
}

fn delete_affinity_on(conn: &Connection, keys: &[(String, String)]) -> Result<usize, StoreError> {
    let mut deleted = 0;
    let mut stmt = conn.prepare_cached("DELETE FROM affinity WHERE model = ?1 AND hash = ?2")?;
    for (model, hash) in keys {
        deleted += stmt.execute(params![model, hash])?;
    }
    Ok(deleted)
}

fn upsert_overage_on(conn: &Connection, row: &OverageRow) -> Result<bool, StoreError> {
    // A reading from another holder of the id is no timeline to be newer than.
    let written = conn.prepare_cached("INSERT INTO overage_state (account_id, in_use, since, reset, observed_at, source, incarnation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(account_id) DO UPDATE SET in_use = excluded.in_use, since = excluded.since, reset = excluded.reset, observed_at = excluded.observed_at, source = excluded.source, incarnation = excluded.incarnation \
         WHERE excluded.observed_at > overage_state.observed_at OR excluded.incarnation <> overage_state.incarnation")?.execute(params![row.account_id, row.in_use, row.since, row.reset, row.observed_at, row.source, row.incarnation as i64])?;
    Ok(written > 0)
}

fn upsert_window_on(conn: &Connection, row: &WindowRow) -> Result<bool, StoreError> {
    // The `WHERE` on the select is what lets SQLite parse the `ON CONFLICT` of an insert-select, and it is
    // the incarnation check too: a deleted and re-paired id answers no row, so nothing is inserted.
    let written = conn.prepare_cached("INSERT INTO usage_windows (account_id, window, utilization, reset, observed_at, incarnation, plan, tier) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 FROM accounts WHERE id = ?1 AND incarnation = ?6 \
         ON CONFLICT(account_id, window) DO UPDATE SET utilization = excluded.utilization, \
           reset = excluded.reset, observed_at = excluded.observed_at, incarnation = excluded.incarnation, \
           plan = excluded.plan, tier = excluded.tier \
         WHERE excluded.observed_at > usage_windows.observed_at \
           OR excluded.incarnation <> usage_windows.incarnation")?.execute(params![
            row.account_id,
            row.window,
            row.utilization,
            row.reset,
            row.observed_at,
            row.incarnation as i64,
            row.plan,
            row.tier
        ])?;
    Ok(written > 0)
}

fn upsert_window_set_on(conn: &Connection, row: &WindowSetRow) -> Result<bool, StoreError> {
    // The insert-select is the incarnation check, as `upsert_window_on`'s is.
    let written = conn
        .prepare_cached(
            "INSERT INTO usage_window_sets (account_id, windows, observed_at, incarnation) \
         SELECT ?1, ?2, ?3, ?4 FROM accounts WHERE id = ?1 AND incarnation = ?4 \
         ON CONFLICT(account_id) DO UPDATE SET windows = excluded.windows, \
           observed_at = excluded.observed_at, incarnation = excluded.incarnation \
         WHERE excluded.observed_at > usage_window_sets.observed_at \
           OR excluded.incarnation <> usage_window_sets.incarnation",
        )?
        .execute(params![
            row.account_id,
            row.windows,
            row.observed_at,
            row.incarnation as i64
        ])?;
    Ok(written > 0)
}

fn delete_window_on(
    conn: &Connection,
    account_id: &str,
    window: &str,
    incarnation: u64,
) -> Result<bool, StoreError> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM usage_windows WHERE account_id = ?1 AND window = ?2 AND incarnation = ?3",
        )?
        .execute(params![account_id, window, incarnation as i64])?
        > 0)
}

fn upsert_credits_on(conn: &Connection, row: &CreditsRow) -> Result<bool, StoreError> {
    let written = conn.prepare_cached("INSERT INTO usage_credits (account_id, enabled, disabled_reason, utilization, observed_at, incarnation) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6 FROM accounts WHERE id = ?1 AND incarnation = ?6 \
         ON CONFLICT(account_id) DO UPDATE SET enabled = excluded.enabled, \
           disabled_reason = excluded.disabled_reason, utilization = excluded.utilization, \
           observed_at = excluded.observed_at, incarnation = excluded.incarnation \
         WHERE excluded.observed_at > usage_credits.observed_at \
           OR excluded.incarnation <> usage_credits.incarnation")?.execute(params![
            row.account_id,
            row.enabled,
            row.disabled_reason,
            row.utilization,
            row.observed_at,
            row.incarnation as i64
        ])?;
    Ok(written > 0)
}

fn purge_stale_windows_on(conn: &Connection) -> Result<usize, StoreError> {
    let mut purged = 0;
    for table in ["usage_windows", "usage_credits", "usage_window_sets"] {
        purged += conn
            .prepare_cached(&format!(
                "DELETE FROM {table} WHERE NOT EXISTS (SELECT 1 FROM accounts \
                 WHERE accounts.id = {table}.account_id AND accounts.incarnation = {table}.incarnation)"
            ))?
            .execute([])?;
    }
    Ok(purged)
}

fn replace_shed_round_on(
    conn: &Connection,
    pool_id: &str,
    account_id: &str,
    incarnation: u64,
    round_at: i64,
) -> Result<(), StoreError> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR REPLACE INTO shed_rounds (pool_id, account_id, incarnation, round_at) \
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    stmt.execute(params![pool_id, account_id, incarnation as i64, round_at])?;
    Ok(())
}

fn purge_shed_rounds_before_on(conn: &Connection, before: i64) -> Result<usize, StoreError> {
    Ok(conn
        .prepare_cached("DELETE FROM shed_rounds WHERE round_at < ?1")?
        .execute(params![before])?)
}

fn rekey_resolution_on(conn: &Connection, rekey: &RekeyWrite) -> Result<(), StoreError> {
    conn.prepare_cached("INSERT INTO model_resolutions (spelling, dated_id, resolved_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(spelling) DO UPDATE SET dated_id = excluded.dated_id, resolved_at = excluded.resolved_at")?.execute(params![
            rekey.resolution.spelling,
            rekey.resolution.dated_id,
            rekey.resolution.resolved_at
        ])?;
    for key in &rekey.delete_pins {
        delete_pin_on(conn, key)?;
    }
    for pin in &rekey.upsert_pins {
        upsert_pin_on(conn, pin)?;
    }
    for (account_id, scope) in &rekey.delete_cooldowns {
        delete_cooldown_on(conn, account_id, scope)?;
    }
    for row in &rekey.upsert_cooldowns {
        upsert_cooldown_on(conn, row)?;
    }
    for (model, account_a, account_b) in &rekey.delete_bindings {
        conn.prepare_cached(
            "DELETE FROM thinking_bindings WHERE model = ?1 AND account_a = ?2 AND account_b = ?3",
        )?
        .execute(params![model, account_a, account_b])?;
    }
    // A moved fact lands only under the holders it names, as the router's own write of a fact does.
    for row in &rekey.upsert_bindings {
        replace_thinking_binding_on(conn, row)?;
    }
    for key in &rekey.delete_lapsed_agents {
        delete_lapsed_agent_on(conn, key)?;
    }
    for row in &rekey.upsert_lapsed_agents {
        upsert_lapsed_agent_on(conn, row)?;
    }
    Ok(())
}
