//! The test tables: a subscription cost model and a price file in round numbers, so a test works its expected
//! figure out by hand from the file rather than reading a shipped value back. Embedded, so a test in another crate
//! reads them without a path. Each carries two revisions, the second a factor of two from the first, so a test
//! crosses a boundary without writing a file.

use crate::{CostRevisions, PlanTables, PriceRevisions};

pub const SUBSCRIPTION_COSTS: &str = include_str!("../tests/fixtures/subscription_costs.yaml");
pub const API_PRICES: &str = include_str!("../tests/fixtures/api_prices.yaml");

/// An instant inside the fixtures' first entry, 2026-01-15 12:00 UTC, for a test that wants the first entry's figures.
pub const JANUARY_2026: i64 = 1_768_478_400_000;

/// An instant inside the fixtures' second entries and out of band, Sunday 2026-02-15 12:00 UTC.
pub const FEBRUARY_2026: i64 = 1_771_156_800_000;

pub fn subscription_costs() -> PlanTables {
    PlanTables {
        revisions: CostRevisions::from_yaml(SUBSCRIPTION_COSTS)
            .expect("the test subscription cost model parses"),
        path: None,
        source: "test".to_string(),
    }
}

pub fn api_prices() -> PriceRevisions {
    PriceRevisions::from_yaml(API_PRICES, "test").expect("the test price file parses")
}

/// The cost fixture's first entry as a file of its own, for a test that reads at an instant past the second entry's
/// date and wants the first entry's figures, or writes them with one edited.
pub fn first_entry_costs_text() -> String {
    let (first, _) = SUBSCRIPTION_COSTS
        .split_once("  - effective: 2026-02-01\n")
        .expect("the cost fixture carries a second entry");
    first.to_string()
}

/// `first_entry_costs_text` loaded.
pub fn first_entry_costs() -> CostRevisions {
    CostRevisions::from_yaml(&first_entry_costs_text()).expect("the first cost entry parses alone")
}
