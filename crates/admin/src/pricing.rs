//! The price table, the import preview, and its confirmation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{diff_against, parse_import, ImportFormat, ModelPrice, PriceTable};
use phoebis_store::PricingVersion;
use serde_json::{json, Value};

use crate::routes::{failure, store_failure};
use crate::{AdminState, PendingImport};

const ACTOR: &str = "admin";
const IMPORT_MAX_BYTES: usize = 16 << 20;
const IMPORT_TIMEOUT: Duration = Duration::from_secs(30);
const PREVIEW_TTL_MS: i64 = 10 * 60 * 1_000;

pub(crate) fn table_json(table: &PriceTable, versions: &[PricingVersion]) -> Value {
    json!({
        "version": table.version, "effective": table.effective, "source": table.source,
        "models": table.models.iter().map(|(m, p)| model_json(m, p)).collect::<Vec<_>>(),
        "versions": versions.iter().map(|v| json!({ "version": v.version, "effective": v.effective, "source": v.source, "imported_at": v.imported_at })).collect::<Vec<_>>(),
    })
}

fn model_json(model: &str, p: &ModelPrice) -> Value {
    json!({ "model": model, "input_per_m": p.input, "cache_write_5m_per_m": p.cache_write_5m, "cache_write_1h_per_m": p.cache_write_1h, "cache_read_per_m": p.cache_read, "output_per_m": p.output })
}

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.pricing_versions() {
        Ok(versions) => Json(table_json(&state.pricing.current(), &versions)).into_response(),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn import(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    #[derive(serde::Deserialize)]
    struct Input {
        url: String,
    }
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry a url");
    };
    if !(input.url.starts_with("http://") || input.url.starts_with("https://")) {
        return failure(StatusCode::BAD_REQUEST, "url must be http or https");
    }
    let bytes = match fetch_url(&state, &input.url).await {
        Ok(b) => b,
        Err(m) => return failure(StatusCode::BAD_GATEWAY, &m),
    };
    let (format, parsed) = match parse_import(&bytes) {
        Ok(p) => p,
        Err(e) => return failure(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let served: Vec<String> = match state.store.load_registry() {
        Ok(load) => {
            let mut ids: Vec<String> = load.models.iter().map(|m| m.canonical_id.clone()).collect();
            ids.sort();
            ids.dedup();
            ids
        }
        Err(e) => return store_failure(e),
    };
    let diff = diff_against(&state.pricing.current(), &parsed, &served);
    let now = (state.clock)();
    let token = format!("imp_{:032x}", rand::random::<u128>());
    let mut pending = state
        .pending_imports
        .lock()
        .expect("pending imports poisoned");
    pending.retain(|_, p| p.expires_at > now);
    pending.insert(
        token.clone(),
        PendingImport {
            models: diff.models.clone(),
            source: input.url,
            expires_at: now + PREVIEW_TTL_MS,
        },
    );
    Json(json!({
        "token": token,
        "format": match format { ImportFormat::LiteLlm => "litellm", ImportFormat::ModelsDev => "models_dev" },
        "in_file": diff.in_file, "served": diff.served,
        "models": diff.models.iter().map(|(m, p)| model_json(m, p)).collect::<Vec<_>>(),
        "changes": diff.changes,
        "expires_at": now + PREVIEW_TTL_MS,
    }))
    .into_response()
}

pub(crate) async fn confirm(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    #[derive(serde::Deserialize)]
    struct Input {
        token: String,
    }
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry a token");
    };
    // One confirmation at a time: the overlay reads the current table and the publish replaces it, and two
    // interleaved confirmations would each overlay the table the other is about to replace.
    let _one_at_a_time = state.confirming.lock().await;
    let now = (state.clock)();
    let pending = state
        .pending_imports
        .lock()
        .expect("pending imports poisoned")
        .remove(&input.token)
        .filter(|p| p.expires_at > now);
    let Some(pending) = pending else {
        return failure(
            StatusCode::NOT_FOUND,
            "no such import preview, or it expired",
        );
    };
    let current = state.pricing.current();
    let mut models: BTreeMap<String, ModelPrice> = current.models.clone();
    models.extend(pending.models);
    let effective = jiff::Timestamp::from_millisecond(now)
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| current.effective.clone());
    let row = PricingVersion {
        version: format!("imported-{now}"),
        effective,
        source: pending.source,
        table_json: match serde_json::to_string(&models) {
            Ok(j) => j,
            Err(_) => {
                return failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the table did not serialize",
                )
            }
        },
        imported_at: now,
    };
    if let Err(e) = state.store.insert_pricing_version(&row, ACTOR) {
        return store_failure(e);
    }
    let table = match PriceTable::from_version(&row) {
        Ok(t) => t,
        Err(e) => return failure(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    state.pricing.replace(table.clone());
    match state.store.pricing_versions() {
        Ok(versions) => (StatusCode::CREATED, Json(table_json(&table, &versions))).into_response(),
        Err(e) => store_failure(e),
    }
}
