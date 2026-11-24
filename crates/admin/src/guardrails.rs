//! Guardrail routes: the registry's guardrails with their attachments, edited from the guardrail's side here and
//! from the subject's side on the pool and key routes.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_bus::OTHER_CLIENT;
use phoebis_guardrails::{
    evaluate, parse_body, CompileError, CompiledGuardrail, Condition, ConditionWords,
    GuardrailAction, GuardrailMode, HintLookup, Preset, RequestFacts, Verdict, MAX_CONDITIONS,
};
use phoebis_ledger::{guardrail_sample_page, request_body, request_body_held, RequestCursor};
use phoebis_store::{
    stamp_token, AttachmentSubject, GuardrailAttachment, GuardrailRow, PresetStamp, RegistryLoad,
    Store, StoreError, SubjectKind, GUARDRAIL_BOUND,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::accounts::{check_slug, within_text_bound, ACTOR};
use crate::analytics::ledger_failure;
use crate::openapi::{ConditionView, ErrorBody, RegistryUnavailable, StorageFailure};
use crate::routes::{
    failure, fence_token, found, parse_object, present, reload_or_no_content, reload_or_refuse,
    stale_edit, store_failure, CreatedId,
};
use crate::AdminState;

pub(crate) const STALE_EDIT: &str =
    "the guardrail changed while it was being edited, so reload and try again";
const DUPLICATE_ID: &str = "a guardrail with this id exists";

/// The ids the static routes of the same names take, so a guardrail under either could never be read back by id.
const RESERVED_IDS: [&str; 2] = ["presets", "test"];

/// The guardrail's fields. On a create `id` is required, and `name` and `conditions` unless `preset` supplies them,
/// `mode` defaults to the preset's suggested mode or `observe`, `enabled` to `true`, `action` to `block` and
/// `description` to empty. On an edit any subset but `id` and `preset`. An absent `pools` or `keys` leaves those
/// attachments as they are, and an empty list detaches every one. An unknown field in the body is refused.
#[derive(Deserialize, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailInput {
    /// `[a-z0-9][a-z0-9-]{0,63}`, neither `presets` nor `test`.
    #[serde(default)]
    id: Option<String>,
    /// At most 256 bytes, not empty, and never containing `model:`, `OAuth token has been revoked` or
    /// `prompt is too long` in any case, since the name is the part of a refusal's message the operator writes and
    /// the client reads each of the three specially.
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
    /// On a create alone, the id of a preset from `GET /admin/guardrails/presets`. Its current version supplies
    /// `name`, `mode` and `conditions` where the body leaves them out, and the guardrail carries the preset's id and
    /// version whatever the body changed.
    #[serde(default)]
    #[schema(example = "subagent-status-summary")]
    preset: Option<String>,
    /// On an edit alone, of a guardrail created from a preset: a version this build ships of that preset, which the
    /// guardrail's `preset.version` moves to. The update to the current preset sends it with that version's
    /// `conditions`.
    #[serde(default)]
    preset_version: Option<u32>,
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
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachmentView {
    pub id: String,
    /// The pool's or the key's label at the read.
    pub label: String,
    /// The guardrail's place in that pool's or key's order. A guardrail detached through its own route or deleted
    /// leaves the others' values as they were, so the order can skip a value: sort by it and never index by it.
    pub position: u32,
}

/// A guardrail on a pool's or a key's list.
#[derive(Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailRef {
    pub id: String,
    pub name: String,
    /// `observe` or `enforce`.
    #[schema(example = "observe")]
    pub mode: String,
    pub enabled: bool,
    /// The guardrail's place in the order. A guardrail detached through its own route or deleted leaves the others'
    /// values as they were, so the list can skip a value: sort by it and never index by it.
    pub position: u32,
}

/// A guardrail with its conditions and the pools and keys it is attached to.
#[derive(Serialize, ToSchema)]
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
    /// The preset the guardrail was created from, null for one created without.
    pub preset: Option<GuardrailPresetState>,
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
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailList {
    pub guardrails: Vec<Guardrail>,
}

/// Where a guardrail came from and whether it still reads as the preset shipped it.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailPresetState {
    /// The preset's id.
    #[schema(example = "subagent-status-summary")]
    pub id: String,
    /// The version the guardrail was created from, or the one its last update took.
    pub version: u32,
    /// The highest version this build ships of the preset, above `version` where an update is available, or the
    /// stamped version where this build ships no preset of that id.
    pub current_version: u32,
    /// Whether the guardrail's conditions differ from the preset's at `version`.
    pub customized: bool,
}

/// A preset at its current version: the definition a guardrail created from it starts with.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailPresetView {
    #[schema(example = "subagent-status-summary")]
    pub id: String,
    pub version: u32,
    pub name: String,
    /// What the preset catches and what blocking it costs.
    pub description: String,
    /// The client versions the preset was checked against, as one line to show.
    pub measured_on: String,
    /// `observe` or `enforce`, the mode a guardrail created from the preset takes where the body names none.
    #[schema(example = "observe")]
    pub suggested_mode: String,
    /// In the order a guardrail created from the preset holds them.
    pub conditions: Vec<ConditionView>,
}

/// The presets this build ships, each at its current version, ordered by id.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresetList {
    pub presets: Vec<GuardrailPresetView>,
}

/// Rows per `read` call, so a console read waits for at most one page of the dry run.
pub const SAMPLE_PAGE: usize = 50;

/// A definition and the requests to test it on: `guardrail` or `conditions`, and `pool_id` or `key_id`. The mode and
/// the action play no part, since the test answers whether the definition matches. An unknown field in the body is
/// refused.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailTestInput {
    /// A saved guardrail's id, evaluated as stored whether enabled or not.
    #[serde(default)]
    #[schema(example = "subagent-status-summary")]
    guardrail: Option<String>,
    /// An unsaved definition, refused as a create refuses its conditions.
    #[serde(default)]
    conditions: Option<Vec<ConditionView>>,
    /// The pool whose requests are sampled, across every key of the pool.
    #[serde(default)]
    pool_id: Option<String>,
    /// The key whose requests are sampled.
    #[serde(default)]
    key_id: Option<String>,
    /// The most requests the sample holds, 1 to 500, default 200.
    #[serde(default)]
    limit: Option<u32>,
}

/// The requests a test read: the one subject it named, its limit, and how far back it reached.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailTestSample {
    /// The pool sampled, where the test named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub pool_id: Option<String>,
    /// The key sampled, where the test named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub key_id: Option<String>,
    pub limit: u32,
    /// The longest body capture a key may ask for, since no captured body is older.
    pub window_hours: u32,
}

/// The sample's verdicts counted.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailTestTotals {
    pub sampled: u32,
    pub matched: u32,
    pub unmatched: u32,
    pub indeterminate: u32,
}

/// One sampled request and what the definition answered on it.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailTestRequest {
    pub id: String,
    pub received_at: i64,
    /// `matched`, `unmatched` or `indeterminate`.
    #[schema(example = "unmatched")]
    pub verdict: String,
    /// The written index of the failing condition for `unmatched`, of the unknown one for `indeterminate`, null for
    /// `matched`.
    pub condition: Option<usize>,
}

/// The verdicts on the newest captured requests of a pool or a key. The answer carries ids and instants, never a
/// body's text.
#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuardrailTestResult {
    pub sample: GuardrailTestSample,
    pub totals: GuardrailTestTotals,
    /// Newest first.
    pub requests: Vec<GuardrailTestRequest>,
}

/// One store read's guardrails and attachments indexed by subject and by guardrail, with the pools' and keys' labels,
/// so one answer reads the registry once however many rows it renders.
pub(crate) struct AttachmentIndex {
    /// Each guardrail's name, mode and enabled flag, the members a subject's list renders.
    guardrails: HashMap<String, (String, GuardrailMode, bool)>,
    /// Each subject's guardrail ids with their positions, in position order.
    by_subject: HashMap<AttachmentSubject, Vec<(String, u32)>>,
    /// Each guardrail's subjects with its positions on them.
    by_guardrail: HashMap<String, Vec<(AttachmentSubject, u32)>>,
    labels: HashMap<AttachmentSubject, String>,
}

impl AttachmentIndex {
    pub(crate) fn from_load(load: &RegistryLoad) -> AttachmentIndex {
        let labels = load
            .pools
            .iter()
            .map(|pool| (AttachmentSubject::Pool(pool.id.clone()), pool.label.clone()))
            .chain(
                load.keys
                    .iter()
                    .map(|key| (AttachmentSubject::Key(key.id.clone()), key.label.clone())),
            );
        AttachmentIndex::from_parts(&load.guardrails, &load.guardrail_attachments, labels)
    }

    /// The guardrails with their index off one take of the store, a failed read answered as `store_failure`.
    #[allow(clippy::result_large_err)]
    pub(crate) fn read(store: &Store) -> Result<(Vec<GuardrailRow>, AttachmentIndex), Response> {
        let read = store.guardrail_index().map_err(store_failure)?;
        let index =
            AttachmentIndex::from_parts(&read.guardrails, &read.guardrail_attachments, read.labels);
        Ok((read.guardrails, index))
    }

    /// The index of the guardrails, their attachments and the subjects' labels, as `Store::guardrail_index` reads
    /// them for a list that needs no other part of the registry.
    fn from_parts(
        guardrails: &[GuardrailRow],
        attachments: &[GuardrailAttachment],
        labels: impl IntoIterator<Item = (AttachmentSubject, String)>,
    ) -> AttachmentIndex {
        let mut by_subject: HashMap<AttachmentSubject, Vec<(String, u32)>> = HashMap::new();
        let mut by_guardrail: HashMap<String, Vec<(AttachmentSubject, u32)>> = HashMap::new();
        for attachment in attachments {
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
        AttachmentIndex {
            guardrails: guardrails
                .iter()
                .map(|row| (row.id.clone(), (row.name.clone(), row.mode, row.enabled)))
                .collect(),
            by_subject,
            by_guardrail,
            labels: labels.into_iter().collect(),
        }
    }

    /// The subject's guardrails in position order.
    pub(crate) fn refs_for(&self, subject: &AttachmentSubject) -> Vec<GuardrailRef> {
        self.by_subject
            .get(subject)
            .into_iter()
            .flatten()
            .filter_map(|(id, position)| {
                self.guardrails
                    .get(id)
                    .map(|(name, mode, enabled)| GuardrailRef {
                        id: id.clone(),
                        name: name.clone(),
                        mode: mode.as_str().to_string(),
                        enabled: *enabled,
                        position: *position,
                    })
            })
            .collect()
    }

    /// The guardrail's subjects of one kind, `pool` or `key`, in position order and by id within a position.
    pub(crate) fn views_for(&self, guardrail_id: &str, kind: SubjectKind) -> Vec<AttachmentView> {
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
        preset: preset_state(row),
        pools: index.views_for(&row.id, SubjectKind::Pool),
        keys: index.views_for(&row.id, SubjectKind::Key),
        revision: stamp_token(row.incarnation, row.updated_at),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// `customized` is the stored conditions against `Preset::at(id, version)`'s, `current_version` the table's highest.
fn preset_state(row: &GuardrailRow) -> Option<GuardrailPresetState> {
    let PresetStamp { id, version } = row.preset.as_ref()?;
    let version = *version;
    let (current_version, customized) = match Preset::current(phoebis_guardrails::presets(), id) {
        Some(current) => (
            current.version,
            Preset::at(id, version).is_none_or(|stamped| stamped.conditions != row.conditions),
        ),
        // The table keeps every shipped preset, so this guards a row from a build that shipped one this build lacks.
        None => (version, true),
    };
    Some(GuardrailPresetState {
        id: id.clone(),
        version,
        current_version,
        customized,
    })
}

fn preset_view(preset: &Preset) -> GuardrailPresetView {
    GuardrailPresetView {
        id: preset.id.to_string(),
        version: preset.version,
        name: preset.name.to_string(),
        description: preset.description.to_string(),
        measured_on: preset.measured_on.to_string(),
        suggested_mode: preset.suggested_mode.as_str().to_string(),
        conditions: preset.conditions.iter().map(ConditionView::from).collect(),
    }
}

/// `Condition::from_words` over the list, then `CompiledGuardrail::compile`, which checks and compiles each
/// condition, the refusal `conditions[{i}]: {reason}` or `conditions holds 1 to 16 entries`. The count is checked
/// first, so a list of the wrong length names its length before any of its conditions. The compiled form is dropped,
/// so a row never holds a pattern the router's build could not compile.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn validate_conditions(views: &[ConditionView]) -> Result<Vec<Condition>, Response> {
    if views.is_empty() || views.len() > MAX_CONDITIONS {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            &CompileError::Count.to_string(),
        ));
    }
    let conditions = views
        .iter()
        .enumerate()
        .map(|(i, view)| {
            Condition::from_words(&ConditionWords::from(view)).map_err(|reason| {
                failure(
                    StatusCode::BAD_REQUEST,
                    &format!("conditions[{i}]: {reason}"),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
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

/// The name's bound, an empty name, and the three phrases the client reads specially in a `400` or a `403` answer:
/// a guardrail's refusal carries the name in its message, and the name is the one part of it the operator writes.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn validate_name(name: &str) -> Result<(), Response> {
    within_text_bound("name", Some(name))?;
    if name.is_empty() {
        return Err(failure(StatusCode::BAD_REQUEST, "name is required"));
    }
    if let Some(phrase) = phoebis_guardrails::client_phrase(name) {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            &format!("name must not contain \"{phrase}\""),
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
pub(crate) fn attachment_refusal(error: StoreError) -> Response {
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
    read_back(state, id, status)
}

/// The guardrail as the registry holds it, with its attachments, answered under `status`.
fn read_back(state: &AdminState, id: &str, status: StatusCode) -> Response {
    let (guardrails, index) = match AttachmentIndex::read(&state.store) {
        Ok(read) => read,
        Err(response) => return response,
    };
    match guardrails.iter().find(|row| row.id == id) {
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
    match AttachmentIndex::read(&state.store) {
        Ok((guardrails, index)) => {
            let guardrails = guardrails
                .iter()
                .map(|row| guardrail_json(row, &index))
                .collect();
            Json(GuardrailList { guardrails }).into_response()
        }
        Err(response) => response,
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
        (status = 400, description = "- `action is block`\n- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `conditions holds 1 to 16 entries`\n- `conditions[{i}]: {reason}`: the condition's index and the reason it is refused (`conditions[1]: value is required`)\n- `description is at most 256 bytes`\n- `id is required`\n- `id is reserved`: `presets` and `test`, which the routes beside `GET /admin/guardrails/{id}` take\n- `id must match [a-z0-9][a-z0-9-]{0,63}`\n- `key {id} would hold more than 64 guardrails`, with nothing written\n- `keys lists {id} twice`\n- `mode is observe or enforce`\n- `name is at most 256 bytes`\n- `name is required`\n- `name must not contain \"{phrase}\"`: the phrase the name carries, `model:`, `OAuth token has been revoked` or `prompt is too long`, each matched in any case\n- `pool {id} would hold more than 64 guardrails`, with nothing written\n- `pools lists {id} twice`\n- `preset_version applies on edit`\n- `unknown preset {id}`: an id `GET /admin/guardrails/presets` does not list", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "- `key {id} not found`, with nothing written\n- `pool {id} not found`, with nothing written", body = ErrorBody, example = json!({"error": "pool {id} not found"})),
        (status = 409, description = "`a guardrail with this id exists`", body = ErrorBody, example = json!({"error": "a guardrail with this id exists"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let input: GuardrailInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
    };
    // A new guardrail takes its stamp from `preset`, so the member that moves the stamp applies to a saved one.
    if input.preset_version.is_some() {
        return failure(StatusCode::BAD_REQUEST, "preset_version applies on edit");
    }
    let Some(id) = &input.id else {
        return failure(StatusCode::BAD_REQUEST, "id is required");
    };
    if let Err(refusal) = check_slug(id) {
        return refusal;
    }
    if RESERVED_IDS.contains(&id.as_str()) {
        return failure(StatusCode::BAD_REQUEST, "id is reserved");
    }
    if let Ok(Some(_)) = state.store.guardrail(id) {
        return failure(StatusCode::CONFLICT, DUPLICATE_ID);
    }
    let preset = match &input.preset {
        Some(preset_id) => match Preset::current(phoebis_guardrails::presets(), preset_id) {
            Some(preset) => Some(preset),
            None => {
                return failure(
                    StatusCode::BAD_REQUEST,
                    &format!("unknown preset {preset_id}"),
                )
            }
        },
        None => None,
    };
    let Some(name) = input.name.as_deref().or(preset.map(|preset| preset.name)) else {
        return failure(StatusCode::BAD_REQUEST, "name is required");
    };
    let now = (state.clock)();
    // The preset's description explains the preset on its own route and can run past the 256 bytes a guardrail's
    // description holds, so a guardrail from a preset starts with the body's description or none. The stamp is the
    // preset's whatever the body changed, since it records where the guardrail came from and `customized` says what
    // changed.
    let mut row = GuardrailRow {
        id: id.clone(),
        name: name.to_string(),
        description: String::new(),
        enabled: true,
        mode: preset.map_or(GuardrailMode::Observe, |preset| preset.suggested_mode),
        action: GuardrailAction::Block,
        conditions: Vec::new(),
        preset: preset.map(|preset| PresetStamp {
            id: preset.id.to_string(),
            version: preset.version,
        }),
        incarnation: 1,
        created_at: now,
        updated_at: now,
    };
    if let Err(refusal) = apply(&mut row, &input) {
        return refusal;
    }
    match (&input.conditions, preset) {
        // The table's own tests check and compile every shipped version, so the preset's conditions stand as they are.
        (None, Some(preset)) => row.conditions = preset.conditions.clone(),
        (views, _) => match validate_conditions(views.as_deref().unwrap_or_default()) {
            Ok(conditions) => row.conditions = conditions,
            Err(refusal) => return refusal,
        },
    }
    if let Err(refusal) = distinct_subjects(&input) {
        return refusal;
    }
    let pools = input.pools.as_deref().unwrap_or_default();
    let keys = input.keys.as_deref().unwrap_or_default();
    // The store takes the incarnation and the stamps inside the insert, so the answer reads the row back.
    match state.store.insert_guardrail(&row, pools, keys, ACTOR) {
        Ok(_) => {}
        // The check above reads outside the insert, so a create that lost the race for the id arrives here.
        Err(StoreError::IdTaken(_)) => return failure(StatusCode::CONFLICT, DUPLICATE_ID),
        Err(error) => return attachment_refusal(error),
    }
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = publish(&state, &row.id, StatusCode::CREATED).await;
    response.extensions_mut().insert(CreatedId(row.id));
    response
}

/// List the presets this build ships.
#[utoipa::path(
    get,
    path = "/admin/guardrails/presets",
    operation_id = "guardrail_presets",
    tag = "Guardrails",
    responses(
        (status = 200, description = "Every preset at its current version, ordered by id.", body = PresetList),
    ),
)]
pub(crate) async fn presets() -> Response {
    let ids: BTreeSet<&str> = phoebis_guardrails::presets()
        .iter()
        .map(|preset| preset.id)
        .collect();
    Json(PresetList {
        presets: ids
            .into_iter()
            .filter_map(|id| Preset::current(phoebis_guardrails::presets(), id))
            .map(preset_view)
            .collect(),
    })
    .into_response()
}

/// Test a guardrail definition against the captured bodies of a pool or a key.
#[utoipa::path(
    post,
    path = "/admin/guardrails/test",
    operation_id = "guardrails_test",
    tag = "Guardrails",
    request_body(content = GuardrailTestInput, content_type = "application/json"),
    responses(
        (status = 200, description = "The verdict on each of the newest requests of the pool or the key with a captured body, no further back than the longest body capture. A sample with no such request answers `sampled` 0.", body = GuardrailTestResult),
        (status = 400, description = "- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `conditions holds 1 to 16 entries`\n- `conditions[{i}]: {reason}`: the condition's index and the reason it is refused (`conditions[0]: value is required`), for an unsaved definition or a saved one this build no longer compiles\n- `guardrail and conditions are exclusive`\n- `guardrail or conditions is required`\n- `limit is 1 to 500`\n- `pool_id and key_id are exclusive`\n- `pool_id or key_id is required`", body = ErrorBody, example = json!({"error": "guardrail or conditions is required"})),
        (status = 404, description = "- `no such guardrail`\n- `no such key`\n- `no such pool`", body = ErrorBody, example = json!({"error": "no such guardrail"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn test(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let input: GuardrailTestInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
    };
    /// The definition the test evaluates: a saved guardrail's id or an unsaved list.
    enum Definition {
        Saved(String),
        Unsaved(Vec<ConditionView>),
    }
    let definition = match (input.guardrail, input.conditions) {
        (Some(_), Some(_)) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "guardrail and conditions are exclusive",
            )
        }
        (None, None) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "guardrail or conditions is required",
            )
        }
        (Some(id), None) => Definition::Saved(id),
        (None, Some(views)) => Definition::Unsaved(views),
    };
    let subject = match (input.pool_id, input.key_id) {
        (Some(_), Some(_)) => {
            return failure(StatusCode::BAD_REQUEST, "pool_id and key_id are exclusive")
        }
        (Some(id), None) => AttachmentSubject::Pool(id),
        (None, Some(id)) => AttachmentSubject::Key(id),
        (None, None) => return failure(StatusCode::BAD_REQUEST, "pool_id or key_id is required"),
    };
    let limit = input.limit.unwrap_or(200);
    if !(1..=500).contains(&limit) {
        return failure(StatusCode::BAD_REQUEST, "limit is 1 to 500");
    }
    let (id, conditions) = match definition {
        Definition::Saved(id) => match state.store.guardrail(&id) {
            Ok(Some(row)) => (row.id, row.conditions),
            Ok(None) => return failure(StatusCode::NOT_FOUND, "no such guardrail"),
            Err(error) => return store_failure(error),
        },
        Definition::Unsaved(views) => match validate_conditions(&views) {
            Ok(conditions) => ("test".to_string(), conditions),
            Err(refusal) => return refusal,
        },
    };
    // A saved row written under rules this build tightened can fail to compile, and its sentence names the condition
    // to edit.
    let compiled = match CompiledGuardrail::compile(
        &id,
        &id,
        GuardrailMode::Observe,
        GuardrailAction::Block,
        &conditions,
    ) {
        Ok(compiled) => Arc::new(compiled),
        Err(error) => return failure(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let known = match &subject {
        AttachmentSubject::Key(id) => state.store.api_key(id).map(|key| key.is_some()),
        AttachmentSubject::Pool(id) => state.store.pool(id).map(|pool| pool.is_some()),
    };
    match known {
        Ok(true) => {}
        Ok(false) => {
            return failure(
                StatusCode::NOT_FOUND,
                &format!("no such {}", subject.kind().as_str()),
            )
        }
        Err(error) => return store_failure(error),
    }
    // A definition with no body or message condition reads no body, so its sample asks only whether each body is
    // held and copies none out.
    let reads_body = compiled
        .conditions
        .iter()
        .any(|c| c.probe.subject().reads_body());
    let window_hours = state.body_capture_max_hours;
    let now = (state.clock)();
    let since = now - i64::from(window_hours) * 3_600_000;
    let limit_rows = limit as usize;
    let mut sampled: Vec<(String, i64, Verdict)> = Vec::new();
    let mut cursor: Option<RequestCursor> = None;
    // One `read` per page, so the read holds a reader connection for one page at a time and a console read waits for
    // one page at most. A page stays full rather than sized to the room left: a row whose body aged out takes no room,
    // and with room-sized pages a sample one short of its limit would walk the rest of the window in pages of one. Each
    // body is parsed and dropped inside the closure, so no body outlives its row.
    loop {
        let page = SAMPLE_PAGE;
        let room = limit_rows - sampled.len();
        let compiled = Arc::clone(&compiled);
        let subject = subject.clone();
        let after = cursor.take();
        let read = state
            .ledger
            .db()
            .read(move |c| {
                let (rows, next) = guardrail_sample_page(c, &subject, since, after.as_ref(), page)?;
                let mut out = Vec::with_capacity(rows.len().min(room));
                for row in &rows {
                    if out.len() == room {
                        break;
                    }
                    // The body aged out, or its own key's capture window ended before the purge reached it, so the
                    // row answers nothing and takes no place in the sample.
                    let tree = if reads_body {
                        let Some(body) = request_body(c, &row.id, now)? else {
                            continue;
                        };
                        parse_body(&body)
                    } else {
                        if !request_body_held(c, &row.id, now)? {
                            continue;
                        }
                        None
                    };
                    let hints = HintLookup {
                        request_class: row.request_class.as_deref(),
                        agent_type: row.agent_type.as_deref(),
                        user_agent: row.user_agent.as_deref(),
                        compaction: row.compaction.as_deref(),
                        context_compacted: row.context_compacted.as_deref(),
                    };
                    let facts = RequestFacts {
                        kind: &row.kind,
                        request_class: row.request_class.as_deref(),
                        agent_type: row.agent_type.as_deref(),
                        client: row.client.as_deref().unwrap_or(OTHER_CLIENT),
                        model: &row.canonical_model,
                        user_agent: row.user_agent.as_deref(),
                        headers: &hints,
                        body: tree.as_ref(),
                    };
                    let verdict = evaluate(&compiled, &facts);
                    out.push((row.id.clone(), row.received_at, verdict));
                }
                Ok((out, next))
            })
            .await;
        let (verdicts, next) = match read {
            Ok(page) => page,
            Err(error) => return ledger_failure(error),
        };
        sampled.extend(verdicts);
        match next {
            Some(next) if sampled.len() < limit_rows => cursor = Some(next),
            _ => break,
        }
    }
    let mut totals = GuardrailTestTotals {
        sampled: 0,
        matched: 0,
        unmatched: 0,
        indeterminate: 0,
    };
    let requests: Vec<GuardrailTestRequest> = sampled
        .into_iter()
        .map(|(id, received_at, verdict)| {
            totals.sampled += 1;
            let (word, condition) = match verdict {
                Verdict::Matched => {
                    totals.matched += 1;
                    ("matched", None)
                }
                Verdict::Unmatched { failed } => {
                    totals.unmatched += 1;
                    ("unmatched", Some(failed))
                }
                Verdict::Indeterminate { condition } => {
                    totals.indeterminate += 1;
                    ("indeterminate", Some(condition))
                }
            };
            GuardrailTestRequest {
                id,
                received_at,
                verdict: word.to_string(),
                condition,
            }
        })
        .collect();
    let (pool_id, key_id) = match subject {
        AttachmentSubject::Key(id) => (None, Some(id)),
        AttachmentSubject::Pool(id) => (Some(id), None),
    };
    Json(GuardrailTestResult {
        sample: GuardrailTestSample {
            pool_id,
            key_id,
            limit,
            window_hours,
        },
        totals,
        requests,
    })
    .into_response()
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
    read_back(&state, &id, StatusCode::OK)
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
        (status = 400, description = "- `action is block`\n- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `conditions holds 1 to 16 entries`\n- `conditions[{i}]: {reason}`: the condition's index and the reason it is refused (`conditions[1]: value is required`)\n- `description is at most 256 bytes`\n- `expected_revision is the revision the row was read at`: a `null`, or a value that is not a non-negative integer\n- `guardrail has no preset`: a `preset_version` on a guardrail created without one\n- `id cannot change`\n- `key {id} would hold more than 64 guardrails`, with nothing written\n- `keys lists {id} twice`\n- `mode is observe or enforce`\n- `name is at most 256 bytes`\n- `name is required`: an empty name\n- `name must not contain \"{phrase}\"`: the phrase the name carries, `model:`, `OAuth token has been revoked` or `prompt is too long`, each matched in any case\n- `pool {id} would hold more than 64 guardrails`, with nothing written\n- `pools lists {id} twice`\n- `preset applies on create`\n- `preset_version is not a shipped version of {preset}`: a version the build does not ship of the guardrail's preset", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
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
    let input: GuardrailInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
    };
    let mut row = match found(state.store.guardrail(&id), "guardrail not found") {
        Ok(row) => row,
        Err(refusal) => return refusal,
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
    // The stamp's id never changes here: a guardrail is re-created to come from another preset.
    if input.preset.is_some() {
        return failure(StatusCode::BAD_REQUEST, "preset applies on create");
    }
    if let Some(version) = input.preset_version {
        let Some(stamp) = row.preset.as_mut() else {
            return failure(StatusCode::BAD_REQUEST, "guardrail has no preset");
        };
        if Preset::at(&stamp.id, version).is_none() {
            return failure(
                StatusCode::BAD_REQUEST,
                &format!("preset_version is not a shipped version of {}", stamp.id),
            );
        }
        stamp.version = version;
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
    match state.store.update_guardrail(
        &row,
        read_at,
        input.pools.as_deref(),
        input.keys.as_deref(),
        ACTOR,
    ) {
        Ok(_) => {}
        Err(StoreError::Stale) => return stale_edit(STALE_EDIT),
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
        Ok(()) => reload_or_no_content(&state).await,
        Err(StoreError::GuardrailNotFound(_)) => {
            failure(StatusCode::NOT_FOUND, "guardrail not found")
        }
        Err(error) => store_failure(error),
    }
}
