//! The live feed's two routes: the upgrade, whose preview right follows the session's role, and the past
//! snapshot for the range control.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use crate::routes::{failure, Caller};
use crate::{AdminState, Role};

const PAST_WINDOW_MS: i64 = 30 * 60 * 1_000;

pub(crate) async fn upgrade(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Ok(upgrade) = upgrade else {
        return failure(
            StatusCode::BAD_REQUEST,
            "this route upgrades to a WebSocket",
        );
    };
    // The permit a connection holds is released when hyper hands the socket to its upgrade task, so the
    // sockets carry their own bound.
    let Some(slot) = state.live.slot() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "live feed at capacity");
    };
    let preview = caller.role == Role::Admin;
    let live = state.live.clone();
    let sessions = state.sessions.clone();
    let clock = state.clock.clone();
    let session_id = caller.session_id;
    let alive: phoebis_live::SessionCheck =
        Box::new(move || sessions.is_live(&session_id, (clock)()));
    upgrade.on_upgrade(move |socket| async move { live.attach(socket, preview, alive, slot).await })
}

// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) async fn past(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let now = (state.clock)();
    let number = |name: &str, default: i64| -> Result<i64, Response> {
        q.get(name)
            .map(|v| {
                v.parse::<i64>().map_err(|_| {
                    failure(StatusCode::BAD_REQUEST, &format!("{name} is not a number"))
                })
            })
            .unwrap_or(Ok(default))
    };
    // The routes table names `at=` and the Live feed section `from=&to=`; both are served, never mixed.
    if q.contains_key("at") && (q.contains_key("from") || q.contains_key("to")) {
        return failure(StatusCode::BAD_REQUEST, "give at, or from and to, not both");
    }
    let (from, to) = if q.contains_key("at") {
        let at = match number("at", now) {
            Ok(a) => a,
            Err(r) => return r,
        };
        (at.saturating_sub(PAST_WINDOW_MS), at)
    } else {
        let to = match number("to", now) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let from = match number("from", to.saturating_sub(PAST_WINDOW_MS)) {
            Ok(f) => f,
            Err(r) => return r,
        };
        (from, to)
    };
    if from > to {
        return failure(StatusCode::BAD_REQUEST, "from must not be after to");
    }
    // A past read never carries a preview, whatever the role: the feed strips it before the entries leave.
    Json(state.live.past(from, to)).into_response()
}
