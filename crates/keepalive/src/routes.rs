//! The companion's two routes, on the proxy listener under the API key.
//! The companion's transport change is the bearer header alone.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use phoebis_relay::{auth_error_response, key_refusal_response, new_request_id, presented_key};

use crate::engine::Engine;
use crate::events::{decode_query_event, EventError};

pub const EVENTS_PATH: &str = "/v0/resource/plugins/cache-keepalive/events";
pub const SNAPSHOT_PATH: &str = "/v0/resource/plugins/cache-keepalive/snapshot";

pub fn companion_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(EVENTS_PATH, get(events))
        .route(SNAPSHOT_PATH, get(snapshot))
        .with_state(engine)
}

/// The key the request presents, resolved exactly as `/v1/messages` resolves it; the refusal bodies are the
/// relay's, so a companion pointed at the wrong port sees the same 401 the client would.
#[allow(clippy::result_large_err)]
fn authenticate(engine: &Engine, request: &Request<Body>) -> Result<String, Response> {
    let request_id = new_request_id();
    let presented = presented_key(request.headers())
        .map_err(|error| no_store(auth_error_response(&error, &request_id)))?;
    let snapshot = engine.parts.router.snapshot();
    snapshot
        .authenticate(presented, engine.now())
        .map(|key| key.id.clone())
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
    json(status, format!(r#"{{"error":"{}"}}"#, error.code))
}

async fn events(State(engine): State<Arc<Engine>>, request: Request<Body>) -> Response {
    let key_id = match authenticate(&engine, &request) {
        Ok(key_id) => key_id,
        Err(refused) => return refused,
    };
    let event = match decode_query_event(request.uri().query().unwrap_or("")) {
        Ok(event) => event,
        Err(error) => return event_error(error),
    };
    match engine.apply_event(&key_id, event) {
        Ok(ack) => json(
            StatusCode::OK,
            serde_json::to_string(&ack).unwrap_or_default(),
        ),
        Err(error) => event_error(error),
    }
}

async fn snapshot(State(engine): State<Arc<Engine>>, request: Request<Body>) -> Response {
    let key_id = match authenticate(&engine, &request) {
        Ok(key_id) => key_id,
        Err(refused) => return refused,
    };
    json(
        StatusCode::OK,
        serde_json::to_string(&engine.snapshot(&key_id)).unwrap_or_default(),
    )
}
