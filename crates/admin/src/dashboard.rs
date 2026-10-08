//! The dashboard: the last day from the rollups, the newest quota sample per account and window, the recent
//! requests, and the live pin count.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{dashboard, latest_quota};
use serde_json::json;

use crate::analytics::ledger_failure;
use crate::requests::{request_summary, ShareContext};
use crate::AdminState;

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let shares = match ShareContext::read(&state) {
        Ok(shares) => shares,
        Err(refusal) => return refusal,
    };
    let board = match dashboard(state.ledger.db(), now).await {
        Ok(board) => board,
        Err(e) => return ledger_failure(e),
    };
    let quota = match state.ledger.db().read(move |c| latest_quota(c, now)).await {
        Ok(quota) => quota,
        Err(e) => return ledger_failure(e),
    };
    let mut accounts: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for q in quota {
        accounts.entry(q.account_id.clone()).or_default().push(json!({ "id": q.window, "utilization": q.utilization, "resets_at": q.reset_at, "bucket": q.bucket * 1_000 }));
    }
    let pins = state.router.pins().await.map(|p| p.len()).unwrap_or(0);
    let mut body = serde_json::to_value(&board).expect("the dashboard serializes");
    body["recent"] = json!(board
        .recent
        .iter()
        .map(|(r, a)| request_summary(r, a, &shares))
        .collect::<Vec<_>>());
    body["accounts"] = json!(accounts
        .into_iter()
        .map(|(id, windows)| json!({ "account_id": id, "windows": windows }))
        .collect::<Vec<_>>());
    body["sessions"] = json!({ "active_pins": pins });
    Json(body).into_response()
}
