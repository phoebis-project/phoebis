//! The routing log: the merged page of decisions and events under the view's filters, and one decision's record.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    decision_record, list_routing_log, DecisionListRow, EventListRow, RoutingLogCursor,
    RoutingLogFilter, RoutingLogRow, DECISION_KINDS, EVENT_KINDS, PROBE_KIND,
};
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{ErrorBody, LedgerFailure};
use crate::requests::{decode_diff, list_param};
use crate::routes::failure;
use crate::AdminState;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 200;
const HOUR_MS: i64 = 3_600_000;

/// The view's URL keys, alphabetical. A key outside them is refused rather than ignored, so a link built for another
/// view never reads as a filter that took.
const KEYS: &[&str] = &[
    "accounts", "cursor", "from", "keys", "kinds", "limit", "models", "pools", "request",
    "session", "to",
];

/// A value that parses as an integer is Unix milliseconds, else an RFC 3339 instant.
fn time(q: &HashMap<String, String>, name: &'static str) -> Result<Option<i64>, String> {
    let Some(raw) = q.get(name) else {
        return Ok(None);
    };
    if let Ok(ms) = raw.parse::<i64>() {
        return Ok(Some(ms));
    }
    jiff::Timestamp::from_str(raw)
        .map(|t| Some(t.as_millisecond()))
        .map_err(|_| format!("{name} is not a time"))
}

/// The routing decisions and the events of a range, merged into one page, newest first.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct RoutingLog {
    /// At one `at`, the decisions before the events. Each row's `type` is `decision` or `event`.
    #[schema(value_type = Vec<Row>)]
    pub rows: Vec<Value>,
    /// Set only while a row lies past the page. A page of exactly `limit` rows with none beyond answers null.
    pub next_cursor: Option<String>,
    /// The rows under the whole filter, past the page too. Answered under a `request` filter alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub counts: Option<Counts>,
}

/// One row of the page.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(untagged)]
pub(crate) enum Row {
    Decision(Box<DecisionRow>),
    Event(Box<EventRow>),
}

/// A decision row. The route answers values and no words: the console labels `by` and each `fact` key. An
/// exhausted row after a hold whose bound ran out reads `held`, the wait, and `answer`, the status the client got,
/// with a null `unit`. A request that passed an account over at its cap after a wait that freed, as when another
/// request took the freed slot, reads the same `answer`.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecisionRow {
    /// `decision`.
    #[serde(rename = "type")]
    #[schema(example = "decision")]
    pub kind_of_row: String,
    pub request_id: String,
    pub seq: u32,
    /// The wire attempt the decision dispatched.
    pub attempt: u32,
    /// The request's attempts, null until its row lands.
    pub attempt_count: Option<u32>,
    pub at: i64,
    pub kind: String,
    pub probe: bool,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    /// The client's own session.
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub account_id: Option<String>,
    /// A move's source.
    pub from_account_id: Option<String>,
    /// What decided the row.
    pub by: SummaryBy,
    /// One to three pairs.
    pub fact: Vec<SummaryFact>,
    #[schema(required = true)]
    pub rule: Option<String>,
    pub strategy: String,
    #[schema(value_type = Option<Vec<u64>>, required = true)]
    pub tier: Option<(u8, u32, u8)>,
    #[schema(required = true)]
    pub rung: Option<SummaryPlacement>,
    #[schema(required = true)]
    pub score: Option<SummaryScore>,
    #[schema(required = true)]
    pub runner_up: Option<SummaryScore>,
    /// How many candidates the choice passed over.
    pub skipped: u32,
    #[schema(required = true)]
    pub warmth: Option<SummaryWarmth>,
    #[schema(required = true)]
    pub hold: Option<SummaryHold>,
    #[schema(required = true)]
    pub exhaustion: Option<SummaryExhaustion>,
    #[schema(required = true)]
    pub refusal: Option<SummaryRefusal>,
    pub fast: bool,
    /// `hit`, `read_tokens`, `promised_tokens` and `input_tokens` of the attempt that answered, read as the
    /// record's completion reads it. Null until that attempt has usage.
    pub cache: Option<CacheFacts>,
    /// That attempt's upstream status. Null for a decision that dispatched nothing or one still running.
    pub status: Option<u16>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryBy {
    /// `rule`, `rung`, `strategy`, `hold`, `exhaustion`, `refusal` or `none`.
    #[schema(example = "rule")]
    pub kind: String,
    /// The rule's, the rung's, the strategy's, the hold's place, the exhaustion's kind or the refusal's kind.
    pub value: String,
    #[schema(value_type = Option<Vec<u64>>)]
    pub tier: Option<(u8, u32, u8)>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryFact {
    pub key: String,
    pub value: FactValue,
    /// `ms`, `tokens`, `percent`, `percent_per_hour`, `usd`, `time`, `count` or `text`, how the value is drawn.
    #[schema(example = "ms")]
    pub unit: Option<String>,
    /// Where the value names one, else null.
    pub entity: Option<FactEntity>,
}

/// A fact's value in its bare JSON form: a boolean, a number, a word or a list of words.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum FactValue {
    Bool(bool),
    Int(i64),
    Number(f64),
    Text(String),
    List(Vec<String>),
}

// JSON Schema counts a whole value as a number, so the derive's separate `integer` and `number` members would both
// match it and fail `oneOf`. `Int` and `Number` share the one `number` member here.
impl utoipa::PartialSchema for FactValue {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, OneOfBuilder, Schema, Type};
        Schema::OneOf(
            OneOfBuilder::new()
                .item(ObjectBuilder::new().schema_type(Type::Boolean))
                .item(ObjectBuilder::new().schema_type(Type::Number))
                .item(ObjectBuilder::new().schema_type(Type::String))
                .item(ArrayBuilder::new().items(ObjectBuilder::new().schema_type(Type::String)))
                .description(Some(
                    "A fact's value in its bare JSON form: a boolean, a number, a word or a list of words.",
                ))
                .build(),
        )
        .into()
    }
}

impl ToSchema for FactValue {}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FactEntity {
    /// One of `account`, `key`, `pool` or `model`.
    #[schema(example = "account")]
    pub kind: String,
    pub id: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryScore {
    pub account_id: String,
    pub score: f64,
    pub unit: String,
}

/// Where a fresh placement's account came from.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryPlacement {
    /// `content` (the deepest message boundary), `static` (the deepest static boundary) or `with_main` (the parent
    /// agent's pin, or another agent's pin of the session when every account was at its cap).
    #[schema(example = "content")]
    pub rung: String,
    /// The matched boundary's depth, 0 for `with_main`.
    pub depth: u32,
    /// The prefix size the entry promised, 0 for `with_main`.
    pub prefix_tokens: u64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryWarmth {
    pub verdict: String,
    pub cause: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryHold {
    pub account_id: String,
    pub cohort: Option<String>,
    pub bound_ms: i64,
    /// `cap` for a hold on a slot, `reset` for a spent window about to reset, `slow_down` for a slowed pin's wait
    /// before its head.
    #[schema(example = "cap")]
    pub reason: String,
    /// The instant the request waits for: under `reset` the later of the spent window's reset and the end of the
    /// account's cooldown, under `slow_down` the pin's deadline. Null under `cap`.
    pub until: Option<i64>,
    /// The account's factor the hold was read under. Set under `slow_down`, null otherwise.
    pub factor: Option<f64>,
    /// The speed the account's holds realize together. Set under `slow_down`, null otherwise.
    pub speed: Option<f64>,
    /// The account's stretch at the read. Set under `slow_down`, null otherwise.
    pub stretch_ms: Option<i64>,
    /// The pool's `slow_down_stretch_seconds`. Set under `slow_down`, null otherwise.
    pub threshold_ms: Option<i64>,
    /// The pin's cycle the hold was sized from. Set under `slow_down`, null otherwise.
    pub cycle_ms: Option<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryExhaustion {
    pub kind: String,
    pub deadline: Option<i64>,
    pub window: Option<String>,
    pub reason: Option<String>,
    pub obstacle: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryRefusal {
    pub kind: String,
    /// The refusal's structured word: a key refusal's variant in snake case, `model_not_allowed` for a resolve
    /// refusal, `drift` for the relay's refusal of a drifted client.
    pub code: String,
    pub reason: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheFacts {
    pub hit: bool,
    pub read_tokens: u64,
    pub promised_tokens: Option<u64>,
    pub input_tokens: u64,
}

/// An event row.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventRow {
    /// `event`.
    #[serde(rename = "type")]
    #[schema(example = "event")]
    pub kind_of_row: String,
    /// The row's id.
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub request_id: Option<String>,
    pub actor: Option<String>,
    /// The object, decoded.
    #[schema(value_type = Option<Object>)]
    pub detail: Value,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Counts {
    pub decisions: u64,
    pub events: u64,
}

/// The record of the request's decision `seq`, counted from 1 in the order the router answered it: a hold and the
/// placement after its wait are two decisions of one attempt. Record v2 decoded, with `request_id` and `seq` ahead
/// of it. The record holds what the engine read, in the order it read it, and what it wrote. Every entity is its
/// id, and no credential, token or message content is in it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecisionRecord {
    pub request_id: String,
    pub seq: u32,
    /// 2.
    pub version: u32,
    /// `request_id`, `attempt`, `seq`, `at`, `received_at` (when the relay received the request, the same on every
    /// selection of it, a refusal's `at`), `key_id`, `key_generation`, `pool_id`, `routing_session_id`, `agent_id`,
    /// `parent_agent_id`, `client_class`, `request_class`, `canonical_model`, `lane` (`speed`, `thinking`,
    /// `effort`), `flags` (`fast`, `count`, `probe`, `portable`, `marker`, `context_compacted`), `identity` (`rung`,
    /// `root`, `depth`, `account_id`), `chain` (`static_boundaries` and `boundaries`, the chains' lengths, and
    /// `static_hash`), `attempted` (the accounts already tried), `holds` (each `account_id`, `cohort`, `held_ms`,
    /// `attempt`, `seq` (the decision that held), `ran_out` (the wait's bound ran out with no slot freed), `reason`
    /// (`cap` for a slot, `reset` for a spent window about to reset, `slow_down` for a slowed pin's wait before its
    /// head), `until` (the instant the wait ends: a reset's, or the pin's slow-down deadline, null under `cap`)),
    /// `registry_revision`, `body_tokens` (the relay's estimate of the body's prompt tokens, which a fresh session's
    /// rewrite reads, null on a refusal).
    ///
    /// A refusal's `request` block reads what the refusal site held. A refusal of the router's own carries the
    /// request's `parent_agent_id`, `request_class`, `lane`, `flags` (a keepalive probe's `flags.probe` among
    /// them), `chain`, `attempted` and `holds`. The relay's client-drift refusal, made before any selection, leaves
    /// the seven at their empty values.
    #[schema(value_type = Object)]
    pub request: Value,
    /// The pin as the selection found it: `found` (`live`, `expired_idle`, `left_pool` or `none`), `account_id`,
    /// `epoch`, `incarnation`, `created_at`, `last_used_at`, `last_refresh_at`, `idle_ms`, `refresh_age_ms`,
    /// `lane`, `shed` (`from`, `at`), `marked`, `warmth` (`refresh`, `cold` or `untouched`), `warmth_cause`,
    /// `released` (the reasons a pin was let go), `marked_at`, `marked_on` and `marked_window` (the shedding mark the
    /// row carries: when it was made, the account and the window whose trigger made it, null without one), `cycle_ms`
    /// (the pin's cycle, the count-weighted mean of its last eight gaps between a dispatch and the next arrival, null
    /// before its first gap), `slow_down_until` (the end of the slow-down hold standing on the pin, null with none).
    /// Null for a request that pins nothing, with `pin_reason` beside it.
    #[schema(value_type = Option<Object>)]
    pub pin: Option<Value>,
    /// `not_looked_up` for a refusal, which precedes the pin lookup, or `pinning_off`.
    #[schema(example = "pinning_off")]
    pub pin_reason: Option<String>,
    /// The values that entered the decision, each named by the `PoolRow` field it is read from: `strategy`,
    /// `prefer_subscription`, `overage_policy`, `affinity_ttl_seconds`, `affinity_mode`, `session_placement`,
    /// `min_shared_prefix_tokens`, `subagent_placement`, `window_ceiling`, `load_shedding`, `migrate_when_spent`,
    /// `overage_move_grace_minutes`, `wind_down_minutes`, `return_to_plan`, `return_to_plan_dwell_seconds`,
    /// `shed_cap`, `burn_average_seconds`, `shed_dwell_seconds`, `warm_sessions`, `slow_down`,
    /// `slow_down_stretch_seconds`, `slow_down_hold_seconds`, `in_flight_cap`, `retry_in_place_seconds`,
    /// `pool_updated_at`. Null for a refusal whose key or pool the registry no longer holds.
    #[schema(value_type = Option<Object>)]
    pub settings: Option<Value>,
    /// One per pool member, eligible or not, in the pool's order: `account_id`, `incarnation`, `kind`,
    /// `plan_tier`, `enabled`, `state`, `priority`, `tier` (`[group, priority, overage_bit]`), `in_overage`,
    /// `overage` (`since`, `reset`, `source`, `observed_at`), `in_flight`, `in_flight_cap`, `windows` (by window
    /// name, each `utilization`, `reset`, `source` (`head`, `poll`, `predicted` or `stored`), `observed_at`,
    /// `age_ms`, `ahead`, `projection` and `horizon` (the projection at the window's horizon, a plan window's alone),
    /// `demand` (the 5-hour window's demand of a subscription member, the pins' burn over the account's realized speed
    /// with the live bookings' burn added undivided, in share per hour, null on every other window),
    /// `reserved` (the live bookings' rewrite the prediction carries)), `allowance_known`, `pace` (`remaining`,
    /// `reset`, `end`, `deadline`, `hours`, `score`, `window` (the window the score read, `seven_day` or the model's
    /// week such as `seven_day_fable`, null for an API key member), `burn` (the burn the score subtracts, share per
    /// hour), under Pace alone), `budget` (each `period`, `budget_usd`, `spent_usd`, `period_start`,
    /// `period_end`, `observed_at`), `cooldowns` (each `scope`, `reason`, `window`, `until`, `blocking`),
    /// `credits` (`enabled`, `disabled_reason`, `utilization`, `observed_at`, `blocked`), `term` (`since`,
    /// `trial_ends_at`, `cancelled`, `ends_at`, `winding_down`, `ended`), `prefix` (per rung the deepest entry the
    /// walk visited, `content` and `static`, and apart from it the entry the rung accepted, `content_match` and
    /// `static_match`, each `hash` (64 lowercase hex characters, the boundary as the cache map keys it), `depth`,
    /// `prefix_tokens`, `written_at`, `ttl_ms`, `live`, `refused_by`, the gate that refused it, and beside the
    /// four `with_main`), `obstacle` (`name`, `reason` (a cooldown's or a credits refusal's reason, and for
    /// `budget` the period of the reached budget that ends last, `day` or `month`), `window`, `until`),
    /// `partition`, `rank`, `score`, `verdict` (`chosen`, `renewed`, `held_on`, `source`, `destination`,
    /// `eligible`, `ranked`, `out` or `not_needed`), `runway_ms`, `fit` (`ending`, `fits`, `least_bad` or
    /// `rewrite_only`) and `binding` (the request's fit where a move's gate or the `no_fit` partition read it, the
    /// runway null when unbounded), `burn` (the pins' burn by plan window, share per hour), `factor` (the member's
    /// slow-down factor as the last pinned selection on it read it, null with no read or on a pool whose slow-down is
    /// off).
    #[schema(value_type = Vec<Object>)]
    pub candidates: Vec<Value>,
    /// The steps in the order the engine ran them, each `step`, `verdict` and `facts`, each fact `key`, `value`,
    /// `account_id`, `unit`.
    #[schema(value_type = Vec<Object>)]
    pub walk: Vec<Value>,
    /// `kind`, `account_id`, `from_account_id`, `upstream_model`, `tier`, `strategy`, `rule`, `rung` (`rung`,
    /// `depth`, `prefix_tokens`), `in_overage`, `pin_written` (`action`, `epoch`, `lane`, `refresh_stamped`,
    /// `hashes_stamped`), `pin_events` (each `kind`, `reason`), `hold` (`account_id`, `cohort`, `bound_ms`,
    /// `reason` (`cap`, `reset` or `slow_down`), `until` (the end of a reset's wait, or the pin's slow-down deadline),
    /// and under `slow_down` the read the hold was sized from, null otherwise: `factor`, `speed` (the speed the
    /// account's holds realize together), `stretch_ms`, `threshold_ms` and `cycle_ms` (the pin's cycle)),
    /// `exhaustion` (`kind`, `deadline`, `window`, `reason`, `obstacle`), `refusal` (`kind`, `code`, `reason`),
    /// `cohort_hold` (`waited_on`, `cohort`, `held_ms` (every wait summed), `waits` (each `reason`, `cap`, `reset` or
    /// `slow_down`, and its `held_ms`)), `shed_marks`, `mark_cleared` (`reason`, `marked_at`, `marked_on`,
    /// `marked_window`, `projection`, `stretch_ms` and `threshold_ms` (the stretch and the threshold the heal read
    /// where the stretch made the mark)), `reservation` (`account_id`, `rewrite` and `burn` by plan window, the burn
    /// per hour, `expires_at`, null for a decision that booked nothing), `cause` (why the failover this decision lands
    /// moved the session: a cooldown's `scope`, `reason` and `window`, or a mark's `state` and `cause`, null for every
    /// other decision), `completion`.
    ///
    /// A refusal's `code` is its structured word beside the display `reason`: `disabled`, `expired`,
    /// `pool_disabled`, `pool_missing`, `replaced`, `revoked` or `unknown` for a key refusal, `model_not_allowed`
    /// for a key or pool that no longer admits the request's model, and `drift` for the relay's refusal of a
    /// drifted client.
    ///
    /// `completion` is null for a decision that dispatched nothing (a hold, an exhaustion, a refusal). For a
    /// placement, a renewal or a move it reads the attempt that answered for the decision: the newest attempt on
    /// the decision's account at or after the decision's own and before the request's next decision, so a retry
    /// in place reads the pass that answered, and a decision the request moved past on the same attempt (a
    /// credential failure that fails over before dispatching) reads no answer, its fields null. It carries
    /// `status`, `cache_read_tokens` and `input_tokens` (null until the attempt has usage), `promised_tokens` (the
    /// matched rung's prefix, null without a rung), `hit` (as `GET /admin/analytics/placements` counts it: a
    /// `content` rung reads up to its promise, any other decision reads anything, null until the attempt has
    /// usage), `held_ms` (every wait of the request, for a slot, a reset or the slow-down, summed, null for a request
    /// never held), `freed` (whether the request waited on the account this decision placed on, so its wait ended in
    /// that slot), `cache_map_written` (the cache-map entries the answer wrote, each `tier`, `depth`,
    /// `prefix_tokens`, `ttl_ms`, `account_id`, null until the answer ends), and `completed_at`.
    #[schema(value_type = Object)]
    pub outcome: Value,
}

/// `GET /admin/routing-log`'s query. Any other key is refused.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RoutingLogQuery {
    /// A comma list of ids. Matches a move's source as well as the account a decision placed on.
    pub accounts: Option<String>,
    /// The `next_cursor` of the page before, `<at>:<rank>:<id>`.
    pub cursor: Option<String>,
    /// An RFC 3339 instant such as `2026-09-16T14:15:00.000Z` or Unix milliseconds, a value that reads as an
    /// integer taken as milliseconds. Defaults to an hour before now. Inclusive.
    pub from: Option<String>,
    /// A comma list of ids.
    pub keys: Option<String>,
    /// A comma list of decision kinds, `exhausted`, `held`, `moved`, `placed`, `refused`, `renewed`, the
    /// pseudo-kind `probe` for a keepalive probe's decision of any kind, and event kinds, the `kind` list of
    /// `GET /admin/requests/{id}`. Absent, the set is every decision and event kind. Present, it names exactly the
    /// kinds listed, so `kinds=probe` lists the probes alone, and an empty `kinds=` reads as absent.
    pub kinds: Option<String>,
    /// 1 to 200, default 25.
    pub limit: Option<u32>,
    /// A comma list of ids.
    pub models: Option<String>,
    /// A comma list of ids.
    pub pools: Option<String>,
    /// One request id: its decisions and the events that name it.
    pub request: Option<String>,
    /// A session id or its prefix, matched literally: `_` and `%` are characters.
    pub session: Option<String>,
    /// As `from`. Defaults to now. Inclusive.
    pub to: Option<String>,
}

/// Read a page of decisions and events.
#[utoipa::path(
    get,
    path = "/admin/routing-log",
    operation_id = "routing_log_list",
    tag = "Routing log",
    params(RoutingLogQuery),
    responses(
        (status = 200, description = "The page.", body = RoutingLog),
        (status = 400, description = "- `unknown filter {name}`\n- `unknown kind {name}`\n- `{name} is not a time`, for `from` or `to`\n- `limit is 1 to 200`\n- `cursor is <at>:<rank>:<id>`", body = ErrorBody, example = json!({"error": "unknown filter {name}"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn list(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut names: Vec<&String> = q.keys().collect();
    names.sort();
    if let Some(unknown) = names
        .into_iter()
        .find(|name| !KEYS.contains(&name.as_str()))
    {
        return failure(
            StatusCode::BAD_REQUEST,
            &format!("unknown filter {unknown}"),
        );
    }
    let now = (state.clock)();
    let (from, to) = match (time(&q, "from"), time(&q, "to")) {
        (Ok(from), Ok(to)) => (from.unwrap_or(now - HOUR_MS), to.unwrap_or(now)),
        (Err(error), _) | (_, Err(error)) => return failure(StatusCode::BAD_REQUEST, &error),
    };
    // An emptied list reads as an absent one, every kind, as the emptied fields below read as absent.
    let kinds = Some(list_param(&q, "kinds")).filter(|kinds| !kinds.is_empty());
    if let Some(unknown) = kinds.iter().flatten().find(|kind| {
        let kind = kind.as_str();
        kind != PROBE_KIND && !DECISION_KINDS.contains(&kind) && !EVENT_KINDS.contains(&kind)
    }) {
        return failure(StatusCode::BAD_REQUEST, &format!("unknown kind {unknown}"));
    }
    let limit = match q.get("limit").map(|raw| raw.parse::<usize>()) {
        None => DEFAULT_LIMIT,
        Some(Ok(n)) if (1..=MAX_LIMIT).contains(&n) => n,
        Some(_) => return failure(StatusCode::BAD_REQUEST, "limit is 1 to 200"),
    };
    let cursor = match q.get("cursor").map(|raw| RoutingLogCursor::parse(raw)) {
        None => None,
        Some(Some(cursor)) => Some(cursor),
        Some(None) => return failure(StatusCode::BAD_REQUEST, "cursor is <at>:<rank>:<id>"),
    };
    let filter = RoutingLogFilter {
        from,
        to,
        // An emptied field in the view's bar narrows nothing, as an absent one.
        request: q.get("request").filter(|r| !r.is_empty()).cloned(),
        session: q.get("session").filter(|s| !s.is_empty()).cloned(),
        keys: list_param(&q, "keys"),
        pools: list_param(&q, "pools"),
        accounts: list_param(&q, "accounts"),
        models: list_param(&q, "models"),
        kinds,
    };
    let page = state
        .ledger
        .db()
        .read(move |c| list_routing_log(c, &filter, cursor.as_ref(), limit))
        .await;
    let page = match page {
        Ok(page) => page,
        Err(e) => return ledger_failure(e),
    };
    Json(RoutingLog {
        rows: page.rows.iter().map(row_json).collect(),
        next_cursor: page.next_cursor,
        counts: page
            .counts
            .map(|(decisions, events)| Counts { decisions, events }),
    })
    .into_response()
}

fn row_json(row: &RoutingLogRow) -> Value {
    match row {
        RoutingLogRow::Decision(d) => decision_json(d),
        RoutingLogRow::Event(e) => {
            serde_json::to_value(event_json(e)).expect("an event row serializes")
        }
    }
}

/// The summary repeats the `kind` and `probe` columns. The columns' values stand, so no field is answered twice.
fn decision_json(d: &DecisionListRow) -> Value {
    let mut row = Map::new();
    row.insert("type".into(), "decision".into());
    let columns = json!({
        "request_id": d.request_id, "seq": d.seq, "attempt": d.attempt, "attempt_count": d.attempt_count,
        "at": d.at, "kind": d.kind, "probe": d.probe, "key_id": d.key_id, "pool_id": d.pool_id,
        "session_id": d.session_id, "agent_id": d.agent_id, "model": d.model, "account_id": d.account_id,
        "from_account_id": d.from_account_id,
    });
    if let Value::Object(columns) = columns {
        row.extend(columns);
    }
    if let Value::Object(summary) = &d.summary {
        for (name, value) in summary {
            row.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    row.insert("cache".into(), json!(d.cache));
    row.insert("status".into(), json!(d.status));
    Value::Object(row)
}

fn event_json(e: &EventListRow) -> EventRow {
    let mut detail = e.detail.clone();
    decode_diff(&mut detail);
    EventRow {
        kind_of_row: "event".into(),
        id: e.id,
        at: e.at,
        kind: e.kind.clone(),
        account_id: e.account_id.clone(),
        key_id: e.key_id.clone(),
        pool_id: e.pool_id.clone(),
        session_id: e.session_id.clone(),
        agent_id: e.agent_id.clone(),
        model: e.model.clone(),
        request_id: e.request_id.clone(),
        actor: e.actor.clone(),
        detail,
    }
}

/// Read one decision's record.
#[utoipa::path(
    get,
    path = "/admin/requests/{id}/decisions/{seq}",
    operation_id = "request_decision",
    tag = "Requests",
    params(
        ("id" = String, Path, description = "The request's id"),
        ("seq" = String, Path, description = "The decision's sequence number, counted from 1"),
    ),
    responses(
        (status = 200, description = "The decision's record.", body = DecisionRecord),
        (status = 400, description = "`seq is the decision's sequence number`, for a `seq` that is not a whole number", body = ErrorBody, example = json!({"error": "seq is the decision's sequence number"})),
        (status = 404, description = "`no decision {seq} for request {id}`, for a decision the retention purged and one never written alike", body = ErrorBody, example = json!({"error": "no decision {seq} for request {id}"})),
        (status = 500, response = LedgerFailure),
    ),
)]
// Purged and never written answer the same 404: the row is the only witness either way. A `seq` that is no
// sequence number answers 400, since no decision could ever be written under it.
pub(crate) async fn record(
    State(state): State<Arc<AdminState>>,
    Path((id, seq)): Path<(String, String)>,
) -> Response {
    let missing = || {
        failure(
            StatusCode::NOT_FOUND,
            &format!("no decision {seq} for request {id}"),
        )
    };
    if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return failure(
            StatusCode::BAD_REQUEST,
            "seq is the decision's sequence number",
        );
    }
    // Past `u32` it is still a sequence number, one no request reaches.
    let Ok(n) = seq.parse::<u32>() else {
        return missing();
    };
    let request_id = id.clone();
    match state
        .ledger
        .db()
        .read(move |c| decision_record(c, &request_id, n))
        .await
    {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => missing(),
        Err(e) => ledger_failure(e),
    }
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::{fixture, round_trips};

    use super::{DecisionRecord, Row};

    #[test]
    fn the_routing_log_fixtures_round_trip_through_their_declared_types() {
        round_trips::<DecisionRecord>("decision");
        let mut decisions = 0;
        for row in fixture("routing_log")["rows"].as_array().unwrap() {
            let typed: Row = serde_json::from_value(row.clone())
                .unwrap_or_else(|e| panic!("{row} does not fit a declared row: {e}"));
            assert_eq!(serde_json::to_value(&typed).unwrap(), *row);
            assert!(matches!(
                (&typed, row["type"].as_str()),
                (Row::Decision(_), Some("decision")) | (Row::Event(_), Some("event"))
            ));
            decisions += matches!(typed, Row::Decision(_)) as usize;
        }
        assert_eq!(
            decisions, 14,
            "the fixture's 14 decision rows round-trip through DecisionRow and its summary mirrors"
        );
    }
}
