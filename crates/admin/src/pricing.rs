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
const TOO_LARGE: &str = "the price file is larger than the 16 MiB bound";

/// What an import records as its source: enough to say where a table came from, never the credential a
/// query string or userinfo may carry, since the label is answered to the viewer role and kept per version.
pub(crate) fn source_label(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!(
        "{}://{}{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or(""),
        port,
        parsed.path()
    )
}

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

/// The error's Display names the URL, which may carry a token in its query; the category is all that is
/// answered, and a timeout keeps the sentence the console shows for it.
fn import_failure(e: reqwest::Error) -> String {
    if e.is_timeout() {
        return "the price file did not arrive in time".to_string();
    }
    let e = e.without_url();
    if e.is_connect() {
        "the price file could not be fetched: connect".to_string()
    } else if e.is_body() || e.is_decode() {
        "the price file could not be fetched: body".to_string()
    } else if let Some(status) = e.status() {
        format!(
            "the price file could not be fetched: status {}",
            status.as_u16()
        )
    } else {
        "the price file could not be fetched: request".to_string()
    }
}

/// The import's own client: built per call because an import is an operator action, never a hot path, and
/// the bounds are the request's rather than a pool's. No redirect, so the bytes come from the host the
/// operator named.
async fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "url does not parse".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("url must be http or https".into());
    }
    if parsed.host_str().is_none() {
        return Err("url has no host".into());
    }
    // Built as the alerts crate builds its webhook client (`ReqwestWebhook::new`), minus its destination
    // guard: whether an import may dial a private address is a decision the owner holds, so the difference
    // is deliberate.
    let client = reqwest::Client::builder()
        .timeout(IMPORT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("phoebis/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "the import client could not be built".to_string())?;
    let mut response = client
        .get(parsed)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(import_failure)?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "the price file answered {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|len| len > IMPORT_MAX_BYTES as u64)
    {
        return Err(TOO_LARGE.to_string());
    }
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(import_failure)? {
        if out.len() + chunk.len() > IMPORT_MAX_BYTES {
            return Err(TOO_LARGE.to_string());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
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
    let bytes = match fetch_url(&input.url).await {
        Ok(b) => b,
        Err(m) => return failure(StatusCode::BAD_GATEWAY, &m),
    };
    let (format, parsed) = match parse_import(&bytes) {
        Ok(p) => p,
        Err(e) => return failure(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let accounts = match state.store.accounts() {
        Ok(accounts) => accounts,
        Err(e) => return store_failure(e),
    };
    let mut served = std::collections::BTreeSet::new();
    for account in accounts.iter().filter(|a| a.enabled) {
        served.extend(
            state
                .catalog
                .fetch(account)
                .await
                .models
                .into_iter()
                .map(|m| m.id),
        );
    }
    let served: Vec<String> = served.into_iter().collect();
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
            source: source_label(&input.url),
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

#[cfg(test)]
mod tests {
    use super::source_label;

    #[test]
    fn source_label_keeps_the_authority_and_path_and_drops_credentials_and_the_query() {
        assert_eq!(
            source_label("https://user:pw@host:8443/p/prices.json?access_token=x#f"),
            "https://host:8443/p/prices.json"
        );
    }
}
