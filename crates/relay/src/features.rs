//! The feature listener's one handler. The listener and no header picks this table, so a path of the upstream's and a
//! path of the router's own never meet in one set of routes.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::Router;

use crate::auth::resolve_key;
use crate::errors::FEATURE_NOT_ALLOWED;
use crate::messages::{new_request_id, Relay};

/// The feature listener's routes. Each request on that listener is a feature endpoint request, whatever its path.
pub fn router(relay: Arc<Relay>) -> Router {
    Router::new().fallback(feature_call).with_state(relay)
}

/// The key first, so a caller without one learns nothing of what the router forwards. The body is never read: no
/// answer here depends on it.
async fn feature_call(
    State(relay): State<Arc<Relay>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let snapshot = relay.router().snapshot();
    let key = match resolve_key(&snapshot, &headers, (relay.clock)()) {
        Ok((key, _)) => key,
        Err(failure) => return failure.response(&new_request_id()),
    };
    tracing::debug!(
        code = FEATURE_NOT_ALLOWED.code,
        %method,
        path = uri.path(),
        key = %key.label,
        "a feature call was refused"
    );
    FEATURE_NOT_ALLOWED.response(&new_request_id())
}
