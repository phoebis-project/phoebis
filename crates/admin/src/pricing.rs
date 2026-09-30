//! The price table in force.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{ModelPrice, ModelRow, PriceTable};
use serde_json::{json, Map, Value};

use crate::AdminState;

pub(crate) fn table_json(table: &PriceTable) -> Value {
    json!({
        "version": table.version, "effective": table.effective, "source": table.source,
        "models": table.models.iter().map(|(m, p)| model_json(m, p)).collect::<Vec<_>>(),
    })
}

fn model_json(model: &str, row: &ModelRow) -> Value {
    let mut out = Map::from_iter([("model".to_string(), Value::from(model))]);
    out.extend(cells_json(&row.standard));
    out.insert("fast".into(), fast_json(row));
    Value::Object(out)
}

/// The fast cells, null on a model that serves no fast mode.
pub(crate) fn fast_json(row: &ModelRow) -> Value {
    row.fast
        .as_ref()
        .map_or(Value::Null, |p| Value::Object(cells_json(p)))
}

/// The five cells under their `*_per_m` names, the standard and the fast ones alike.
fn cells_json(p: &ModelPrice) -> Map<String, Value> {
    Map::from_iter([
        ("input_per_m".to_string(), Value::from(p.input)),
        (
            "cache_write_5m_per_m".to_string(),
            Value::from(p.cache_write_5m),
        ),
        (
            "cache_write_1h_per_m".to_string(),
            Value::from(p.cache_write_1h),
        ),
        ("cache_read_per_m".to_string(), Value::from(p.cache_read)),
        ("output_per_m".to_string(), Value::from(p.output)),
    ])
}

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    Json(table_json(&state.pricing.current())).into_response()
}
