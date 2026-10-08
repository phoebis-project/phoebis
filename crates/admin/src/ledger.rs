//! The ledger's one control: the start-up replay of the parked priced records, on demand.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::LedgerError;
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

/// How long the replay route waits for the writer's turn.
pub const REPLAY_BOUND: Duration = Duration::from_secs(30);

/// `POST /admin/ledger/parked/replay`: `200 {"landed", "remaining"}`, `503 the ledger writer is held, try again` once
/// `REPLAY_BOUND` passes on the ledger's clock without the writer's turn, or `500 ledger failure`.
pub(crate) async fn replay_parked(State(state): State<Arc<AdminState>>) -> Response {
    match state
        .ledger
        .replay_parked((state.clock)(), REPLAY_BOUND)
        .await
    {
        Ok(replayed) => Json(json!({
            "landed": replayed.landed,
            "remaining": replayed.remaining,
        }))
        .into_response(),
        Err(error @ LedgerError::WriterHeld) => {
            failure(StatusCode::SERVICE_UNAVAILABLE, &error.to_string())
        }
        Err(error) => {
            tracing::warn!(%error, "the parked records could not be replayed");
            failure(StatusCode::INTERNAL_SERVER_ERROR, "ledger failure")
        }
    }
}
