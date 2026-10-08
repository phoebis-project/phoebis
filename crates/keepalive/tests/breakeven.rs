//! The break-even: the pause length at which a recipe's chain of probes costs one re-write of its prefix, by the
//! model's factors under the account's funding and the entry's TTL, and the ceilings the policy resolves against it.
use phoebis_keepalive::{
    break_even_ms, factors_for, resolve, Factors, Funding, KeepalivePolicy, KeepaliveTiming,
    FIVE_MINUTES_MS, ONE_HOUR_MS,
};
use phoebis_ledger::{PlanTables, PriceTable, SubscriptionCosts};

fn factors(read: f64, write: f64) -> Factors {
    Factors {
        read,
        write_5m: write,
        write_1h: write,
    }
}

#[test]
fn the_break_even_is_write_less_read_over_cadence_times_read() {
    let timing = KeepaliveTiming::default();
    assert_eq!(
        (timing.lead_ms(FIVE_MINUTES_MS), timing.lead_ms(ONE_HOUR_MS)),
        (60_000, 360_000),
        "the production leads"
    );
    assert_eq!(timing.cadence_per_hour(FIVE_MINUTES_MS), 15.0);
    assert!((timing.cadence_per_hour(ONE_HOUR_MS) - 1.1111).abs() < 1e-4);
    let plan = factors(0.025, 1.25);
    assert_eq!(
        break_even_ms(plan, ONE_HOUR_MS, &timing),
        Some(158_760_000),
        "44.1 h on a one-hour entry"
    );
    assert_eq!(
        break_even_ms(plan, FIVE_MINUTES_MS, &timing),
        Some(11_760_000),
        "3 h 16 min on a five-minute entry"
    );
    assert_eq!(
        break_even_ms(factors(0.1, 1.25), FIVE_MINUTES_MS, &timing),
        Some(2_760_000),
        "46 min"
    );
    assert_eq!(
        break_even_ms(factors(0.05, 1.25), FIVE_MINUTES_MS, &timing),
        Some(5_760_000),
        "1.6 h"
    );
    let one_hour_write = Factors {
        read: 0.025,
        write_5m: 1.25,
        write_1h: 2.0,
    };
    assert_eq!(
        break_even_ms(one_hour_write, ONE_HOUR_MS, &timing),
        Some(255_960_000),
        "71.1 h: a one-hour entry reads the one-hour write"
    );
    let half_minute_lead = KeepaliveTiming {
        lead_5m_ms: 30_000,
        ..KeepaliveTiming::default()
    };
    assert_eq!(
        (
            break_even_ms(plan, FIVE_MINUTES_MS, &half_minute_lead),
            break_even_ms(factors(0.1, 1.25), FIVE_MINUTES_MS, &half_minute_lead)
        ),
        (Some(13_230_000), Some(3_105_000)),
        "the lead is in the figure: a later probe stretches every interval"
    );
    assert_eq!(
        break_even_ms(factors(0.0, 1.25), FIVE_MINUTES_MS, &timing),
        None,
        "a free read never reaches a re-write"
    );
    assert_eq!(
        break_even_ms(factors(0.07, 1.25), ONE_HOUR_MS, &timing),
        Some(54_617_142),
        "54,617,142.857 ms floors, so the ceiling never runs past the exact value"
    );
}

#[test]
fn a_null_field_and_a_zero_idle_window_resolve_to_the_break_even_and_a_number_caps_below_it() {
    let timing = KeepaliveTiming::default();
    let dollars = factors(0.1, 1.25);
    let open = KeepalivePolicy {
        enabled: true,
        max_idle_seconds: 0,
        max_parked_seconds: None,
        scheduled_horizon_seconds: None,
    };
    let resolved = resolve(&open, dollars, FIVE_MINUTES_MS, &timing).unwrap();
    assert_eq!(
        (
            resolved.idle_ms,
            resolved.parked_ms,
            resolved.horizon_ms,
            resolved.break_even_ms
        ),
        (2_760_000, 2_760_000, 2_760_000, 2_760_000)
    );
    let above = KeepalivePolicy {
        enabled: true,
        max_idle_seconds: 3_600,
        max_parked_seconds: Some(7_200),
        scheduled_horizon_seconds: Some(604_800),
    };
    let resolved = resolve(&above, dollars, FIVE_MINUTES_MS, &timing).unwrap();
    assert_eq!(
        (resolved.idle_ms, resolved.parked_ms, resolved.horizon_ms),
        (2_760_000, 2_760_000, 2_760_000),
        "a number above the break-even reads as the break-even"
    );
    let below = KeepalivePolicy {
        enabled: true,
        max_idle_seconds: 600,
        max_parked_seconds: Some(1_800),
        scheduled_horizon_seconds: Some(60),
    };
    let resolved = resolve(&below, dollars, FIVE_MINUTES_MS, &timing).unwrap();
    assert_eq!(
        (
            resolved.idle_ms,
            resolved.parked_ms,
            resolved.horizon_ms,
            resolved.break_even_ms
        ),
        (600_000, 1_800_000, 60_000, 2_760_000),
        "a number below the break-even is the cap"
    );
    assert_eq!(
        resolve(&below, factors(0.0, 1.25), FIVE_MINUTES_MS, &timing),
        None
    );
}

#[test]
fn the_idle_share_is_the_windows_probes_over_one_rewrite() {
    let timing = KeepaliveTiming::default();
    let hour = KeepalivePolicy {
        enabled: true,
        ..KeepalivePolicy::default()
    };
    let share = resolve(&hour, factors(0.025, 1.25), ONE_HOUR_MS, &timing)
        .unwrap()
        .idle_share;
    assert!((share - 0.0222).abs() < 5e-5, "{share}");
    let at_break_even = KeepalivePolicy {
        enabled: true,
        max_idle_seconds: 0,
        ..KeepalivePolicy::default()
    };
    let share = resolve(&at_break_even, factors(0.1, 1.25), FIVE_MINUTES_MS, &timing)
        .unwrap()
        .idle_share;
    assert!(
        (share - 0.92).abs() < 1e-9,
        "the probes of a window as long as the break-even cost a re-write less one read: {share}"
    );
}

/// One weighted model and one model the plan leaves unweighted, written here so the figures follow from the text.
const COSTS: &str = r#"
effective: 2026-01-01
unit:
  defined_by: {tier: default_claude_max_20x, window: five_hour, band: out_of_band}
  units_per_window: 100
capacity:
  default_claude_max_20x: {five_hour: 100, seven_day: 1000}
peak:
  zone: America/Los_Angeles
  weekdays: true
  from_hour: 5
  to_hour: 11
  capacity_factor: {five_hour: 0.5, seven_day: 1}
type_multiplier: {cache_read: 0.025, cache_write_1h: 1.25, cache_write_5m: 1.25, input: 1, output: 5}
model_weight:
  claude-opus-5-5: 2
  claude-sonnet-5-5: null
model_windows: {}
overrides: {}
"#;

/// The tightest row is Sonnet's at both TTLs: its read is the largest share of its writes.
const PRICES: &str = r#"
effective: 2026-01-01
models:
  claude-fable-5-1: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 0.25, output: 50}
  claude-opus-5-5: {input: 4, cache_write_5m: 5, cache_write_1h: 8, cache_read: 0.2, output: 20}
  claude-sonnet-5-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, output: 10}
"#;

#[test]
fn a_plan_priced_model_takes_the_plan_factors_an_unpriced_one_falls_back_to_dollars_and_a_model_priced_nowhere_takes_the_tightest_row(
) {
    let pricing = PriceTable::from_yaml(PRICES, "test").unwrap();
    let tables = PlanTables {
        costs: SubscriptionCosts::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    let timing = KeepaliveTiming::default();
    let both = |funding: Funding, model: &str| {
        let factors = factors_for(&pricing, &tables, funding, model);
        (
            break_even_ms(factors, FIVE_MINUTES_MS, &timing),
            break_even_ms(factors, ONE_HOUR_MS, &timing),
        )
    };
    assert_eq!(
        both(Funding::Plan, "claude-opus-5-5"),
        (Some(11_760_000), Some(158_760_000)),
        "the plan's read of 0.025 against both writes of 1.25"
    );
    assert_eq!(
        both(Funding::Plan, "claude-opus-5-5-20261001"),
        both(Funding::Plan, "claude-opus-5-5"),
        "a dated id reads its undated row"
    );
    assert_eq!(
        factors_for(&pricing, &tables, Funding::ApiKey, "claude-opus-5-5"),
        Factors {
            read: 0.2 / 1_000_000.0,
            write_5m: 5.0 / 1_000_000.0,
            write_1h: 8.0 / 1_000_000.0,
        },
        "an api key reads the dollar row"
    );
    for funding in [Funding::ApiKey, Funding::Overage] {
        assert_eq!(
            both(funding, "claude-opus-5-5"),
            (Some(5_760_000), Some(126_360_000)),
            "{funding:?} reads the dollar row"
        );
    }
    assert_eq!(
        both(Funding::Plan, "claude-sonnet-5-5"),
        (Some(2_760_000), Some(61_560_000)),
        "a model the plan leaves unweighted falls back to its dollar row"
    );
    for funding in [Funding::ApiKey, Funding::Plan] {
        assert_eq!(
            both(funding, "claude-unlisted-9"),
            both(Funding::ApiKey, "claude-sonnet-5-5"),
            "{funding:?}: a model neither table prices takes the smallest break-even of the priced rows"
        );
    }
}

/// The tightest row differs by TTL: `claude-early-1` gains the least over its read on a five-minute write and
/// `claude-late-1` on a one-hour write, so a model priced nowhere takes each TTL's smallest excess rather than one
/// whole row.
const SPLIT_PRICES: &str = r#"
effective: 2026-01-01
models:
  claude-early-1: {input: 1, cache_write_5m: 2, cache_write_1h: 10, cache_read: 1, output: 1}
  claude-late-1: {input: 1, cache_write_5m: 5, cache_write_1h: 3, cache_read: 1, output: 1}
"#;

#[test]
fn a_model_priced_nowhere_takes_the_tightest_row_of_each_ttl() {
    let pricing = PriceTable::from_yaml(SPLIT_PRICES, "test").unwrap();
    let tables = PlanTables {
        costs: SubscriptionCosts::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    let timing = KeepaliveTiming::default();
    let at = |model: &str, ttl_ms: i64| {
        break_even_ms(
            factors_for(&pricing, &tables, Funding::ApiKey, model),
            ttl_ms,
            &timing,
        )
    };
    assert_eq!(
        (
            at("claude-early-1", FIVE_MINUTES_MS),
            at("claude-late-1", ONE_HOUR_MS)
        ),
        (Some(240_000), Some(6_480_000)),
        "each row's own figure at the TTL it is tightest on"
    );
    assert_eq!(
        (
            at("claude-unlisted-9", FIVE_MINUTES_MS),
            at("claude-unlisted-9", ONE_HOUR_MS)
        ),
        (Some(240_000), Some(6_480_000)),
        "the early row on five minutes and the late row on an hour"
    );
}
