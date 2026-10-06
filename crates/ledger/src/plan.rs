//! The subscription cost model, shipped beside the price table and read like it. A token costs a model weight times
//! a type multiplier in units on every window; a window holds a capacity in units per tier; the peak band is a
//! capacity factor per window, read at a request's completion; a model week is sized against the 7-day window. A
//! model or a tier left out, or set to `null`, prices nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use jiff::civil::Weekday;
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::Timestamp;
use phoebis_routing::{PinUsage, RateTable, Rates, FIVE_HOUR, SEVEN_DAY};

use crate::LedgerError;

const SHIPPED_SUBSCRIPTION_COSTS: &str = include_str!("../subscription_costs.yaml");

/// A number or `null`. Named on both members so the key must be present: a tier row that leaves a window out is
/// refused rather than read as unpriced.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    <Option<f64> as serde::Deserialize>::deserialize(d)
}

/// The units one tier's window holds, or `None` where the tier is unpriced on it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    #[serde(deserialize_with = "present")]
    pub five_hour: Option<f64>,
    #[serde(deserialize_with = "present")]
    pub seven_day: Option<f64>,
}

impl Capacity {
    fn on(&self, window: &str) -> Option<f64> {
        match window {
            FIVE_HOUR => self.five_hour,
            SEVEN_DAY => self.seven_day,
            _ => None,
        }
    }
}

/// One number for each shared window.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PerWindow {
    pub five_hour: f64,
    pub seven_day: f64,
}

impl PerWindow {
    fn on(&self, window: &str) -> Option<f64> {
        match window {
            FIVE_HOUR => Some(self.five_hour),
            SEVEN_DAY => Some(self.seven_day),
            _ => None,
        }
    }
}

/// The tier, window and band whose capacity the unit is a fraction of.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefinedBy {
    pub band: String,
    pub tier: String,
    pub window: String,
}

/// Documents the unit and anchors the file to itself: `units_per_window` is the capacity `defined_by` names.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    pub defined_by: DefinedBy,
    pub units_per_window: u64,
}

/// The peak band: the hours and weekdays it covers, read in `zone` (an IANA name) from `from_hour` up to but not
/// including `to_hour`, and the fraction of its capacity each window holds for a request completing inside it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Peak {
    pub capacity_factor: PerWindow,
    pub from_hour: u8,
    pub to_hour: u8,
    pub weekdays: bool,
    pub zone: String,
    /// Set by `from_yaml`, so a lookup never parses a name.
    #[serde(skip)]
    resolved: Option<TimeZone>,
}

impl Peak {
    /// The zone comes from the database bundled into the binary, so the hours never depend on the host's copy, or
    /// on the host having one.
    fn resolve(&mut self) -> Result<(), LedgerError> {
        let zone = TimeZoneDatabase::bundled()
            .get(&self.zone)
            .map_err(|e| LedgerError::Plan(format!("peak zone `{}`: {e}", self.zone)))?;
        self.resolved = Some(zone);
        Ok(())
    }

    fn covers(&self, at: i64) -> bool {
        let Some(zone) = &self.resolved else {
            return false;
        };
        let Ok(instant) = Timestamp::from_millisecond(at) else {
            return false;
        };
        let local = instant.to_zoned(zone.clone());
        if self.weekdays && matches!(local.weekday(), Weekday::Saturday | Weekday::Sunday) {
            return false;
        }
        let hour = i16::from(local.hour());
        i16::from(self.from_hour) <= hour && hour < i16::from(self.to_hour)
    }
}

/// The units a token of each type costs per unit of model weight, shared by every model and both windows.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypeMultiplier {
    pub cache_read: f64,
    pub cache_write_1h: f64,
    pub cache_write_5m: f64,
    pub input: f64,
    pub output: f64,
}

/// The multipliers one model sets in place of the shared ones; a type left out keeps the shared multiplier.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypeOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
}

impl TypeOverride {
    fn named(&self) -> [(&'static str, Option<f64>); 5] {
        [
            ("cache_read", self.cache_read),
            ("cache_write_1h", self.cache_write_1h),
            ("cache_write_5m", self.cache_write_5m),
            ("input", self.input),
            ("output", self.output),
        ]
    }
}

/// A window only some models carry, sized against the shared window `of`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelWindow {
    pub capacity_factor: f64,
    pub of: String,
}

/// A model that breaks a shared constant: its own multipliers, and its own weight on the windows it names.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_multiplier: Option<TypeOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_by_window: Option<IndexMap<String, f64>>,
}

/// The file as loaded, key for key and in the file's order; serializes back to the same keys. `from_yaml` is the one
/// loader that resolves the peak band's zone and checks the rules: a model deserialized any other way never covers the
/// band and is unchecked.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionCosts {
    pub effective: String,
    pub unit: Unit,
    /// Keyed by the tier the account's profile reports.
    pub capacity: IndexMap<String, Capacity>,
    pub peak: Peak,
    pub type_multiplier: TypeMultiplier,
    /// Units per plain input token, keyed by undated model id; `null` prices nothing.
    pub model_weight: IndexMap<String, Option<f64>>,
    pub model_windows: IndexMap<String, ModelWindow>,
    pub overrides: IndexMap<String, Override>,
}

/// The tier's row as the profile stored it, then with its `default_` prefix taken off or put on, since only one
/// tier's spelling has been seen on the wire.
fn tier_row<'a, T>(rows: &'a IndexMap<String, T>, tier: &str) -> Option<&'a T> {
    rows.get(tier)
        .or_else(|| tier.strip_prefix("default_").and_then(|t| rows.get(t)))
        .or_else(|| rows.get(&format!("default_{tier}")))
}

fn refuse(message: String) -> LedgerError {
    LedgerError::Plan(message)
}

/// A dated key would shadow the undated lookup for one release and vanish with the next.
fn undated_key(section: &str, key: &str) -> Result<(), LedgerError> {
    match phoebis_routing::listing::undated(key) {
        Some(_) => Err(refuse(format!(
            "{section}.{key}: a dated id; the file keys undated ids"
        ))),
        None => Ok(()),
    }
}

fn weight_rule(path: &str, weight: f64) -> Result<(), LedgerError> {
    if weight.is_finite() && weight > 0.0 {
        Ok(())
    } else {
        Err(refuse(format!("{path}: a weight is a number above zero")))
    }
}

fn multiplier_rule(path: &str, multiplier: f64) -> Result<(), LedgerError> {
    if multiplier.is_finite() && multiplier >= 0.0 {
        Ok(())
    } else {
        Err(refuse(format!(
            "{path}: a multiplier is a number at or above zero"
        )))
    }
}

/// A capacity factor holds a fraction of its window: above zero, and 1 at most.
fn factor_rule(path: &str, factor: f64) -> Result<(), LedgerError> {
    if factor.is_finite() && factor > 0.0 && factor <= 1.0 {
        Ok(())
    } else {
        Err(refuse(format!(
            "{path}: a factor is a number above zero and at most 1"
        )))
    }
}

impl SubscriptionCosts {
    pub fn shipped() -> SubscriptionCosts {
        SubscriptionCosts::from_yaml(SHIPPED_SUBSCRIPTION_COSTS)
            .expect("the shipped subscription cost model parses")
    }

    /// Refuses with `LedgerError::Plan` naming the key and the rule: serde's refusals (a parse error, an unknown or
    /// a missing key), a zone the bundled database lacks, then each value rule in the file's order.
    pub fn from_yaml(text: &str) -> Result<SubscriptionCosts, LedgerError> {
        // The typed maps keep the last of two equal keys, so the untyped parse runs first: it refuses the repeat,
        // naming the key with its line and column. The typed parse stays second so serde's field errors keep theirs.
        serde_norway::from_str::<serde_norway::Value>(text).map_err(|e| refuse(e.to_string()))?;
        let mut costs: SubscriptionCosts =
            serde_norway::from_str(text).map_err(|e| refuse(e.to_string()))?;
        costs.peak.resolve()?;
        costs.validate()?;
        Ok(costs)
    }

    fn validate(&self) -> Result<(), LedgerError> {
        for (tier, row) in &self.capacity {
            for (window, capacity) in [(FIVE_HOUR, row.five_hour), (SEVEN_DAY, row.seven_day)] {
                if capacity.is_some_and(|c| !c.is_finite() || c <= 0.0) {
                    return Err(refuse(format!(
                        "capacity.{tier}.{window}: a capacity is a number above zero, or null"
                    )));
                }
            }
        }
        let anchor = &self.unit.defined_by;
        // The router's burn rates and every tier's capacity ratio read the anchor on each window, so a null there
        // would leave the rates empty and the ratio `None` on a window a share still charges.
        if let Some(row) = self.capacity.get(&anchor.tier) {
            for (window, capacity) in [(FIVE_HOUR, row.five_hour), (SEVEN_DAY, row.seven_day)] {
                if capacity.is_none() {
                    return Err(refuse(format!(
                        "capacity.{}.{window}: the anchor tier carries a capacity on both windows",
                        anchor.tier
                    )));
                }
            }
        }
        if self.unit.units_per_window == 0 {
            return Err(refuse(
                "unit.units_per_window: a count of units above zero".to_string(),
            ));
        }
        let Some(row) = self.capacity.get(&anchor.tier) else {
            return Err(refuse(format!(
                "unit.defined_by.tier: `{}` is not a capacity row",
                anchor.tier
            )));
        };
        let Some(anchored) = row.on(&anchor.window) else {
            return Err(refuse(format!(
                "unit.defined_by.window: `{}` is neither {FIVE_HOUR} nor {SEVEN_DAY}",
                anchor.window
            )));
        };
        if anchor.band != "out_of_band" {
            return Err(refuse(format!(
                "unit.defined_by.band: `{}` is not out_of_band",
                anchor.band
            )));
        }
        if anchored != self.unit.units_per_window as f64 {
            return Err(refuse(format!(
                "unit.units_per_window: {} differs from capacity.{}.{}",
                self.unit.units_per_window, anchor.tier, anchor.window
            )));
        }
        let peak = &self.peak;
        for (name, hour) in [("from_hour", peak.from_hour), ("to_hour", peak.to_hour)] {
            if hour > 24 {
                return Err(refuse(format!("peak.{name}: an hour is 0 to 24")));
            }
        }
        if peak.from_hour >= peak.to_hour {
            return Err(refuse("peak.from_hour: below to_hour".to_string()));
        }
        let factors = &peak.capacity_factor;
        for (window, factor) in [
            (FIVE_HOUR, factors.five_hour),
            (SEVEN_DAY, factors.seven_day),
        ] {
            factor_rule(&format!("peak.capacity_factor.{window}"), factor)?;
        }
        let m = &self.type_multiplier;
        for (name, multiplier) in [
            ("cache_read", m.cache_read),
            ("cache_write_1h", m.cache_write_1h),
            ("cache_write_5m", m.cache_write_5m),
            ("input", m.input),
            ("output", m.output),
        ] {
            multiplier_rule(&format!("type_multiplier.{name}"), multiplier)?;
        }
        for (model, weight) in &self.model_weight {
            undated_key("model_weight", model)?;
            if weight.is_some_and(|w| !w.is_finite() || w <= 0.0) {
                return Err(refuse(format!(
                    "model_weight.{model}: a weight is a number above zero, or null"
                )));
            }
        }
        for (window, row) in &self.model_windows {
            if !window.starts_with("seven_day_") {
                return Err(refuse(format!(
                    "model_windows.{window}: a model week starts with seven_day_"
                )));
            }
            if row.of != SEVEN_DAY {
                return Err(refuse(format!("model_windows.{window}.of: {SEVEN_DAY}")));
            }
            factor_rule(
                &format!("model_windows.{window}.capacity_factor"),
                row.capacity_factor,
            )?;
        }
        for (model, row) in &self.overrides {
            undated_key("overrides", model)?;
            if !self.model_weight.contains_key(model) {
                return Err(refuse(format!("overrides.{model}: no model_weight row")));
            }
            for (name, multiplier) in row.type_multiplier.iter().flat_map(TypeOverride::named) {
                if let Some(multiplier) = multiplier {
                    multiplier_rule(
                        &format!("overrides.{model}.type_multiplier.{name}"),
                        multiplier,
                    )?;
                }
            }
            for (window, weight) in row.weight_by_window.iter().flatten() {
                let path = format!("overrides.{model}.weight_by_window.{window}");
                if !matches!(window.as_str(), FIVE_HOUR | SEVEN_DAY)
                    && !self.model_windows.contains_key(window)
                {
                    return Err(refuse(format!(
                        "{path}: neither a shared window nor a model_windows key"
                    )));
                }
                weight_rule(&path, *weight)?;
            }
        }
        Ok(())
    }

    /// The shared window `window` prices on and the model-week divisor: the two shared windows price on themselves;
    /// a `model_windows` key on its `of` window over its `capacity_factor`; any other window starting with
    /// `seven_day` as the 7-day share once, as the classifier's window claims allow; anything else prices nothing.
    fn priced_window(&self, window: &str) -> Option<(&str, f64)> {
        match window {
            // The constants, never `window`: the elided result lifetime is `self`'s, which the argument does not
            // outlive.
            FIVE_HOUR => Some((FIVE_HOUR, 1.0)),
            SEVEN_DAY => Some((SEVEN_DAY, 1.0)),
            w => match self.model_windows.get(w) {
                Some(row) => Some((row.of.as_str(), row.capacity_factor)),
                None if w.starts_with(SEVEN_DAY) => Some((SEVEN_DAY, 1.0)),
                None => None,
            },
        }
    }

    fn override_of(&self, canonical_model: &str) -> Option<&Override> {
        phoebis_routing::listing::row_for(canonical_model, |k| self.overrides.get(k))
    }

    /// `overrides[model].weight_by_window[window]` when that row names `window` as sent, else
    /// `model_weight[model]`; the model by its canonical id, then its undated base. `None` is unpriced.
    fn weight(&self, canonical_model: &str, window: &str) -> Option<f64> {
        self.override_of(canonical_model)
            .and_then(|row| row.weight_by_window.as_ref()?.get(window).copied())
            .or_else(|| {
                *phoebis_routing::listing::row_for(canonical_model, |k| self.model_weight.get(k))?
            })
    }

    /// `overrides[model].type_multiplier` over the shared `type_multiplier`, in the order cache read, 1-hour write,
    /// 5-minute write, input, output.
    fn multipliers(&self, canonical_model: &str) -> [f64; 5] {
        let shared = &self.type_multiplier;
        let own = self
            .override_of(canonical_model)
            .and_then(|row| row.type_multiplier.as_ref());
        let pick =
            |base: f64, read: fn(&TypeOverride) -> Option<f64>| own.and_then(read).unwrap_or(base);
        [
            pick(shared.cache_read, |t| t.cache_read),
            pick(shared.cache_write_1h, |t| t.cache_write_1h),
            pick(shared.cache_write_5m, |t| t.cache_write_5m),
            pick(shared.input, |t| t.input),
            pick(shared.output, |t| t.output),
        ]
    }

    /// `peak.capacity_factor[priced]` when `peak` covers `at`, else 1.
    fn factor_at(&self, priced: &str, at: i64) -> f64 {
        if !self.peak.covers(at) {
            return 1.0;
        }
        self.peak.capacity_factor.on(priced).unwrap_or(1.0)
    }

    fn anchor_capacity(&self, priced: &str) -> Option<f64> {
        self.capacity.get(&self.unit.defined_by.tier)?.on(priced)
    }

    /// The share of the tier's own `window` `usage` consumes for a request completing at `at`: its units over the
    /// tier's capacity on the priced window, over the band's capacity factor there and the model week's divisor.
    pub fn share_at(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        let (priced, _) = self.priced_window(window)?;
        self.share_with(
            window,
            tier,
            canonical_model,
            usage,
            self.factor_at(priced, at),
        )
    }

    fn share_with(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        factor: f64,
    ) -> Option<f64> {
        let (priced, divisor) = self.priced_window(window)?;
        let weight = self.weight(canonical_model, window)?;
        let capacity = tier_row(&self.capacity, tier)?.on(priced)?;
        let [cache_read, write_1h, write_5m, input, output] = self.multipliers(canonical_model);
        let units = usage.input as f64 * weight * input
            + usage.cache_read as f64 * weight * cache_read
            + usage.write_5m as f64 * weight * write_5m
            + usage.write_1h as f64 * weight * write_1h
            + usage.output as f64 * weight * output;
        Some(units / capacity / factor / divisor)
    }

    /// The weight the band puts on a charge landing on `window` at `at`: the reciprocal of the priced window's
    /// capacity factor inside the band, else 1.
    pub fn band_factor(&self, window: &str, at: i64) -> f64 {
        self.priced_window(window)
            .map_or(1.0, |(priced, _)| 1.0 / self.factor_at(priced, at))
    }

    /// The per-token share of the anchor tier's `window` out of band, a row per model with a weight on it; empty
    /// for a window that prices nothing.
    pub fn rate_table(&self, window: &str) -> RateTable {
        // A model the file does not price has no rate: a guessed burn could order a shed the meter never
        // charged, while an unpriced pin only ranks last and stays unmarked.
        let fallback = Rates {
            input: None,
            cache_read: None,
            write_5m: None,
            write_1h: None,
            output: None,
        };
        let Some((priced, divisor)) = self.priced_window(window) else {
            return RateTable {
                rows: HashMap::new(),
                fallback,
            };
        };
        let Some(capacity) = self.anchor_capacity(priced) else {
            return RateTable {
                rows: HashMap::new(),
                fallback,
            };
        };
        let rows = self
            .model_weight
            .keys()
            .filter_map(|model| {
                let weight = self.weight(model, window)?;
                let [cache_read, write_1h, write_5m, input, output] = self.multipliers(model);
                let rate = |multiplier: f64| Some(weight * multiplier / capacity / divisor);
                Some((
                    model.clone(),
                    Rates {
                        input: rate(input),
                        cache_read: rate(cache_read),
                        write_5m: rate(write_5m),
                        write_1h: rate(write_1h),
                        output: rate(output),
                    },
                ))
            })
            .collect();
        RateTable { rows, fallback }
    }

    /// The tier's capacity over the anchor's on the priced window: what a reader holding anchor rates divides by to
    /// read the share on the tier; `None` where either is unpriced.
    pub fn tier_factor(&self, tier: &str, window: &str) -> Option<f64> {
        let (priced, _) = self.priced_window(window)?;
        let capacity = tier_row(&self.capacity, tier)?.on(priced)?;
        Some(capacity / self.anchor_capacity(priced)?)
    }

    /// True for the two shared windows and every model week the file names; a week priced as the 7-day share for
    /// want of a row reads false.
    pub fn measured(&self, window: &str) -> bool {
        matches!(window, FIVE_HOUR | SEVEN_DAY) || self.model_windows.contains_key(window)
    }

    /// Whether `share_at` answers a share for any usage: the model has a weight on the window and the tier a
    /// capacity on the window it prices on.
    pub fn priced(&self, window: &str, tier: &str, canonical_model: &str) -> bool {
        let Some((priced, _)) = self.priced_window(window) else {
            return false;
        };
        self.weight(canonical_model, window).is_some()
            && tier_row(&self.capacity, tier).is_some_and(|row| row.on(priced).is_some())
    }
}

/// The model every plan-side pricer holds, with where it came from: `source` reads `shipped`, `file` or `test`,
/// and `path` the configured path, `None` when none is.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanTables {
    pub costs: SubscriptionCosts,
    pub path: Option<PathBuf>,
    pub source: String,
}

impl PlanTables {
    pub fn shipped() -> PlanTables {
        PlanTables {
            costs: SubscriptionCosts::shipped(),
            path: None,
            source: "shipped".to_string(),
        }
    }

    /// The operator's file when one is at the path, the shipped model when nothing is (`NotFound`), and the I/O
    /// error for any other failure to read it; `source` and `path` set to match.
    pub fn load(override_path: Option<&Path>) -> Result<PlanTables, LedgerError> {
        let text = match override_path {
            Some(path) => crate::pricing::read_override(path)?,
            None => None,
        };
        let path = override_path.map(Path::to_path_buf);
        Ok(match text {
            Some(text) => PlanTables {
                costs: SubscriptionCosts::from_yaml(&text)?,
                path,
                source: "file".to_string(),
            },
            None => PlanTables {
                costs: SubscriptionCosts::shipped(),
                path,
                source: "shipped".to_string(),
            },
        })
    }

    pub fn share_at(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        self.costs
            .share_at(window, tier, canonical_model, usage, at)
    }

    pub fn band_factor(&self, window: &str, at: i64) -> f64 {
        self.costs.band_factor(window, at)
    }

    pub fn rate_table(&self, window: &str) -> RateTable {
        self.costs.rate_table(window)
    }

    pub fn tier_factor(&self, tier: &str, window: &str) -> Option<f64> {
        self.costs.tier_factor(tier, window)
    }

    pub fn measured(&self, window: &str) -> bool {
        self.costs.measured(window)
    }

    pub fn priced(&self, window: &str, tier: &str, canonical_model: &str) -> bool {
        self.costs.priced(window, tier, canonical_model)
    }

    /// The share outside the band at any hour, for the readers that call it rather than `share_at`.
    pub fn share(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
    ) -> Option<f64> {
        self.costs
            .share_with(window, tier, canonical_model, usage, 1.0)
    }

    /// `share_at` on the 5-hour window, for the readers that call it rather than `share_at`.
    pub fn five_hour_share_at(
        &self,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        self.share_at(FIVE_HOUR, tier, canonical_model, usage, at)
    }
}
