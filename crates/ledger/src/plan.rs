//! The subscription cost model, shipped beside the price file and read like it: a list of revisions keyed by the date
//! each takes effect. A token costs a model weight times a type multiplier in units on every window, a window holds a
//! capacity in units per tier, the peak band is a capacity factor per window read at a request's completion, and a
//! model week is sized against the 7-day window. A model without a weight reads the latest weighted model of its model
//! tier, and a model of no weighted model tier, or a tier left out or set to `null`, prices nothing.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use jiff::civil::Weekday;
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::Timestamp;
use phoebis_routing::{window_for, BandSegment, PinUsage, RateTable, Rates, FIVE_HOUR, SEVEN_DAY};
use phoebis_store::PinBurn;

use crate::rollup::{BILLED_FUNDING, FAST_FUNDING, OVERAGE_FUNDING, PLAN_FUNDING};
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
    /// Whether the tier holds a capacity on either window: a tier null on both prices nothing.
    fn sizes(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }

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

    /// The band's spans inside `[from, to)`, in order: each local day the span touches, a weekend day skipped under
    /// `weekdays` as `covers` skips it, its `[from_hour, to_hour)` resolved in the zone and cut to the span. A local
    /// hour the zone skips or repeats resolves as `covers` reads it: the band opens at the first instant whose hour is
    /// `from_hour` or later. Empty where the zone did not resolve.
    fn edges(&self, from: i64, to: i64) -> Vec<(i64, i64)> {
        let mut spans = Vec::new();
        let Some(zone) = &self.resolved else {
            return spans;
        };
        let (Ok(start), Ok(end)) = (
            Timestamp::from_millisecond(from),
            Timestamp::from_millisecond(to),
        ) else {
            return spans;
        };
        let at_hour = |day: jiff::civil::Date, hour: u8| -> Option<i64> {
            // `to_hour` may be 24, the next day's midnight.
            let (day, hour) = if hour >= 24 {
                (day.tomorrow().ok()?, 0)
            } else {
                (day, hour)
            };
            let local = day.at(i8::try_from(hour).ok()?, 0, 0, 0);
            Some(
                local
                    .to_zoned(zone.clone())
                    .ok()?
                    .timestamp()
                    .as_millisecond(),
            )
        };
        let last = end.to_zoned(zone.clone()).date();
        let mut day = start.to_zoned(zone.clone()).date();
        while day <= last {
            let weekend = matches!(day.weekday(), Weekday::Saturday | Weekday::Sunday);
            if !(self.weekdays && weekend) {
                if let (Some(open), Some(close)) =
                    (at_hour(day, self.from_hour), at_hour(day, self.to_hour))
                {
                    let (lo, hi) = (open.max(from), close.min(to));
                    if lo < hi {
                        spans.push((lo, hi));
                    }
                }
            }
            let Ok(next) = day.tomorrow() else {
                break;
            };
            day = next;
        }
        spans
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

/// A regime: the entrypoints it prices and the capacity rows and band factor it sets in place of the base rows for
/// them. A tier an entry leaves out prices at the base row, and the unit stays anchored on the base regime.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Regime {
    pub entrypoints: Vec<String>,
    pub capacity: IndexMap<String, Capacity>,
    pub peak: RegimePeak,
}

/// The band factor a regime sets in place of the base one, read inside the base band's hours.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegimePeak {
    pub capacity_factor: PerWindow,
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

/// One revision of the file as loaded, key for key and in the file's order, and serializes back to the same keys.
/// `CostRevisions::from_yaml` is the one loader that resolves the peak band's zone and checks the rules: a model
/// deserialized any other way never covers the band and is unchecked.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionCosts {
    pub effective: String,
    pub unit: Unit,
    /// Keyed by the tier the account's profile reports.
    pub capacity: IndexMap<String, Capacity>,
    pub peak: Peak,
    pub type_multiplier: TypeMultiplier,
    /// Units per plain input token, keyed by undated model id. A `null` weight, like a model left out, prices at the
    /// weight of its model tier's latest weighted model as an estimate, and a model of no model tier, or of one with
    /// no weighted model, prices nothing.
    pub model_weight: IndexMap<String, Option<f64>>,
    pub model_windows: IndexMap<String, ModelWindow>,
    pub overrides: IndexMap<String, Override>,
    /// The model tiers in display order, the shipped value `[fable, opus, sonnet, haiku]`.
    pub tier_order: Vec<String>,
    /// Undated model id to a name of `tier_order`, naming every model either shipped file lists.
    pub model_tier: IndexMap<String, String>,
    /// Keyed by a name that says what the regime prices. A file without the map loads with none, and every
    /// entrypoint prices at the base rows.
    #[serde(default)]
    pub regimes: IndexMap<String, Regime>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CostFile {
    revisions: Vec<SubscriptionCosts>,
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
            "{section}.{key}: a dated id, where the file keys undated ids"
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

/// The capacity rule and the anchor rule over one set of tier rows, `path` the key the rows sit under, so the base
/// rows and each regime's read the same words.
fn capacity_rules(
    path: &str,
    rows: &IndexMap<String, Capacity>,
    anchor: &str,
) -> Result<(), LedgerError> {
    for (tier, row) in rows {
        for (window, capacity) in [(FIVE_HOUR, row.five_hour), (SEVEN_DAY, row.seven_day)] {
            if capacity.is_some_and(|c| !c.is_finite() || c <= 0.0) {
                return Err(refuse(format!(
                    "{path}.{tier}.{window}: a capacity is a number above zero, or null"
                )));
            }
        }
    }
    // The router's burn rates and every tier's capacity ratio read the anchor on each window, so a null there
    // would leave the rates empty and the ratio `None` on a window a share still charges.
    if let Some(row) = rows.get(anchor) {
        for (window, capacity) in [(FIVE_HOUR, row.five_hour), (SEVEN_DAY, row.seven_day)] {
            if capacity.is_none() {
                return Err(refuse(format!(
                    "{path}.{anchor}.{window}: the anchor tier carries a capacity on both windows"
                )));
            }
        }
    }
    Ok(())
}

/// The relay's `MAX_ID_LEN`, restated since the ledger cannot name the relay.
const ENTRYPOINT_MAX: usize = 256;

/// The entrypoint token alphabet: ASCII lowercase letters, ASCII digits, hyphens and underscores, 1 to
/// `ENTRYPOINT_MAX` (256, the relay's `MAX_ID_LEN`) bytes, which `regimes.{key}` and each entry of
/// `regimes.{key}.entrypoints` pass and the relay's `is_entrypoint_token` reads too. A digit is in it because
/// `claude-desktop-3p` carries one, and an underscore because the client spells `claude_in_slack`, `local_agent` and
/// the `remote_*` family with one. Every separator stays out, so no listed name can carry a second field.
fn bare_token(candidate: &str) -> bool {
    (1..=ENTRYPOINT_MAX).contains(&candidate.len())
        && candidate
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl SubscriptionCosts {
    /// The entry with its peak zone resolved and its rules checked, a refusal prefixed by the entry's date.
    fn resolved(mut self) -> Result<SubscriptionCosts, LedgerError> {
        let checked = self.peak.resolve().and_then(|()| self.validate());
        match checked {
            Ok(()) => Ok(self),
            Err(LedgerError::Plan(message)) => {
                Err(refuse(format!("revision {}: {message}", self.effective)))
            }
            Err(other) => Err(other),
        }
    }

    fn validate(&self) -> Result<(), LedgerError> {
        let anchor = &self.unit.defined_by;
        capacity_rules("capacity", &self.capacity, &anchor.tier)?;
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
        // One regime prices an entrypoint, so a token two entries list is refused rather than read by file order.
        let mut listed: HashMap<&str, &str> = HashMap::new();
        for (key, regime) in &self.regimes {
            if !bare_token(key) {
                return Err(refuse(format!(
                    "regimes.{key}: a bare token of lowercase letters, digits, hyphens and underscores"
                )));
            }
            if regime.entrypoints.is_empty() {
                return Err(refuse(format!("regimes.{key}.entrypoints: lists none")));
            }
            for name in &regime.entrypoints {
                if !bare_token(name) {
                    return Err(refuse(format!(
                        "regimes.{key}.entrypoints: {name} is not a bare token of lowercase letters, digits, \
                         hyphens and underscores"
                    )));
                }
                if let Some(other) = listed.insert(name, key) {
                    return Err(refuse(format!(
                        "regimes.{key}.entrypoints: {name} is listed by {other}"
                    )));
                }
            }
            if regime.capacity.is_empty() {
                return Err(refuse(format!("regimes.{key}: names no tier")));
            }
            for tier in regime.capacity.keys() {
                if !self.capacity.contains_key(tier) {
                    return Err(refuse(format!(
                        "regimes.{key}.capacity.{tier}: no capacity row"
                    )));
                }
            }
            // `rate_table` divides by the regime's own anchor row, so the anchor rule holds there too.
            capacity_rules(
                &format!("regimes.{key}.capacity"),
                &regime.capacity,
                &anchor.tier,
            )?;
            let factors = &regime.peak.capacity_factor;
            for (window, factor) in [
                (FIVE_HOUR, factors.five_hour),
                (SEVEN_DAY, factors.seven_day),
            ] {
                factor_rule(
                    &format!("regimes.{key}.peak.capacity_factor.{window}"),
                    factor,
                )?;
            }
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
        let tiers: HashSet<&str> = self.tier_order.iter().map(String::as_str).collect();
        if tiers.is_empty() || tiers.len() != self.tier_order.len() {
            return Err(refuse(
                "tier_order: a list of distinct tier names".to_string(),
            ));
        }
        for (model, tier) in &self.model_tier {
            undated_key("model_tier", model)?;
            if !tiers.contains(tier.as_str()) {
                return Err(refuse(format!(
                    "model_tier.{model}: {tier} is not in tier_order"
                )));
            }
        }
        // Every weighted model has a tier, so the fallback never guesses one for a model the file lists.
        for model in self.model_weight.keys().chain(self.overrides.keys()) {
            if !self.model_tier.contains_key(model) {
                return Err(refuse(format!("model_tier: no tier for {model}")));
            }
        }
        Ok(())
    }

    /// The regime whose `entrypoints` list the token, `None` for no token and for one no regime lists (the base).
    pub fn regime_of(&self, entrypoint: Option<&str>) -> Option<&Regime> {
        let token = entrypoint?;
        self.regimes
            .values()
            .find(|regime| regime.entrypoints.iter().any(|e| e == token))
    }

    /// The regime's row where it names the tier, else the base row through `tier_row`.
    pub fn capacity_of(&self, entrypoint: Option<&str>, tier: &str) -> Option<&Capacity> {
        self.regime_of(entrypoint)
            .and_then(|regime| tier_row(&regime.capacity, tier))
            .or_else(|| tier_row(&self.capacity, tier))
    }

    /// The `model_tier` entry for the id, else for its undated base, else the `tier_order` name that is the first
    /// segment of the undated id after `claude-` (`listing::tier_segment`), else `None`.
    pub fn tier_of(&self, canonical_model: &str) -> Option<&str> {
        if let Some(tier) =
            phoebis_routing::listing::row_for(canonical_model, |k| self.model_tier.get(k))
        {
            return Some(tier.as_str());
        }
        // A model can serve traffic before a release lists it, so its id names the tier until one does.
        let named = phoebis_routing::listing::tier_segment(canonical_model)?;
        self.tier_order
            .iter()
            .find(|tier| *tier == named)
            .map(String::as_str)
    }

    /// By the tier's position in `tier_order` (no tier last), then version descending (the numeric segments after
    /// the tier name, compared from the left), then by id. The file's own order is not consulted.
    pub fn model_order(&self, ids: &[&str]) -> Vec<String> {
        let mut keyed: Vec<_> = ids
            .iter()
            .map(|id| {
                let position = self
                    .tier_of(id)
                    .and_then(|tier| self.tier_order.iter().position(|t| t == tier))
                    .unwrap_or(self.tier_order.len());
                (position, Reverse(version_of(id)), id.to_string())
            })
            .collect();
        keyed.sort();
        keyed.into_iter().map(|(_, _, id)| id).collect()
    }

    /// The first model of `model_order` over the `model_weight` keys with a weight whose `tier_of` is `tier`.
    pub fn latest_weighted(&self, tier: &str) -> Option<&str> {
        let weighted: Vec<&str> = self
            .model_weight
            .iter()
            .filter(|(model, weight)| weight.is_some() && self.tier_of(model) == Some(tier))
            .map(|(model, _)| model.as_str())
            .collect();
        let first = self.model_order(&weighted).into_iter().next()?;
        self.model_weight
            .get_key_value(&first)
            .map(|(model, _)| model.as_str())
    }

    /// `(weight, false)` from the model's own weight on `window` (its override row first, then its `model_weight`),
    /// else `(weight, true)` from the base `model_weight` entry of `latest_weighted` of its tier, never from the
    /// donor's override row, else `None`. An override row applies to the model it names alone.
    pub fn weight_or_fallback(&self, canonical_model: &str, window: &str) -> Option<(f64, bool)> {
        if let Some(weight) = self.weight(canonical_model, window) {
            return Some((weight, false));
        }
        let donor = self.latest_weighted(self.tier_of(canonical_model)?)?;
        let weight = (*self.model_weight.get(donor)?)?;
        Some((weight, true))
    }

    /// The `model_tier` entries that `weighted_model_count` does not count (an override's window weight counts as a
    /// weight) and whose tier has a weighted model: what `check-config` prints beside the weighted count.
    pub fn by_tier_count(&self) -> usize {
        let weighted = self.weighted_models();
        self.model_tier
            .iter()
            .filter(|(model, tier)| {
                !weighted.contains(model.as_str()) && self.latest_weighted(tier).is_some()
            })
            .count()
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

    /// The regime's factor on `priced` where one lists the entrypoint, else the base, 1 outside the band.
    fn factor_at(&self, priced: &str, entrypoint: Option<&str>, at: i64) -> f64 {
        if !self.peak.covers(at) {
            return 1.0;
        }
        self.band_capacity_factor(priced, entrypoint)
    }

    /// The factor the band sets on `priced` under the regime the entrypoint names, else the base one.
    fn band_capacity_factor(&self, priced: &str, entrypoint: Option<&str>) -> f64 {
        let factors = self
            .regime_of(entrypoint)
            .map_or(&self.peak.capacity_factor, |r| &r.peak.capacity_factor);
        factors.on(priced).unwrap_or(1.0)
    }

    /// The anchor tier's capacity under the regime: the unit is the base window, and a regime's table divides by its
    /// own anchor row so an interactive pin ranks at the rate the meter charges it.
    fn anchor_capacity(&self, priced: &str, entrypoint: Option<&str>) -> Option<f64> {
        self.capacity_of(entrypoint, &self.unit.defined_by.tier)?
            .on(priced)
    }

    /// The share of the tier's own `window` `usage` consumes for a request completing at `at`: its units over the
    /// tier's capacity on the priced window, over the band's capacity factor there and the model week's divisor, the
    /// capacity and the factor read under the regime `entrypoint` names.
    pub fn share_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        self.share_and_flag_at(window, entrypoint, tier, canonical_model, usage, at)
            .map(|(share, _)| share)
    }

    /// `share_at` with the fallback's mark from the same weight lookup, so a reader that flags the share walks
    /// `latest_weighted` once.
    fn share_and_flag_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<(f64, bool)> {
        let (priced, divisor) = self.priced_window(window)?;
        let (weight, estimated) = self.weight_or_fallback(canonical_model, window)?;
        let capacity = self.capacity_of(entrypoint, tier)?.on(priced)?;
        let [cache_read, write_1h, write_5m, input, output] = self.multipliers(canonical_model);
        let units = usage.input as f64 * weight * input
            + usage.cache_read as f64 * weight * cache_read
            + usage.write_5m as f64 * weight * write_5m
            + usage.write_1h as f64 * weight * write_1h
            + usage.output as f64 * weight * output;
        Some((
            units / capacity / self.factor_at(priced, entrypoint, at) / divisor,
            estimated,
        ))
    }

    /// The share of the tier's own `window` a pin burning `burn` typed tokens a second consumes per second at `at`:
    /// `share_at` over a rate rather than a count, the same weight with its fallback, multipliers, capacity, capacity
    /// factor and model-week divisor. `None` where `share_at` answers none.
    pub fn share_rate_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        burn: &PinBurn,
        at: i64,
    ) -> Option<f64> {
        let (priced, divisor) = self.priced_window(window)?;
        let (weight, _) = self.weight_or_fallback(canonical_model, window)?;
        let capacity = self.capacity_of(entrypoint, tier)?.on(priced)?;
        let [cache_read, write_1h, write_5m, input, output] = self.multipliers(canonical_model);
        let units = burn.input * weight * input
            + burn.cache_read * weight * cache_read
            + burn.write_5m * weight * write_5m
            + burn.write_1h * weight * write_1h
            + burn.output * weight * output;
        Some(units / capacity / self.factor_at(priced, entrypoint, at) / divisor)
    }

    /// `share_at` under the anchor tier (`unit.defined_by.tier`): the cost model's units over the Max 20x capacity,
    /// the regime's own where `entrypoint` names one, since the equivalent is what a Max 20x account would spend.
    pub fn equivalent_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        self.share_at(
            window,
            entrypoint,
            &self.unit.defined_by.tier,
            canonical_model,
            usage,
            at,
        )
    }

    /// The weight the band puts on a charge landing on `window` at `at`: the reciprocal of the priced window's
    /// capacity factor inside the band, the regime's where `entrypoint` names one, else 1.
    pub fn band_factor(&self, window: &str, entrypoint: Option<&str>, at: i64) -> f64 {
        self.priced_window(window).map_or(1.0, |(priced, _)| {
            1.0 / self.factor_at(priced, entrypoint, at)
        })
    }

    /// `[from, to)` cut at the band's edges for `window`'s priced window under the regime the entrypoint names, each
    /// piece at one over the regime's capacity factor inside the band and 1 outside: the local days of the span are
    /// walked in the resolved zone, a weekend day skipped under `weekdays`, and `[from_hour, to_hour)` of each day
    /// intersected with the span, the hours read in local time as `covers` reads them, so a daylight-rule day follows
    /// the zone's rule. One piece at weight 1 where the factor is 1, the band covers nothing of the span, the window
    /// prices nothing, or the zone did not resolve. A list, so a file with two bands a day prices right. An empty span
    /// reads one piece at the band's weight at `from`, since a horizon clamped at its instant asks for one.
    pub fn band_segments(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        from: i64,
        to: i64,
    ) -> Vec<BandSegment> {
        let one = |weight: f64| vec![BandSegment { from, to, weight }];
        let Some((priced, _)) = self.priced_window(window) else {
            return one(1.0);
        };
        let factor = self.band_capacity_factor(priced, entrypoint);
        if factor == 1.0 {
            return one(1.0);
        }
        if from >= to {
            return one(1.0 / self.factor_at(priced, entrypoint, from));
        }
        let mut segments = Vec::new();
        let mut at = from;
        for (open, close) in self.peak.edges(from, to) {
            if at < open {
                segments.push(BandSegment {
                    from: at,
                    to: open,
                    weight: 1.0,
                });
            }
            segments.push(BandSegment {
                from: open,
                to: close,
                weight: 1.0 / factor,
            });
            at = close;
        }
        if at < to {
            segments.push(BandSegment {
                from: at,
                to,
                weight: 1.0,
            });
        }
        segments
    }

    /// The per-token share of the anchor tier's `window` out of band under the regime `entrypoint` names, empty for a
    /// window that prices nothing. A row per `model_weight` and `model_tier` key: at the model's weight on the window,
    /// else at the base weight of its model tier's `latest_weighted` (the estimate is the same model tier's measured
    /// weight, so a pin on an unweighted model ranks by it rather than last, as the console prices it), else with no
    /// rate. A `tiers` row per model tier with a weighted model, at the same base weight and the shared multipliers,
    /// prices an id no file lists by the model tier its id names, as `tier_of` reads it.
    pub fn rate_table(&self, window: &str, entrypoint: Option<&str>) -> RateTable {
        // An estimate is the model tier's measured weight, the one `share_at` charges the model at, so a shed it
        // orders is the one the meter charges. A model neither a row nor a model tier row names takes no rate, and
        // its pin ranks last.
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
                long_context: HashMap::new(),
                tiers: HashMap::new(),
                fallback,
            };
        };
        let Some(capacity) = self.anchor_capacity(priced, entrypoint) else {
            return RateTable {
                rows: HashMap::new(),
                long_context: HashMap::new(),
                tiers: HashMap::new(),
                fallback,
            };
        };
        let models: HashSet<&String> = self
            .model_weight
            .keys()
            .chain(self.model_tier.keys())
            .collect();
        let rates_at = |weight: f64, [cache_read, write_1h, write_5m, input, output]: [f64; 5]| {
            let rate = |multiplier: f64| Some(weight * multiplier / capacity / divisor);
            Rates {
                input: rate(input),
                cache_read: rate(cache_read),
                write_5m: rate(write_5m),
                write_1h: rate(write_1h),
                output: rate(output),
            }
        };
        // A listed model the fallback leaves unpriced keeps a row of no rates, so `rates_for` reads it before the
        // model tier its id names, as the meter reads the model tier the file lists it under.
        let rows = models
            .into_iter()
            .map(|model| {
                let rates = self
                    .weight_or_fallback(model, window)
                    .map_or(fallback, |(weight, _)| {
                        rates_at(weight, self.multipliers(model))
                    });
                (model.clone(), rates)
            })
            .collect();
        // An id no file lists has no override row, since every override names a listed model, so it reads the
        // shared multipliers and the donor's base weight, as `weight_or_fallback` does.
        let shared = &self.type_multiplier;
        let shared = [
            shared.cache_read,
            shared.cache_write_1h,
            shared.cache_write_5m,
            shared.input,
            shared.output,
        ];
        let tiers = self
            .tier_order
            .iter()
            .filter_map(|tier| {
                let donor = self.latest_weighted(tier)?;
                let weight = (*self.model_weight.get(donor)?)?;
                Some((tier.clone(), rates_at(weight, shared)))
            })
            .collect();
        RateTable {
            rows,
            long_context: HashMap::new(),
            tiers,
            fallback,
        }
    }

    /// The tier's capacity over the anchor's on the priced window, both under the regime `entrypoint` names: what a
    /// reader holding that regime's anchor rates divides by to read the share on the tier, `None` where either is
    /// unpriced.
    pub fn tier_factor(&self, tier: &str, window: &str, entrypoint: Option<&str>) -> Option<f64> {
        let (priced, _) = self.priced_window(window)?;
        let capacity = self.capacity_of(entrypoint, tier)?.on(priced)?;
        Some(capacity / self.anchor_capacity(priced, entrypoint)?)
    }

    /// True for the two shared windows and every model week the file names; a week priced as the 7-day share for
    /// want of a row reads false.
    pub fn measured(&self, window: &str) -> bool {
        matches!(window, FIVE_HOUR | SEVEN_DAY) || self.model_windows.contains_key(window)
    }

    /// Whether `share_at` answers a share for any usage: the model has a weight on the window and the tier a
    /// capacity on the window it prices on.
    pub fn priced(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
    ) -> bool {
        let Some((priced, _)) = self.priced_window(window) else {
            return false;
        };
        self.weight_or_fallback(canonical_model, window).is_some()
            && self
                .capacity_of(entrypoint, tier)
                .is_some_and(|row| row.on(priced).is_some())
    }

    /// The models some window prices, each counted once: a `model_weight` row with a weight, or an `overrides` row
    /// that weighs the model on a window of its own, which prices there even under a null shared weight. The count
    /// sits on the model, beside `weight`, so what `check-config` prints follows the rule the pricers read.
    pub fn weighted_model_count(&self) -> usize {
        self.weighted_models().len()
    }

    fn weighted_models(&self) -> HashSet<&str> {
        self.model_weight
            .iter()
            .filter(|(_, weight)| weight.is_some_and(|w| w > 0.0))
            .map(|(model, _)| model.as_str())
            .chain(
                self.overrides
                    .iter()
                    .filter(|(_, o)| o.weight_by_window.as_ref().is_some_and(|w| !w.is_empty()))
                    .map(|(model, _)| model.as_str()),
            )
            .collect()
    }

    /// Whether the account tier holds a capacity on either window under the entrypoint's regime: the regime's row
    /// where it names the tier, else the base row, as `share_at` reads it.
    pub(crate) fn sizes(&self, entrypoint: Option<&str>, tier: &str) -> bool {
        self.capacity_of(entrypoint, tier)
            .is_some_and(Capacity::sizes)
    }

    /// The tiers with a capacity on either window; a listed tier null on both prices nothing. The count sits on the
    /// model, beside `priced`, so what `check-config` prints follows the rule the pricers read.
    pub fn sized_tier_count(&self) -> usize {
        self.capacity.values().filter(|c| c.sizes()).count()
    }
}

/// The undated id, the id itself when it carries no date.
fn undated_id(canonical_model: &str) -> &str {
    phoebis_routing::listing::undated(canonical_model).unwrap_or(canonical_model)
}

/// The numeric segments of the undated id after its name segment (`claude-opus-4-8` reads `[4, 8]`).
fn version_of(canonical_model: &str) -> Vec<u32> {
    let rest = undated_id(canonical_model);
    let rest = rest.strip_prefix("claude-").unwrap_or(rest);
    let (_, version) = rest.split_once('-').unwrap_or((rest, ""));
    version.split('-').filter_map(|p| p.parse().ok()).collect()
}

/// The cost model's revisions sorted ascending by instant, with the instants precomputed at load so `in_force` is a
/// binary search. Serializes as the file's one key.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CostRevisions {
    revisions: Vec<SubscriptionCosts>,
    #[serde(skip)]
    instants: Vec<i64>,
}

// `new` refuses an empty list, so a list never reads empty.
#[allow(clippy::len_without_is_empty)]
impl CostRevisions {
    /// Sorts, refuses an empty list and two entries dated the same, and computes the instants.
    pub fn new(revisions: Vec<SubscriptionCosts>) -> Result<CostRevisions, LedgerError> {
        let dates: Vec<&str> = revisions.iter().map(|r| r.effective.as_str()).collect();
        let instants = crate::pricing::sorted_instants(&dates).map_err(refuse)?;
        let (instants, revisions) = crate::pricing::by_instant(instants, revisions);
        Ok(CostRevisions {
            revisions,
            instants,
        })
    }

    /// Refuses with `LedgerError::Plan` naming the key and the rule: the untyped pass's refusals (a parse error, a
    /// repeated key, a key that is not an untagged string), serde's (an unknown or a missing key), then per entry
    /// under its date a zone the bundled database lacks and each value rule in the file's order, then the list's.
    pub fn from_yaml(text: &str) -> Result<CostRevisions, LedgerError> {
        // The typed maps read a key as its text and keep the last of two equal keys, so the untyped pass runs first
        // and refuses a repeat, a tagged one included. The typed parse stays second so serde's field errors keep
        // their line and column.
        crate::keys::untagged_string_keys(text).map_err(refuse)?;
        let file: CostFile = serde_norway::from_str(text).map_err(|e| refuse(e.to_string()))?;
        let revisions = file
            .revisions
            .into_iter()
            .map(SubscriptionCosts::resolved)
            .collect::<Result<Vec<_>, _>>()?;
        CostRevisions::new(revisions)
    }

    pub fn shipped() -> CostRevisions {
        CostRevisions::from_yaml(SHIPPED_SUBSCRIPTION_COSTS)
            .expect("the shipped subscription cost model parses")
    }

    /// The last entry whose instant is at or before `at_ms`, else the first.
    pub fn in_force(&self, at_ms: i64) -> &SubscriptionCosts {
        &self.revisions[crate::pricing::in_force_index(&self.instants, at_ms)]
    }

    pub fn latest(&self) -> &SubscriptionCosts {
        &self.revisions[self.revisions.len() - 1]
    }

    /// The first entry dated after `now_ms` that `in_force` does not answer, held until its instant.
    pub fn pending(&self, now_ms: i64) -> Option<&SubscriptionCosts> {
        self.revisions
            .get(crate::pricing::pending_index(&self.instants, now_ms))
    }

    pub fn iter(&self) -> impl Iterator<Item = &SubscriptionCosts> {
        self.revisions.iter()
    }

    pub fn len(&self) -> usize {
        self.revisions.len()
    }
}

/// The model every plan-side pricer holds, with where it came from: `source` reads `shipped`, `file` or `test`,
/// and `path` the configured path, `None` when none is. Every accessor reads the revision in force at `at`.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanTables {
    pub revisions: CostRevisions,
    pub path: Option<PathBuf>,
    pub source: String,
}

impl PlanTables {
    pub fn shipped() -> PlanTables {
        PlanTables {
            revisions: CostRevisions::shipped(),
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
                revisions: CostRevisions::from_yaml(&text)?,
                path,
                source: "file".to_string(),
            },
            None => PlanTables {
                revisions: CostRevisions::shipped(),
                path,
                source: "shipped".to_string(),
            },
        })
    }

    pub fn share_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        self.revisions
            .in_force(at)
            .share_at(window, entrypoint, tier, canonical_model, usage, at)
    }

    pub fn share_rate_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        burn: &PinBurn,
        at: i64,
    ) -> Option<f64> {
        self.revisions.in_force(at).share_rate_at(
            window,
            entrypoint,
            tier,
            canonical_model,
            burn,
            at,
        )
    }

    pub fn equivalent_at(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        self.revisions
            .in_force(at)
            .equivalent_at(window, entrypoint, canonical_model, usage, at)
    }

    pub fn band_factor(&self, window: &str, entrypoint: Option<&str>, at: i64) -> f64 {
        self.revisions
            .in_force(at)
            .band_factor(window, entrypoint, at)
    }

    /// The segments of `[from, to)` under the revision in force at `from`.
    pub fn band_segments(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        from: i64,
        to: i64,
    ) -> Vec<BandSegment> {
        self.revisions
            .in_force(from)
            .band_segments(window, entrypoint, from, to)
    }

    pub fn rate_table(&self, window: &str, entrypoint: Option<&str>, at: i64) -> RateTable {
        self.revisions.in_force(at).rate_table(window, entrypoint)
    }

    /// The `window` tables of the revision in force at `at`, one per regime it names, each read through the regime's
    /// first listed entrypoint, with every listed entrypoint mapped to its regime, and the base table every other
    /// entrypoint and none fall back to.
    pub fn regime_rates(&self, window: &str, at: i64) -> phoebis_routing::RegimeRates {
        let costs = self.revisions.in_force(at);
        phoebis_routing::RegimeRates {
            base: costs.rate_table(window, None),
            regimes: costs
                .regimes
                .iter()
                .map(|(name, regime)| {
                    (
                        name.clone(),
                        costs.rate_table(window, regime.entrypoints.first().map(String::as_str)),
                    )
                })
                .collect(),
            entrypoints: costs
                .regimes
                .iter()
                .flat_map(|(name, regime)| {
                    regime
                        .entrypoints
                        .iter()
                        .map(move |entrypoint| (entrypoint.clone(), name.clone()))
                })
                .collect(),
        }
    }

    pub fn tier_factor(
        &self,
        tier: &str,
        window: &str,
        entrypoint: Option<&str>,
        at: i64,
    ) -> Option<f64> {
        self.revisions
            .in_force(at)
            .tier_factor(tier, window, entrypoint)
    }

    pub fn measured(&self, window: &str, at: i64) -> bool {
        self.revisions.in_force(at).measured(window)
    }

    pub fn priced(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        at: i64,
    ) -> bool {
        self.revisions
            .in_force(at)
            .priced(window, entrypoint, tier, canonical_model)
    }

    pub fn tier_of(&self, canonical_model: &str, at: i64) -> Option<&str> {
        self.revisions.in_force(at).tier_of(canonical_model)
    }

    pub fn latest_weighted(&self, tier: &str, at: i64) -> Option<&str> {
        self.revisions.in_force(at).latest_weighted(tier)
    }

    pub fn model_order(&self, ids: &[&str], at: i64) -> Vec<String> {
        self.revisions.in_force(at).model_order(ids)
    }
}

/// Who paid for an attempt. The account's kind decides first: an api_key account's answer is billed in dollars
/// whatever its speed or overage flag, and the rollup key files it under `billed` so every reader prices it so. On
/// an oauth account a fast answer draws usage credits, then an answer in overage is billed in dollars, else the
/// subscription's window paid. The console's `fundingOf` reads the same three facts in the same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Funding {
    Plan,
    Overage,
    Fast,
    ApiKey,
}

/// The funding of an attempt from its row's three facts: the account's kind, the answer's `speed` and the head's
/// `overage_in_use`, read in `Funding`'s order.
pub fn funding_of(
    account_kind: &str,
    overage_in_use: Option<bool>,
    speed: Option<&str>,
) -> Funding {
    match (account_kind, speed, overage_in_use) {
        ("api_key", _, _) => Funding::ApiKey,
        (_, Some("fast"), _) => Funding::Fast,
        (_, _, Some(true)) => Funding::Overage,
        _ => Funding::Plan,
    }
}

impl Funding {
    /// A rollup row's funding from its key's word alone: the key carries the served attempt's funding whole, so the
    /// kind the registry holds for the account now has no say, and a deleted or recreated account moves no row.
    pub fn of_rollup(funding: &str) -> Funding {
        match funding {
            BILLED_FUNDING => Funding::ApiKey,
            FAST_FUNDING => Funding::Fast,
            OVERAGE_FUNDING => Funding::Overage,
            _ => Funding::Plan,
        }
    }

    /// The word the rollup key files the funding under: `billed` for `ApiKey`, since the key names who paid where
    /// `as_str` names the account's kind for the request routes.
    pub fn rollup_word(self) -> &'static str {
        match self {
            Funding::Plan => PLAN_FUNDING,
            Funding::Overage => OVERAGE_FUNDING,
            Funding::Fast => FAST_FUNDING,
            Funding::ApiKey => BILLED_FUNDING,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Funding::Plan => "plan",
            Funding::Overage => "overage",
            Funding::Fast => "fast",
            Funding::ApiKey => "api_key",
        }
    }
}

/// The shares of an account's own windows one usage charges, keyed by the ledger's window ids in the order
/// `five_hour`, `seven_day`, then the model week `window_for(canonical_model)` names when it is one. Every window
/// the model applies to is a key: `Some(share)` where the tier and the model price it, `None` where the tier has no
/// capacity or the model no weight on it, so a reader tells a window that does not apply (absent) from one it
/// could not price (`None`), and a model week is never nulled by a model that has none.
pub type WindowShares = IndexMap<&'static str, Option<f64>>;

/// Shares with the fallback's mark per window: `estimated` holds the keys of `shares`, true where that window's
/// share priced at the weight of the model tier's latest weighted model. A model with a null weight and an override
/// weight on one window prices that window on its own weight and the others through the fallback.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct PricedShares {
    pub shares: WindowShares,
    pub estimated: IndexMap<&'static str, bool>,
}

impl PricedShares {
    /// Whether any window priced through the fallback, the flag a reader marking a whole figure reads.
    pub fn any_estimated(&self) -> bool {
        self.estimated.values().any(|estimated| *estimated)
    }
}

/// `share_at` under the account tier and the regime `entrypoint` names on the two shared windows and on
/// `window_for(canonical_model)` where that is a model week, each window flagged where it priced through the fallback.
/// `at` selects the cost revision (the request's receipt, what the dollars and the rollup select on) and `band_at` the
/// peak band inside that revision (the completion where a reader has one, else `at`). The revision never reads
/// `band_at`.
pub fn price_shares(
    tables: &PlanTables,
    entrypoint: Option<&str>,
    tier: &str,
    canonical_model: &str,
    usage: PinUsage,
    at: i64,
    band_at: i64,
) -> PricedShares {
    let costs = tables.revisions.in_force(at);
    priced_windows(canonical_model, |window| {
        costs.share_and_flag_at(window, entrypoint, tier, canonical_model, usage, band_at)
    })
}

/// The three equivalent columns (`five_hour`, `seven_day`, the model week where the model carries one) at the
/// revision in force at `at` under the regime `entrypoint` names, the band read at `band_at`, the weight through the
/// fallback.
pub fn price_equivalent(
    tables: &PlanTables,
    entrypoint: Option<&str>,
    canonical_model: &str,
    usage: PinUsage,
    at: i64,
    band_at: i64,
) -> PricedShares {
    let costs = tables.revisions.in_force(at);
    let anchor = &costs.unit.defined_by.tier;
    priced_windows(canonical_model, |window| {
        costs.share_and_flag_at(window, entrypoint, anchor, canonical_model, usage, band_at)
    })
}

/// `Some(token)` for the relay's `claude-code/<token>` label, `None` for `other`, `sdk/<lang>` and the rollups' `-`,
/// so a ledger row prices under the regime its own `client` column names, history included.
pub fn entrypoint_of_client(client: &str) -> Option<&str> {
    client.strip_prefix("claude-code/")
}

/// `share` on the two shared windows and on `window_for(canonical_model)` where that is a model week, each window
/// marked where it priced through the fallback.
fn priced_windows(
    canonical_model: &str,
    share: impl Fn(&'static str) -> Option<(f64, bool)>,
) -> PricedShares {
    let week = window_for(canonical_model);
    let mut priced = PricedShares::default();
    for window in [FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .chain((week != SEVEN_DAY).then_some(week))
    {
        let share = share(window);
        // A window that prices nothing carries no estimate, so a bucket the readers null is never marked.
        priced.shares.insert(window, share.map(|(share, _)| share));
        priced
            .estimated
            .insert(window, share.is_some_and(|(_, estimated)| estimated));
    }
    priced
}
