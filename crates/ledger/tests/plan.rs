//! The plan side's shipped tables: the cost table and the tier factor table that price a window share, the
//! per-type factor table, and the window sizes and the peak band per tier.

use phoebis_ledger::{CostTable, ModelFactor, PlanFactors, PlanTables, PlanWindows, TierFactors};
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
fn the_shipped_bands_predicted_factor_answers_the_row_that_lists_the_window() {
    let windows = PlanWindows::shipped();
    assert_eq!(
        windows.band_factor_of(FIVE_HOUR, SEPTEMBER_BEFORE_NOON),
        Some(1.45),
        "the row's factor stands whether or not now falls inside its hours"
    );
    assert_eq!(
        windows.band_factor_of(SEVEN_DAY, SEPTEMBER_AFTERNOON),
        None,
        "no band lists the seven-day window"
    );
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

/// One million input tokens, the count every cost cell is priced per.
fn million_input() -> PinUsage {
    PinUsage {
        input: 1_000_000,
        ..Default::default()
    }
}

/// Every cell of one model, dated, in a cost file the refusal tests edit one line of.
const CELLS: &str = "
      input: {share_per_million: 0.0841, measured: 2026-09-19}
      cache_read: {share_per_million: 0.00219, measured: 2026-09-19}
      cache_write_5m: {share_per_million: 0.103, measured: 2026-09-19}
      cache_write_1h: {share_per_million: 0.103, measured: 2026-09-19}
      output: {share_per_million: 0.393, measured: 2026-09-19}";

fn cost_file(windows: &[&str], cells: &str, zone: &str) -> String {
    let mut text = String::from("effective: 2026-09-29\ntier: default_claude_max_5x\nwindows:\n");
    for window in windows {
        text += &format!("  {window}:\n    claude-opus-5:{cells}\n");
    }
    text += &format!(
        "bands:\n  - zone: {zone}\n    weekdays: true\n    from_hour: 5\n    to_hour: 11\n    factor: 1.45\n    \
         windows: [five_hour]\n    measured: 2026-09-24\n"
    );
    text
}

const TIERS: &str = "
measured: 2026-09-24
reference: default_claude_max_5x
tiers:
  default_claude_max_5x:
    five_hour: {factor: 1, measured: 2026-09-19}
    seven_day: {factor: 1, measured: 2026-09-19}
";

fn per_million(rates: Rates) -> [f64; 5] {
    [
        rates.input,
        rates.cache_read,
        rates.write_5m,
        rates.write_1h,
        rates.output,
    ]
    .map(|rate| rate * 1_000_000.0)
}

fn all_close(got: [f64; 5], want: [f64; 5]) -> bool {
    got.iter().zip(want).all(|(g, w)| close(*g, w))
}

#[test]
fn the_shipped_cost_table_dates_or_flags_every_cell_and_the_tier_table_every_factor() {
    let cost = CostTable::shipped();
    assert_eq!(
        CostTable::from_yaml(include_str!("../cost_table.yaml")).unwrap(),
        cost,
        "the shipped table is the file"
    );
    assert_eq!(
        (
            cost.effective.as_str(),
            cost.version.as_str(),
            cost.tier.as_str()
        ),
        ("2026-09-29", "2026-09-29", "default_claude_max_5x")
    );
    assert_eq!(
        cost.windows.keys().map(String::as_str).collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY]
    );
    for (window, rows) in &cost.windows {
        assert_eq!(
            rows.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "claude-fable-5",
                "claude-fable-5-1",
                "claude-haiku-4-5",
                "claude-opus-5",
                "claude-sonnet-5"
            ],
            "{window}"
        );
        for (model, cells) in rows {
            for (kind, cell) in [
                ("input", &cells.input),
                ("cache_read", &cells.cache_read),
                ("cache_write_5m", &cells.cache_write_5m),
                ("cache_write_1h", &cells.cache_write_1h),
                ("output", &cells.output),
            ] {
                assert!(
                    cell.measured.is_some() != cell.unmeasured,
                    "{window} {model} {kind} is dated or flagged, never both and never neither"
                );
                let dated = window == FIVE_HOUR
                    && matches!(model.as_str(), "claude-fable-5-1" | "claude-opus-5");
                assert_eq!(
                    cell.measured.as_deref(),
                    dated.then_some("2026-09-19"),
                    "{window} {model} {kind}: only the Max 5x 5-hour rows of Opus and Fable 5.1 are measured"
                );
                assert!(cell.share_per_million > 0.0, "{window} {model} {kind}");
            }
        }
    }
    assert_eq!(cost.bands.len(), 1);
    let band = &cost.bands[0];
    assert_eq!(
        (
            band.zone.as_str(),
            band.weekdays,
            band.from_hour,
            band.to_hour,
            band.factor,
            band.windows.as_slice(),
            band.measured.as_str(),
            band.provisional,
            band.effective_from.as_deref()
        ),
        (
            "America/Los_Angeles",
            true,
            5,
            11,
            1.45,
            [FIVE_HOUR.to_string()].as_slice(),
            "2026-09-24",
            true,
            None
        ),
        "the peak band ships beside the cost table"
    );

    let tiers = TierFactors::shipped();
    assert_eq!(
        TierFactors::from_yaml(include_str!("../tier_factors.yaml")).unwrap(),
        tiers,
        "the shipped table is the file"
    );
    assert_eq!(
        (tiers.measured.as_str(), tiers.reference.as_str()),
        ("2026-09-24", "default_claude_max_5x")
    );
    assert_eq!(
        tiers.tiers.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "default_claude_max_20x",
            "default_claude_max_5x",
            "default_claude_pro"
        ]
    );
    for (tier, row) in &tiers.tiers {
        for (window, factor) in [(FIVE_HOUR, &row.five_hour), (SEVEN_DAY, &row.seven_day)] {
            assert!(
                factor.measured.is_some() != factor.unmeasured,
                "{tier} {window} is dated or flagged, never both and never neither"
            );
        }
    }
    let factor = |tier: &str, window: &str| {
        let f = tiers.factor(tier, window).unwrap();
        (f.factor, f.measured.as_deref(), f.unmeasured)
    };
    assert_eq!(
        factor("default_claude_pro", FIVE_HOUR),
        (0.215, Some("2026-09-24"), false)
    );
    assert_eq!(
        factor("default_claude_pro", SEVEN_DAY),
        (0.18, Some("2026-09-24"), false)
    );
    assert_eq!(
        factor("default_claude_max_20x", FIVE_HOUR),
        (4.03, None, true)
    );
    assert_eq!(
        factor("default_claude_max_20x", SEVEN_DAY),
        (2.14, None, true)
    );
    assert_eq!(
        [FIVE_HOUR, SEVEN_DAY].map(|window| factor("default_claude_max_5x", window).0),
        [1.0, 1.0],
        "the reference tier is 1 on both windows"
    );
    assert_eq!(
        factor("default_claude_pro", SEVEN_DAY_FABLE),
        factor("default_claude_pro", SEVEN_DAY),
        "the Fable-scoped week reads the 7-day factor"
    );
}

#[test]
fn a_cost_table_whose_windows_are_not_the_two_is_refused() {
    let zone = "America/Los_Angeles";
    CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, zone))
        .expect("the file with both windows parses");
    let third = CostTable::from_yaml(&cost_file(
        &[FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE],
        CELLS,
        zone,
    ))
    .unwrap_err()
    .to_string();
    assert!(
        third.contains(SEVEN_DAY_FABLE),
        "the refusal names the third window: {third}"
    );
    let missing = CostTable::from_yaml(&cost_file(&[FIVE_HOUR], CELLS, zone))
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains(SEVEN_DAY),
        "a table without the 7-day window is refused too: {missing}"
    );
}

#[test]
fn a_cost_table_model_without_one_of_its_five_cells_is_refused() {
    let without_output = CELLS.replace(
        "\n      output: {share_per_million: 0.393, measured: 2026-09-19}",
        "",
    );
    assert_ne!(without_output, CELLS);
    let refused = CostTable::from_yaml(&cost_file(
        &[FIVE_HOUR, SEVEN_DAY],
        &without_output,
        "America/Los_Angeles",
    ))
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("output"),
        "the refusal names the missing cell: {refused}"
    );
}

#[test]
fn a_cost_table_band_in_a_zone_the_bundled_database_lacks_is_refused() {
    let refused = CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, "Mars/Olympus"))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("Mars/Olympus"),
        "the refusal names the zone: {refused}"
    );
}

#[test]
fn a_tier_without_one_of_its_two_windows_is_refused() {
    TierFactors::from_yaml(TIERS).expect("the tier with both windows parses");
    let refused = TierFactors::from_yaml(
        &TIERS.replace("    seven_day: {factor: 1, measured: 2026-09-19}\n", ""),
    )
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("seven_day"),
        "the refusal names the missing window: {refused}"
    );
}

#[test]
fn a_tier_factor_at_or_below_zero_is_refused() {
    for factor in ["0", "-1"] {
        let refused = TierFactors::from_yaml(&TIERS.replace(
            "five_hour: {factor: 1,",
            &format!("five_hour: {{factor: {factor},"),
        ))
        .unwrap_err()
        .to_string();
        assert!(
            refused.contains("default_claude_max_5x") && refused.contains(FIVE_HOUR),
            "the refusal names the tier and the window: {refused}"
        );
    }
}

#[test]
fn a_pro_account_prices_a_request_as_the_max_5x_share_over_its_factor() {
    let tables = PlanTables::shipped();
    let five = tables
        .share(
            FIVE_HOUR,
            "default_claude_pro",
            "claude-opus-5",
            million_input(),
        )
        .unwrap();
    assert!(close(five.share, 0.0841 / 0.215), "{}", five.share);
    assert!(
        !five.unmeasured,
        "the Opus 5-hour cell and Pro's factor are measured"
    );
    let seven = tables
        .share(
            SEVEN_DAY,
            "default_claude_pro",
            "claude-opus-5",
            million_input(),
        )
        .unwrap();
    assert!(close(seven.share, 0.00610 / 0.18), "{}", seven.share);
    assert!(seven.unmeasured, "every 7-day cell is a stand-in");
    assert_eq!(
        tables.tier_factor("default_claude_pro", FIVE_HOUR),
        Some(0.215)
    );

    // 100 input, 1,000 cache reads, 10 five-minute and 20 one-hour writes, 50 output.
    let mixed = PinUsage {
        input: 100,
        cache_read: 1_000,
        write_5m: 10,
        write_1h: 20,
        output: 50,
    };
    let want = (100.0 * 0.0841 + 1_000.0 * 0.00219 + 10.0 * 0.103 + 20.0 * 0.103 + 50.0 * 0.393)
        / 1_000_000.0;
    let got = tables
        .cost
        .max_5x_share(FIVE_HOUR, "claude-opus-5", mixed)
        .unwrap();
    assert!(close(got.share, want), "{} against {want}", got.share);
    let pro = tables
        .share(FIVE_HOUR, "default_claude_pro", "claude-opus-5", mixed)
        .unwrap();
    assert!(close(pro.share, want / 0.215));
    let max_20x = tables
        .share(FIVE_HOUR, "default_claude_max_20x", "claude-opus-5", mixed)
        .unwrap();
    assert!(close(max_20x.share, want / 4.03));
    assert!(
        max_20x.unmeasured,
        "a flagged tier factor flags the share it divides"
    );
}

#[test]
fn a_fable_requests_week_share_is_twice_its_seven_day_share() {
    let tables = PlanTables::shipped();
    let week = tables
        .share(
            SEVEN_DAY_FABLE,
            "default_claude_pro",
            "claude-opus-5",
            million_input(),
        )
        .unwrap();
    assert!(close(week.share, 2.0 * 0.00610 / 0.18), "{}", week.share);
    assert!(week.unmeasured);
    let seven = tables
        .share(
            SEVEN_DAY,
            "default_claude_pro",
            "claude-opus-5",
            million_input(),
        )
        .unwrap();
    assert!((week.share - 2.0 * seven.share).abs() <= 1e-9);
    let max_5x = tables
        .share(
            SEVEN_DAY_FABLE,
            "default_claude_max_5x",
            "claude-opus-5",
            million_input(),
        )
        .unwrap();
    assert!(close(max_5x.share, 0.0122), "{}", max_5x.share);
}

#[test]
fn an_unlisted_tier_and_an_unlisted_model_read_none() {
    let tables = PlanTables::shipped();
    let usage = million_input();
    assert_eq!(
        tables.share(FIVE_HOUR, "default_claude_team", "claude-opus-5", usage),
        None
    );
    assert_eq!(tables.tier_factor("default_claude_team", FIVE_HOUR), None);
    assert_eq!(
        tables.five_hour_share_at(
            "default_claude_team",
            "claude-opus-5",
            usage,
            SEPTEMBER_AFTERNOON
        ),
        None
    );
    assert_eq!(
        tables.share(FIVE_HOUR, "default_claude_pro", "claude-opus-4-8", usage),
        None
    );
    assert_eq!(
        tables
            .cost
            .max_5x_share(SEVEN_DAY, "claude-opus-4-8", usage),
        None
    );
    assert_eq!(tables.cost.cells(FIVE_HOUR, "claude-opus-4-8"), None);
    assert_eq!(
        tables.share("5h", "default_claude_pro", "claude-opus-5", usage),
        None,
        "a window the tables do not name prices nothing"
    );
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert_eq!(
            tables.share(window, "claude_pro", "claude-opus-5", usage),
            tables.share(window, "default_claude_pro", "claude-opus-5", usage),
            "a tier spelled without its prefix reads the same row on {window}"
        );
    }
    assert_eq!(tables.tier_factor("claude_pro", SEVEN_DAY), Some(0.18));
}

#[test]
fn the_band_weighs_a_five_hour_charge_at_completion_and_never_a_seven_day_one() {
    let tables = PlanTables::shipped();
    assert_eq!(
        tables.cost.band_factor(FIVE_HOUR, SEPTEMBER_AFTERNOON),
        1.45
    );
    assert_eq!(
        tables.cost.band_factor(SEVEN_DAY, SEPTEMBER_AFTERNOON),
        1.0,
        "the band names the 5-hour window alone"
    );
    assert_eq!(
        tables.cost.band_factor(FIVE_HOUR, SEPTEMBER_BEFORE_NOON),
        1.0
    );
    assert_eq!(tables.cost.band_factor(FIVE_HOUR, SATURDAY_AFTERNOON), 1.0);
    let at = |instant: i64| {
        tables
            .five_hour_share_at(
                "default_claude_pro",
                "claude-opus-5",
                million_input(),
                instant,
            )
            .unwrap()
    };
    let inside = at(SEPTEMBER_AFTERNOON);
    let outside = at(SEPTEMBER_BEFORE_NOON);
    assert!(
        close(inside.share, 1.45 * outside.share),
        "{} against {}",
        inside.share,
        outside.share
    );
    let unbanded = tables
        .share(
            FIVE_HOUR,
            "default_claude_pro",
            "claude-opus-5",
            million_input(),
        )
        .unwrap();
    assert_eq!(
        outside, unbanded,
        "outside the band the charge is the plain share"
    );
    assert_eq!(
        at(SATURDAY_AFTERNOON),
        unbanded,
        "a weekend afternoon is outside a weekday band"
    );
}

#[test]
fn the_share_rate_table_prices_a_dated_id_at_its_undated_row_and_an_unknown_model_at_the_dearest_cells(
) {
    let tables = PlanTables::shipped();
    let five = tables.rate_table(FIVE_HOUR);
    assert!(
        all_close(
            per_million(five.rates_for("claude-haiku-4-5-20251001")),
            [0.0303, 0.000787, 0.0372, 0.0372, 0.141]
        ),
        "a dated id reads its undated row's cells: {:?}",
        per_million(five.rates_for("claude-haiku-4-5-20251001"))
    );
    assert!(
        all_close(
            per_million(five.rates_for("gpt-5")),
            [0.304, 0.00789, 0.373, 0.373, 1.42]
        ),
        "an unknown model reads the dearest 5-hour cell of each type: {:?}",
        per_million(five.rates_for("gpt-5"))
    );
    let seven = tables.rate_table(SEVEN_DAY);
    assert!(
        all_close(
            per_million(seven.rates_for("gpt-5")),
            [0.0220, 0.000572, 0.0271, 0.0271, 0.103]
        ),
        "an unknown model reads the dearest 7-day cell of each type: {:?}",
        per_million(seven.rates_for("gpt-5"))
    );
    let mixed = PinUsage {
        input: 100,
        cache_read: 1_000,
        write_5m: 10,
        write_1h: 20,
        output: 50,
    };
    let priced = five.price("claude-fable-5-1", mixed);
    let share = tables
        .cost
        .max_5x_share(FIVE_HOUR, "claude-fable-5-1", mixed)
        .unwrap()
        .share;
    assert!(
        close(priced, share),
        "the rate table prices a usage as max_5x_share does: {priced} against {share}"
    );
}
