//! List prices per million tokens, the estimate every attempt is stamped with. Every estimate is an estimate:
//! tier and batch modifiers are not known here, and subscription quota is the rate-limit windows, never this
//! number.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::record::AttemptRecord;
use crate::rollup::CostSplit;
use crate::LedgerError;

const SHIPPED: &str = include_str!("../pricing.yaml");

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
    pub models: BTreeMap<String, ModelRow>,
}

#[derive(serde::Deserialize)]
struct PriceFile {
    effective: String,
    models: BTreeMap<String, ModelRow>,
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
        PriceTable::from_yaml(SHIPPED, "shipped").expect("the shipped price table parses")
    }

    pub fn from_yaml(text: &str, source: &str) -> Result<PriceTable, LedgerError> {
        let file: PriceFile =
            serde_norway::from_str(text).map_err(|e| LedgerError::Yaml(e.to_string()))?;
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

    /// The operator's file wins over the shipped table.
    pub fn load(override_path: Option<&Path>) -> Result<Pricing, LedgerError> {
        if let Some(path) = override_path.filter(|p| p.exists()) {
            return Ok(Pricing::new(PriceTable::from_yaml(
                &std::fs::read_to_string(path)?,
                "file",
            )?));
        }
        Ok(Pricing::new(PriceTable::shipped()))
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

    #[test]
    fn the_shipped_table_is_the_specs_and_an_estimate_is_list_price_per_million() {
        let table = PriceTable::shipped();
        assert_eq!(
            (
                table.version.as_str(),
                table.effective.as_str(),
                table.source.as_str()
            ),
            ("2026-09-30", "2026-09-30", "shipped")
        );
        assert_eq!(table.models.len(), 6);
        let cells = |p: &ModelPrice| {
            (
                p.input,
                p.cache_write_5m,
                p.cache_write_1h,
                p.cache_read,
                p.output,
            )
        };
        let row = |model: &str| table.models.get(model).unwrap();
        assert_eq!(
            cells(&row("claude-opus-5-5").standard),
            (4.0, 5.0, 8.0, 0.2, 20.0)
        );
        assert_eq!(
            row("claude-opus-5-5").fast.as_ref().map(cells),
            Some((8.0, 10.0, 16.0, 0.4, 40.0))
        );
        assert_eq!(
            row("claude-opus-5").fast.as_ref().map(cells),
            Some((10.0, 12.5, 20.0, 1.0, 50.0))
        );
        assert_eq!(
            cells(&row("claude-opus-4-8").standard),
            (5.0, 6.25, 10.0, 0.5, 25.0)
        );
        assert_eq!(
            row("claude-opus-4-8").fast.as_ref().map(cells),
            Some((10.0, 12.5, 20.0, 1.0, 50.0))
        );
        assert_eq!(row("claude-sonnet-5").fast, None);
        let sonnet = table.price("claude-sonnet-5").unwrap();
        assert_eq!(
            (
                sonnet.input,
                sonnet.cache_write_5m,
                sonnet.cache_write_1h,
                sonnet.cache_read,
                sonnet.output
            ),
            (2.0, 2.5, 4.0, 0.2, 10.0)
        );
        let (usd, split) = table
            .estimate(
                "claude-sonnet-5",
                Speed::Standard,
                1_000_000,
                500_000,
                100_000,
                0,
                10_000,
            )
            .unwrap();
        assert!(
            (split.input_usd - 2.0).abs() < 1e-9
                && (split.cache_read_usd - 0.1).abs() < 1e-9
                && (split.cache_write_usd - 0.25).abs() < 1e-9
                && (split.output_usd - 0.1).abs() < 1e-9
        );
        assert!((usd - 2.45).abs() < 1e-9);
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
        let table = PriceTable::shipped();
        let input = |model: &str, speed: Speed| {
            table
                .estimate(model, speed, 1_000_000, 0, 0, 0, 0)
                .unwrap()
                .0
        };
        assert_eq!(input("claude-opus-5-5", Speed::Standard), 4.0);
        assert_eq!(input("claude-opus-5-5", Speed::Fast), 8.0);
        assert_eq!(
            input("claude-sonnet-5", Speed::Fast),
            2.0,
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
            Some(8.0 / 1_000_000.0)
        );
        assert_eq!(
            fast.price("claude-sonnet-5", one_input),
            standard.price("claude-sonnet-5", one_input),
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
        let rates = PriceTable::shipped().rate_table();
        let one = |pick: fn(&mut phoebis_routing::PinUsage)| {
            let mut usage = phoebis_routing::PinUsage::default();
            pick(&mut usage);
            usage
        };
        let sonnet = |usage| rates.price("claude-sonnet-5", usage);
        assert_eq!(sonnet(one(|u| u.input = 1)), Some(2.0 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.cache_read = 1)), Some(0.2 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.write_5m = 1)), Some(2.5 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.write_1h = 1)), Some(4.0 / 1_000_000.0));
        assert_eq!(sonnet(one(|u| u.output = 1)), Some(10.0 / 1_000_000.0));
        assert_eq!(
            rates.price("claude-haiku-4-5-20251001", one(|u| u.input = 1)),
            Some(1.0 / 1_000_000.0),
            "a dated row prices as itself"
        );
        assert_eq!(
            rates.price("gpt-5", one(|u| u.output = 1)),
            Some(50.0 / 1_000_000.0),
            "a model the table lacks prices at the dearest listed rate of each type"
        );
    }

    #[test]
    fn a_dated_id_prices_as_its_undated_base_when_the_table_lacks_the_dated_row() {
        let mut models = BTreeMap::new();
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
            models: BTreeMap::from([
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
        let table = PriceTable::shipped();
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
        assert!((attempts[1].estimated_usd.unwrap() - 0.0015).abs() < 1e-12);
        assert_eq!(attempts[1].pricing_version.as_deref(), Some("2026-09-30"));
        assert!((split.total() - 0.0015).abs() < 1e-12);
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
        let path = dir.path().join("pricing.yaml");
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
}
