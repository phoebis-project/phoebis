//! The sessions view this revision can show: the live pins, and the release that expires them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    let Ok(mut pins) = state.router.pins().await else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        );
    };
    let cutoff = (state.clock)() - state.session_expiry.as_millis() as i64;
    pins.retain(|p| p.last_used_at >= cutoff);
    pins.sort_by_key(|p| std::cmp::Reverse(p.last_used_at));
    Json(json!({ "sessions": pins.iter().map(|p| json!({
        "key_id": p.key_id, "session_id": p.session_id, "model": p.model, "account_id": p.account_id,
        "epoch": p.epoch, "created_at": p.created_at, "last_used_at": p.last_used_at,
    })).collect::<Vec<_>>() }))
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
