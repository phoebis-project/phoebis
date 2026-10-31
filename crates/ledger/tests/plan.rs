//! The subscription cost model: a token costs a weight times a multiplier in units, a window holds a capacity per
//! tier, and the peak band is a capacity factor per window. Every expected figure is worked out by hand from the
//! round numbers of the test table (`tests/fixtures/subscription_costs.yaml`), on its first revision unless a case
//! crosses to the second. The shipped file is held to its shape.

use phoebis_ledger::test_tables::{self, JANUARY_2026, SUBSCRIPTION_COSTS};
use phoebis_ledger::{
    effective_instant, entrypoint_of_client, funding_of, price_equivalent, price_shares,
    CostRevisions, Funding, LedgerError, PlanTables, SubscriptionCosts, BILLED_FUNDING,
    FAST_FUNDING,
};
use phoebis_routing::{PinUsage, Rates, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::PinBurn;

/// Monday 2026-09-21 at 14:00 UTC, inside the band while Pacific daylight time holds.
const SEPTEMBER_AFTERNOON: i64 = 1_789_999_200_000;
/// Monday 2026-09-21 at 12:00 UTC, the band's first minute in September.
const SEPTEMBER_NOON: i64 = 1_789_992_000_000;
/// Monday 2026-09-21 at 11:59:59 UTC, the last second before the band in September.
const SEPTEMBER_BEFORE_NOON: i64 = 1_789_991_999_000;
/// Monday 2026-09-21 at 18:00 UTC, the band's end in September, which it excludes.
const SEPTEMBER_EVENING: i64 = 1_790_013_600_000;
/// Saturday 2026-09-26 at 14:00 UTC: the band's hours on a weekend day.
const SATURDAY_AFTERNOON: i64 = 1_790_431_200_000;
/// Monday 2026-11-02 at 12:30 UTC: inside the band's September hours, before it once daylight time has ended.
const NOVEMBER_HALF_PAST_TWELVE: i64 = 1_793_622_600_000;
/// Monday 2026-11-02 at 13:30 UTC, inside the band once daylight time has ended.
const NOVEMBER_HALF_PAST_ONE: i64 = 1_793_626_200_000;
/// Monday 2026-11-02 at 18:30 UTC: past the band's September hours, still inside it in November.
const NOVEMBER_HALF_PAST_SIX: i64 = 1_793_644_200_000;
/// Friday 2026-09-25 at 23:00 UTC, 16:00 in the band's zone: past the band on the last weekday before a weekend.
const FRIDAY_NIGHT: i64 = 1_790_377_200_000;

/// Inside the band and outside it, on the same weekday.
const IN_BAND: i64 = SEPTEMBER_AFTERNOON;
const OUT_OF_BAND: i64 = SEPTEMBER_BEFORE_NOON;
/// Thursday 2026-01-15 at 14:00 UTC, inside the band and inside the test table's first revision.
const JANUARY_IN_BAND: i64 = 1_768_485_600_000;

const MAX_20X: &str = "default_claude_max_20x";
const MAX_5X: &str = "default_claude_max_5x";
const PRO: &str = "default_claude_ai";
const TEAM: &str = "default_claude_team";
const OPUS: &str = "claude-opus-5-5";
const HAIKU: &str = "claude-haiku-4-5";
const FABLE: &str = "claude-fable-5-1";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

fn assert_share(got: Option<f64>, want: f64, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: no share, want {want}"));
    assert!(close(got, want), "{what}: {got} against {want}");
}

/// A rate is the share of the usage that counts its tokens a second, so the two answer alike, `None` included.
fn assert_rate_follows_share(rate: Option<f64>, share: Option<f64>, what: &str) {
    match (rate, share) {
        (Some(rate), Some(share)) => assert!(close(rate, share), "{what}: {rate} against {share}"),
        (rate, share) => assert_eq!(rate, share, "{what}"),
    }
}

fn input(tokens: u64) -> PinUsage {
    PinUsage {
        input: tokens,
        ..Default::default()
    }
}

fn cache_read(tokens: u64) -> PinUsage {
    PinUsage {
        cache_read: tokens,
        ..Default::default()
    }
}

fn output(tokens: u64) -> PinUsage {
    PinUsage {
        output: tokens,
        ..Default::default()
    }
}

fn input_burn(tokens: f64) -> PinBurn {
    PinBurn {
        input: tokens,
        ..Default::default()
    }
}

fn tables() -> PlanTables {
    test_tables::subscription_costs()
}

/// The test table's first revision, whose round numbers the figures below are worked out from.
fn first() -> SubscriptionCosts {
    tables().revisions.in_force(JANUARY_2026).clone()
}

fn per_type(rates: Rates) -> [Option<f64>; 5] {
    [
        rates.input,
        rates.cache_read,
        rates.write_5m,
        rates.write_1h,
        rates.output,
    ]
}

fn all_close(got: [Option<f64>; 5], want: [Option<f64>; 5]) -> bool {
    got.iter().zip(want).all(|(g, w)| match (g, w) {
        (Some(g), Some(w)) => close(*g, w),
        (None, None) => true,
        _ => false,
    })
}

// Opus 5.5 weighs 2 and an input token multiplies by 1, so one input token is 2 units: 2 / 100 of the Max 20x
// 5-hour window out of band, over the band's 0.5 inside it.
#[test]
fn one_input_token_spends_its_weight_over_the_tiers_five_hour_capacity_and_twice_that_inside_the_band(
) {
    let tables = first();
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), OUT_OF_BAND),
        0.02,
        "out of band",
    );
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), IN_BAND),
        0.04,
        "in band",
    );
}

// One Opus 5.5 input token a second is 2 units a second: 2 / 100 of the Max 20x 5-hour window per second out of
// band, over the band's 0.5 inside it.
#[test]
fn one_input_token_a_second_burns_its_weight_over_the_capacity_per_second_and_twice_that_inside_the_band(
) {
    let tables = first();
    for (at, want, what) in [
        (OUT_OF_BAND, 0.02, "out of band"),
        (IN_BAND, 0.04, "in band"),
    ] {
        let rate = tables.share_rate_at(FIVE_HOUR, None, MAX_20X, OPUS, &input_burn(1.0), at);
        assert_share(rate, want, what);
        assert_rate_follows_share(
            rate,
            tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), at),
            what,
        );
    }
}

// Opus 5.5 at weight 2 over the multipliers input 1, cache read 0.1, both writes 2 and output 10:
// (2 + 0.2 + 4 + 4 + 20) / 100 per second for one token of each type a second.
#[test]
fn a_burn_of_every_type_sums_its_five_rates_as_a_usage_sums_its_five_counts() {
    let tables = first();
    let one_of_each = PinBurn {
        cache_read: 1.0,
        input: 1.0,
        output: 1.0,
        write_1h: 1.0,
        write_5m: 1.0,
    };
    let rate = tables.share_rate_at(FIVE_HOUR, None, MAX_20X, OPUS, &one_of_each, OUT_OF_BAND);
    assert_share(rate, 0.302, "one token of each type a second");
    assert_rate_follows_share(
        rate,
        tables.share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            OPUS,
            PinUsage {
                input: 1,
                cache_read: 1,
                write_5m: 1,
                write_1h: 1,
                output: 1,
            },
            OUT_OF_BAND,
        ),
        "one token of each type a second",
    );
    let apart = PinBurn {
        cache_read: 2.0,
        input: 1.0,
        output: 5.0,
        write_1h: 4.0,
        write_5m: 3.0,
    };
    assert_rate_follows_share(
        tables.share_rate_at(FIVE_HOUR, None, MAX_20X, HAIKU, &apart, OUT_OF_BAND),
        tables.share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            HAIKU,
            PinUsage {
                input: 1,
                cache_read: 2,
                write_5m: 3,
                write_1h: 4,
                output: 5,
            },
            OUT_OF_BAND,
        ),
        "each type at its own rate, under Haiku's own cache read multiplier",
    );
}

// The Fable week is the 7-day window at a capacity factor of 0.5: 2 / 1000 / 0.5. Pro holds 5 units on the 5-hour
// window: 2 / 5.
#[test]
fn the_rate_on_a_model_week_and_on_a_smaller_tier_follows_share_at() {
    let tables = first();
    for (window, tier, want, what) in [
        (SEVEN_DAY_FABLE, MAX_20X, 0.004, "the Fable week"),
        (FIVE_HOUR, PRO, 0.4, "Pro"),
    ] {
        let rate = tables.share_rate_at(window, None, tier, OPUS, &input_burn(1.0), OUT_OF_BAND);
        assert_share(rate, want, what);
        assert_rate_follows_share(
            rate,
            tables.share_at(window, None, tier, OPUS, input(1), OUT_OF_BAND),
            what,
        );
    }
}

// The mystery tier has no weighted model and GPT-5 no tier. The file lacks the enterprise tier, and Team holds no
// capacity on the week.
#[test]
fn a_model_of_no_weighted_tier_or_a_tier_without_capacity_prices_no_rate() {
    let tables = first();
    for model in ["claude-mystery-2", "gpt-5"] {
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            let what = format!("{model} on {window}");
            let rate =
                tables.share_rate_at(window, None, MAX_20X, model, &input_burn(1.0), OUT_OF_BAND);
            assert_eq!(rate, None, "{what}");
            assert_rate_follows_share(
                rate,
                tables.share_at(window, None, MAX_20X, model, input(1), OUT_OF_BAND),
                &what,
            );
        }
    }
    for (window, tier) in [
        (FIVE_HOUR, "claude_enterprise"),
        (SEVEN_DAY, TEAM),
        (SEVEN_DAY_FABLE, TEAM),
    ] {
        let what = format!("{tier} on {window}");
        let rate = tables.share_rate_at(window, None, tier, OPUS, &input_burn(1.0), OUT_OF_BAND);
        assert_eq!(rate, None, "{what}");
        assert_rate_follows_share(
            rate,
            tables.share_at(window, None, tier, OPUS, input(1), OUT_OF_BAND),
            &what,
        );
    }
}

#[test]
fn a_zero_burn_prices_zero_on_a_weighted_model() {
    let tables = first();
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        for at in [IN_BAND, OUT_OF_BAND] {
            assert_eq!(
                tables.share_rate_at(window, None, MAX_20X, OPUS, &PinBurn::default(), at),
                Some(0.0),
                "{window} at {at}"
            );
        }
    }
}

// Opus 5 (a null weight), Opus 4.8 (a tier entry and no weight) and Opus 9 (no row at all) burn at Opus 5.5's 2,
// the latest weighted model of their tier, as their share prices.
#[test]
fn a_model_without_a_weight_burns_at_its_tiers_latest_weight_as_its_share_prices() {
    let tables = first();
    assert_share(
        tables.share_rate_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            "claude-opus-5",
            &input_burn(1.0),
            OUT_OF_BAND,
        ),
        0.02,
        "Opus 5 at Opus 5.5's weight: 2 / 100",
    );
    for model in ["claude-opus-5", "claude-opus-4-8", "claude-opus-9"] {
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            assert_rate_follows_share(
                tables.share_rate_at(window, None, MAX_20X, model, &input_burn(1.0), OUT_OF_BAND),
                tables.share_at(window, None, MAX_20X, model, input(1), OUT_OF_BAND),
                &format!("{model} on {window}"),
            );
        }
    }
}

// The second revision doubles every weight, so one Opus 5.5 input token a second burns 4 / 100 a second from
// 2026-02-01 on.
#[test]
fn the_tables_price_a_burn_at_the_revision_in_force_at_the_instant() {
    let tables = tables();
    for (at, want, what) in [
        (JANUARY_2026, 0.02, "the first revision in January"),
        (OUT_OF_BAND, 0.04, "the second revision in September"),
    ] {
        let rate = tables.share_rate_at(FIVE_HOUR, None, MAX_20X, OPUS, &input_burn(1.0), at);
        assert_share(rate, want, what);
        assert_rate_follows_share(
            rate,
            tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), at),
            what,
        );
    }
}

// The week holds 1000 units on Max 20x and its capacity factor is 1, so the band leaves it at 2 / 1000.
#[test]
fn the_week_prices_on_its_own_capacity_and_its_factor_of_one_leaves_it_unbanded() {
    let tables = first();
    for at in [IN_BAND, OUT_OF_BAND] {
        assert_share(
            tables.share_at(SEVEN_DAY, None, MAX_20X, OPUS, input(1), at),
            0.002,
            &format!("the week at {at}"),
        );
    }
}

// Pro holds 5 units on the 5-hour window: 2 / 5.
#[test]
fn a_pro_account_spends_the_same_units_over_its_own_smaller_capacity() {
    assert_share(
        first().share_at(FIVE_HOUR, None, PRO, OPUS, input(1), OUT_OF_BAND),
        0.4,
        "Pro",
    );
}

// The Fable week is the 7-day window at a capacity factor of 0.5: 2 / 1000 / 0.5.
#[test]
fn a_model_week_reads_the_seven_day_share_over_its_capacity_factor() {
    assert_share(
        first().share_at(SEVEN_DAY_FABLE, None, MAX_20X, OPUS, input(1), OUT_OF_BAND),
        0.004,
        "the Fable week",
    );
}

// Opus 5.5 at weight 2 over the multipliers input 1, cache read 0.1, both writes 2 and output 10:
// (2 + 0.2 + 4 + 4 + 20) / 100.
#[test]
fn every_token_type_costs_the_weight_times_its_shared_multiplier() {
    let one_of_each = PinUsage {
        input: 1,
        cache_read: 1,
        write_5m: 1,
        write_1h: 1,
        output: 1,
    };
    assert_share(
        first().share_at(FIVE_HOUR, None, MAX_20X, OPUS, one_of_each, OUT_OF_BAND),
        0.302,
        "one token of each type",
    );
}

// Haiku weighs 0.5, and its override sets the cache read multiplier to 0.2 and its weight on `seven_day` to 1.
#[test]
fn an_override_replaces_its_multiplier_everywhere_and_its_weight_on_the_window_it_names_alone() {
    let tables = first();
    assert_share(
        tables.share_at(SEVEN_DAY, None, MAX_20X, HAIKU, cache_read(1), OUT_OF_BAND),
        0.0002,
        "one cache read on the week: weight 1 times 0.2 over 1000",
    );
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, HAIKU, input(1), OUT_OF_BAND),
        0.005,
        "the 5-hour window keeps the weight 0.5: 0.5 / 100",
    );
    assert_share(
        tables.share_at(SEVEN_DAY_FABLE, None, MAX_20X, HAIKU, input(1), OUT_OF_BAND),
        0.001,
        "the override names `seven_day` alone, so the Fable week keeps 0.5: 0.5 / 1000 / 0.5",
    );
    assert_share(
        tables.share_at(
            SEVEN_DAY,
            None,
            MAX_20X,
            HAIKU,
            PinUsage {
                write_5m: 1,
                ..Default::default()
            },
            OUT_OF_BAND,
        ),
        0.002,
        "a type the override leaves keeps the shared multiplier: 1 x 2 / 1000",
    );
}

#[test]
fn a_dated_id_prices_at_its_undated_weight() {
    assert_share(
        first().share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            "claude-opus-5-5-20260401",
            input(1),
            OUT_OF_BAND,
        ),
        0.02,
        "a dated Opus 5.5",
    );
}

// Opus 5 (a null weight), Opus 4.8 (a tier entry and no weight) and Opus 9 (no row at all) read Opus 5.5's 2,
// the latest weighted model of their tier. The mystery tier has no weighted model, and GPT-5 no tier.
#[test]
fn a_model_without_a_weight_prices_at_its_tiers_latest_weight_and_a_model_of_no_weighted_tier_prices_nothing(
) {
    let tables = first();
    assert_share(
        tables.share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            "claude-opus-5",
            input(1),
            OUT_OF_BAND,
        ),
        0.02,
        "Opus 5 at Opus 5.5's weight: 2 / 100",
    );
    for model in ["claude-opus-5", "claude-opus-4-8", "claude-opus-9"] {
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            assert_eq!(
                tables.share_at(window, None, MAX_20X, model, input(1), OUT_OF_BAND),
                tables.share_at(window, None, MAX_20X, OPUS, input(1), OUT_OF_BAND),
                "{model} on {window}"
            );
            assert!(
                tables.priced(window, None, MAX_20X, model),
                "{model} on {window}"
            );
        }
    }
    for model in ["claude-mystery-2", "gpt-5"] {
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            assert_eq!(
                tables.share_at(window, None, MAX_20X, model, input(1), OUT_OF_BAND),
                None,
                "{model} on {window}"
            );
            assert!(
                !tables.priced(window, None, MAX_20X, model),
                "{model} on {window}"
            );
        }
    }
    assert!(tables.priced(FIVE_HOUR, None, MAX_20X, OPUS));
}

// The second revision doubles every weight, so one Opus 5.5 input token reads 4 / 100 from 2026-02-01 on.
#[test]
fn the_tables_price_at_the_revision_in_force_at_the_instant_and_hold_the_next_one() {
    let tables = tables();
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), JANUARY_2026),
        0.02,
        "the first revision in January",
    );
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), OUT_OF_BAND),
        0.04,
        "the second revision in September",
    );
    assert_eq!(
        tables
            .revisions
            .pending(JANUARY_2026)
            .map(|r| r.effective.as_str()),
        Some("2026-02-01")
    );
    assert_eq!(tables.revisions.pending(OUT_OF_BAND), None);
    assert!(tables.priced(FIVE_HOUR, None, MAX_20X, OPUS, JANUARY_2026));
    assert!(tables.measured(SEVEN_DAY_FABLE, JANUARY_2026));
    assert_eq!(
        tables.tier_factor(PRO, FIVE_HOUR, None, JANUARY_2026),
        Some(1.0)
    );
    assert_eq!(tables.band_factor(FIVE_HOUR, None, IN_BAND), 2.0);
    // The second revision's Opus 5.5 weight of 4 over the anchor's 5-hour row of 5.
    assert_eq!(
        tables.rate_table(FIVE_HOUR, None, OUT_OF_BAND).rows[OPUS].input,
        Some(0.8)
    );
}

#[test]
fn a_model_reads_its_listed_tier_then_its_undated_tier_then_the_tier_its_id_names() {
    let tables = tables();
    for (model, tier) in [
        ("claude-opus-5-5", Some("opus")),
        ("claude-opus-5-5-20260401", Some("opus")),
        ("claude-opus-6", Some("opus")),
        ("claude-mystery-2", Some("mystery")),
        ("gpt-5", None),
    ] {
        assert_eq!(tables.tier_of(model, JANUARY_2026), tier, "{model}");
    }
}

#[test]
fn the_model_order_reads_the_tier_order_then_the_newer_version_then_the_id() {
    let shuffled = [
        "claude-haiku-4-5",
        "gpt-5",
        "claude-opus-4-8",
        "claude-mystery-2",
        "claude-sonnet-5-5",
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-opus-5-5",
    ];
    assert_eq!(
        tables().model_order(&shuffled, JANUARY_2026),
        [
            "claude-fable-5-1",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-sonnet-5-5",
            "claude-haiku-4-5",
            "claude-mystery-2",
            "gpt-5",
        ]
    );
}

#[test]
fn the_latest_weighted_model_of_a_tier_is_the_first_in_the_model_order_with_a_weight() {
    let tables = tables();
    for (tier, model) in [
        ("fable", Some("claude-fable-5-1")),
        ("opus", Some("claude-opus-5-5")),
        ("sonnet", Some("claude-sonnet-5-5")),
        ("haiku", Some("claude-haiku-4-5")),
        ("mystery", None),
    ] {
        assert_eq!(tables.latest_weighted(tier, JANUARY_2026), model, "{tier}");
    }
}

#[test]
fn a_model_without_a_weight_reads_its_tiers_latest_weight_marked_as_a_fallback() {
    let costs = first();
    assert_eq!(
        costs.weight_or_fallback(OPUS, FIVE_HOUR),
        Some((2.0, false))
    );
    for model in ["claude-opus-5", "claude-opus-4-8", "claude-opus-9"] {
        assert_eq!(
            costs.weight_or_fallback(model, FIVE_HOUR),
            Some((2.0, true)),
            "{model}"
        );
    }
    for model in ["claude-mystery-2", "gpt-5"] {
        assert_eq!(costs.weight_or_fallback(model, FIVE_HOUR), None, "{model}");
    }
}

// Haiku 4.5 weighs 0.5 with an override of 1 on the week, which names Haiku 4.5 alone.
#[test]
fn a_fallback_reads_the_donors_base_weight_and_never_its_override() {
    let costs = first();
    for window in [FIVE_HOUR, SEVEN_DAY] {
        assert_eq!(
            costs.weight_or_fallback("claude-haiku-9", window),
            Some((0.5, true)),
            "{window}"
        );
    }
    assert_eq!(
        costs.weight_or_fallback(HAIKU, FIVE_HOUR),
        Some((0.5, false))
    );
    assert_eq!(
        costs.weight_or_fallback(HAIKU, SEVEN_DAY),
        Some((1.0, false))
    );
}

// Opus 5 and Opus 4.8 have a tier entry and no weight while Opus 5.5 weighs, and Mystery 2's tier has none.
#[test]
fn the_by_tier_count_reads_the_tier_entries_without_a_weight_whose_tier_has_one() {
    assert_eq!(first().by_tier_count(), 2);
    assert_eq!(first().weighted_model_count(), 4);
}

// Team holds 10 units on the 5-hour window and none on the week.
#[test]
fn a_tier_the_file_lacks_or_a_null_capacity_prices_nothing_on_that_window() {
    let tables = first();
    assert_eq!(
        tables.share_at(
            FIVE_HOUR,
            None,
            "claude_enterprise",
            OPUS,
            input(1),
            OUT_OF_BAND
        ),
        None
    );
    assert!(!tables.priced(FIVE_HOUR, None, "claude_enterprise", OPUS));
    for window in [SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert_eq!(
            tables.share_at(window, None, TEAM, OPUS, input(1), OUT_OF_BAND),
            None,
            "Team on {window}"
        );
        assert!(!tables.priced(window, None, TEAM, OPUS), "Team on {window}");
    }
    assert_share(
        tables.share_at(FIVE_HOUR, None, TEAM, OPUS, input(1), OUT_OF_BAND),
        0.2,
        "Team on the 5-hour window: 2 / 10",
    );
    assert!(tables.priced(FIVE_HOUR, None, TEAM, OPUS));
}

// The equivalent is the share of the anchor account tier, Pro: one Opus 5.5 input token is 2 units, 2 / 5 of the
// 5-hour window out of band and over the band's 0.5 inside it, 2 / 50 of the week, and one Fable 5.1 input token is
// 10 units over the week's 50 and the Fable week's 0.5.
#[test]
fn the_equivalent_is_the_share_of_the_pro_windows_on_the_three_columns() {
    let tables = tables();
    assert_share(
        tables.equivalent_at(FIVE_HOUR, None, OPUS, input(1), JANUARY_2026),
        0.4,
        "the 5-hour window out of band",
    );
    assert_share(
        tables.equivalent_at(FIVE_HOUR, None, OPUS, input(1), JANUARY_IN_BAND),
        0.8,
        "the 5-hour window inside the band",
    );
    assert_share(
        first().equivalent_at(FIVE_HOUR, None, OPUS, input(1), IN_BAND),
        0.8,
        "one revision's equivalent reads the band at its instant",
    );
    assert_share(
        tables.equivalent_at(SEVEN_DAY, None, OPUS, input(1), JANUARY_2026),
        0.04,
        "the week",
    );
    assert_share(
        tables.equivalent_at(SEVEN_DAY_FABLE, None, FABLE, input(1), JANUARY_2026),
        0.4,
        "the Fable week: 10 / 50 / 0.5",
    );
    for window in [FIVE_HOUR, SEVEN_DAY] {
        assert_eq!(
            tables.equivalent_at(window, None, "claude-mystery-2", input(1), JANUARY_2026),
            None,
            "a model tier with no weighted model on {window}"
        );
    }
}

#[test]
fn the_equivalent_answers_the_model_week_for_a_model_that_carries_one_and_the_fallback_marked() {
    let tables = tables();
    let fable = price_equivalent(&tables, None, FABLE, input(1), JANUARY_2026, JANUARY_2026);
    assert_eq!(
        fable.shares.keys().copied().collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]
    );
    for (window, want) in [(FIVE_HOUR, 2.0), (SEVEN_DAY, 0.2), (SEVEN_DAY_FABLE, 0.4)] {
        assert_share(fable.shares[window], want, window);
        assert!(!fable.estimated[window], "{window}");
    }
    let opus = price_equivalent(&tables, None, OPUS, input(1), JANUARY_2026, JANUARY_2026);
    assert_eq!(
        opus.shares.keys().copied().collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY]
    );
    assert_share(opus.shares[FIVE_HOUR], 0.4, "Opus 5.5 on the 5-hour window");
    assert_share(opus.shares[SEVEN_DAY], 0.04, "Opus 5.5 on the week");
    assert!(!opus.any_estimated());
    let unweighted = price_equivalent(
        &tables,
        None,
        "claude-opus-5",
        input(1),
        JANUARY_2026,
        JANUARY_2026,
    );
    assert_eq!(
        unweighted.shares, opus.shares,
        "Opus 5 at Opus 5.5's weight"
    );
    assert!(
        unweighted.estimated.values().all(|estimated| *estimated),
        "{unweighted:?}"
    );
}

#[test]
fn a_share_priced_at_its_model_tiers_latest_weight_is_marked_estimated() {
    let tables = tables();
    let unweighted = price_shares(
        &tables,
        None,
        MAX_20X,
        "claude-opus-5",
        input(1),
        JANUARY_2026,
        JANUARY_2026,
    );
    assert_eq!(
        unweighted.estimated.into_iter().collect::<Vec<_>>(),
        [(FIVE_HOUR, true), (SEVEN_DAY, true)]
    );
    let weighted = price_shares(
        &tables,
        None,
        MAX_20X,
        OPUS,
        input(1),
        JANUARY_2026,
        JANUARY_2026,
    );
    assert_eq!(
        weighted.estimated.into_iter().collect::<Vec<_>>(),
        [(FIVE_HOUR, false), (SEVEN_DAY, false)]
    );
    assert_eq!(unweighted.shares, weighted.shares);
}

// Haiku 4.5 keeps its override of 1 on the week with its shared weight set to null, and Haiku 5.5 weighs 0.25: the
// week prices on the override's own weight, the 5-hour window on Haiku 5.5's as the estimate.
#[test]
fn a_null_weight_with_an_override_on_one_window_marks_the_other_windows_alone_as_estimated() {
    let text = test_tables::first_entry_costs_text()
        .replace(
            "      claude-haiku-4-5: 0.5\n",
            "      claude-haiku-4-5: null\n      claude-haiku-5-5: 0.25\n",
        )
        .replace(
            "      claude-haiku-4-5: haiku\n",
            "      claude-haiku-4-5: haiku\n      claude-haiku-5-5: haiku\n",
        );
    let edited = PlanTables {
        revisions: CostRevisions::from_yaml(&text).expect("the edited model loads"),
        path: None,
        source: "test".to_string(),
    };
    // The Max 20x share reads Haiku 5.5's 0.25 over 100 and the override's 1 over 1000, the equivalent the same units
    // over the anchor's 5 and 50.
    for (priced, five_hour, seven_day) in [
        (
            price_shares(
                &edited,
                None,
                MAX_20X,
                HAIKU,
                input(1),
                JANUARY_2026,
                JANUARY_2026,
            ),
            0.0025,
            0.001,
        ),
        (
            price_equivalent(&edited, None, HAIKU, input(1), JANUARY_2026, JANUARY_2026),
            0.05,
            0.02,
        ),
    ] {
        assert_share(priced.shares[FIVE_HOUR], five_hour, "Haiku 5.5's 0.25");
        assert_share(priced.shares[SEVEN_DAY], seven_day, "the override's 1");
        assert_eq!(
            priced
                .estimated
                .iter()
                .map(|(w, e)| (*w, *e))
                .collect::<Vec<_>>(),
            [(FIVE_HOUR, true), (SEVEN_DAY, false)]
        );
        assert!(priced.any_estimated());
    }
    let unedited = price_shares(
        &tables(),
        None,
        MAX_20X,
        HAIKU,
        input(1),
        JANUARY_2026,
        JANUARY_2026,
    );
    assert!(!unedited.any_estimated(), "{unedited:?}");
}

// The worked figures on the shipped file, each derived from its members rather than written down, so a revision edits
// no test: one million input tokens of Opus 5.5 out of the band and inside it and of Fable 5.1, one million Opus 5.5
// output tokens, and the Opus 5.5 request as a share of a Max 20x account's own 5-hour window.
#[test]
fn the_shipped_equivalent_is_the_units_over_the_pro_capacity_and_a_max_20x_share_scales_it_by_the_capacities(
) {
    let shipped = CostRevisions::shipped();
    let costs = shipped.latest().clone();
    // The instants are found inside the latest entry rather than written down, so its date and weekday edit no test:
    // the first half hour after its date outside the band, and the first inside it.
    let from = effective_instant(&costs.effective).expect("the entry's date");
    let half_hours = (0..7 * 48).map(|n| from + n * 1_800_000 + 900_000);
    let out_of_band = half_hours
        .clone()
        .find(|at| costs.band_factor(FIVE_HOUR, None, *at) == 1.0)
        .expect("an instant out of band");
    let in_band = half_hours
        .clone()
        .find(|at| costs.band_factor(FIVE_HOUR, None, *at) > 1.0)
        .expect("the band falls inside a week");
    for at in [out_of_band, in_band] {
        assert_eq!(
            shipped.in_force(at).effective,
            costs.effective,
            "the entry is in force at {at}"
        );
    }
    for model in [OPUS, FABLE] {
        assert!(
            !costs.overrides.contains_key(model),
            "{model} carries no override"
        );
    }
    let anchor = &costs.capacity[&costs.unit.tier];
    let (five_hour, seven_day) = (anchor.five_hour.unwrap(), anchor.seven_day.unwrap());
    let fable_week = costs.model_windows[SEVEN_DAY_FABLE].capacity_factor;
    let million = 1_000_000;
    let units = |model: &str, multiplier: f64| {
        million as f64 * costs.model_weight[model].unwrap() * multiplier
    };
    let multiplier = &costs.type_multiplier;
    let opus = units(OPUS, multiplier.input);
    let opus_equivalent = opus / five_hour;
    assert_share(
        costs.equivalent_at(FIVE_HOUR, None, OPUS, input(million), out_of_band),
        opus_equivalent,
        "Opus 5.5 input on the 5-hour window",
    );
    assert_share(
        costs.equivalent_at(FIVE_HOUR, None, OPUS, input(million), in_band),
        opus_equivalent / costs.peak.capacity_factor.five_hour,
        "Opus 5.5 input on the 5-hour window inside the band",
    );
    assert_share(
        costs.equivalent_at(SEVEN_DAY, None, OPUS, input(million), out_of_band),
        opus / seven_day,
        "Opus 5.5 input on the week",
    );
    let fable = units(FABLE, multiplier.input);
    for (window, want) in [
        (FIVE_HOUR, fable / five_hour),
        (SEVEN_DAY, fable / seven_day),
        (SEVEN_DAY_FABLE, fable / seven_day / fable_week),
    ] {
        assert_share(
            costs.equivalent_at(window, None, FABLE, input(million), out_of_band),
            want,
            &format!("Fable 5.1 input on {window}"),
        );
    }
    assert_share(
        costs.equivalent_at(FIVE_HOUR, None, OPUS, output(million), out_of_band),
        units(OPUS, multiplier.output) / five_hour,
        "Opus 5.5 output on the 5-hour window",
    );
    let max_20x = costs.capacity[MAX_20X].five_hour.unwrap();
    assert_share(
        costs.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(million), out_of_band),
        opus_equivalent * five_hour / max_20x,
        "the Max 20x share is the equivalent times the anchor's capacity over Max 20x's",
    );
}

// Max 20x holds 100 against the anchor's 5 on the 5-hour window, Max 5x 250 against its 50 on the week.
#[test]
fn the_tier_factor_is_the_capacity_over_the_anchors_and_reads_a_tier_with_or_without_its_prefix() {
    let tables = first();
    assert_share(
        tables.tier_factor("claude_max_20x", FIVE_HOUR, None),
        20.0,
        "without the prefix",
    );
    assert_share(
        tables.tier_factor(MAX_20X, FIVE_HOUR, None),
        20.0,
        "as reported",
    );
    assert_share(
        tables.tier_factor(MAX_5X, SEVEN_DAY, None),
        5.0,
        "Max 5x week",
    );
    assert_share(
        tables.tier_factor(MAX_5X, SEVEN_DAY_FABLE, None),
        5.0,
        "a model week reads the week's ratio",
    );
    assert_eq!(tables.tier_factor(TEAM, SEVEN_DAY, None), None);
    assert_eq!(tables.tier_factor(MAX_20X, "five_hour_x", None), None);
    assert_eq!(
        tables.tier_factor("claude_enterprise", FIVE_HOUR, None),
        None
    );
}

// The 5-hour capacity factor is 0.5 inside the band, so the band weighs a 5-hour charge 2; the week's is 1.
#[test]
fn the_band_factor_is_the_reciprocal_of_the_windows_capacity_factor_inside_the_band() {
    let tables = first();
    assert_eq!(tables.band_factor(FIVE_HOUR, None, IN_BAND), 2.0);
    assert_eq!(tables.band_factor(FIVE_HOUR, None, OUT_OF_BAND), 1.0);
    assert_eq!(tables.band_factor(SEVEN_DAY, None, IN_BAND), 1.0);
    assert_eq!(tables.band_factor(SEVEN_DAY_FABLE, None, IN_BAND), 1.0);
    assert_eq!(tables.band_factor("x", None, IN_BAND), 1.0);
}

#[test]
fn the_band_covers_weekday_hours_in_its_zone_and_follows_the_zones_daylight_rule() {
    let tables = first();
    let factor = |at: i64| tables.band_factor(FIVE_HOUR, None, at);
    assert_eq!(factor(SEPTEMBER_AFTERNOON), 2.0);
    assert_eq!(factor(SEPTEMBER_NOON), 2.0, "the band's first minute");
    assert_eq!(factor(SEPTEMBER_BEFORE_NOON), 1.0);
    assert_eq!(
        factor(SEPTEMBER_EVENING),
        1.0,
        "the band ends before its closing hour"
    );
    assert_eq!(
        factor(SATURDAY_AFTERNOON),
        1.0,
        "a weekend afternoon is outside a weekday band"
    );
    // Pacific daylight time ends on 2026-11-01, so the UTC edges move an hour later.
    assert_eq!(factor(NOVEMBER_HALF_PAST_TWELVE), 1.0);
    assert_eq!(factor(NOVEMBER_HALF_PAST_ONE), 2.0);
    assert_eq!(factor(NOVEMBER_HALF_PAST_SIX), 2.0);
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), SATURDAY_AFTERNOON),
        0.02,
        "a weekend charge is the plain share",
    );
}

#[test]
fn the_band_segments_cut_a_span_at_the_bands_edges_in_its_zone() {
    const HOUR: i64 = 3_600_000;
    const MINUTE: i64 = 60_000;
    let tables = first();
    let segments = |window: &str, entrypoint: Option<&str>, from: i64, to: i64| {
        let segments = tables.band_segments(window, entrypoint, from, to);
        let covered = segments.first().map(|s| s.from) == Some(from)
            && segments.last().map(|s| s.to) == Some(to)
            && segments.windows(2).all(|pair| pair[0].to == pair[1].from);
        assert!(covered, "{segments:?} covers [{from}, {to}) end to end");
        segments
            .iter()
            .map(|s| (s.from, s.to, s.weight))
            .collect::<Vec<_>>()
    };
    let five_hour = |from: i64, to: i64| segments(FIVE_HOUR, None, from, to);
    assert_eq!(
        five_hour(SEPTEMBER_NOON, SEPTEMBER_NOON + 2 * HOUR),
        [(SEPTEMBER_NOON, SEPTEMBER_NOON + 2 * HOUR, 2.0)],
        "inside the band, at one over its factor of 0.5"
    );
    assert_eq!(
        five_hour(OUT_OF_BAND - HOUR, OUT_OF_BAND),
        [(OUT_OF_BAND - HOUR, OUT_OF_BAND, 1.0)],
        "outside it"
    );
    assert_eq!(
        five_hour(SEPTEMBER_NOON - HOUR, SEPTEMBER_NOON + HOUR),
        [
            (SEPTEMBER_NOON - HOUR, SEPTEMBER_NOON, 1.0),
            (SEPTEMBER_NOON, SEPTEMBER_NOON + HOUR, 2.0),
        ],
        "across its start"
    );
    assert_eq!(
        five_hour(SEPTEMBER_EVENING - HOUR, SEPTEMBER_EVENING + HOUR),
        [
            (SEPTEMBER_EVENING - HOUR, SEPTEMBER_EVENING, 2.0),
            (SEPTEMBER_EVENING, SEPTEMBER_EVENING + HOUR, 1.0),
        ],
        "across its end"
    );
    // The span reaches 08:00 on Saturday in the zone, inside the band's hours of a weekday.
    assert_eq!(
        five_hour(FRIDAY_NIGHT, SATURDAY_AFTERNOON + HOUR),
        [(FRIDAY_NIGHT, SATURDAY_AFTERNOON + HOUR, 1.0)],
        "across midnight into a weekend"
    );
    // Pacific daylight time ends on 2026-11-01, so the band opens at 13:00 UTC, an hour after September's 12:00.
    assert_eq!(
        five_hour(NOVEMBER_HALF_PAST_TWELVE, NOVEMBER_HALF_PAST_SIX),
        [
            (
                NOVEMBER_HALF_PAST_TWELVE,
                NOVEMBER_HALF_PAST_TWELVE + 30 * MINUTE,
                1.0
            ),
            (
                NOVEMBER_HALF_PAST_TWELVE + 30 * MINUTE,
                NOVEMBER_HALF_PAST_SIX,
                2.0
            ),
        ],
        "on the daylight-rule day"
    );
    assert_eq!(
        segments(
            SEVEN_DAY,
            None,
            SEPTEMBER_NOON - HOUR,
            SEPTEMBER_NOON + HOUR
        ),
        [(SEPTEMBER_NOON - HOUR, SEPTEMBER_NOON + HOUR, 1.0)],
        "the 7-day window's factor is 1"
    );
    assert_eq!(
        segments(
            FIVE_HOUR,
            Some("desk"),
            SEPTEMBER_NOON - HOUR,
            SEPTEMBER_NOON + HOUR
        ),
        [(SEPTEMBER_NOON - HOUR, SEPTEMBER_NOON + HOUR, 1.0)],
        "the regime's factor is 1"
    );
    // A horizon clamped at its instant asks for an empty span, which reads the band's weight there.
    assert_eq!(
        five_hour(IN_BAND, IN_BAND),
        [(IN_BAND, IN_BAND, 2.0)],
        "an empty span in the band"
    );
    assert_eq!(
        five_hour(OUT_OF_BAND, OUT_OF_BAND),
        [(OUT_OF_BAND, OUT_OF_BAND, 1.0)],
        "an empty span outside it"
    );
}

#[test]
fn the_shared_windows_and_every_model_week_the_file_names_read_measured() {
    let tables = first();
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert!(tables.measured(window), "{window}");
    }
    for window in ["seven_day_other", "five_hour_x"] {
        assert!(!tables.measured(window), "{window}");
    }
}

#[test]
fn a_week_the_file_does_not_name_prices_the_seven_day_share_once_and_any_other_window_nothing() {
    let tables = first();
    assert_share(
        tables.share_at(
            "seven_day_other",
            None,
            MAX_20X,
            OPUS,
            input(1),
            OUT_OF_BAND,
        ),
        0.002,
        "an unnamed week",
    );
    assert_eq!(
        tables.share_at("one_hour", None, MAX_20X, OPUS, input(1), OUT_OF_BAND),
        None
    );
    assert!(!tables.priced("one_hour", None, MAX_20X, OPUS));
}

// Per token on the anchor's 5-hour window of 5: Opus 5.5 at 2 times (1, 0.1, 2, 2, 10), Haiku at 0.5 times
// (1, 0.2, 2, 2, 10) with its cache read override. Opus 5 (a null weight) and Opus 4.8 (a model tier entry alone)
// take Opus 5.5's 2 and the shared multipliers, and the Mystery model tier has no weighted model, so Mystery 2's row
// carries no rate.
#[test]
fn the_rate_table_carries_a_row_per_weighted_model_and_per_unweighted_model_of_a_weighted_tier() {
    let rates = first().rate_table(FIVE_HOUR, None);
    assert!(
        all_close(
            per_type(rates.rows[OPUS]),
            [Some(0.4), Some(0.04), Some(0.8), Some(0.8), Some(4.0)]
        ),
        "{:?}",
        rates.rows[OPUS]
    );
    assert!(
        all_close(
            per_type(rates.rows[HAIKU]),
            [Some(0.1), Some(0.02), Some(0.2), Some(0.2), Some(1.0)]
        ),
        "{:?}",
        rates.rows[HAIKU]
    );
    for model in ["claude-opus-5", "claude-opus-4-8"] {
        let row = rates
            .rows
            .get(model)
            .unwrap_or_else(|| panic!("{model}: no row"));
        assert!(
            all_close(per_type(*row), per_type(rates.rows[OPUS])),
            "{model} at Opus 5.5's weight: {row:?}"
        );
    }
    assert_eq!(
        rates.rows.get("claude-mystery-2").copied().map(per_type),
        Some([None; 5])
    );
    assert_eq!(per_type(rates.fallback), [None; 5]);
    assert_eq!(
        per_type(rates.rates_for("claude-opus-5-5-20260401", 0)),
        per_type(rates.rows[OPUS]),
        "a dated id reads its undated row"
    );
    assert!(first().rate_table("one_hour", None).rows.is_empty());
}

// An id no file lists prices through the model tier its id names: Opus 9 at the opus model tier's latest weighted
// model, Opus 5.5 at 2, with the shared multipliers, the share the meter charges the anchor tier. Mystery 9's model
// tier has no weighted model and GPT 5 names no model tier, so neither has a rate.
#[test]
fn an_unlisted_id_naming_a_weighted_tier_rates_at_its_latest_weight_as_the_meter_charges_it() {
    let costs = first();
    let rates = costs.rate_table(FIVE_HOUR, None);
    assert_eq!(rates.rows.get("claude-opus-9"), None, "no file lists it");
    let usage = PinUsage {
        input: 1_000,
        cache_read: 10_000,
        write_5m: 100,
        write_1h: 200,
        output: 500,
    };
    for model in ["claude-opus-9", "claude-opus-9-20270101"] {
        let rated = rates
            .price(model, usage, 0)
            .unwrap_or_else(|| panic!("{model}: no rate"));
        let charged = costs
            .share_at(FIVE_HOUR, None, PRO, model, usage, OUT_OF_BAND)
            .expect("the meter charges it");
        assert!(close(rated, charged), "{model}: {rated} against {charged}");
    }
    assert!(
        all_close(
            per_type(rates.rates_for("claude-opus-9", 0)),
            per_type(rates.rows[OPUS])
        ),
        "at Opus 5.5's base weight: {:?}",
        rates.rates_for("claude-opus-9", 0)
    );
    for model in ["claude-mystery-9", "gpt-5"] {
        assert_eq!(per_type(rates.rates_for(model, 0)), [None; 5], "{model}");
    }
}

// A file can list an id under a model tier other than the one its id names. Opus 7 listed under the Mystery model
// tier, which has no weighted model, is unpriced on the meter, so its rate row carries no rate where its id's own
// model tier would rate it.
#[test]
fn a_listed_id_its_model_tier_leaves_unpriced_takes_no_rate_from_the_model_tier_its_id_names() {
    let text = SUBSCRIPTION_COSTS.replace(
        "      claude-mystery-2: mystery\n",
        "      claude-mystery-2: mystery\n      claude-opus-7: mystery\n",
    );
    assert_ne!(text, SUBSCRIPTION_COSTS);
    let costs = CostRevisions::from_yaml(&text)
        .unwrap()
        .in_force(JANUARY_2026)
        .clone();
    assert_eq!(
        costs.share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            "claude-opus-7",
            input(1),
            OUT_OF_BAND
        ),
        None,
        "the meter leaves it unpriced"
    );
    let rates = costs.rate_table(FIVE_HOUR, None);
    assert_eq!(per_type(rates.rates_for("claude-opus-7", 0)), [None; 5]);
    assert_eq!(rates.price("claude-opus-7", input(1), 0), None);
}

// The test regime holds twice the base Max 20x 5-hour row and 1.25 times its week (200 and 1,250 against 100 and
// 1,000), so one Opus 5.5 input token, 2 units, reads 2 / 200 and 2 / 1,250 under it, and one Fable 5.1 input token,
// 10 units, reads 10 / 1,250 over the model week's 0.5.
#[test]
fn a_listed_entrypoint_prices_the_five_hour_share_at_half_and_the_week_at_four_fifths_of_the_base()
{
    let tables = first();
    for (window, model, regime, base) in [
        (FIVE_HOUR, OPUS, 0.01, 0.02),
        (SEVEN_DAY, OPUS, 0.0016, 0.002),
        (SEVEN_DAY_FABLE, FABLE, 0.016, 0.02),
    ] {
        for entrypoint in ["desk", "pane"] {
            assert_share(
                tables.share_at(
                    window,
                    Some(entrypoint),
                    MAX_20X,
                    model,
                    input(1),
                    OUT_OF_BAND,
                ),
                regime,
                &format!("{entrypoint} on {window}"),
            );
        }
        assert_share(
            tables.share_at(window, None, MAX_20X, model, input(1), OUT_OF_BAND),
            base,
            &format!("the base on {window}"),
        );
    }
}

// The base band holds half the 5-hour window and the test regime's holds all of it, so inside the band the base share
// doubles to 0.04 while the regime's stays at 2 / 200.
#[test]
fn the_band_moves_the_base_share_and_not_the_regimes() {
    let tables = first();
    assert_share(
        tables.share_at(FIVE_HOUR, Some("desk"), MAX_20X, OPUS, input(1), IN_BAND),
        0.01,
        "the regime in band",
    );
    assert_share(
        tables.share_at(FIVE_HOUR, None, MAX_20X, OPUS, input(1), IN_BAND),
        0.04,
        "the base in band",
    );
    assert_eq!(tables.band_factor(FIVE_HOUR, Some("desk"), IN_BAND), 1.0);
    assert_eq!(tables.band_factor(FIVE_HOUR, None, IN_BAND), 2.0);
}

// The test regime names Pro and Max 20x alone, so Max 5x reads its base 25 under it, 2 / 25. A tier factor divides by
// the regime's own anchor row: Max 20x's 200 over 10 is the base ratio of 100 over 5, and Max 5x's base 25 over 10 is
// the factor that turns the regime's anchor rate of 2 / 10 back into the 0.08.
#[test]
fn a_tier_the_regime_leaves_out_prices_at_the_base_row_under_it() {
    let tables = first();
    assert_share(
        tables.share_at(FIVE_HOUR, Some("desk"), MAX_5X, OPUS, input(1), OUT_OF_BAND),
        0.08,
        "Max 5x under the regime",
    );
    assert_share(
        tables.tier_factor(MAX_20X, FIVE_HOUR, Some("desk")),
        20.0,
        "Max 20x under the regime",
    );
    assert_share(
        tables.tier_factor(MAX_5X, FIVE_HOUR, Some("desk")),
        2.5,
        "Max 5x under the regime's anchor",
    );
}

// The equivalent is the share a Pro account spends, so under the regime it reads the regime's Pro row, 2 / 10, and
// the rate table divides by that row too.
#[test]
fn the_equivalent_and_the_rate_table_follow_the_regime() {
    let tables = first();
    assert_share(
        tables.equivalent_at(FIVE_HOUR, Some("desk"), OPUS, input(1), OUT_OF_BAND),
        0.2,
        "the regime's equivalent",
    );
    let input_rate = |entrypoint: Option<&str>| {
        tables.rate_table(FIVE_HOUR, entrypoint).rows[OPUS]
            .input
            .expect("Opus 5.5 carries a rate")
    };
    let (regime, base) = (input_rate(Some("desk")), input_rate(None));
    assert!(close(regime, base / 2.0), "{regime} against {base}");
}

#[test]
fn regime_of_names_the_regime_for_a_listed_token_and_none_otherwise() {
    let tables = first();
    let desk = &tables.regimes["desk"];
    for token in ["desk", "pane"] {
        assert_eq!(tables.regime_of(Some(token)), Some(desk), "{token}");
    }
    assert_eq!(tables.regime_of(Some("sdk-cli")), None);
    assert_eq!(tables.regime_of(None), None);
}

// The relay labels a Claude Code request `claude-code/<entrypoint>`, so that token names the row's regime, and every
// other label, the rollups' `-` among them, prices at the base.
#[test]
fn entrypoint_of_client_reads_the_claude_code_token_and_none_for_any_other_label() {
    assert_eq!(entrypoint_of_client("claude-code/desk"), Some("desk"));
    for label in ["other", "sdk/py", "-"] {
        assert_eq!(entrypoint_of_client(label), None, "{label}");
    }
}

#[test]
fn a_file_without_the_regimes_map_prices_every_entrypoint_at_the_base() {
    let text = test_tables::first_entry_costs_text();
    let (without, _) = text
        .split_once("    regimes:\n")
        .expect("the first entry carries the map");
    let costs = CostRevisions::from_yaml(without)
        .expect("a file without the map loads")
        .in_force(JANUARY_2026)
        .clone();
    assert!(costs.regimes.is_empty());
    assert_eq!(costs.regime_of(Some("desk")), None);
    assert_share(
        costs.share_at(
            FIVE_HOUR,
            Some("desk"),
            MAX_20X,
            OPUS,
            input(1),
            OUT_OF_BAND,
        ),
        0.02,
        "a listed token at the base",
    );
}

/// The test table with the first `from` replaced by `to`, refused: the `Plan` message. Both revisions carry most
/// keys, so an edit of one lands in the first and the refusal names it.
fn refused(from: &str, to: &str) -> String {
    refused_all(&[(from, to)])
}

/// The test table with each edit's first `from` replaced by its `to`, refused: the `Plan` message.
fn refused_all(edits: &[(&str, &str)]) -> String {
    let mut text = SUBSCRIPTION_COSTS.to_string();
    for (from, to) in edits {
        let edited = text.replacen(from, to, 1);
        assert_ne!(edited, text, "the edit of `{from}` lands");
        text = edited;
    }
    match CostRevisions::from_yaml(&text) {
        Err(LedgerError::Plan(message)) => message,
        Err(other) => panic!("{edits:?} refused as another error: {other}"),
        Ok(_) => panic!("{edits:?} loads"),
    }
}

fn assert_names(message: &str, key: &str) {
    assert!(
        message.contains(key),
        "the refusal names `{key}`: {message}"
    );
}

#[test]
fn the_test_table_loads() {
    let revisions = CostRevisions::from_yaml(SUBSCRIPTION_COSTS).expect("the test table parses");
    let dates: Vec<&str> = revisions.iter().map(|r| r.effective.as_str()).collect();
    assert_eq!(dates, ["2026-01-01", "2026-02-01"]);
}

#[test]
fn an_unknown_key_or_a_missing_one_is_refused_by_name() {
    assert_names(
        &refused(
            "effective: 2026-01-01\n",
            "effective: 2026-01-01\n    tiers: {}\n",
        ),
        "unknown field `tiers`",
    );
    let second_key = CostRevisions::from_yaml(&format!("{SUBSCRIPTION_COSTS}source: x\n"))
        .unwrap_err()
        .to_string();
    assert_names(&second_key, "unknown field `source`");
    assert_names(
        &refused("{five_hour: 5, seven_day: 50}", "{five_hour: 5}"),
        "missing field `seven_day`",
    );
    assert_names(
        &refused(
            "cache_write_1h: 2, cache_write_5m: 2,",
            "cache_write_1h: 2,",
        ),
        "missing field `cache_write_5m`",
    );
    assert_names(
        &refused("{cache_read: 0.2}", "{cache_reads: 0.2}"),
        "unknown field `cache_reads`",
    );
}

#[test]
fn a_malformed_regime_is_refused_by_key() {
    let bare = "a bare token of lowercase letters, digits, hyphens and underscores";
    assert_names(&refused("desk:", "Desk:"), &format!("regimes.Desk: {bare}"));
    // A separator inside the key.
    assert_names(
        &refused("desk:", "desk top:"),
        &format!("regimes.desk top: {bare}"),
    );
    let long = "a".repeat(257);
    for member in ["Pane", "pane=1", "pane.1", "", long.as_str()] {
        let list = if member.is_empty() {
            "[desk, \"\"]".to_string()
        } else {
            format!("[desk, {member}]")
        };
        assert_names(
            &refused("[desk, pane]", &list),
            &format!("regimes.desk.entrypoints: {member} is not {bare}"),
        );
    }
    assert_names(
        &refused("[desk, pane]", "[]"),
        "regimes.desk.entrypoints: lists none",
    );
    let peak = "          capacity_factor: {five_hour: 1, seven_day: 1}\n";
    assert_names(
        &refused(
            peak,
            &format!(
                "{peak}      lap:\n        entrypoints: [desk]\n        capacity:\n          \
                 default_claude_ai: {{five_hour: 10, seven_day: 62.5}}\n        peak:\n{peak}"
            ),
        ),
        "regimes.lap.entrypoints: desk is listed by desk",
    );
    let pro = "          default_claude_ai:      {five_hour: 10, seven_day: 62.5}\n";
    let max = "          default_claude_max_20x: {five_hour: 200, seven_day: 1250}\n";
    assert_names(
        &refused(
            &format!("        capacity:\n{pro}{max}"),
            "        capacity: {}\n",
        ),
        "regimes.desk: names no tier",
    );
    assert_names(
        &refused(
            max,
            &format!("{max}          default_claude_pro: {{five_hour: 10, seven_day: 62.5}}\n"),
        ),
        "regimes.desk.capacity.default_claude_pro: no capacity row",
    );
    assert_names(
        &refused("five_hour: 200", "five_hour: 0"),
        "regimes.desk.capacity.default_claude_max_20x.five_hour: a capacity is a number above zero, or null",
    );
    assert_names(
        &refused("seven_day: 62.5", "seven_day: null"),
        "regimes.desk.capacity.default_claude_ai.seven_day: the anchor tier carries a capacity on both windows",
    );
    assert_names(
        &refused("{five_hour: 1, seven_day: 1}", "{five_hour: 0, seven_day: 1}"),
        "regimes.desk.peak.capacity_factor.five_hour: a factor is a number above zero and at most 1",
    );
}

// The shipped interactive regime lists `claude-desktop-3p`, so the alphabet carries the digits, and the client spells
// some entrypoints with an underscore (`local_agent`), so it carries that too.
#[test]
fn a_token_with_a_digit_or_an_underscore_is_a_bare_token() {
    for member in ["pane-3p", "pane_2"] {
        let text = SUBSCRIPTION_COSTS.replacen("desk:", "desk-2:", 1).replacen(
            "[desk, pane]",
            &format!("[desk, {member}]"),
            1,
        );
        let costs = CostRevisions::from_yaml(&text)
            .unwrap_or_else(|e| panic!("{member} loads: {e}"))
            .in_force(JANUARY_2026)
            .clone();
        assert_eq!(
            costs.regime_of(Some(member)),
            Some(&costs.regimes["desk-2"]),
            "{member}"
        );
    }
}

#[test]
fn a_peak_zone_the_bundled_database_lacks_is_refused() {
    assert_names(
        &refused("zone: America/Los_Angeles", "zone: Mars/Olympus"),
        "peak zone `Mars/Olympus`",
    );
}

#[test]
fn a_capacity_at_or_below_zero_or_not_finite_is_refused_and_a_null_one_loads() {
    for bad in ["0", "-5", ".inf"] {
        assert_names(
            &refused(
                "default_claude_ai:      {five_hour: 5,",
                &format!("default_claude_ai:      {{five_hour: {bad},"),
            ),
            "capacity.default_claude_ai.five_hour: a capacity is a number above zero, or null",
        );
    }
    let text = SUBSCRIPTION_COSTS.replacen("{five_hour: 25,", "{five_hour: null,", 1);
    assert_ne!(text, SUBSCRIPTION_COSTS);
    CostRevisions::from_yaml(&text).expect("a null capacity loads");
}

#[test]
fn an_anchor_tier_without_a_capacity_on_either_window_is_refused() {
    assert_names(
        &refused(
            "{five_hour: 5, seven_day: 50}",
            "{five_hour: 5, seven_day: null}",
        ),
        "capacity.default_claude_ai.seven_day: the anchor tier carries a capacity on both windows",
    );
    assert_names(
        &refused(
            "{five_hour: 5, seven_day: 50}",
            "{five_hour: null, seven_day: 50}",
        ),
        "capacity.default_claude_ai.five_hour",
    );
}

/// The second revision's head through its anchor row, reached from the split marker since `refused_all` edits the
/// first occurrence and the first revision carries the same row.
const SECOND_REVISION_ANCHOR_ROW: &str =
    "  - effective: 2026-02-01\n    capacity:\n      default_claude_ai:      {five_hour: 5, seven_day: 50}\n";

#[test]
fn a_unit_inside_a_revision_is_refused_as_an_unknown_field() {
    assert_names(
        &refused(
            "unit: {tier: default_claude_ai, window: seven_day}\nrevisions:\n  - effective: 2026-01-01\n",
            "revisions:\n  - effective: 2026-01-01\n    unit: {tier: default_claude_ai, window: seven_day}\n",
        ),
        "unknown field `unit`",
    );
}

#[test]
fn a_file_without_the_unit_block_is_refused() {
    let message = refused("unit: {tier: default_claude_ai, window: seven_day}\n", "");
    assert_names(&message, "missing field `unit`");
    assert!(
        !message.contains("revisions[0]"),
        "the refusal names the file's own key: {message}"
    );
}

// `capacity_rules` passes an anchor tier with no row at all, so the unit's own check refuses the dropped row.
#[test]
fn a_revision_whose_anchor_tier_lacks_the_units_window_is_refused_naming_the_revision() {
    assert_names(
        &refused(
            SECOND_REVISION_ANCHOR_ROW,
            "  - effective: 2026-02-01\n    capacity:\n",
        ),
        "revision 2026-02-01: capacity.default_claude_ai.seven_day: the anchor tier carries a capacity on the unit's \
         window",
    );
}

// The unit's size is the anchor row of the revision in force, so nothing refuses a later revision that resizes it.
#[test]
fn a_revision_that_resizes_the_anchor_row_loads() {
    let resized = SECOND_REVISION_ANCHOR_ROW.replace("seven_day: 50}", "seven_day: 100}");
    let text = SUBSCRIPTION_COSTS.replacen(SECOND_REVISION_ANCHOR_ROW, &resized, 1);
    assert_ne!(text, SUBSCRIPTION_COSTS, "the edit lands");
    let revisions = CostRevisions::from_yaml(&text).expect("a resized anchor row loads");
    let dates: Vec<&str> = revisions.iter().map(|r| r.effective.as_str()).collect();
    assert_eq!(dates, ["2026-01-01", "2026-02-01"]);
    assert_eq!(revisions.latest().capacity[PRO].seven_day, Some(100.0));
}

#[test]
fn a_unit_naming_a_tier_the_file_lacks_is_refused() {
    assert_names(
        &refused("{tier: default_claude_ai,", "{tier: default_claude_max_1x,"),
        "revision 2026-01-01: capacity.default_claude_max_1x.seven_day: the anchor tier carries a capacity on the \
         unit's window",
    );
}

#[test]
fn a_unit_naming_a_window_other_than_the_two_is_refused() {
    assert_eq!(
        refused("window: seven_day}", "window: seven_day_fable}"),
        "unit.window: `seven_day_fable` is neither five_hour nor seven_day",
        "a file-level refusal, under no revision's date"
    );
}

#[test]
fn an_hour_past_twenty_four_is_refused() {
    assert_names(
        &refused("to_hour: 11", "to_hour: 25"),
        "peak.to_hour: an hour is 0 to 24",
    );
    assert_names(
        &refused("from_hour: 5", "from_hour: 25"),
        "peak.from_hour: an hour is 0 to 24",
    );
}

#[test]
fn a_band_whose_first_hour_is_not_below_its_last_is_refused() {
    assert_names(
        &refused("from_hour: 5", "from_hour: 11"),
        "peak.from_hour: below to_hour",
    );
}

#[test]
fn a_capacity_factor_outside_zero_to_one_is_refused() {
    for bad in ["0", "1.5", "-0.5"] {
        assert_names(
            &refused(
                "capacity_factor: {five_hour: 0.5,",
                &format!("capacity_factor: {{five_hour: {bad},"),
            ),
            "peak.capacity_factor.five_hour: a factor is a number above zero and at most 1",
        );
    }
}

#[test]
fn a_multiplier_below_zero_or_not_finite_is_refused() {
    for bad in ["-1", ".nan"] {
        assert_names(
            &refused("output: 10}", &format!("output: {bad}}}")),
            "type_multiplier.output: a multiplier is a number at or above zero",
        );
    }
    let text = SUBSCRIPTION_COSTS.replacen("output: 10}", "output: 0}", 1);
    assert_ne!(text, SUBSCRIPTION_COSTS);
    CostRevisions::from_yaml(&text).expect("a multiplier of zero loads");
}

/// A map that kept the last of two rows would load a model the file does not say.
#[test]
fn a_repeated_model_weight_key_is_refused_by_name() {
    let message = refused(
        "      claude-opus-5-5: 2\n",
        "      claude-opus-5-5: 2\n      claude-opus-5-5: 3\n",
    );
    assert_names(&message, "duplicate");
    assert_names(&message, "claude-opus-5-5");
}

/// The typed map reads a tagged key as its plain text, so a tagged repeat would load as the model's last row.
#[test]
fn a_tagged_repeat_of_a_model_weight_key_is_refused_as_a_repeat() {
    let message = refused(
        "      claude-opus-5-5: 2\n",
        "      claude-opus-5-5: 2\n      !weight claude-opus-5-5: null\n",
    );
    assert_names(&message, "duplicate");
    assert_names(&message, "revisions[0].model_weight");
    assert_names(&message, "claude-opus-5-5");
}

#[test]
fn a_tagged_model_weight_key_is_refused_as_not_an_untagged_string() {
    let message = refused(
        "      claude-opus-5-5: 2\n",
        "      claude-opus-5-5: 2\n      !weight claude-opus-4: 1\n",
    );
    assert_eq!(
        message,
        "revisions[0].model_weight.!weight claude-opus-4: a key is an untagged string",
    );
}

#[test]
fn a_dated_model_weight_key_is_refused() {
    assert_names(
        &refused("claude-opus-5-5: 2", "claude-opus-5-5-20260401: 2"),
        "revision 2026-01-01: model_weight.claude-opus-5-5-20260401: a dated id, where the file keys undated ids",
    );
}

#[test]
fn a_weight_at_or_below_zero_is_refused_and_a_null_one_loads() {
    for bad in ["0", "-2", ".inf"] {
        assert_names(
            &refused("claude-opus-5-5: 2", &format!("claude-opus-5-5: {bad}")),
            "model_weight.claude-opus-5-5: a weight is a number above zero, or null",
        );
    }
}

#[test]
fn a_model_window_whose_key_is_no_week_is_refused() {
    assert_names(
        &refused("seven_day_fable: {of", "five_hour_x: {of"),
        "model_windows.five_hour_x: a model week starts with seven_day_",
    );
}

#[test]
fn a_model_window_sized_against_another_window_than_the_week_is_refused() {
    assert_names(
        &refused(
            "seven_day_fable: {of: seven_day,",
            "seven_day_x: {of: five_hour,",
        ),
        "model_windows.seven_day_x.of: seven_day",
    );
}

#[test]
fn a_model_window_capacity_factor_outside_zero_to_one_is_refused() {
    for bad in ["0", "1.5"] {
        assert_names(
            &refused(
                "seven_day_fable: {of: seven_day, capacity_factor: 0.5}",
                &format!("seven_day_x: {{of: seven_day, capacity_factor: {bad}}}"),
            ),
            "model_windows.seven_day_x.capacity_factor: a factor is a number above zero and at most 1",
        );
    }
}

#[test]
fn an_override_for_a_model_without_a_weight_row_is_refused() {
    assert_names(
        &refused(
            "claude-haiku-4-5: {type_multiplier",
            "claude-x: {type_multiplier",
        ),
        "overrides.claude-x: no model_weight row",
    );
}

#[test]
fn a_dated_override_key_is_refused() {
    assert_names(
        &refused(
            "claude-haiku-4-5: {type_multiplier",
            "claude-haiku-4-5-20251001: {type_multiplier",
        ),
        "overrides.claude-haiku-4-5-20251001: a dated id, where the file keys undated ids",
    );
}

#[test]
fn an_override_weight_on_a_window_neither_shared_nor_a_model_week_is_refused() {
    assert_names(
        &refused("weight_by_window: {seven_day: 1}", "weight_by_window: {sometime: 1}"),
        "overrides.claude-haiku-4-5.weight_by_window.sometime: neither a shared window nor a model_windows key",
    );
    let text = SUBSCRIPTION_COSTS.replacen(
        "weight_by_window: {seven_day: 1}",
        "weight_by_window: {seven_day_fable: 1}",
        1,
    );
    assert_ne!(text, SUBSCRIPTION_COSTS);
    CostRevisions::from_yaml(&text).expect("a model week's key loads");
}

#[test]
fn an_override_weight_or_multiplier_failing_the_shared_rule_is_refused() {
    assert_names(
        &refused(
            "weight_by_window: {seven_day: 1}",
            "weight_by_window: {seven_day: 0}",
        ),
        "overrides.claude-haiku-4-5.weight_by_window.seven_day: a weight is a number above zero",
    );
    assert_names(
        &refused("{cache_read: 0.2}", "{cache_read: -0.2}"),
        "overrides.claude-haiku-4-5.type_multiplier.cache_read: a multiplier is a number at or above zero",
    );
}

#[test]
fn an_empty_or_repeated_tier_order_is_refused() {
    for order in ["[]", "[fable, fable]"] {
        assert_eq!(
            refused(
                "tier_order: [fable, opus, sonnet, haiku, mystery]",
                &format!("tier_order: {order}"),
            ),
            "revision 2026-01-01: tier_order: a list of distinct tier names",
            "{order}"
        );
    }
}

#[test]
fn a_model_tier_outside_the_tier_order_is_refused() {
    assert_eq!(
        refused_all(&[
            (
                "tier_order: [fable, opus, sonnet, haiku, mystery]",
                "tier_order: [opus, sonnet, haiku, mystery]"
            ),
            ("claude-fable-5-1: fable", "claude-fable-5-1: opus"),
            ("claude-opus-5-5: opus", "claude-opus-5-5: fable"),
        ]),
        "revision 2026-01-01: model_tier.claude-opus-5-5: fable is not in tier_order"
    );
}

#[test]
fn a_dated_model_tier_key_is_refused() {
    assert_eq!(
        refused(
            "claude-mystery-2: mystery",
            "claude-mystery-2-20260401: mystery"
        ),
        "revision 2026-01-01: model_tier.claude-mystery-2-20260401: a dated id, where the file keys undated ids"
    );
}

/// The fallback never guesses a tier for a model the file lists.
#[test]
fn a_weighted_or_overridden_model_without_a_tier_is_refused() {
    assert_eq!(
        refused("      claude-opus-5-5: opus\n", ""),
        "revision 2026-01-01: model_tier: no tier for claude-opus-5-5"
    );
    assert_eq!(
        refused("      claude-haiku-4-5: haiku\n", ""),
        "revision 2026-01-01: model_tier: no tier for claude-haiku-4-5",
        "the override's model"
    );
}

#[test]
fn the_operators_file_wins_and_the_handle_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subscription_costs.yaml");
    std::fs::write(&path, SUBSCRIPTION_COSTS).unwrap();
    let loaded = PlanTables::load(Some(&path)).unwrap();
    assert_eq!(loaded.source, "file");
    assert_eq!(loaded.path.as_deref(), Some(path.as_path()));
    assert_eq!(
        loaded.revisions,
        test_tables::subscription_costs().revisions
    );
}

#[test]
fn a_path_with_nothing_at_it_reads_the_shipped_model_and_keeps_the_configured_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.yaml");
    let loaded = PlanTables::load(Some(&path)).unwrap();
    assert_eq!(loaded.source, "shipped");
    assert_eq!(loaded.path.as_deref(), Some(path.as_path()));
    assert_eq!(loaded.revisions, CostRevisions::shipped());
}

#[test]
fn no_path_reads_the_shipped_model_with_no_path() {
    let loaded = PlanTables::load(None).unwrap();
    assert_eq!(loaded.source, "shipped");
    assert_eq!(loaded.path, None);
    assert_eq!(loaded, PlanTables::shipped());
}

#[test]
fn a_malformed_operators_file_refuses_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subscription_costs.yaml");
    std::fs::write(
        &path,
        SUBSCRIPTION_COSTS.replacen("output: 10}", "output: -1}", 1),
    )
    .unwrap();
    match PlanTables::load(Some(&path)) {
        Err(LedgerError::Plan(message)) => assert_names(&message, "type_multiplier.output"),
        other => panic!("a malformed file loads as {other:?}"),
    }
}

/// A directory at the path exists and cannot be read as a file, so the load refuses rather than falling back.
#[test]
fn a_path_that_cannot_be_read_refuses_the_load_rather_than_reading_the_shipped_model() {
    let dir = tempfile::tempdir().unwrap();
    match PlanTables::load(Some(dir.path())) {
        Err(LedgerError::Io(_)) => {}
        other => panic!("an unreadable path loads as {other:?}"),
    }
}

#[test]
fn the_shipped_model_parses_and_holds_the_shape() {
    let revisions = CostRevisions::shipped();
    assert_eq!(PlanTables::shipped().source, "shipped");
    assert!(revisions.len() >= 1);
    for costs in revisions.iter() {
        assert!(!costs.capacity.is_empty());
        for (tier, row) in &costs.capacity {
            for (window, capacity) in [(FIVE_HOUR, row.five_hour), (SEVEN_DAY, row.seven_day)] {
                assert!(
                    capacity.is_none_or(|c| c.is_finite() && c > 0.0),
                    "{tier} {window}: {capacity:?}"
                );
            }
        }
        let anchor = &costs.capacity[&costs.unit.tier];
        assert!(anchor.five_hour.is_some() && anchor.seven_day.is_some());
        assert!(!costs.model_weight.is_empty());
        for (model, weight) in &costs.model_weight {
            assert!(
                weight.is_none_or(|w| w.is_finite() && w > 0.0),
                "{model}: {weight:?}"
            );
        }
        assert!(!costs.tier_order.is_empty());
        for model in costs.model_weight.keys().chain(costs.overrides.keys()) {
            assert!(costs.model_weight.contains_key(model), "{model}");
            let tier = costs.tier_of(model);
            assert!(
                tier.is_some_and(|tier| costs.tier_order.iter().any(|t| t == tier)),
                "{model}: {tier:?}"
            );
        }
        let keys: Vec<&str> = costs.model_weight.keys().map(String::as_str).collect();
        assert_eq!(
            costs.model_order(&keys),
            keys,
            "the weights are in the model order"
        );
    }
    assert!(
        !revisions.latest().regimes.is_empty(),
        "the shipped model names a regime"
    );
}

#[test]
fn the_model_serializes_back_to_the_files_keys_in_the_files_order() {
    let revisions = test_tables::subscription_costs().revisions;
    let value = serde_json::to_value(&revisions).unwrap();
    let entries = value["revisions"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert_eq!(
            entry.as_object().unwrap().keys().next().map(String::as_str),
            Some("effective"),
            "each entry leads with its date"
        );
    }
    let value = &entries[0];
    let models: Vec<&str> = value["model_weight"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        models,
        [
            "claude-fable-5-1",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5-5",
            HAIKU
        ]
    );
    assert_eq!(
        value["model_weight"]["claude-opus-5"],
        serde_json::Value::Null
    );
    assert_eq!(
        value["overrides"][HAIKU]["type_multiplier"],
        serde_json::json!({"cache_read": 0.2}),
        "an override written with one multiplier answers one"
    );
    assert_eq!(
        value["tier_order"],
        serde_json::json!(["fable", "opus", "sonnet", "haiku", "mystery"])
    );
    let reread = CostRevisions::from_yaml(&serde_norway::to_string(&revisions).unwrap()).unwrap();
    assert_eq!(reread, revisions);
}

#[test]
fn the_funding_reads_the_account_kind_before_the_speed_and_the_overage_flag() {
    assert_eq!(
        funding_of("api_key", false, Some(true), Some("fast")),
        Funding::ApiKey,
        "an api_key attempt is billed in dollars whatever its speed or overage flag"
    );
    assert_eq!(
        Funding::of_rollup(BILLED_FUNDING),
        Funding::ApiKey,
        "an api_key account's rollup row carries its billing in its key's word"
    );
    assert_eq!(
        Funding::of_rollup(FAST_FUNDING),
        Funding::Fast,
        "a fast row draws usage credits"
    );
    assert_eq!(
        Funding::ApiKey.rollup_word(),
        BILLED_FUNDING,
        "and the word of an api_key attempt's funding is the billed one"
    );
}
