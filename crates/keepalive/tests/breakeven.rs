//! The break-even: the pause length at which a recipe's chain of probes costs one re-write of its prefix, by the
//! model's factors under the account's funding and the entry's TTL, and the ceilings the policy resolves against it.
mod common;

use common::relay_common::test_account_row;
use phoebis_keepalive::{
    break_even_ms, factors_for, resolve, BreakEvenFactors, Funding, KeepalivePolicy,
    KeepaliveTiming, Range, FIVE_MINUTES_MS, ONE_HOUR_MS,
};
use phoebis_ledger::{CostRevisions, PlanTables, PriceRevisions, PriceTable};
use phoebis_store::{ModelRule, PoolRow};

fn factors(read: f64, write: f64) -> BreakEvenFactors {
    BreakEvenFactors {
        read,
        write_5m: write,
        write_1h: write,
    }
}

#[test]
fn the_break_even_is_write_less_read_over_cadence_times_read() {
    let timing = KeepaliveTiming::default();
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
    let one_hour_write = BreakEvenFactors {
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
fn a_zero_idle_window_and_a_null_park_field_resolve_to_the_break_even_and_only_park_numbers_cap() {
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
        (
            resolved.idle_ms,
            resolved.parked_ms,
            resolved.horizon_ms,
            resolved.break_even_ms
        ),
        (3_600_000, 2_760_000, 2_760_000, 2_760_000),
        "an idle number above the break-even is taken as set, and a park number above it reads as the break-even"
    );
    assert!(
        (resolved.idle_share - 1.2).abs() < 1e-9,
        "the hour's 15 probes reading 0.1 each cost 1.2 re-writes of 1.25, a share above one: {}",
        resolved.idle_share
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
        "a number below the break-even is taken as set on every field"
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
revisions:
  - effective: 2026-01-01
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
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-opus-5-5: opus
      claude-sonnet-5-5: sonnet
"#;

/// The tightest row is Sonnet's at both TTLs: its read is the largest share of its writes.
const PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-fable-5-1: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 0.25, output: 50}
      claude-opus-5-5: {input: 4, cache_write_5m: 5, cache_write_1h: 8, cache_read: 0.2, output: 20}
      claude-sonnet-5-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, output: 10}
"#;

#[test]
fn a_plan_recipe_reads_the_plans_multipliers_on_every_model_and_a_dollar_recipe_its_row_or_the_tightest_row(
) {
    let pricing = PriceRevisions::from_yaml(PRICES, "test")
        .unwrap()
        .latest()
        .clone();
    let tables = PlanTables {
        revisions: CostRevisions::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    let timing = KeepaliveTiming::default();
    let both = |funding: Funding, model: &str| {
        let factors = factors_for(&pricing, &tables, funding, model, false, 0, common::NOW);
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
        factors_for(
            &pricing,
            &tables,
            Funding::ApiKey,
            "claude-opus-5-5",
            false,
            0,
            common::NOW
        ),
        BreakEvenFactors {
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
        both(Funding::Plan, "claude-opus-5-5"),
        "a model the plan leaves unweighted breaks even on the plan's multipliers, since the weight cancels"
    );
    assert_eq!(
        both(Funding::Plan, "claude-unlisted-9"),
        both(Funding::Plan, "claude-opus-5-5"),
        "a plan recipe on a model neither table prices breaks even on the plan's multipliers too"
    );
    assert_eq!(
        both(Funding::ApiKey, "claude-unlisted-9"),
        (Some(2_760_000), Some(61_560_000)),
        "a dollar recipe on a model neither table prices takes the smallest break-even of the priced rows, Sonnet's"
    );
}

/// On the shipped tables `claude-opus-5` has a dollar row and a plan weight other than Opus 5.5's. A token costs the
/// model's weight times the type's multiplier, so the weight cancels from read over write and a plan recipe on it
/// breaks even where every plan recipe does.
#[test]
fn a_plan_recipe_breaks_even_on_the_plans_multipliers_whatever_the_models_weight() {
    let timing = KeepaliveTiming::default();
    let both = |pricing: &PriceTable, tables: &PlanTables, model: &str| {
        let factors = factors_for(pricing, tables, Funding::Plan, model, false, 0, common::NOW);
        (
            break_even_ms(factors, FIVE_MINUTES_MS, &timing),
            break_even_ms(factors, ONE_HOUR_MS, &timing),
        )
    };
    let (revisions, plan) = (PriceRevisions::shipped(), PlanTables::shipped());
    let prices = revisions.in_force(common::NOW).clone();
    assert_eq!(
        both(&prices, &plan, "claude-opus-5"),
        (Some(11_760_000), Some(158_760_000)),
        "44.1 h on the hour, not the 17.1 h of its dollar row"
    );
    assert_eq!(
        both(&prices, &plan, "claude-opus-5"),
        both(&prices, &plan, "claude-opus-5-5"),
        "the same figure as Opus 5.5 at another weight"
    );
    let costs = COSTS.replace(
        "overrides: {}",
        "overrides:\n      claude-sonnet-5-5: {type_multiplier: {cache_read: 0.05}}",
    );
    let overridden = PlanTables {
        revisions: CostRevisions::from_yaml(&costs).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    assert_eq!(
        both(
            PriceRevisions::from_yaml(PRICES, "test").unwrap().latest(),
            &overridden,
            "claude-sonnet-5-5"
        ),
        (Some(5_760_000), Some(77_760_000)),
        "an unweighted model's own multipliers stand in for the shared ones: a read of 0.05 against 1.25"
    );
}

/// The tightest row differs by TTL: `claude-early-1` gains the least over its read on a five-minute write and
/// `claude-late-1` on a one-hour write, so a model priced nowhere takes each TTL's smallest excess rather than one
/// whole row.
const SPLIT_PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-early-1: {input: 1, cache_write_5m: 2, cache_write_1h: 10, cache_read: 1, output: 1}
      claude-late-1: {input: 1, cache_write_5m: 5, cache_write_1h: 3, cache_read: 1, output: 1}
"#;

#[test]
fn a_model_priced_nowhere_takes_the_tightest_row_of_each_ttl() {
    let pricing = PriceRevisions::from_yaml(SPLIT_PRICES, "test")
        .unwrap()
        .latest()
        .clone();
    let tables = PlanTables {
        revisions: CostRevisions::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    let timing = KeepaliveTiming::default();
    let at = |model: &str, ttl_ms: i64| {
        break_even_ms(
            factors_for(
                &pricing,
                &tables,
                Funding::ApiKey,
                model,
                false,
                0,
                common::NOW,
            ),
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

/// The rows `PRICES` and `COSTS` give at the production timing: every plan recipe reads 0.025 against 1.25 on either
/// TTL whatever its model's weight, and Sonnet's dollar row is the tightest. The policy keeps the default hour of idle
/// window, which every combination takes as set, Sonnet's dollar row on five minutes breaking even before it.
#[tokio::test]
async fn a_pool_resolves_its_ceilings_over_its_fundings_ttls_and_models() {
    let rig = common::rig(common::RigSpec {
        pricing: PriceRevisions::from_yaml(PRICES, "test").unwrap(),
        tables: PlanTables {
            revisions: CostRevisions::from_yaml(COSTS).unwrap(),
            path: None,
            source: "test".to_string(),
        },
        timing: KeepaliveTiming::default(),
        ..common::RigSpec::default()
    })
    .await;
    let plan = test_account_row(&common::oauth("o", "sk-ant-oat01-OOOO", 0));
    let dollars = test_account_row(&common::api_key("k", common::A, 0));
    // An allow list closes with a deny of every other name, since a name no entry matches is allowed.
    let only = |models: &[&str]| -> Vec<ModelRule> {
        models
            .iter()
            .map(|m| ModelRule {
                pattern: m.to_string(),
                not: false,
            })
            .chain([ModelRule {
                pattern: "*".into(),
                not: true,
            }])
            .collect()
    };
    let pool = |cache_ttl: &str, model_rules: Vec<ModelRule>| PoolRow {
        cache_ttl_json: cache_ttl.into(),
        keepalive_json: r#"{"enabled":true}"#.into(),
        model_rules,
        ..PoolRow::for_tests("p")
    };
    let between = |min: f64, max: f64| Range { min, max };
    let one = |value: f64| between(value, value);
    let rewritten = r#"{"enabled":true}"#;
    let figures = async |pool: &PoolRow, accounts: &[phoebis_store::AccountRow]| {
        let resolved = rig
            .engine
            .resolve_pool_ceilings(pool, accounts)
            .await
            .unwrap();
        (
            (
                resolved.break_even_seconds,
                resolved.idle_seconds,
                resolved.parked_seconds,
                resolved.horizon_seconds,
            ),
            resolved.idle_share,
        )
    };

    let (seconds, share) = figures(
        &pool(rewritten, only(&["claude-opus-5-5"])),
        std::slice::from_ref(&plan),
    )
    .await;
    assert_eq!(
        seconds,
        (one(158_760.0), one(3_600.0), one(158_760.0), one(158_760.0)),
        "an OAuth pool whose policy writes one hour for main has one combination, so one figure each"
    );
    assert!(
        share.min == share.max && (share.min - 0.0222).abs() < 5e-5,
        "{share:?}"
    );

    let (seconds, share) = figures(
        &pool(rewritten, only(&["claude-opus-5-5", "claude-sonnet-5-5"])),
        &[plan.clone(), dollars.clone()],
    )
    .await;
    assert_eq!(
        seconds,
        (
            between(2_760.0, 158_760.0),
            one(3_600.0),
            between(2_760.0, 158_760.0),
            between(2_760.0, 158_760.0)
        ),
        "a mixed pool on two dollar models spans the plan's hour on Opus down to Sonnet's dollar row on five \
         minutes, the tightest row, while the idle window reads the set hour on every combination"
    );
    assert!(
        (share.min - 0.0222).abs() < 5e-5 && (share.max - 1.2).abs() < 1e-9,
        "the plan's hour costs least, and Sonnet's hour of 15 probes reading 0.2 against a re-write of 2.5 costs \
         1.2 re-writes: {share:?}"
    );

    let (seconds, _) = figures(
        &pool("{}", only(&["claude-opus-5-5"])),
        std::slice::from_ref(&dollars),
    )
    .await;
    assert_eq!(
        seconds.0,
        between(5_760.0, 126_360.0),
        "a policy that leaves the markers as sent brings both TTLs"
    );

    let (seconds, _) = figures(&pool(rewritten, Vec::new()), std::slice::from_ref(&dollars)).await;
    assert_eq!(
        seconds.0,
        between(2_760.0, 11_760.0),
        "no rule: every model the price table lists, Sonnet to Fable on five minutes"
    );

    // The dialog's override picker and a rule both name the catalog's dated id, which a price row keys undated.
    let dated_override = r#"{"enabled":true,"plan":{"models":[{"model":"claude-opus-5-5-20261001","main":"5m","subagent":"5m"}]}}"#;
    let (seconds, _) = figures(
        &pool(dated_override, Vec::new()),
        std::slice::from_ref(&plan),
    )
    .await;
    assert_eq!(
        seconds.0,
        between(11_760.0, 158_760.0),
        "a dated override that writes five minutes for main brings the plan's five-minute figure as the low end"
    );
    let (seconds, _) = figures(
        &pool(rewritten, only(&["claude-opus-5-5-20261001"])),
        std::slice::from_ref(&dollars),
    )
    .await;
    assert_eq!(
        seconds.0,
        one(5_760.0),
        "an allow entry naming the dated id reads that model's row through its undated base, not the tightest row"
    );
    // A request's model reaches the rows lowercased and without a `[1m]` suffix, so a named id is judged the same way.
    let (seconds, _) = figures(
        &pool(rewritten, only(&["Claude-Opus-5-5-20261001"])),
        std::slice::from_ref(&dollars),
    )
    .await;
    assert_eq!(
        seconds.0,
        one(5_760.0),
        "a capitalized name reads the model's row, as a request on it does"
    );
    let (seconds, _) = figures(
        &pool(rewritten, only(&["claude-opus-5-5", "claude-opus-5-5[1m]"])),
        std::slice::from_ref(&dollars),
    )
    .await;
    assert_eq!(
        seconds.0,
        one(5_760.0),
        "a `[1m]` twin is its base model and adds no combination of its own"
    );

    let (seconds, _) = figures(
        &pool(rewritten, only(&["claude-unlisted-9"])),
        std::slice::from_ref(&dollars),
    )
    .await;
    assert_eq!(
        seconds.0,
        one(2_760.0),
        "rules that allow no priced model leave the tightest row, as a recipe on an unpriced model reads"
    );
    // A wildcard alone names no model, so neither the price table nor a named id leaves a combination.
    for (account, expected, funding) in [
        (&dollars, 2_760.0, "the tightest dollar row"),
        (&plan, 158_760.0, "the plan's multipliers"),
    ] {
        let (seconds, _) = figures(
            &pool(rewritten, only(&["gpt-*"])),
            std::slice::from_ref(account),
        )
        .await;
        assert_eq!(
            seconds.0,
            one(expected),
            "rules that allow only an unpriced family read {funding}"
        );
    }

    assert_eq!(
        rig.engine
            .resolve_pool_ceilings(&pool(rewritten, Vec::new()), &[])
            .await,
        None,
        "a pool with no member resolves nothing"
    );
}

/// A usage-based seat settles its probes in dollars, so the pool's ceilings read the dollar rows for it as for an
/// API-key member, never the plan's multipliers an unclassed OAuth member brings.
#[tokio::test]
async fn a_usage_based_seat_brings_the_dollar_rows_to_the_pools_ceilings_as_an_api_key_member_does()
{
    let rig = common::rig(common::RigSpec {
        accounts: vec![common::oauth("seat", "sk-ant-oat01-SSSS", 0)],
        tier: Some("default_claude_zero"),
        pricing: PriceRevisions::from_yaml(PRICES, "test").unwrap(),
        tables: PlanTables {
            revisions: CostRevisions::from_yaml(COSTS).unwrap(),
            path: None,
            source: "test".to_string(),
        },
        timing: KeepaliveTiming::default(),
        ..common::RigSpec::default()
    })
    .await;
    let seat = test_account_row(&common::oauth("seat", "sk-ant-oat01-SSSS", 0));
    let plan = test_account_row(&common::oauth("o", "sk-ant-oat01-OOOO", 0));
    let dollars = test_account_row(&common::api_key("k", common::A, 0));
    let pool = PoolRow {
        keepalive_json: r#"{"enabled":true}"#.into(),
        model_rules: vec![
            ModelRule {
                pattern: "claude-opus-5-5".into(),
                not: false,
            },
            ModelRule {
                pattern: "*".into(),
                not: true,
            },
        ],
        ..PoolRow::for_tests("p")
    };
    let break_even = async |account: &phoebis_store::AccountRow| {
        rig.engine
            .resolve_pool_ceilings(&pool, std::slice::from_ref(account))
            .await
            .unwrap()
            .break_even_seconds
    };
    let on_the_seat = break_even(&seat).await;
    assert_eq!(
        on_the_seat,
        break_even(&dollars).await,
        "the seat reads the API-key member's dollar rows"
    );
    assert_ne!(
        on_the_seat,
        break_even(&plan).await,
        "and not the plan's multipliers an unclassed OAuth member brings"
    );
}

/// Fast cells that are no multiple of the standard ones, and a second model with no fast cells of its own.
const FAST_PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-opus-5-5:
        input: 4
        cache_write_5m: 1.25
        cache_write_1h: 2.0
        cache_read: 0.1
        output: 20
        fast: {input: 8, cache_write_5m: 2.5, cache_write_1h: 4.0, cache_read: 0.4, output: 40}
      claude-sonnet-5-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, output: 10}
"#;

#[test]
fn a_fast_recipe_reads_the_fast_cells_and_a_model_without_them_its_standard_ones() {
    let pricing = PriceRevisions::from_yaml(FAST_PRICES, "test")
        .unwrap()
        .latest()
        .clone();
    let tables = PlanTables {
        revisions: CostRevisions::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    let timing = KeepaliveTiming::default();
    let at = |model: &str, fast: bool| {
        break_even_ms(
            factors_for(
                &pricing,
                &tables,
                Funding::ApiKey,
                model,
                fast,
                0,
                common::NOW,
            ),
            FIVE_MINUTES_MS,
            &timing,
        )
    };
    assert_eq!(
        (at("claude-opus-5-5", false), at("claude-opus-5-5", true)),
        (Some(2_760_000), Some(1_260_000)),
        "46 minutes at the standard cells, 21 at the fast ones"
    );
    assert_eq!(
        at("claude-sonnet-5-5", true),
        at("claude-sonnet-5-5", false),
        "a model with no fast cells prices a fast recipe at its standard ones, as the price table lists it"
    );
    assert_eq!(
        (
            at("claude-unlisted-9", false),
            at("claude-unlisted-9", true)
        ),
        (Some(2_760_000), Some(1_260_000)),
        "a model priced nowhere takes the tightest row of the table its speed reads"
    );
}

/// Sonnet 4.5's row, keyed undated, is the tightest at five minutes, so whether it stays in a pool's range shows in the
/// range's low end: 960 s against Sonnet 5.5's 2,760 s and Fable's 11,760 s.
const UNDATED_PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-fable-5-1: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 0.25, output: 50}
      claude-sonnet-4-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.5, output: 10}
      claude-sonnet-5-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, output: 10}
"#;

/// A request on an undated spelling the catalog lists dated runs under the dated id, which is what the pool's rules
/// judge, so a deny entry naming the dated id drops the undated price row once the router maps the spelling to it.
/// Without that reading the row is judged under its own id and stays.
#[tokio::test]
async fn a_deny_naming_the_dated_id_drops_the_undated_row_the_router_maps_to_it() {
    let rig = common::rig(common::RigSpec {
        pricing: PriceRevisions::from_yaml(UNDATED_PRICES, "test").unwrap(),
        timing: KeepaliveTiming::default(),
        ..common::RigSpec::default()
    })
    .await;
    let dollars = test_account_row(&common::api_key("a", common::A, 0));
    let pool = PoolRow {
        cache_ttl_json: r#"{"enabled":true}"#.into(),
        keepalive_json: r#"{"enabled":true}"#.into(),
        model_rules: vec![ModelRule {
            pattern: "claude-sonnet-4-5-20250929".into(),
            not: true,
        }],
        ..PoolRow::for_tests("p")
    };
    let break_even = async || {
        rig.engine
            .resolve_pool_ceilings(&pool, std::slice::from_ref(&dollars))
            .await
            .unwrap()
            .break_even_seconds
    };
    assert_eq!(
        break_even().await,
        Range {
            min: 960.0,
            max: 11_760.0
        },
        "with no catalog reading the undated row is judged under its own id"
    );
    let listing = common::relay_common::catalog_rules().remove(0);
    rig.h
        .catalog_up
        .push_credential_rule(common::A, &listing.path_prefix, listing.respond);
    let a = rig.h.store.account("a").unwrap().unwrap();
    rig.h.catalog.refresh(&a).await;
    assert_eq!(
        rig.h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some("claude-sonnet-4-5-20250929")
    );
    assert_eq!(
        break_even().await,
        Range {
            min: 2_760.0,
            max: 11_760.0
        },
        "the deny on the dated id drops the row the router maps to it"
    );
}

/// One row priced by prompt length whose set over the threshold reads dearer against its writes: a read of 0.1
/// against a 1.25 write up to 1,000 prompt tokens, and 0.5 against 2.5 over them.
const LONG_CONTEXT_PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-haiku-5-5:
        input: 1
        cache_write_5m: 1.25
        cache_write_1h: 2
        cache_read: 0.1
        output: 5
        long_context: {over: 1000, input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.5, output: 10}
"#;

#[test]
fn a_prompt_over_the_threshold_breaks_even_at_the_long_context_cells() {
    let pricing = PriceRevisions::from_yaml(LONG_CONTEXT_PRICES, "test")
        .unwrap()
        .latest()
        .clone();
    let tables = PlanTables {
        revisions: CostRevisions::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    };
    let timing = KeepaliveTiming::default();
    let at = |model: &str, fast: bool, prompt_tokens: u64| {
        break_even_ms(
            factors_for(
                &pricing,
                &tables,
                Funding::ApiKey,
                model,
                fast,
                prompt_tokens,
                common::NOW,
            ),
            FIVE_MINUTES_MS,
            &timing,
        )
    };
    assert_eq!(
        (
            at("claude-haiku-5-5", false, 0),
            at("claude-haiku-5-5", false, 1_000),
            at("claude-haiku-5-5", false, 1_001)
        ),
        (Some(2_760_000), Some(2_760_000), Some(960_000)),
        "46 minutes up to the threshold, 16 past it"
    );
    assert_eq!(
        at("claude-haiku-5-5-20261001", false, 1_001),
        Some(960_000),
        "a dated id reads its undated row's set"
    );
    assert_eq!(
        at("claude-haiku-5-5", true, 1_001),
        Some(960_000),
        "a model without fast cells prices a fast recipe at the set its prompt selects, as the price table does"
    );
}
