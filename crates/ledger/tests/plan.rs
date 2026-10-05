//! The plan side's one shipped table: the cost table, whose cells and tier factors price a window share, and the
//! peak band on its 5-hour cells.

use phoebis_ledger::{CostTable, ModelCells, PlanTables};
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

const MAX_20X: &str = "default_claude_max_20x";
const MAX_5X: &str = "default_claude_max_5x";
const PRO: &str = "default_claude_pro";
const ZONE: &str = "America/Los_Angeles";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

/// One million input tokens, the count every cost cell is priced per.
fn million_input() -> PinUsage {
    PinUsage {
        input: 1_000_000,
        ..Default::default()
    }
}

/// 100 input, 1,000 cache reads, 10 five-minute and 20 one-hour writes, 50 output.
fn mixed() -> PinUsage {
    PinUsage {
        input: 100,
        cache_read: 1_000,
        write_5m: 10,
        write_1h: 20,
        output: 50,
    }
}

/// Opus 5.5's five cells, one per line, in a cost file the refusal tests edit one line of.
const CELLS: &str = "
      input: 0.01461
      cache_read: 0.000365
      cache_write_5m: 0.0183
      cache_write_1h: 0.01824
      output: 0.07279";

/// A v2 file with the shipped tiers, `cells` as Opus 5.5's row on each of `windows`, and one band row in `zone`.
fn cost_file(windows: &[&str], cells: &str, zone: &str) -> String {
    let mut text = String::from(
        "effective: 2026-10-01\ntiers:\n  default_claude_max_20x: {five_hour: 1, seven_day: 1}\n  \
         default_claude_max_5x: {five_hour: 0.25, seven_day: null}\n  \
         default_claude_pro: {five_hour: 0.05, seven_day: null}\nwindows:\n",
    );
    for window in windows {
        text += &format!("  {window}:\n    claude-opus-5-5:{cells}\n");
    }
    text += &format!(
        "bands:\n  - zone: {zone}\n    weekdays: true\n    from_hour: 5\n    to_hour: 11\n    factor: 1.435\n    \
         windows: [five_hour]\n"
    );
    text
}

/// A cost file whose 7-day cells are priced too, at the 5-hour ones.
fn priced_seven_day() -> PlanTables {
    PlanTables {
        cost: CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE)).unwrap(),
    }
}

/// The five cells in the file's order: input, cache read, 5-minute write, 1-hour write, output.
fn cells_of(cells: &ModelCells) -> [Option<f64>; 5] {
    [
        cells.input,
        cells.cache_read,
        cells.cache_write_5m,
        cells.cache_write_1h,
        cells.output,
    ]
}

fn per_million(rates: Rates) -> [Option<f64>; 5] {
    [
        rates.input,
        rates.cache_read,
        rates.write_5m,
        rates.write_1h,
        rates.output,
    ]
    .map(|rate| rate.map(|r| r * 1_000_000.0))
}

fn all_close(got: [Option<f64>; 5], want: [Option<f64>; 5]) -> bool {
    got.iter().zip(want).all(|(g, w)| match (g, w) {
        (Some(g), Some(w)) => close(*g, w),
        (None, None) => true,
        _ => false,
    })
}

#[test]
fn the_band_weighs_a_weekday_afternoon_dispatch_on_the_five_hour_window_and_nothing_else() {
    let cost = CostTable::shipped();
    assert_eq!(cost.bands.len(), 1);
    let band = &cost.bands[0];
    assert_eq!(
        (
            band.zone.as_str(),
            band.from_hour,
            band.to_hour,
            band.windows.as_slice()
        ),
        (ZONE, 5, 11, [FIVE_HOUR.to_string()].as_slice())
    );
    let every = [
        SEPTEMBER_AFTERNOON,
        SEPTEMBER_NOON,
        SEPTEMBER_BEFORE_NOON,
        SEPTEMBER_EVENING,
        SATURDAY_AFTERNOON,
        NOVEMBER_HALF_PAST_TWELVE,
        NOVEMBER_HALF_PAST_ONE,
        NOVEMBER_HALF_PAST_SIX,
    ];
    assert_eq!(cost.band_factor(FIVE_HOUR, SEPTEMBER_AFTERNOON), 1.435);
    assert_eq!(cost.band_factor(FIVE_HOUR, SEPTEMBER_NOON), 1.435);
    assert_eq!(cost.band_factor(FIVE_HOUR, SEPTEMBER_BEFORE_NOON), 1.0);
    assert_eq!(
        cost.band_factor(FIVE_HOUR, SEPTEMBER_EVENING),
        1.0,
        "the band ends before its closing hour"
    );
    assert_eq!(
        cost.band_factor(FIVE_HOUR, SATURDAY_AFTERNOON),
        1.0,
        "a weekend afternoon is outside a weekday band"
    );
    // Pacific daylight time ends on 2026-11-01, so the UTC edges move an hour later.
    assert_eq!(cost.band_factor(FIVE_HOUR, NOVEMBER_HALF_PAST_TWELVE), 1.0);
    assert_eq!(cost.band_factor(FIVE_HOUR, NOVEMBER_HALF_PAST_ONE), 1.435);
    assert_eq!(cost.band_factor(FIVE_HOUR, NOVEMBER_HALF_PAST_SIX), 1.435);
    for at in every {
        assert_eq!(
            cost.band_factor(SEVEN_DAY, at),
            1.0,
            "the band names the 5-hour window alone, so a 7-day charge at {at} is unweighed"
        );
    }
    assert_eq!(cost.band_factor(SEVEN_DAY_FABLE, SEPTEMBER_AFTERNOON), 1.0);

    let two_rows = format!(
        "{}  - zone: {ZONE}\n    weekdays: true\n    from_hour: 5\n    to_hour: 11\n    factor: 2.0\n    \
         windows: [five_hour]\n",
        cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE)
    );
    let later = CostTable::from_yaml(&two_rows).unwrap();
    assert_eq!(
        later.band_factor(FIVE_HOUR, OCTOBER_AFTERNOON),
        2.0,
        "the last row naming the window is in force"
    );
    assert_eq!(later.band_factor(SEVEN_DAY, OCTOBER_AFTERNOON), 1.0);
}

#[test]
fn the_shipped_cost_table_is_the_v2_file_with_null_for_unmeasured() {
    let cost = CostTable::shipped();
    assert_eq!(
        CostTable::from_yaml(include_str!("../cost_table.yaml")).unwrap(),
        cost,
        "the shipped table is the file"
    );
    assert_eq!(cost.effective, "2026-10-01");
    assert_eq!(
        cost.windows.keys().map(String::as_str).collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY]
    );
    let priced = [
        "claude-fable-5-1",
        "claude-haiku-4-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
    ];
    for (window, rows) in &cost.windows {
        assert_eq!(
            rows.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "claude-fable-5",
                "claude-fable-5-1",
                "claude-haiku-4-5",
                "claude-opus-5",
                "claude-opus-5-5",
                "claude-sonnet-5",
                "claude-sonnet-5-5"
            ],
            "{window}"
        );
        for (model, cells) in rows {
            let cells = cells_of(cells);
            if window == FIVE_HOUR && priced.contains(&model.as_str()) {
                assert!(
                    cells.iter().all(|c| c.is_some_and(|c| c > 0.0)),
                    "{window} {model}: the plan meter measured every cell: {cells:?}"
                );
            } else {
                assert_eq!(
                    cells, [None; 5],
                    "{window} {model}: an older model and every 7-day cell are unmeasured"
                );
            }
        }
    }
    assert_eq!(
        cells_of(cost.cells(FIVE_HOUR, "claude-fable-5-1").unwrap()),
        [
            Some(0.06647),
            Some(0.001675),
            Some(0.08326),
            Some(0.084),
            Some(0.3292)
        ]
    );
    let tiers: Vec<(&str, Option<f64>, Option<f64>)> = cost
        .tiers
        .iter()
        .map(|(tier, f)| (tier.as_str(), f.five_hour, f.seven_day))
        .collect();
    assert_eq!(
        tiers,
        [
            (MAX_20X, Some(1.0), Some(1.0)),
            (MAX_5X, Some(0.25), None),
            (PRO, Some(0.05), None)
        ]
    );
}

/// A v1 field under the v2 reader, one line to a case; the refusal names it.
const V1_CELL: &str = "      input: {share_per_million: 0.01461, measured: 2026-09-19}"; // v1 field
/// A row's five cells with a v1 flag beside them.
const V1_FLAG: &str = "      output: 0.07279\n      unmeasured: true"; // v1 field

#[test]
fn a_file_with_a_v1_field_is_refused_naming_it() {
    let file = cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE);
    CostTable::from_yaml(&file).expect("the v2 file parses");
    let top = |line: &str| {
        file.replacen(
            "effective: 2026-10-01\n",
            &format!("effective: 2026-10-01\n{line}"),
            1,
        )
    };
    let band = |line: &str| format!("{file}{line}");
    let cases = [
        (top("tier: default_claude_max_5x\n"), "tier"),
        (top("reference: default_claude_max_20x\n"), "reference"),
        (top("measured: 2026-09-24\n"), "measured"), // v1 field
        (file.replace("      input: 0.01461", V1_CELL), "input"),
        (file.replace("      output: 0.07279", V1_FLAG), "unmeasured"),
        (band("    measured: 2026-09-24\n"), "measured"), // v1 field
        (band("    provisional: true\n"), "provisional"), // v1 field
        (band("    effective_from: 2026-10-01\n"), "effective_from"), // v1 field
        (
            file.replace("seven_day: 1}", "seven_day: 1, seven_day_fable: 1}"),
            "seven_day_fable",
        ),
        (top("version: 2026-10-01\n"), "version"),
    ];
    for (text, name) in cases {
        assert_ne!(text, file, "the edit for {name} lands");
        let refused = CostTable::from_yaml(&text).unwrap_err().to_string();
        let named = if name == "input" {
            refused.contains(".input") && refused.contains("invalid type: map")
        } else {
            refused.contains(&format!("unknown field `{name}`"))
        };
        assert!(named, "the refusal names `{name}`: {refused}");
    }
}

/// A caller that deserializes the table itself skips `from_yaml`, so the table's own shape refuses a v1 key too.
#[test]
fn a_cost_table_read_without_from_yaml_refuses_a_top_level_unknown_key_by_name() {
    let file = cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE);
    serde_norway::from_str::<CostTable>(&file).expect("the v2 file deserializes");
    for name in ["tier", "version"] {
        let text = file.replacen(
            "effective: 2026-10-01\n",
            &format!("effective: 2026-10-01\n{name}: default_claude_max_5x\n"),
            1,
        );
        assert_ne!(text, file, "the edit for {name} lands");
        let refused = serde_norway::from_str::<CostTable>(&text)
            .map(|_| "read".to_string())
            .unwrap_or_else(|e| e.to_string());
        assert!(
            refused.contains(&format!("unknown field `{name}`")),
            "the refusal names `{name}`: {refused}"
        );
    }
}

#[test]
fn a_row_or_a_tier_missing_a_key_is_refused_rather_than_read_as_unmeasured() {
    let without_output = CELLS.replace("\n      output: 0.07279", "");
    assert_ne!(without_output, CELLS);
    let refused = CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], &without_output, ZONE))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("missing field `output`"),
        "a row without its output cell is refused, not read as null: {refused}"
    );
    let file = cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE);
    let without_seven_day = file.replace("{five_hour: 0.05, seven_day: null}", "{five_hour: 0.05}");
    assert_ne!(without_seven_day, file);
    let refused = CostTable::from_yaml(&without_seven_day)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("missing field `seven_day`"),
        "a tier without its 7-day factor is refused, not read as null: {refused}"
    );
}

#[test]
fn a_cost_table_whose_windows_are_not_the_two_is_refused() {
    CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE))
        .expect("the file with both windows parses");
    let third = CostTable::from_yaml(&cost_file(
        &[FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE],
        CELLS,
        ZONE,
    ))
    .unwrap_err()
    .to_string();
    assert!(
        third.contains(r#"["five_hour", "seven_day", "seven_day_fable"]"#),
        "the refusal lists the windows the file carries: {third}"
    );
    let missing = CostTable::from_yaml(&cost_file(&[FIVE_HOUR], CELLS, ZONE))
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains(r#"["five_hour"]"#),
        "a table without the 7-day window is refused too, listing the one it carries: {missing}"
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
fn a_negative_cell_or_a_factor_at_or_below_zero_is_refused() {
    let negative = CELLS.replace("input: 0.01461", "input: -0.1");
    assert_ne!(negative, CELLS);
    let refused = CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], &negative, ZONE))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("claude-opus-5-5") && refused.contains("input"),
        "the refusal names the model and the cell: {refused}"
    );
    let file = cost_file(&[FIVE_HOUR, SEVEN_DAY], CELLS, ZONE);
    let with_factor = |factor: &str| {
        let text = file.replace(
            "default_claude_max_20x: {five_hour: 1,",
            &format!("default_claude_max_20x: {{five_hour: {factor},"),
        );
        assert_ne!(text, file);
        CostTable::from_yaml(&text)
    };
    for factor in ["0", "-1"] {
        let refused = with_factor(factor).unwrap_err().to_string();
        assert!(
            refused.contains(MAX_20X) && refused.contains(FIVE_HOUR),
            "the refusal names the tier and the window: {refused}"
        );
    }
    let unmeasured = with_factor("null").expect("a null factor parses");
    assert_eq!(unmeasured.tiers[MAX_20X].five_hour, None);
}

#[test]
fn a_null_cell_prices_a_request_only_while_it_holds_no_tokens_of_that_type() {
    let cells = CELLS.replace("output: 0.07279", "output: null");
    assert_ne!(cells, CELLS);
    let cost = CostTable::from_yaml(&cost_file(&[FIVE_HOUR, SEVEN_DAY], &cells, ZONE)).unwrap();
    let input_only = cost
        .reference_share(FIVE_HOUR, "claude-opus-5-5", million_input())
        .unwrap();
    assert!(close(input_only, 0.01461), "{input_only}");
    assert_eq!(
        cost.reference_share(
            FIVE_HOUR,
            "claude-opus-5-5",
            PinUsage {
                output: 1,
                ..million_input()
            }
        ),
        None,
        "one output token meets the null cell, so the request prices no share"
    );
    let shipped = CostTable::shipped();
    assert_eq!(
        shipped.reference_share(FIVE_HOUR, "claude-opus-5", million_input()),
        None,
        "every Opus 5 cell is null"
    );
    assert_eq!(
        shipped.reference_share(FIVE_HOUR, "claude-opus-5", PinUsage::default()),
        Some(0.0),
        "an empty usage meets no cell"
    );
}

#[test]
fn a_tier_prices_the_reference_share_over_its_five_hour_factor_and_a_null_factor_prices_no_share() {
    let tables = PlanTables::shipped();
    let fable = |tier: &str| tables.share(FIVE_HOUR, tier, "claude-fable-5-1", million_input());
    for (tier, want) in [
        (MAX_20X, 0.06647),
        (MAX_5X, 0.06647 / 0.25),
        (PRO, 0.06647 / 0.05),
    ] {
        let got = fable(tier).unwrap();
        assert!(close(got, want), "{tier}: {got} against {want}");
    }
    assert_eq!(fable("default_claude_team"), None, "a tier the table lacks");
    assert_eq!(
        tables.five_hour_share_at(
            "default_claude_team",
            "claude-fable-5-1",
            million_input(),
            SEPTEMBER_AFTERNOON
        ),
        None
    );
    assert_eq!(
        fable("claude_pro"),
        fable(PRO),
        "a tier spelled without its prefix reads the same row"
    );
    let haiku = tables.share(FIVE_HOUR, PRO, "claude-haiku-4-5", million_input());
    assert!(haiku.is_some());
    assert_eq!(
        tables.share(FIVE_HOUR, PRO, "claude-haiku-4-5-20251001", million_input()),
        haiku,
        "a dated id prices at its undated row"
    );
    assert_eq!(
        tables.share(FIVE_HOUR, PRO, "claude-opus-4-8", million_input()),
        None,
        "a model the table lacks"
    );
    assert_eq!(tables.tier_factor(MAX_5X, FIVE_HOUR), Some(0.25));
    assert_eq!(tables.tier_factor(MAX_5X, SEVEN_DAY), None);
    assert_eq!(tables.tier_factor(MAX_5X, "5h"), None);
    for tier in [MAX_20X, MAX_5X, PRO] {
        for window in [SEVEN_DAY, SEVEN_DAY_FABLE] {
            assert_eq!(
                tables.share(window, tier, "claude-fable-5-1", million_input()),
                None,
                "{tier} {window}: every 7-day cell is null"
            );
        }
    }
    let priced = priced_seven_day();
    assert_eq!(
        priced.share(SEVEN_DAY, MAX_5X, "claude-opus-5-5", million_input()),
        None,
        "the 7-day factor of Max 5x is null, so a priced cell still prices no share of its week"
    );
    let reference = priced
        .share(SEVEN_DAY, MAX_20X, "claude-opus-5-5", million_input())
        .unwrap();
    assert!(close(reference, 0.01461), "{reference}");
}

#[test]
fn a_fable_requests_week_share_is_twice_its_seven_day_share() {
    let priced = priced_seven_day();
    let seven = priced
        .share(SEVEN_DAY, MAX_20X, "claude-opus-5-5", million_input())
        .unwrap();
    let week = priced
        .share(SEVEN_DAY_FABLE, MAX_20X, "claude-opus-5-5", million_input())
        .unwrap();
    assert!(close(week, 2.0 * seven), "{week} against {seven}");
    let shipped = PlanTables::shipped();
    for window in [SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert_eq!(
            shipped.share(window, MAX_20X, "claude-opus-5-5", million_input()),
            None
        );
    }
}

/// A model week the table does not measure prices as the 7-day share once and reads unmeasured; the 5-hour, the 7-day
/// and the Fable week read measured, and a window that is no week prices nothing.
#[test]
fn an_unmeasured_model_week_prices_the_seven_day_share_once_and_reads_unmeasured() {
    let priced = priced_seven_day();
    let seven = priced
        .share(SEVEN_DAY, MAX_20X, "claude-opus-5-5", million_input())
        .unwrap();
    assert_eq!(
        priced.share(
            "seven_day_opus",
            MAX_20X,
            "claude-opus-5-5",
            million_input()
        ),
        Some(seven)
    );
    assert_eq!(
        priced.share("one_hour", MAX_20X, "claude-opus-5-5", million_input()),
        None
    );
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert!(priced.measured(window), "{window}");
    }
    assert!(!priced.measured("seven_day_opus"));
}

#[test]
fn the_band_weighs_a_five_hour_charge_at_completion_and_never_a_seven_day_one() {
    let tables = PlanTables::shipped();
    let at = |instant: i64| {
        tables
            .five_hour_share_at(MAX_20X, "claude-opus-5-5", million_input(), instant)
            .unwrap()
    };
    let inside = at(SEPTEMBER_AFTERNOON);
    let outside = at(SEPTEMBER_BEFORE_NOON);
    assert!(close(inside, 0.01461 * 1.435), "{inside}");
    assert!(close(outside, 0.01461), "{outside}");
    let unbanded = tables
        .share(FIVE_HOUR, MAX_20X, "claude-opus-5-5", million_input())
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

    let priced = priced_seven_day();
    assert_eq!(
        priced.cost.band_factor(SEVEN_DAY, SEPTEMBER_AFTERNOON),
        1.0,
        "the band names the 5-hour window alone"
    );
    let seven = priced
        .share(SEVEN_DAY, MAX_20X, "claude-opus-5-5", million_input())
        .unwrap();
    assert_eq!(
        seven * priced.cost.band_factor(SEVEN_DAY, SEPTEMBER_AFTERNOON),
        seven,
        "a 7-day charge in the band books the plain share"
    );
}

#[test]
fn the_share_rate_table_prices_a_dated_id_at_its_undated_row_and_an_unlisted_model_or_a_null_cell_at_no_rate(
) {
    let tables = PlanTables::shipped();
    let five = tables.rate_table(FIVE_HOUR);
    let haiku = per_million(five.rates_for("claude-haiku-4-5-20251001"));
    assert!(
        all_close(
            haiku,
            [
                Some(0.00659),
                Some(0.000167),
                Some(0.00824),
                Some(0.00823),
                Some(0.03294)
            ]
        ),
        "a dated id reads its undated row's cells: {haiku:?}"
    );
    for model in ["gpt-5", "claude-opus-4-8"] {
        assert_eq!(
            per_million(five.rates_for(model)),
            [None; 5],
            "a model the table does not list has no rate: {model}"
        );
    }
    assert_eq!(
        per_million(five.rates_for("claude-opus-5")),
        [None; 5],
        "a listed model whose cells are null has no rate"
    );
    let seven = tables.rate_table(SEVEN_DAY);
    for (model, rates) in &seven.rows {
        assert_eq!(per_million(*rates), [None; 5], "{model}");
    }
    assert_eq!(per_million(seven.fallback), [None; 5]);
    let priced = five.price("claude-fable-5-1", mixed()).unwrap();
    let share = tables
        .cost
        .reference_share(FIVE_HOUR, "claude-fable-5-1", mixed())
        .unwrap();
    assert!(
        close(priced, share),
        "the rate table prices a usage as reference_share does: {priced} against {share}"
    );
}
