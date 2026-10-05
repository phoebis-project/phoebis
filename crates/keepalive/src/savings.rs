//! The savings accounting: what a kept-warm request saved, what a probe cost, and what a stopped recipe wasted,
//! as the ledger's rollup rows carry them. The accounting this feeds is an estimate in money for an API key and
//! a proxy for quota on a subscription account, which is why it is kept apart from the ledger's own measured
//! spend.

use std::collections::BTreeMap;

use phoebis_ledger::{KeepaliveDelta, PlanTables, PriceTable, Record, Speed, UNATTRIBUTED};
use phoebis_relay::{CacheTtl, CacheTtlPolicy, TtlBranch};
use phoebis_routing::{PinUsage, FIVE_HOUR};
use phoebis_store::{BudgetRow, BudgetScope};

use crate::config::{FIVE_MINUTES_MS, ONE_HOUR_MS};
use crate::engine::{Engine, SavingsUnit, SessionKey, Settlement, MAIN_AGENT};
use crate::probe::{Applied, ProbeOutcome, ProbeRecord};
use crate::usage::ProbeUsage;

/// The gross saving of a request that read `cache_read_tokens` it would otherwise have paid as input, in the
/// unit the covering probe settled in: dollars at the list prices of the cells `speed` selects, or the share of
/// the `tier`'s 5-hour window at `at` (the band inside the price). `None` on the share side alone, for a model the
/// cost table lacks, a cell it leaves `null`, or a tier it does not size.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gross_saving(
    unit: SavingsUnit,
    table: &PriceTable,
    tables: &PlanTables,
    tier: Option<&str>,
    canonical_model: &str,
    cache_read_tokens: u64,
    at: i64,
    speed: Speed,
) -> Option<f64> {
    match unit {
        SavingsUnit::Usd => {
            let per_token = match table.price_at(canonical_model, speed) {
                Some(price) => (price.input - price.cache_read) / 1_000_000.0,
                // The dearest cells `probe_dollars` prices this model's probes at: both sides of the net read one
                // rate table, so a cache that saves more than its probes cost keeps the recipe net positive.
                None => {
                    let dearest = table.rate_table().fallback;
                    dearest.input.unwrap_or(0.0) - dearest.cache_read.unwrap_or(0.0)
                }
            };
            Some(cache_read_tokens as f64 * per_token)
        }
        SavingsUnit::WindowShare => {
            let tier = tier?;
            let priced = |usage| tables.five_hour_share_at(tier, canonical_model, usage, at);
            let as_input = priced(PinUsage {
                input: cache_read_tokens,
                ..PinUsage::default()
            })?;
            let as_read = priced(PinUsage {
                cache_read: cache_read_tokens,
                ..PinUsage::default()
            })?;
            Some(as_input - as_read)
        }
    }
}

/// A probe's estimated cost in `unit`: `probe_dollars`, or the share of the `tier`'s 5-hour window at `at` (the band
/// inside the price). `None` on the share side alone, for a model the cost table lacks, a cell it leaves `null`, or
/// a tier it does not size.
pub(crate) fn probe_cost(
    unit: SavingsUnit,
    table: &PriceTable,
    tables: &PlanTables,
    tier: Option<&str>,
    canonical_model: &str,
    usage: ProbeUsage,
    at: i64,
) -> Option<f64> {
    match unit {
        SavingsUnit::Usd => Some(probe_dollars(table, canonical_model, usage)),
        SavingsUnit::WindowShare => {
            tables.five_hour_share_at(tier?, canonical_model, typed(usage), at)
        }
    }
}

/// A probe's dollars at the list prices of the cells the answer's `usage.speed` selects. A model the price table
/// does not list prices at the dearest listed cell of each type, the fallback `PriceTable::rate_table` gives the
/// dollar burn: the daily cap and the net read this figure, and a zero would let a probe that consumed quota pass
/// both.
pub(crate) fn probe_dollars(table: &PriceTable, canonical_model: &str, usage: ProbeUsage) -> f64 {
    let typed = typed(usage);
    if let Some((total, _)) = table.estimate(
        canonical_model,
        usage.speed.unwrap_or(Speed::Standard),
        typed.input,
        typed.cache_read,
        typed.write_5m,
        typed.write_1h,
        typed.output,
    ) {
        return total;
    }
    let dearest = table.rate_table().fallback;
    let at = |tokens: u64, rate: Option<f64>| tokens as f64 * rate.unwrap_or(0.0);
    at(typed.input, dearest.input)
        + at(typed.cache_read, dearest.cache_read)
        + at(typed.write_5m, dearest.write_5m)
        + at(typed.write_1h, dearest.write_1h)
        + at(typed.output, dearest.output)
}

/// A probe answer's tokens by type. An answer that reports the aggregate `cache_creation_input_tokens` and no TTL
/// breakdown is priced as five-minute writes, which is how the relay's own usage parser reads the same answer; the
/// estimate would otherwise miss a charge the request ledger recognises.
fn typed(usage: ProbeUsage) -> PinUsage {
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

/// One token of each type a probe of a recipe can carry: the uncached tail, the kept-warm prefix, output (a probe
/// asks for none, yet an answer can carry some), the five-minute write (an answer reporting its write in aggregate
/// alone is priced as one) and the one-hour write when the TTL in force (`ttl_ms`) or any marker of the recipe
/// (`longest_ttl_ms`) is above five minutes, since a probe that finds the prefix cold writes each marked part at its
/// own TTL. Known before the probe runs, so the gate and the settlement decide on the same usage.
pub(crate) fn probe_types(ttl_ms: i64, longest_ttl_ms: i64) -> PinUsage {
    PinUsage {
        input: 1,
        cache_read: 1,
        write_5m: 1,
        write_1h: u64::from(ttl_ms.max(longest_ttl_ms) > FIVE_MINUTES_MS),
        output: 1,
    }
}

/// `probe_types` for the body the relay dispatches. While the pool's cache TTL `policy` is on, `Relay::dispatch_to`
/// rewrites every marker of a probe to the TTL its agent's branch takes in the set the account's funding selects
/// (`metered` for an api_key account or one in overage), and adds one at that TTL to a body without any where the
/// policy says so; that TTL then replaces both the one in force and the recipe's own. A body left without a marker
/// writes nothing.
pub(crate) fn dispatched_types(
    policy: &CacheTtlPolicy,
    canonical_model: &str,
    metered: bool,
    agent_id: &str,
    ttl_ms: i64,
    longest_ttl_ms: i64,
) -> PinUsage {
    let branch = if agent_id == MAIN_AGENT {
        TtlBranch::Main
    } else {
        TtlBranch::Subagent
    };
    let Some(ttl) = policy.ttl_for(canonical_model, branch, metered) else {
        return probe_types(ttl_ms, longest_ttl_ms);
    };
    let marked = longest_ttl_ms > 0 || policy.inject_when_absent;
    let dispatched_ms = if marked && ttl == CacheTtl::OneHour {
        ONE_HOUR_MS
    } else {
        FIVE_MINUTES_MS
    };
    probe_types(dispatched_ms, dispatched_ms)
}

/// `row` when the model has no weight on the 5-hour window, else `tier`: the only two reasons left, since the loader
/// refuses an anchor tier without a capacity on either window and a weighted model prices every type.
pub(crate) fn unpriced_reason(tables: &PlanTables, canonical_model: &str) -> &'static str {
    if !tables.priced(
        FIVE_HOUR,
        &tables.costs.unit.defined_by.tier,
        canonical_model,
    ) {
        "row"
    } else {
        "tier"
    }
}

/// The unit a probe is gated in, settles in and earns its savings in: `default_unit`, unless that is the window share
/// and the table cannot price `types` on the account's tier (no row, a `null` cell under a carried type, no tier or a
/// `null` factor), in which case dollars. One decision for the three, so the stop and the caps read the balance the
/// probes move.
pub(crate) fn settlement_unit(
    default_unit: SavingsUnit,
    tables: &PlanTables,
    tier: Option<&str>,
    canonical_model: &str,
    types: PinUsage,
) -> SavingsUnit {
    let priced = tier.is_some_and(|tier| {
        tables
            .share(FIVE_HOUR, tier, canonical_model, types)
            .is_some()
    });
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
    /// Every probe the upstream served lands here, landed on its recipe or not: the cost is the pool's whatever
    /// happened to the recipe while the probe was in flight.
    pub(crate) fn record_probe(&self, record: &ProbeRecord, _applied: Applied, now: i64) {
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
        d.probe_cost_usd = record.cost_usd;
        if record.outcome == ProbeOutcome::Migrated {
            d.probe_migrated = 1;
        }
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// One request the previous recipe kept warm. The caller reads every part of the key off the agent under
    /// the lock and hands them over, so nothing here needs the lock back.
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
        saving: f64,
        fast: bool,
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
        d.saved_usd_gross = saving;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    pub(crate) fn record_settlement(&self, at: i64, settlement: Settlement) {
        let mut d = delta(
            at,
            &settlement.key,
            settlement.key_generation,
            &settlement.pool_id,
            settlement.account.as_deref(),
            &settlement.canonical_model,
            "probe",
            settlement.fast,
        );
        d.wasted_probes = settlement.costs.len() as u64;
        d.wasted_probe_usd = settlement.costs.iter().sum();
        d.wasted_probe_reasons =
            BTreeMap::from([(settlement.label.as_str().to_string(), d.wasted_probes)]);
        self.parts.ledger.record(Record::Keepalive(d));
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_skipped(
        &self,
        at: i64,
        key: &SessionKey,
        key_generation: i64,
        pool_id: &str,
        account: Option<&str>,
        canonical_model: &str,
        fast: bool,
    ) {
        let mut d = delta(
            at,
            key,
            key_generation,
            pool_id,
            account,
            canonical_model,
            "probe",
            fast,
        );
        d.probes_skipped = 1;
        self.parts.ledger.record(Record::Keepalive(d));
    }

    /// Drains what the last locked section queued: the settlements of stopped recipes and the probes a gate
    /// withheld. Called after every lock release by the entry points that can queue either.
    pub(crate) fn flush(&self) {
        let now = self.now();
        let (settlements, skipped) = {
            let mut inner = self.lock();
            let queued = std::mem::take(&mut inner.skipped);
            let mut skipped: Vec<(SessionKey, i64, String, Option<String>, String, bool)> =
                Vec::new();
            for (key, agent_id) in queued {
                let Some(agent) = inner
                    .sessions
                    .get(&key)
                    .and_then(|s| s.agents.get(&agent_id))
                else {
                    continue;
                };
                skipped.push((
                    key,
                    agent.key_generation,
                    agent.pool_id.clone(),
                    agent.account.clone(),
                    agent.canonical_model.clone(),
                    agent.current.as_ref().is_some_and(|o| o.recipe.fast),
                ));
            }
            (std::mem::take(&mut inner.settlements), skipped)
        };
        for settlement in settlements {
            self.record_settlement(now, settlement);
        }
        for (key, key_generation, pool_id, account, model, fast) in skipped {
            self.record_skipped(
                now,
                &key,
                key_generation,
                &pool_id,
                account.as_deref(),
                &model,
                fast,
            );
        }
    }

    pub fn spent_today(&self) -> BTreeMap<String, f64> {
        self.lock().spent_today.clone()
    }

    /// The share of each subscription account's 5-hour window its probes consumed on the current UTC day, the
    /// band inside each probe's price.
    pub fn share_spent_today(&self) -> BTreeMap<String, f64> {
        self.lock().share_spent_today.clone()
    }

    /// The pool's probe spend for the current UTC day, and the accounts' plan-side figure beside it, as the
    /// probes of the day settled them into the store: what a restart starts from. Days before today are deleted
    /// here too, since a restart across midnight is a roll no running engine saw.
    pub async fn load_spent_today(&self) -> BTreeMap<String, f64> {
        let now = self.now();
        let day = now.div_euclid(86_400_000);
        self.purge_budgets_before(now);
        let rows = self
            .parts
            .store
            .keepalive_budgets(&budget_date(day))
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the keepalive's daily budgets could not be read; both start at zero");
                Vec::new()
            });
        let mut spent = BTreeMap::new();
        let mut share_spent = BTreeMap::new();
        for row in rows {
            match row.scope {
                BudgetScope::Account => {
                    share_spent.insert(row.id, row.five_hour_share);
                }
                BudgetScope::Pool => {
                    spent.insert(row.id, row.usd);
                }
            }
        }
        let mut inner = self.lock();
        inner.spent_today = spent.clone();
        inner.share_spent_today = share_spent;
        inner.spent_day = day;
        spent
    }

    /// Adds a settled probe's charge to its row for `day`, a day index since the epoch. A failed write costs the
    /// restore that charge, never the running engine's own figure.
    pub(crate) fn persist_charge(&self, day: i64, charge: &BudgetRow) {
        if let Err(error) = self
            .parts
            .store
            .add_keepalive_budget(&budget_date(day), charge)
        {
            tracing::warn!(%error, id = %charge.id, "a probe's charge could not be kept for a restart");
        }
    }

    /// Deletes the stored budgets of every UTC day before `now`'s.
    pub(crate) fn purge_budgets_before(&self, now: i64) {
        let today = budget_date(now.div_euclid(86_400_000));
        if let Err(error) = self.parts.store.purge_keepalive_budgets_before(&today) {
            tracing::warn!(%error, "the keepalive's past daily budgets could not be deleted");
        }
    }
}

/// The UTC date, `YYYY-MM-DD`, of the day index `day` counts from the epoch.
fn budget_date(day: i64) -> String {
    jiff::Timestamp::from_millisecond(day * 86_400_000)
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relay's own usage parser reads an aggregate `cache_creation_input_tokens` as five-minute writes,
    /// so a probe answer that carries only the aggregate costs what the same answer with the split costs.
    #[test]
    fn an_aggregate_only_cache_write_costs_a_five_minute_write() {
        let table = PriceTable::shipped();
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
        let cost = |usage| {
            probe_cost(
                SavingsUnit::Usd,
                &table,
                &tables,
                None,
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
    fn a_window_share_saving_is_the_input_share_less_the_read_share() {
        // 2026-09-26, a Saturday: outside the peak band's weekdays, so the band weighs nothing.
        const SATURDAY_AFTERNOON: i64 = 1_790_431_200_000;
        let saved = gross_saving(
            SavingsUnit::WindowShare,
            &phoebis_ledger::test_tables::api_prices(),
            &phoebis_ledger::test_tables::subscription_costs(),
            Some("default_claude_max_20x"),
            "claude-opus-5-5",
            1,
            SATURDAY_AFTERNOON,
            Speed::Standard,
        )
        .expect("a priced model on a known tier");
        // The test table: Opus 5.5 weighs 2, an input token multiplies by 1 and a cache read by 0.1, and the Max
        // 20x 5-hour window holds 100 units.
        let want = 2.0 * 1.0 / 100.0 - 2.0 * 0.1 / 100.0;
        assert!((saved - want).abs() < 1e-12, "{saved} against {want}");
    }

    #[test]
    fn an_unpriced_probe_reads_row_for_a_model_without_a_weight_and_tier_otherwise() {
        // The test table weighs Opus 5.5, leaves Opus 5 null and has no row for Opus 9.
        let tables = phoebis_ledger::test_tables::subscription_costs();
        assert_eq!(unpriced_reason(&tables, "claude-opus-5"), "row");
        assert_eq!(unpriced_reason(&tables, "claude-opus-9"), "row");
        assert_eq!(unpriced_reason(&tables, "claude-opus-5-5"), "tier");
        assert_eq!(
            unpriced_reason(&tables, "claude-opus-5-5-20260401"),
            "tier",
            "a dated id reads its undated weight"
        );
    }

    #[test]
    fn a_fast_recipes_saving_reads_the_fast_input_less_the_fast_cache_read() {
        let saved = |speed| {
            gross_saving(
                SavingsUnit::Usd,
                &PriceTable::shipped(),
                &PlanTables::shipped(),
                None,
                "claude-opus-5",
                1_000_000,
                0,
                speed,
            )
            .expect("a priced model")
        };
        // The shipped Opus 5 row, per million tokens: input 10 and cache read 1 at the fast cells, 5 and 0.5
        // at the standard ones.
        assert!(
            (saved(Speed::Fast) - (10.0 - 1.0)).abs() < 1e-12,
            "{:?}",
            saved(Speed::Fast)
        );
        assert!(
            (saved(Speed::Standard) - (5.0 - 0.5)).abs() < 1e-12,
            "{:?}",
            saved(Speed::Standard)
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
