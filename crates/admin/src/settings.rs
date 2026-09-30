//! The effective bootstrap values, redacted: a setting's value, never a secret and never the name of the
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
        "about": { "profile": s.profile, "version": env!("CARGO_PKG_VERSION"), "public_url": s.admin_origin },
        "access": { "admin_configured": true, "viewer_configured": s.viewer_configured },
        "compatibility": { "cache_diagnostics": s.cache_diagnostics, "profile": s.profile },
        "keepalive": s.keepalive,
        "listeners": { "admin": s.admin_listen, "metrics": s.metrics_listen, "proxy": s.proxy_listen },
        "plan_tables": { "cost": tables.cost, "tiers": tables.tiers },
        "pricing": crate::pricing::table_json(&table),
        "quota": { "sample_retention_days": s.quota_sample_retention_days },
        "retention": { "body_capture_max_hours": s.body_capture_max_hours, "ledger_days": s.retention_days, "quota_samples_days": s.quota_sample_retention_days, "rollup_days": s.rollup_retention_days },
        "routing": { "catalog_cache_seconds": s.catalog_cache_seconds, "catalog_refresh_seconds": s.catalog_refresh_seconds, "cooldown_fallback_seconds": s.cooldown_fallback_seconds, "max_attempts": s.max_attempts, "model_rejection_seconds": s.model_rejection_seconds, "startup_fetch_seconds": s.startup_fetch_seconds },
        "sessions": { "expire_after_seconds": s.session_expire_after_seconds },
        "storage": { "ledger_path": s.ledger_path, "path": s.storage_path, "pricing_path": s.pricing_path },
        "usage": { "cache_ttl_seconds": s.usage_cache_ttl_seconds, "refresh_concurrency": s.usage_refresh_concurrency },
    }))
    .into_response()
}
