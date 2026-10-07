//! The break-even: the pause length at which a recipe's chain of probes costs one re-write of its prefix, the
//! default every ceiling of the policy resolves to and the cap each of its numbers stays under.

use phoebis_ledger::{PlanTables, PriceTable};
use phoebis_routing::{listing::row_for, Rates, FIVE_HOUR};

use crate::config::{KeepalivePolicy, KeepaliveTiming, FIVE_MINUTES_MS, ONE_HOUR_MS};
use crate::engine::Funding;

/// The read and write factors of one model under one funding, any unit: only their ratio enters the break-even.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Factors {
    pub read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
}

impl Factors {
    fn write(&self, ttl_ms: i64) -> f64 {
        if ttl_ms > FIVE_MINUTES_MS {
            self.write_1h
        } else {
            self.write_5m
        }
    }

    /// A row's three cells, when the row prices all three.
    fn of(rates: &Rates) -> Option<Factors> {
        Some(Factors {
            read: rates.cache_read?,
            write_5m: rates.write_5m?,
            write_1h: rates.write_1h?,
        })
    }
}

/// `(write - read) / (cadence * read)` in milliseconds for the entry's TTL, `None` when read is 0. Computed as
/// `floor((write - read) * (ttl_ms - lead_ms) / read)`: one probe interval is `ttl_ms - lead_ms`, so the cadence never
/// enters as a rounded figure, and the floor keeps the ceiling at or below the exact value, never above it.
pub fn break_even_ms(factors: Factors, ttl_ms: i64, timing: &KeepaliveTiming) -> Option<i64> {
    let interval_ms = ttl_ms - timing.lead_ms(ttl_ms);
    if factors.read <= 0.0 || interval_ms <= 0 {
        return None;
    }
    let exact = (factors.write(ttl_ms) - factors.read) * interval_ms as f64 / factors.read;
    // The factors come from decimal tables a binary float holds only approximately, so a whole figure can land a
    // few ulps under its integer. A nanosecond of slack keeps the floor from dropping a millisecond to that noise.
    Some((exact + 1e-6).floor().max(0.0) as i64)
}

/// The factors for `canonical_model` under `funding`. Plan funding reads `tables.rate_table(FIVE_HOUR).rows` and falls
/// back to the dollar row when the plan prices the model nowhere, dollars read `pricing.rate_table().rows`, and a model
/// neither prices takes the tightest dollar row (the smallest break-even among the priced rows).
pub fn factors_for(
    pricing: &PriceTable,
    tables: &PlanTables,
    funding: Funding,
    canonical_model: &str,
) -> Factors {
    let dollars = pricing.rate_table().rows;
    let plan = match funding {
        Funding::Plan => {
            let rows = tables.rate_table(FIVE_HOUR).rows;
            row_for(canonical_model, |k| rows.get(k)).and_then(Factors::of)
        }
        Funding::ApiKey | Funding::Overage => None,
    };
    plan.or_else(|| row_for(canonical_model, |k| dollars.get(k)).and_then(Factors::of))
        .unwrap_or_else(|| tightest(dollars.values().filter_map(Factors::of)))
}

/// The smallest break-even any row gives at each TTL, as one set of factors in the ratio's own unit: a row
/// tightest on five minutes need not be tightest on an hour, and an unpriced model's ceiling stays under both.
/// No priced row at all gives a zero read, whose break-even is `None`.
fn tightest(rows: impl Iterator<Item = Factors>) -> Factors {
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
            Factors {
                read: 0.0,
                write_5m: 0.0,
                write_1h: 0.0,
            },
            |(over_5m, over_1h)| Factors {
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
    factors: Factors,
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
