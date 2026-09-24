//! List prices per million tokens, the estimate every attempt is stamped with, and the mapping of the two
//! public price catalogs onto the same shape. Every estimate is an estimate: tier and batch modifiers are not
//! known here, and subscription quota is the rate-limit windows, never this number.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use phoebis_store::{PricingVersion, Store};
use serde_json::Value;

use crate::record::AttemptRecord;
use crate::rollup::CostSplit;
use crate::LedgerError;

const SHIPPED: &str = include_str!("../pricing.yaml");

/// The list ratio between the one-hour and the five-minute cache write (2.0 over 1.25 times input), for a
/// catalog that prices only one of them.
const ONE_HOUR_OVER_FIVE_MINUTE: f64 = 1.6;

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelPrice {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PriceTable {
    pub version: String,
    pub effective: String,
    pub source: String,
    pub models: BTreeMap<String, ModelPrice>,
}

#[derive(serde::Deserialize)]
struct PriceFile {
    effective: String,
    models: BTreeMap<String, ModelPrice>,
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

    pub fn from_version(row: &PricingVersion) -> Result<PriceTable, LedgerError> {
        Ok(PriceTable {
            version: row.version.clone(),
            effective: row.effective.clone(),
            source: row.source.clone(),
            models: serde_json::from_str(&row.table_json)?,
        })
    }

    /// The row for the id, else the row for the id with a trailing `-YYYYMMDD` removed, with the key it sits
    /// under: a dated id the catalog lists prices as the undated id the table ships. `price` and `model_label`
    /// both read this, so a label never names another row than the one that priced the request.
    fn entry(&self, canonical_model: &str) -> Option<(&str, &ModelPrice)> {
        self.models
            .get_key_value(canonical_model)
            .or_else(|| {
                phoebis_routing::listing::undated(canonical_model)
                    .and_then(|base| self.models.get_key_value(base))
            })
            .map(|(key, price)| (key.as_str(), price))
    }

    pub fn price(&self, canonical_model: &str) -> Option<&ModelPrice> {
        self.entry(canonical_model).map(|(_, price)| price)
    }

    /// The metrics label: a priced model reads as the table key that priced it, everything else is one bucket,
    /// so a caller cannot grow the label set by naming models.
    pub fn model_label(&self, canonical_model: &str) -> &str {
        self.entry(canonical_model).map_or("other", |(key, _)| key)
    }

    pub fn estimate(
        &self,
        model: &str,
        input: u64,
        cache_read: u64,
        write_5m: u64,
        write_1h: u64,
        output: u64,
    ) -> Option<(f64, CostSplit)> {
        let p = self.price(model)?;
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
    table: RwLock<Arc<PriceTable>>,
    unknown: AtomicU64,
}

impl Pricing {
    pub fn new(table: PriceTable) -> Pricing {
        Pricing {
            table: RwLock::new(Arc::new(table)),
            unknown: AtomicU64::new(0),
        }
    }

    /// The newest confirmed import wins, then the operator's file, then the shipped table.
    pub fn load(store: &Store, override_path: Option<&Path>) -> Result<Pricing, LedgerError> {
        if let Some(row) = store
            .latest_pricing_version()
            .map_err(|e| LedgerError::Yaml(e.to_string()))?
        {
            return Ok(Pricing::new(PriceTable::from_version(&row)?));
        }
        if let Some(path) = override_path.filter(|p| p.exists()) {
            return Ok(Pricing::new(PriceTable::from_yaml(
                &std::fs::read_to_string(path)?,
                "file",
            )?));
        }
        Ok(Pricing::new(PriceTable::shipped()))
    }

    pub fn current(&self) -> Arc<PriceTable> {
        self.table
            .read()
            .expect("price table lock poisoned")
            .clone()
    }

    pub fn replace(&self, table: PriceTable) {
        *self.table.write().expect("price table lock poisoned") = Arc::new(table);
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
        let has_usage = a.input_tokens.is_some()
            || a.output_tokens.is_some()
            || a.cache_read_tokens.is_some()
            || a.cache_write_5m_tokens.is_some()
            || a.cache_write_1h_tokens.is_some();
        if !has_usage {
            continue;
        }
        match table.estimate(
            canonical_model,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFormat {
    LiteLlm,
    ModelsDev,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PriceChange {
    pub model: String,
    pub field: String,
    pub from: Option<f64>,
    pub to: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PriceDiff {
    pub in_file: usize,
    pub served: usize,
    pub models: BTreeMap<String, ModelPrice>,
    pub changes: Vec<PriceChange>,
}

pub fn parse_import(
    body: &[u8],
) -> Result<(ImportFormat, BTreeMap<String, ModelPrice>), LedgerError> {
    let root: Value = serde_json::from_slice(body)?;
    let Some(object) = root.as_object() else {
        return Err(LedgerError::Yaml(
            "the price file is not a JSON object".into(),
        ));
    };
    let litellm = object
        .values()
        .any(|v| v.get("input_cost_per_token").is_some());
    let models_dev = !litellm
        && object
            .values()
            .any(|v| v.get("models").is_some_and(Value::is_object));
    let f = |v: Option<&Value>| v.and_then(Value::as_f64);
    let mut out = BTreeMap::new();
    if litellm {
        for (key, entry) in object {
            let (Some(input), Some(output)) = (
                f(entry.get("input_cost_per_token")),
                f(entry.get("output_cost_per_token")),
            ) else {
                continue;
            };
            let write_5m = f(entry.get("cache_creation_input_token_cost")).unwrap_or(0.0);
            let write_1h = f(entry.get("cache_creation_input_token_cost_above_1hr"))
                .unwrap_or(write_5m * ONE_HOUR_OVER_FIVE_MINUTE);
            // `anthropic.claude-x` and `bedrock/claude-x` name the same model; the bare id is what the registry serves.
            let id = key.rsplit('/').next().unwrap_or(key);
            let id = id.strip_prefix("anthropic.").unwrap_or(id);
            out.insert(
                id.to_string(),
                ModelPrice {
                    input: input * 1e6,
                    cache_write_5m: write_5m * 1e6,
                    cache_write_1h: write_1h * 1e6,
                    cache_read: f(entry.get("cache_read_input_token_cost")).unwrap_or(0.0) * 1e6,
                    output: output * 1e6,
                },
            );
        }
        return Ok((ImportFormat::LiteLlm, out));
    }
    if models_dev {
        for provider in object.values() {
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            for (id, entry) in models {
                let Some(cost) = entry.get("cost") else {
                    continue;
                };
                let (Some(input), Some(output)) = (f(cost.get("input")), f(cost.get("output")))
                else {
                    continue;
                };
                let write_5m = f(cost.get("cache_write")).unwrap_or(0.0);
                out.entry(id.clone()).or_insert(ModelPrice {
                    input,
                    cache_write_5m: write_5m,
                    cache_write_1h: write_5m * ONE_HOUR_OVER_FIVE_MINUTE,
                    cache_read: f(cost.get("cache_read")).unwrap_or(0.0),
                    output,
                });
            }
        }
        return Ok((ImportFormat::ModelsDev, out));
    }
    Err(LedgerError::Yaml(
        "the price file is neither a LiteLLM nor a models.dev catalog".into(),
    ))
}

pub fn diff_against(
    current: &PriceTable,
    parsed: &BTreeMap<String, ModelPrice>,
    served: &[String],
) -> PriceDiff {
    let lower: BTreeMap<String, &ModelPrice> = parsed
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
    let mut models = BTreeMap::new();
    for id in served {
        if let Some(price) = lower.get(&id.to_ascii_lowercase()) {
            models.insert(id.to_ascii_lowercase(), (*price).clone());
        }
    }
    let mut changes = Vec::new();
    for (id, next) in &models {
        let now = current.price(id);
        let fields: [(&str, Option<f64>, f64); 5] = [
            ("input", now.map(|p| p.input), next.input),
            (
                "cache_write_5m",
                now.map(|p| p.cache_write_5m),
                next.cache_write_5m,
            ),
            (
                "cache_write_1h",
                now.map(|p| p.cache_write_1h),
                next.cache_write_1h,
            ),
            ("cache_read", now.map(|p| p.cache_read), next.cache_read),
            ("output", now.map(|p| p.output), next.output),
        ];
        for (field, from, to) in fields {
            if from != Some(to) {
                changes.push(PriceChange {
                    model: id.clone(),
                    field: field.into(),
                    from,
                    to,
                });
            }
        }
    }
    PriceDiff {
        in_file: parsed.len(),
        served: models.len(),
        models,
        changes,
    }
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
            ("2026-09-08", "2026-09-08", "shipped")
        );
        assert_eq!(table.models.len(), 4);
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
            .estimate("claude-sonnet-5", 1_000_000, 500_000, 100_000, 0, 10_000)
            .unwrap();
        assert!(
            (split.input_usd - 2.0).abs() < 1e-9
                && (split.cache_read_usd - 0.1).abs() < 1e-9
                && (split.cache_write_usd - 0.25).abs() < 1e-9
                && (split.output_usd - 0.1).abs() < 1e-9
        );
        assert!((usd - 2.45).abs() < 1e-9);
        assert!(table.estimate("gpt-5", 1, 0, 0, 0, 1).is_none());
        assert_eq!(
            (
                table.model_label("claude-opus-5"),
                table.model_label("gpt-5")
            ),
            ("claude-opus-5", "other")
        );
    }

    #[test]
    fn a_dated_id_prices_as_its_undated_base_when_the_table_lacks_the_dated_row() {
        let mut models = BTreeMap::new();
        models.insert(
            "claude-sonnet-4-5".to_string(),
            ModelPrice {
                input: 3.0,
                cache_write_5m: 3.75,
                cache_write_1h: 6.0,
                cache_read: 0.3,
                output: 15.0,
            },
        );
        models.insert(
            "claude-haiku-4-5-20251001".to_string(),
            ModelPrice {
                input: 1.0,
                cache_write_5m: 1.25,
                cache_write_1h: 2.0,
                cache_read: 0.1,
                output: 5.0,
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
        let price = ModelPrice {
            input: 1.0,
            cache_write_5m: 1.25,
            cache_write_1h: 2.0,
            cache_read: 0.1,
            output: 5.0,
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
        assert_eq!(attempts[1].pricing_version.as_deref(), Some("2026-09-08"));
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
    fn litellm_and_models_dev_files_map_to_per_million_prices_and_the_diff_is_filtered_to_served_models(
    ) {
        let litellm = br#"{"claude-sonnet-5":{"input_cost_per_token":3e-06,"cache_creation_input_token_cost":3.75e-06,"cache_creation_input_token_cost_above_1hr":6e-06,"cache_read_input_token_cost":3e-07,"output_cost_per_token":1.5e-05,"litellm_provider":"anthropic"},"anthropic.claude-opus-5":{"input_cost_per_token":5e-06,"cache_creation_input_token_cost":6.25e-06,"cache_read_input_token_cost":5e-07,"output_cost_per_token":2.5e-05},"gpt-5":{"input_cost_per_token":1e-06,"output_cost_per_token":8e-06},"sample_spec":{"max_tokens":1}}"#;
        let (format, parsed) = parse_import(litellm).unwrap();
        assert_eq!(format, ImportFormat::LiteLlm);
        assert_eq!(parsed.len(), 3, "an entry without a price is not a model");
        let sonnet = &parsed["claude-sonnet-5"];
        assert!(
            (sonnet.input - 3.0).abs() < 1e-9
                && (sonnet.cache_write_5m - 3.75).abs() < 1e-9
                && (sonnet.cache_write_1h - 6.0).abs() < 1e-9
                && (sonnet.cache_read - 0.3).abs() < 1e-9
                && (sonnet.output - 15.0).abs() < 1e-9
        );
        let opus = &parsed["claude-opus-5"];
        assert!(
            (opus.cache_write_1h - 10.0).abs() < 1e-9,
            "a missing 1h write is 1.6 times the 5m write"
        );
        assert!(
            (parsed["gpt-5"].cache_read - 0.0).abs() < 1e-9,
            "a model without cache prices has zero for them"
        );
        let models_dev = br#"{"anthropic":{"id":"anthropic","models":{"claude-sonnet-5":{"id":"claude-sonnet-5","cost":{"input":3,"output":15,"cache_read":0.3,"cache_write":3.75}},"free":{"id":"free"}}},"openai":{"id":"openai","models":{"gpt-5":{"cost":{"input":1.25,"output":10}}}}}"#;
        let (format, parsed_dev) = parse_import(models_dev).unwrap();
        assert_eq!(format, ImportFormat::ModelsDev);
        assert_eq!(parsed_dev.len(), 2);
        assert!((parsed_dev["claude-sonnet-5"].cache_write_1h - 6.0).abs() < 1e-9);
        assert!(parse_import(b"[]").is_err());
        let diff = diff_against(
            &PriceTable::shipped(),
            &parsed,
            &[
                "claude-sonnet-5".into(),
                "GPT-5".into(),
                "claude-fable-5-1".into(),
            ],
        );
        assert_eq!((diff.in_file, diff.served, diff.models.len()), (3, 2, 2));
        assert!(diff.models.contains_key("gpt-5") && diff.models.contains_key("claude-sonnet-5"));
        assert!(
            !diff.models.contains_key("claude-fable-5-1"),
            "a served model the file lacks keeps its price and is not a change"
        );
        let changed: Vec<(&str, &str)> = diff
            .changes
            .iter()
            .map(|c| (c.model.as_str(), c.field.as_str()))
            .collect();
        assert!(
            changed.contains(&("claude-sonnet-5", "input"))
                && changed.contains(&("gpt-5", "input"))
        );
        assert_eq!(
            diff.changes
                .iter()
                .find(|c| c.model == "gpt-5" && c.field == "input")
                .unwrap()
                .from,
            None
        );
        assert_eq!(
            diff.changes
                .iter()
                .find(|c| c.model == "claude-sonnet-5" && c.field == "input")
                .unwrap()
                .from,
            Some(2.0)
        );
        assert_eq!(
            diff.changes.len(),
            10,
            "five fields for two models, every one different from the current table"
        );
    }

    #[test]
    fn a_stored_version_beats_the_override_file_which_beats_the_shipped_table() {
        let store = phoebis_store::Store::open_in_memory(&phoebis_store::StorageKey::generate()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Pricing::load(&store, Some(&dir.path().join("missing.yaml")))
                .unwrap()
                .current()
                .source,
            "shipped"
        );
        let path = dir.path().join("pricing.yaml");
        std::fs::write(&path, "effective: 2026-10-01\nmodels:\n  claude-sonnet-5: {input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1}\n").unwrap();
        let from_file = Pricing::load(&store, Some(&path)).unwrap().current();
        assert_eq!(
            (
                from_file.source.as_str(),
                from_file.version.as_str(),
                from_file.models.len()
            ),
            ("file", "2026-10-01", 1)
        );
        store.insert_pricing_version(&phoebis_store::PricingVersion { version: "imported-5".into(), effective: "2026-10-02".into(), source: "https://example.test".into(), table_json: r#"{"claude-opus-5":{"input":5,"cache_write_5m":6.25,"cache_write_1h":10,"cache_read":0.5,"output":25}}"#.into(), imported_at: 5 }, "admin").unwrap();
        let stored = Pricing::load(&store, Some(&path)).unwrap().current();
        assert_eq!(
            (stored.version.as_str(), stored.source.as_str()),
            ("imported-5", "https://example.test")
        );
        assert!(
            stored.price("claude-opus-5").is_some() && stored.price("claude-sonnet-5").is_none()
        );
        let pricing = Pricing::new(PriceTable::shipped());
        pricing.replace(from_file.as_ref().clone());
        assert_eq!(pricing.current().version, "2026-10-01");
    }
}
