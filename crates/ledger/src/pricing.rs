//! List prices per million tokens, the estimate every attempt is stamped with. Every estimate is an estimate:
//! tier and batch modifiers are not known here, and subscription quota is the rate-limit windows, never this
//! number.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;

use crate::record::AttemptRecord;
use crate::rollup::CostSplit;
use crate::LedgerError;

const SHIPPED_API_PRICES: &str = include_str!("../api_prices.yaml");

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelPrice {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

/// Which cells price an answer: the ones its reported `usage.speed` selects. `Copy`, so a record that carries a
/// speed holds this rather than a `String`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Fast,
    Standard,
}

impl Speed {
    /// `fast` reads `Fast`; any other value, and none, reads `Standard`.
    pub fn of(reported: Option<&str>) -> Speed {
        match reported {
            Some("fast") => Speed::Fast,
            _ => Speed::Standard,
        }
    }
}

/// One model's row: the standard cells, and the fast cells on a model that serves fast mode.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelRow {
    #[serde(flatten)]
    pub standard: ModelPrice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<ModelPrice>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PriceTable {
    pub version: String,
    pub effective: String,
    pub source: String,
    pub models: IndexMap<String, ModelRow>,
}

#[derive(serde::Deserialize)]
struct PriceFile {
    effective: String,
    models: IndexMap<String, ModelRow>,
}

fn per_token(p: &ModelPrice) -> phoebis_routing::Rates {
    phoebis_routing::Rates {
        input: Some(p.input / 1_000_000.0),
        cache_read: Some(p.cache_read / 1_000_000.0),
        write_5m: Some(p.cache_write_5m / 1_000_000.0),
        write_1h: Some(p.cache_write_1h / 1_000_000.0),
        output: Some(p.output / 1_000_000.0),
    }
}

impl PriceTable {
    pub fn shipped() -> PriceTable {
        PriceTable::from_yaml(SHIPPED_API_PRICES, "shipped")
            .expect("the shipped price table parses")
    }

    /// Refuses a cell that is not a finite non-negative number: a negative or `NaN` price makes every estimate
    /// negative or `NaN`, which a budget then never trips on.
    pub fn from_yaml(text: &str, source: &str) -> Result<PriceTable, LedgerError> {
        // The typed map reads a key as its text and keeps the last of two equal keys, so the untyped pass runs
        // first and refuses a repeat, a tagged one included. The typed parse stays second so serde's field errors
        // keep their line and column.
        crate::keys::untagged_string_keys(text).map_err(LedgerError::Yaml)?;
        let file: PriceFile =
            serde_norway::from_str(text).map_err(|e| LedgerError::Yaml(e.to_string()))?;
        for (model, row) in &file.models {
            let blocks = [("", Some(&row.standard)), ("fast.", row.fast.as_ref())];
            for (prefix, cells) in blocks {
                let Some(p) = cells else { continue };
                let named = [
                    ("input", p.input),
                    ("cache_write_5m", p.cache_write_5m),
                    ("cache_write_1h", p.cache_write_1h),
                    ("cache_read", p.cache_read),
                    ("output", p.output),
                ];
                if let Some((cell, _)) = named.iter().find(|(_, v)| !v.is_finite() || *v < 0.0) {
                    return Err(LedgerError::Yaml(format!(
                        "model {model}, cell {prefix}{cell} must be a finite non-negative number"
                    )));
                }
            }
        }
        Ok(PriceTable {
            version: file.effective.clone(),
            effective: file.effective,
            source: source.into(),
            models: file.models,
        })
    }

    /// The row for the id, else the row for the id with a trailing `-YYYYMMDD` removed, with the key it sits
    /// under: a dated id the catalog lists prices as the undated id the table ships. `price` and `model_label`
    /// both read this, so a label never names another row than the one that priced the request.
    fn entry(&self, canonical_model: &str) -> Option<(&str, &ModelRow)> {
        self.models
            .get_key_value(canonical_model)
            .or_else(|| {
                phoebis_routing::listing::undated(canonical_model)
                    .and_then(|base| self.models.get_key_value(base))
            })
            .map(|(key, row)| (key.as_str(), row))
    }

    /// The standard cells of the row `entry` finds.
    pub fn price(&self, canonical_model: &str) -> Option<&ModelPrice> {
        self.entry(canonical_model).map(|(_, row)| &row.standard)
    }

    /// The cells `speed` selects: the `fast` block for a fast answer when the row has one, else the standard cells.
    pub fn price_at(&self, canonical_model: &str, speed: Speed) -> Option<&ModelPrice> {
        self.entry(canonical_model).map(|(_, row)| match speed {
            Speed::Fast => row.fast.as_ref().unwrap_or(&row.standard),
            Speed::Standard => &row.standard,
        })
    }

    /// The metrics label: a priced model reads as the table key that priced it, everything else is one bucket,
    /// so a caller cannot grow the label set by naming models.
    pub fn model_label(&self, canonical_model: &str) -> &str {
        self.entry(canonical_model).map_or("other", |(key, _)| key)
    }

    /// Dollars per token, the routing task's api_key burn unit. A model the table lacks prices at the dearest
    /// listed rate of each type, so a model the table has not learned never looks cheap to the shedding order.
    pub fn rate_table(&self) -> phoebis_routing::RateTable {
        let rows = self.per_token_rows(|row| &row.standard);
        let dearest = |of: fn(&phoebis_routing::Rates) -> Option<f64>| {
            rows.values().filter_map(of).fold(0.0, f64::max)
        };
        let fallback = phoebis_routing::Rates {
            input: Some(dearest(|r| r.input)),
            cache_read: Some(dearest(|r| r.cache_read)),
            write_5m: Some(dearest(|r| r.write_5m)),
            write_1h: Some(dearest(|r| r.write_1h)),
            output: Some(dearest(|r| r.output)),
        };
        phoebis_routing::RateTable { rows, fallback }
    }

    /// Dollars per token at the fast cells; a model without a `fast` block reads its standard cells, and the
    /// fallback is `rate_table`'s, so a weight of fast over standard reads 1 wherever no fast price is known.
    pub fn fast_rate_table(&self) -> phoebis_routing::RateTable {
        phoebis_routing::RateTable {
            rows: self.per_token_rows(|row| row.fast.as_ref().unwrap_or(&row.standard)),
            fallback: self.rate_table().fallback,
        }
    }

    fn per_token_rows(
        &self,
        cells: fn(&ModelRow) -> &ModelPrice,
    ) -> std::collections::HashMap<String, phoebis_routing::Rates> {
        self.models
            .iter()
            .map(|(model, row)| (model.clone(), per_token(cells(row))))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn estimate(
        &self,
        model: &str,
        speed: Speed,
        input: u64,
        cache_read: u64,
        write_5m: u64,
        write_1h: u64,
        output: u64,
    ) -> Option<(f64, CostSplit)> {
        let p = self.price_at(model, speed)?;
        let per_m = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        let split = CostSplit {
            input_usd: per_m(input, p.input),
            cache_read_usd: per_m(cache_read, p.cache_read),
            cache_write_usd: per_m(write_5m, p.cache_write_5m) + per_m(write_1h, p.cache_write_1h),
            output_usd: per_m(output, p.output),
        };
        Some((split.total(), split))
    }
}

/// The operator's file at `path`: `None` when nothing is there (`NotFound`), the I/O error for any other failure to
/// read it, so a path that exists and cannot be read refuses the start rather than selecting the shipped table, the
/// way `Path::exists` would by reading false whenever the metadata cannot be read.
pub(crate) fn read_override(path: &Path) -> Result<Option<String>, LedgerError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LedgerError::Io(e)),
    }
}

pub struct Pricing {
    table: Arc<PriceTable>,
    unknown: AtomicU64,
}

impl Pricing {
    pub fn new(table: PriceTable) -> Pricing {
        Pricing {
            table: Arc::new(table),
            unknown: AtomicU64::new(0),
        }
    }

    /// The operator's file when one is at the path, the shipped table when nothing is (`NotFound`), and the I/O
    /// error for any other failure to read it.
    pub fn load(override_path: Option<&Path>) -> Result<Pricing, LedgerError> {
        let text = match override_path {
            Some(path) => read_override(path)?,
            None => None,
        };
        Ok(Pricing::new(match text {
            Some(text) => PriceTable::from_yaml(&text, "file")?,
            None => PriceTable::shipped(),
        }))
    }

    pub fn current(&self) -> Arc<PriceTable> {
        self.table.clone()
    }

    pub fn unknown_models(&self) -> u64 {
        self.unknown.load(Ordering::Relaxed)
    }

    pub(crate) fn unknown_counter(&self) -> &AtomicU64 {
        &self.unknown
    }
}

pub fn price_attempts(
    table: &PriceTable,
    canonical_model: &str,
    attempts: &mut [AttemptRecord],
    unknown: &AtomicU64,
) -> CostSplit {
    let mut total = CostSplit::default();
    let mut counted = false;
    for a in attempts.iter_mut() {
        if !a.has_usage() {
            continue;
        }
        match table.estimate(
            canonical_model,
            Speed::of(a.speed.as_deref()),
            a.input_tokens.unwrap_or(0),
            a.cache_read_tokens.unwrap_or(0),
            a.cache_write_5m_tokens.unwrap_or(0),
            a.cache_write_1h_tokens.unwrap_or(0),
            a.output_tokens.unwrap_or(0),
        ) {
            Some((usd, split)) => {
                a.estimated_usd = Some(usd);
                a.pricing_version = Some(table.version.clone());
                a.input_usd = Some(split.input_usd);
                a.cache_read_usd = Some(split.cache_read_usd);
                a.cache_write_usd = Some(split.cache_write_usd);
                a.output_usd = Some(split.output_usd);
                total.input_usd += split.input_usd;
                total.cache_read_usd += split.cache_read_usd;
                total.cache_write_usd += split.cache_write_usd;
                total.output_usd += split.output_usd;
            }
            None => {
                a.estimated_usd = None;
                a.pricing_version = None;
                a.input_usd = None;
                a.cache_read_usd = None;
                a.cache_write_usd = None;
                a.output_usd = None;
                if !counted {
                    unknown.fetch_add(1, Ordering::Relaxed);
                    counted = true;
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::AttemptRecord;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The refusal `from_yaml` answers for a file whose one model carries `standard` and, when given, `fast` cells.
    fn refusal(standard: &str, fast: Option<&str>) -> String {
        let fast = fast.map(|f| format!(", fast: {{{f}}}")).unwrap_or_default();
        PriceTable::from_yaml(
            &format!("effective: 2026-10-04\nmodels:\n  m: {{{standard}{fast}}}\n"),
            "file",
        )
        .unwrap_err()
        .to_string()
    }

    const CELLS: &str = "input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1";

    /// A negative price makes every estimate negative, which a budget never trips on.
    #[test]
    fn a_negative_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(&CELLS.replace("input: 1", "input: -1"), None),
            "pricing table: model m, cell input must be a finite non-negative number"
        );
    }

    #[test]
    fn a_nan_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(&CELLS.replace("output: 1", "output: .nan"), None),
            "pricing table: model m, cell output must be a finite non-negative number"
        );
    }

    #[test]
    fn an_infinite_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(&CELLS.replace("cache_read: 1", "cache_read: .inf"), None),
            "pricing table: model m, cell cache_read must be a finite non-negative number"
        );
    }

    /// The fast block repeats the standard cells' names, so its cell is named under `fast.`.
    #[test]
    fn a_bad_fast_cell_is_refused_under_its_fast_name() {
        assert_eq!(
            refusal(
                CELLS,
                Some(&CELLS.replace("cache_write_1h: 1", "cache_write_1h: -.inf"))
            ),
            "pricing table: model m, cell fast.cache_write_1h must be a finite non-negative number"
        );
    }

    /// A map that kept the last of two rows would price a model at a figure the file does not say.
    #[test]
    fn a_repeated_model_key_is_refused_by_name() {
        let text = crate::test_tables::API_PRICES.replacen(
            "  claude-sonnet-5-5:",
            "  claude-haiku-4-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, output: 10}\n  \
             claude-sonnet-5-5:",
            1,
        );
        assert_ne!(text, crate::test_tables::API_PRICES);
        match PriceTable::from_yaml(&text, "file") {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.starts_with("pricing table: ") && message.contains("claude-haiku-4-5"),
                    "{message}"
                );
            }
            Ok(table) => panic!("a repeated model key loads: {:?}", table.models),
        }
    }

    /// The test table with one more row, tagged with `!price`, after its last.
    fn with_tagged_row(model: &str) -> String {
        format!(
            "{}  !price {model}: {{input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, \
             output: 10}}\n",
            crate::test_tables::API_PRICES
        )
    }

    /// The typed map reads a tagged key as its plain text, so a tagged repeat would load as the model's price.
    #[test]
    fn a_tagged_repeat_of_a_model_key_is_refused_by_name() {
        match PriceTable::from_yaml(&with_tagged_row("claude-haiku-4-5"), "file") {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.starts_with("pricing table: ")
                        && message.contains("duplicate")
                        && message.contains("claude-haiku-4-5"),
                    "{message}"
                );
            }
            Ok(table) => panic!("a tagged repeat loads: {:?}", table.models),
        }
    }

    #[test]
    fn a_tagged_model_key_is_refused_by_name() {
        match PriceTable::from_yaml(&with_tagged_row("claude-sonnet-4"), "file") {
            Err(error) => assert_eq!(
                error.to_string(),
                "pricing table: models.!price claude-sonnet-4: a key is an untagged string"
            ),
            Ok(table) => panic!("a tagged key loads: {:?}", table.models),
        }
    }

    #[test]
    fn the_shipped_table_parses_and_every_cell_is_a_finite_price_at_or_above_zero() {
        let table = PriceTable::shipped();
        assert_eq!(table.source, "shipped");
        assert!(!table.models.is_empty());
        let finite = |p: &ModelPrice| {
            [
                p.input,
                p.cache_write_5m,
                p.cache_write_1h,
                p.cache_read,
                p.output,
            ]
            .iter()
            .all(|c| c.is_finite() && *c >= 0.0)
        };
        for (model, row) in &table.models {
            assert!(finite(&row.standard), "{model}: {:?}", row.standard);
            assert!(
                row.fast.as_ref().is_none_or(finite),
                "{model}: {:?}",
                row.fast
            );
        }
        let models: Vec<&str> = table.models.keys().map(String::as_str).collect();
        assert!(
            crate::test_tables::in_family_order(&models),
            "the rows keep the family order: {models:?}"
        );
    }

    // The test table's Sonnet 5.5 row: input 3, cache read 0.3, 5-minute write 3.75, output 15 per million.
    #[test]
    fn an_estimate_is_the_list_price_per_million_tokens_of_each_type() {
        let table = crate::test_tables::api_prices();
        let (usd, split) = table
            .estimate(
                "claude-sonnet-5-5",
                Speed::Standard,
                1_000_000,
                500_000,
                100_000,
                0,
                10_000,
            )
            .unwrap();
        assert!(
            (split.input_usd - 3.0).abs() < 1e-9
                && (split.cache_read_usd - 0.15).abs() < 1e-9
                && (split.cache_write_usd - 0.375).abs() < 1e-9
                && (split.output_usd - 0.15).abs() < 1e-9,
            "{split:?}"
        );
        assert!((usd - 3.675).abs() < 1e-9, "{usd}");
        assert!(table
            .estimate("gpt-5", Speed::Standard, 1, 0, 0, 0, 1)
            .is_none());
        assert_eq!(
            (
                table.model_label("claude-opus-5"),
                table.model_label("gpt-5")
            ),
            ("claude-opus-5", "other")
        );
    }

    #[test]
    fn a_fast_answer_prices_at_the_fast_cells_and_a_model_without_them_at_the_standard_cells() {
        // The test table's Opus 5.5 input is 10 per million standard and 20 fast; Sonnet 5.5 has no fast cells.
        let table = crate::test_tables::api_prices();
        let input = |model: &str, speed: Speed| {
            table
                .estimate(model, speed, 1_000_000, 0, 0, 0, 0)
                .unwrap()
                .0
        };
        assert_eq!(input("claude-opus-5-5", Speed::Standard), 10.0);
        assert_eq!(input("claude-opus-5-5", Speed::Fast), 20.0);
        assert_eq!(
            input("claude-sonnet-5-5", Speed::Fast),
            3.0,
            "a model without fast cells prices a fast answer at its standard cells"
        );
        assert_eq!(
            (
                Speed::of(Some("fast")),
                Speed::of(Some("standard")),
                Speed::of(Some("turbo")),
                Speed::of(None)
            ),
            (
                Speed::Fast,
                Speed::Standard,
                Speed::Standard,
                Speed::Standard
            )
        );
        let (fast, standard) = (table.fast_rate_table(), table.rate_table());
        let one_input = phoebis_routing::PinUsage {
            input: 1,
            ..phoebis_routing::PinUsage::default()
        };
        assert_eq!(
            fast.price("claude-opus-5-5", one_input),
            Some(20.0 / 1_000_000.0)
        );
        assert_eq!(
            fast.price("claude-sonnet-5-5", one_input),
            standard.price("claude-sonnet-5-5", one_input),
            "the fast table prices a model without fast cells as the standard table does"
        );
        assert_eq!(
            fast.price("gpt-5", one_input),
            standard.price("gpt-5", one_input),
            "and an unlisted model at the standard table's fallback, so its weight reads 1"
        );
    }

    #[test]
    fn the_rate_table_prices_one_token_at_the_list_price_over_a_million() {
        // The test table: Sonnet 5.5 at 3, 0.3, 3.75, 6 and 15 per million; Haiku's input at 1; Opus 5.5 the
        // dearest row, at 10 input and 50 output.
        let rates = crate::test_tables::api_prices().rate_table();
        let one = |pick: fn(&mut phoebis_routing::PinUsage)| {
            let mut usage = phoebis_routing::PinUsage::default();
            pick(&mut usage);
            usage
        };
        let sonnet = |usage| rates.price("claude-sonnet-5-5", usage);
        assert_eq!(sonnet(one(|u| u.input = 1)), Some(3.0 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.cache_read = 1)), Some(0.3 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.write_5m = 1)), Some(3.75 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.write_1h = 1)), Some(6.0 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.output = 1)), Some(15.0 / 1_000_000.0));
        assert_eq!(
            rates.price("claude-haiku-4-5-20251001", one(|u| u.input = 1)),
            Some(1.0 / 1_000_000.0),
            "a dated row prices as itself"
        );
        assert_eq!(
            rates.price("gpt-5", one(|u| u.input = 1)),
            Some(10.0 / 1_000_000.0),
            "a model the table lacks prices at the dearest listed rate of each type"
        );
        assert_eq!(
            rates.price("gpt-5", one(|u| u.output = 1)),
            Some(50.0 / 1_000_000.0),
            "a model the table lacks prices at the dearest listed rate of each type"
        );
    }

    #[test]
    fn a_dated_id_prices_as_its_undated_base_when_the_table_lacks_the_dated_row() {
        let mut models = IndexMap::new();
        models.insert(
            "claude-sonnet-4-5".to_string(),
            ModelRow {
                standard: ModelPrice {
                    input: 3.0,
                    cache_write_5m: 3.75,
                    cache_write_1h: 6.0,
                    cache_read: 0.3,
                    output: 15.0,
                },
                fast: None,
            },
        );
        models.insert(
            "claude-haiku-4-5-20251001".to_string(),
            ModelRow {
                standard: ModelPrice {
                    input: 1.0,
                    cache_write_5m: 1.25,
                    cache_write_1h: 2.0,
                    cache_read: 0.1,
                    output: 5.0,
                },
                fast: None,
            },
        );
        let table = PriceTable {
            version: "test".into(),
            effective: "test".into(),
            source: "test".into(),
            models,
        };
        assert_eq!(
            table.price("claude-sonnet-4-5-20250929").unwrap().input,
            3.0,
            "the dated id prices as its undated base"
        );
        assert_eq!(
            table.price("claude-haiku-4-5-20251001").unwrap().cache_read,
            0.1,
            "a row of its own is used directly, never the fallback"
        );
        assert!(
            table.price("claude-x-2025").is_none(),
            "a four-digit suffix is not a date, so no fallback and no match"
        );
    }

    #[test]
    fn a_metric_label_names_the_table_key_that_priced_the_model() {
        let price = ModelRow {
            standard: ModelPrice {
                input: 1.0,
                cache_write_5m: 1.25,
                cache_write_1h: 2.0,
                cache_read: 0.1,
                output: 5.0,
            },
            fast: None,
        };
        let table = PriceTable {
            version: "test".into(),
            effective: "test".into(),
            source: "test".into(),
            models: IndexMap::from([
                ("claude-haiku-4-5-20251001".to_string(), price.clone()),
                ("claude-sonnet-4-5".to_string(), price),
            ]),
        };
        assert_eq!(
            table.model_label("claude-sonnet-4-5-20250929"),
            "claude-sonnet-4-5",
            "a dated id priced by its undated row labels as that row"
        );
        assert_eq!(
            table.model_label("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5-20251001",
            "a dated id with a row of its own labels as itself"
        );
        assert_eq!(table.model_label("claude-nope-20250101"), "other");
        assert_eq!(table.model_label("gpt-5"), "other");
    }

    #[test]
    fn attempts_are_stamped_with_the_version_and_an_unknown_model_is_counted_once() {
        let table = crate::test_tables::api_prices();
        // The test table's Haiku row: input 1 and output 5 per million.
        let want = (1_000.0 * 1.0 + 100.0 * 5.0) / 1_000_000.0;
        let unknown = AtomicU64::new(0);
        let mut attempts = vec![
            AttemptRecord {
                attempt: 1,
                ..Default::default()
            },
            AttemptRecord {
                attempt: 2,
                input_tokens: Some(1_000),
                output_tokens: Some(100),
                ..Default::default()
            },
        ];
        let split = price_attempts(&table, "claude-haiku-4-5-20251001", &mut attempts, &unknown);
        assert_eq!(
            (
                attempts[0].estimated_usd,
                attempts[0].pricing_version.as_deref()
            ),
            (None, None),
            "no usage, no estimate"
        );
        assert!((attempts[1].estimated_usd.unwrap() - want).abs() < 1e-12);
        assert_eq!(attempts[1].pricing_version.as_deref(), Some("2026-01-01"));
        assert!((split.total() - want).abs() < 1e-12);
        let split = price_attempts(&table, "gpt-5", &mut attempts, &unknown);
        assert_eq!(
            (
                attempts[1].estimated_usd,
                unknown.load(Ordering::SeqCst),
                split
            ),
            (None, 1, CostSplit::default())
        );
    }

    #[test]
    fn the_override_file_beats_the_shipped_table() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Pricing::load(Some(&dir.path().join("missing.yaml")))
                .unwrap()
                .current()
                .source,
            "shipped"
        );
        let path = dir.path().join("api_prices.yaml");
        std::fs::write(&path, "effective: 2026-10-01\nmodels:\n  claude-sonnet-5: {input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1}\n").unwrap();
        let from_file = Pricing::load(Some(&path)).unwrap().current();
        assert_eq!(
            (
                from_file.source.as_str(),
                from_file.version.as_str(),
                from_file.models.len()
            ),
            ("file", "2026-10-01", 1)
        );
    }

    /// Only a path with nothing at it falls back: any other failure to read the operator's file refuses the start,
    /// since a silent fallback would price every request at a table the operator meant to replace.
    #[test]
    fn a_path_that_cannot_be_read_refuses_the_load_rather_than_reading_the_shipped_table() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        assert!(
            matches!(Pricing::load(Some(dir.path())), Err(LedgerError::Io(_))),
            "a directory at the path"
        );
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("api_prices.yaml"), "not read").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let behind_a_locked_parent = Pricing::load(Some(&locked.join("api_prices.yaml")));
        // Restored before the assertion so the temp directory can still be cleaned up on either outcome.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(behind_a_locked_parent, Err(LedgerError::Io(_))),
            "a file whose parent grants no search permission"
        );
    }
}
