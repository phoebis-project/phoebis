//! The subscription cost model: a token costs a weight times a multiplier in units, a window holds a capacity per
//! tier, and the peak band is a capacity factor per window. Every expected figure is worked out by hand from the
//! round numbers of the test table (`tests/fixtures/subscription_costs.yaml`); the shipped file is held to its shape.

use std::cmp::Reverse;

use phoebis_ledger::test_tables::{self, SUBSCRIPTION_COSTS};
use phoebis_ledger::{LedgerError, PlanTables, SubscriptionCosts};
use phoebis_routing::{PinUsage, Rates, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};

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

/// Inside the band and outside it, on the same weekday.
const IN_BAND: i64 = SEPTEMBER_AFTERNOON;
const OUT_OF_BAND: i64 = SEPTEMBER_BEFORE_NOON;

const MAX_20X: &str = "default_claude_max_20x";
const MAX_5X: &str = "default_claude_max_5x";
const PRO: &str = "default_claude_ai";
const TEAM: &str = "default_claude_team";
const OPUS: &str = "claude-opus-5-5";
const HAIKU: &str = "claude-haiku-4-5";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

fn assert_share(got: Option<f64>, want: f64, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: no share, want {want}"));
    assert!(close(got, want), "{what}: {got} against {want}");
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

fn tables() -> PlanTables {
    test_tables::subscription_costs()
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
    let tables = tables();
    assert_share(
        tables.share_at(FIVE_HOUR, MAX_20X, OPUS, input(1), OUT_OF_BAND),
        0.02,
        "out of band",
    );
    assert_share(
        tables.share_at(FIVE_HOUR, MAX_20X, OPUS, input(1), IN_BAND),
        0.04,
        "in band",
    );
}

// The week holds 1000 units on Max 20x and its capacity factor is 1, so the band leaves it at 2 / 1000.
#[test]
fn the_week_prices_on_its_own_capacity_and_its_factor_of_one_leaves_it_unbanded() {
    let tables = tables();
    for at in [IN_BAND, OUT_OF_BAND] {
        assert_share(
            tables.share_at(SEVEN_DAY, MAX_20X, OPUS, input(1), at),
            0.002,
            &format!("the week at {at}"),
        );
    }
}

// Pro holds 5 units on the 5-hour window: 2 / 5.
#[test]
fn a_pro_account_spends_the_same_units_over_its_own_smaller_capacity() {
    assert_share(
        tables().share_at(FIVE_HOUR, PRO, OPUS, input(1), OUT_OF_BAND),
        0.4,
        "Pro",
    );
}

// The Fable week is the 7-day window at a capacity factor of 0.5: 2 / 1000 / 0.5.
#[test]
fn a_model_week_reads_the_seven_day_share_over_its_capacity_factor() {
    assert_share(
        tables().share_at(SEVEN_DAY_FABLE, MAX_20X, OPUS, input(1), OUT_OF_BAND),
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
        tables().share_at(FIVE_HOUR, MAX_20X, OPUS, one_of_each, OUT_OF_BAND),
        0.302,
        "one token of each type",
    );
}

// Haiku weighs 0.5, and its override sets the cache read multiplier to 0.2 and its weight on `seven_day` to 1.
#[test]
fn an_override_replaces_its_multiplier_everywhere_and_its_weight_on_the_window_it_names_alone() {
    let tables = tables();
    assert_share(
        tables.share_at(SEVEN_DAY, MAX_20X, HAIKU, cache_read(1), OUT_OF_BAND),
        0.0002,
        "one cache read on the week: weight 1 times 0.2 over 1000",
    );
    assert_share(
        tables.share_at(FIVE_HOUR, MAX_20X, HAIKU, input(1), OUT_OF_BAND),
        0.005,
        "the 5-hour window keeps the weight 0.5: 0.5 / 100",
    );
    assert_share(
        tables.share_at(SEVEN_DAY_FABLE, MAX_20X, HAIKU, input(1), OUT_OF_BAND),
        0.001,
        "the override names `seven_day` alone, so the Fable week keeps 0.5: 0.5 / 1000 / 0.5",
    );
    assert_share(
        tables.share_at(
            SEVEN_DAY,
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
        tables().share_at(
            FIVE_HOUR,
            MAX_20X,
            "claude-opus-5-5-20260401",
            input(1),
            OUT_OF_BAND,
        ),
        0.02,
        "a dated Opus 5.5",
    );
}

#[test]
fn a_model_with_a_null_weight_or_no_row_prices_nothing() {
    let tables = tables();
    for model in ["claude-opus-5", "claude-opus-9"] {
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            assert_eq!(
                tables.share_at(window, MAX_20X, model, input(1), OUT_OF_BAND),
                None,
                "{model} on {window}"
            );
            assert!(
                !tables.priced(window, MAX_20X, model),
                "{model} on {window}"
            );
        }
    }
    assert!(tables.priced(FIVE_HOUR, MAX_20X, OPUS));
}

// Team holds 10 units on the 5-hour window and none on the week.
#[test]
fn a_tier_the_file_lacks_or_a_null_capacity_prices_nothing_on_that_window() {
    let tables = tables();
    assert_eq!(
        tables.share_at(FIVE_HOUR, "claude_enterprise", OPUS, input(1), OUT_OF_BAND),
        None
    );
    assert!(!tables.priced(FIVE_HOUR, "claude_enterprise", OPUS));
    for window in [SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert_eq!(
            tables.share_at(window, TEAM, OPUS, input(1), OUT_OF_BAND),
            None,
            "Team on {window}"
        );
        assert!(!tables.priced(window, TEAM, OPUS), "Team on {window}");
    }
    assert_share(
        tables.share_at(FIVE_HOUR, TEAM, OPUS, input(1), OUT_OF_BAND),
        0.2,
        "Team on the 5-hour window: 2 / 10",
    );
    assert!(tables.priced(FIVE_HOUR, TEAM, OPUS));
}

// Pro holds 5 of the anchor's 100 on the 5-hour window, Max 5x 250 of its 1000 on the week.
#[test]
fn the_tier_factor_is_the_capacity_over_the_anchors_and_reads_a_tier_with_or_without_its_prefix() {
    let tables = tables();
    assert_share(
        tables.tier_factor("claude_ai", FIVE_HOUR),
        0.05,
        "without the prefix",
    );
    assert_share(tables.tier_factor(PRO, FIVE_HOUR), 0.05, "as reported");
    assert_share(tables.tier_factor(MAX_5X, SEVEN_DAY), 0.25, "Max 5x week");
    assert_share(
        tables.tier_factor(MAX_5X, SEVEN_DAY_FABLE),
        0.25,
        "a model week reads the week's ratio",
    );
    assert_eq!(tables.tier_factor(TEAM, SEVEN_DAY), None);
    assert_eq!(tables.tier_factor(MAX_20X, "five_hour_x"), None);
    assert_eq!(tables.tier_factor("claude_enterprise", FIVE_HOUR), None);
}

// The 5-hour capacity factor is 0.5 inside the band, so the band weighs a 5-hour charge 2; the week's is 1.
#[test]
fn the_band_factor_is_the_reciprocal_of_the_windows_capacity_factor_inside_the_band() {
    let tables = tables();
    assert_eq!(tables.band_factor(FIVE_HOUR, IN_BAND), 2.0);
    assert_eq!(tables.band_factor(FIVE_HOUR, OUT_OF_BAND), 1.0);
    assert_eq!(tables.band_factor(SEVEN_DAY, IN_BAND), 1.0);
    assert_eq!(tables.band_factor(SEVEN_DAY_FABLE, IN_BAND), 1.0);
    assert_eq!(tables.band_factor("x", IN_BAND), 1.0);
}

#[test]
fn the_band_covers_weekday_hours_in_its_zone_and_follows_the_zones_daylight_rule() {
    let tables = tables();
    let factor = |at: i64| tables.band_factor(FIVE_HOUR, at);
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
        tables.share_at(FIVE_HOUR, MAX_20X, OPUS, input(1), SATURDAY_AFTERNOON),
        0.02,
        "a weekend charge is the plain share",
    );
}

#[test]
fn the_shared_windows_and_every_model_week_the_file_names_read_measured() {
    let tables = tables();
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert!(tables.measured(window), "{window}");
    }
    for window in ["seven_day_other", "five_hour_x"] {
        assert!(!tables.measured(window), "{window}");
    }
}

#[test]
fn a_week_the_file_does_not_name_prices_the_seven_day_share_once_and_any_other_window_nothing() {
    let tables = tables();
    assert_share(
        tables.share_at("seven_day_other", MAX_20X, OPUS, input(1), OUT_OF_BAND),
        0.002,
        "an unnamed week",
    );
    assert_eq!(
        tables.share_at("one_hour", MAX_20X, OPUS, input(1), OUT_OF_BAND),
        None
    );
    assert!(!tables.priced("one_hour", MAX_20X, OPUS));
}

// Per token on the anchor's 5-hour window of 100: Opus 5.5 at 2 times (1, 0.1, 2, 2, 10), Haiku at 0.5 times
// (1, 0.2, 2, 2, 10) with its cache read override.
#[test]
fn the_rate_table_carries_a_row_per_weighted_model_on_the_anchor_and_no_rate_for_any_other() {
    let rates = tables().rate_table(FIVE_HOUR);
    assert!(
        all_close(
            per_type(rates.rows[OPUS]),
            [Some(0.02), Some(0.002), Some(0.04), Some(0.04), Some(0.2)]
        ),
        "{:?}",
        rates.rows[OPUS]
    );
    assert!(
        all_close(
            per_type(rates.rows[HAIKU]),
            [Some(0.005), Some(0.001), Some(0.01), Some(0.01), Some(0.05)]
        ),
        "{:?}",
        rates.rows[HAIKU]
    );
    assert_eq!(rates.rows.get("claude-opus-5"), None);
    assert_eq!(per_type(rates.fallback), [None; 5]);
    assert_eq!(
        per_type(rates.rates_for("claude-opus-5-5-20260401")),
        per_type(rates.rows[OPUS]),
        "a dated id reads its undated row"
    );
    assert!(tables().rate_table("one_hour").rows.is_empty());
}

/// The test table with `from` replaced by `to`, refused: the `Plan` message.
fn refused(from: &str, to: &str) -> String {
    let text = SUBSCRIPTION_COSTS.replacen(from, to, 1);
    assert_ne!(text, SUBSCRIPTION_COSTS, "the edit of `{from}` lands");
    match SubscriptionCosts::from_yaml(&text) {
        Err(LedgerError::Plan(message)) => message,
        Err(other) => panic!("`{to}` refused as another error: {other}"),
        Ok(_) => panic!("`{to}` loads"),
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
    SubscriptionCosts::from_yaml(SUBSCRIPTION_COSTS).expect("the test table parses");
}

#[test]
fn an_unknown_key_or_a_missing_one_is_refused_by_name() {
    assert_names(
        &refused(
            "effective: 2026-01-01\n",
            "effective: 2026-01-01\ntiers: {}\n",
        ),
        "unknown field `tiers`",
    );
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
    SubscriptionCosts::from_yaml(&text).expect("a null capacity loads");
}

#[test]
fn an_anchor_tier_without_a_capacity_on_either_window_is_refused() {
    assert_names(
        &refused(
            "{five_hour: 100, seven_day: 1000}",
            "{five_hour: 100, seven_day: null}",
        ),
        "capacity.default_claude_max_20x.seven_day: the anchor tier carries a capacity on both windows",
    );
    assert_names(
        &refused(
            "{five_hour: 100, seven_day: 1000}",
            "{five_hour: null, seven_day: 1000}",
        ),
        "capacity.default_claude_max_20x.five_hour",
    );
}

#[test]
fn a_unit_of_no_units_is_refused() {
    assert_names(
        &refused("units_per_window: 100", "units_per_window: 0"),
        "unit.units_per_window",
    );
}

#[test]
fn a_unit_naming_a_tier_the_file_lacks_is_refused() {
    assert_names(
        &refused(
            "{tier: default_claude_max_20x,",
            "{tier: default_claude_max_1x,",
        ),
        "unit.defined_by.tier",
    );
}

#[test]
fn a_unit_naming_a_window_other_than_the_two_is_refused() {
    assert_names(
        &refused("window: five_hour, band", "window: seven_day_fable, band"),
        "unit.defined_by.window",
    );
}

#[test]
fn a_unit_naming_a_band_other_than_out_of_band_is_refused() {
    assert_names(
        &refused("band: out_of_band", "band: in_band"),
        "unit.defined_by.band",
    );
}

#[test]
fn a_unit_whose_count_differs_from_the_capacity_it_names_is_refused() {
    assert_names(
        &refused("units_per_window: 100", "units_per_window: 90"),
        "unit.units_per_window: 90 differs from capacity.default_claude_max_20x.five_hour",
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
    SubscriptionCosts::from_yaml(&text).expect("a multiplier of zero loads");
}

/// A map that kept the last of two rows would load a model the file does not say.
#[test]
fn a_repeated_model_weight_key_is_refused_by_name() {
    let message = refused(
        "  claude-opus-5-5: 2\n",
        "  claude-opus-5-5: 2\n  claude-opus-5-5: 3\n",
    );
    assert_names(&message, "duplicate");
    assert_names(&message, "claude-opus-5-5");
}

#[test]
fn a_dated_model_weight_key_is_refused() {
    assert_names(
        &refused("claude-opus-5-5: 2", "claude-opus-5-5-20260401: 2"),
        "model_weight.claude-opus-5-5-20260401: a dated id; the file keys undated ids",
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
        "overrides.claude-haiku-4-5-20251001: a dated id; the file keys undated ids",
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
    SubscriptionCosts::from_yaml(&text).expect("a model week's key loads");
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
fn the_operators_file_wins_and_the_handle_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subscription_costs.yaml");
    std::fs::write(&path, SUBSCRIPTION_COSTS).unwrap();
    let loaded = PlanTables::load(Some(&path)).unwrap();
    assert_eq!(loaded.source, "file");
    assert_eq!(loaded.path.as_deref(), Some(path.as_path()));
    assert_eq!(loaded.costs, test_tables::subscription_costs().costs);
}

#[test]
fn a_path_with_nothing_at_it_reads_the_shipped_model_and_keeps_the_configured_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.yaml");
    let loaded = PlanTables::load(Some(&path)).unwrap();
    assert_eq!(loaded.source, "shipped");
    assert_eq!(loaded.path.as_deref(), Some(path.as_path()));
    assert_eq!(loaded.costs, SubscriptionCosts::shipped());
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

/// Fable, Opus, Sonnet, Haiku, then anything else; within a family the newer version first.
fn family_order(model: &str) -> (usize, Reverse<Vec<u32>>) {
    let rest = model.strip_prefix("claude-").unwrap_or(model);
    let (family, version) = rest.split_once('-').unwrap_or((rest, ""));
    let rank = ["fable", "opus", "sonnet", "haiku"]
        .iter()
        .position(|f| *f == family)
        .unwrap_or(4);
    let version = version.split('-').filter_map(|p| p.parse().ok()).collect();
    (rank, Reverse(version))
}

#[test]
fn the_family_order_ranks_fable_opus_sonnet_haiku_and_the_newer_version_first() {
    let mut models = vec![
        "claude-haiku-4-5",
        "claude-opus-4-8",
        "claude-sonnet-5-5",
        "claude-fable-5",
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-opus-5-5",
    ];
    models.sort_by_key(|m| family_order(m));
    assert_eq!(
        models,
        [
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-sonnet-5-5",
            "claude-haiku-4-5",
        ]
    );
}

#[test]
fn the_shipped_model_parses_and_holds_the_shape() {
    let costs = SubscriptionCosts::shipped();
    assert_eq!(PlanTables::shipped().source, "shipped");
    assert!(!costs.capacity.is_empty());
    for (tier, row) in &costs.capacity {
        for (window, capacity) in [(FIVE_HOUR, row.five_hour), (SEVEN_DAY, row.seven_day)] {
            assert!(
                capacity.is_none_or(|c| c.is_finite() && c > 0.0),
                "{tier} {window}: {capacity:?}"
            );
        }
    }
    let anchor = &costs.capacity[&costs.unit.defined_by.tier];
    assert!(anchor.five_hour.is_some() && anchor.seven_day.is_some());
    assert!(!costs.model_weight.is_empty());
    for (model, weight) in &costs.model_weight {
        assert!(
            weight.is_none_or(|w| w.is_finite() && w > 0.0),
            "{model}: {weight:?}"
        );
    }
    for model in costs.overrides.keys() {
        assert!(costs.model_weight.contains_key(model), "{model}");
    }
    let keys: Vec<&str> = costs.model_weight.keys().map(String::as_str).collect();
    let mut sorted = keys.clone();
    sorted.sort_by_key(|m| family_order(m));
    assert_eq!(keys, sorted, "the weights are in the family order");
}

#[test]
fn the_model_serializes_back_to_the_files_keys_in_the_files_order() {
    let costs = test_tables::subscription_costs().costs;
    let value = serde_json::to_value(&costs).unwrap();
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
    let reread: SubscriptionCosts =
        SubscriptionCosts::from_yaml(&serde_norway::to_string(&costs).unwrap()).unwrap();
    assert_eq!(reread, costs);
}
