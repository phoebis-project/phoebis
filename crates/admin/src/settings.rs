//! The bootstrap values the console reads, redacted: a setting's value, never a secret and never the name of the
//! variable that holds one.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::AdminState;

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    let s = &state.settings;
    let envelopes = crate::pricing::table_envelopes(&state.pricing.current(), &state.tables);
    Json(json!({
        "about": { "profile": s.profile, "version": env!("CARGO_PKG_VERSION") },
        "access": { "admin_configured": true },
        "api_prices": envelopes["api_prices"],
        "listeners": { "metrics": s.metrics_listen, "proxy": s.proxy_listen, "proxy_tls": s.proxy_tls },
        "retention": { "body_capture_max_hours": s.body_capture_max_hours },
        "sessions": { "expire_after_seconds": s.session_expire_after_seconds },
        "storage": {
            "api_prices_path": s.api_prices_path,
            "ledger_path": s.ledger_path,
            "path": s.storage_path,
            "subscription_costs_path": s.subscription_costs_path,
        },
        "subscription_costs": envelopes["subscription_costs"],
    }))
    .into_response()
}
