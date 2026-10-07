//! The price table and the subscription cost model in force.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{ModelRow, PlanTables, PriceTable};
use serde_json::{json, Value};

use crate::AdminState;

/// The fast cells under their `*_per_m` names, null on a model that serves no fast mode.
pub(crate) fn fast_json(row: &ModelRow) -> Value {
    row.fast.as_ref().map_or(Value::Null, |p| {
        json!({
            "input_per_m": p.input,
            "cache_write_5m_per_m": p.cache_write_5m,
            "cache_write_1h_per_m": p.cache_write_1h,
            "cache_read_per_m": p.cache_read,
            "output_per_m": p.output,
        })
    })
}

/// The two tables in force, one envelope per table, each file as loaded with its models in the file's order:
///
/// ```text
/// {"api_prices": {effective, source, content: {effective, models: {<id>: {input, cache_write_5m, cache_write_1h,
/// cache_read, output, fast?}}}}, "subscription_costs": {effective, source, content: <the model, key for key>}}
/// ```
pub fn table_envelopes(prices: &PriceTable, tables: &PlanTables) -> Value {
    json!({
        "api_prices": {
            "effective": prices.effective,
            "source": prices.source,
            "content": { "effective": prices.effective, "models": prices.models },
        },
        "subscription_costs": {
            "effective": tables.costs.effective,
            "source": tables.source,
            "content": &tables.costs,
        },
    })
}

pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    Json(table_envelopes(&state.pricing.current(), &state.tables)).into_response()
}
