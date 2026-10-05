//! The test tables: a subscription cost model and a price table in round numbers, so a test works its expected
//! figure out by hand from the file rather than reading a shipped value back. Embedded, so a test in another crate
//! reads them without a path.

use crate::{PlanTables, PriceTable, SubscriptionCosts};

pub const SUBSCRIPTION_COSTS: &str = include_str!("../tests/fixtures/subscription_costs.yaml");
pub const API_PRICES: &str = include_str!("../tests/fixtures/api_prices.yaml");

pub fn subscription_costs() -> PlanTables {
    PlanTables {
        costs: SubscriptionCosts::from_yaml(SUBSCRIPTION_COSTS)
            .expect("the test subscription cost model parses"),
        path: None,
        source: "test".to_string(),
    }
}

pub fn api_prices() -> PriceTable {
    PriceTable::from_yaml(API_PRICES, "test").expect("the test price table parses")
}
