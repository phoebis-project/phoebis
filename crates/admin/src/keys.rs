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
use phoebis_store::{stamp_token, ApiKeyRow, AttachmentSubject, StoreError, ROTATION_GRACE_MS};
use serde_json::{json, Value};
use utoipa::ToSchema;

use crate::accounts::{check_slug, within_text_bound};
use crate::guardrails::{attachment_refusal, distinct_ids, AttachmentIndex, GuardrailRef};
use crate::openapi::{ErrorBody, RegistryUnavailable, StorageFailure};
use crate::routes::{
    double_option, failure, failure_with_code, fence_token, parse_object, present,
    reload_or_refuse, store_failure, CreatedId,
};
use crate::rules::{parse_mode, parse_rules, RuleInput};
use crate::AdminState;

const ACTOR: &str = "admin";
const STALE_EDIT: &str = "the key changed while it was being edited, so reload and try again";

/// The key's fields. On a create `id`, `label` and `pool_id` are required. On an edit any subset but `id`. An unknown
/// field in the body is refused.
#[derive(serde::Deserialize, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyInput {
    /// `[a-z0-9][a-z0-9-]{0,63}`.
    #[serde(default)]
    id: Option<String>,
    /// At most 256 bytes.
    #[serde(default)]
    label: Option<String>,
    /// An existing pool.
    #[serde(default)]
    pool_id: Option<String>,
    /// At most 256 bytes, or null to clear.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, nullable)]
    notes: Option<Option<String>>,
    #[serde(default)]
    enabled: Option<bool>,
    /// Unix milliseconds, or null to clear.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<i64>, nullable)]
    expires_at: Option<Option<i64>>,
    /// An ordered list of `{pattern, not}`, at most 256.
    #[serde(default)]
    model_rules: Option<Vec<RuleInput>>,
    /// `extend` or `replace`.
    #[serde(default)]
    #[schema(example = "extend")]
    model_rules_mode: Option<String>,
    /// Default `true`.
    #[serde(default)]
    long_context_twins: Option<bool>,
    #[serde(default)]
    keepalive_opt_out: Option<bool>,
    /// At most `stats.body_capture_max_hours`, or null to inherit.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<u32>, nullable)]
    body_capture_hours: Option<Option<u32>>,
    /// `true`, `false`, or null to inherit the pool's, default null.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<bool>, nullable)]
    auto_mode_server: Option<Option<Value>>,
    /// The key's own guardrail ids in the order it runs them after its pool's, replacing the key's list whole, on an
    /// edit alone: a new key starts with none of its own, so a create refuses the field.
    #[serde(default)]
    guardrails: Option<Vec<String>>,
    /// Optional, the `revision` the row was read at. The write lands only while the key still carries it, and a body
    /// without it fences on the route's own read. A malformed value answers `400`, a stale one `409` under
    /// `stale_edit`. A create accepts it and ignores it, since a create has no row to fence on. `revision` names one
    /// state of one key: an edit, a rotation or a revocation moves it, and a key deleted and created again under the
    /// same id never answers a token its predecessor did, so an edit read before the deletion answers `409` and
    /// writes nothing.
    // The raw value, kept apart from an absent field, so a present null, a string or a fraction is refused with
    // its own sentence rather than read as "not sent" or refused with the body's.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = u64)]
    expected_revision: Option<Value>,
}

/// The keys, one row each.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyList {
    pub keys: Vec<Key>,
}

/// A key as the registry and the ledger see it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub id: String,
    pub label: String,
    pub notes: Option<String>,
    pub pool_id: String,
    pub enabled: bool,
    pub revoked_at: Option<i64>,
    pub expires_at: Option<i64>,
    #[schema(value_type = Vec<crate::openapi::ModelRuleView>)]
    pub model_rules: Vec<phoebis_store::ModelRule>,
    /// `extend` or `replace`.
    #[schema(example = "extend")]
    pub model_rules_mode: String,
    /// Default `true`.
    pub long_context_twins: bool,
    pub keepalive_opt_out: bool,
    pub body_capture_hours: Option<u32>,
    /// Falls back to the pool's value when the key's own is unset.
    pub effective_body_capture_hours: u32,
    /// `true`, `false`, or null.
    pub auto_mode_server: Option<bool>,
    /// The key's own where set, else the pool's.
    pub effective_auto_mode_server: bool,
    pub prefix: String,
    pub created_at: i64,
    /// The key's last create, edit, rotation or revocation. A use never moves it.
    pub updated_at: i64,
    /// The row's edit token, which `PATCH /admin/keys/{id}` takes back as `expected_revision`.
    pub revision: u64,
    pub rotated_at: Option<i64>,
    /// A client request presenting the key. A keepalive probe never moves it.
    pub last_used_at: Option<i64>,
    pub retired_secret_last_used_at: Option<i64>,
    /// Live sessions whose newest request presented the secret the last rotation retired, 0 once the 15-minute
    /// grace ends, and a session the keepalive mod registered that sent no request since counts as switched.
    pub sessions_on_retired_secret: usize,
    /// The key's own guardrails in position order, which it runs after its pool's. One its pool holds too runs once,
    /// at the pool's position.
    pub guardrails: Vec<GuardrailRef>,
    /// The pool's guardrails in the pool's order, which the key runs first and cannot drop.
    pub pool_guardrails: Vec<GuardrailRef>,
    pub usage_7d: KeyUsage,
    pub lifetime: KeyLifetime,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyUsage {
    /// The dollars of the billed rows alone (fast mode, overage, API key accounts and usage-based seats), a part of
    /// `cost_usd`.
    pub billed_usd: f64,
    pub cost_usd: f64,
    /// The plan-funded rows as the Pro-week figure, whatever tier served them, null where a row priced nothing and 0
    /// with no plan-funded row.
    pub equivalent: Option<f64>,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyLifetime {
    /// The dollars of the billed rows alone (fast mode, overage, API key accounts and usage-based seats), a part of
    /// `cost_usd`.
    pub billed_usd: f64,
    pub cost_usd: f64,
    /// The plan-funded rows as the Pro-week figure, whatever tier served them, null where a row priced nothing and 0
    /// with no plan-funded row.
    pub equivalent: Option<f64>,
    pub requests: u64,
}

/// The key's fields and `secret`, the one answer that carries the secret.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyCreated {
    pub id: String,
    pub label: String,
    pub notes: Option<String>,
    pub pool_id: String,
    pub enabled: bool,
    pub revoked_at: Option<i64>,
    pub expires_at: Option<i64>,
    #[schema(value_type = Vec<crate::openapi::ModelRuleView>)]
    pub model_rules: Vec<phoebis_store::ModelRule>,
    #[schema(example = "extend")]
    pub model_rules_mode: String,
    pub long_context_twins: bool,
    pub keepalive_opt_out: bool,
    pub body_capture_hours: Option<u32>,
    pub effective_body_capture_hours: u32,
    pub auto_mode_server: Option<bool>,
    pub effective_auto_mode_server: bool,
    pub prefix: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub revision: u64,
    pub rotated_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub retired_secret_last_used_at: Option<i64>,
    pub sessions_on_retired_secret: usize,
    pub guardrails: Vec<GuardrailRef>,
    pub pool_guardrails: Vec<GuardrailRef>,
    pub usage_7d: KeyUsage,
    pub lifetime: KeyLifetime,
    /// The secret, issued once.
    pub secret: String,
}

fn new_secret() -> String {
    let mut bytes = [0u8; 32];
    phoebis_oauth::random::fill(&mut bytes);
    format!(
        "{}{}",
        phoebis_store::KEY_SECRET_PREFIX,
        URL_SAFE_NO_PAD.encode(bytes)
    )
}

// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
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
    let to = crate::analytics::hour_end(hour);
    let week_start = crate::lifetime::week_start(hour);
    let live = phoebis_ledger::LiveKeys::from_snapshot(&state.router.snapshot());
    let prices = state.pricing.revisions();
    let tables = state.tables.clone();
    let read = async {
        let old = state
            .old_totals
            .keys
            .get((week_start, live.0.clone()), || {
                let (live, prices, tables) = (live.clone(), prices.clone(), tables.clone());
                state.ledger.db().read(move |c| {
                    phoebis_ledger::key_request_totals(
                        c,
                        i64::MIN,
                        week_start,
                        &live,
                        &prices,
                        &tables,
                        now,
                    )
                })
            })
            .await?;
        let week = state
            .ledger
            .db()
            .read(move |c| {
                phoebis_ledger::key_request_totals(c, week_start, to, &live, &prices, &tables, now)
            })
            .await?;
        Ok::<_, phoebis_ledger::LedgerError>((old, week))
    }
    .await;
    let (old, week) = match read {
        Ok(totals) => totals,
        Err(error) => {
            tracing::warn!(%error, "the key view could not read the ledger, so usage and lifetime totals are empty");
            Default::default()
        }
    };
    let lifetime = crate::lifetime::lifetime_of(old, &week);
    (week, lifetime)
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
            tracing::warn!(%error, "the key view could not read session activity, so retired-secret counts are empty");
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

/// List the keys.
#[utoipa::path(
    get,
    path = "/admin/keys",
    operation_id = "keys_list",
    tag = "Keys",
    responses(
        (status = 200, description = "The keys.", body = KeyList),
        (status = 500, response = StorageFailure),
    ),
)]
pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.api_keys() {
        Ok(rows) => {
            let (_, index) = match AttachmentIndex::read(&state.store) {
                Ok(read) => read,
                Err(response) => return response,
            };
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
                out.push(key_json(&snapshot, &row, &week, &lifetime, count, &index));
            }
            Json(KeyList { keys: out }).into_response()
        }
        Err(error) => store_failure(error),
    }
}

/// Create a key.
#[utoipa::path(
    post,
    path = "/admin/keys",
    operation_id = "keys_create",
    tag = "Keys",
    request_body(content = KeyInput, content_type = "application/json"),
    responses(
        (status = 201, description = "The created key.", body = KeyCreated),
        (status = 400, description = "- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `id, label, and pool_id are required`\n- `id must match [a-z0-9][a-z0-9-]{0,63}`\n- `label is at most 256 bytes`\n- `notes is at most 256 bytes`\n- `body_capture_hours exceeds stats.body_capture_max_hours`\n- `auto_mode_server must be true, false, or null`\n- `model_rules holds at most 256 entries`\n- `model_rules[{i}]: {reason}`: the entry's index and the grammar's own text\n- `model_rules_mode is extend or replace`\n- `guardrails applies on edit`: a new key starts with none of its own", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "`pool not found`", body = ErrorBody, example = json!({"error": "pool not found"})),
        (status = 409, description = "`a key with this id exists`", body = ErrorBody, example = json!({"error": "a key with this id exists"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let input: KeyInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
    };
    // A new key starts with none of its own, and its pool's list never passes the bound, so a create counts nothing.
    if input.guardrails.is_some() {
        return failure(StatusCode::BAD_REQUEST, "guardrails applies on edit");
    }
    let (Some(id), Some(_), Some(_)) = (&input.id, &input.label, &input.pool_id) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "id, label, and pool_id are required",
        );
    };
    if let Err(refusal) = check_slug(id) {
        return refusal;
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
        // The check above reads outside the insert, so a create that lost the race for the id arrives here.
        Err(StoreError::IdTaken(_)) => {
            return failure(StatusCode::CONFLICT, "a key with this id exists")
        }
        Err(error) => return store_failure(error),
    }
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = match reload_or_refuse(&state).await {
        Err(refusal) => refusal,
        Ok(()) => match AttachmentIndex::read(&state.store) {
            Ok((_, index)) => {
                let (week, lifetime) = totals(&state).await;
                // A key this route just created has never rotated, so no session can present a retired secret of it.
                let key = key_json(&state.router.snapshot(), &row, &week, &lifetime, 0, &index);
                let mut v = serde_json::to_value(key).expect("a key serializes");
                v["secret"] = json!(secret);
                (StatusCode::CREATED, Json(v)).into_response()
            }
            Err(response) => response,
        },
    };
    response.extensions_mut().insert(CreatedId(row.id));
    response
}

/// Edit a key.
#[utoipa::path(
    patch,
    path = "/admin/keys/{id}",
    operation_id = "key_patch",
    tag = "Keys",
    params(("id" = String, Path, description = "The key's id")),
    request_body(content = KeyInput, content_type = "application/json"),
    responses(
        (status = 200, description = "The key's fields, the same shape as `POST /admin/keys` without `secret`.", body = Key),
        (status = 400, description = "- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `expected_revision is the revision the row was read at`: a `null`, or a value that is not a non-negative integer\n- `label is at most 256 bytes`\n- `notes is at most 256 bytes`\n- `body_capture_hours exceeds stats.body_capture_max_hours`\n- `auto_mode_server must be true, false, or null`\n- `model_rules holds at most 256 entries`\n- `model_rules[{i}]: {reason}`: the entry's index and the grammar's own text\n- `model_rules_mode is extend or replace`\n- `guardrails lists {id} twice`\n- `key {id} would hold more than 64 guardrails`, with nothing written, also where a `pool_id` change alone moves the key under a pool whose guardrails take it over the bound", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "- `key not found`\n- `pool not found`\n- `guardrail {id} not found`, with nothing written", body = ErrorBody, example = json!({"error": "key not found"})),
        // The header goes ahead of `example`: utoipa-gen 6 takes the comma after an example as its own, so an
        // attribute after it fails to parse.
        (status = 409, description = "- `a revoked key cannot be re-enabled`\n- `the key changed while it was being edited, so reload and try again`, which carries the `stale_edit` code",
            headers(("x-phoebis-error-code" = String, description = "`stale_edit`: the row changed since it was read")),
            body = ErrorBody, example = json!({"error": "a revoked key cannot be re-enabled"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let input: KeyInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
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
    if let Some(guardrails) = &input.guardrails {
        if let Err(refusal) = distinct_ids("guardrails", guardrails) {
            return refusal;
        }
    }
    if read_at != row.updated_at {
        return failure_with_code(StatusCode::CONFLICT, "stale_edit", STALE_EDIT);
    }
    match state.store.update_api_key_with_guardrails(
        &row,
        row.updated_at,
        input.guardrails.as_deref(),
        ACTOR,
    ) {
        Ok(updated_at) => row.updated_at = updated_at,
        Err(StoreError::Stale) => {
            return failure_with_code(StatusCode::CONFLICT, "stale_edit", STALE_EDIT)
        }
        Err(StoreError::KeyNotFound(_)) => return failure(StatusCode::NOT_FOUND, "key not found"),
        Err(StoreError::PoolNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "pool not found")
        }
        Err(StoreError::GuardrailNotFound(id)) => {
            return failure(StatusCode::NOT_FOUND, &format!("guardrail {id} not found"))
        }
        // The store counts the pool the row names, so a `pool_id` change alone is refused here too.
        Err(error @ StoreError::KeyGuardrailBound(_)) => return attachment_refusal(error),
        Err(error) => return store_failure(error),
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    let (_, index) = match AttachmentIndex::read(&state.store) {
        Ok(read) => read,
        Err(response) => return response,
    };
    let (week, lifetime) = totals(&state).await;
    let now = (state.clock)();
    let activity = recent_activity(&state, now).await;
    let count = sessions_on_retired_secret(&row, &activity, now);
    let key = key_json(
        &state.router.snapshot(),
        &row,
        &week,
        &lifetime,
        count,
        &index,
    );
    Json(key).into_response()
}

/// Rotate a key's secret.
#[utoipa::path(
    post,
    path = "/admin/keys/{id}/rotate",
    operation_id = "key_rotate",
    tag = "Keys",
    params(("id" = String, Path, description = "The key's id")),
    responses(
        (status = 200, description = "The key's fields and a new `secret`, issued the same way as `POST /admin/keys`. The previous secret keeps authenticating for 15 minutes after the rotation, so a running session can switch on its own time. A second rotation inside that window ends it at once, and a revocation ends it too. The key list counts the sessions still presenting the previous secret until the grace ends.", body = KeyCreated),
        (status = 404, description = "`key not found`", body = ErrorBody, example = json!({"error": "key not found"})),
        (status = 409, description = "`a revoked key cannot be rotated`", body = ErrorBody, example = json!({"error": "a revoked key cannot be rotated"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
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
            let (_, index) = match AttachmentIndex::read(&state.store) {
                Ok(read) => read,
                Err(response) => return response,
            };
            let (week, lifetime) = totals(&state).await;
            let activity = recent_activity(&state, now).await;
            let count = sessions_on_retired_secret(&row, &activity, now);
            let key = key_json(
                &state.router.snapshot(),
                &row,
                &week,
                &lifetime,
                count,
                &index,
            );
            let mut v = serde_json::to_value(key).expect("a key serializes");
            v["secret"] = json!(secret);
            Json(v).into_response()
        }
        Ok(None) => failure(StatusCode::NOT_FOUND, "key not found"),
        Err(e) => store_failure(e),
    }
}

/// Revoke a key.
#[utoipa::path(
    post,
    path = "/admin/keys/{id}/revoke",
    operation_id = "key_revoke",
    tag = "Keys",
    params(("id" = String, Path, description = "The key's id")),
    responses(
        (status = 204, description = "The key is revoked."),
        (status = 404, description = "`key not found`", body = ErrorBody, example = json!({"error": "key not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
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

/// Delete a key.
#[utoipa::path(
    delete,
    path = "/admin/keys/{id}",
    operation_id = "key_delete",
    tag = "Keys",
    params(("id" = String, Path, description = "The key's id")),
    responses(
        (status = 204, description = "The id is free to use again at once. The deleted key's usage stays under the key that spent it, filed by the key's creation time, so a later key under the same id never reads it, and the hourly rollups keep it for `stats.rollup_retention_days` as they keep everything."),
        (status = 404, description = "`key not found`", body = ErrorBody, example = json!({"error": "key not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
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
    index: &AttachmentIndex,
) -> Key {
    let week = week.get(&row.id);
    let lifetime = lifetime.get(&row.id);
    Key {
        id: row.id.clone(),
        label: row.label.clone(),
        notes: row.notes.clone(),
        pool_id: row.pool_id.clone(),
        enabled: row.enabled,
        revoked_at: row.revoked_at,
        expires_at: row.expires_at,
        model_rules: row.model_rules.clone(),
        model_rules_mode: row.model_rules_mode.as_str().to_string(),
        long_context_twins: row.long_context_twins,
        keepalive_opt_out: row.keepalive_opt_out,
        body_capture_hours: row.body_capture_hours,
        effective_body_capture_hours: snapshot.body_capture_hours(row),
        auto_mode_server: row.auto_mode_server,
        effective_auto_mode_server: snapshot.auto_mode_server(row),
        prefix: row.prefix.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        revision: stamp_token(row.incarnation, row.updated_at),
        rotated_at: row.rotated_at,
        last_used_at: row.last_used_at,
        retired_secret_last_used_at: row.retired_secret_last_used_at,
        sessions_on_retired_secret,
        guardrails: index.refs_for(&AttachmentSubject::Key(row.id.clone())),
        pool_guardrails: index.refs_for(&AttachmentSubject::Pool(row.pool_id.clone())),
        usage_7d: KeyUsage {
            billed_usd: week.map_or(0.0, |t| t.billed_usd),
            cost_usd: week.map_or(0.0, |t| t.cost_usd),
            equivalent: week.map_or(Some(0.0), |t| t.equivalent),
            requests: week.map_or(0, |t| t.requests),
            tokens: week.map_or(0, |t| t.tokens),
        },
        lifetime: KeyLifetime {
            billed_usd: lifetime.map_or(0.0, |t| t.billed_usd),
            cost_usd: lifetime.map_or(0.0, |t| t.cost_usd),
            equivalent: lifetime.map_or(Some(0.0), |t| t.equivalent),
            requests: lifetime.map_or(0, |t| t.requests),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::{fixture, round_trips, round_trips_each};
    use crate::openapi::ModelRuleView;

    use super::{Key, KeyCreated};

    #[test]
    fn every_model_rule_of_the_key_list_round_trips_through_its_mirror() {
        round_trips_each::<ModelRuleView>("keys", &["keys", "*", "model_rules", "*"]);
    }

    #[test]
    fn the_created_key_answer_round_trips_through_its_declared_type() {
        round_trips::<KeyCreated>("key_created");
    }

    #[test]
    fn the_created_key_is_a_key_with_the_secret_last() {
        let mut body = fixture("key_created");
        let object = body.as_object_mut().unwrap();
        assert_eq!(
            object.keys().next_back().map(String::as_str),
            Some("secret")
        );
        assert!(object.shift_remove("secret").is_some_and(|s| s.is_string()));
        let key: Key = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(serde_json::to_value(&key).unwrap(), body);
    }
}
