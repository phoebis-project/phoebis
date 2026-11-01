//! The ledger's one control: the start-up replay of the parked priced records, on demand.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::LedgerError;
use utoipa::ToSchema;

use crate::openapi::ErrorBody;
use crate::routes::failure;
use crate::AdminState;

/// How long the replay route waits for the writer's turn.
pub const REPLAY_BOUND: Duration = Duration::from_secs(30);

/// The replay's count: `remaining` is the table's count afterwards. The system page's `ledger.parked_priced` and
/// the `ledger_record_parked` condition read the result.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Replay {
    pub landed: u64,
    pub remaining: u64,
}

/// Replay the parked priced records now.
///
/// Runs the start-up replay of the parked priced records on demand: every parked row once, in parking order, each in
/// its own transaction, run by the ledger's writer between two of its batches so it never interleaves with the
/// start-up pass, another replay or a landing, and followed by the members' spend refresh before the answer, so a
/// request after it is judged on the landed spend.
///
/// The route waits at most 30 seconds on the ledger's clock for the writer's turn. Past it, with the writer still on
/// a held batch, it answers `503 the ledger writer is held, try again` and replays nothing, then or later.
#[utoipa::path(
    post,
    path = "/admin/ledger/parked/replay",
    operation_id = "ledger_replay_parked",
    tag = "Ledger",
    responses(
        (status = 200, description = "The replay's count.", body = Replay),
        (status = 500, description = "`ledger failure` when the parked rows or their count cannot be read, or the ledger's writer has stopped. A pass that could not read the rows ran no replay, so it never answers as one that kept them.", body = ErrorBody, example = json!({"error": "ledger failure"})),
        (status = 503, description = "`the ledger writer is held, try again` when the 30 seconds pass before the writer's turn.", body = ErrorBody),
    ),
)]
pub(crate) async fn replay_parked(State(state): State<Arc<AdminState>>) -> Response {
    match state
        .ledger
        .replay_parked((state.clock)(), REPLAY_BOUND)
        .await
    {
        Ok(replayed) => Json(Replay {
            landed: replayed.landed,
            remaining: replayed.remaining,
        })
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

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::round_trips;
    use crate::openapi::ErrorBody;

    #[test]
    fn the_held_replay_refusal_round_trips_through_the_error_body() {
        round_trips::<ErrorBody>("replay_held");
    }
}
