//! The request log: one row per client request with its attempt trail, the detail with its events, and the
//! captured bodies.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_bus::Classifier;
use phoebis_ledger::{
    entrypoint_of_client, free_equivalent, free_shares, funding_of, list_request_page,
    price_equivalent, price_shares, request_by_id, request_events, AttemptRecord, Funding,
    LiveKeys, PlanTables, PriceRevisions, PricedShares, RequestFilter, RequestRecord, Speed,
    SHARE_REASON_CLIENT_CLASSIFIER,
};
use phoebis_routing::PinUsage;
use serde_json::Value;
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{ErrorBody, LedgerFailure};
use crate::routes::failure;
use crate::AdminState;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// What a request's figures are priced with, read once per handler call: the cost model, whose revision in force at
/// the request's receipt prices the serving attempt's shares under the account tier that attempt recorded and its 20x
/// equivalent, and the API price list, whose revision in force at the receipt prices its attempts' dollars.
pub(crate) struct ShareContext {
    pub tables: Arc<PlanTables>,
    pub prices: Arc<PriceRevisions>,
}

impl ShareContext {
    pub(crate) fn read(state: &AdminState) -> ShareContext {
        ShareContext::of(state.tables.clone(), state.pricing.revisions())
    }

    /// The tables and prices given: a test's own, or the process's shared ones.
    pub(crate) fn of(
        tables: impl Into<Arc<PlanTables>>,
        prices: impl Into<Arc<PriceRevisions>>,
    ) -> ShareContext {
        ShareContext {
            tables: tables.into(),
            prices: prices.into(),
        }
    }
}

/// One page of the request log.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestList {
    pub requests: Vec<RequestSummary>,
    /// Set only while a row lies beyond the page. A page of exactly `limit` rows with none beyond answers null.
    pub next_cursor: Option<String>,
    /// The newest write instant among the rows the filters admit at this read, `to` included, the `landed_after` passed
    /// where none above it has landed, and 0 where neither is set, never past a `to` the query names unless
    /// `fixed_end` is set, since a row received after `to` lands after it and a fixed range's poll reads a late row
    /// once. Pass it as `landed_after` to read what landed since.
    pub landed_through: i64,
}

/// One client request with its attempt trail. `request_truncated` and `response_truncated` say whether the captured
/// body is shorter than what the relay saw. `agent_type`, `compaction` and `context_compacted` are the gateway hint
/// headers as the client sent them, null when it sent none.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestSummary {
    pub id: String,
    pub at: i64,
    pub finished_at: Option<i64>,
    pub key_id: Option<String>,
    /// The key's `created_at`, `0` for a request no key was read for.
    pub key_generation: i64,
    pub pool_id: Option<String>,
    pub account_id: Option<String>,
    /// The client's own.
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub model: String,
    pub canonical_model: String,
    pub kind: String,
    pub origin: String,
    /// `sse` or `json`.
    #[schema(example = "sse")]
    pub transport: String,
    /// The class the client named on `x-claude-code-request-class`, null when it sent none.
    pub request_class: Option<String>,
    /// `server` for a request that asked for a server-side auto mode check under an on setting and `stripped`
    /// for one that carried the `safeguards` member under an off setting, which the dispatch drops, both as
    /// admission read them whether or not the request reached an upstream. `body_shape` for one refused with
    /// that error code before it reached an upstream, whatever it asked for. Null otherwise.
    #[schema(example = "server")]
    pub auto_mode: Option<String>,
    /// `client` for a request the class header `auxiliary` and the beta `auto-mode-classifier-2026-07-16` mark as
    /// the client's own auto-mode classifier call, which takes no share of a plan window. Null otherwise.
    #[schema(example = "client")]
    pub classifier: Option<String>,
    /// A request that continued another conversation at depth 2 or deeper: that conversation's root.
    /// Null otherwise and always null on an auxiliary request.
    pub forked_from: Option<String>,
    /// The matched depth of `forked_from`, null with it.
    pub fork_depth: Option<u32>,
    pub status: Option<u16>,
    /// `canceled`, `exhausted`, `failed`, `incomplete`, `rejected`, `succeeded`.
    #[schema(example = "canceled")]
    pub outcome: String,
    /// Under `incomplete` one of `stream_cut`, `stream_incomplete`, `stream_stalled`, `stream_undecodable`: the
    /// body ended before its end. A cut, a stall or a body that did not decode on a non-200 answer sits under
    /// `failed`.
    #[schema(example = "stream_cut")]
    pub error_code: Option<String>,
    pub client_drift: Option<String>,
    /// The item a `client_contract` refusal named: `client_version`, `session_header`, `class_header`,
    /// `attribution_line`, `metadata_user_id`, `agent_id_header` or `compaction_header`. Null for every other request.
    #[schema(example = "class_header")]
    pub client_contract: Option<String>,
    pub ttfb_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub kept_warm: bool,
    pub captured_bodies: bool,
    pub request_truncated: bool,
    pub response_truncated: bool,
    pub user_agent: Option<String>,
    /// The class the router derived at admission: `claude-code/<entrypoint>` from a Claude Code user agent, else
    /// `other`. Null on a row from before the column existed.
    pub client: Option<String>,
    pub agent_type: Option<String>,
    pub compaction: Option<String>,
    pub context_compacted: Option<String>,
    /// The sum of `x-claude-code-prev-tool-durations` in milliseconds.
    pub prev_tool_ms: Option<i64>,
    /// The `cc_turn_origin` of the attribution line, first-party only.
    pub turn_origin: Option<String>,
    pub shape: RequestShape,
    pub usage: RequestUsage,
    pub cost_usd: f64,
    /// Null for a request billed in dollars: an overage attempt, a fast answer, an API key account, a usage-based seat
    /// or no attempt.
    /// Else an object keyed by the account's window ids, `five_hour`, `seven_day` and the model's own week where it
    /// has one, each the serving attempt's share of that window under the account tier the attempt recorded, priced
    /// at read time on the cost revision in force at the request's receipt with the peak band read at the attempt's
    /// finish, or null where the account tier has no capacity on that window or neither the model nor its model tier
    /// has a weight there. An empty object for an attempt that recorded no account tier. A free call under
    /// `share_reason` lists the windows its account tier sizes alone, each at zero.
    pub cost_shares: Option<BTreeMap<String, Option<f64>>>,
    /// Null for a request billed in dollars, as `cost_shares`. Else the serving attempt's usage as the share of a
    /// Max 20x account's windows under the same keys, at the same revision and band, whatever account tier served it.
    pub cost_equivalent: Option<BTreeMap<String, Option<f64>>>,
    /// `client_classifier` where `cost_shares` and `cost_equivalent` read zero by rule, for the client's own
    /// classifier call under a subscription window. Null for a figure priced from the request's counts.
    pub share_reason: Option<String>,
    /// Whether `cost_shares` or `cost_equivalent` priced a window at the weight of the model tier's latest weighted
    /// model, for a model the cost model does not weight.
    pub estimated: bool,
    /// The serving attempt's account tier the router priced by when the request landed, null as on the attempt.
    pub tier: Option<String>,
    pub phases: RequestPhases,
    pub attempts: Vec<Attempt>,
    /// `{waited_on, cohort, held_ms, waits}` for the holds the request waited through, whether or not an attempt
    /// followed, `held_ms` every wait summed in milliseconds and `waits` each wait apart, oldest first, with its
    /// `reason` (`cap` for a slot, `reset` for a spent window about to reset, `slow_down` for a slowed pin's wait
    /// before its head, `rewrite` for a moved pin's wait for the head of the request that carried the move) and its
    /// `held_ms`. A row from before the waits were kept carries no `waits`. Null for a request never held.
    #[schema(value_type = Option<Object>)]
    pub held: Option<Value>,
    /// `client` when the client's body carried its own `diagnostics` member, `router` when the relay inserted it,
    /// null for a body that went without one and on a row from before the column existed.
    #[schema(example = "client")]
    pub diagnostics_source: Option<String>,
    /// The guardrail that blocked the request, null where none did.
    pub guardrail_id: Option<String>,
    /// Every guardrail that matched, in evaluation order, observed or blocking.
    pub guardrails: Vec<GuardrailMatchView>,
    /// True where the router refused the request before any account was asked, for the client's or a policy's cause.
    /// The figures leave such a request out, the log lists it.
    pub refused: bool,
}

/// A guardrail that matched a request: its id, its place in the evaluation order and whether it blocked.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailMatchView {
    pub id: String,
    pub position: u32,
    pub blocked: bool,
}

/// Each key as the request sent it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestShape {
    pub context_edits: Vec<String>,
    pub effort: Option<String>,
    pub max_tokens: Option<u64>,
    pub message_count: u32,
    pub requested_service_tier: Option<String>,
    pub requested_speed: Option<String>,
    pub system_blocks: u32,
    pub temperature: Option<f64>,
    pub thinking_budget: Option<u64>,
    pub thinking_type: Option<String>,
    pub tool_count: u32,
}

/// The serving attempt's usage, zero where it has none.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestUsage {
    pub cache_read: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub input: u64,
    pub output: u64,
    pub thinking: u64,
    pub web_fetch: u64,
    pub web_search: u64,
}

/// Each Unix milliseconds, null for an instant the ledger holds none of.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestPhases {
    pub received_at: i64,
    /// The first attempt's start.
    pub selected_at: Option<i64>,
    /// When the relay flushed the body to the serving attempt's upstream.
    pub written_at: Option<i64>,
    pub head_at: Option<i64>,
    pub first_content_at: Option<i64>,
    /// When the ledger read the answer's `message_stop`, never after `finished_at`.
    pub stream_end_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// One attempt of the trail.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attempt {
    pub index: u32,
    pub account_id: String,
    pub account_kind: String,
    pub outcome: String,
    pub upstream_status: Option<u16>,
    pub upstream_request_id: Option<String>,
    /// The `id` of the message the answer carried, null without one: the join to a Claude Code transcript's
    /// assistant entry.
    pub upstream_message_id: Option<String>,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub ttfb_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub cache_miss_reason: Option<String>,
    pub service_tier: Option<String>,
    pub client_drift: Option<String>,
    pub cooldown_set: Option<CooldownSet>,
    /// The head's `5h`, `7d` and `7d_oi`, answered under the usage endpoint's keys as the quota samples are, each
    /// present only when the head carried it.
    pub windows: Vec<AttemptWindow>,
    pub overage_in_use: Option<bool>,
    /// The serving account's class at the attempt, false on a row from before the router recorded it.
    pub usage_based: bool,
    /// The attempt's dollars at the cells its own prompt selects on the API price revision in force at its request's
    /// receipt, priced when it is read, null for a model the revision lacks and for an attempt without usage.
    pub estimated_usd: Option<f64>,
    /// Null, or the trace behind the attempt's placement: `strategy`, `tier` (the three-element array `[group,
    /// priority, overage_bit]` the choice was made in, null for a renewed pin), `renewed`, `moved` (the reason a
    /// fail-over or a move landed it here), `skipped` (array of `account_id`, `obstacle`, every candidate the
    /// choice passed over), `placement` (null, or `rung` (`content`, `static`, or `with_main`), `depth`, and
    /// `prefix_tokens` for a fresh placement with no pin of its own). The completion adds `cache_read`, the tokens the
    /// answer read from the cache.
    #[schema(value_type = Option<Object>)]
    pub selection: Option<Value>,
    pub response: AttemptResponse,
    /// The serving account's rate-limit tier the router priced by when the request landed, null for an api_key
    /// account, for an oauth account whose profile had not been read, for a plan the router does not price (a free
    /// organization, a usage-based seat), and for an attempt recorded before the router kept it.
    pub tier: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CooldownSet {
    pub reason: String,
    pub until: Option<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptWindow {
    /// `five_hour`, `seven_day` or `seven_day_overage_included`.
    #[schema(example = "five_hour")]
    pub id: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
}

/// Each key as the answer said it. `served_model` only when it differs from the resolved model.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptResponse {
    pub cleared_input_tokens: Option<u64>,
    pub inference_geo: Option<String>,
    pub refusal_category: Option<String>,
    pub refusal_recommended_model: Option<String>,
    pub served_model: Option<String>,
    pub speed: Option<String>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

/// The newest `stream_interrupted` event of the request, lifted so a reader takes one member rather than the list.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StreamCut {
    pub at: i64,
    /// One of `account_canceled`, `stream_cut`, `stream_stalled` and `stream_undecodable`.
    #[schema(example = "stream_cut")]
    pub code: String,
    pub text: String,
    pub bytes_relayed: u64,
    pub upstream_request_id: Option<String>,
    pub attempt: u32,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestDetail {
    /// The same shape as one row of `GET /admin/requests`.
    pub request: RequestSummary,
    pub events: Vec<Event>,
    /// Null on a request without such a row.
    pub stream_cut: Option<StreamCut>,
}

/// One event the request names, the last three keys the pool the event is about, the agent the pin is held for,
/// and the request the event names, each null where the kind has none. Rows from before read null.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub at: i64,
    /// One of:
    ///
    /// - `account_state_changed`: an account moved to another state, with `from`, `to`, the `cause` (the
    ///   registry's word for who moved it: `auth_failed`, `deleted`, `funding_refused`, `funding_retry`,
    ///   `operator`, `paired` or `refresh_failed`) and the upstream's `detail` where the answer carried one. A
    ///   move to `reauth_required` is one such row, its `to` reading `reauth_required`, and a deleted account's
    ///   last row reads `to` and `cause` `deleted`.
    /// - `budget_period_ended`: a member whose budget was reached entered its next period, with
    ///   `previous_spent_usd`.
    /// - `budget_reached`: a pool member's spend reached its daily or monthly budget, with the request that
    ///   crossed it when the ledger's spend read followed one. A restart does not announce a crossing its period
    ///   already holds.
    /// - `catalog_changed`: an account's model listing differs from the one before, with the ids `added` and
    ///   `removed` and the new `count`.
    /// - `cooldown_cleared`: a cooldown left its account, with `cause` `admin`, `expired` or `recovered`.
    /// - `cooldown_set`: an upstream rejection cooled an account or one of its models.
    /// - `fast_credits_changed`: a credits reading changed whether fast mode can draw on the account's extra
    ///   usage, or why it cannot.
    /// - `ledger_gap`: a request whose record never reached the ledger, with the reason.
    /// - `model_permitted`: the account that served a model another account had rejected for the same request.
    /// - `model_rejected`: an account refused the model the request named.
    /// - `overage_ended`: the account left overage, with `since`, when it entered.
    /// - `overage_started`: the account entered overage.
    /// - `pin_created`: a session's first placement on an account.
    /// - `pin_expired`: a pin left its account, with the facts that released it.
    /// - `pin_migrated`: a pin moved to another account, with the move's reason.
    /// - `pin_released`: an operator released the session.
    /// - `plan_ended`: a profile read found the account's organization holding no plan, which flags the account
    ///   plan ended, with the read's `source` (`profile`, or `usage_403` where a usage 403 naming the organization's
    ///   refusal made the read due) and the profile's `organization_type`, `rate_limit_tier` and
    ///   `subscription_status`.
    /// - `plan_restored`: the account's plan-ended flag cleared, with its `source` (`profile` for a profile read that
    ///   found a plan, `pairing` for a pairing confirm) and the profile's `organization_type`, `rate_limit_tier` and
    ///   `subscription_status`.
    /// - `poll_backoff_ended`: the usage poll's backoff on an account ended, with `duration_ms` and `cause`,
    ///   `clean_poll` where a clean answer ended it or `plan_ended` where the account's plan-ended pause closed it.
    /// - `poll_backoff_started`: the usage or the profile endpoint (`endpoint`) rate-limited an account, starting
    ///   the usage poll's backoff, with `next_attempt_at` and `last_clean_poll_at`, when the last clean answer
    ///   before it was read.
    /// - `poll_failed`: the usage poll's read of the usage or the profile endpoint (`endpoint`) answered a status
    ///   other than 200 and 429 or failed in transport, once per poll, with the `status`, null on a transport
    ///   failure, the `error` text the poll recorded and `transport`. A usage 401 writes none, since it moves the
    ///   account to `reauth_required`, and a usage 403 naming the organization's refusal whose profile reports no plan
    ///   writes `plan_ended` instead.
    /// - `refresh_failed`: an OAuth token refresh failed, with the `error` the refresh recorded and its `cause`:
    ///   the token endpoint's error code (`invalid_grant`), else the status it answered (`503`), else the words
    ///   before the colon of an error that carries its detail after one (`transport`), else the error's own words
    ///   (`identity_mismatch`).
    /// - `refresh_succeeded`: an OAuth token refresh committed, with `access_expires_at`, when the new access
    ///   token expires (also `until`), and `refresh_expires_at`, the refresh token's expiry or null. All three
    ///   read null when a later refresh, a re-pairing or a disconnect replaced the committed credential before the
    ///   row was written.
    /// - `refresh_uncertain`: an OAuth token refresh that may or may not have rotated the token, with its `error`
    ///   and `cause` as on a failed one.
    /// - `registry_changed`: an operator's edit to the registry, with its `kind`, `target` and `diff`: the row's
    ///   snapshot for a creation, the `{field: {from, to}}` pairs of the changed fields for an account, a key or a pool
    ///   update, the same pairs for a guardrail update with its `pools` and `keys` lists among the fields, `{}` for a
    ///   deletion or a revocation, and the member list for `pool_members_set`. An account update's pairs carry
    ///   `"secret_replaced": true` beside them where the edit staged a secret, whose value the diff never holds, and
    ///   `key_rotated` carries the new secret's `prefix` and `previous_secret_until`, the instant the retired secret
    ///   stops being accepted.
    /// - `router_started`: the router started, with what it loaded, `down_since`, the newest row the ledger held
    ///   before it, and `holds_lost`, since no held request survives a restart. A clock reset or a wind-down the
    ///   previous run announced before that row is not written again.
    /// - `shedding_round`: a pool's shedding round ran on one of its member accounts, with the window that fired it and
    ///   what fired it (the projection over the ceiling after its wait, or the slow-down's stretch over its threshold),
    ///   the excess and the target, the pool's pins on that account in the round's order with their burn, their last
    ///   prompt, their rewrite as the Max 20x equivalent of the pin's weekly window with that window named, their
    ///   planned destination and their mark, then the pins it left out, each pin's skip word among `fast_lane`,
    ///   `last_pin`, `no_runway`, `off_window`, `standing` and `unpriced` and its note among `cheapest` and `current`,
    ///   and the outcome with its reason among `last_pin`, `no_runway`, `off_window`, `standing` and `unpriced`. The
    ///   marks and the dwell are the pool's on that account.
    /// - `slow_down_ended`: the account's slow-down on the pool ended, its factor back at 1 for the safety wait or
    ///   its entry dropped at the purge, with the span's `since`, `duration_ms` (to the factor's return, or to the
    ///   entry's last read where it never returned), `factor_min`, the lowest factor of the span, and `held_ms`, the
    ///   time its new holds waited.
    /// - `slow_down_started`: the account's factor on the pool read under 1 for the first time since it last read 1,
    ///   with the read's `factor`, `stretch_ms`, `threshold_ms`, `demand` (share of the 5-hour window per hour) and
    ///   the window's `reset`.
    /// - `stream_interrupted`: the request's body ended short after the head, with the `code` its `error_code` carries
    ///   (`account_canceled`, `stream_cut`, `stream_stalled` or `stream_undecodable`), the error's own words as
    ///   `text`, `bytes_relayed`, the bytes the client received before the end, the head's `upstream_request_id`, null
    ///   where the head carried none, and the `attempt`.
    /// - `warm_prefix_expired`: the cache-map entries the purge removed for one conversation root on one account,
    ///   with the largest of them and the cause, `ttl` or `incarnation`.
    /// - `wind_down_started`: a pool member entered its wind-down before its subscription ends, `canceled` or
    ///   `trial`.
    /// - `window_cleared`: a plan window read under 1.0 after a reading at 1.0 or a predicted spent edge under the
    ///   same reset, with the figure before, the reading's or the prediction's, the reading after and the unchanged
    ///   reset, so the history no longer keeps the window spent.
    /// - `window_reset`: a plan window's reset passed, seen by a sample naming a later reset, or naming none once
    ///   read past it (`sample`), or by the clock (`clock`), once per reset, with the `plan` and the `tier` of the
    ///   reading whose reset passed, each null where that reading names none.
    /// - `window_spent`: a plan window's reading reached 1.0 from under it, with the head's request when a head
    ///   read it. A window first seen at 1.0, with no stored reading before it, reads as spent.
    #[schema(example = "account_state_changed")]
    pub kind: String,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub actor: Option<String>,
    /// The kind's detail, decoded. A `pin_migrated` event's carries `agent_id`, `from_account`, `epoch`, and
    /// `reason` (`cooldown`, `drain`, `fast_lane`, `mark`, `overage`, `return_to_plan`, `shed`, or `spent`), a
    /// `pin_expired` event's `agent_id` and `reasons`, the facts the selection found against the released pin's
    /// account (the documentation site's routing page describes them under Moves), and a `pin_released` event's
    /// `agent_id`.
    #[schema(value_type = Option<Object>)]
    pub detail: Value,
    pub pool_id: Option<String>,
    pub agent_id: Option<String>,
    pub request_id: Option<String>,
}

/// A side whose bytes parse as JSON answers as that value, and any other side, an SSE stream or a capture cut short
/// among them, as a string. The four byte counts are the bytes each side stored and the bytes it ran to, and the
/// difference is what was dropped. `request_truncated` and `response_truncated` are the request's own flags, as
/// `GET /admin/requests` answers them, so a capture whose last chunks never reached the ledger reads truncated even
/// where the two byte counts agree.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bodies {
    pub request_id: String,
    /// JSON or text.
    pub request: Value,
    /// JSON or text, nullable.
    pub response: Option<Value>,
    pub request_bytes_stored: u64,
    pub request_bytes_seen: u64,
    pub response_bytes_stored: u64,
    pub response_bytes_seen: u64,
    pub request_truncated: bool,
    pub response_truncated: bool,
    pub expires_at: i64,
}

pub(crate) fn request_summary(
    r: &RequestRecord,
    attempts: &[AttemptRecord],
    shares: &ShareContext,
) -> RequestSummary {
    let last = attempts.last();
    let plan = plan_figures(r, attempts, &shares.tables);
    RequestSummary {
        id: r.id.clone(),
        at: r.received_at,
        finished_at: r.finished_at,
        key_id: r.key_id.clone(),
        key_generation: r.key_generation,
        pool_id: r.pool_id.clone(),
        account_id: last.map(|a| a.account_id.clone()),
        session_id: r.session_id.clone(),
        agent_id: r.agent_id.clone(),
        parent_agent_id: r.parent_agent_id.clone(),
        model: r.model.clone(),
        canonical_model: r.canonical_model.clone(),
        kind: r.kind.clone(),
        origin: r.origin.clone(),
        transport: if r.stream { "sse" } else { "json" }.to_string(),
        request_class: r.request_class.clone(),
        auto_mode: r.auto_mode.clone(),
        classifier: r.classifier.clone(),
        forked_from: r.forked_from.clone(),
        fork_depth: r.fork_depth,
        status: r.status,
        outcome: r.outcome.clone(),
        error_code: r.error_code.clone(),
        client_drift: r.client_drift.clone(),
        client_contract: r.client_contract.clone(),
        ttfb_ms: r.ttfb_ms,
        ttft_ms: r.ttfc_ms,
        duration_ms: r.duration_ms,
        request_bytes: r.request_bytes,
        response_bytes: r.response_bytes,
        kept_warm: r.kept_warm,
        captured_bodies: r.bodies_captured,
        request_truncated: r.request_truncated,
        response_truncated: r.response_truncated,
        user_agent: r.user_agent.clone(),
        client: r.client.clone(),
        agent_type: r.agent_type.clone(),
        compaction: r.compaction.clone(),
        context_compacted: r.context_compacted.clone(),
        prev_tool_ms: r.prev_tool_ms,
        turn_origin: r.turn_origin.clone(),
        shape: shape_json(r),
        usage: usage_json(last),
        // Folded onto a positive zero rather than summed: `Sum<f64>` starts at -0.0, so a request with no
        // priced attempt would put a signed zero on the wire and the UI would read "-$0.00".
        cost_usd: attempts
            .iter()
            .filter_map(|a| attempt_usd(a, r, &shares.prices))
            .fold(0.0, |a, b| a + b),
        cost_shares: plan.as_ref().map(cost_shares_json),
        cost_equivalent: plan.as_ref().map(|p| windows_json(&p.equivalent)),
        share_reason: plan
            .as_ref()
            .and_then(|p| p.share_reason.map(str::to_string)),
        estimated: plan.as_ref().is_some_and(|p| {
            p.equivalent.any_estimated()
                || p.shares.as_ref().is_some_and(PricedShares::any_estimated)
        }),
        tier: last.and_then(|a| a.tier.clone()),
        phases: phases_json(r, attempts),
        attempts: attempts
            .iter()
            .map(|a| attempt_json(a, r, &shares.prices))
            .collect(),
        // The request row's own facts rather than an attempt's: a hold that ended in an exhaustion has no attempt.
        held: r
            .held
            .as_deref()
            .and_then(|h| serde_json::from_str::<Value>(h).ok()),
        diagnostics_source: r.diagnostics_source.clone(),
        guardrail_id: r.guardrail_id.clone(),
        guardrails: r
            .guardrails
            .iter()
            .map(|m| GuardrailMatchView {
                id: m.guardrail_id.clone(),
                position: m.position,
                blocked: m.blocked,
            })
            .collect(),
        refused: r.refused,
    }
}

fn shape_json(r: &RequestRecord) -> RequestShape {
    RequestShape {
        context_edits: r
            .context_edits
            .split(',')
            .filter(|e| !e.is_empty())
            .map(String::from)
            .collect(),
        effort: r.effort.clone(),
        max_tokens: r.max_tokens,
        message_count: r.message_count,
        requested_service_tier: r.requested_service_tier.clone(),
        requested_speed: r.requested_speed.clone(),
        system_blocks: r.system_blocks,
        temperature: r.temperature,
        thinking_budget: r.thinking_budget,
        thinking_type: r.thinking_type.clone(),
        tool_count: r.tool_count,
    }
}

/// The write is the serving attempt's: an earlier attempt's write led nowhere the answer came from.
fn phases_json(r: &RequestRecord, attempts: &[AttemptRecord]) -> RequestPhases {
    let last = attempts.last();
    RequestPhases {
        received_at: r.received_at,
        selected_at: attempts.first().map(|a| a.started_at),
        written_at: last.and_then(|a| a.written_at),
        head_at: r.ttfb_ms.map(|ms| r.received_at + ms),
        first_content_at: r.ttfc_ms.map(|ms| r.received_at + ms),
        stream_end_at: r.stream_end_at,
        finished_at: r.finished_at,
    }
}

/// The serving attempt's figures on the plan: its shares under the account tier it recorded, `None` for an attempt
/// that recorded none, and its 20x equivalent, which reads no account tier.
struct PlanFigures {
    shares: Option<PricedShares>,
    equivalent: PricedShares,
    share_reason: Option<&'static str>,
}

/// `None` for a request no subscription window paid: no attempt, or a serving attempt whose funding is not plan.
/// The request's receipt selects the cost revision, as it selects the price revision of the dollars, and the
/// attempt's completion (else the request's, else the attempt's start) reads the peak band, where the meter charges
/// it. The entrypoint the request's `client` names picks the regime both figures price under.
fn plan_figures(
    r: &RequestRecord,
    attempts: &[AttemptRecord],
    tables: &PlanTables,
) -> Option<PlanFigures> {
    let a = attempts.last()?;
    if funding_of(
        &a.account_kind,
        a.usage_based,
        a.overage_in_use,
        a.speed.as_deref(),
    ) != Funding::Plan
    {
        return None;
    }
    let usage = PinUsage {
        input: a.input_tokens.unwrap_or(0),
        cache_read: a.cache_read_tokens.unwrap_or(0),
        write_5m: a.cache_write_5m_tokens.unwrap_or(0),
        write_1h: a.cache_write_1h_tokens.unwrap_or(0),
        output: a.output_tokens.unwrap_or(0),
    };
    let band_at = a.finished_at.or(r.finished_at).unwrap_or(a.started_at);
    let entrypoint = r.client.as_deref().and_then(entrypoint_of_client);
    if r.classifier.as_deref() == Some(Classifier::Client.as_str()) {
        // The admin prices through the share helpers and not price_row, so the free call's rule is taken here.
        return Some(PlanFigures {
            shares: a.tier.as_deref().map(|tier| PricedShares {
                shares: free_shares(tables, entrypoint, tier, &r.canonical_model, r.received_at),
                estimated: Default::default(),
            }),
            equivalent: PricedShares {
                shares: free_equivalent(tables, entrypoint, &r.canonical_model, r.received_at),
                estimated: Default::default(),
            },
            share_reason: Some(SHARE_REASON_CLIENT_CLASSIFIER),
        });
    }
    Some(PlanFigures {
        shares: a.tier.as_deref().map(|tier| {
            price_shares(
                tables,
                entrypoint,
                tier,
                &r.canonical_model,
                usage,
                r.received_at,
                band_at,
            )
        }),
        equivalent: price_equivalent(
            tables,
            entrypoint,
            &r.canonical_model,
            usage,
            r.received_at,
            band_at,
        ),
        share_reason: None,
    })
}

/// `{}` for an attempt that recorded no account tier, since the burn route prices no share without one.
fn cost_shares_json(plan: &PlanFigures) -> BTreeMap<String, Option<f64>> {
    plan.shares
        .as_ref()
        .map_or_else(BTreeMap::new, windows_json)
}

/// A sorted map keeps the order `price_shares` and `price_equivalent` insert the window ids in: `five_hour`,
/// `seven_day`, then the model week named `seven_day_<family>`.
fn windows_json(priced: &PricedShares) -> BTreeMap<String, Option<f64>> {
    priced
        .shares
        .iter()
        .map(|(window, share)| (window.to_string(), *share))
        .collect()
}

fn usage_json(last: Option<&AttemptRecord>) -> RequestUsage {
    let tokens = |f: fn(&AttemptRecord) -> Option<u64>| last.and_then(f).unwrap_or(0);
    RequestUsage {
        cache_read: tokens(|a| a.cache_read_tokens),
        cache_write_1h: tokens(|a| a.cache_write_1h_tokens),
        cache_write_5m: tokens(|a| a.cache_write_5m_tokens),
        input: tokens(|a| a.input_tokens),
        output: tokens(|a| a.output_tokens),
        thinking: tokens(|a| a.thinking_tokens),
        web_fetch: tokens(|a| a.web_fetch_requests),
        web_search: tokens(|a| a.web_search_requests),
    }
}

/// The attempt's dollars at the cells its own prompt and its reported speed select on the revision in force when its
/// request arrived, `None` for an attempt without usage and for a model the revision lacks.
fn attempt_usd(a: &AttemptRecord, r: &RequestRecord, prices: &PriceRevisions) -> Option<f64> {
    if !a.has_usage() {
        return None;
    }
    prices
        .in_force(r.received_at)
        .estimate(
            &r.canonical_model,
            Speed::of(a.speed.as_deref()),
            a.input_tokens.unwrap_or(0),
            a.cache_read_tokens.unwrap_or(0),
            a.cache_write_5m_tokens.unwrap_or(0),
            a.cache_write_1h_tokens.unwrap_or(0),
            a.output_tokens.unwrap_or(0),
        )
        .map(|(usd, _)| usd)
}

fn attempt_json(a: &AttemptRecord, r: &RequestRecord, prices: &PriceRevisions) -> Attempt {
    let window = |id: &str, u: Option<f64>, reset: Option<i64>| {
        (u.is_some() || reset.is_some()).then(|| AttemptWindow {
            id: id.to_string(),
            utilization: u,
            resets_at: reset,
        })
    };
    // The attempt's columns hold the head's `5h`, `7d` and `7d_oi`, answered under the usage endpoint's keys as
    // the quota samples are.
    let windows: Vec<AttemptWindow> = [
        window(
            "five_hour",
            a.ratelimit_5h_utilization,
            a.ratelimit_5h_reset,
        ),
        window(
            "seven_day",
            a.ratelimit_7d_utilization,
            a.ratelimit_7d_reset,
        ),
        window(
            "seven_day_overage_included",
            a.ratelimit_7d_oi_utilization,
            a.ratelimit_7d_oi_reset,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    Attempt {
        index: a.attempt,
        account_id: a.account_id.clone(),
        account_kind: a.account_kind.clone(),
        outcome: a.outcome.clone(),
        upstream_status: a.upstream_status,
        upstream_request_id: a.upstream_request_id.clone(),
        upstream_message_id: a.upstream_message_id.clone(),
        started_at: a.started_at,
        duration_ms: a.finished_at.map(|f| f - a.started_at),
        ttfb_ms: a.ttfb_ms,
        ttft_ms: a.ttfc_ms,
        cache_miss_reason: a.cache_miss_reason.clone(),
        service_tier: a.service_tier.clone(),
        client_drift: a.client_drift.clone(),
        cooldown_set: a.cooldown_scope.as_ref().map(|scope| CooldownSet {
            reason: scope.clone(),
            until: a.cooldown_until,
        }),
        windows,
        overage_in_use: a.overage_in_use,
        usage_based: a.usage_based,
        estimated_usd: attempt_usd(a, r, prices),
        selection: a
            .selection
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok()),
        response: AttemptResponse {
            cleared_input_tokens: a.cleared_input_tokens,
            inference_geo: a.inference_geo.clone(),
            refusal_category: a.refusal_category.clone(),
            refusal_recommended_model: a.refusal_recommended_model.clone(),
            served_model: a.served_model.clone(),
            speed: a.speed.clone(),
            stop_reason: a.stop_reason.clone(),
            stop_sequence: a.stop_sequence.clone(),
        },
        tier: a.tier.clone(),
    }
}

pub(crate) fn list_param(q: &HashMap<String, String>, name: &str) -> Vec<String> {
    q.get(name)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `GET /admin/requests`'s query.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RequestsQuery {
    /// Unix milliseconds.
    pub from: Option<i64>,
    /// Unix milliseconds.
    pub to: Option<i64>,
    /// A comma list of numbers.
    pub statuses: Option<String>,
    /// Matches the request id, the session, the key, the model, the error code, and the attempts' account id,
    /// upstream request id and upstream message id, literally: `_`, `%` and `\` are characters.
    pub text: Option<String>,
    /// The one request whose id equals the value, literally. Combines with `text` and every other filter.
    pub request: Option<String>,
    /// The requests whose session id equals the value, literally. Combines with `text` and every other filter.
    pub session: Option<String>,
    /// The requests whose agent id equals the value, literally. Combines with `session` and every other filter.
    pub agent_id: Option<String>,
    /// Unix milliseconds. Only the rows the ledger wrote after this instant, for a poll that passes back the
    /// `landed_through` of its last answer, beside the other filters.
    pub landed_after: Option<i64>,
    /// A comma list. A value is read as the breakdown's: a bare id for the current key's rows,
    /// `<id>@<generation>` for an earlier holder's, or `-` for requests no key was read for.
    pub keys: Option<String>,
    /// A comma list.
    pub pools: Option<String>,
    /// A comma list. A value matches the account of the serving attempt, the last one, or `-` for a request no
    /// attempt served.
    pub accounts: Option<String>,
    /// A comma list.
    pub models: Option<String>,
    /// A comma list.
    pub kinds: Option<String>,
    /// A comma list.
    pub outcomes: Option<String>,
    /// A pattern, the grammar of the analytics' `user_agents`.
    pub user_agent: Option<String>,
    /// A pattern over the client class, the same grammar and bound.
    pub client: Option<String>,
    /// A comma list.
    pub agent_types: Option<String>,
    /// A comma list.
    pub turn_origins: Option<String>,
    /// A comma list of reasons, read off the serving attempt, any name the beta sent.
    pub cache_miss_reasons: Option<String>,
    /// A comma list of stop reasons, read off the serving attempt, the last one, `-` for a request whose serving
    /// attempt carries none or that no attempt served.
    pub stop_reasons: Option<String>,
    /// A comma list of refusal categories, read off the serving attempt as `stop_reasons` is, `-` for a request with
    /// none.
    pub refusal_categories: Option<String>,
    /// A comma list of efforts as the rollups fold them: a level, `none`, `other`.
    pub efforts: Option<String>,
    /// A comma list of classes as the rollups fold them: the class, `-`, `other`.
    pub request_classes: Option<String>,
    /// A comma list of `client`, `-` for a request the marker does not hold.
    pub classifiers: Option<String>,
    /// A comma list of guardrail ids, `-` for a request no guardrail matched.
    pub guardrails: Option<String>,
    /// A comma list of `fast`, `standard`, read off the serving attempt, else `fast` for a request that asked for
    /// it and `standard` otherwise.
    pub speeds: Option<String>,
    /// `true`/`1`: the requests that waited through a hold, one that answered without a dispatch included.
    pub held: Option<bool>,
    /// `true`/`1`.
    pub include_probes: Option<bool>,
    /// `true`/`1`: the range's end is fixed, so `landed_through` may pass `to` and a poll over the range reads a row
    /// that landed after the end once. Leave it off for a live range.
    pub fixed_end: Option<bool>,
    /// Default 50, clamped to 1 through 500.
    pub limit: Option<u32>,
    /// `<received_at>:<id>`.
    pub cursor: Option<String>,
}

/// List the requests.
#[utoipa::path(
    get,
    path = "/admin/requests",
    operation_id = "requests_list",
    tag = "Requests",
    params(RequestsQuery),
    responses(
        (status = 200, description = "The page.", body = RequestList),
        (status = 400, description = "- `{name} is not a number`, for `from` or `to`\n- `landed_after is not a number`\n- `statuses are numbers`\n- `limit is not a number`\n- `cursor is <received_at>:<id>`\n- `patterns are at most 256 characters, at most 32 of them`, for `user_agent` or `client`", body = ErrorBody, example = json!({"error": "{name} is not a number"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn list(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let number = |name: &'static str| match q.get(name) {
        Some(v) => v.parse::<i64>().map(Some).map_err(|_| name),
        None => Ok(None),
    };
    let (from, to) = match (number("from"), number("to")) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(name), _) | (_, Err(name)) => {
            return failure(StatusCode::BAD_REQUEST, &format!("{name} is not a number"))
        }
    };
    let landed_after = match number("landed_after") {
        Ok(landed_after) => landed_after,
        Err(name) => return failure(StatusCode::BAD_REQUEST, &format!("{name} is not a number")),
    };
    let statuses: Result<Vec<u16>, _> = list_param(&q, "statuses")
        .iter()
        .map(|s| s.parse::<u16>())
        .collect();
    let Ok(statuses) = statuses else {
        return failure(StatusCode::BAD_REQUEST, "statuses are numbers");
    };
    let filter = RequestFilter {
        text: q.get("text").cloned(),
        // An empty value narrows nothing, as an absent one, since the ledger drops it as it drops an empty `text`.
        request: q.get("request").cloned(),
        session: q.get("session").cloned(),
        agent_id: q.get("agent_id").cloned(),
        landed_after,
        keys: list_param(&q, "keys"),
        pools: list_param(&q, "pools"),
        accounts: list_param(&q, "accounts"),
        models: list_param(&q, "models"),
        kinds: list_param(&q, "kinds"),
        statuses,
        outcomes: list_param(&q, "outcomes"),
        user_agent: q.get("user_agent").cloned(),
        client: q.get("client").cloned(),
        clients: Vec::new(),
        user_agents: Vec::new(),
        agent_types: list_param(&q, "agent_types"),
        turn_origins: list_param(&q, "turn_origins"),
        speeds: list_param(&q, "speeds"),
        held: q.get("held").is_some_and(|v| v == "true" || v == "1"),
        from,
        to,
        include_probes: q
            .get("include_probes")
            .is_some_and(|v| v == "true" || v == "1"),
        cache_miss_reasons: list_param(&q, "cache_miss_reasons"),
        efforts: list_param(&q, "efforts"),
        request_classes: list_param(&q, "request_classes"),
        classifiers: list_param(&q, "classifiers"),
        guardrails: list_param(&q, "guardrails"),
        stop_reasons: list_param(&q, "stop_reasons"),
        refusal_categories: list_param(&q, "refusal_categories"),
        fixed_end: q.get("fixed_end").is_some_and(|v| v == "true" || v == "1"),
    };
    if !phoebis_ledger::patterns_within_bounds(filter.user_agent.as_slice())
        || !phoebis_ledger::patterns_within_bounds(filter.client.as_slice())
    {
        return failure(StatusCode::BAD_REQUEST, phoebis_ledger::PATTERNS_TOO_LONG);
    }
    let limit = match q.get("limit") {
        None => DEFAULT_LIMIT,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) => n,
            Err(_) => return failure(StatusCode::BAD_REQUEST, "limit is not a number"),
        },
    }
    .clamp(1, MAX_LIMIT);
    let cursor = q.get("cursor").cloned();
    if cursor.as_deref().is_some_and(|c| {
        c.split_once(':')
            .is_none_or(|(at, _)| at.parse::<i64>().is_err())
    }) {
        return failure(StatusCode::BAD_REQUEST, "cursor is <received_at>:<id>");
    }
    let shares = ShareContext::read(&state);
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    match state
        .ledger
        .db()
        .read(move |c| list_request_page(c, &filter, &live, cursor.as_deref(), limit))
        .await
    {
        Ok(page) => Json(RequestList {
            requests: page
                .rows
                .iter()
                .map(|(r, a)| request_summary(r, a, &shares))
                .collect(),
            next_cursor: page.next,
            landed_through: page.landed_through,
        })
        .into_response(),
        Err(e) => ledger_failure(e),
    }
}

/// Read a request with its events.
#[utoipa::path(
    get,
    path = "/admin/requests/{id}",
    operation_id = "request_show",
    tag = "Requests",
    params(("id" = String, Path, description = "The request's id")),
    responses(
        (status = 200, description = "The request with its events.", body = RequestDetail),
        (status = 404, description = "`no such request`", body = ErrorBody, example = json!({"error": "no such request"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn show(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let shares = ShareContext::read(&state);
    let found = state
        .ledger
        .db()
        .read(move |c| {
            let Some((r, a)) = request_by_id(c, &id)? else {
                return Ok(None);
            };
            let events = request_events(c, &r)?;
            Ok(Some((r, a, events)))
        })
        .await;
    match found {
        Ok(Some((r, a, events))) => Json(RequestDetail {
            request: request_summary(&r, &a, &shares),
            events: events.iter().map(event_json).collect(),
            stream_cut: stream_cut(&r.id, &events),
        })
        .into_response(),
        Ok(None) => failure(StatusCode::NOT_FOUND, "no such request"),
        Err(e) => ledger_failure(e),
    }
}

/// The newest `stream_interrupted` row the request wrote itself, last among `events` since the read orders them by
/// `at`. Matched by its id: a subagent's call shares the session, so its cut can sit inside a main turn's window.
fn stream_cut(request_id: &str, events: &[phoebis_ledger::EventRecord]) -> Option<StreamCut> {
    let row = events
        .iter()
        .rev()
        .find(|e| e.kind == "stream_interrupted" && e.request_id.as_deref() == Some(request_id))?;
    let detail: Value = serde_json::from_str(row.detail.as_deref()?).ok()?;
    Some(StreamCut {
        at: row.at,
        code: detail["code"].as_str()?.to_string(),
        text: detail["text"].as_str()?.to_string(),
        bytes_relayed: detail["bytes_relayed"].as_u64()?,
        upstream_request_id: detail["upstream_request_id"].as_str().map(str::to_string),
        attempt: u32::try_from(detail["attempt"].as_u64()?).ok()?,
    })
}

pub(crate) fn event_json(e: &phoebis_ledger::EventRecord) -> Event {
    let mut detail = e
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .unwrap_or(Value::Null);
    decode_diff(&mut detail);
    Event {
        at: e.at,
        kind: e.kind.clone(),
        account_id: e.account_id.clone(),
        key_id: e.key_id.clone(),
        session_id: e.session_id.clone(),
        model: e.model.clone(),
        actor: e.actor.clone(),
        detail,
        pool_id: e.pool_id.clone(),
        agent_id: e.agent_id.clone(),
        request_id: e.request_id.clone(),
    }
}

/// A row the mirror wrote before `registry_changed`'s diff became a value still carries it as a JSON string, which
/// is decoded here so every row reads the same.
pub(crate) fn decode_diff(detail: &mut Value) {
    if let Some(diff) = detail
        .get("diff")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    {
        detail["diff"] = diff;
    }
}

/// Read the captured bodies.
#[utoipa::path(
    get,
    path = "/admin/requests/{id}/bodies",
    operation_id = "request_bodies",
    tag = "Requests",
    params(("id" = String, Path, description = "The request's id")),
    responses(
        (status = 200, description = "The request's captured bodies.", body = Bodies),
        (status = 404, description = "`no captured bodies for this request`, once the capture has expired or was never made", body = ErrorBody, example = json!({"error": "no captured bodies for this request"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn bodies(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    let as_json_or_text = |bytes: &[u8]| {
        serde_json::from_slice::<Value>(bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
    };
    let db = state.ledger.db();
    let b = match db.bodies(&id, (state.clock)()).await {
        Ok(Some(b)) => b,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "no captured bodies for this request"),
        Err(e) => return ledger_failure(e),
    };
    // The request row's flags know what the stored lengths cannot: a chunk the bus dropped is missing from both
    // the capture and its seen count, so only the assembler, which saw the chunk count fall short, can tell.
    let (request_cut, response_cut) = match db.request(&id).await {
        Ok(row) => row.map_or((false, false), |(r, _)| {
            (r.request_truncated, r.response_truncated)
        }),
        Err(e) => return ledger_failure(e),
    };
    // The byte counts are read off the raw vectors before rendering: a re-encoded JSON string adds quotes and
    // escapes, and a JavaScript `.length` on it counts UTF-16 units, so neither measures what was cut.
    Json(Bodies {
        request_id: b.request_id,
        request: as_json_or_text(&b.request_body),
        response: b.response_body.as_deref().map(as_json_or_text),
        request_bytes_stored: b.request_body.len() as u64,
        request_bytes_seen: b.request_len_seen,
        response_bytes_stored: b.response_body.as_ref().map_or(0, Vec::len) as u64,
        response_bytes_seen: b.response_len_seen,
        request_truncated: request_cut || (b.request_body.len() as u64) < b.request_len_seen,
        response_truncated: response_cut
            || b.response_body
                .as_ref()
                .is_some_and(|body| (body.len() as u64) < b.response_len_seen),
        expires_at: b.expires_at,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::round_trip::{fixture, round_trips};
    use phoebis_ledger::{CostRevisions, PlanTables, RequestRecord};
    use serde_json::json;

    /// Monday 2026-09-21 at 20:00 UTC, past the peak band on a weekday.
    const WEEKDAY_OFF_PEAK: i64 = 1_790_020_800_000;

    /// The same Monday at 18:00 UTC, 11:00 in Los Angeles, where `SHARE_COSTS`'s peak band ends.
    const WEEKDAY_PEAK_END: i64 = WEEKDAY_OFF_PEAK - 2 * 3_600_000;

    /// A cost model in round numbers: one Fable input token weighs 10 units, `test_tier` holds 100 units on the
    /// 5-hour window and 1000 on the week, `narrow_tier` sizes the 5-hour window alone, and the Fable week is half
    /// the 7-day window.
    const SHARE_COSTS: &str = "\
revisions:
  - effective: 2026-01-01
    unit:
      defined_by: {tier: test_tier, window: five_hour, band: out_of_band}
      units_per_window: 100
    capacity:
      test_tier:   {five_hour: 100, seven_day: 1000}
      narrow_tier: {five_hour: 100, seven_day: null}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-fable-5-1: 10
      claude-sonnet-5-5: 1
      claude-haiku-4-5: 0.5
    model_windows:
      seven_day_fable: {of: seven_day, capacity_factor: 0.5}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-fable-5-1: fable
      claude-sonnet-5-5: sonnet
      claude-haiku-4-5: haiku
";

    fn share_tables(revisions: CostRevisions) -> PlanTables {
        PlanTables {
            revisions,
            path: None,
            source: "test".to_string(),
        }
    }

    fn shares_of() -> ShareContext {
        ShareContext::of(
            share_tables(
                CostRevisions::from_yaml(SHARE_COSTS).expect("the share cost model parses"),
            ),
            PriceRevisions::shipped(),
        )
    }

    fn request_of(model: &str) -> RequestRecord {
        RequestRecord {
            canonical_model: model.into(),
            finished_at: Some(WEEKDAY_OFF_PEAK + 900),
            ..Default::default()
        }
    }

    /// One input token on account `a` under `test_tier`, finished off the band.
    fn attempt_of(kind: &str, overage_in_use: Option<bool>, speed: Option<&str>) -> AttemptRecord {
        AttemptRecord {
            account_id: "a".into(),
            account_kind: kind.into(),
            started_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 400),
            input_tokens: Some(1),
            overage_in_use,
            speed: speed.map(String::from),
            tier: Some("test_tier".into()),
            ..Default::default()
        }
    }

    fn cost_shares(model: &str, attempt: AttemptRecord, shares: &ShareContext) -> Value {
        serde_json::to_value(request_summary(&request_of(model), &[attempt], shares)).unwrap()
            ["cost_shares"]
            .clone()
    }

    #[test]
    fn an_attempt_started_inside_the_peak_band_and_finished_outside_it_prices_off_band() {
        let shares = shares_of();
        let (inside, outside) = (WEEKDAY_PEAK_END - 60_000, WEEKDAY_PEAK_END + 60_000);
        assert!(
            shares.tables.band_factor("five_hour", None, inside) > 1.0
                && shares.tables.band_factor("five_hour", None, outside) == 1.0,
            "the two instants sit either side of the band's end"
        );
        let request = RequestRecord {
            finished_at: Some(inside),
            ..request_of("claude-fable-5-1")
        };
        let attempt = AttemptRecord {
            started_at: inside,
            finished_at: Some(outside),
            ..attempt_of("oauth", None, None)
        };
        assert_eq!(
            serde_json::to_value(request_summary(&request, &[attempt], &shares)).unwrap()
                ["cost_shares"],
            cost_shares("claude-fable-5-1", attempt_of("oauth", None, None), &shares),
            "the attempt's finish prices the share, neither the request's finish nor the attempt's start"
        );
    }

    fn keys(shares: &Value) -> Vec<&str> {
        shares
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn a_request_with_no_priced_attempt_costs_an_unsigned_zero() {
        let summary = serde_json::to_value(request_summary(
            &RequestRecord::default(),
            &[],
            &ShareContext::of(PlanTables::shipped(), PriceRevisions::shipped()),
        ))
        .unwrap();
        assert_eq!(
            summary["cost_usd"].to_string(),
            "0.0",
            "a signed zero on the wire is read back as -$0.00"
        );
        assert_eq!(
            summary["cost_shares"],
            Value::Null,
            "a request with no attempt is billed in dollars"
        );
    }

    #[test]
    fn a_summary_s_phases_read_the_row_and_the_serving_attempt() {
        let row = RequestRecord {
            received_at: 1_000,
            finished_at: Some(1_900),
            ttfb_ms: Some(120),
            ttfc_ms: Some(300),
            stream_end_at: Some(1_800),
            ..Default::default()
        };
        let attempts = [
            AttemptRecord {
                started_at: 1_010,
                ..Default::default()
            },
            AttemptRecord {
                started_at: 1_400,
                written_at: Some(1_430),
                ..Default::default()
            },
        ];
        let shares = ShareContext::of(PlanTables::shipped(), PriceRevisions::shipped());
        let summary = |attempts: &[AttemptRecord]| {
            serde_json::to_value(request_summary(&row, attempts, &shares)).unwrap()
        };
        assert_eq!(
            summary(&attempts)["phases"],
            json!({
                "received_at": 1_000, "selected_at": 1_010, "written_at": 1_430, "head_at": 1_120,
                "first_content_at": 1_300, "stream_end_at": 1_800, "finished_at": 1_900,
            }),
            "the first attempt's start is the selection and the serving attempt's write is the write"
        );
        assert_eq!(
            summary(&[])["phases"],
            json!({
                "received_at": 1_000, "selected_at": null, "written_at": null, "head_at": 1_120,
                "first_content_at": 1_300, "stream_end_at": 1_800, "finished_at": 1_900,
            }),
            "a request with no attempt was neither selected nor written"
        );
        assert_eq!(
            summary(&attempts)["cost_shares"],
            json!({}),
            "a plan attempt that recorded no tier reads an empty object"
        );
    }

    #[test]
    fn cost_shares_is_null_for_an_api_key_attempt() {
        let shares = shares_of();
        assert_eq!(
            cost_shares(
                "claude-fable-5-1",
                attempt_of("api_key", None, None),
                &shares
            ),
            Value::Null
        );
        assert_eq!(
            cost_shares(
                "claude-fable-5-1",
                attempt_of("api_key", Some(true), Some("fast")),
                &shares
            ),
            Value::Null,
            "the kind decides whatever the speed or the overage flag says"
        );
    }

    /// The ledger's readers take the free rule on the bus's word alone, so a row naming another word prices from its
    /// counts here as it does in the analytics.
    #[test]
    fn a_classifier_word_other_than_client_prices_from_the_counts() {
        let shares = shares_of();
        let summary = |classifier: Option<&str>| {
            let request = RequestRecord {
                classifier: classifier.map(String::from),
                ..request_of("claude-fable-5-1")
            };
            serde_json::to_value(request_summary(
                &request,
                &[attempt_of("oauth", None, None)],
                &shares,
            ))
            .unwrap()
        };
        let (plain, other, client) = (
            summary(None),
            summary(Some("other")),
            summary(Some("client")),
        );
        let figures = |summary: &Value| {
            (
                summary["cost_shares"].clone(),
                summary["cost_equivalent"].clone(),
                summary["share_reason"].clone(),
            )
        };
        assert_eq!(figures(&other), figures(&plain));
        assert_eq!(plain["share_reason"], Value::Null);
        assert_eq!(client["share_reason"], "client_classifier");
        assert_ne!(
            client["cost_shares"], plain["cost_shares"],
            "the counts price above the free call's zero"
        );
    }

    #[test]
    fn cost_shares_is_null_for_an_overage_attempt() {
        let shares = shares_of();
        assert_eq!(
            cost_shares(
                "claude-fable-5-1",
                attempt_of("oauth", Some(true), None),
                &shares
            ),
            Value::Null
        );
    }

    #[test]
    fn cost_shares_is_null_for_a_fast_answer() {
        let shares = shares_of();
        assert_eq!(
            cost_shares(
                "claude-fable-5-1",
                attempt_of("oauth", None, Some("fast")),
                &shares
            ),
            Value::Null
        );
    }

    #[test]
    fn a_plan_attempt_of_a_fable_model_reads_three_windows() {
        let shares = shares_of();
        let got = cost_shares("claude-fable-5-1", attempt_of("oauth", None, None), &shares);
        assert_eq!(keys(&got), ["five_hour", "seven_day", "seven_day_fable"]);
        assert!(
            keys(&got)
                .iter()
                .all(|k| got[k].as_f64().is_some_and(|s| s > 0.0 && s < 1.0)),
            "each window a positive fraction: {got}"
        );
        assert_eq!(
            cost_shares(
                "claude-fable-5-1",
                attempt_of("oauth", Some(false), Some("standard")),
                &shares
            ),
            got,
            "a standard answer with the overage flag read off is the plan reading"
        );
    }

    #[test]
    fn the_model_week_is_absent_for_a_haiku_model() {
        let shares = shares_of();
        let got = cost_shares("claude-haiku-4-5", attempt_of("oauth", None, None), &shares);
        assert_eq!(keys(&got), ["five_hour", "seven_day"]);
        assert!(
            keys(&got)
                .iter()
                .all(|k| got[k].as_f64().is_some_and(|s| s > 0.0)),
            "{got}"
        );
    }

    #[test]
    fn an_attempt_that_recorded_no_tier_reads_an_empty_object_and_an_unsized_window_reads_null() {
        let shares = shares_of();
        let untiered = AttemptRecord {
            tier: None,
            ..attempt_of("oauth", None, None)
        };
        assert_eq!(
            cost_shares("claude-fable-5-1", untiered, &shares),
            json!({}),
            "an attempt that recorded no tier prices no share, as the burn route prices none"
        );
        let narrow = cost_shares(
            "claude-fable-5-1",
            AttemptRecord {
                tier: Some("narrow_tier".into()),
                ..attempt_of("oauth", None, None)
            },
            &shares,
        );
        assert_eq!(keys(&narrow), ["five_hour", "seven_day", "seven_day_fable"]);
        assert!(
            narrow["five_hour"].as_f64().is_some_and(|s| s > 0.0)
                && narrow["seven_day"].is_null()
                && narrow["seven_day_fable"].is_null(),
            "the 5-hour window prices and the two the tier does not size read null: {narrow}"
        );
    }

    #[test]
    fn the_serving_attempt_s_tier_prices_the_shares_and_names_the_summary_s_tier() {
        let attempts = [
            AttemptRecord {
                attempt: 1,
                outcome: "failed".into(),
                tier: Some("narrow_tier".into()),
                ..attempt_of("oauth", None, None)
            },
            AttemptRecord {
                attempt: 2,
                ..attempt_of("oauth", None, None)
            },
        ];
        let summary = request_summary(&request_of("claude-fable-5-1"), &attempts, &shares_of());
        assert_eq!(
            (
                summary.tier.as_deref(),
                summary
                    .attempts
                    .iter()
                    .map(|a| a.tier.as_deref())
                    .collect::<Vec<_>>()
            ),
            (
                Some("test_tier"),
                vec![Some("narrow_tier"), Some("test_tier")]
            ),
            "the summary names the serving attempt's tier and each attempt its own"
        );
        let shares = summary.cost_shares.expect("a plan request");
        assert!(
            shares["seven_day"].is_some_and(|s| s > 0.0),
            "the serving attempt's tier sizes the week the earlier attempt's did not: {shares:?}"
        );
    }

    #[test]
    fn cost_equivalent_answers_the_three_columns_for_a_plan_request_and_null_for_a_billed_one() {
        let shares = shares_of();
        let plan = request_summary(
            &request_of("claude-fable-5-1"),
            &[attempt_of("oauth", None, None)],
            &shares,
        );
        let equivalent = plan.cost_equivalent.expect("a plan request");
        let one_input = PinUsage {
            input: 1,
            ..PinUsage::default()
        };
        let finished = WEEKDAY_OFF_PEAK + 400;
        assert_eq!(
            equivalent.keys().map(String::as_str).collect::<Vec<_>>(),
            ["five_hour", "seven_day", "seven_day_fable"],
            "the two shared windows and the model week"
        );
        for (window, share) in &equivalent {
            assert_eq!(
                *share,
                shares
                    .tables
                    .equivalent_at(window, None, "claude-fable-5-1", one_input, finished),
                "{window}: the cost model's units over the anchor tier's capacity"
            );
        }
        let billed = serde_json::to_value(request_summary(
            &request_of("claude-fable-5-1"),
            &[attempt_of("api_key", None, None)],
            &shares,
        ))
        .unwrap();
        assert_eq!(
            billed.get("cost_equivalent"),
            Some(&Value::Null),
            "a request billed in dollars answers the member as null: {billed}"
        );
    }

    #[test]
    fn a_plan_request_on_a_model_without_a_weight_is_estimated_and_one_with_a_weight_is_not() {
        let shares = shares_of();
        let estimated = |model: &str, kind: &str| {
            let summary =
                request_summary(&request_of(model), &[attempt_of(kind, None, None)], &shares);
            (summary.estimated, summary.cost_equivalent.is_some())
        };
        assert_eq!(
            estimated("claude-sonnet-5", "oauth"),
            (true, true),
            "a Sonnet model the cost model does not list prices at Sonnet 5.5's weight"
        );
        assert_eq!(
            estimated("claude-fable-5-1", "oauth"),
            (false, true),
            "a weighted model prices at its own weight"
        );
        assert_eq!(
            estimated("claude-sonnet-5", "api_key"),
            (false, false),
            "a request billed in dollars has no window figure to mark"
        );
    }

    #[test]
    fn the_request_s_receipt_selects_the_cost_revision_and_the_attempt_s_completion_the_band() {
        /// Monday 2026-09-21 at 23:00 UTC, 16:00 in Los Angeles: off the band, before the second revision.
        const RECEIPT: i64 = 1_790_031_600_000;
        /// Tuesday 2026-09-22 at 13:00 UTC, 06:00 in Los Angeles: inside the band, under the second revision.
        const COMPLETION: i64 = 1_790_082_000_000;
        let first = CostRevisions::from_yaml(SHARE_COSTS)
            .expect("the share cost model parses")
            .latest()
            .clone();
        // Tripled rather than doubled, so no pairing of a revision with a band lands on the figure of another: the
        // band halves the capacity, which a doubled weight would match.
        let mut second = first.clone();
        second.effective = "2026-09-22".into();
        for weight in second.model_weight.values_mut().flatten() {
            *weight *= 3.0;
        }
        let shares = ShareContext::of(
            share_tables(CostRevisions::new(vec![first.clone(), second.clone()]).unwrap()),
            PriceRevisions::shipped(),
        );
        let request = RequestRecord {
            received_at: RECEIPT,
            finished_at: Some(COMPLETION),
            ..request_of("claude-fable-5-1")
        };
        let attempt = AttemptRecord {
            started_at: RECEIPT,
            finished_at: Some(COMPLETION),
            ..attempt_of("oauth", None, None)
        };
        let summary = request_summary(&request, &[attempt], &shares);
        let one_input = PinUsage {
            input: 1,
            ..PinUsage::default()
        };
        let share = |costs: &phoebis_ledger::SubscriptionCosts, at: i64| {
            costs.share_at(
                "five_hour",
                None,
                "test_tier",
                "claude-fable-5-1",
                one_input,
                at,
            )
        };
        let equivalent = |costs: &phoebis_ledger::SubscriptionCosts, at: i64| {
            costs.equivalent_at("five_hour", None, "claude-fable-5-1", one_input, at)
        };
        let got = (
            summary.cost_shares.expect("a plan request")["five_hour"],
            summary.cost_equivalent.expect("a plan request")["five_hour"],
        );
        assert_eq!(
            got,
            (share(&first, COMPLETION), equivalent(&first, COMPLETION)),
            "the first revision's weight at the completion's band"
        );
        for (wrong, pairing) in [
            (
                (share(&first, RECEIPT), equivalent(&first, RECEIPT)),
                "the band read at the receipt",
            ),
            (
                (share(&second, COMPLETION), equivalent(&second, COMPLETION)),
                "the revision read at the completion",
            ),
            (
                (share(&second, RECEIPT), equivalent(&second, RECEIPT)),
                "both read at the other instant",
            ),
        ] {
            assert_ne!(got, wrong, "{pairing} prices another figure");
        }
    }

    /// One plain input token at $1 a million in January and $2 from February.
    const TWO_PRICES: &str = "\
revisions:
  - effective: 2026-01-01
    models:
      claude-fable-5-1: {input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1}
  - effective: 2026-02-01
    models:
      claude-fable-5-1: {input: 2, cache_write_5m: 2, cache_write_1h: 2, cache_read: 2, output: 2}
";

    /// Saturday 2026-01-31 at 23:59 UTC, under the first price revision.
    const LAST_JANUARY_MINUTE: i64 = 1_769_903_940_000;

    /// Sunday 2026-02-01 at 00:01 UTC, under the second.
    const FIRST_FEBRUARY_MINUTE: i64 = 1_769_904_060_000;

    fn priced_by(prices: &str) -> ShareContext {
        ShareContext::of(
            share_tables(
                CostRevisions::from_yaml(SHARE_COSTS).expect("the share cost model parses"),
            ),
            PriceRevisions::from_yaml(prices, "test").expect("the price list parses"),
        )
    }

    #[test]
    fn an_attempt_s_dollars_read_the_price_revision_in_force_at_its_request_s_receipt() {
        let request = RequestRecord {
            received_at: LAST_JANUARY_MINUTE,
            finished_at: Some(FIRST_FEBRUARY_MINUTE + 1_000),
            ..request_of("claude-fable-5-1")
        };
        let attempt = AttemptRecord {
            started_at: FIRST_FEBRUARY_MINUTE,
            finished_at: Some(FIRST_FEBRUARY_MINUTE + 1_000),
            ..attempt_of("api_key", None, None)
        };
        let summary = request_summary(&request, &[attempt], &priced_by(TWO_PRICES));
        assert_eq!(
            (summary.attempts[0].estimated_usd, summary.cost_usd),
            (Some(1e-6), 1e-6),
            "January's cell, though the attempt started and finished in February"
        );
    }

    #[test]
    fn an_attempt_s_dollars_read_null_without_usage_and_for_a_model_the_revision_lacks() {
        let shares = priced_by(TWO_PRICES);
        let request = RequestRecord {
            received_at: LAST_JANUARY_MINUTE,
            ..request_of("claude-fable-5-1")
        };
        let without_usage = AttemptRecord {
            input_tokens: None,
            ..attempt_of("api_key", None, None)
        };
        let unlisted = RequestRecord {
            canonical_model: "claude-mystery-1".into(),
            ..request.clone()
        };
        for (request, attempt, case) in [
            (&request, without_usage, "an attempt without usage"),
            (
                &unlisted,
                attempt_of("api_key", None, None),
                "a model the revision lacks",
            ),
        ] {
            let summary =
                serde_json::to_value(request_summary(request, &[attempt], &shares)).unwrap();
            assert_eq!(
                (
                    summary["attempts"][0].get("estimated_usd"),
                    summary["cost_usd"].to_string()
                ),
                (Some(&Value::Null), "0.0".to_string()),
                "{case}: no dollars, and the request sums none: {summary}"
            );
        }
    }

    #[test]
    fn the_request_fixtures_round_trip_through_their_declared_types() {
        round_trips::<RequestList>("requests");
        round_trips::<RequestDetail>("request");
        round_trips::<Bodies>("request_bodies");
        // The three captures the route answers outside a JSON object, as the document test reads them back.
        for (request, response) in [
            (
                json!({"model": "claude-sonnet-5", "messages": []}),
                json!("event: message_stop\ndata: {}\n\n"),
            ),
            (json!("{\"model\":\"claude-son"), Value::Null),
            (json!(42), json!("ok")),
        ] {
            let mut capture = fixture("request_bodies");
            capture["request"] = request;
            capture["response"] = response;
            let typed: Bodies = serde_json::from_value(capture.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&typed).unwrap(),
                capture,
                "a capture outside a JSON object round-trips"
            );
        }
    }
}
