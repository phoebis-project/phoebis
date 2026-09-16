//! The sessions view: the live pins joined with what the ledger saw, the release that expires a pin, and the
//! per-model cost card the console shows beside them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    let Ok(pins) = state.router.pins().await else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        );
    };
    let now = (state.clock)();
    let cutoff = now - state.session_expiry.as_millis() as i64;
    let activity = match state
        .ledger
        .db()
        .read(|c| phoebis_ledger::session_activity(c, cutoff))
    {
        Ok(a) => a,
        Err(e) => return crate::analytics::ledger_failure(e),
    };
    let mut rows: std::collections::BTreeMap<(String, String), serde_json::Value> =
        std::collections::BTreeMap::new();
    for p in pins.into_iter().filter(|p| p.last_used_at >= cutoff) {
        let key = (p.key_id.clone(), p.session_id.clone());
        // A session holds a pin per model and the row shows one of them, so it shows the one the caller used
        // last rather than whichever model the sorted list ends on.
        if rows
            .get(&key)
            .is_some_and(|held| held["last_used_at"].as_i64().unwrap_or(0) >= p.last_used_at)
        {
            continue;
        }
        rows.insert(
            key,
            json!({
                "key_id": p.key_id, "session_id": p.session_id, "model": p.model, "account_id": p.account_id, "epoch": p.epoch,
                "created_at": p.created_at, "last_used_at": p.last_used_at, "last_request_at": null, "requests": null, "pinned": true,
            }),
        );
    }
    for a in activity {
        let entry = rows
            .entry((a.key_id.clone(), a.session_id.clone()))
            .or_insert_with(|| {
                json!({
                    "key_id": a.key_id, "session_id": a.session_id, "model": a.model, "account_id": null, "epoch": null,
                    "created_at": null, "last_used_at": null, "last_request_at": null, "requests": 0, "pinned": false,
                })
            });
        entry["last_request_at"] = json!(a.last_request_at);
        entry["requests"] = json!(a.requests);
        if entry["account_id"].is_null() {
            // Without a pin the serving account is the ledger's: the newest attempt of the newest request.
            entry["account_id"] = json!(a.account_id);
        }
    }
    let mut out: Vec<serde_json::Value> = rows.into_values().collect();
    let latest = |v: &serde_json::Value| {
        v["last_used_at"]
            .as_i64()
            .unwrap_or(0)
            .max(v["last_request_at"].as_i64().unwrap_or(0))
    };
    out.sort_by_key(|v| std::cmp::Reverse(latest(v)));
    Json(json!({ "sessions": out })).into_response()
}

pub(crate) async fn costs(State(state): State<Arc<AdminState>>) -> Response {
    let table = state.pricing.current();
    Json(json!({
        "version": table.version, "effective": table.effective,
        "models": table.models.iter().map(|(m, p)| json!({
            "model": m, "input_per_m": p.input, "cache_write_5m_per_m": p.cache_write_5m, "cache_write_1h_per_m": p.cache_write_1h, "cache_read_per_m": p.cache_read, "output_per_m": p.output,
            "reads_per_write_5m": if p.cache_read > 0.0 { Some(p.cache_write_5m / p.cache_read) } else { None },
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

pub(crate) async fn release(
    State(state): State<Arc<AdminState>>,
    Path((key, session)): Path<(String, String)>,
) -> Response {
    match state.router.release_session(&key, &session).await {
        Ok(0) => failure(StatusCode::NOT_FOUND, "no pin for this session"),
        Ok(n) => {
            let mut r = StatusCode::NO_CONTENT.into_response();
            r.headers_mut().insert(
                "x-phoebis-released",
                HeaderValue::from_str(&n.to_string()).expect("digits"),
            );
            r
        }
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        ),
    }
}
