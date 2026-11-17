//! The feature calls: one row per call the router sent to a feature host under an account.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{list_feature_calls, FeatureCallCursor, FeatureCallFilter, FeatureCallRow};
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{ErrorBody, LedgerFailure};
use crate::requests::{list_param, DEFAULT_LIMIT, MAX_LIMIT};
use crate::routes::failure;
use crate::AdminState;

/// One page of the feature calls.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeatureCallList {
    pub feature_calls: Vec<FeatureCallEntry>,
    /// Set only while a row lies beyond the page.
    pub next_cursor: Option<String>,
}

/// One call the router sent to a feature host under an account.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeatureCallEntry {
    pub account_id: String,
    pub duration_ms: i64,
    /// The router's code where it answered a forward that could not complete.
    pub error_code: Option<String>,
    /// `artifacts` or `connectors`.
    pub feature: String,
    pub id: String,
    pub key_id: String,
    pub method: String,
    /// `canceled`, `failed` or `forwarded`.
    pub outcome: String,
    /// The path without its query.
    pub path: String,
    pub received_at: i64,
    /// Null where no upstream head arrived.
    pub status: Option<u16>,
}

/// `GET /admin/feature-calls`'s query.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct FeatureCallsQuery {
    /// A comma list of account ids.
    pub accounts: Option<String>,
    /// The `next_cursor` of the page before, `<received_at>:<id>`.
    pub cursor: Option<String>,
    /// A comma list of feature words, `artifacts` and `connectors`.
    pub features: Option<String>,
    /// Unix milliseconds: the rows received at or after it.
    pub from: Option<i64>,
    /// A comma list of key ids.
    pub keys: Option<String>,
    /// Default 50, clamped to 1 through 500.
    pub limit: Option<u32>,
    /// A comma list of outcomes, `canceled`, `failed` and `forwarded`.
    pub outcomes: Option<String>,
    /// Unix milliseconds: the rows received at or before it.
    pub to: Option<i64>,
}

/// List the feature calls.
#[utoipa::path(
    get,
    path = "/admin/feature-calls",
    operation_id = "feature_calls_list",
    tag = "Requests",
    params(FeatureCallsQuery),
    responses(
        (status = 200, description = "The page, newest first by `received_at` and, inside one `received_at`, by `id` descending.", body = FeatureCallList),
        (status = 400, description = "- `{name} is not a number`, for `from` or `to`\n- `limit is not a number`\n- `cursor is <received_at>:<id>`", body = ErrorBody, example = json!({"error": "{name} is not a number"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn list(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let number = |name: &'static str| match q.get(name) {
        Some(v) => v.parse::<i64>().map(Some).map_err(|_| name),
        None => Ok(None),
    };
    let (from, to) = match (number("from"), number("to")) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(name), _) | (_, Err(name)) => {
            return failure(StatusCode::BAD_REQUEST, &format!("{name} is not a number"))
        }
    };
    let limit = match q.get("limit") {
        None => DEFAULT_LIMIT,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) => n,
            Err(_) => return failure(StatusCode::BAD_REQUEST, "limit is not a number"),
        },
    }
    .clamp(1, MAX_LIMIT);
    let cursor = match q.get("cursor").map(|c| FeatureCallCursor::parse(c)) {
        None => None,
        Some(Some(cursor)) => Some(cursor),
        Some(None) => return failure(StatusCode::BAD_REQUEST, "cursor is <received_at>:<id>"),
    };
    let filter = FeatureCallFilter {
        accounts: list_param(q.get("accounts").map(String::as_str)),
        features: list_param(q.get("features").map(String::as_str)),
        from,
        keys: list_param(q.get("keys").map(String::as_str)),
        outcomes: list_param(q.get("outcomes").map(String::as_str)),
        to,
    };
    match state
        .ledger
        .db()
        .read(move |c| list_feature_calls(c, &filter, cursor.as_ref(), limit))
        .await
    {
        Ok((rows, next_cursor)) => Json(FeatureCallList {
            feature_calls: rows.into_iter().map(entry).collect(),
            next_cursor,
        })
        .into_response(),
        Err(e) => ledger_failure(e),
    }
}

fn entry(row: FeatureCallRow) -> FeatureCallEntry {
    FeatureCallEntry {
        account_id: row.account_id,
        duration_ms: row.duration_ms,
        error_code: row.error_code,
        feature: row.feature,
        id: row.id,
        key_id: row.key_id,
        method: row.method,
        outcome: row.outcome,
        path: row.path,
        received_at: row.received_at,
        status: row.status.and_then(|status| u16::try_from(status).ok()),
    }
}
