//! The keepalive's rows in the ledger: what a probe cost, which request a probe kept warm, what a stopped recipe
//! wasted and which due probe went unsent, as the rollup rows carry them, and the pricing a probe's cost reads.

use std::collections::BTreeMap;

use phoebis_ledger::{Band, KeepaliveDelta, PlanTables, Record, Speed, UNATTRIBUTED};
use phoebis_routing::{PinUsage, SEVEN_DAY};
use phoebis_store::AccountKind;
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::sessions::state::SessionKey;
use crate::warming::config::ONE_HOUR_MS;
use crate::warming::observation::{Credit, Settlement, Skipped};
use crate::warming::state::WarmingState;
use crate::warming::usage::ProbeUsage;

/// The routing crate's unit selector, so the keepalive and the shedding round never disagree on a pin's unit.
pub type SavingsUnit = phoebis_routing::BurnUnit;

/// The serving attempt's funding as the credit and the probe price it: one `Copy` value the saved forms convert at
/// `of` and `restore`, each saved form keeping its own members. The default is an attempt no account served yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AttemptFunding {
    /// The kind of the account the attempt was served on, `None` before any selection, which prices nothing.
    pub kind: Option<AccountKind>,
    /// Whether that account is a usage-based seat.
    pub usage_based: bool,
    /// Whether the attempt's answer reported overage in use.
    pub in_overage: bool,
}

/// The client label and the user agent a keepalive row keys, as the relay observed them on the request the row
/// describes. A saved charge flattens it, and a payload written before the pair rode the probe reads each as `None`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct RowClient {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// A probe's estimated cost as the share of the `tier`'s 7-day window at `at` (the band inside the price) under the
/// regime of the agent's `entrypoint`, the base one for `None`. `None` for a model the cost model gives no weight on
/// the window or an account tier it has no capacity for on it. A probe's dollars are its own request row's counts,
/// which the ledger prices.
pub(crate) fn probe_cost(
    tables: &PlanTables,
    entrypoint: Option<&str>,
    tier: Option<&str>,
    canonical_model: &str,
    usage: ProbeUsage,
    at: i64,
) -> Option<f64> {
    tables.share_at(
        SEVEN_DAY,
        entrypoint,
        tier?,
        canonical_model,
        typed(usage),
        at,
    )
}

/// A probe answer's tokens by type. An answer that reports the aggregate `cache_creation_input_tokens` and no TTL
/// breakdown is priced as five-minute writes, which is how the relay's own usage parser reads the same answer; the
/// estimate would otherwise miss a charge the request ledger recognizes.
pub(crate) fn typed(usage: ProbeUsage) -> PinUsage {
    let (write_5m, write_1h) = match (usage.five_minutes, usage.one_hour) {
        (0, 0) => (usage.write, 0),
        split => split,
    };
    PinUsage {
        input: usage.input,
        cache_read: usage.read,
        write_5m,
        write_1h,
        output: usage.output,
    }
}

/// `row` when the cost model in force at `at` gives the model no weight on the 7-day window, else `tier`: the only
/// two reasons left, since the loader refuses an anchor tier without a capacity on the unit's window and a weighted
/// model prices every type. Read under the regime of `entrypoint`.
pub(crate) fn unpriced_reason(
    tables: &PlanTables,
    entrypoint: Option<&str>,
    canonical_model: &str,
    at: i64,
) -> &'static str {
    if !tables.priced(
        SEVEN_DAY,
        entrypoint,
        &tables.revisions.in_force(at).unit.tier,
        canonical_model,
        at,
    ) {
        "row"
    } else {
        "tier"
    }
}

/// The unit a probe settles in: the share of the account's 7-day window where the cost model weighs the model on the
/// week and the account's tier carries a capacity on it, dollars otherwise, or for any other funding. A
/// `default_unit` other than the window share is that funding's own and passes through as it is. Read under the
/// regime of `entrypoint` on the cost model in force at `at`.
pub(crate) fn settlement_unit(
    default_unit: SavingsUnit,
    tables: &PlanTables,
    entrypoint: Option<&str>,
    tier: Option<&str>,
    canonical_model: &str,
    at: i64,
) -> SavingsUnit {
    let priced =
        tier.is_some_and(|tier| tables.priced(SEVEN_DAY, entrypoint, tier, canonical_model, at));
    if default_unit == SavingsUnit::WindowShare && !priced {
        SavingsUnit::Usd
    } else {
        default_unit
    }
}

/// One delta of a recipe's key. An account the recipe never reached takes the ledger's reserved id, the one every
/// reader groups an unattributed row under, since an empty string would read as an account of its own. A client the
/// carrier holds none of takes the reserved id too, as an agent restored from a save that predates its label does
/// until its next request. A `fast` delta files under speed `fast`: a credit or a skip at its recipe's speed, beside
/// the fast requests the recipe keeps warm, and a settlement at its probe's answer's, beside the probe's own request
/// row.
#[allow(clippy::too_many_arguments)]
fn delta(
    at: i64,
    key: &SessionKey,
    key_generation: i64,
    pool_id: &str,
    account: Option<&str>,
    canonical_model: &str,
    kind: &str,
    client: &RowClient,
    fast: bool,
    funding: AttemptFunding,
) -> KeepaliveDelta {
    KeepaliveDelta {
        at,
        key_id: key.0.clone(),
        key_generation,
        pool_id: pool_id.to_string(),
        account_id: account
            .filter(|a| !a.is_empty())
            .unwrap_or(UNATTRIBUTED)
            .to_string(),
        canonical_model: canonical_model.to_string(),
        kind: kind.to_string(),
        client: client.client.as_deref().unwrap_or(UNATTRIBUTED).to_string(),
        user_agent: client.user_agent.clone(),
        speed: if fast { Speed::Fast } else { Speed::Standard },
        account_kind: funding.kind,
        usage_based: funding.usage_based,
        overage_in_use: funding.in_overage,
        ..Default::default()
    }
}

impl Engine {
    /// One request the previous recipe kept warm, its keys read off the agent under the lock into `credit`, so nothing
    /// here needs the lock back. `tier` and `band` are its request row's, so the credit prices at the cells and the
    /// windows that row does.
    pub(crate) fn record_kept_warm(
        &self,
        credit: &Credit,
        request_id: &str,
        tier: Option<String>,
        band: Band,
    ) {
        let mut d = delta(
            credit.at,
            &credit.key,
            credit.key_generation,
            &credit.pool_id,
            credit.account.as_deref(),
            &credit.model,
            "inference",
            &credit.client,
            credit.fast,
            credit.funding,
        );
        d.kept_warm_request_id = Some(request_id.to_string());
        d.kept_warm_requests = 1;
        if credit.ttl_ms == ONE_HOUR_MS {
            d.kept_warm_1h_tokens = credit.read;
        } else {
            d.kept_warm_5m_tokens = credit.read;
        }
        d.tier = tier;
        d.band = band;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// One wasted probe, filed under the funding its own cost row took: the kind and the class it was served under and
    /// its answer's overage and speed. The row keys the probe's own exchange (its instant, its band and the account
    /// tier it ran under) rather than the settlement's clock, so a settlement filed hours or a day later prices its
    /// counts at the cells and the revision the probe ran at.
    pub(crate) fn record_settlement(&self, settlement: Settlement) {
        let charge = settlement.charge;
        let mut d = delta(
            charge.at,
            &settlement.key,
            settlement.key_generation,
            &settlement.pool_id,
            charge.account.as_deref(),
            &settlement.canonical_model,
            "probe",
            &charge.client,
            // The speed the probe's answer reported, so the waste prices at the cells and files under the funding its
            // own cost row took even when a fast recipe's answer came back at standard speed.
            charge.usage.speed == Speed::Fast,
            charge.funding,
        );
        let counts = typed(charge.usage);
        d.wasted_probes = 1;
        d.wasted_input_tokens = counts.input;
        d.wasted_cache_read_tokens = counts.cache_read;
        d.wasted_cache_write_5m_tokens = counts.write_5m;
        d.wasted_cache_write_1h_tokens = counts.write_1h;
        d.wasted_output_tokens = counts.output;
        d.wasted_probe_reasons = BTreeMap::from([(settlement.label.as_str().to_string(), 1)]);
        d.tier = charge.tier;
        d.band = charge.band;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    pub(crate) fn record_skipped(&self, at: i64, skip: &Skipped) {
        let mut d = delta(
            at,
            &skip.key,
            skip.key_generation,
            &skip.pool_id,
            skip.account.as_deref(),
            &skip.canonical_model,
            "probe",
            &skip.client,
            skip.fast,
            skip.funding,
        );
        d.probes_skipped = 1;
        self.parts.ledger.record(Record::Keepalive(d));
        self.parts
            .metrics
            .observe_skipped(&skip.pool_id, skip.state.as_str());
    }

    /// Drains what the last locked section queued: the settlements of stopped recipes and the probes a gate
    /// withheld. Called after every lock release by the entry points that can queue either.
    pub(crate) fn flush(&self) {
        let now = self.now();
        let (settlements, skipped) = self.lock().warming.take_ledger_rows();
        for settlement in settlements {
            self.record_settlement(settlement);
        }
        for skip in &skipped {
            self.record_skipped(now, skip);
        }
    }
}

impl WarmingState {
    /// The settlements and the skips the last locked section queued, for the flush to send once the lock is released.
    pub(crate) fn take_ledger_rows(&mut self) -> (Vec<Settlement>, Vec<Skipped>) {
        (
            std::mem::take(&mut self.settlements),
            std::mem::take(&mut self.skipped),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relay's own usage parser reads an aggregate `cache_creation_input_tokens` as five-minute writes,
    /// so a probe answer that carries only the aggregate costs what the same answer with the split costs.
    #[test]
    fn an_aggregate_only_cache_write_costs_a_five_minute_write() {
        let tables = PlanTables::shipped();
        let split = ProbeUsage {
            write: 2_000,
            five_minutes: 2_000,
            ..ProbeUsage::default()
        };
        let aggregate = ProbeUsage {
            write: 2_000,
            ..ProbeUsage::default()
        };
        assert_eq!(typed(aggregate), typed(split));
        let cost = |usage| {
            probe_cost(
                &tables,
                None,
                Some("default_claude_max_20x"),
                "claude-sonnet-5",
                usage,
                0,
            )
        };
        let priced = cost(split).expect("a priced model");
        assert!(priced > 0.0, "{priced}");
        assert_eq!(cost(aggregate), Some(priced));
    }

    #[test]
    fn an_unpriced_probe_reads_row_for_a_model_without_a_weight_and_tier_for_a_tier_without_a_week()
    {
        // The test table weighs Opus 5.5, leaves Opus 5 null, has no row for Opus 9 and places Mystery 2 in a tier with
        // no weighted model, so only Opus 5.5 has a weight of its own and the other three price nothing. Its Team row
        // sizes a 5-hour window and no week.
        let tables = phoebis_ledger::test_tables::subscription_costs();
        let at = phoebis_ledger::test_tables::JANUARY_2026;
        for model in ["claude-opus-5", "claude-opus-9", "claude-mystery-2"] {
            assert_eq!(unpriced_reason(&tables, None, model, at), "row", "{model}");
        }
        assert_eq!(
            unpriced_reason(&tables, None, "claude-opus-5-5", at),
            "tier"
        );
        assert_eq!(
            unpriced_reason(&tables, None, "claude-opus-5-5-20260401", at),
            "tier",
            "a dated id reads its undated weight"
        );
        assert_eq!(
            settlement_unit(
                SavingsUnit::WindowShare,
                &tables,
                None,
                Some("default_claude_team"),
                "claude-opus-5-5",
                at
            ),
            SavingsUnit::Usd,
            "a weighted model on a tier without a week settles in dollars, its reason the tier"
        );
    }

    #[test]
    fn a_delta_without_an_account_takes_the_ledgers_reserved_id() {
        let key = ("desktop".to_string(), "session".to_string());
        let of = |account| {
            delta(
                0,
                &key,
                0,
                "main",
                account,
                "claude-sonnet-5",
                "probe",
                &RowClient {
                    client: None,
                    user_agent: None,
                },
                false,
                AttemptFunding::default(),
            )
            .account_id
        };
        assert_eq!(of(None), UNATTRIBUTED, "a recipe with no account");
        assert_eq!(of(Some("")), UNATTRIBUTED, "a probe record that named none");
        assert_eq!(of(Some("a")), "a");
    }
}
