//! The price table in force.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{ModelPrice, PriceTable};
use serde_json::{json, Value};

use crate::AdminState;

pub(crate) fn table_json(table: &PriceTable) -> Value {
    json!({
        "version": table.version, "effective": table.effective, "source": table.source,
        "models": table.models.iter().map(|(m, p)| model_json(m, p)).collect::<Vec<_>>(),
    })
}

fn model_json(model: &str, p: &ModelPrice) -> Value {
    json!({ "model": model, "input_per_m": p.input, "cache_write_5m_per_m": p.cache_write_5m, "cache_write_1h_per_m": p.cache_write_1h, "cache_read_per_m": p.cache_read, "output_per_m": p.output })
}

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    Json(table_json(&state.pricing.current())).into_response()
}
