//! Account registry routes: creation with verification, edits, deletion, the live catalog, and the two
//! operator actions that clear a cooldown or a funding hold.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_relay::DiscoveryError;
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, Cancellation, CooldownScope, Secret,
    StateCause, StoreError,
};
use serde_json::Value;
use utoipa::ToSchema;

use crate::openapi::{
    query_refusal, ErrorBody, RegistryUnavailable, RouterUnavailable, StorageFailure,
};
use crate::routes::{
    account, account_json, account_write_failure, double_option, failure, fence_revision,
    parse_object, present, reload_or_no_content, reload_or_refuse, router_unavailable, stale_edit,
    store_failure, AdminQuery, CreatedId,
};
use crate::AdminState;

pub(crate) const ACTOR: &str = "admin";

pub(crate) const DUPLICATE_ID: &str = "an account with this id exists";

/// The message of the 409 a stale account edit answers, in the shape a stale key, pool or guardrail edit answers.
const STALE_EDIT: &str = "the account changed while it was being edited, so reload and try again";

/// The account to create. A member the route does not name is ignored.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct CreateAccountInput {
    /// Required, `[a-z0-9][a-z0-9-]{0,63}`.
    pub id: String,
    /// Required, `oauth` or `api_key`.
    #[schema(example = "oauth")]
    pub kind: String,
    /// Required, at most 256 bytes.
    pub label: String,
    /// Required for `api_key`.
    #[serde(default)]
    pub secret: Option<String>,
}

/// The fields to edit, all optional. A member the route does not name is ignored. Every field the body names lands
/// in one write, fenced against `expected_revision` when the body carries it and against the row the route read
/// otherwise, so an edit lands whole or not at all. `subscription_ends_at` needs `canceled` set on the resulting
/// row, and `canceled` needs an end on it, stored or given. A bare `canceled: false` drops the stored end.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct PatchAccountInput {
    /// At most 256 bytes.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// An `api_key` account's new key. An oauth account rejects `secret`. Changing `secret` re-verifies the
    /// credential against the upstream first, and a check that does not succeed refuses the edit whole.
    #[serde(default)]
    pub secret: Option<String>,
    /// Only an oauth account accepts `canceled`.
    #[serde(default)]
    pub canceled: Option<bool>,
    /// Unix milliseconds, or null to drop the stored end.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<i64>, nullable)]
    pub subscription_ends_at: Option<Option<i64>>,
    /// The `revision` the row was read at. `revision` is an opaque token naming one state of one account: an edit,
    /// a pause or resume, a state change or a disconnect moves it, and an account deleted and created again under
    /// the same id never answers a token its predecessor did, so an edit read before the deletion, or one whose
    /// account is replaced while its `secret` is verified, answers `409` and writes nothing. A malformed value
    /// answers `400`, a stale one `409` under `stale_edit`, and a body without it fences on the route's own read.
    // The edit token the row was read at, the `revision` of the account JSON, kept raw and apart from an absent field
    // as `PoolInput` and `KeyInput` keep theirs, so a present null is refused with its own sentence rather than read
    // as "not sent". The write lands only while the row still answers it. Absent, the route fences on its own read.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = u64)]
    pub expected_revision: Option<Value>,
}

/// The longest `label` or `notes` an account or key route accepts, in bytes.
const TEXT_MAX_BYTES: usize = 256;

/// Refuses `value` past `TEXT_MAX_BYTES`, naming `field`, before anything is written.
// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn within_text_bound(field: &str, value: Option<&str>) -> Result<(), Response> {
    match value {
        Some(value) if value.len() > TEXT_MAX_BYTES => Err(failure(
            StatusCode::BAD_REQUEST,
            &format!("{field} is at most {TEXT_MAX_BYTES} bytes"),
        )),
        _ => Ok(()),
    }
}

/// Refuses an id outside `[a-z0-9][a-z0-9-]{0,63}`, the one slug every created row's id takes.
// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn check_slug(id: &str) -> Result<(), Response> {
    if valid_slug(id) {
        Ok(())
    } else {
        Err(failure(
            StatusCode::BAD_REQUEST,
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        ))
    }
}

fn valid_slug(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || b.is_ascii_digit() || (b == b'-' && i > 0))
}

/// Create an account.
///
/// The console pairs an OAuth account through `POST /admin/oauth/sessions` with `new` and never calls this route
/// for the `oauth` kind. The kind stays for a slot created ahead of its pairing by a script.
#[utoipa::path(
    post,
    path = "/admin/accounts",
    operation_id = "accounts_create",
    tag = "Accounts",
    request_body(content = CreateAccountInput, content_type = "application/json"),
    responses(
        (status = 201, description = "The account's fields, the same shape `GET /admin/accounts/{id}` answers.", body = crate::routes::Account),
        (status = 400, description = "- `body must be a JSON object`\n- `body must carry id, kind, and label`\n- `id must match [a-z0-9][a-z0-9-]{0,63}`\n- `label is at most 256 bytes`\n- `kind must be oauth or api_key`\n- `an oauth account is paired, not keyed`\n- `an api_key account needs a secret`\n- `credential rejected by the upstream`", body = ErrorBody, example = json!({"error": "body must carry id, kind, and label"})),
        (status = 404, description = "`account not found`: the row left between the create and its read-back.", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 409, description = "`an account with this id exists`", body = ErrorBody, example = json!({"error": "an account with this id exists"})),
        (status = 500, response = StorageFailure),
        (status = 502, description = "`the key check failed: {reason}`: the models endpoint answered a status other than `200` or `401`, is unreachable, or answered a malformed body. The reason names which, and nothing is created.", body = ErrorBody, example = json!({"error": "the key check failed: {reason}"})),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    // `IgnoredAny` reads any value, so the check refuses a non-object alone, and an object the struct read refuses
    // keeps the create's own sentence.
    if let Err(refusal) = parse_object::<serde::de::IgnoredAny>(&body) {
        return refusal;
    }
    let Ok(input) = serde_json::from_slice::<CreateAccountInput>(&body) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "body must carry id, kind, and label",
        );
    };
    if let Err(refusal) = check_slug(&input.id) {
        return refusal;
    }
    if let Err(refusal) = within_text_bound("label", Some(&input.label)) {
        return refusal;
    }
    let Some(kind) = AccountKind::parse(&input.kind) else {
        return failure(StatusCode::BAD_REQUEST, "kind must be oauth or api_key");
    };
    match (kind, &input.secret) {
        (AccountKind::Oauth, Some(_)) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "an oauth account is paired, not keyed",
            )
        }
        (AccountKind::ApiKey, None) => {
            return failure(StatusCode::BAD_REQUEST, "an api_key account needs a secret")
        }
        _ => {}
    }
    if let Ok(Some(_)) = state.store.account(&input.id) {
        return failure(StatusCode::CONFLICT, DUPLICATE_ID);
    }
    let now = (state.clock)();
    let row = AccountRow {
        id: input.id.clone(),
        kind,
        label: input.label,
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: match kind {
            AccountKind::Oauth => AccountState::Unpaired,
            AccountKind::ApiKey => AccountState::Ready,
        },
        state_cause: StateCause::Operator,
    };
    let secret = input.secret.map(|s| Secret::new(&s));
    // The key is verified before the row exists, so a refused creation leaves nothing behind.
    if let Some(secret) = &secret {
        if let Err(refusal) = verify_staged(&state, &row, secret).await {
            return refusal;
        }
    }
    match state
        .store
        .create_account(&row, secret.as_ref(), now, ACTOR)
    {
        Ok(()) => {}
        // The check above is a read, and discovery runs between it and this insert, so a creation that lost a
        // race for the id arrives here.
        Err(StoreError::IdTaken(_)) => return failure(StatusCode::CONFLICT, DUPLICATE_ID),
        Err(error) => return store_failure(error),
    }
    state.catalog.evict(&row.id);
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = match reload_or_refuse(&state).await {
        Err(refusal) => refusal,
        Ok(()) => match account(&state, &row.id) {
            Ok(created) => (
                StatusCode::CREATED,
                Json(account_json(&state, &created).await),
            )
                .into_response(),
            Err(refusal) => refusal,
        },
    };
    response.extensions_mut().insert(CreatedId(row.id));
    response
}

/// A discovery request with the staged secret against the staged account is the key check. A clean discovery
/// admits the write, a rejected credential refuses it as the operator's mistake, and every other outcome refuses
/// it as the endpoint's, so an account is created or re-keyed only with a key the upstream answered and a refused
/// write leaves nothing behind (the cache reads the live list on demand once the account exists).
async fn verify_staged(
    state: &AdminState,
    staged: &AccountRow,
    secret: &Secret,
) -> Result<(), Response> {
    match state.catalog.discover_with(staged, Some(secret)).await {
        Ok(_) => Ok(()),
        Err(DiscoveryError::Unauthorized) => Err(failure(
            StatusCode::BAD_REQUEST,
            "credential rejected by the upstream",
        )),
        Err(error) => Err(failure(
            StatusCode::BAD_GATEWAY,
            &format!("the key check failed: {error}"),
        )),
    }
}

/// Edit an account.
#[utoipa::path(
    patch,
    path = "/admin/accounts/{id}",
    operation_id = "account_patch",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    request_body(content = PatchAccountInput, content_type = "application/json"),
    responses(
        (status = 200, description = "The account's fields, the same shape as `GET /admin/accounts/{id}`.", body = crate::routes::Account),
        (status = 400, description = "- `body must be a JSON object`\n- `expected_revision is the revision the row was read at`: a `null`, or a value that is not a non-negative integer\n- `label is at most 256 bytes`\n- `only an api_key account has a secret`\n- `only an oauth account is canceled`\n- `subscription_ends_at needs canceled`\n- `canceled needs subscription_ends_at`\n- `credential rejected by the upstream`", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        // The header goes ahead of `example`: utoipa-gen 6 takes the comma after an example as its own, so an
        // attribute after it fails to parse.
        (status = 409, description = "`the account changed while it was being edited, so reload and try again`",
            headers(("x-phoebis-error-code" = String, description = "`stale_edit`: the row changed since it was read")),
            body = ErrorBody, example = json!({"error": "the account changed while it was being edited, so reload and try again"})),
        (status = 500, response = StorageFailure),
        (status = 502, description = "`the key check failed: {reason}`: the models endpoint answered a status other than `200` or `401`, is unreachable, or answered a malformed body. The reason names which, and the stored key stays.", body = ErrorBody, example = json!({"error": "the key check failed: {reason}"})),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let input = match parse_object::<PatchAccountInput>(&body) {
        Ok(input) => input,
        Err(refusal) => return refusal,
    };
    if let Err(refusal) = within_text_bound("label", input.label.as_deref()) {
        return refusal;
    }
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let sent_token = match fence_revision(input.expected_revision.as_ref()) {
        Ok(token) => token,
        Err(refusal) => return refusal,
    };
    if row.kind == AccountKind::Oauth && input.secret.is_some() {
        return failure(
            StatusCode::BAD_REQUEST,
            "only an api_key account has a secret",
        );
    }
    // Validated against the resulting row, not just the edited field: an end saved earlier and a bare
    // `canceled: false` today must not leave the end standing without the flag that gates it.
    let cancellation = if input.canceled.is_some() || input.subscription_ends_at.is_some() {
        let term = match state.store.subscription_term(&id) {
            Ok(term) => term.unwrap_or_default(),
            Err(error) => return store_failure(error),
        };
        let (stored_canceled, stored_ends_at) = term.cancellation.stored_pair();
        let canceled = input.canceled.unwrap_or(stored_canceled);
        let ends_at = input.subscription_ends_at.unwrap_or(stored_ends_at);
        if canceled && row.kind == AccountKind::ApiKey {
            return failure(StatusCode::BAD_REQUEST, "only an oauth account is canceled");
        }
        // The stored end alone never trips this: an uncancel drops it below, so only an end this patch itself asked
        // for can conflict with an uncancel it also asked for.
        if !canceled && input.subscription_ends_at.flatten().is_some() {
            return failure(
                StatusCode::BAD_REQUEST,
                "subscription_ends_at needs canceled",
            );
        }
        if canceled && ends_at.is_none() {
            return failure(
                StatusCode::BAD_REQUEST,
                "canceled needs subscription_ends_at",
            );
        }
        Some(Cancellation::from_flag(canceled, ends_at))
    } else {
        None
    };
    let label = input.label.as_deref().unwrap_or(&row.label);
    let secret = input.secret.as_deref().map(Secret::new);
    if let Some(secret) = &secret {
        // Staged, never written: the row the store holds is untouched until discovery has passed, and the
        // verification below has to see what the patch would land, not what is stored now.
        let staged = AccountRow {
            label: label.to_string(),
            ..row.clone()
        };
        if let Err(refusal) = verify_staged(&state, &staged, secret).await {
            return refusal;
        }
    }
    // A body naming no field moves no revision, so nothing that watches the revision is told the account changed.
    let edits = input.label.is_some()
        || secret.is_some()
        || input.enabled.is_some()
        || cancellation.is_some();
    if edits {
        if sent_token.is_some_and(|token| !row.answers_edit_token(token)) {
            return stale_edit(STALE_EDIT);
        }
        // Any token sent matched here, so every field's commit below is fenced on the incarnation and the revision the
        // route read: an edit built from a stale read, or from an account since deleted and created again under its
        // id, lands nothing, and a combined edit never fences against its own earlier write.
        let update = AccountUpdate {
            id: &id,
            expected_revision: row.revision,
            incarnation: row.incarnation,
            lifecycle_generation: row.lifecycle_generation,
            label,
            secret: secret.as_ref(),
            state: secret
                .as_ref()
                .map(|_| (AccountState::Ready, StateCause::Operator)),
            at: (state.clock)(),
            actor: ACTOR,
            enabled: input.enabled,
            cancellation,
        };
        match state.store.commit_account_update(&update) {
            Ok(()) => {}
            Err(StoreError::Stale) => return stale_edit(STALE_EDIT),
            Err(error) => return account_write_failure(error),
        }
        if secret.is_some() {
            state.catalog.evict(&id);
        }
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match account(&state, &id) {
        Ok(row) => Json(account_json(&state, &row).await).into_response(),
        Err(refusal) => refusal,
    }
}

/// Delete an account.
#[utoipa::path(
    delete,
    path = "/admin/accounts/{id}",
    operation_id = "account_delete",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 204, description = "The account is removed from every pool and deleted, and its request history stays in the ledger."),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn remove(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_account(&id, ACTOR) {
        Ok(()) => {}
        Err(error) => return account_write_failure(error),
    }
    state.forget_account(&id, (state.clock)());
    reload_or_no_content(&state).await
}

/// `GET /admin/accounts/{id}/catalog`'s query.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CatalogQuery {
    /// `1` reads the upstream again before answering. Any other value, or none, answers the cache, filling it on
    /// a miss.
    pub refresh: Option<String>,
}

/// The account's model listing as last read, held until the next scheduled read (every
/// `routing.catalog_refresh_seconds`) or an eviction replaces it, and read on demand when the account has none. A
/// failed read is held for sixty seconds.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    /// Sorted by id. Empty when the fetch failed.
    pub models: Vec<CatalogModel>,
    pub fetched_at: i64,
    /// String or null.
    pub error: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogModel {
    pub id: String,
    pub display_name: String,
    pub created_at: String,
    pub max_input_tokens: Option<u64>,
    pub max_tokens: Option<u64>,
    /// The effort levels the listing says the model supports, in the API's order (`low`, `medium`, `high`,
    /// `xhigh`, `max`, then any other name as listed), empty when the listing names none.
    pub effort_levels: Vec<String>,
}

/// Read the account's model catalog.
#[utoipa::path(
    get,
    path = "/admin/accounts/{id}/catalog",
    operation_id = "account_catalog",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id"), CatalogQuery),
    responses(
        (status = 200, description = "The account's model listing.", body = Catalog),
        (status = 400, description = query_refusal!(), body = ErrorBody, example = json!({"error": "duplicate field `refresh`"})),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
    ),
)]
// `?refresh=1` reads the upstream again before answering, and otherwise the cache answers, filling it on a
// miss.
pub(crate) async fn catalog(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    AdminQuery(q): AdminQuery<CatalogQuery>,
) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let refresh = q.refresh.as_deref() == Some("1");
    let fetched = if refresh {
        state.catalog.refresh(&row).await
    } else {
        state.catalog.fetch(&row).await
    };
    let mut models = fetched.models;
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Json(Catalog {
        models: models
            .into_iter()
            .map(|m| CatalogModel {
                id: m.id,
                display_name: m.display_name,
                created_at: m.created_at,
                max_input_tokens: m.max_input_tokens,
                max_tokens: m.max_tokens,
                effort_levels: m.effort_levels,
            })
            .collect(),
        fetched_at: fetched.fetched_at,
        error: fetched.error,
    })
    .into_response()
}

/// Clear a cooldown.
#[utoipa::path(
    delete,
    path = "/admin/accounts/{id}/cooldowns/{scope}",
    operation_id = "account_cooldown_delete",
    tag = "Accounts",
    params(
        ("id" = String, Path, description = "The account's id"),
        ("scope" = String, Path, description = "`account`, `fast` (the fast lane's rate limit), `fast_credits` (the fast lane's usage-credits refusal), or `model:<canonical>`"),
    ),
    responses(
        (status = 204, description = "The cooldown is cleared."),
        (status = 400, description = "`scope is account, fast, fast_credits or model:<canonical>`", body = ErrorBody, example = json!({"error": "scope is account, fast, fast_credits or model:<canonical>"})),
        (status = 404, description = "- `account not found`\n- `no such cooldown`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RouterUnavailable),
    ),
)]
pub(crate) async fn clear_cooldown(
    State(state): State<Arc<AdminState>>,
    Path((id, scope)): Path<(String, String)>,
) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    let Some(scope) = CooldownScope::parse(&scope) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "scope is account, fast, fast_credits or model:<canonical>",
        );
    };
    match state.router.clear_cooldown(&id, scope).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => failure(StatusCode::NOT_FOUND, "no such cooldown"),
        Err(_) => router_unavailable(),
    }
}

/// Clear a funding hold.
#[utoipa::path(
    post,
    path = "/admin/accounts/{id}/clear-funding",
    operation_id = "account_clear_funding",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 204, description = "No content, once the account leaves `funding_required` for `ready`. The router tries a marked account again after five minutes, or the model rejection interval when that is shorter, and clears it on success, so this operation is for a retry before then."),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 409, description = "`the account is not waiting on funding`", body = ErrorBody, example = json!({"error": "the account is not waiting on funding"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn clear_funding(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    if row.state != AccountState::FundingRequired {
        return failure(
            StatusCode::CONFLICT,
            "the account is not waiting on funding",
        );
    }
    if let Err(error) = state
        .store
        .set_state(&id, AccountState::Ready, StateCause::Operator)
    {
        return store_failure(error);
    }
    reload_or_no_content(&state).await
}

/// `GET /admin/accounts/{id}/quota`'s query.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct QuotaQuery {
    /// `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. Default `7d`.
    pub range: Option<String>,
}

/// The account's quota samples and extra usage samples over the range.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Quota {
    pub account_id: String,
    pub from: i64,
    pub to: i64,
    /// One per window id with its samples.
    pub windows: Vec<QuotaWindow>,
    pub extra_usage: Vec<ExtraUsageSample>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotaWindow {
    pub id: String,
    pub samples: Vec<QuotaSample>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotaSample {
    pub at: i64,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
    pub source: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtraUsageSample {
    pub at: i64,
    pub is_enabled: bool,
    pub in_use: Option<bool>,
    pub monthly_limit: Option<i64>,
    pub used_credits: Option<i64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
    pub disabled_reason: Option<String>,
    pub overage_reset: Option<i64>,
    pub source: String,
}

/// Read the account's quota samples.
#[utoipa::path(
    get,
    path = "/admin/accounts/{id}/quota",
    operation_id = "account_quota",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id"), QuotaQuery),
    responses(
        (status = 200, description = "The samples over the range.", body = Quota),
        (status = 400, description = concat!("For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\n", query_refusal!()), body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn quota(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    AdminQuery(q): AdminQuery<QuotaQuery>,
) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    let now = (state.clock)();
    let (from, to) =
        match phoebis_ledger::parse_range(Some(q.range.as_deref().unwrap_or("7d")), now) {
            Ok(r) => r,
            Err(m) => return failure(StatusCode::BAD_REQUEST, &m),
        };
    let (from_bucket, to_bucket) = (from / 1_000, to / 1_000 + 300);
    let (quota, extra) = match (
        state
            .ledger
            .db()
            .quota_samples(&id, from_bucket, to_bucket)
            .await,
        state
            .ledger
            .db()
            .extra_usage_samples(&id, from_bucket, to_bucket)
            .await,
    ) {
        (Ok(q), Ok(e)) => (q, e),
        (Err(e), _) | (_, Err(e)) => return crate::analytics::ledger_failure(e),
    };
    let mut windows: BTreeMap<String, Vec<QuotaSample>> = BTreeMap::new();
    for s in quota {
        windows
            .entry(s.window.clone())
            .or_default()
            .push(QuotaSample {
                at: s.bucket * 1_000,
                utilization: s.utilization,
                resets_at: s.reset_at,
                source: s.source.as_str().into(),
            });
    }
    Json(Quota {
        account_id: id,
        from,
        to,
        windows: windows
            .into_iter()
            .map(|(id, samples)| QuotaWindow { id, samples })
            .collect(),
        extra_usage: extra
            .into_iter()
            .map(|e| ExtraUsageSample {
                at: e.bucket * 1_000,
                is_enabled: e.is_enabled,
                in_use: e.in_use,
                monthly_limit: e.monthly_limit,
                used_credits: e.used_credits,
                utilization: e.utilization,
                currency: e.currency,
                disabled_reason: e.disabled_reason,
                overage_reset: e.overage_reset,
                source: e.source.as_str().into(),
            })
            .collect(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::round_trips;

    #[test]
    fn the_account_list_answer_round_trips_through_its_declared_type() {
        round_trips::<crate::routes::AccountList>("accounts");
    }
}
