//! The price table and the subscription cost model in force.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{ModelRow, PlanTables, PriceRevisions, SubscriptionCosts};
use serde_json::{json, Value};
use utoipa::ToSchema;

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

/// The long-context set under the `*_per_m` names with its `over`, null on a model priced one way.
pub(crate) fn long_context_json(row: &ModelRow) -> Value {
    row.long_context.as_ref().map_or(Value::Null, |l| {
        json!({
            "over": l.over,
            "input_per_m": l.cells.input,
            "cache_write_5m_per_m": l.cells.cache_write_5m,
            "cache_write_1h_per_m": l.cells.cache_write_1h,
            "cache_read_per_m": l.cells.cache_read,
            "output_per_m": l.cells.output,
        })
    })
}

/// The two tables in force, one envelope per table. No envelope carries a `path`: the settings route's `storage` and
/// the system document name them.
#[derive(serde::Serialize, ToSchema)]
pub struct Envelopes {
    pub api_prices: Envelope<PriceTableContent>,
    pub subscription_costs: Envelope<SubscriptionCostsContent>,
}

/// One table's envelope, its `content` the file as loaded, key for key.
// The sentence on `content` sits on the type, since utoipa renders a generic field as its argument's schema and
// drops the field's own description.
#[derive(serde::Serialize, ToSchema)]
pub struct Envelope<T: ToSchema> {
    /// The table's date.
    pub effective: String,
    /// `shipped`, or `file` for the operator's file.
    #[schema(example = "shipped")]
    pub source: String,
    pub content: T,
}

/// The price table's `effective` and `models`.
#[derive(serde::Serialize, ToSchema)]
pub struct PriceTableContent {
    pub effective: String,
    /// Each model id to its `input`, `cache_write_5m`, `cache_write_1h`, `cache_read` and `output` cells, and
    /// `fast` and `long_context` where the model has them, in the file's order.
    #[schema(value_type = BTreeMap<String, crate::openapi::ModelRowView>)]
    pub models: indexmap::IndexMap<String, ModelRow>,
}

// The ledger's own type stays in the field so the bytes are its serializer's, and the schema is the mirror's.
#[derive(serde::Serialize, ToSchema)]
#[schema(value_type = crate::openapi::SubscriptionCostsView)]
pub struct SubscriptionCostsContent(pub SubscriptionCosts);

/// The two tables' envelopes, each the revision in force at `now`, as the price route answers them and the settings
/// echo carries them.
pub fn table_envelopes(prices: &PriceRevisions, tables: &PlanTables, now: i64) -> Envelopes {
    let table = prices.in_force(now);
    let costs = tables.revisions.in_force(now);
    Envelopes {
        api_prices: Envelope {
            effective: table.effective.clone(),
            source: prices.source.clone(),
            content: PriceTableContent {
                effective: table.effective.clone(),
                models: table.models.clone(),
            },
        },
        subscription_costs: Envelope {
            effective: costs.effective.clone(),
            source: tables.source.clone(),
            content: SubscriptionCostsContent(costs.clone()),
        },
    }
}

/// Read the price table and the subscription cost model in force.
#[utoipa::path(
    get,
    path = "/admin/pricing",
    operation_id = "pricing_show",
    tag = "Pricing",
    responses(
        (status = 200, description = "The two envelopes.", body = Envelopes),
    ),
)]
pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    Json(table_envelopes(
        &state.pricing.revisions(),
        &state.tables,
        (state.clock)(),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::round_trips_each;
    use crate::openapi::{ModelRowView, SubscriptionCostsView};

    #[test]
    fn the_pricing_fixture_round_trips_through_the_mirrors() {
        round_trips_each::<ModelRowView>("pricing", &["api_prices", "content", "models", "*"]);
        round_trips_each::<SubscriptionCostsView>("pricing", &["subscription_costs", "content"]);
    }
}
