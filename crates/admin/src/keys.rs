//! Key routes: issue, edit, rotate, revoke, delete. The secret is shown in the create and rotate answers and
//! nowhere else.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use phoebis_store::{ApiKeyRow, StoreError};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::pools::valid_slug;
use crate::routes::{failure, reload_or_refuse, store_failure};
use crate::AdminState;

const ACTOR: &str = "admin";

#[derive(serde::Deserialize, Default)]
struct KeyInput {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    pool_id: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    notes: Option<Option<String>>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    expires_at: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    allowed_models: Option<Option<Vec<String>>>,
    #[serde(default)]
    keepalive_opt_out: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    body_capture_hours: Option<Option<u32>>,
}

fn double_option<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

fn new_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("phoebis-{}", URL_SAFE_NO_PAD.encode(bytes))
}

// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn apply(state: &AdminState, row: &mut ApiKeyRow, input: &KeyInput) -> Result<(), Response> {
    if let Some(label) = &input.label {
        row.label = label.clone();
    }
    if let Some(notes) = &input.notes {
        row.notes = notes.clone();
    }
    if let Some(pool_id) = &input.pool_id {
        match state.store.pool(pool_id) {
            Ok(Some(_)) => row.pool_id = pool_id.clone(),
            Ok(None) => return Err(failure(StatusCode::NOT_FOUND, "pool not found")),
            Err(e) => return Err(store_failure(e)),
        }
    }
    if let Some(enabled) = input.enabled {
        if enabled && row.revoked_at.is_some() {
            return Err(failure(
                StatusCode::CONFLICT,
                "a revoked key cannot be re-enabled",
            ));
        }
        row.enabled = enabled;
    }
    if let Some(expires_at) = input.expires_at {
        row.expires_at = expires_at;
    }
    if let Some(list) = &input.allowed_models {
        row.allowed_models = list
            .as_ref()
            .map(|l| l.iter().map(|m| phoebis_store::canonical_id(m)).collect());
    }
    if let Some(opt_out) = input.keepalive_opt_out {
        row.keepalive_opt_out = opt_out;
    }
    if let Some(hours) = input.body_capture_hours {
        if hours.is_some_and(|h| h > state.body_capture_max_hours) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "body_capture_hours exceeds stats.body_capture_max_hours",
            ));
        }
        row.body_capture_hours = hours;
    }
    Ok(())
}

/// The two ledger totals every key row shares, read once per call rather than once per key.
async fn totals(
    state: &AdminState,
) -> (
    BTreeMap<String, phoebis_ledger::Totals>,
    BTreeMap<String, phoebis_ledger::Totals>,
) {
    let now = (state.clock)();
    let hour = phoebis_ledger::hour_of(now);
    let read = state.ledger.db().read(|c| {
        Ok((
            phoebis_ledger::request_totals(c, "key_id", hour - 7 * 24 * 3_600)?,
            // Lifetime: `i64::MIN` sits at or below the earliest hour the rollups can hold, so every row clears it.
            phoebis_ledger::request_totals(c, "key_id", i64::MIN)?,
        ))
    });
    match read {
        Ok(totals) => totals,
        Err(error) => {
            tracing::warn!(%error, "the key view could not read the ledger; usage and lifetime totals are empty");
            Default::default()
        }
    }
}

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.api_keys() {
        Ok(rows) => {
            let (week, lifetime) = totals(&state).await;
            let mut out = Vec::new();
            for row in rows {
                match key_json(&state, &row, &week, &lifetime) {
                    Ok(v) => out.push(v),
                    Err(e) => return store_failure(e),
                }
            }
            Json(json!({ "keys": out })).into_response()
        }
        Err(error) => store_failure(error),
    }
}

pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let Ok(input) = serde_json::from_slice::<KeyInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let (Some(id), Some(_), Some(_)) = (&input.id, &input.label, &input.pool_id) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "id, label, and pool_id are required",
        );
    };
    if !valid_slug(id) {
        return failure(
            StatusCode::BAD_REQUEST,
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        );
    }
    if let Ok(Some(_)) = state.store.api_key(id) {
        return failure(StatusCode::CONFLICT, "a key with this id exists");
    }
    let secret = new_secret();
    let now = (state.clock)();
    let mut row = ApiKeyRow {
        id: id.clone(),
        label: String::new(),
        notes: None,
        pool_id: String::new(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        allowed_models: None,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: state.store.api_key_digest(&secret).to_vec(),
        prefix: phoebis_store::key_prefix(&secret),
        created_at: now,
        rotated_at: None,
        last_used_at: None,
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    match state.store.insert_api_key(&row, ACTOR) {
        Ok(()) => {}
        Err(StoreError::PoolNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "pool not found")
        }
        Err(error) => return store_failure(error),
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    let (week, lifetime) = totals(&state).await;
    match key_json(&state, &row, &week, &lifetime) {
        Ok(mut v) => {
            v["secret"] = json!(secret);
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<KeyInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let mut row = match state.store.api_key(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "key not found"),
        Err(e) => return store_failure(e),
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    if let Err(error) = state.store.update_api_key(&row, ACTOR) {
        return store_failure(error);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    let (week, lifetime) = totals(&state).await;
    match key_json(&state, &row, &week, &lifetime) {
        Ok(v) => Json(v).into_response(),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn rotate(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    let row = match state.store.api_key(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "key not found"),
        Err(e) => return store_failure(e),
    };
    if row.revoked_at.is_some() {
        return failure(StatusCode::CONFLICT, "a revoked key cannot be rotated");
    }
    let secret = new_secret();
    let now = (state.clock)();
    if let Err(error) = state.store.rotate_api_key(
        &id,
        &state.store.api_key_digest(&secret),
        &phoebis_store::key_prefix(&secret),
        now,
        ACTOR,
    ) {
        return store_failure(error);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match state.store.api_key(&id) {
        Ok(Some(row)) => {
            let (week, lifetime) = totals(&state).await;
            match key_json(&state, &row, &week, &lifetime) {
                Ok(mut v) => {
                    v["secret"] = json!(secret);
                    Json(v).into_response()
                }
                Err(e) => store_failure(e),
            }
        }
        Ok(None) => failure(StatusCode::NOT_FOUND, "key not found"),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn revoke(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.revoke_api_key(&id, (state.clock)(), ACTOR) {
        Ok(()) => match reload_or_refuse(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(refusal) => refusal,
        },
        Err(StoreError::KeyNotFound(_)) => failure(StatusCode::NOT_FOUND, "key not found"),
        Err(error) => store_failure(error),
    }
}

pub(crate) async fn remove(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_api_key(&id, ACTOR) {
        Ok(()) => match reload_or_refuse(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(refusal) => refusal,
        },
        Err(StoreError::KeyNotFound(_)) => failure(StatusCode::NOT_FOUND, "key not found"),
        Err(error) => store_failure(error),
    }
}

/// Never the digest and never the secret.
fn key_json(
    state: &AdminState,
    row: &ApiKeyRow,
    week: &BTreeMap<String, phoebis_ledger::Totals>,
    lifetime: &BTreeMap<String, phoebis_ledger::Totals>,
) -> Result<Value, StoreError> {
    let effective = row.body_capture_hours.unwrap_or(
        state
            .store
            .pool(&row.pool_id)?
            .map(|p| p.body_capture_hours)
            .unwrap_or(0),
    );
    Ok(json!({
        "id": row.id, "label": row.label, "notes": row.notes, "pool_id": row.pool_id, "enabled": row.enabled,
        "revoked_at": row.revoked_at, "expires_at": row.expires_at, "allowed_models": row.allowed_models,
        "keepalive_opt_out": row.keepalive_opt_out, "body_capture_hours": row.body_capture_hours, "effective_body_capture_hours": effective,
        "prefix": row.prefix, "created_at": row.created_at, "rotated_at": row.rotated_at, "last_used_at": row.last_used_at,
        "usage_7d": { "cost_usd": week.get(&row.id).map_or(0.0, |t| t.cost_usd), "requests": week.get(&row.id).map_or(0, |t| t.requests), "tokens": week.get(&row.id).map_or(0, |t| t.tokens) },
        "lifetime": { "cost_usd": lifetime.get(&row.id).map_or(0.0, |t| t.cost_usd), "requests": lifetime.get(&row.id).map_or(0, |t| t.requests) },
    }))
}
