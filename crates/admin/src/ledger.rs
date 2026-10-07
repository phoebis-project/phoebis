//! The ledger's one control: the start-up replay of the parked priced records, on demand.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

/// `POST /admin/ledger/parked/replay`: `200 {"landed", "remaining"}`, or `500 ledger failure`.
pub(crate) async fn replay_parked(State(state): State<Arc<AdminState>>) -> Response {
    match state.ledger.replay_parked((state.clock)()).await {
        Ok(replayed) => Json(json!({
            "landed": replayed.landed,
            "remaining": replayed.remaining,
        }))
        .into_response(),
        Err(error) => {
            tracing::warn!(%error, "the parked records could not be replayed");
            failure(StatusCode::INTERNAL_SERVER_ERROR, "ledger failure")
        }
    }
}
