//! The dashboard: the last day from the rollups, the newest quota sample per account and window, the recent
//! requests, and the live pin count.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{dashboard, latest_quota};
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{ErrorBody, HourPointView};
use crate::requests::{request_summary, RequestSummary, ShareContext};
use crate::AdminState;

/// The ledger's dashboard rollup with the recent requests, the newest quota sample per account and window, and
/// the live pin count. The rollup reads the last 24 hours up to the end of the current hour and leaves the probe
/// rows out.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dashboard {
    /// The requests of the window.
    pub requests_24h: u64,
    /// The window's tokens of the five types: uncached input, cache reads, both cache writes and output.
    pub tokens_24h: u64,
    /// The window's cache-read tokens.
    pub tokens_cached_24h: u64,
    /// The window's estimated cost in dollars.
    pub cost_24h: f64,
    /// The requests that did not succeed over the requests, null with no request.
    pub error_rate_24h: Option<f64>,
    /// The window's most frequent error code, null with none.
    pub top_error_code: Option<String>,
    /// The window's 24 hours, oldest first: each hour's start as `at`, and its `cost`, `errors` (the requests that
    /// did not succeed), `requests` and `tokens`.
    pub hourly: Vec<HourPointView>,
    /// One row per recent request and attempt.
    pub recent: Vec<RequestSummary>,
    pub accounts: Vec<AccountWindows>,
    pub sessions: DashboardSessions,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountWindows {
    pub account_id: String,
    pub windows: Vec<DashboardWindow>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardWindow {
    pub id: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
    pub bucket: i64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardSessions {
    pub active_pins: usize,
}

/// Read the dashboard.
#[utoipa::path(
    get,
    path = "/admin/dashboard",
    operation_id = "dashboard_show",
    tag = "Dashboard",
    responses(
        (status = 200, description = "The dashboard.", body = Dashboard),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
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
    let mut accounts: std::collections::BTreeMap<String, Vec<DashboardWindow>> =
        std::collections::BTreeMap::new();
    for q in quota {
        accounts
            .entry(q.account_id.clone())
            .or_default()
            .push(DashboardWindow {
                id: q.window,
                utilization: q.utilization,
                resets_at: q.reset_at,
                bucket: q.bucket * 1_000,
            });
    }
    let pins = state.router.pins().await.map(|p| p.len()).unwrap_or(0);
    Json(Dashboard {
        requests_24h: board.requests_24h,
        tokens_24h: board.tokens_24h,
        tokens_cached_24h: board.tokens_cached_24h,
        cost_24h: board.cost_24h,
        error_rate_24h: board.error_rate_24h,
        top_error_code: board.top_error_code,
        hourly: board
            .hourly
            .iter()
            .map(|h| HourPointView {
                at: h.at,
                cost: h.cost,
                errors: h.errors,
                requests: h.requests,
                tokens: h.tokens,
            })
            .collect(),
        recent: board
            .recent
            .iter()
            .map(|(r, a)| request_summary(r, a, &shares))
            .collect(),
        accounts: accounts
            .into_iter()
            .map(|(account_id, windows)| AccountWindows {
                account_id,
                windows,
            })
            .collect(),
        sessions: DashboardSessions { active_pins: pins },
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::Dashboard;
    use crate::openapi::round_trip::round_trips;

    #[test]
    fn the_dashboard_fixture_round_trips_through_its_declared_type() {
        round_trips::<Dashboard>("dashboard");
    }
}
