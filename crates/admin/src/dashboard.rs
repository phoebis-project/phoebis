//! The dashboard: the last day from the rollups and the live pin count.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::dashboard_on;
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{HourPointView, LedgerFailure};
use crate::AdminState;

/// The ledger's dashboard rollup and the live pin count. The rollup reads the last 24 hours up to the end of the
/// current hour and leaves the probe rows out.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dashboard {
    /// The requests of the window, not counting the ones the router refused before any account was asked, which
    /// `refused_24h` counts.
    pub requests_24h: u64,
    /// The window's tokens of the five types: uncached input, cache reads, both cache writes and output.
    pub tokens_24h: u64,
    /// The window's cache-read tokens.
    pub tokens_cached_24h: u64,
    /// The window's estimated cost in dollars.
    pub cost_24h: f64,
    /// The dollars of the window's billed rows alone (fast mode, overage, API key accounts and usage-based seats), a
    /// part of `cost_24h`.
    pub billed_24h: f64,
    /// The window's plan-funded rows as the Pro-week figure, whatever tier served them, null where a row priced
    /// nothing.
    pub equivalent_24h: Option<f64>,
    /// The requests that did not succeed, the ones a client canceled left out, over the requests, null with none. The
    /// refused requests are in neither.
    pub error_rate_24h: Option<f64>,
    /// The window's most frequent error code, null with none.
    pub top_error_code: Option<String>,
    /// The requests the router refused before any account was asked over the window, by the codes of the refusals
    /// map, not in `requests_24h`.
    pub refused_24h: u64,
    /// The window's most frequent refusal code, null with none.
    pub top_refusal_code: Option<String>,
    /// The window's 24 hours, oldest first: each hour's start as `at`, and its `billed`, `cost`, `equivalent`,
    /// `errors` (the requests that did not succeed, the ones a client canceled left out), `refused` (the requests the
    /// router refused before any account was asked, not in `requests`), `requests` and `tokens`.
    pub hourly: Vec<HourPointView>,
    pub sessions: DashboardSessions,
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
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let prices = state.pricing.revisions();
    let tables = state.tables.clone();
    let (board, pins) = tokio::join!(
        state
            .ledger
            .db()
            .read(move |c| dashboard_on(c, now, &prices, &tables)),
        state.router.pins_by_pool(),
    );
    let board = match board {
        Ok(board) => board,
        Err(e) => return ledger_failure(e),
    };
    let pins = pins
        .map(|by_pool| by_pool.iter().map(|(_, n)| *n as usize).sum())
        .unwrap_or(0);
    Json(Dashboard {
        requests_24h: board.requests_24h,
        tokens_24h: board.tokens_24h,
        tokens_cached_24h: board.tokens_cached_24h,
        cost_24h: board.cost_24h,
        billed_24h: board.billed_24h,
        equivalent_24h: board.equivalent_24h,
        error_rate_24h: board.error_rate_24h,
        top_error_code: board.top_error_code,
        refused_24h: board.refused_24h,
        top_refusal_code: board.top_refusal_code,
        hourly: board
            .hourly
            .iter()
            .map(|h| HourPointView {
                at: h.at,
                billed: h.billed,
                cost: h.cost,
                equivalent: h.equivalent,
                errors: h.errors,
                refused: h.refused,
                requests: h.requests,
                tokens: h.tokens,
            })
            .collect(),
        sessions: DashboardSessions { active_pins: pins },
    })
    .into_response()
}
