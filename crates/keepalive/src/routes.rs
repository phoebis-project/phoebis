//! The keepalive mod's route, on the proxy listener under the API key, and the snapshot route beside it.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use phoebis_relay::{auth_error_response, key_refusal_response, new_request_id, presented_key};

use crate::engine::Engine;
use crate::events::{decode_json_event, err, EventError};

pub const EVENTS_PATH: &str = "/v0/keepalive/events";
pub const SNAPSHOT_PATH: &str = "/v0/resource/plugins/cache-keepalive/snapshot";
pub const MAX_BODY_BYTES: usize = 16 << 10;

pub fn keepalive_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(EVENTS_PATH, post(events))
        .route(SNAPSHOT_PATH, get(snapshot))
        .with_state(engine)
}

/// The key the request presents, resolved exactly as `/v1/messages` resolves it, with the generation it was
/// created under: a session belongs to the pair, so a key id created again reaches none of the previous
/// holder's. The refusal bodies are the relay's, so a companion pointed at the wrong port sees the same 401
/// the client would.
#[allow(clippy::result_large_err)]
fn authenticate(engine: &Engine, request: &Request<Body>) -> Result<(String, i64), Response> {
    let request_id = new_request_id();
    let presented = presented_key(request.headers())
        .map_err(|error| no_store(auth_error_response(&error, &request_id)))?;
    let snapshot = engine.parts.router.snapshot();
    snapshot
        .authenticate(presented, engine.now())
        .map(|(key, _)| (key.id.clone(), key.created_at))
        .map_err(|refusal| no_store(key_refusal_response(&refusal, &request_id)))
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn json(status: StatusCode, body: String) -> Response {
    no_store((status, [(CONTENT_TYPE, "application/json")], body).into_response())
}

fn event_error(error: EventError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST);
    json(status, serde_json::to_string(&error).unwrap_or_default())
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
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
        Ok(ack) => json(
            StatusCode::OK,
            serde_json::to_string(&ack).unwrap_or_default(),
        ),
        Err(error) => event_error(error),
    }
}

async fn snapshot(State(engine): State<Arc<Engine>>, request: Request<Body>) -> Response {
    let (key_id, key_generation) = match authenticate(&engine, &request) {
        Ok(key) => key,
        Err(refused) => return refused,
    };
    json(
        StatusCode::OK,
        serde_json::to_string(&engine.snapshot(&key_id, key_generation)).unwrap_or_default(),
    )
}
