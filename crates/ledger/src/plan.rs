//! The cost table, shipped beside the price table and read like it: per window and model, the share of a Max 20x
//! window one million tokens of each type consume, the tier factors that turn it into a share of another tier's
//! window, and the peak band that weighs a 5-hour charge more; a cell or a factor left `null` prices nothing.

use std::collections::{BTreeMap, HashMap};

use phoebis_routing::{PinUsage, RateTable, Rates, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use jiff::civil::Weekday;
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::Timestamp;

use crate::LedgerError;

const SHIPPED_COST_TABLE: &str = include_str!("../cost_table.yaml");

/// A time band in which the meter weighs a charge more, read at the request's completion, where the charge lands;
/// `factor` 1.0 turns it off. The hours and weekdays are read in `zone` (an IANA name), from `from_hour` up to
/// but not including `to_hour` of one day, and `weekdays` keeps it to Monday through Friday there.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Band {
    pub weekdays: bool,
    pub from_hour: u8,
    pub to_hour: u8,
    pub factor: f64,
    pub windows: Vec<String>,
    pub zone: String,
    /// Set by the `from_yaml` of the table the row ships in, so a lookup never parses a name.
    #[serde(skip)]
    resolved: Option<Resolved>,
}

/// A band's zone, resolved once.
#[derive(Clone, Debug, PartialEq)]
struct Resolved {
    zone: TimeZone,
}

impl Band {
    fn names(&self, window: &str) -> bool {
        self.windows.iter().any(|w| w == window)
    }

    /// The zone comes from the database bundled into the binary, so the hours a row names never depend on the
    /// host's copy, or on the host having one.
    fn resolve(&self) -> Result<Resolved, LedgerError> {
        let zone = TimeZoneDatabase::bundled()
            .get(&self.zone)
            .map_err(|e| LedgerError::Plan(format!("band zone `{}`: {e}", self.zone)))?;
        Ok(Resolved { zone })
    }

    fn covers(&self, zone: &TimeZone, at: i64) -> bool {
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

/// Resolves every band once; refuses a zone the bundled time-zone database does not know, so a lookup never
/// meets one.
fn resolve_bands(bands: &mut [Band]) -> Result<(), LedgerError> {
    for band in bands {
        band.resolved = Some(band.resolve()?);
    }
    Ok(())
}

/// The row in force on `window`: the last in the file naming it. The one lookup every reader of the band goes
/// through; a row `resolve_bands` did not resolve is never in force.
fn in_force<'a>(bands: &'a [Band], window: &str) -> Option<(&'a Band, &'a Resolved)> {
    bands
        .iter()
        .filter(|band| band.names(window))
        .filter_map(|band| Some((band, band.resolved.as_ref()?)))
        .next_back()
}

/// The in-force row's factor when its hours cover `at`, else 1.
fn weight_at(bands: &[Band], window: &str, at: i64) -> f64 {
    in_force(bands, window)
        .filter(|(band, resolved)| band.covers(&resolved.zone, at))
        .map_or(1.0, |(band, _)| band.factor)
}

/// The tier's row as the profile stored it, then with its `default_` prefix taken off or put on, since only one
/// tier's spelling has been seen on the wire.
fn tier_row<'a, T>(rows: &'a BTreeMap<String, T>, tier: &str) -> Option<&'a T> {
    rows.get(tier)
        .or_else(|| tier.strip_prefix("default_").and_then(|t| rows.get(t)))
        .or_else(|| rows.get(&format!("default_{tier}")))
}

/// A number or `null`. Named on every cell and factor so the key must be present: a row that leaves a type out is
/// refused rather than read as unmeasured.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    <Option<f64> as serde::Deserialize>::deserialize(d)
}

/// A model's five cells on one window: the share of the reference window per million tokens, or `None`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCells {
    #[serde(deserialize_with = "present")]
    pub cache_read: Option<f64>,
    #[serde(deserialize_with = "present")]
    pub cache_write_1h: Option<f64>,
    #[serde(deserialize_with = "present")]
    pub cache_write_5m: Option<f64>,
    #[serde(deserialize_with = "present")]
    pub input: Option<f64>,
    #[serde(deserialize_with = "present")]
    pub output: Option<f64>,
}

impl ModelCells {
    /// The five cells with their file keys.
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

/// One tier's capacity of each window over the reference tier's, or `None`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Factors {
    #[serde(deserialize_with = "present")]
    pub five_hour: Option<f64>,
    #[serde(deserialize_with = "present")]
    pub seven_day: Option<f64>,
}

/// The cost table measured on Max 20x, the reference tier whose factors are 1: per window and model, the share of
/// that window per million tokens of each type; `tiers` sizes every other tier against it; `bands` is the peak band,
/// one measured factor applied at a request's completion on the windows its row names.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CostTable {
    pub bands: Vec<Band>,
    pub effective: String,
    /// Keyed by the tier the account's profile reports; the Fable-scoped week is half the 7-day window on every
    /// tier and needs no column.
    pub tiers: BTreeMap<String, Factors>,
    /// `five_hour` and `seven_day`, each keyed by undated model id.
    pub windows: BTreeMap<String, BTreeMap<String, ModelCells>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CostFile {
    bands: Vec<Band>,
    effective: String,
    tiers: BTreeMap<String, Factors>,
    windows: BTreeMap<String, BTreeMap<String, ModelCells>>,
}

impl CostTable {
    pub fn shipped() -> CostTable {
        CostTable::from_yaml(SHIPPED_COST_TABLE).expect("the shipped cost table parses")
    }

    /// Refuses a file with a window other than the two, a v1 field or a key left out (serde names it), a cell below
    /// zero, a factor at or below zero, or a band `resolve_bands` refuses.
    pub fn from_yaml(text: &str) -> Result<CostTable, LedgerError> {
        let mut file: CostFile =
            serde_norway::from_str(text).map_err(|e| LedgerError::Plan(e.to_string()))?;
        let windows: Vec<&str> = file.windows.keys().map(String::as_str).collect();
        if windows != [FIVE_HOUR, SEVEN_DAY] {
            return Err(LedgerError::Plan(format!(
                "cost table windows {windows:?}: exactly `{FIVE_HOUR}` and `{SEVEN_DAY}` are priced"
            )));
        }
        for (window, models) in &file.windows {
            for (model, cells) in models {
                for (name, cell) in cells.named() {
                    if cell.is_some_and(|c| !c.is_finite() || c < 0.0) {
                        return Err(LedgerError::Plan(format!(
                            "cost table cell {window}.{model}.{name}: a share is a number at or above zero, or null"
                        )));
                    }
                }
            }
        }
        for (tier, factors) in &file.tiers {
            for (window, factor) in [
                (FIVE_HOUR, factors.five_hour),
                (SEVEN_DAY, factors.seven_day),
            ] {
                if factor.is_some_and(|f| !f.is_finite() || f <= 0.0) {
                    return Err(LedgerError::Plan(format!(
                        "tier factor {tier}.{window}: a factor is a number above zero, or null"
                    )));
                }
            }
        }
        resolve_bands(&mut file.bands)?;
        Ok(CostTable {
            bands: file.bands,
            effective: file.effective,
            tiers: file.tiers,
            windows: file.windows,
        })
    }

    /// The model's cells on `window`, the exact id then its undated base (`listing::row_for`); `None` for a
    /// model the window lacks.
    pub fn cells(&self, window: &str, canonical_model: &str) -> Option<&ModelCells> {
        let models = self.windows.get(window)?;
        phoebis_routing::listing::row_for(canonical_model, |k| models.get(k))
    }

    /// The reference share of `window` `usage` consumes: `None` for a model the window lacks or a cell with tokens
    /// left `null`; a `null` cell with no tokens of its type adds nothing.
    pub fn reference_share(
        &self,
        window: &str,
        canonical_model: &str,
        usage: PinUsage,
    ) -> Option<f64> {
        let cells = self.cells(window, canonical_model)?;
        let mut share = 0.0;
        for (tokens, cell) in [
            (usage.input, cells.input),
            (usage.cache_read, cells.cache_read),
            (usage.write_5m, cells.cache_write_5m),
            (usage.write_1h, cells.cache_write_1h),
            (usage.output, cells.output),
        ] {
            // A type the usage holds none of adds nothing, so its cell being unmeasured says nothing about this
            // charge; a type it does hold prices only through a measured cell.
            if tokens == 0 {
                continue;
            }
            share += tokens as f64 * cell? / 1_000_000.0;
        }
        Some(share)
    }

    /// The weight the meter puts on a charge landing on `window` at `at`: the in-force band row's factor when its
    /// hours cover `at`, else 1.
    pub fn band_factor(&self, window: &str, at: i64) -> f64 {
        weight_at(&self.bands, window, at)
    }
}

/// The table every plan-side pricer holds.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanTables {
    pub cost: CostTable,
}

impl PlanTables {
    pub fn shipped() -> PlanTables {
        PlanTables {
            cost: CostTable::shipped(),
        }
    }

    /// `reference_share` over the tier's factor for the window (`seven_day_fable` twice the 7-day share); `None`
    /// when either is.
    pub fn share(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
    ) -> Option<f64> {
        let (priced, times) = match window {
            FIVE_HOUR | SEVEN_DAY => (window, 1.0),
            SEVEN_DAY_FABLE => (SEVEN_DAY, 2.0),
            _ => return None,
        };
        let reference = self.cost.reference_share(priced, canonical_model, usage)?;
        let factor = self.tier_factor(tier, priced)?;
        Some(times * reference / factor)
    }

    /// `share` on the 5-hour window weighed by the band at `at`: the meter's charge for a request completing then.
    pub fn five_hour_share_at(
        &self,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        let share = self.share(FIVE_HOUR, tier, canonical_model, usage)?;
        Some(share * self.cost.band_factor(FIVE_HOUR, at))
    }

    /// Per-token rates from the window's rows, `None` for a `null` cell; the fallback is `None` for every type, so
    /// a model the window does not list has no rate.
    pub fn rate_table(&self, window: &str) -> RateTable {
        let per_token = |cells: &ModelCells| Rates {
            input: cells.input.map(|s| s / 1_000_000.0),
            cache_read: cells.cache_read.map(|s| s / 1_000_000.0),
            write_5m: cells.cache_write_5m.map(|s| s / 1_000_000.0),
            write_1h: cells.cache_write_1h.map(|s| s / 1_000_000.0),
            output: cells.output.map(|s| s / 1_000_000.0),
        };
        let rows: HashMap<String, Rates> = self
            .cost
            .windows
            .get(window)
            .into_iter()
            .flatten()
            .map(|(model, cells)| (model.clone(), per_token(cells)))
            .collect();
        // A model the window does not list has no rate: a guessed burn could order a shed the meter never
        // charged, while an unpriced pin only ranks last and stays unmarked.
        let fallback = Rates {
            input: None,
            cache_read: None,
            write_5m: None,
            write_1h: None,
            output: None,
        };
        RateTable { rows, fallback }
    }

    /// The tier's factor for the window (`seven_day_fable` reads the 7-day one), `None` for a tier the table lacks,
    /// a factor left `null`, or a window that is none of the three.
    pub fn tier_factor(&self, tier: &str, window: &str) -> Option<f64> {
        let factors = tier_row(&self.cost.tiers, tier)?;
        match window {
            FIVE_HOUR => factors.five_hour,
            SEVEN_DAY | SEVEN_DAY_FABLE => factors.seven_day,
            _ => None,
        }
    }
}
