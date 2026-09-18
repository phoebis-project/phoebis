//! The alert routes: rules the operator edits, the events the engine opened, and the deliveries behind each.
//! A rule's secret goes in and never comes out; `has_secret` is all a route says about it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use phoebis_alerts::{allowed_scopes, ipv4_refusal, ipv6_refusal, Kind, ScopeType};
use phoebis_store::{AlertDeliveryRow, AlertEventRow, AlertRuleRow, Secret, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};
use url::{Host, Url};

use crate::routes::{failure, store_failure, Caller};
use crate::{AdminState, Role};

const ACTOR: &str = "admin";
const EVENT_LIMIT: usize = 500;

/// `kind` is read as free text rather than a `ScopeType`, so an unknown or absent scope reaches `validate` and
/// is refused by name instead of failing the body parse with nothing to go on.
#[derive(Deserialize)]
struct ScopeInput {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize, Default)]
struct RuleInput {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    scope: Option<ScopeInput>,
    #[serde(default, deserialize_with = "double_option")]
    threshold: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    window: Option<Option<String>>,
    #[serde(default)]
    webhook_url: Option<String>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Tells an absent field from an explicit `null`, so a `PATCH` can clear a nullable column.
fn double_option<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

fn rule_json(r: &AlertRuleRow) -> Value {
    json!({
        "created_at": r.created_at,
        "enabled": r.enabled,
        "has_secret": r.has_secret,
        "id": r.id,
        "kind": r.kind,
        "scope": { "id": r.scope_id, "type": r.scope_type },
        "threshold": r.threshold,
        "updated_at": r.updated_at,
        "webhook_url": r.webhook_url,
        "window": r.window,
    })
}

fn delivery_json(d: &AlertDeliveryRow) -> Value {
    json!({ "at": d.at, "attempt": d.attempt, "error": d.error, "id": d.id, "state": d.state, "status": d.status })
}

/// The question text in a detail came out of a captured response body, which the viewer may not read on the
/// bodies route; the event route keeps that line by blanking the text for a viewer and saying it did.
fn event_json(state: &AdminState, role: Role, e: &AlertEventRow) -> Result<Value, StoreError> {
    let deliveries = state.store.alert_deliveries(&e.id)?;
    let mut detail = e
        .detail_json
        .as_deref()
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .unwrap_or(Value::Null);
    if role != Role::Admin {
        if let Some(map) = detail.as_object_mut() {
            if map.contains_key("question") {
                map.insert("question".into(), Value::Null);
                map.insert("question_redacted".into(), Value::Bool(true));
            }
        }
    }
    Ok(json!({
        "deliveries": deliveries.iter().map(delivery_json).collect::<Vec<_>>(),
        "detail": detail,
        "id": e.id,
        "kind": e.kind,
        "opened_at": e.opened_at,
        "resolved_at": e.resolved_at,
        "rule_id": e.rule_id,
        "state": e.state,
        "subject": e.subject,
    }))
}

/// The reason a destination is refused, or `None` for one a rule may name. This covers the address a rule
/// spells out; a name is the deliverer's to check, since the record behind it can move afterwards.
fn destination_refusal(url: &Url) -> Option<&'static str> {
    match url.host()? {
        // RFC 6761 reserves the name and everything under it for loopback, so no lookup is needed. A single
        // trailing dot spells the same name, so trim it before comparing.
        Host::Domain(name) => {
            let name = name.strip_suffix('.').unwrap_or(name);
            (name == "localhost" || name.ends_with(".localhost")).then_some(phoebis_alerts::LOOPBACK)
        }
        Host::Ipv4(ip) => ipv4_refusal(ip),
        Host::Ipv6(ip) => ipv6_refusal(ip),
    }
}

/// Every rule the spec's table rules out, over the whole row, so create and patch refuse the same things.
// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn validate(state: &AdminState, row: &AlertRuleRow) -> Result<(), Response> {
    let bad = |m: &str| Err(failure(StatusCode::BAD_REQUEST, m));
    let Some(kind) = Kind::parse(&row.kind) else {
        return bad("kind is one of the eight alert kinds");
    };
    let Ok(scope) = serde_json::from_value::<ScopeType>(Value::String(row.scope_type.clone()))
    else {
        return bad("scope.type is account, all, any_account, key, or pool");
    };
    if !allowed_scopes(kind).contains(&scope) {
        return bad("this kind does not take that scope");
    }
    match (scope, &row.scope_id) {
        (ScopeType::All | ScopeType::AnyAccount, Some(_)) => return bad("this scope takes no id"),
        (ScopeType::All | ScopeType::AnyAccount, None) => {}
        (_, None) => return bad("this scope needs an id"),
        (ScopeType::Account, Some(id)) => {
            if !matches!(state.store.account(id), Ok(Some(_))) {
                return bad("scope.id names no account");
            }
        }
        (ScopeType::Key, Some(id)) => {
            if !matches!(state.store.api_key(id), Ok(Some(_))) {
                return bad("scope.id names no key");
            }
        }
        (ScopeType::Pool, Some(id)) => {
            if !matches!(state.store.pool(id), Ok(Some(_))) {
                return bad("scope.id names no pool");
            }
        }
    }
    if kind == Kind::UtilizationAbove {
        if !row.threshold.is_some_and(|t| (0.0..=1.0).contains(&t)) {
            return bad("utilization_above needs a threshold between 0 and 1");
        }
        if !matches!(
            row.window.as_deref(),
            Some("5h") | Some("7d") | Some("7d_oi")
        ) {
            return bad("utilization_above needs a window of 5h, 7d, or 7d_oi");
        }
    } else if row.threshold.is_some() || row.window.is_some() {
        return bad("only utilization_above takes a threshold and a window");
    }
    let url = match Url::parse(&row.webhook_url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => url,
        _ => return bad("webhook_url must be an absolute http or https URL"),
    };
    match destination_refusal(&url) {
        Some(reason) if !state.allow_local_webhooks => {
            bad(&format!("webhook_url must not point at {reason}"))
        }
        _ => Ok(()),
    }
}

fn apply(row: &mut AlertRuleRow, input: &RuleInput, now: i64) {
    if let Some(kind) = &input.kind {
        row.kind = kind.clone();
    }
    if let Some(scope) = &input.scope {
        row.scope_type = scope.kind.clone();
        row.scope_id = scope.id.clone();
    }
    if let Some(t) = input.threshold {
        row.threshold = t;
    }
    if let Some(w) = &input.window {
        row.window = w.clone();
    }
    if let Some(u) = &input.webhook_url {
        row.webhook_url = u.clone();
    }
    if let Some(e) = input.enabled {
        row.enabled = e;
    }
    row.updated_at = now;
}

/// `None` leaves the secret, `Some(None)` clears it, `Some(Some(s))` stores `s`: an empty string is the clear.
fn secret_of(input: &RuleInput) -> Option<Option<Secret>> {
    input
        .secret
        .as_deref()
        .map(|s| (!s.is_empty()).then(|| Secret::new(s)))
}

/// The write already landed, so a failed reload gives the caller nothing to retry: the engine goes on matching
/// against the rules it last read, and only the log can tell an operator so.
fn reload_engine(state: &AdminState) {
    if let Err(error) = state.alerts.reload() {
        tracing::warn!(%error, "the alert rules were written but the engine could not re-read them");
    }
}

pub(crate) async fn list_rules(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.alert_rules() {
        Ok(rows) => {
            Json(json!({ "rules": rows.iter().map(rule_json).collect::<Vec<_>>() })).into_response()
        }
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn create_rule(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let Ok(input) = serde_json::from_slice::<RuleInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let now = (state.clock)();
    let mut row = AlertRuleRow {
        id: format!("rule_{:016x}", rand::random::<u64>()),
        kind: String::new(),
        scope_type: String::new(),
        scope_id: None,
        threshold: None,
        window: None,
        webhook_url: String::new(),
        has_secret: false,
        enabled: true,
        created_at: now,
        updated_at: now,
    };
    apply(&mut row, &input, now);
    if let Err(refusal) = validate(&state, &row) {
        return refusal;
    }
    let secret = secret_of(&input);
    if let Err(e) = state
        .store
        .upsert_alert_rule(&row, secret.as_ref().map(|s| s.as_ref()), ACTOR)
    {
        return store_failure(e);
    }
    reload_engine(&state);
    match state.store.alert_rule(&row.id) {
        Ok(Some(row)) => (StatusCode::CREATED, Json(rule_json(&row))).into_response(),
        Ok(None) => failure(StatusCode::INTERNAL_SERVER_ERROR, "the rule was not stored"),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn patch_rule(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<RuleInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let mut row = match state.store.alert_rule(&id) {
        Ok(Some(r)) => r,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "alert rule not found"),
        Err(e) => return store_failure(e),
    };
    // An open event keeps the kind it opened under while `finish` posts its resolve under the rule's, so a
    // kind that moved out from under an open event would deliver an open and a resolve that disagree.
    if input.kind.as_deref().is_some_and(|kind| kind != row.kind) {
        let open = match state.store.open_alert_events() {
            Ok(events) => events.iter().filter(|e| e.rule_id == id).count(),
            Err(e) => return store_failure(e),
        };
        if open > 0 {
            let subject = if open == 1 { "event is" } else { "events are" };
            return failure(
                StatusCode::CONFLICT,
                &format!("the condition cannot change while {open} {subject} open for this rule"),
            );
        }
    }
    apply(&mut row, &input, (state.clock)());
    if let Err(refusal) = validate(&state, &row) {
        return refusal;
    }
    let secret = secret_of(&input);
    if let Err(e) = state
        .store
        .upsert_alert_rule(&row, secret.as_ref().map(|s| s.as_ref()), ACTOR)
    {
        return store_failure(e);
    }
    reload_engine(&state);
    match state.store.alert_rule(&id) {
        Ok(Some(row)) => Json(rule_json(&row)).into_response(),
        Ok(None) => failure(StatusCode::NOT_FOUND, "alert rule not found"),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn delete_rule(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_alert_rule(&id, ACTOR) {
        Ok(()) => {
            reload_engine(&state);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(StoreError::AlertRuleNotFound(_)) => {
            failure(StatusCode::NOT_FOUND, "alert rule not found")
        }
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn test_rule(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    if !matches!(state.store.alert_rule(&id), Ok(Some(_))) {
        return failure(StatusCode::NOT_FOUND, "alert rule not found");
    }
    Json(state.alerts.test(&id).await).into_response()
}

pub(crate) async fn list_events(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let filter = match q.get("state").map(String::as_str) {
        None => None,
        Some(s @ ("open" | "resolved")) => Some(s),
        Some(_) => return failure(StatusCode::BAD_REQUEST, "state is open or resolved"),
    };
    let rows = match state.store.alert_events(filter, EVENT_LIMIT) {
        Ok(r) => r,
        Err(e) => return store_failure(e),
    };
    let mut events = Vec::with_capacity(rows.len());
    for row in &rows {
        match event_json(&state, caller.role, row) {
            Ok(v) => events.push(v),
            Err(e) => return store_failure(e),
        }
    }
    Json(json!({ "events": events })).into_response()
}

pub(crate) async fn list_deliveries(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    if !matches!(state.store.alert_event(&id), Ok(Some(_))) {
        return failure(StatusCode::NOT_FOUND, "alert event not found");
    }
    match state.store.alert_deliveries(&id) {
        Ok(rows) => {
            Json(json!({ "deliveries": rows.iter().map(delivery_json).collect::<Vec<_>>() }))
                .into_response()
        }
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn resolve_event(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    match state.store.alert_event(&id) {
        Ok(Some(e)) if e.state != "open" => {
            return failure(StatusCode::CONFLICT, "the event is already resolved")
        }
        Ok(Some(_)) => {}
        Ok(None) => return failure(StatusCode::NOT_FOUND, "alert event not found"),
        Err(e) => return store_failure(e),
    }
    match state.alerts.resolve(&id) {
        Ok(true) => {}
        Ok(false) => return failure(StatusCode::CONFLICT, "the event is already resolved"),
        Err(e) => return store_failure(e),
    }
    match state.store.alert_event(&id) {
        Ok(Some(row)) => event_json(&state, caller.role, &row)
            .map(|v| Json(v).into_response())
            .unwrap_or_else(store_failure),
        Ok(None) => failure(StatusCode::NOT_FOUND, "alert event not found"),
        Err(e) => store_failure(e),
    }
}
