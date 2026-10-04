//! The conditions that hold now, worst first: what the dashboard's attention line and a pool card's attention rows
//! draw.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    // A failed read answers 500 and never an empty list, which the dashboard would draw as the quiet state.
    let mut held: Vec<_> = match state.attention.conditions((state.clock)()) {
        Ok(conditions) => conditions.into_iter().filter(|c| c.holds).collect(),
        Err(error) => {
            tracing::warn!(%error, "the attention read failed; the route answers 500");
            return failure(StatusCode::INTERNAL_SERVER_ERROR, "storage failure");
        }
    };
    held.sort_by(|a, b| (a.kind, a.since, &a.subject.id).cmp(&(b.kind, b.since, &b.subject.id)));
    let items: Vec<_> = held
        .into_iter()
        .map(|c| {
            json!({
                "kind": c.kind.as_str(),
                "subject": { "type": c.subject.kind.as_str(), "id": c.subject.id, "label": c.subject.label },
                "link": c.link.map(|l| json!({ "page": l.page, "id": l.id })),
                "pools": c.pools,
                "since": c.since,
                "detail": c.detail,
            })
        })
        .collect();
    Json(json!({ "items": items })).into_response()
}
