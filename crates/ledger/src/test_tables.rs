//! The test tables: a subscription cost model and a price table in round numbers, so a test works its expected
//! figure out by hand from the file rather than reading a shipped value back. Embedded, so a test in another crate
//! reads them without a path.

use std::cmp::Reverse;

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

/// Whether `ids` keep the order both shipped files list their models in: Fable, Opus, Sonnet, Haiku, then anything
/// else, and within a family the newer version first.
pub fn in_family_order(ids: &[&str]) -> bool {
    ids.windows(2)
        .all(|pair| family_rank(pair[0]) <= family_rank(pair[1]))
}

fn family_rank(model: &str) -> (usize, Reverse<Vec<u32>>) {
    let rest = model.strip_prefix("claude-").unwrap_or(model);
    let (family, version) = rest.split_once('-').unwrap_or((rest, ""));
    let rank = ["fable", "opus", "sonnet", "haiku"]
        .iter()
        .position(|f| *f == family)
        .unwrap_or(4);
    let version = version.split('-').filter_map(|p| p.parse().ok()).collect();
    (rank, Reverse(version))
}
