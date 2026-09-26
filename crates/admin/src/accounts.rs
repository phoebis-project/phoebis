//! Account registry routes: creation with verification, edits, deletion, the live catalog, and the two
//! operator actions that clear a cooldown or a funding hold.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_relay::DiscoveryError;
use phoebis_store::{AccountKind, AccountRow, AccountState, AccountUpdate, Secret, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::{account, account_json, failure, reload_or_refuse, store_failure};
use crate::AdminState;

const ACTOR: &str = "admin";

const DUPLICATE_ID: &str = "an account with this id exists";

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
    #[serde(default)]
    cancelled: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    subscription_ends_at: Option<Option<i64>>,
}

fn double_option<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
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
    if let Some(secret) = &secret {
        if let Err(refusal) = verify_staged(&state, &row, secret).await {
            return refusal;
        }
    }
    if let Err(error) = state
        .store
        .create_account(&row, secret.as_ref(), now, ACTOR)
    {
        // The check above is a read, and discovery runs between it and this insert, so a creation that lost a
        // race for the id arrives here as the store's unique constraint. The insert is one transaction, so a
        // row standing under that id now belongs to the winner rather than to a half-written attempt.
        return match state.store.account(&row.id) {
            Ok(Some(_)) => failure(StatusCode::CONFLICT, DUPLICATE_ID),
            _ => store_failure(error),
        };
    }
    state.catalog.evict(&row.id);
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match account(&state, &row.id) {
        Ok(row) => (StatusCode::CREATED, Json(account_json(&state, &row).await)).into_response(),
        Err(refusal) => refusal,
    }
}

/// A discovery request with the staged secret against the staged account doubles as the key check: a 401
/// refuses the edit, and any other outcome, success or failure, changes nothing here (the cache reads the
/// live list on demand once the account exists).
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
        Err(error) => {
            tracing::warn!(account = %staged.id, %error, "the account's catalog probe failed during verification");
            Ok(())
        }
    }
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
    // Validated against the resulting row, not just the edited field: an end saved earlier and a bare
    // `cancelled: false` today must not leave the end standing without the flag that gates it.
    let cancellation = if input.cancelled.is_some() || input.subscription_ends_at.is_some() {
        let term = match state.store.subscription_term(&id) {
            Ok(term) => term.unwrap_or_default(),
            Err(error) => return store_failure(error),
        };
        let cancelled = input.cancelled.unwrap_or(term.cancelled);
        let ends_at = input.subscription_ends_at.unwrap_or(term.ends_at);
        if cancelled && row.kind == AccountKind::ApiKey {
            return failure(
                StatusCode::BAD_REQUEST,
                "only an oauth account is cancelled",
            );
        }
        // The stored end alone never trips this: `set_cancelled` drops it the moment the flag goes off, so
        // only an end this patch itself asked for can conflict with an uncancel it also asked for.
        if !cancelled && input.subscription_ends_at.flatten().is_some() {
            return failure(
                StatusCode::BAD_REQUEST,
                "subscription_ends_at needs cancelled",
            );
        }
        if cancelled && ends_at.is_none() {
            return failure(
                StatusCode::BAD_REQUEST,
                "cancelled needs subscription_ends_at",
            );
        }
        Some((cancelled, ends_at))
    } else {
        None
    };
    let label = input.label.as_deref().unwrap_or(&row.label);
    let verifying = row.kind == AccountKind::ApiKey && input.secret.is_some();
    if verifying {
        let secret = Secret::new(input.secret.as_deref().expect("verifying implies a secret"));
        // Staged, never written: the row the store holds is untouched until discovery has passed, and the
        // verification below has to see what the patch would land, not what is stored now.
        let staged = AccountRow {
            label: label.to_string(),
            ..row.clone()
        };
        if let Err(refusal) = verify_staged(&state, &staged, &secret).await {
            return refusal;
        }
        let update = AccountUpdate {
            id: &id,
            expected_revision: row.revision,
            lifecycle_generation: row.lifecycle_generation,
            label,
            secret: Some(&secret),
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
        state.catalog.evict(&id);
    } else if input.label.is_some() {
        // Fenced on the revision the row was read at, like the verified branch above: a patch built from a
        // stale read must not land on a row another edit has moved since.
        if let Err(error) = state.store.update_account(&id, row.revision, label, ACTOR) {
            match &error {
                StoreError::Stale => {
                    return failure(
                        StatusCode::CONFLICT,
                        "the account changed while the edit was being saved",
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
    if let Some((cancelled, ends_at)) = cancellation {
        if let Err(error) = state.store.set_cancelled(&id, cancelled, ends_at) {
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
    state.catalog.evict(&id);
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

/// `?refresh=1` reads the upstream again before answering; otherwise the cache answers, filling it on a
/// miss.
pub(crate) async fn catalog(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let refresh = q.get("refresh").map(String::as_str) == Some("1");
    let fetched = if refresh {
        state.catalog.refresh(&row).await
    } else {
        state.catalog.fetch(&row).await
    };
    let mut models = fetched.models;
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Json(json!({
        "models": models.iter().map(|m| json!({
            "id": m.id,
            "display_name": m.display_name,
            "created_at": m.created_at,
            "max_input_tokens": m.max_input_tokens,
            "max_tokens": m.max_tokens,
        })).collect::<Vec<_>>(),
        "fetched_at": fetched.fetched_at,
        "error": fetched.error,
    }))
    .into_response()
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
