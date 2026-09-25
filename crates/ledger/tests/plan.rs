//! The plan side's two shipped tables: meter tokens per token by model and type, and the window sizes and the
//! peak band per tier.

use phoebis_ledger::{ModelFactor, PlanFactors, PlanWindows};
use phoebis_routing::{PinUsage, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};

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
/// Monday 2026-10-05 at 14:00 UTC, inside the band's hours.
const OCTOBER_AFTERNOON: i64 = 1_791_208_800_000;

/// A table whose second row, listed first, takes over the 5-hour band from 2026-10-01 at another factor.
const TWO_ROWS: &str = "
measured: 2026-09-25
tiers: {}
bands:
  - zone: America/Los_Angeles
    effective_from: 2026-10-01
    weekdays: true
    from_hour: 5
    to_hour: 11
    factor: 1.2
    windows: [five_hour]
    measured: 2026-09-25
  - zone: America/Los_Angeles
    weekdays: true
    from_hour: 5
    to_hour: 11
    factor: 1.45
    windows: [five_hour]
    measured: 2026-09-24
";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

#[test]
fn the_shipped_tables_parse_and_the_reference_row_is_the_unit() {
    let factors = PlanFactors::shipped();
    assert_eq!(
        PlanFactors::from_yaml(include_str!("../plan_factors.yaml")).unwrap(),
        factors,
        "the shipped table is the file"
    );
    assert_eq!(factors.reference, "claude-opus-5");
    assert_eq!(
        factors.factor(&factors.reference),
        Some(&ModelFactor {
            factor: 1.0,
            input: 1.0,
            cache_read: 0.026,
            cache_write_5m: 1.23,
            cache_write_1h: 1.23,
            output: 4.67,
            measured: Some("2026-09-19".into()),
            unmeasured: false,
        })
    );
    assert_eq!(
        factors.meter_tokens("claude-opus-5", 1, 0, 0, 0, 0),
        Some(1.0),
        "one plain input token of the reference is one meter token"
    );
    let rates = factors.rate_table();
    let one_input = PinUsage {
        input: 1,
        ..Default::default()
    };
    assert_eq!(rates.price("claude-opus-5", one_input), 1.0);
    assert_eq!(
        rates.price("gpt-5", one_input),
        1.0,
        "a model the table lacks prices as the reference"
    );
    assert!(PlanFactors::from_yaml("models: [").is_err());

    let windows = PlanWindows::shipped();
    assert_eq!(
        PlanWindows::from_yaml(include_str!("../plan_windows.yaml")).unwrap(),
        windows,
        "the shipped table is the file"
    );
    let cells = |tier: &str| {
        [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE].map(|window| windows.predicted(tier, window))
    };
    assert_eq!(
        cells("default_claude_max_5x"),
        [Some(11_890_000.0), Some(164_000_000.0), Some(23_000_000.0)]
    );
    assert_eq!(
        cells("default_claude_max_20x"),
        [Some(47_900_000.0), Some(351_100_000.0), Some(49_000_000.0)]
    );
    assert_eq!(
        cells("default_claude_pro"),
        [Some(2_560_000.0), Some(30_000_000.0), None],
        "the Pro tier carries no Fable-scoped window"
    );
    assert_eq!(
        cells("claude_pro"),
        cells("default_claude_pro"),
        "a tier spelled without its prefix reads the same row"
    );
    assert_eq!(cells("default_claude_team"), [None, None, None]);
    assert_eq!(windows.predicted("default_claude_max_5x", "5h"), None);
    assert!(windows.tiers["default_claude_max_20x"].unmeasured);
    assert!(!windows.tiers["default_claude_max_5x"].unmeasured);
    assert!(PlanWindows::from_yaml("tiers: [").is_err());
    let unknown_zone =
        PlanWindows::from_yaml(&TWO_ROWS.replace("America/Los_Angeles", "Mars/Olympus"))
            .unwrap_err()
            .to_string();
    assert!(
        unknown_zone.contains("Mars/Olympus"),
        "the refusal names the zone: {unknown_zone}"
    );
    let bad_date = PlanWindows::from_yaml(&TWO_ROWS.replace("2026-10-01", "2026-13-45"))
        .unwrap_err()
        .to_string();
    assert!(
        bad_date.contains("2026-13-45"),
        "the refusal names the date: {bad_date}"
    );
}

#[test]
fn an_unmeasured_model_carries_the_list_ratio_and_the_flag_and_a_dated_id_reads_its_undated_row() {
    let factors = PlanFactors::shipped();
    let fable = factors.factor("claude-fable-5").unwrap();
    assert_eq!((fable.factor, fable.unmeasured), (2.0, true));
    assert_eq!(
        factors.factor("claude-fable-5-20260601"),
        Some(fable),
        "a dated id reads its undated row"
    );
    let one_input = PinUsage {
        input: 1,
        ..Default::default()
    };
    assert_eq!(
        factors.rate_table().price("claude-fable-5", one_input),
        2.0,
        "a Fable 5 pin weighs the list ratio, not the reference's 1"
    );
    for (model, factor) in [
        ("claude-haiku-4-5-20251001", 0.36),
        ("claude-sonnet-5", 0.54),
    ] {
        let row = factors.factor(model).unwrap();
        assert_eq!(
            (row.factor, row.measured.as_deref(), row.unmeasured),
            (factor, Some("2026-09-24"), false),
            "{model}"
        );
        assert_eq!(
            (
                row.cache_read,
                row.cache_write_5m,
                row.cache_write_1h,
                row.output
            ),
            (0.026, 1.23, 1.23, 4.67),
            "{model} carries the reference's type ratios"
        );
    }
    let fable_5_1 = factors.factor("claude-fable-5-1").unwrap();
    assert_eq!(
        (
            fable_5_1.factor,
            fable_5_1.measured.as_deref(),
            fable_5_1.unmeasured
        ),
        (3.61, Some("2026-09-19"), false)
    );
}

#[test]
fn the_band_weighs_a_weekday_afternoon_dispatch_on_the_five_hour_window_and_nothing_else() {
    let windows = PlanWindows::shipped();
    assert_eq!(windows.bands.len(), 1);
    let band = &windows.bands[0];
    assert_eq!(
        (
            band.zone.as_str(),
            band.from_hour,
            band.to_hour,
            band.effective_from.as_deref()
        ),
        ("America/Los_Angeles", 5, 11, None)
    );
    assert!(band.provisional);
    assert_eq!(band.measured, "2026-09-24");
    assert_eq!(windows.band_factor(FIVE_HOUR, SEPTEMBER_AFTERNOON), 1.45);
    assert_eq!(windows.band_factor(FIVE_HOUR, SEPTEMBER_NOON), 1.45);
    assert_eq!(windows.band_factor(FIVE_HOUR, SEPTEMBER_BEFORE_NOON), 1.0);
    assert_eq!(
        windows.band_factor(FIVE_HOUR, SEPTEMBER_EVENING),
        1.0,
        "the band ends before its closing hour"
    );
    assert_eq!(windows.band_factor(SEVEN_DAY, SEPTEMBER_AFTERNOON), 1.0);
    assert_eq!(
        windows.band_factor(SEVEN_DAY_FABLE, SEPTEMBER_AFTERNOON),
        1.0
    );
    assert_eq!(
        windows.band_factor(FIVE_HOUR, SATURDAY_AFTERNOON),
        1.0,
        "a weekend afternoon is outside a weekday band"
    );
    // Pacific daylight time ends on 2026-11-01, so the UTC edges move an hour later.
    assert_eq!(
        windows.band_factor(FIVE_HOUR, NOVEMBER_HALF_PAST_TWELVE),
        1.0
    );
    assert_eq!(windows.band_factor(FIVE_HOUR, NOVEMBER_HALF_PAST_ONE), 1.45);
    assert_eq!(windows.band_factor(FIVE_HOUR, NOVEMBER_HALF_PAST_SIX), 1.45);

    let dated = PlanWindows::from_yaml(TWO_ROWS).unwrap();
    assert_eq!(
        dated.band_factor(FIVE_HOUR, SEPTEMBER_AFTERNOON),
        1.45,
        "before the second row's date the first row is in force"
    );
    assert_eq!(
        dated.band_factor(FIVE_HOUR, OCTOBER_AFTERNOON),
        1.2,
        "from its date the second row is in force, wherever it sits in the file"
    );
    assert_eq!(dated.band_factor(SEVEN_DAY, OCTOBER_AFTERNOON), 1.0);
}

#[test]
fn meter_tokens_of_a_mixed_usage_are_the_weighted_sum_and_a_missing_model_is_none() {
    let factors = PlanFactors::shipped();
    // 100 input, 1,000 cache reads, 10 five-minute and 20 one-hour writes, 50 output.
    let opus = 100.0 + 1_000.0 * 0.026 + 10.0 * 1.23 + 20.0 * 1.23 + 50.0 * 4.67;
    let got = factors
        .meter_tokens("claude-opus-5", 100, 1_000, 10, 20, 50)
        .unwrap();
    assert!(close(got, opus), "{got} against {opus}");
    let fable = factors
        .meter_tokens("claude-fable-5-1", 100, 1_000, 10, 20, 50)
        .unwrap();
    assert!(
        close(fable, 3.61 * opus),
        "the model factor scales the whole sum"
    );
    assert_eq!(factors.meter_tokens("gpt-5", 100, 0, 0, 0, 0), None);
    let priced = factors.rate_table().price(
        "claude-fable-5-1",
        PinUsage {
            input: 100,
            cache_read: 1_000,
            write_5m: 10,
            write_1h: 20,
            output: 50,
        },
    );
    assert!(
        close(priced, fable),
        "the rate table prices a usage as meter_tokens does"
    );
}
