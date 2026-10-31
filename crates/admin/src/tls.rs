//! `POST /admin/tls/reload`: the router re-reads every listener's certificate pair, and the route answers each
//! listener's outcome.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use utoipa::ToSchema;

use crate::openapi::ErrorBody;
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

/// One object per public listener, `proxy` then `admin`.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsReloaded {
    pub listeners: Vec<ListenerOutcome>,
}

/// A listener's outcome: a pair that loaded, a pair that was refused, or a plain listener.
// Every outcome is a closed object and the most specific comes first, so the untagged walk lands a payload on the one
// shape whose field set it has instead of the first whose fields it covers.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(untagged)]
pub(crate) enum ListenerOutcome {
    Pair(PairOutcome),
    Refused(RefusedOutcome),
    Plain(PlainOutcome),
}

/// `serving` `tls` and `reloaded` true: the pair loaded and serves the next connection. `days_left` is the whole days
/// until the new leaf's `not_after`.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairOutcome {
    #[schema(example = "proxy")]
    pub listener: String,
    #[schema(example = "tls")]
    pub serving: String,
    pub reloaded: bool,
    pub cert_file: String,
    /// The new leaf's `not_after` as an RFC 3339 UTC stamp. Null where that instant lies past the latest instant the
    /// router writes as a stamp (late on 9999-12-30 UTC), as RFC 5280's no-expiry `99991231235959Z` does.
    pub expires: Option<String>,
    pub days_left: i32,
}

/// `serving` `tls` and `reloaded` false: the pair did not load, `error` is the line the start would print for it,
/// and the listener keeps serving the pair it had.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefusedOutcome {
    #[schema(example = "proxy")]
    pub listener: String,
    #[schema(example = "tls")]
    pub serving: String,
    pub reloaded: bool,
    pub error: String,
}

/// `serving` `plain`: the listener has no pair to reload.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlainOutcome {
    #[schema(example = "proxy")]
    pub listener: String,
    #[schema(example = "plain")]
    pub serving: String,
}

/// Reload the certificate pairs.
///
/// No body. Re-reads the certificate pair of each public listener that serves one, from the paths `tls` named at
/// start, through the loader and the refusals the start applies, in the listeners' order. A connection already open
/// finishes on the pair it took. Answers `200` whatever the outcomes: the files are the operator's and the router
/// keeps serving. The body carries each refusal. A refused pair reads `{"listener": "proxy", "serving": "tls",
/// "reloaded": false, "error": "proxy.tls.key_file /etc/phoebis/proxy.key: does not match the certificate in
/// /etc/phoebis/proxy.pem"}`.
#[utoipa::path(
    post,
    path = "/admin/tls/reload",
    operation_id = "tls_reload",
    tag = "System",
    responses(
        (status = 200, description = "The listeners' outcomes.", body = TlsReloaded, example = json!({"listeners": [{"listener": "proxy", "serving": "tls", "reloaded": true, "cert_file": "/etc/phoebis/proxy.pem", "expires": "2027-10-05T12:00:00Z", "days_left": 364}, {"listener": "admin", "serving": "plain"}]})),
        (status = 500, description = "`the certificate pairs' reload did not finish` when the reload stopped before it answered.", body = ErrorBody, example = json!({"error": "the certificate pairs' reload did not finish"})),
    ),
)]
pub async fn reload(State(state): State<Arc<AdminState>>) -> Response {
    let Some(pairs) = state.tls_reload.clone() else {
        return Json(TlsReloaded {
            listeners: Vec::new(),
        })
        .into_response();
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
    let listeners: Vec<ListenerOutcome> = outcomes
        .into_iter()
        .map(|(listener, outcome)| listener_json(listener, outcome))
        .collect();
    Json(TlsReloaded { listeners }).into_response()
}

fn listener_json(listener: &str, outcome: Reloaded) -> ListenerOutcome {
    match outcome {
        Reloaded::Plain => ListenerOutcome::Plain(PlainOutcome {
            listener: listener.into(),
            serving: "plain".into(),
        }),
        Reloaded::Pair {
            cert_file,
            expires_ms,
            days_left,
        } => ListenerOutcome::Pair(PairOutcome {
            listener: listener.into(),
            serving: "tls".into(),
            reloaded: true,
            cert_file: cert_file.display().to_string(),
            expires: jiff::Timestamp::from_millisecond(expires_ms)
                .ok()
                .map(|at| at.to_string()),
            days_left,
        }),
        Reloaded::Refused { error } => ListenerOutcome::Refused(RefusedOutcome {
            listener: listener.into(),
            serving: "tls".into(),
            reloaded: false,
            error,
        }),
    }
}
