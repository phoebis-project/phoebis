//! The keepalive mod's one route, on the proxy listener under the API key: the mod's report in, its acknowledgment
//! or its refusal code out.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use phoebis_relay::{authenticated_key, is_json, new_request_id, no_store_answer};

use crate::engine::Engine;
use crate::events::{decode_json_event, err, EventError};

pub const EVENTS_PATH: &str = "/v0/companion/events";
pub const MAX_BODY_BYTES: usize = 16 << 10;

pub fn events_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(EVENTS_PATH, post(events))
        .layer(map_response(no_store_answer))
        .with_state(engine)
}

/// The key the request presents, resolved exactly as `/v1/messages` resolves it, with the generation it was
/// created under: a session belongs to the pair, so a key id created again reaches none of the previous
/// holder's. The refusal bodies are the relay's, so a companion pointed at the wrong port sees the same 401
/// the client would.
#[allow(clippy::result_large_err)]
fn authenticate(engine: &Engine, request: &Request<Body>) -> Result<(String, i64), Response> {
    let request_id = new_request_id();
    let snapshot = engine.parts.router.snapshot();
    authenticated_key(&snapshot, request.headers(), engine.now(), &request_id)
        .map(|key| (key.id.clone(), key.created_at))
}

pub(crate) fn json(status: StatusCode, body: String) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body).into_response()
}

fn event_error(error: EventError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST);
    json(status, serde_json::to_string(&error).unwrap_or_default())
}

async fn events(State(engine): State<Arc<Engine>>, request: Request<Body>) -> Response {
    let (key_id, key_generation) = match authenticate(&engine, &request) {
        Ok(key) => key,
        Err(refused) => return refused,
    };
    if !is_json(request.headers()) {
        return event_error(err("invalid_event", 400));
    }
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return event_error(err("event_too_large", 413)),
    };
    let event = match decode_json_event(&body) {
        Ok(event) => event,
        Err(error) => return event_error(error),
    };
    match engine.apply_event(&key_id, key_generation, event) {
        Ok(applied) => {
            if applied.filed_facts {
                // Not awaited: the answer goes out at once, and the one save worker takes the request from here.
                engine.save_soon();
            }
            json(
                StatusCode::OK,
                serde_json::to_string(&applied.ack).unwrap_or_default(),
            )
        }
        Err(error) => event_error(error),
    }
}
