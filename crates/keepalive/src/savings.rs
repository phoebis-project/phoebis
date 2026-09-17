//! What a probe costs, at the canonical model's list prices. The accounting this feeds is an estimate in
//! money for an API key and a proxy for quota on a subscription account, which is why it is kept apart from
//! the ledger's own measured spend.

use phoebis_ledger::PriceTable;

use crate::usage::ProbeUsage;

/// A probe's estimated cost at list prices, for every account kind; a subscription account's is a proxy for
/// quota rather than money.
pub(crate) fn probe_cost(
    table: &PriceTable,
    canonical_model: &str,
    usage: ProbeUsage,
) -> Option<f64> {
    table
        .estimate(
            canonical_model,
            usage.input,
            usage.read,
            usage.five_minutes,
            usage.one_hour,
            usage.output,
        )
        .map(|(total, _)| total)
}
