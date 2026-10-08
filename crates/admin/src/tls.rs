//! `POST /admin/tls/reload`: the router re-reads every listener's certificate pair, and the route answers each
//! listener's outcome.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::routes::failure;
use crate::AdminState;

/// One public listener's reload outcome, as `POST /admin/tls/reload` answers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reloaded {
    Plain,
    Pair {
        cert_file: PathBuf,
        expires_ms: i64,
        days_left: i32,
    },
    Refused {
        error: String,
    },
}

/// What the admin crate needs of the router's TLS state: a reload of every pair, one outcome per public listener in
/// the listeners' order (`proxy`, then `admin`). Runs under `spawn_blocking`, since a reload reads files and builds
/// an acceptor.
pub trait TlsReload: Send + Sync {
    fn reload(&self, now: i64) -> Vec<(&'static str, Reloaded)>;
}

/// `200` whatever the outcomes: the files are the operator's and the router keeps serving, so a refusal is the
/// listener's outcome in the body rather than the route's status.
pub async fn reload(State(state): State<Arc<AdminState>>) -> Response {
    let Some(pairs) = state.tls_reload.clone() else {
        return Json(json!({ "listeners": [] })).into_response();
    };
    let now = (state.clock)();
    let outcomes = match tokio::task::spawn_blocking(move || pairs.reload(now)).await {
        Ok(outcomes) => outcomes,
        Err(error) => {
            tracing::error!(%error, "the certificate pairs' reload did not finish");
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the certificate pairs' reload did not finish",
            );
        }
    };
    let listeners: Vec<Value> = outcomes
        .into_iter()
        .map(|(listener, outcome)| listener_json(listener, outcome))
        .collect();
    Json(json!({ "listeners": listeners })).into_response()
}

fn listener_json(listener: &str, outcome: Reloaded) -> Value {
    match outcome {
        Reloaded::Plain => json!({ "listener": listener, "serving": "plain" }),
        Reloaded::Pair {
            cert_file,
            expires_ms,
            days_left,
        } => json!({
            "listener": listener,
            "serving": "tls",
            "reloaded": true,
            "cert_file": cert_file.display().to_string(),
            "expires": jiff::Timestamp::from_millisecond(expires_ms).ok().map(|at| at.to_string()),
            "days_left": days_left,
        }),
        Reloaded::Refused { error } => json!({
            "listener": listener,
            "serving": "tls",
            "reloaded": false,
            "error": error,
        }),
    }
}
