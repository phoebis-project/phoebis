//! The keepalive's rows in the ledger: what a probe cost, which request a probe kept warm, what a stopped recipe
//! wasted and which due probe went unsent, as the rollup rows carry them, and the pricing a probe's cost reads.

use std::collections::BTreeMap;

use phoebis_ledger::{KeepaliveDelta, PlanTables, Record, Speed, UNATTRIBUTED};
use phoebis_routing::{PinUsage, FIVE_HOUR};
use phoebis_store::AccountKind;

use crate::engine::{Engine, SavingsUnit, SessionKey, Settlement, Skipped};
use crate::probe::{Applied, ProbeOutcome, ProbeRecord};
use crate::usage::ProbeUsage;

/// A probe's estimated cost as the share of the `tier`'s 5-hour window at `at` (the band inside the price). `None` for
/// a model the subscription cost model gives no weight, its own or its model tier's latest, or an account tier it has
/// no capacity for on the window. A probe's dollars are its own request row's counts, which the ledger prices.
pub(crate) fn probe_cost(
    tables: &PlanTables,
    tier: Option<&str>,
    canonical_model: &str,
    usage: ProbeUsage,
    at: i64,
) -> Option<f64> {
    tables.share_at(FIVE_HOUR, tier?, canonical_model, typed(usage), at)
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

/// `row` when the cost model in force at `at` gives the model no weight on the 5-hour window, neither its own nor its
/// model tier's latest, else `tier`: the only two reasons left, since the loader refuses an anchor tier without a
/// capacity on either window and a weighted model prices every type.
pub(crate) fn unpriced_reason(tables: &PlanTables, canonical_model: &str, at: i64) -> &'static str {
    if !tables.priced(
        FIVE_HOUR,
        &tables.revisions.in_force(at).unit.defined_by.tier,
        canonical_model,
        at,
    ) {
        "row"
    } else {
        "tier"
    }
}

/// The unit a probe settles in: `default_unit`, unless that is the window share and the cost model in force at `at`
/// cannot price the account's tier and model on the 5-hour window (no weight of the model's own or of its model
/// tier, or no capacity), in which case dollars.
pub(crate) fn settlement_unit(
    default_unit: SavingsUnit,
    tables: &PlanTables,
    tier: Option<&str>,
    canonical_model: &str,
    at: i64,
) -> SavingsUnit {
    let priced = tier.is_some_and(|tier| tables.priced(FIVE_HOUR, tier, canonical_model, at));
    if default_unit == SavingsUnit::WindowShare && !priced {
        SavingsUnit::Usd
    } else {
        default_unit
    }
}

/// One delta of a recipe's key. An account the recipe never reached takes the ledger's reserved id, which is
/// what every reader groups an unattributed row under; an empty string would read as an account of its own. A
/// fast recipe's delta files under speed `fast`, beside the fast requests it keeps warm.
#[allow(clippy::too_many_arguments)]
fn delta(
    at: i64,
    key: &SessionKey,
    key_generation: i64,
    pool_id: &str,
    account: Option<&str>,
    canonical_model: &str,
    kind: &str,
    fast: bool,
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
        speed: fast.then_some(Speed::Fast),
        ..Default::default()
    }
}

impl Engine {
    /// Every probe the upstream served lands here, landed on its recipe or not: the probe is the pool's whatever
    /// happened to the recipe while it was in flight. A probe that migrated its session counts on its key. The delta
    /// carries no dollar and no token, since the probe's own request row carries its counts, which a reader prices.
    pub(crate) fn record_probe(&self, record: &ProbeRecord, _applied: Applied, now: i64) {
        // The migration is all the delta carries, so any other probe's would fold a row of zeros under the credit key,
        // which reads as an empty group of its own.
        if record.outcome != ProbeOutcome::Migrated {
            return;
        }
        let mut d = delta(
            now,
            &record.key,
            record.key_generation,
            &record.pool_id,
            Some(&record.account),
            &record.canonical_model,
            "probe",
            record.fast,
        );
        d.account_kind = record.kind;
        d.overage_in_use = Some(record.in_overage);
        d.probe_migrated = 1;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// One request the previous recipe kept warm. The caller reads every part of the key off the agent under
    /// the lock and hands them over, so nothing here needs the lock back. `kind` and `in_overage` are the credited
    /// request's serving attempt's, so the credit files under the funding word its request row took.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_kept_warm(
        &self,
        at: i64,
        key: &SessionKey,
        key_generation: i64,
        pool_id: &str,
        account: Option<&str>,
        canonical_model: &str,
        request_id: &str,
        tokens: u64,
        fast: bool,
        kind: Option<AccountKind>,
        in_overage: bool,
    ) {
        let mut d = delta(
            at,
            key,
            key_generation,
            pool_id,
            account,
            canonical_model,
            "inference",
            fast,
        );
        d.kept_warm_request_id = Some(request_id.to_string());
        d.kept_warm_requests = 1;
        d.kept_warm_tokens = tokens;
        d.account_kind = kind;
        d.overage_in_use = Some(in_overage);
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// One wasted probe, filed under the funding its own cost row took: the kind it was served under, its answer's
    /// overage and its speed. The row keys the probe's own exchange (its instant, its band and the account tier it ran
    /// under) rather than the settlement's clock, so a settlement filed hours or a day later prices its counts at the
    /// cells and the revision the probe ran at.
    pub(crate) fn record_settlement(&self, settlement: Settlement) {
        let mut d = delta(
            settlement.at,
            &settlement.key,
            settlement.key_generation,
            &settlement.pool_id,
            settlement.account.as_deref(),
            &settlement.canonical_model,
            "probe",
            settlement.fast,
        );
        let counts = typed(settlement.usage);
        d.wasted_probes = 1;
        d.wasted_input_tokens = counts.input;
        d.wasted_cache_read_tokens = counts.cache_read;
        d.wasted_cache_write_5m_tokens = counts.write_5m;
        d.wasted_cache_write_1h_tokens = counts.write_1h;
        d.wasted_output_tokens = counts.output;
        d.wasted_probe_reasons = BTreeMap::from([(settlement.label.as_str().to_string(), 1)]);
        d.tier = settlement.tier;
        d.band = Some(settlement.band);
        d.account_kind = settlement.kind;
        d.overage_in_use = Some(settlement.in_overage);
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
            skip.fast,
        );
        d.probes_skipped = 1;
        d.account_kind = skip.kind;
        d.overage_in_use = Some(skip.in_overage);
        self.parts.ledger.record(Record::Keepalive(d));
        self.parts
            .metrics
            .observe_skipped(&skip.pool_id, skip.state.as_str());
    }

    /// Drains what the last locked section queued: the settlements of stopped recipes and the probes a gate
    /// withheld. Called after every lock release by the entry points that can queue either.
    pub(crate) fn flush(&self) {
        let now = self.now();
        let (settlements, skipped) = {
            let mut inner = self.lock();
            (
                std::mem::take(&mut inner.settlements),
                std::mem::take(&mut inner.skipped),
            )
        };
        for settlement in settlements {
            self.record_settlement(settlement);
        }
        for skip in &skipped {
            self.record_skipped(now, skip);
        }
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
    fn an_unpriced_probe_reads_row_for_a_model_of_no_weighted_tier_and_tier_otherwise() {
        // The test table weighs Opus 5.5, leaves Opus 5 null and has no row for Opus 9, so both price at Opus 5.5's
        // weight, and places Mystery 2 in a tier with no weighted model.
        let tables = phoebis_ledger::test_tables::subscription_costs();
        let at = phoebis_ledger::test_tables::JANUARY_2026;
        assert_eq!(unpriced_reason(&tables, "claude-mystery-2", at), "row");
        for model in ["claude-opus-5", "claude-opus-9", "claude-opus-5-5"] {
            assert_eq!(unpriced_reason(&tables, model, at), "tier", "{model}");
        }
        assert_eq!(
            unpriced_reason(&tables, "claude-opus-5-5-20260401", at),
            "tier",
            "a dated id reads its undated weight"
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
                false,
            )
            .account_id
        };
        assert_eq!(of(None), UNATTRIBUTED, "a recipe with no account");
        assert_eq!(of(Some("")), UNATTRIBUTED, "a probe record that named none");
        assert_eq!(of(Some("a")), "a");
    }
}
