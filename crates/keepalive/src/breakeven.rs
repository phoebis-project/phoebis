//! The break-even: the pause length at which a recipe's chain of probes costs one re-write of its prefix, the
//! default every ceiling of the policy resolves to and the cap each of its numbers stays under.

use std::collections::HashMap;
use std::sync::LazyLock;

use phoebis_ledger::{PlanTables, PriceRevisions, PriceTable};
use phoebis_relay::{CacheTtl, CacheTtlPolicy, TtlBranch};
use phoebis_routing::rules::{allows, process, RulesMode};
use phoebis_routing::{listing::row_for, LongContextRates, Rates, FIVE_HOUR};
use phoebis_store::{canonical_id, ModelRule};

use crate::config::{KeepalivePolicy, KeepaliveTiming, FIVE_MINUTES_MS, ONE_HOUR_MS};
use crate::engine::Funding;

/// The read and write factors of one model under one funding, any unit: only their ratio enters the break-even.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BreakEvenFactors {
    pub read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
}

impl BreakEvenFactors {
    fn write(&self, ttl_ms: i64) -> f64 {
        if ttl_ms > FIVE_MINUTES_MS {
            self.write_1h
        } else {
            self.write_5m
        }
    }

    /// A row's three cells, when the row prices all three.
    fn of(rates: &Rates) -> Option<BreakEvenFactors> {
        Some(BreakEvenFactors {
            read: rates.cache_read?,
            write_5m: rates.write_5m?,
            write_1h: rates.write_1h?,
        })
    }
}

/// `(write - read) / (cadence * read)` in milliseconds for the entry's TTL, `None` when read is 0. Computed as
/// `floor((write - read) * (ttl_ms - lead_ms) / read)`: one probe interval is `ttl_ms - lead_ms`, so the cadence never
/// enters as a rounded figure, and the floor keeps the ceiling at or below the exact value within the nanosecond of
/// float slack below.
pub fn break_even_ms(
    factors: BreakEvenFactors,
    ttl_ms: i64,
    timing: &KeepaliveTiming,
) -> Option<i64> {
    let interval_ms = ttl_ms - timing.lead_ms(ttl_ms);
    if factors.read <= 0.0 || interval_ms <= 0 {
        return None;
    }
    let exact = (factors.write(ttl_ms) - factors.read) * interval_ms as f64 / factors.read;
    // The factors come from decimal tables a binary float holds only approximately, so a whole figure can land a
    // few ulps under its integer. A nanosecond of slack keeps the floor from dropping a millisecond to that noise.
    Some((exact + 1e-6).floor().max(0.0) as i64)
}

/// The factors for `canonical_model` under `funding`, at the fast cells when `fast`, on the cost model in force at
/// `at`. Plan funding reads `tables.rate_table(FIVE_HOUR, at).rows`, a listed model without a weight of its own at
/// its model tier's estimate there. Any other model reads its type multipliers, the model's own override where it has
/// one: an id no file lists, which the share prices at its named model tier's estimate and the shared multipliers,
/// breaks even at that same ratio. A token costs the model's weight times the type's multiplier, so the weight cancels
/// from read over write and every plan recipe breaks even alike. Dollars read `pricing.rate_table().rows`, or
/// `pricing.fast_rate_table().rows` for a fast recipe, whose probes draw usage credits at the fast cells, at a row's
/// long-context rates when `prompt_tokens` is above their threshold, the set a probe's cost reads on the same prompt,
/// and a model the price table prices nowhere takes the tightest row of that table (the smallest break-even among the
/// priced rows). Probe costs do not read these factors.
pub fn factors_for(
    pricing: &PriceTable,
    tables: &PlanTables,
    funding: Funding,
    canonical_model: &str,
    fast: bool,
    prompt_tokens: u64,
    at: i64,
) -> BreakEvenFactors {
    FactorRows::of(pricing, tables, at).factors(funding, canonical_model, fast, prompt_tokens)
}

/// The rows `factors_for` reads, built once so a tick that resolves every retained recipe builds each table's rows
/// and the tightest rows once rather than per recipe.
pub(crate) struct FactorRows {
    dollars: HashMap<String, Rates>,
    /// The dollar rows at the fast cells, a model without fast cells at its standard ones, as the price table lists
    /// them.
    fast: HashMap<String, Rates>,
    /// Each set's long-context rates by the key of the row they belong to, which a prompt above their threshold reads
    /// in place of the row's own. A row with fast cells has none in the fast set.
    dollars_long: HashMap<String, LongContextRates>,
    fast_long: HashMap<String, LongContextRates>,
    plan: HashMap<String, Rates>,
    /// The cost model's shared type multipliers, and the models that set their own.
    plan_multipliers: BreakEvenFactors,
    plan_overrides: HashMap<String, BreakEvenFactors>,
    tightest: BreakEvenFactors,
    tightest_fast: BreakEvenFactors,
}

impl FactorRows {
    pub(crate) fn of(pricing: &PriceTable, tables: &PlanTables, at: i64) -> FactorRows {
        let dollar_table = pricing.rate_table();
        let fast_table = pricing.fast_rate_table();
        let (dollars, dollars_long) = (dollar_table.rows, dollar_table.long_context);
        let (fast, fast_long) = (fast_table.rows, fast_table.long_context);
        let tightest_fast = tightest(fast.values().filter_map(BreakEvenFactors::of));
        let tightest = tightest(dollars.values().filter_map(BreakEvenFactors::of));
        let costs = tables.revisions.in_force(at);
        let shared = &costs.type_multiplier;
        let plan_multipliers = BreakEvenFactors {
            read: shared.cache_read,
            write_5m: shared.cache_write_5m,
            write_1h: shared.cache_write_1h,
        };
        let plan_overrides = costs
            .overrides
            .iter()
            .filter_map(|(model, row)| {
                let own = row.type_multiplier.as_ref()?;
                Some((
                    model.clone(),
                    BreakEvenFactors {
                        read: own.cache_read.unwrap_or(plan_multipliers.read),
                        write_5m: own.cache_write_5m.unwrap_or(plan_multipliers.write_5m),
                        write_1h: own.cache_write_1h.unwrap_or(plan_multipliers.write_1h),
                    },
                ))
            })
            .collect();
        FactorRows {
            dollars,
            fast,
            dollars_long,
            fast_long,
            plan: tables.rate_table(FIVE_HOUR, None, at).rows,
            plan_multipliers,
            plan_overrides,
            tightest,
            tightest_fast,
        }
    }

    /// The factors of `canonical_model` under `funding`, from the fast rows when `fast`: a fast recipe's probes draw
    /// usage credits at the fast cells, whatever funds the account. A dollar row's long-context rates stand in for its
    /// own when `prompt_tokens` is above their threshold, as `RateTable::rates_for` selects them. The plan's rows carry
    /// no such set.
    pub(crate) fn factors(
        &self,
        funding: Funding,
        canonical_model: &str,
        fast: bool,
        prompt_tokens: u64,
    ) -> BreakEvenFactors {
        let (dollars, long) = if fast {
            (&self.fast, &self.fast_long)
        } else {
            (&self.dollars, &self.dollars_long)
        };
        match funding {
            Funding::Plan => row_for(canonical_model, |k| self.plan.get(k))
                .and_then(BreakEvenFactors::of)
                .or_else(|| row_for(canonical_model, |k| self.plan_overrides.get(k)).copied()),
            Funding::ApiKey | Funding::Overage => {
                // One key for both maps, so a dated row without the set never reads its undated row's.
                row_for(canonical_model, |k| {
                    dollars.get_key_value(k).map(|(key, _)| key)
                })
                .and_then(|key| match long.get(key) {
                    Some(long) if prompt_tokens > long.over => Some(&long.rates),
                    _ => dollars.get(key),
                })
                .and_then(BreakEvenFactors::of)
            }
        }
        .unwrap_or_else(|| self.priced_nowhere(funding, fast))
    }

    /// The models the dollar table prices, by their row keys.
    pub(crate) fn dollar_models(&self) -> impl Iterator<Item = &str> {
        self.dollars.keys().map(String::as_str)
    }

    /// A model no row and no override names: the plan's shared multipliers, or the tightest dollar row at the recipe's
    /// speed.
    pub(crate) fn priced_nowhere(&self, funding: Funding, fast: bool) -> BreakEvenFactors {
        match funding {
            Funding::Plan => self.plan_multipliers,
            Funding::ApiKey | Funding::Overage if fast => self.tightest_fast,
            Funding::ApiKey | Funding::Overage => self.tightest,
        }
    }
}

/// The shipped price list, parsed once: the fallback below runs per retained recipe on every tick whose tables give no
/// break-even. The list is held rather than the factors, since it never changes and each call still reads the revision
/// in force at its own instant, where factors held from the first call would keep that call's revision past the date
/// the next one takes over.
static SHIPPED_PRICES: LazyLock<PriceRevisions> = LazyLock::new(PriceRevisions::shipped);

fn shipped_in_force(now: i64) -> &'static PriceTable {
    SHIPPED_PRICES.in_force(now)
}

/// The tightest row at the recipe's speed of the shipped price revision in force at `now`: what a recipe's ceilings
/// fall back to when the tables in force give no break-even, since an operator's price file with no usable row must
/// not leave a parked session probed until the session expires.
pub(crate) fn shipped_tightest(fast: bool, now: i64) -> BreakEvenFactors {
    let table = shipped_in_force(now);
    let rows = if fast {
        table.fast_rate_table()
    } else {
        table.rate_table()
    }
    .rows;
    tightest(rows.values().filter_map(BreakEvenFactors::of))
}

/// The smallest break-even any row gives at each TTL, as one set of factors in the ratio's own unit: a row
/// tightest on five minutes need not be tightest on an hour, and an unpriced model's ceiling stays under both.
/// No priced row at all gives a zero read, whose break-even is `None`.
fn tightest(rows: impl Iterator<Item = BreakEvenFactors>) -> BreakEvenFactors {
    let excess = |write: f64, read: f64| (write - read) / read;
    rows.filter(|row| row.read > 0.0)
        .map(|row| {
            (
                excess(row.write_5m, row.read),
                excess(row.write_1h, row.read),
            )
        })
        .reduce(|(a5, a1), (b5, b1)| (a5.min(b5), a1.min(b1)))
        .map_or(
            BreakEvenFactors {
                read: 0.0,
                write_5m: 0.0,
                write_1h: 0.0,
            },
            |(over_5m, over_1h)| BreakEvenFactors {
                read: 1.0,
                write_5m: 1.0 + over_5m,
                write_1h: 1.0 + over_1h,
            },
        )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ceilings {
    pub idle_ms: i64,
    pub parked_ms: i64,
    pub horizon_ms: i64,
    pub break_even_ms: i64,
    pub idle_share: f64,
}

/// Each field capped at the break-even, a null field and `max_idle_seconds` 0 resolving to it. `idle_share` is the
/// idle window's cost as a share of one re-write: `cadence * idle_hours * read / write`.
pub fn resolve(
    policy: &KeepalivePolicy,
    factors: BreakEvenFactors,
    ttl_ms: i64,
    timing: &KeepaliveTiming,
) -> Option<Ceilings> {
    let break_even = break_even_ms(factors, ttl_ms, timing)?;
    let capped = |seconds: Option<u64>| {
        seconds.map_or(break_even, |s| {
            i64::try_from(s)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000)
                .min(break_even)
        })
    };
    let idle_ms = capped((policy.max_idle_seconds > 0).then_some(policy.max_idle_seconds));
    let idle_share =
        timing.cadence_per_hour(ttl_ms) * (idle_ms as f64 / ONE_HOUR_MS as f64) * factors.read
            / factors.write(ttl_ms);
    Some(Ceilings {
        idle_ms,
        parked_ms: capped(policy.max_parked_seconds),
        horizon_ms: capped(policy.scheduled_horizon_seconds),
        break_even_ms: break_even,
        idle_share,
    })
}

/// The lowest and the highest figure a pool's combinations resolve to, equal when every combination agrees.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Range {
    pub min: f64,
    pub max: f64,
}

impl Range {
    fn widen(range: Option<Range>, value: f64) -> Range {
        range.map_or(
            Range {
                min: value,
                max: value,
            },
            |r| Range {
                min: r.min.min(value),
                max: r.max.max(value),
            },
        )
    }
}

/// A pool's ceilings in seconds and its idle window's share of one re-write, each as the range its members'
/// fundings, the main agent's TTLs and the pool's models span. The console shows them beside the policy's fields,
/// since it holds no price of its own.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ResolvedCeilings {
    pub break_even_seconds: Range,
    pub horizon_seconds: Range,
    pub idle_seconds: Range,
    pub idle_share: Range,
    pub parked_seconds: Range,
}

/// `resolve` over every combination of `fundings`, the main agent's TTL for each under `cache_ttl` (both TTLs when
/// the policy leaves the markers as sent) and the models `rules` allow, each at standard speed and on a prompt under
/// any long-context threshold, the one the dialog's figures describe. The models are the dollar table's rows and the
/// ids the TTL sets and the rules' wildcard-free entries name, and each is folded, judged, priced and given its TTL as
/// a request on it would be. A row keyed undated that `dated` maps to the catalog's dated id is judged under that id,
/// which is what a request on it runs under, so a deny entry naming the dated id drops the row. Rules that allow none
/// of them leave a model priced nowhere. Tables that give no break-even fall back to the shipped tightest row, as
/// `ceilings_for` does. `None` with no funding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_pool(
    policy: &KeepalivePolicy,
    cache_ttl: &CacheTtlPolicy,
    rules: &[ModelRule],
    fundings: &[Funding],
    rows: &FactorRows,
    dated: &HashMap<String, String>,
    timing: &KeepaliveTiming,
    now: i64,
) -> Option<ResolvedCeilings> {
    let processed = process(&[], RulesMode::Extend, rules);
    let named = cache_ttl
        .plan
        .models
        .iter()
        .chain(&cache_ttl.metered.models)
        .map(|m| m.model.as_str())
        .chain(
            rules
                .iter()
                .map(|r| r.pattern.as_str())
                .filter(|p| !p.contains(['*', '?', '\\'])),
        )
        // A request's model reaches the rows lowercased and without its `[1m]` suffix, so a name spelled otherwise
        // would miss every row and add a combination no request runs.
        .map(canonical_id);
    let mut models: Vec<String> = rows
        .dollars
        .keys()
        .cloned()
        .chain(named)
        .filter(|model| allows(&processed, dated.get(model).unwrap_or(model)))
        .collect();
    models.sort_unstable();
    models.dedup();
    let ttl_ms = |ttl: CacheTtl| match ttl {
        CacheTtl::FiveMinutes => FIVE_MINUTES_MS,
        CacheTtl::OneHour => ONE_HOUR_MS,
    };
    let mut resolved: Option<ResolvedCeilings> = None;
    for &funding in fundings {
        let metered = funding != Funding::Plan;
        let picks: Vec<(BreakEvenFactors, &str)> = if models.is_empty() {
            vec![(rows.priced_nowhere(funding, false), "")]
        } else {
            models
                .iter()
                .map(|model| (rows.factors(funding, model, false, 0), model.as_str()))
                .collect()
        };
        for (factors, model) in picks {
            let ttls = match cache_ttl.ttl_for(model, TtlBranch::Main, metered) {
                Some(ttl) => vec![ttl_ms(ttl)],
                None => vec![FIVE_MINUTES_MS, ONE_HOUR_MS],
            };
            for ttl in ttls {
                let Some(ceilings) = resolve(policy, factors, ttl, timing)
                    .or_else(|| resolve(policy, shipped_tightest(false, now), ttl, timing))
                else {
                    continue;
                };
                let seconds = |ms: i64| ms as f64 / 1_000.0;
                let previous = resolved.as_ref();
                resolved = Some(ResolvedCeilings {
                    break_even_seconds: Range::widen(
                        previous.map(|r| r.break_even_seconds),
                        seconds(ceilings.break_even_ms),
                    ),
                    horizon_seconds: Range::widen(
                        previous.map(|r| r.horizon_seconds),
                        seconds(ceilings.horizon_ms),
                    ),
                    idle_seconds: Range::widen(
                        previous.map(|r| r.idle_seconds),
                        seconds(ceilings.idle_ms),
                    ),
                    idle_share: Range::widen(previous.map(|r| r.idle_share), ceilings.idle_share),
                    parked_seconds: Range::widen(
                        previous.map(|r| r.parked_seconds),
                        seconds(ceilings.parked_ms),
                    ),
                });
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use phoebis_ledger::effective_instant;

    use super::*;

    /// The boundary is the shipped file's second date, read through the loader so the test pins no shipped date. The
    /// earlier instant is read first, as a tick before the date would, so a figure held from the first call reads one
    /// revision at both.
    #[test]
    fn two_calls_on_either_side_of_a_shipped_revisions_date_read_the_two_revisions() {
        let shipped = PriceRevisions::shipped();
        let boundary = shipped
            .iter()
            .nth(1)
            .and_then(|revision| effective_instant(&revision.effective))
            .expect("the shipped price file carries a second revision");
        let before = shipped_in_force(boundary - 1);
        let after = shipped_in_force(boundary);
        assert_eq!(
            before,
            shipped.in_force(boundary - 1),
            "the revision before the date"
        );
        assert_eq!(
            after,
            shipped.in_force(boundary),
            "the revision from the date on"
        );
        assert_ne!(
            before.effective, after.effective,
            "the two calls read two revisions"
        );
    }
}
