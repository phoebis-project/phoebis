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
use phoebis_routing::RegistrySnapshot;
use phoebis_store::{stamp_token, ApiKeyRow, StoreError, ROTATION_GRACE_MS};
use rand::Rng;
use serde_json::{json, Value};

use crate::accounts::within_text_bound;
use crate::pools::valid_slug;
use crate::routes::{
    double_option, failure, failure_with_code, fence_token, present, reload_or_refuse,
    store_failure, CreatedId,
};
use crate::rules::{parse_mode, parse_rules, RuleInput};
use crate::AdminState;

const ACTOR: &str = "admin";
const STALE_EDIT: &str = "the key changed while it was being edited; reload and try again";

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
    #[serde(default)]
    model_rules: Option<Vec<RuleInput>>,
    #[serde(default)]
    model_rules_mode: Option<String>,
    #[serde(default)]
    long_context_twins: Option<bool>,
    #[serde(default)]
    keepalive_opt_out: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    body_capture_hours: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    auto_mode_server: Option<Option<Value>>,
    /// The raw value, kept apart from an absent field, so a present null, a string or a fraction is refused with
    /// its own sentence rather than read as "not sent" or refused with the body's. A create reads nothing of it,
    /// since there is no row to fence on.
    #[serde(default, deserialize_with = "present")]
    expected_revision: Option<Value>,
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
    within_text_bound("label", input.label.as_deref())?;
    within_text_bound("notes", input.notes.as_ref().and_then(Option::as_deref))?;
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
    if let Some(list) = &input.model_rules {
        row.model_rules = parse_rules(list)?;
    }
    if let Some(mode) = &input.model_rules_mode {
        row.model_rules_mode = parse_mode(mode)?;
    }
    if let Some(twins) = input.long_context_twins {
        row.long_context_twins = twins;
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
    if let Some(value) = &input.auto_mode_server {
        row.auto_mode_server = match value {
            None => None,
            Some(Value::Bool(on)) => Some(*on),
            Some(_) => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "auto_mode_server must be true, false, or null",
                ))
            }
        };
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
    let live = phoebis_ledger::LiveKeys::from_snapshot(&state.router.snapshot());
    let read = state
        .ledger
        .db()
        .read(move |c| {
            Ok((
                phoebis_ledger::key_request_totals(c, hour - 7 * 24 * 3_600, &live)?,
                // Lifetime: `i64::MIN` sits at or below the earliest hour the rollups can hold, so every row clears it.
                phoebis_ledger::key_request_totals(c, i64::MIN, &live)?,
            ))
        })
        .await;
    match read {
        Ok(totals) => totals,
        Err(error) => {
            tracing::warn!(%error, "the key view could not read the ledger; usage and lifetime totals are empty");
            Default::default()
        }
    }
}

/// The (key, generation) sessions the ledger has seen inside the console's own session window, read once
/// rather than once per key row.
async fn recent_activity(state: &AdminState, now: i64) -> Vec<phoebis_ledger::SessionActivity> {
    let cutoff = now - state.session_expiry.as_millis() as i64;
    match state
        .ledger
        .db()
        .read(move |c| phoebis_ledger::session_activity(c, cutoff))
        .await
    {
        Ok(activity) => activity,
        Err(error) => {
            tracing::warn!(%error, "the key view could not read session activity; retired-secret counts are empty");
            Vec::new()
        }
    }
}

/// Whether the row's rotation is still inside the grace the previous secret keeps authenticating for; a row
/// outside it, or never rotated, contributes nothing a session-activity read could answer.
fn in_rotation_grace(row: &ApiKeyRow, now: i64) -> bool {
    row.rotated_at
        .is_some_and(|at| now < at + ROTATION_GRACE_MS)
}

// Live sessions whose newest request presented the retired secret of this rotation, while its grace lasts;
// the console says how many sessions still need to switch before the old secret stops working. A session
// last seen on the secret an earlier rotation retired is not on this one: that secret no longer
// authenticates, so the session has already switched or gone.
fn sessions_on_retired_secret(
    row: &ApiKeyRow,
    activity: &[phoebis_ledger::SessionActivity],
    now: i64,
) -> usize {
    row.rotated_at
        .filter(|at| now < at + ROTATION_GRACE_MS)
        .map(|rotated_at| {
            activity
                .iter()
                .filter(|s| {
                    s.key_id == row.id
                        && s.key_generation == row.created_at
                        && s.on_retired_secret
                        && s.rotation_at == Some(rotated_at)
                })
                .count()
        })
        .unwrap_or(0)
}

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.api_keys() {
        Ok(rows) => {
            let (week, lifetime) = totals(&state).await;
            let now = (state.clock)();
            // Every count is 0 without a key inside its grace, so the grouped read is worth running only then.
            let activity = if rows.iter().any(|row| in_rotation_grace(row, now)) {
                recent_activity(&state, now).await
            } else {
                Vec::new()
            };
            let snapshot = state.router.snapshot();
            let mut out = Vec::new();
            for row in rows {
                let count = sessions_on_retired_secret(&row, &activity, now);
                out.push(key_json(&snapshot, &row, &week, &lifetime, count));
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
        model_rules: Vec::new(),
        model_rules_mode: phoebis_store::RulesMode::Extend,
        long_context_twins: true,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: state.store.api_key_digest(&secret).to_vec(),
        prefix: phoebis_store::key_prefix(&secret),
        incarnation: 1,
        created_at: now,
        updated_at: now,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
        auto_mode_server: None,
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    match state.store.insert_api_key(&row, ACTOR) {
        // The store stamps the generation, moving a create that lands inside the previous holder's millisecond
        // one past it, and takes the incarnation. An `<id>@<generation>` selector or a `revision` built from this
        // answer has to name what the store wrote.
        Ok(written) => row = written,
        Err(StoreError::PoolNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "pool not found")
        }
        Err(error) => return store_failure(error),
    }
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = match reload_or_refuse(&state).await {
        Err(refusal) => refusal,
        Ok(()) => {
            let (week, lifetime) = totals(&state).await;
            // A key this route just created has never rotated, so no session can present a retired secret of it.
            let mut v = key_json(&state.router.snapshot(), &row, &week, &lifetime, 0);
            v["secret"] = json!(secret);
            (StatusCode::CREATED, Json(v)).into_response()
        }
    };
    response.extensions_mut().insert(CreatedId(row.id));
    response
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
    let read_at = match fence_token(
        input.expected_revision.as_ref(),
        row.incarnation,
        row.updated_at,
        STALE_EDIT,
    ) {
        Ok(stamp) => stamp,
        Err(refusal) => return refusal,
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    if read_at != row.updated_at {
        return failure_with_code(StatusCode::CONFLICT, "stale_edit", STALE_EDIT);
    }
    match state.store.update_api_key(&row, row.updated_at, ACTOR) {
        Ok(updated_at) => row.updated_at = updated_at,
        Err(StoreError::Stale) => {
            return failure_with_code(StatusCode::CONFLICT, "stale_edit", STALE_EDIT)
        }
        Err(StoreError::KeyNotFound(_)) => return failure(StatusCode::NOT_FOUND, "key not found"),
        Err(StoreError::PoolNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "pool not found")
        }
        Err(error) => return store_failure(error),
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    let (week, lifetime) = totals(&state).await;
    let now = (state.clock)();
    let activity = recent_activity(&state, now).await;
    let count = sessions_on_retired_secret(&row, &activity, now);
    let v = key_json(&state.router.snapshot(), &row, &week, &lifetime, count);
    Json(v).into_response()
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
            let activity = recent_activity(&state, now).await;
            let count = sessions_on_retired_secret(&row, &activity, now);
            let mut v = key_json(&state.router.snapshot(), &row, &week, &lifetime, count);
            v["secret"] = json!(secret);
            Json(v).into_response()
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
    snapshot: &RegistrySnapshot,
    row: &ApiKeyRow,
    week: &BTreeMap<String, phoebis_ledger::Totals>,
    lifetime: &BTreeMap<String, phoebis_ledger::Totals>,
    sessions_on_retired_secret: usize,
) -> Value {
    json!({
        "id": row.id, "label": row.label, "notes": row.notes, "pool_id": row.pool_id, "enabled": row.enabled,
        "revoked_at": row.revoked_at, "expires_at": row.expires_at, "model_rules": row.model_rules,
        "model_rules_mode": row.model_rules_mode.as_str(), "long_context_twins": row.long_context_twins,
        "keepalive_opt_out": row.keepalive_opt_out, "body_capture_hours": row.body_capture_hours, "effective_body_capture_hours": snapshot.body_capture_hours(row),
        "auto_mode_server": row.auto_mode_server, "effective_auto_mode_server": snapshot.auto_mode_server(row),
        "prefix": row.prefix, "created_at": row.created_at, "updated_at": row.updated_at, "revision": stamp_token(row.incarnation, row.updated_at), "rotated_at": row.rotated_at, "last_used_at": row.last_used_at,
        "retired_secret_last_used_at": row.retired_secret_last_used_at, "sessions_on_retired_secret": sessions_on_retired_secret,
        "usage_7d": { "cost_usd": week.get(&row.id).map_or(0.0, |t| t.cost_usd), "requests": week.get(&row.id).map_or(0, |t| t.requests), "tokens": week.get(&row.id).map_or(0, |t| t.tokens) },
        "lifetime": { "cost_usd": lifetime.get(&row.id).map_or(0.0, |t| t.cost_usd), "requests": lifetime.get(&row.id).map_or(0, |t| t.requests) },
    })
}
