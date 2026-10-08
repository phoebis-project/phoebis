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

/// One model's row: the standard cells, the fast cells on a model that serves fast mode, and the long-context
/// cells on a model priced by prompt length.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelRow {
    #[serde(flatten)]
    pub standard: ModelPrice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<ModelPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContext>,
}

/// The prices over the threshold: an attempt whose prompt is longer than `over` tokens prices all five of its
/// counts at `cells`, the output among them.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LongContext {
    pub over: u64,
    #[serde(flatten)]
    pub cells: ModelPrice,
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

/// The prompt length an attempt is priced by: its uncached input, its cache read and its two cache writes
/// together, what the API counted as the request's input. The output, the thinking share of it and the tokens a
/// context edit cleared are not part of it.
pub fn prompt_tokens(input: u64, cache_read: u64, write_5m: u64, write_1h: u64) -> u64 {
    input + cache_read + write_5m + write_1h
}

impl PriceTable {
    pub fn shipped() -> PriceTable {
        PriceTable::from_yaml(SHIPPED_API_PRICES, "shipped")
            .expect("the shipped price table parses")
    }

    /// Refuses a cell that is not a finite non-negative number, and a `long_context.over` that is zero: a negative
    /// or `NaN` price makes every estimate negative or `NaN`, which a budget then never trips on, and a zero
    /// threshold would price every prompt at the long-context cells.
    pub fn from_yaml(text: &str, source: &str) -> Result<PriceTable, LedgerError> {
        // The typed map reads a key as its text and keeps the last of two equal keys, so the untyped pass runs
        // first and refuses a repeat, a tagged one included. The typed parse stays second so serde's field errors
        // keep their line and column.
        crate::keys::untagged_string_keys(text).map_err(LedgerError::Yaml)?;
        let file: PriceFile =
            serde_norway::from_str(text).map_err(|e| LedgerError::Yaml(e.to_string()))?;
        for (model, row) in &file.models {
            let blocks = [
                ("", Some(&row.standard)),
                ("fast.", row.fast.as_ref()),
                ("long_context.", row.long_context.as_ref().map(|l| &l.cells)),
            ];
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
            // A zero would price every prompt at the long-context cells, which is a table that should carry them
            // as its standard ones. Serde already refuses a negative or fractional count before this runs.
            if row.long_context.as_ref().is_some_and(|l| l.over == 0) {
                return Err(LedgerError::Yaml(format!(
                    "model {model}, cell long_context.over must be a whole number of tokens above zero"
                )));
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

    /// The cells `speed` and `prompt_tokens` select, in this order: the `fast` block for a fast answer on a row
    /// that has one, else the `long_context` cells when the row has the block and the prompt is longer than its
    /// `over`, else the standard cells.
    pub fn price_at(
        &self,
        canonical_model: &str,
        speed: Speed,
        prompt_tokens: u64,
    ) -> Option<&ModelPrice> {
        self.entry(canonical_model).map(|(_, row)| {
            match (speed, row.fast.as_ref(), row.long_context.as_ref()) {
                (Speed::Fast, Some(fast), _) => fast,
                (_, _, Some(long)) if prompt_tokens > long.over => &long.cells,
                _ => &row.standard,
            }
        })
    }

    /// Whether the row `entry` finds for `canonical_model` carries a `long_context` set: such a model's attempts
    /// cannot be re-priced from a period's token sums, which carry no per-attempt prompt.
    pub fn priced_by_prompt_length(&self, canonical_model: &str) -> bool {
        self.entry(canonical_model)
            .is_some_and(|(_, row)| row.long_context.is_some())
    }

    /// The metrics label: a priced model reads as the table key that priced it, everything else is one bucket,
    /// so a caller cannot grow the label set by naming models.
    pub fn model_label(&self, canonical_model: &str) -> &str {
        self.entry(canonical_model).map_or("other", |(key, _)| key)
    }

    /// Dollars per token, the routing task's api_key burn unit: the standard cells per row and the long-context
    /// cells of every row that carries them. A model the table lacks prices at the dearest listed standard rate of
    /// each type, so a model the table has not learned never looks cheap to the shedding order.
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
        let long_context = self
            .models
            .iter()
            .filter_map(|(model, row)| {
                let long = row.long_context.as_ref()?;
                Some((
                    model.clone(),
                    phoebis_routing::LongContextRates {
                        over: long.over,
                        rates: per_token(&long.cells),
                    },
                ))
            })
            .collect();
        phoebis_routing::RateTable {
            rows,
            long_context,
            fallback,
        }
    }

    /// Dollars per token at the fast cells, which a fast answer reads whatever its prompt, so a row with a `fast`
    /// block carries no long-context set here. A row without one reads its standard and long-context rates as
    /// `rate_table` carries them, the cells `price_at` selects for a fast answer on it, and the fallback is
    /// `rate_table`'s, so a weight of fast over the selected set reads 1 wherever no fast price is known.
    pub fn fast_rate_table(&self) -> phoebis_routing::RateTable {
        let dollars = self.rate_table();
        let long_context = dollars
            .long_context
            .into_iter()
            .filter(|(model, _)| self.models[model].fast.is_none())
            .collect();
        phoebis_routing::RateTable {
            rows: self.per_token_rows(|row| row.fast.as_ref().unwrap_or(&row.standard)),
            long_context,
            fallback: dollars.fallback,
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
        let p = self.price_at(
            model,
            speed,
            prompt_tokens(input, cache_read, write_5m, write_1h),
        )?;
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

    /// The refusal `from_yaml` answers for a file whose one model carries `standard` and, when given, `fast` cells
    /// and a `long_context` block.
    fn refusal(standard: &str, fast: Option<&str>, long_context: Option<&str>) -> String {
        let fast = fast.map(|f| format!(", fast: {{{f}}}")).unwrap_or_default();
        let long_context = long_context
            .map(|l| format!(", long_context: {{{l}}}"))
            .unwrap_or_default();
        PriceTable::from_yaml(
            &format!("effective: 2026-10-04\nmodels:\n  m: {{{standard}{fast}{long_context}}}\n"),
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
            refusal(&CELLS.replace("input: 1", "input: -1"), None, None),
            "pricing table: model m, cell input must be a finite non-negative number"
        );
    }

    #[test]
    fn a_nan_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(&CELLS.replace("output: 1", "output: .nan"), None, None),
            "pricing table: model m, cell output must be a finite non-negative number"
        );
    }

    #[test]
    fn an_infinite_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(
                &CELLS.replace("cache_read: 1", "cache_read: .inf"),
                None,
                None
            ),
            "pricing table: model m, cell cache_read must be a finite non-negative number"
        );
    }

    /// The fast block repeats the standard cells' names, so its cell is named under `fast.`.
    #[test]
    fn a_bad_fast_cell_is_refused_under_its_fast_name() {
        assert_eq!(
            refusal(
                CELLS,
                Some(&CELLS.replace("cache_write_1h: 1", "cache_write_1h: -.inf")),
                None,
            ),
            "pricing table: model m, cell fast.cache_write_1h must be a finite non-negative number"
        );
    }

    /// The long-context block repeats the standard cells' names, so its cell is named under `long_context.`.
    #[test]
    fn a_bad_long_context_cell_is_refused_under_its_long_context_name() {
        let block = format!("over: 1000, {}", CELLS.replace("input: 1", "input: -1"));
        assert_eq!(
            refusal(CELLS, None, Some(&block)),
            "pricing table: model m, cell long_context.input must be a finite non-negative number"
        );
    }

    /// A zero threshold would price every prompt at the long-context cells, which is a table that should carry
    /// them as its standard ones.
    #[test]
    fn a_zero_threshold_is_refused_naming_long_context_over() {
        assert_eq!(
            refusal(CELLS, None, Some(&format!("over: 0, {CELLS}"))),
            "pricing table: model m, cell long_context.over must be a whole number of tokens above zero"
        );
    }

    /// A negative or fractional count is no token count: serde refuses it under the field's path before the rule
    /// runs.
    #[test]
    fn a_negative_or_fractional_threshold_is_refused_under_its_path() {
        for over in ["-1", "1.5"] {
            let message = refusal(CELLS, None, Some(&format!("over: {over}, {CELLS}")));
            assert!(
                message.starts_with("pricing table: ")
                    && message.contains("long_context.over")
                    && message.contains("expected u64"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_long_context_block_missing_a_cell_is_refused_by_serde() {
        let message = refusal(
            CELLS,
            None,
            Some("over: 1000, input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1"),
        );
        assert!(
            message.starts_with("pricing table: ")
                && message.contains("long_context")
                && message.contains("missing field"),
            "{message}"
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
            assert!(
                row.long_context
                    .as_ref()
                    .is_none_or(|l| l.over > 0 && finite(&l.cells)),
                "{model}: {:?}",
                row.long_context
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
        let prompt =
            |u: &phoebis_routing::PinUsage| u.input + u.cache_read + u.write_5m + u.write_1h;
        assert_eq!(
            fast.price("claude-opus-5-5", one_input, prompt(&one_input)),
            Some(20.0 / 1_000_000.0)
        );
        assert_eq!(
            fast.price("claude-sonnet-5-5", one_input, prompt(&one_input)),
            standard.price("claude-sonnet-5-5", one_input, prompt(&one_input)),
            "the fast table prices a model without fast cells as the standard table does"
        );
        assert_eq!(
            fast.price("gpt-5", one_input, prompt(&one_input)),
            standard.price("gpt-5", one_input, prompt(&one_input)),
            "and an unlisted model at the standard table's fallback, so its weight reads 1"
        );
    }

    #[test]
    fn price_at_selects_the_fast_cells_then_the_long_context_cells_then_the_standard_ones() {
        // The test table's Haiku 5.5 row: input 1 up to 1,000 prompt tokens and 5 over them, no fast cells.
        let table = crate::test_tables::api_prices();
        let input = |speed, prompt| {
            table
                .price_at("claude-haiku-5-5", speed, prompt)
                .unwrap()
                .input
        };
        assert_eq!(
            input(Speed::Standard, 1_000),
            1.0,
            "a prompt of exactly `over` tokens prices at the standard cells"
        );
        assert_eq!(
            input(Speed::Standard, 1_001),
            5.0,
            "one token more prices at the long-context cells"
        );
        assert_eq!(
            input(Speed::Fast, 1_001),
            5.0,
            "a fast answer on a row without fast cells prices by its prompt"
        );
        assert_eq!(
            table
                .price_at("claude-sonnet-5-5", Speed::Standard, u64::MAX)
                .unwrap()
                .input,
            3.0,
            "a row without the block prices a prompt of any length at its standard cells"
        );
        // A row the test writes with both blocks: the fast cells win whatever the prompt.
        let both = PriceTable::from_yaml(
            "effective: 2026-01-01\nmodels:\n  m: {input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1, \
             fast: {input: 2, cache_write_5m: 2, cache_write_1h: 2, cache_read: 2, output: 2}, \
             long_context: {over: 10, input: 3, cache_write_5m: 3, cache_write_1h: 3, cache_read: 3, output: 3}}\n",
            "test",
        )
        .unwrap();
        assert_eq!(both.price_at("m", Speed::Fast, 11).unwrap().input, 2.0);
        assert_eq!(both.price_at("m", Speed::Standard, 11).unwrap().input, 3.0);
        assert_eq!(both.price_at("m", Speed::Standard, 10).unwrap().input, 1.0);
        let fast = both.fast_rate_table();
        assert_eq!(
            (
                fast.price(
                    "m",
                    phoebis_routing::PinUsage {
                        input: 1,
                        ..phoebis_routing::PinUsage::default()
                    },
                    11
                ),
                fast.long_context.is_empty()
            ),
            (Some(2.0 / 1_000_000.0), true),
            "the fast table keeps a row's fast cells with no long-context set beside them"
        );
    }

    #[test]
    fn an_estimate_prices_every_count_at_the_set_the_prompt_selects() {
        // The test table's Haiku 5.5 row, per million: 1, 1.25, 2, 0.1 and 5 up to 1,000 prompt tokens, and 5,
        // 6.25, 10, 0.5 and 25 over them. The prompt is the four input-side counts together, 250 each here.
        let table = crate::test_tables::api_prices();
        let at = |input| {
            table
                .estimate(
                    "claude-haiku-5-5",
                    Speed::Standard,
                    input,
                    250,
                    250,
                    250,
                    100,
                )
                .unwrap()
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let (usd, split) = at(250);
        assert!(
            close(split.input_usd, 250e-6)
                && close(split.cache_read_usd, 25e-6)
                && close(split.cache_write_usd, 812.5e-6)
                && close(split.output_usd, 500e-6),
            "{split:?}"
        );
        assert!(close(usd, 1_587.5e-6), "{usd}");
        let (usd, split) = at(251);
        assert!(
            close(split.input_usd, 1_255e-6)
                && close(split.cache_read_usd, 125e-6)
                && close(split.cache_write_usd, 4_062.5e-6)
                && close(split.output_usd, 2_500e-6),
            "one token over the threshold prices every count, the output included, at the long-context cells: \
             {split:?}"
        );
        assert!(close(usd, 7_942.5e-6), "{usd}");
        assert_eq!(prompt_tokens(1, 2, 3, 4), 10);
    }

    #[test]
    fn a_serialized_row_carries_long_context_only_when_the_file_did() {
        let table = crate::test_tables::api_prices();
        let haiku = serde_json::to_value(&table.models["claude-haiku-5-5"]).unwrap();
        assert_eq!(haiku["long_context"]["over"], 1_000);
        assert_eq!(haiku["long_context"]["output"], 25.0);
        assert!(serde_json::to_value(&table.models["claude-sonnet-5-5"])
            .unwrap()
            .get("long_context")
            .is_none());
    }

    #[test]
    fn the_rate_table_prices_one_token_at_the_list_price_over_a_million() {
        // The test table: Sonnet 5.5 at 3, 0.3, 3.75, 6 and 15 per million; Haiku 4.5's input at 1; Haiku 5.5's
        // input at 1 up to 1,000 prompt tokens and 5 over them, with no fast cells; Opus 5.5 the dearest row, at
        // 10 input and 50 output, its fast input at 20.
        let rates = crate::test_tables::api_prices().rate_table();
        let one = |pick: fn(&mut phoebis_routing::PinUsage)| {
            let mut usage = phoebis_routing::PinUsage::default();
            pick(&mut usage);
            usage
        };
        let prompt =
            |u: &phoebis_routing::PinUsage| u.input + u.cache_read + u.write_5m + u.write_1h;
        let priced = |model: &str, usage: phoebis_routing::PinUsage| {
            rates.price(model, usage, prompt(&usage))
        };
        let sonnet = |usage| priced("claude-sonnet-5-5", usage);
        assert_eq!(sonnet(one(|u| u.input = 1)), Some(3.0 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.cache_read = 1)), Some(0.3 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.write_5m = 1)), Some(3.75 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.write_1h = 1)), Some(6.0 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.output = 1)), Some(15.0 / 1_000_000.0));
        assert_eq!(
            priced("claude-haiku-4-5-20251001", one(|u| u.input = 1)),
            Some(1.0 / 1_000_000.0),
            "a dated row prices as itself"
        );
        let haiku = |prompt| rates.price("claude-haiku-5-5", one(|u| u.input = 1), prompt);
        assert_eq!(
            haiku(1_000),
            Some(1.0 / 1_000_000.0),
            "a prompt of exactly `over` prices at the standard cells"
        );
        assert_eq!(
            haiku(1_001),
            Some(5.0 / 1_000_000.0),
            "one token more at the long-context cells"
        );
        let fast = crate::test_tables::api_prices().fast_rate_table();
        assert_eq!(
            fast.price("claude-haiku-5-5", one(|u| u.input = 1), 1_001),
            Some(5.0 / 1_000_000.0),
            "a row without fast cells carries its long-context set in the fast table, so a fast answer on it \
             prices by its prompt as `price_at` does"
        );
        assert_eq!(
            fast.price("claude-opus-5-5", one(|u| u.input = 1), u64::MAX),
            Some(20.0 / 1_000_000.0),
            "a row with fast cells prices at them whatever the prompt"
        );
        assert_eq!(
            priced("gpt-5", one(|u| u.input = 1)),
            Some(10.0 / 1_000_000.0),
            "a model the table lacks prices at the dearest listed rate of each type"
        );
        assert_eq!(
            priced("gpt-5", one(|u| u.output = 1)),
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
                long_context: None,
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
                long_context: None,
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
            long_context: None,
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
        // The test table's Haiku 4.5 row: input 1 and output 5 per million.
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

    /// An attempt whose answer named no cache counts prices its prompt as its input alone.
    #[test]
    fn an_attempts_missing_counts_read_as_zero_in_its_prompt_length() {
        let table = crate::test_tables::api_prices();
        let unknown = AtomicU64::new(0);
        let attempt = |input| AttemptRecord {
            attempt: 1,
            input_tokens: Some(input),
            ..Default::default()
        };
        let mut under = vec![attempt(1_000)];
        let mut over = vec![attempt(1_001)];
        price_attempts(&table, "claude-haiku-5-5", &mut under, &unknown);
        price_attempts(&table, "claude-haiku-5-5", &mut over, &unknown);
        assert!(
            (under[0].input_usd.unwrap() - 1_000e-6).abs() < 1e-12,
            "{:?}",
            under[0].input_usd
        );
        assert!(
            (over[0].input_usd.unwrap() - 5_005e-6).abs() < 1e-12,
            "{:?}",
            over[0].input_usd
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

    /// A dated id reads its undated row, so it answers as that row does.
    #[test]
    fn a_model_is_priced_by_prompt_length_when_its_row_carries_the_block() {
        let table = crate::test_tables::api_prices();
        assert!(table.priced_by_prompt_length("claude-haiku-5-5"));
        assert!(table.priced_by_prompt_length("claude-haiku-5-5-20260101"));
        assert!(!table.priced_by_prompt_length("claude-sonnet-5-5"));
        assert!(!table.priced_by_prompt_length("gpt-5"));
    }
}
