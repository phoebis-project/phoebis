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
    let table = state.pricing.current();
    let tables = &state.tables;
    Json(json!({
        "about": { "profile": s.profile, "version": env!("CARGO_PKG_VERSION") },
        "access": { "admin_configured": true },
        "keepalive": s.keepalive,
        "listeners": { "metrics": s.metrics_listen, "proxy": s.proxy_listen },
        "plan_tables": { "cost": tables.cost },
        "pricing": crate::pricing::table_json(&table),
        "retention": { "body_capture_max_hours": s.body_capture_max_hours },
        "sessions": { "expire_after_seconds": s.session_expire_after_seconds },
        "storage": { "ledger_path": s.ledger_path, "path": s.storage_path, "pricing_path": s.pricing_path },
        "usage": { "cache_ttl_seconds": s.usage_cache_ttl_seconds },
    }))
    .into_response()
}
