//! Account registry routes: creation with verification, edits, deletion, served models, and the two operator
//! actions that clear a cooldown or a funding hold.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_relay::DiscoveryError;
use phoebis_store::{
    canonical_id, AccountKind, AccountRow, AccountState, AccountUpdate, ModelSource, Secret,
    ServedModelWrite, StoreError,
};
use serde_json::{json, Value};

use crate::routes::{
    account, account_json, failure, failure_with_code, reload_or_refuse, store_failure,
};
use crate::AdminState;

const ACTOR: &str = "admin";

const DUPLICATE_ID: &str = "an account with this id exists";

/// What discovery said about a staged credential: the catalog to store, or the error to record.
enum Verified {
    Catalog(Vec<(String, bool)>),
    Error(String),
}

#[derive(serde::Deserialize)]
struct CreateInput {
    id: String,
    kind: String,
    label: String,
    #[serde(default)]
    secret: Option<String>,
}

#[derive(serde::Deserialize)]
struct PatchInput {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    secret: Option<String>,
}

#[derive(serde::Deserialize)]
struct ModelsInput {
    models: Vec<ModelInput>,
}

#[derive(serde::Deserialize)]
struct ModelInput {
    upstream_id: String,
    selected: bool,
    #[serde(default)]
    long_context: bool,
}

fn valid_slug(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || b.is_ascii_digit() || (b == b'-' && i > 0))
}

pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let Ok(input) = serde_json::from_slice::<CreateInput>(&body) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "body must carry id, kind, and label",
        );
    };
    if !valid_slug(&input.id) {
        return failure(
            StatusCode::BAD_REQUEST,
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        );
    }
    let kind = match input.kind.as_str() {
        "oauth" => AccountKind::Oauth,
        "api_key" => AccountKind::ApiKey,
        _ => return failure(StatusCode::BAD_REQUEST, "kind must be oauth or api_key"),
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
    };
    let secret = input.secret.map(|s| Secret::new(&s));
    // The key is verified before the row exists, so a refused creation leaves nothing behind.
    let verified = match &secret {
        Some(secret) => match verify_staged(&state, &row, secret).await {
            Ok(verified) => Some(verified),
            Err(refusal) => return refusal,
        },
        None => None,
    };
    let (catalog, catalog_error) = match &verified {
        Some(Verified::Catalog(models)) => (Some(models.as_slice()), None),
        Some(Verified::Error(error)) => (None, Some(error.as_str())),
        None => (None, None),
    };
    if let Err(error) =
        state
            .store
            .create_account(&row, secret.as_ref(), catalog, catalog_error, now, ACTOR)
    {
        // The check above is a read, and discovery runs between it and this insert, so a creation that lost a
        // race for the id arrives here as the store's unique constraint. The insert is one transaction, so a
        // row standing under that id now belongs to the winner rather than to a half-written attempt.
        return match state.store.account(&row.id) {
            Ok(Some(_)) => failure(StatusCode::CONFLICT, DUPLICATE_ID),
            _ => store_failure(error),
        };
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match account(&state, &row.id) {
        Ok(row) => (StatusCode::CREATED, Json(account_json(&state, &row).await)).into_response(),
        Err(refusal) => refusal,
    }
}

/// A discovery request with the staged secret against the staged account doubles as the key check: a 401
/// refuses the edit, a successful list is the catalog to store, and any other answer is recorded as the
/// catalog error.
async fn verify_staged(
    state: &AdminState,
    staged: &AccountRow,
    secret: &Secret,
) -> Result<Verified, Response> {
    match state.catalog.discover_with(staged, Some(secret)).await {
        // A list carrying two spellings of one canonical id would roll the store transaction back and refuse
        // the whole write, leaving the operator no way to hold the account at all; the scheduled sync records
        // the same clash as a catalog error, so the row lands here too and the models are added by hand.
        Ok(discovered) => Ok(match clashing_canonical_id(&discovered.models) {
            Some(canonical) => {
                Verified::Error(format!("two spellings of {canonical} in one response"))
            }
            None => Verified::Catalog(discovered.models),
        }),
        Err(DiscoveryError::Unauthorized) => Err(failure(
            StatusCode::BAD_REQUEST,
            "credential rejected by the upstream",
        )),
        Err(error) => Ok(Verified::Error(error.to_string())),
    }
}

/// The first discovered id whose canonical form the account already serves under a different upstream id.
fn stored_clash(
    state: &AdminState,
    account_id: &str,
    discovered: &[(String, bool)],
) -> Result<Option<String>, StoreError> {
    let stored = state.store.served_models(account_id)?;
    Ok(discovered.iter().find_map(|(upstream_id, _)| {
        let canonical = canonical_id(upstream_id);
        stored
            .iter()
            .any(|m| m.canonical_id == canonical && m.upstream_id != *upstream_id)
            .then_some(canonical)
    }))
}

/// The first canonical id a discovered list names twice, whatever the spellings.
fn clashing_canonical_id(models: &[(String, bool)]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    models
        .iter()
        .map(|(upstream_id, _)| canonical_id(upstream_id))
        .find(|canonical| !seen.insert(canonical.clone()))
}

pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<PatchInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    if row.kind == AccountKind::Oauth && input.secret.is_some() {
        return failure(
            StatusCode::BAD_REQUEST,
            "only an api_key account has a secret",
        );
    }
    let label = input.label.as_deref().unwrap_or(&row.label);
    let verifying = row.kind == AccountKind::ApiKey && input.secret.is_some();
    if verifying {
        // Staged, never written: the row the store holds is untouched until discovery has passed.
        let staged = AccountRow {
            label: label.to_string(),
            ..row.clone()
        };
        let secret = Secret::new(input.secret.as_deref().expect("verifying implies a secret"));
        let verified = match verify_staged(&state, &staged, &secret).await {
            Ok(verified) => verified,
            Err(refusal) => return refusal,
        };
        // A stored row under another spelling of a discovered id breaks the catalog's canonical-id
        // uniqueness inside the commit, which would roll the verified secret or URL back with a 500. It is
        // the same clash the intra-list check finds, so it is recorded the same way and the stored catalog
        // is left as it is.
        let verified = match verified {
            Verified::Catalog(models) => match stored_clash(&state, &id, &models) {
                Ok(Some(canonical)) => Verified::Error(format!(
                    "{canonical} is already served under another spelling"
                )),
                Ok(None) => Verified::Catalog(models),
                Err(error) => return store_failure(error),
            },
            error => error,
        };
        let (catalog, catalog_error) = match &verified {
            Verified::Catalog(models) => (Some(models.as_slice()), None),
            Verified::Error(error) => (None, Some(error.as_str())),
        };
        let update = AccountUpdate {
            id: &id,
            expected_revision: row.revision,
            lifecycle_generation: row.lifecycle_generation,
            label,
            secret: Some(&secret),
            catalog,
            catalog_error,
            state: AccountState::Ready,
            at: (state.clock)(),
            actor: ACTOR,
        };
        match state.store.commit_account_update(&update) {
            Ok(()) => {}
            Err(StoreError::Stale) => {
                return failure(
                    StatusCode::CONFLICT,
                    "the account changed while the edit was being verified",
                )
            }
            Err(StoreError::AccountNotFound(_)) => {
                return failure(StatusCode::NOT_FOUND, "account not found")
            }
            Err(error) => return store_failure(error),
        }
    } else if input.label.is_some() {
        // Fenced on the revision the row was read at, like the verified branch above: a patch built from a
        // stale read must not land on a row another edit has moved since.
        if let Err(error) = state.store.update_account(
            &id,
            row.revision,
            label,
            ACTOR,
        ) {
            match &error {
                StoreError::Stale => {
                    return failure(
                        StatusCode::CONFLICT,
                        "the account changed while the edit was being verified",
                    )
                }
                _ => return store_failure(error),
            }
        }
    }
    // Last, because every refusal above answers that nothing changed: a rejected key that had already flipped
    // `enabled` would make that answer a lie.
    if let Some(enabled) = input.enabled {
        if let Err(error) = state.store.set_enabled(&id, enabled) {
            // A verified edit may already be in the store, so it is published before the storage failure is
            // answered rather than left waiting for an unrelated write.
            let _ = reload_or_refuse(&state).await;
            return store_failure(error);
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

pub(crate) async fn remove(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_account(&id, ACTOR) {
        Ok(()) => {}
        Err(StoreError::AccountNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "account not found")
        }
        Err(error) => return store_failure(error),
    }
    state.usage.invalidate(&id);
    let now = (state.clock)();
    for (session_id, owner) in state.vault.discard_for_account(&id) {
        let _ = state.pairing.cancel(&session_id, &owner, now);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) async fn models(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    models_json(&state, &id)
        .map(|v| Json(v).into_response())
        .unwrap_or_else(store_failure)
}

pub(crate) async fn put_models(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<ModelsInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry a models list");
    };
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    let rows: Vec<ServedModelWrite> = input
        .models
        .into_iter()
        .map(|m| ServedModelWrite {
            upstream_id: m.upstream_id,
            selected: m.selected,
            long_context: m.long_context,
        })
        .collect();
    match state.store.put_served_models(&id, &rows, ACTOR) {
        Ok(()) => {}
        Err(StoreError::CanonicalClash { canonical_id, .. }) => {
            return failure(
                StatusCode::CONFLICT,
                &format!("two spellings of {canonical_id}"),
            )
        }
        Err(error) => return store_failure(error),
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    models_json(&state, &id)
        .map(|v| Json(v).into_response())
        .unwrap_or_else(store_failure)
}

pub(crate) async fn sync(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    match state.catalog.sync(&id, ACTOR).await {
        Ok(outcome) => {
            // The rows are in the store either way, so the refusal is about publication alone, which is what
            // the same answer means everywhere else an admin write cannot reach the router.
            if let Some(error) = &outcome.reload_failed {
                tracing::error!(account = %id, %error, "a catalog sync could not be published");
                let written = outcome.delta.is_some() || outcome.seeded;
                let message = if written {
                    "the catalog was written but could not be published; the next successful reload publishes it"
                } else if outcome.stale {
                    "the account changed during discovery, so nothing was written; the reload that followed also failed"
                } else {
                    "discovery failed and its error was recorded; the reload that followed also failed"
                };
                let mut response = failure_with_code(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "registry_unavailable",
                    message,
                );
                *response.body_mut() = axum::body::Body::from(
                    json!({ "error": message, "written": written }).to_string(),
                );
                return response;
            }
            Json(json!({
                "added": outcome.delta.as_ref().map(|d| d.added.clone()).unwrap_or_default(),
                "missing": outcome.delta.as_ref().map(|d| d.missing.clone()).unwrap_or_default(),
                "error": outcome.error,
                "seeded": outcome.seeded,
            }))
            .into_response()
        }
        Err(error) => store_failure(error),
    }
}

pub(crate) async fn clear_cooldown(
    State(state): State<Arc<AdminState>>,
    Path((id, scope)): Path<(String, String)>,
) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    if scope != "account" && !scope.starts_with("model:") {
        return failure(
            StatusCode::BAD_REQUEST,
            "scope is account or model:<canonical>",
        );
    }
    match state.router.clear_cooldown(&id, &scope).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => failure(StatusCode::NOT_FOUND, "no such cooldown"),
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        ),
    }
}

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
    if let Err(error) = state.store.set_state(&id, AccountState::Ready) {
        return store_failure(error);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) fn models_json(state: &AdminState, id: &str) -> Result<Value, StoreError> {
    let rows = state.store.served_models(id)?;
    let (synced_at, error) = state.store.catalog_status(id)?;
    // A seed is what an account serves until a discovery lands, so only the newer of the two says which the
    // current list is.
    let built_in = state
        .store
        .latest_catalog_change(id)?
        .is_some_and(|kind| kind == "catalog_seeded");
    Ok(json!({
        "models": rows.iter().map(|m| json!({
            "upstream_id": m.upstream_id,
            "canonical_id": m.canonical_id,
            "selected": m.selected,
            "source": match m.source { ModelSource::Discovered => "discovered", ModelSource::Manual => "manual" },
            "long_context": m.long_context,
            "missing": m.missing,
            "new": m.is_new,
        })).collect::<Vec<_>>(),
        "catalog_synced_at": synced_at,
        "catalog_error": error,
        "built_in": built_in,
    }))
}

pub(crate) async fn quota(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    let now = (state.clock)();
    let (from, to) = match phoebis_ledger::parse_range(
        Some(q.get("range").map(String::as_str).unwrap_or("7d")),
        now,
    ) {
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
    let mut windows: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for s in quota {
        windows.entry(s.window.clone()).or_default().push(json!({ "at": s.bucket * 1_000, "utilization": s.utilization, "resets_at": s.reset_at, "source": s.source }));
    }
    Json(json!({
        "account_id": id, "from": from, "to": to,
        "windows": windows.into_iter().map(|(id, samples)| json!({ "id": id, "samples": samples })).collect::<Vec<_>>(),
        "extra_usage": extra.iter().map(|e| json!({ "at": e.bucket * 1_000, "is_enabled": e.is_enabled, "in_use": e.in_use, "monthly_limit": e.monthly_limit, "used_credits": e.used_credits, "utilization": e.utilization, "currency": e.currency, "disabled_reason": e.disabled_reason, "overage_reset": e.overage_reset, "source": e.source })).collect::<Vec<_>>(),
    }))
    .into_response()
}
