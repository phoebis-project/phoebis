//! The request log: one row per client request with its attempt trail, the detail with its events, and the
//! captured bodies.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    funding_of, list_requests, price_shares, request_by_id, request_events,
    AttemptRecord, Funding, LiveKeys, PlanTables, RequestFilter, RequestRecord, Tiers,
};
use phoebis_routing::PinUsage;
use serde_json::{json, Value};

use crate::analytics::ledger_failure;
use crate::routes::{failure, store_failure};
use crate::tiers::oauth_tiers;
use crate::AdminState;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// What a request's share is priced with, read once per handler call: the cost model in force and each oauth
/// account's tier as the registry holds it now, so a request made under an earlier tier reads under the current
/// one, as the burn route reads it.
pub(crate) struct ShareContext {
    pub tables: Arc<PlanTables>,
    pub tiers: Tiers,
}

impl ShareContext {
    /// `accounts()` and one `subscription()` per oauth account. A store failure is the route's 500.
    // The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
    // pointer chase and nothing else.
    #[allow(clippy::result_large_err)]
    pub(crate) fn read(state: &AdminState) -> Result<ShareContext, Response> {
        let accounts = state.store.accounts().map_err(store_failure)?;
        let tiers = oauth_tiers(&state.store, &accounts).map_err(store_failure)?;
        Ok(ShareContext::of(state.tables.clone(), tiers))
    }

    /// The tables and tiers given: a test's own cost model, or the process's shared one.
    pub(crate) fn of(tables: impl Into<Arc<PlanTables>>, tiers: Tiers) -> ShareContext {
        ShareContext {
            tables: tables.into(),
            tiers,
        }
    }
}

pub(crate) fn request_summary(
    r: &RequestRecord,
    attempts: &[AttemptRecord],
    shares: &ShareContext,
) -> Value {
    let last = attempts.last();
    json!({
        "id": r.id, "at": r.received_at, "finished_at": r.finished_at, "key_id": r.key_id, "key_generation": r.key_generation, "pool_id": r.pool_id,
        "account_id": last.map(|a| a.account_id.clone()), "session_id": r.session_id, "agent_id": r.agent_id, "parent_agent_id": r.parent_agent_id,
        "model": r.model, "canonical_model": r.canonical_model, "kind": r.kind, "origin": r.origin, "transport": if r.stream { "sse" } else { "json" },
        "request_class": r.request_class, "auto_mode": r.auto_mode,
        "forked_from": r.forked_from, "fork_depth": r.fork_depth,
        "status": r.status, "outcome": r.outcome, "error_code": r.error_code, "client_drift": r.client_drift, "ttfb_ms": r.ttfb_ms, "ttft_ms": r.ttfc_ms, "duration_ms": r.duration_ms,
        "request_bytes": r.request_bytes, "response_bytes": r.response_bytes, "kept_warm": r.kept_warm, "captured_bodies": r.bodies_captured,
        "request_truncated": r.request_truncated, "response_truncated": r.response_truncated,
        "user_agent": r.user_agent,
        "client": r.client, "agent_type": r.agent_type, "compaction": r.compaction, "context_compacted": r.context_compacted, "prev_tool_ms": r.prev_tool_ms, "turn_origin": r.turn_origin,
        "shape": shape_json(r),
        "usage": usage_json(last),
        // Folded onto a positive zero rather than summed: `Sum<f64>` starts at -0.0, so a request with no
        // priced attempt would put a signed zero on the wire and the UI would read "-$0.00".
        "cost_usd": attempts.iter().filter_map(|a| a.estimated_usd).fold(0.0, |a, b| a + b),
        "cost_shares": cost_shares_json(r, attempts, shares),
        "phases": phases_json(r, attempts),
        "attempts": attempts.iter().map(attempt_json).collect::<Vec<_>>(),
        // The request row's own facts rather than an attempt's: a hold that ended in an exhaustion has no attempt.
        "held": r.held.as_deref().and_then(|h| serde_json::from_str::<Value>(h).ok()),
        "diagnostics_source": r.diagnostics_source,
    })
}

/// Its own function, beside `usage_json` and `attempt_json`, for the same reason: nested inline the object
/// would overflow the `json!` macro's expansion depth.
fn shape_json(r: &RequestRecord) -> Value {
    json!({
        "context_edits": r.context_edits.split(',').filter(|e| !e.is_empty()).collect::<Vec<_>>(),
        "effort": r.effort, "max_tokens": r.max_tokens, "message_count": r.message_count,
        "requested_service_tier": r.requested_service_tier, "requested_speed": r.requested_speed,
        "system_blocks": r.system_blocks, "temperature": r.temperature, "thinking_budget": r.thinking_budget,
        "thinking_type": r.thinking_type, "tool_count": r.tool_count,
    })
}

/// Its own function, beside `shape_json` and `usage_json`, for the same reason. The write is the serving
/// attempt's: an earlier attempt's write led nowhere the answer came from.
fn phases_json(r: &RequestRecord, attempts: &[AttemptRecord]) -> Value {
    let last = attempts.last();
    json!({
        "received_at": r.received_at,
        "selected_at": attempts.first().map(|a| a.started_at),
        "written_at": last.and_then(|a| a.written_at),
        "head_at": r.ttfb_ms.map(|ms| r.received_at + ms),
        "first_content_at": r.ttfc_ms.map(|ms| r.received_at + ms),
        "stream_end_at": r.stream_end_at,
        "finished_at": r.finished_at,
    })
}

/// Null for a request no subscription window paid: no attempt, or a serving attempt whose funding is not plan.
/// Else the serving attempt's shares under the account's current tier at its completion, where the meter charges
/// it (else the request's, else the attempt's start), `{}` for an account the tiers lack or hold without a tier,
/// since the burn route prices no share without one.
fn cost_shares_json(r: &RequestRecord, attempts: &[AttemptRecord], shares: &ShareContext) -> Value {
    let Some(a) = attempts.last() else {
        return Value::Null;
    };
    if funding_of(&a.account_kind, a.overage_in_use, a.speed.as_deref()) != Funding::Plan {
        return Value::Null;
    }
    let Some(Some(tier)) = shares.tiers.get(&a.account_id) else {
        return json!({});
    };
    let usage = PinUsage {
        input: a.input_tokens.unwrap_or(0),
        cache_read: a.cache_read_tokens.unwrap_or(0),
        write_5m: a.cache_write_5m_tokens.unwrap_or(0),
        write_1h: a.cache_write_1h_tokens.unwrap_or(0),
        output: a.output_tokens.unwrap_or(0),
    };
    let at = a.finished_at.or(r.finished_at).unwrap_or(a.started_at);
    serde_json::to_value(price_shares(
        &shares.tables,
        tier,
        &r.canonical_model,
        usage,
        at,
    ))
    .expect("window shares serialize")
}

/// Its own function, beside `shape_json` and `attempt_json`, for the same reason.
fn usage_json(last: Option<&AttemptRecord>) -> Value {
    let tokens = |f: fn(&AttemptRecord) -> Option<u64>| last.and_then(f).unwrap_or(0);
    json!({
        "cache_read": tokens(|a| a.cache_read_tokens), "cache_write_1h": tokens(|a| a.cache_write_1h_tokens),
        "cache_write_5m": tokens(|a| a.cache_write_5m_tokens), "input": tokens(|a| a.input_tokens),
        "output": tokens(|a| a.output_tokens), "thinking": tokens(|a| a.thinking_tokens),
        "web_fetch": tokens(|a| a.web_fetch_requests), "web_search": tokens(|a| a.web_search_requests),
    })
}

/// One attempt of the trail. Its own function because the two objects together overflow the `json!` macro's
/// expansion depth.
fn attempt_json(a: &AttemptRecord) -> Value {
    let window = |id: &str, u: Option<f64>, reset: Option<i64>| {
        (u.is_some() || reset.is_some())
            .then(|| json!({ "id": id, "utilization": u, "resets_at": reset }))
    };
    // The attempt's columns hold the head's `5h`, `7d` and `7d_oi`, answered under the usage endpoint's keys as
    // the quota samples are.
    let windows: Vec<Value> = [
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
    json!({
        "index": a.attempt, "account_id": a.account_id, "account_kind": a.account_kind, "outcome": a.outcome,
        "upstream_status": a.upstream_status, "upstream_request_id": a.upstream_request_id,
        "upstream_message_id": a.upstream_message_id, "started_at": a.started_at,
        "duration_ms": a.finished_at.map(|f| f - a.started_at), "ttfb_ms": a.ttfb_ms, "ttft_ms": a.ttfc_ms,
        "cache_miss_reason": a.cache_miss_reason, "service_tier": a.service_tier, "client_drift": a.client_drift,
        "cooldown_set": a.cooldown_scope.as_ref().map(|scope| json!({ "reason": scope, "until": a.cooldown_until })),
        "windows": windows,
        "overage_in_use": a.overage_in_use, "estimated_usd": a.estimated_usd, "pricing_version": a.pricing_version,
        "selection": a.selection.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
        "response": {
            "cleared_input_tokens": a.cleared_input_tokens, "inference_geo": a.inference_geo,
            "refusal_category": a.refusal_category, "refusal_recommended_model": a.refusal_recommended_model,
            "served_model": a.served_model, "speed": a.speed, "stop_reason": a.stop_reason, "stop_sequence": a.stop_sequence,
        },
    })
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
    let statuses: Result<Vec<u16>, _> = list_param(&q, "statuses")
        .iter()
        .map(|s| s.parse::<u16>())
        .collect();
    let Ok(statuses) = statuses else {
        return failure(StatusCode::BAD_REQUEST, "statuses are numbers");
    };
    let filter = RequestFilter {
        text: q.get("text").cloned(),
        keys: list_param(&q, "keys"),
        pools: list_param(&q, "pools"),
        accounts: list_param(&q, "accounts"),
        models: list_param(&q, "models"),
        kinds: list_param(&q, "kinds"),
        statuses,
        outcomes: list_param(&q, "outcomes"),
        user_agent: q.get("user_agent").cloned(),
        client: q.get("client").cloned(),
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
    let shares = match ShareContext::read(&state) {
        Ok(shares) => shares,
        Err(refusal) => return refusal,
    };
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    match state
        .ledger
        .db()
        .read(move |c| list_requests(c, &filter, &live, cursor.as_deref(), limit))
        .await
    {
        Ok((rows, next)) => Json(json!({ "requests": rows.iter().map(|(r, a)| request_summary(r, a, &shares)).collect::<Vec<_>>(), "next_cursor": next })).into_response(),
        Err(e) => ledger_failure(e),
    }
}

pub(crate) async fn show(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let shares = match ShareContext::read(&state) {
        Ok(shares) => shares,
        Err(refusal) => return refusal,
    };
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
        Ok(Some((r, a, events))) => Json(json!({ "request": request_summary(&r, &a, &shares), "events": events.iter().map(event_json).collect::<Vec<_>>() })).into_response(),
        Ok(None) => failure(StatusCode::NOT_FOUND, "no such request"),
        Err(e) => ledger_failure(e),
    }
}

pub(crate) fn event_json(e: &phoebis_ledger::EventRecord) -> Value {
    let mut detail = e
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .unwrap_or(Value::Null);
    decode_diff(&mut detail);
    json!({ "at": e.at, "kind": e.kind, "account_id": e.account_id, "key_id": e.key_id, "session_id": e.session_id, "model": e.model, "actor": e.actor, "detail": detail, "pool_id": e.pool_id, "agent_id": e.agent_id, "request_id": e.request_id })
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

pub(crate) async fn bodies(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    let as_json_or_text = |bytes: &[u8]| {
        serde_json::from_slice::<Value>(bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
    };
    let db = state.ledger.db();
    let b = match db.bodies(&id).await {
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
    Json(json!({
        "request_id": b.request_id, "request": as_json_or_text(&b.request_body), "response": b.response_body.as_deref().map(as_json_or_text),
        "request_bytes_stored": b.request_body.len(), "request_bytes_seen": b.request_len_seen,
        "response_bytes_stored": b.response_body.as_ref().map_or(0, Vec::len), "response_bytes_seen": b.response_len_seen,
        "request_truncated": request_cut || (b.request_body.len() as u64) < b.request_len_seen,
        "response_truncated": response_cut || b.response_body.as_ref().is_some_and(|body| (body.len() as u64) < b.response_len_seen),
        "expires_at": b.expires_at,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_ledger::{PlanTables, RequestRecord, SubscriptionCosts, Tiers};

    /// Monday 2026-09-21 at 20:00 UTC, past the peak band on a weekday.
    const WEEKDAY_OFF_PEAK: i64 = 1_790_020_800_000;

    /// The same Monday at 18:00 UTC, 11:00 in Los Angeles, where `SHARE_COSTS`'s peak band ends.
    const WEEKDAY_PEAK_END: i64 = WEEKDAY_OFF_PEAK - 2 * 3_600_000;

    /// A cost model in round numbers: one Fable input token weighs 10 units, `test_tier` holds 100 units on the
    /// 5-hour window and 1000 on the week, `narrow_tier` sizes the 5-hour window alone, and the Fable week is half
    /// the 7-day window.
    const SHARE_COSTS: &str = "\
effective: 2026-01-01
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
";

    fn shares_of(tiers: &[(&str, Option<&str>)]) -> ShareContext {
        ShareContext::of(
            PlanTables {
                costs: SubscriptionCosts::from_yaml(SHARE_COSTS)
                    .expect("the share cost model parses"),
                path: None,
                source: "test".to_string(),
            },
            tiers
                .iter()
                .map(|(id, tier)| (id.to_string(), tier.map(String::from)))
                .collect(),
        )
    }

    fn request_of(model: &str) -> RequestRecord {
        RequestRecord {
            canonical_model: model.into(),
            finished_at: Some(WEEKDAY_OFF_PEAK + 900),
            ..Default::default()
        }
    }

    /// One input token on account `a`, finished off the band.
    fn attempt_of(kind: &str, overage_in_use: Option<bool>, speed: Option<&str>) -> AttemptRecord {
        AttemptRecord {
            account_id: "a".into(),
            account_kind: kind.into(),
            started_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 400),
            input_tokens: Some(1),
            overage_in_use,
            speed: speed.map(String::from),
            ..Default::default()
        }
    }

    fn cost_shares(model: &str, attempt: AttemptRecord, shares: &ShareContext) -> Value {
        request_summary(&request_of(model), &[attempt], shares)["cost_shares"].clone()
    }

    #[test]
    fn an_attempt_started_inside_the_peak_band_and_finished_outside_it_prices_off_band() {
        let shares = shares_of(&[("a", Some("test_tier"))]);
        let (inside, outside) = (WEEKDAY_PEAK_END - 60_000, WEEKDAY_PEAK_END + 60_000);
        assert!(
            shares.tables.band_factor("five_hour", inside) > 1.0
                && shares.tables.band_factor("five_hour", outside) == 1.0,
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
            request_summary(&request, &[attempt], &shares)["cost_shares"],
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
        let summary = request_summary(
            &RequestRecord::default(),
            &[],
            &ShareContext::of(PlanTables::shipped(), Tiers::new()),
        );
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
        let shares = ShareContext::of(PlanTables::shipped(), Tiers::new());
        let summary = |attempts: &[AttemptRecord]| request_summary(&row, attempts, &shares);
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
            "a plan attempt of an account the tiers lack reads an empty object"
        );
    }

    #[test]
    fn cost_shares_is_null_for_an_api_key_attempt() {
        let shares = shares_of(&[("a", Some("test_tier"))]);
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

    #[test]
    fn cost_shares_is_null_for_an_overage_attempt() {
        let shares = shares_of(&[("a", Some("test_tier"))]);
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
        let shares = shares_of(&[("a", Some("test_tier"))]);
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
        let shares = shares_of(&[("a", Some("test_tier"))]);
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
        let shares = shares_of(&[("a", Some("test_tier"))]);
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
    fn an_absent_account_reads_an_empty_object_and_an_unsized_window_reads_null() {
        let attempt = || attempt_of("oauth", None, None);
        assert_eq!(
            cost_shares("claude-fable-5-1", attempt(), &shares_of(&[])),
            json!({}),
            "an account the tiers lack"
        );
        assert_eq!(
            cost_shares("claude-fable-5-1", attempt(), &shares_of(&[("a", None)])),
            json!({}),
            "an account held without a tier prices no share, as the burn route prices none"
        );
        let narrow = cost_shares(
            "claude-fable-5-1",
            attempt(),
            &shares_of(&[("a", Some("narrow_tier"))]),
        );
        assert_eq!(keys(&narrow), ["five_hour", "seven_day", "seven_day_fable"]);
        assert!(
            narrow["five_hour"].as_f64().is_some_and(|s| s > 0.0)
                && narrow["seven_day"].is_null()
                && narrow["seven_day_fable"].is_null(),
            "the 5-hour window prices and the two the tier does not size read null: {narrow}"
        );
    }
}
