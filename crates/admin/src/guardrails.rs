//! Guardrail routes: the registry's guardrails with their attachments, edited from the guardrail's side here and
//! from the subject's side on the pool and key routes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_guardrails::{
    CompiledGuardrail, Condition, ConditionWords, GuardrailAction, GuardrailMode, MAX_CONDITIONS,
};
use phoebis_store::{
    stamp_token, AttachmentSubject, GuardrailRow, RegistryLoad, StoreError, GUARDRAIL_BOUND,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::accounts::within_text_bound;
use crate::openapi::{ConditionView, ErrorBody, RegistryUnavailable, StorageFailure};
use crate::pools::valid_slug;
use crate::routes::{
    failure, failure_with_code, fence_token, present, reload_or_refuse, store_failure, CreatedId,
};
use crate::AdminState;

const ACTOR: &str = "admin";
pub(crate) const STALE_EDIT: &str =
    "the guardrail changed while it was being edited, so reload and try again";

/// The ids the static routes of the same names take, so a guardrail under either could never be read back by id.
const RESERVED_IDS: [&str; 2] = ["presets", "test"];

/// The guardrail's fields. On a create `id`, `name` and `conditions` are required, `mode` defaults to `observe`,
/// `enabled` to `true`, `action` to `block` and `description` to empty. On an edit any subset but `id`. An absent
/// `pools` or `keys` leaves those attachments as they are, and an empty list detaches every one. An unknown field in
/// the body is refused.
#[derive(Deserialize, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailInput {
    /// `[a-z0-9][a-z0-9-]{0,63}`, neither `presets` nor `test`.
    #[serde(default)]
    id: Option<String>,
    /// At most 256 bytes, not empty, and never containing `prompt is too long` in any case, since the name is the
    /// part of a refusal's message the operator writes.
    #[serde(default)]
    name: Option<String>,
    /// At most 256 bytes, default empty.
    #[serde(default)]
    description: Option<String>,
    /// Default `true`. A disabled guardrail stays attached and is skipped.
    #[serde(default)]
    enabled: Option<bool>,
    /// `observe` or `enforce`, default `observe`.
    #[serde(default)]
    #[schema(example = "observe")]
    mode: Option<String>,
    /// `block`, the default.
    #[serde(default)]
    #[schema(example = "block")]
    action: Option<String>,
    /// One to sixteen, every one of which must hold for the guardrail to match. A refusal names a condition by its
    /// index in this list.
    #[serde(default)]
    conditions: Option<Vec<ConditionView>>,
    /// The pools the guardrail is attached to. A pool newly named takes the guardrail at the end of its list, a pool
    /// already attached keeps its place, and a pool left out is detached.
    #[serde(default)]
    pools: Option<Vec<String>>,
    /// The keys the guardrail is attached to, under the rule `pools` follows.
    #[serde(default)]
    keys: Option<Vec<String>>,
    /// Optional, the `revision` the row was read at, which the edit reads. A create accepts it and ignores it, since a
    /// create has no row to fence on. The write lands only while the guardrail still carries the `updated_at` the
    /// route read, or the `revision` the body sent here. A guardrail deleted and created again under the same id never
    /// answers a `revision` its predecessor did, so an edit read before the deletion answers `409` and writes nothing.
    // The raw value, kept apart from an absent field, so a present null, a string or a fraction is refused with
    // its own sentence rather than read as "not sent" or refused with the body's.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = u64)]
    expected_revision: Option<Value>,
}

/// A pool or key a guardrail is attached to.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachmentView {
    pub id: String,
    /// The pool's or the key's label at the read.
    pub label: String,
    /// The guardrail's place in that pool's or key's list, from 0.
    pub position: u32,
}

/// A guardrail on a pool's or a key's list.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailRef {
    pub id: String,
    pub name: String,
    /// `observe` or `enforce`.
    #[schema(example = "observe")]
    pub mode: String,
    pub enabled: bool,
    /// The guardrail's place in the list, from 0.
    pub position: u32,
}

/// A guardrail with its conditions and the pools and keys it is attached to.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Guardrail {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// `observe` or `enforce`.
    #[schema(example = "observe")]
    pub mode: String,
    /// `block`.
    #[schema(example = "block")]
    pub action: String,
    /// In the order they were written, the order a refusal and the dry run name them by.
    pub conditions: Vec<ConditionView>,
    /// In position order.
    pub pools: Vec<AttachmentView>,
    /// In position order.
    pub keys: Vec<AttachmentView>,
    /// The row's edit token, which `PATCH /admin/guardrails/{id}` takes back as `expected_revision`.
    pub revision: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The guardrails, ordered by id.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailList {
    pub guardrails: Vec<Guardrail>,
}

/// One registry load's guardrails and attachments indexed by subject and by guardrail, with the pools' and keys'
/// labels, so one answer reads the registry once however many rows it renders.
pub(crate) struct AttachmentIndex {
    guardrails: HashMap<String, GuardrailRow>,
    /// Each subject's guardrail ids with their positions, in position order.
    by_subject: HashMap<AttachmentSubject, Vec<(String, u32)>>,
    /// Each guardrail's subjects with its positions on them.
    by_guardrail: HashMap<String, Vec<(AttachmentSubject, u32)>>,
    labels: HashMap<AttachmentSubject, String>,
}

impl AttachmentIndex {
    pub(crate) fn from_load(load: &RegistryLoad) -> AttachmentIndex {
        let mut by_subject: HashMap<AttachmentSubject, Vec<(String, u32)>> = HashMap::new();
        let mut by_guardrail: HashMap<String, Vec<(AttachmentSubject, u32)>> = HashMap::new();
        for attachment in &load.guardrail_attachments {
            by_subject
                .entry(attachment.subject.clone())
                .or_default()
                .push((attachment.guardrail_id.clone(), attachment.position));
            by_guardrail
                .entry(attachment.guardrail_id.clone())
                .or_default()
                .push((attachment.subject.clone(), attachment.position));
        }
        for list in by_subject.values_mut() {
            list.sort_by_key(|(_, position)| *position);
        }
        let labels = load
            .pools
            .iter()
            .map(|pool| (AttachmentSubject::Pool(pool.id.clone()), pool.label.clone()))
            .chain(
                load.keys
                    .iter()
                    .map(|key| (AttachmentSubject::Key(key.id.clone()), key.label.clone())),
            )
            .collect();
        AttachmentIndex {
            guardrails: load
                .guardrails
                .iter()
                .map(|row| (row.id.clone(), row.clone()))
                .collect(),
            by_subject,
            by_guardrail,
            labels,
        }
    }

    /// The subject's guardrails in position order.
    pub(crate) fn refs_for(&self, subject: &AttachmentSubject) -> Vec<GuardrailRef> {
        self.by_subject
            .get(subject)
            .into_iter()
            .flatten()
            .filter_map(|(id, position)| {
                self.guardrails.get(id).map(|row| GuardrailRef {
                    id: row.id.clone(),
                    name: row.name.clone(),
                    mode: row.mode.as_str().to_string(),
                    enabled: row.enabled,
                    position: *position,
                })
            })
            .collect()
    }

    /// The guardrail's subjects of one kind, `pool` or `key`, in position order and by id within a position.
    pub(crate) fn views_for(&self, guardrail_id: &str, kind: &str) -> Vec<AttachmentView> {
        let mut views: Vec<AttachmentView> = self
            .by_guardrail
            .get(guardrail_id)
            .into_iter()
            .flatten()
            .filter(|(subject, _)| subject.kind() == kind)
            .map(|(subject, position)| AttachmentView {
                id: subject.id().to_string(),
                label: self.labels.get(subject).cloned().unwrap_or_default(),
                position: *position,
            })
            .collect();
        views.sort_by(|a, b| (a.position, &a.id).cmp(&(b.position, &b.id)));
        views
    }
}

pub(crate) fn guardrail_json(row: &GuardrailRow, index: &AttachmentIndex) -> Guardrail {
    Guardrail {
        id: row.id.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        enabled: row.enabled,
        mode: row.mode.as_str().to_string(),
        action: row.action.as_str().to_string(),
        conditions: row.conditions.iter().map(ConditionView::from).collect(),
        pools: index.views_for(&row.id, "pool"),
        keys: index.views_for(&row.id, "key"),
        revision: stamp_token(row.incarnation, row.updated_at),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// `Condition::from_words`, `check` and `CompiledGuardrail::compile` over the list, the refusal `conditions[{i}]:
/// {reason}` or `conditions holds 1 to 16 entries`. The compiled form is dropped, so a row never holds a pattern the
/// router's build could not compile.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn validate_conditions(views: &[ConditionView]) -> Result<Vec<Condition>, Response> {
    if views.is_empty() || views.len() > MAX_CONDITIONS {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "conditions holds 1 to 16 entries",
        ));
    }
    let mut conditions = Vec::with_capacity(views.len());
    for (i, view) in views.iter().enumerate() {
        let condition = Condition::from_words(&ConditionWords::from(view))
            .and_then(|condition| condition.check().map(|_| condition))
            .map_err(|reason| {
                failure(
                    StatusCode::BAD_REQUEST,
                    &format!("conditions[{i}]: {reason}"),
                )
            })?;
        conditions.push(condition);
    }
    CompiledGuardrail::compile(
        "",
        "",
        GuardrailMode::Observe,
        GuardrailAction::Block,
        &conditions,
    )
    .map_err(|error| failure(StatusCode::BAD_REQUEST, &error.to_string()))?;
    Ok(conditions)
}

/// `{field} lists {id} twice` over a subject or guardrail list.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn distinct_ids(field: &str, ids: &[String]) -> Result<(), Response> {
    let mut seen = HashSet::new();
    match ids.iter().find(|id| !seen.insert(id.as_str())) {
        Some(repeat) => Err(failure(
            StatusCode::BAD_REQUEST,
            &format!("{field} lists {repeat} twice"),
        )),
        None => Ok(()),
    }
}

/// The name's bound, an empty name, and the phrase the client reads in a `400` as its compaction trigger: a
/// guardrail's refusal carries the name in its message, and the name is the one part of it the operator writes.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn validate_name(name: &str) -> Result<(), Response> {
    within_text_bound("name", Some(name))?;
    if name.is_empty() {
        return Err(failure(StatusCode::BAD_REQUEST, "name is required"));
    }
    if name.to_lowercase().contains("prompt is too long") {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "name must not contain \"prompt is too long\"",
        ));
    }
    Ok(())
}

/// Applies `input`'s present members but the conditions and the attachments over `row`, refusing every value the
/// rules rule out.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn apply(row: &mut GuardrailRow, input: &GuardrailInput) -> Result<(), Response> {
    if let Some(name) = &input.name {
        validate_name(name)?;
        row.name = name.clone();
    }
    if let Some(description) = &input.description {
        within_text_bound("description", Some(description))?;
        row.description = description.clone();
    }
    if let Some(enabled) = input.enabled {
        row.enabled = enabled;
    }
    if let Some(mode) = &input.mode {
        row.mode = GuardrailMode::parse(mode)
            .ok_or_else(|| failure(StatusCode::BAD_REQUEST, "mode is observe or enforce"))?;
    }
    if let Some(action) = &input.action {
        row.action = GuardrailAction::parse(action)
            .ok_or_else(|| failure(StatusCode::BAD_REQUEST, "action is block"))?;
    }
    Ok(())
}

/// `pools lists {id} twice` or `keys lists {id} twice` over the lists the body names.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn distinct_subjects(input: &GuardrailInput) -> Result<(), Response> {
    if let Some(pools) = &input.pools {
        distinct_ids("pools", pools)?;
    }
    if let Some(keys) = &input.keys {
        distinct_ids("keys", keys)?;
    }
    Ok(())
}

/// A refused attachment write as the API words it: the store's display names a key `api key`, and the bound counts
/// inside the write, so the route maps its two errors rather than counting anything itself.
fn attachment_refusal(error: StoreError) -> Response {
    match error {
        StoreError::KeyGuardrailBound(id) => failure(
            StatusCode::BAD_REQUEST,
            &format!("key {id} would hold more than {GUARDRAIL_BOUND} guardrails"),
        ),
        StoreError::KeyNotFound(id) => {
            failure(StatusCode::NOT_FOUND, &format!("key {id} not found"))
        }
        StoreError::PoolGuardrailBound(id) => failure(
            StatusCode::BAD_REQUEST,
            &format!("pool {id} would hold more than {GUARDRAIL_BOUND} guardrails"),
        ),
        StoreError::PoolNotFound(id) => {
            failure(StatusCode::NOT_FOUND, &format!("pool {id} not found"))
        }
        error => store_failure(error),
    }
}

/// The reload, then the guardrail as the registry holds it after the write, with its attachments.
async fn publish(state: &AdminState, id: &str, status: StatusCode) -> Response {
    if let Err(refusal) = reload_or_refuse(state).await {
        return refusal;
    }
    let load = match state.store.load_registry() {
        Ok(load) => load,
        Err(error) => return store_failure(error),
    };
    let index = AttachmentIndex::from_load(&load);
    match load.guardrails.iter().find(|row| row.id == id) {
        Some(row) => (status, Json(guardrail_json(row, &index))).into_response(),
        None => failure(StatusCode::NOT_FOUND, "guardrail not found"),
    }
}

/// List the guardrails.
#[utoipa::path(
    get,
    path = "/admin/guardrails",
    operation_id = "guardrails_list",
    tag = "Guardrails",
    responses(
        (status = 200, description = "The guardrails, ordered by id.", body = GuardrailList),
        (status = 500, response = StorageFailure),
    ),
)]
pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.load_registry() {
        Ok(load) => {
            let index = AttachmentIndex::from_load(&load);
            let guardrails = load
                .guardrails
                .iter()
                .map(|row| guardrail_json(row, &index))
                .collect();
            Json(GuardrailList { guardrails }).into_response()
        }
        Err(error) => store_failure(error),
    }
}

/// Create a guardrail.
#[utoipa::path(
    post,
    path = "/admin/guardrails",
    operation_id = "guardrails_create",
    tag = "Guardrails",
    request_body(content = GuardrailInput, content_type = "application/json"),
    responses(
        (status = 201, description = "The guardrail, the same shape as a row of `GET /admin/guardrails`.", body = Guardrail),
        (status = 400, description = "- `action is block`\n- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `conditions holds 1 to 16 entries`\n- `conditions[{i}]: {reason}`: the condition's index and the reason it is refused (`conditions[1]: value is required`)\n- `description is at most 256 bytes`\n- `id is required`\n- `id is reserved`: `presets` and `test`, which the routes beside `GET /admin/guardrails/{id}` take\n- `id must match [a-z0-9][a-z0-9-]{0,63}`\n- `key {id} would hold more than 64 guardrails`, with nothing written\n- `keys lists {id} twice`\n- `mode is observe or enforce`\n- `name is at most 256 bytes`\n- `name is required`\n- `name must not contain \"prompt is too long\"`\n- `pool {id} would hold more than 64 guardrails`, with nothing written\n- `pools lists {id} twice`", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "- `key {id} not found`, with nothing written\n- `pool {id} not found`, with nothing written", body = ErrorBody, example = json!({"error": "pool {id} not found"})),
        (status = 409, description = "`a guardrail with this id exists`", body = ErrorBody, example = json!({"error": "a guardrail with this id exists"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let Ok(input) = serde_json::from_slice::<GuardrailInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let Some(id) = &input.id else {
        return failure(StatusCode::BAD_REQUEST, "id is required");
    };
    if !valid_slug(id) {
        return failure(
            StatusCode::BAD_REQUEST,
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        );
    }
    if RESERVED_IDS.contains(&id.as_str()) {
        return failure(StatusCode::BAD_REQUEST, "id is reserved");
    }
    if let Ok(Some(_)) = state.store.guardrail(id) {
        return failure(StatusCode::CONFLICT, "a guardrail with this id exists");
    }
    let Some(name) = &input.name else {
        return failure(StatusCode::BAD_REQUEST, "name is required");
    };
    let now = (state.clock)();
    let mut row = GuardrailRow {
        id: id.clone(),
        name: name.clone(),
        description: String::new(),
        enabled: true,
        mode: GuardrailMode::Observe,
        action: GuardrailAction::Block,
        conditions: Vec::new(),
        preset_id: None,
        preset_version: None,
        incarnation: 1,
        created_at: now,
        updated_at: now,
    };
    if let Err(refusal) = apply(&mut row, &input) {
        return refusal;
    }
    match validate_conditions(input.conditions.as_deref().unwrap_or_default()) {
        Ok(conditions) => row.conditions = conditions,
        Err(refusal) => return refusal,
    }
    if let Err(refusal) = distinct_subjects(&input) {
        return refusal;
    }
    let pools = input.pools.as_deref().unwrap_or_default();
    let keys = input.keys.as_deref().unwrap_or_default();
    // The store takes the incarnation and the stamps inside the insert, so the answer reads the row back.
    if let Err(error) = state.store.insert_guardrail(&row, pools, keys, ACTOR) {
        return attachment_refusal(error);
    }
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = publish(&state, &row.id, StatusCode::CREATED).await;
    response.extensions_mut().insert(CreatedId(row.id));
    response
}

/// Read a guardrail.
#[utoipa::path(
    get,
    path = "/admin/guardrails/{id}",
    operation_id = "guardrail_show",
    tag = "Guardrails",
    params(("id" = String, Path, description = "The guardrail's id")),
    responses(
        (status = 200, description = "The guardrail, the same shape as a row of `GET /admin/guardrails`.", body = Guardrail),
        (status = 404, description = "`guardrail not found`", body = ErrorBody, example = json!({"error": "guardrail not found"})),
        (status = 500, response = StorageFailure),
    ),
)]
pub(crate) async fn show(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let row = match state.store.guardrail(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "guardrail not found"),
        Err(error) => return store_failure(error),
    };
    match state.store.load_registry() {
        Ok(load) => Json(guardrail_json(&row, &AttachmentIndex::from_load(&load))).into_response(),
        Err(error) => store_failure(error),
    }
}

/// Edit a guardrail.
#[utoipa::path(
    patch,
    path = "/admin/guardrails/{id}",
    operation_id = "guardrail_patch",
    tag = "Guardrails",
    params(("id" = String, Path, description = "The guardrail's id")),
    request_body(content = GuardrailInput, content_type = "application/json"),
    responses(
        (status = 200, description = "The guardrail, the same shape as a row of `GET /admin/guardrails`.", body = Guardrail),
        (status = 400, description = "- `action is block`\n- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `conditions holds 1 to 16 entries`\n- `conditions[{i}]: {reason}`: the condition's index and the reason it is refused (`conditions[1]: value is required`)\n- `description is at most 256 bytes`\n- `expected_revision is the revision the row was read at`: a `null`, or a value that is not a non-negative integer\n- `id cannot change`\n- `key {id} would hold more than 64 guardrails`, with nothing written\n- `keys lists {id} twice`\n- `mode is observe or enforce`\n- `name is at most 256 bytes`\n- `name is required`: an empty name\n- `name must not contain \"prompt is too long\"`\n- `pool {id} would hold more than 64 guardrails`, with nothing written\n- `pools lists {id} twice`", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "- `guardrail not found`\n- `key {id} not found`, with nothing written\n- `pool {id} not found`, with nothing written", body = ErrorBody, example = json!({"error": "guardrail not found"})),
        // The header goes ahead of `example`: utoipa-gen 6 takes the comma after an example as its own, so an
        // attribute after it fails to parse.
        (status = 409, description = "`the guardrail changed while it was being edited, so reload and try again`, which carries the `stale_edit` code",
            headers(("x-phoebis-error-code" = String, description = "`stale_edit`: the row changed since it was read")),
            body = ErrorBody, example = json!({"error": "the guardrail changed while it was being edited, so reload and try again"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<GuardrailInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let mut row = match state.store.guardrail(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "guardrail not found"),
        Err(error) => return store_failure(error),
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
    if input.id.is_some() {
        return failure(StatusCode::BAD_REQUEST, "id cannot change");
    }
    if let Err(refusal) = apply(&mut row, &input) {
        return refusal;
    }
    if let Some(conditions) = &input.conditions {
        match validate_conditions(conditions) {
            Ok(conditions) => row.conditions = conditions,
            Err(refusal) => return refusal,
        }
    }
    if let Err(refusal) = distinct_subjects(&input) {
        return refusal;
    }
    if read_at != row.updated_at {
        return failure_with_code(StatusCode::CONFLICT, "stale_edit", STALE_EDIT);
    }
    match state.store.update_guardrail(
        &row,
        row.updated_at,
        input.pools.as_deref(),
        input.keys.as_deref(),
        ACTOR,
    ) {
        Ok(_) => {}
        Err(StoreError::Stale) => {
            return failure_with_code(StatusCode::CONFLICT, "stale_edit", STALE_EDIT)
        }
        Err(StoreError::GuardrailNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "guardrail not found")
        }
        Err(error) => return attachment_refusal(error),
    }
    publish(&state, &row.id, StatusCode::OK).await
}

/// Delete a guardrail.
#[utoipa::path(
    delete,
    path = "/admin/guardrails/{id}",
    operation_id = "guardrail_delete",
    tag = "Guardrails",
    params(("id" = String, Path, description = "The guardrail's id")),
    responses(
        (status = 204, description = "The guardrail is deleted and detached from every pool and key."),
        (status = 404, description = "`guardrail not found`", body = ErrorBody, example = json!({"error": "guardrail not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn remove(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_guardrail(&id, ACTOR) {
        Ok(()) => match reload_or_refuse(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(refusal) => refusal,
        },
        Err(StoreError::GuardrailNotFound(_)) => {
            failure(StatusCode::NOT_FOUND, "guardrail not found")
        }
        Err(error) => store_failure(error),
    }
}
