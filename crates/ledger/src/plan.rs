//! The plan side's dated tables, shipped beside the price table and read like it: the cost table, the share of a
//! Max 5x window one million tokens of each model and type consume, with the peak band that weighs a 5-hour charge
//! more; and the tier factor table that turns it into a share of another tier's window.

use std::collections::{BTreeMap, HashMap};

use phoebis_routing::{PinUsage, RateTable, Rates, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use jiff::civil::{Date, Weekday};
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::Timestamp;

use crate::LedgerError;

const SHIPPED_COST_TABLE: &str = include_str!("../cost_table.yaml");
const SHIPPED_TIER_FACTORS: &str = include_str!("../tier_factors.yaml");

/// A time band in which the meter weighs a charge more, read at the request's completion, where the charge lands;
/// `factor` 1.0 turns it off. The hours and weekdays are read in `zone` (an IANA name), from `from_hour` up to
/// but not including `to_hour` of one day, and `weekdays` keeps it to Monday through Friday there. The row is in
/// force from `effective_from` (a `YYYY-MM-DD` date in its zone), or since always without one.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Band {
    pub weekdays: bool,
    pub from_hour: u8,
    pub to_hour: u8,
    pub factor: f64,
    pub windows: Vec<String>,
    pub measured: String,
    #[serde(default)]
    pub provisional: bool,
    pub zone: String,
    #[serde(default)]
    pub effective_from: Option<String>,
    /// Set by the `from_yaml` of the table the row ships in, so a lookup never parses a name or a date.
    #[serde(skip)]
    resolved: Option<Resolved>,
}

/// A band's zone and the first instant it is in force, `None` for a row in force since always.
#[derive(Clone, Debug, PartialEq)]
struct Resolved {
    zone: TimeZone,
    from: Option<i64>,
}

impl Band {
    fn names(&self, window: &str) -> bool {
        self.windows.iter().any(|w| w == window)
    }

    /// The zone comes from the database bundled into the binary, so the hours a row names never depend on the
    /// host's copy, or on the host having one; `effective_from` starts at midnight in that zone.
    fn resolve(&self) -> Result<Resolved, LedgerError> {
        let zone = TimeZoneDatabase::bundled()
            .get(&self.zone)
            .map_err(|e| LedgerError::Plan(format!("band zone `{}`: {e}", self.zone)))?;
        let from = match &self.effective_from {
            None => None,
            Some(date) => {
                let refused = |e: jiff::Error| {
                    LedgerError::Plan(format!("band effective_from `{date}`: {e}"))
                };
                let day: Date = date.parse().map_err(refused)?;
                let start = day.to_zoned(zone.clone()).map_err(refused)?;
                Some(start.timestamp().as_millisecond())
            }
        };
        Ok(Resolved { zone, from })
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

/// Resolves every band once; refuses a zone the bundled time-zone database does not know and an `effective_from`
/// that is not a date, so a lookup never meets either.
fn resolve_bands(bands: &mut [Band]) -> Result<(), LedgerError> {
    for band in bands {
        band.resolved = Some(band.resolve()?);
    }
    Ok(())
}

/// The row in force at `at` among those naming `window`: the latest `effective_from` at or before it, a row
/// without one being the oldest, the later in the file among equals. The one lookup every reader of the band goes
/// through, so how the rows spell their edges stays this function's business. A row `resolve_bands` did not
/// resolve is never in force.
fn in_force<'a>(bands: &'a [Band], window: &str, at: i64) -> Option<(&'a Band, &'a Resolved)> {
    bands
        .iter()
        .filter(|band| band.names(window))
        .filter_map(|band| Some((band, band.resolved.as_ref()?)))
        .filter(|(_, resolved)| resolved.from.is_none_or(|from| from <= at))
        .max_by_key(|(_, resolved)| resolved.from)
}

/// The in-force row's factor when its hours cover `at`, else 1.
fn weight_at(bands: &[Band], window: &str, at: i64) -> f64 {
    in_force(bands, window, at)
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

/// One cell: the fraction of the Max 5x window one million tokens of one type on one model consume, and the day
/// it was measured. A cell no measurement filled carries a stand-in derived from the meter's earlier readings and
/// is flagged, so every reader shows it as unmeasured until a measurement fills it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub measured: Option<String>,
    pub share_per_million: f64,
    #[serde(default)]
    pub unmeasured: bool,
}

/// A model's five cells on one window.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCells {
    pub cache_read: Cell,
    pub cache_write_1h: Cell,
    pub cache_write_5m: Cell,
    pub input: Cell,
    pub output: Cell,
}

/// The cost table measured on Max 5x: per window and model, the share of that window per million tokens of each
/// type; `bands` is the peak band on the 5-hour cells, one provisional factor applied at a request's completion.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CostTable {
    pub bands: Vec<Band>,
    pub effective: String,
    /// The tier every cell is measured on, `default_claude_max_5x`; the factor table's reference.
    pub tier: String,
    pub version: String,
    /// `five_hour` and `seven_day`, each keyed by undated model id.
    pub windows: BTreeMap<String, BTreeMap<String, ModelCells>>,
}

/// The file carries no `version`: the table's version is its effective date, as the price table's is.
#[derive(serde::Deserialize)]
struct CostFile {
    bands: Vec<Band>,
    effective: String,
    tier: String,
    windows: BTreeMap<String, BTreeMap<String, ModelCells>>,
}

impl CostTable {
    pub fn shipped() -> CostTable {
        CostTable::from_yaml(SHIPPED_COST_TABLE).expect("the shipped cost table parses")
    }

    /// Refuses a file with a window other than the two, a model without its five cells, or a band
    /// `resolve_bands` refuses.
    pub fn from_yaml(text: &str) -> Result<CostTable, LedgerError> {
        let mut file: CostFile =
            serde_norway::from_str(text).map_err(|e| LedgerError::Plan(e.to_string()))?;
        let windows: Vec<&str> = file.windows.keys().map(String::as_str).collect();
        if windows != [FIVE_HOUR, SEVEN_DAY] {
            return Err(LedgerError::Plan(format!(
                "cost table windows {windows:?}: exactly `{FIVE_HOUR}` and `{SEVEN_DAY}` are priced"
            )));
        }
        resolve_bands(&mut file.bands)?;
        Ok(CostTable {
            bands: file.bands,
            version: file.effective.clone(),
            effective: file.effective,
            tier: file.tier,
            windows: file.windows,
        })
    }

    /// The model's cells on `window`, the exact id then its undated base (`listing::row_for`); `None` for a
    /// model the window lacks.
    pub fn cells(&self, window: &str, canonical_model: &str) -> Option<&ModelCells> {
        let models = self.windows.get(window)?;
        phoebis_routing::listing::row_for(canonical_model, |k| models.get(k))
    }

    /// The Max 5x share of `window` that `usage` on the model consumes, `None` for a model the window lacks.
    pub fn max_5x_share(
        &self,
        window: &str,
        canonical_model: &str,
        usage: PinUsage,
    ) -> Option<Share> {
        let cells = self.cells(window, canonical_model)?;
        let mut share = Share {
            share: 0.0,
            unmeasured: false,
        };
        for (tokens, cell) in [
            (usage.input, &cells.input),
            (usage.cache_read, &cells.cache_read),
            (usage.write_5m, &cells.cache_write_5m),
            (usage.write_1h, &cells.cache_write_1h),
            (usage.output, &cells.output),
        ] {
            share.share += tokens as f64 * cell.share_per_million / 1_000_000.0;
            // A type the usage holds none of adds nothing, so its cell's flag says nothing about this charge.
            share.unmeasured |= tokens > 0 && cell.unmeasured;
        }
        Some(share)
    }

    /// The weight the meter puts on a charge landing on `window` at `at`: the in-force band row's factor when its
    /// hours cover `at`, else 1.
    pub fn band_factor(&self, window: &str, at: i64) -> f64 {
        weight_at(&self.bands, window, at)
    }
}

/// One tier's capacity of one window over Max 5x's, dated or flagged like a cell.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Factor {
    pub factor: f64,
    pub measured: Option<String>,
    #[serde(default)]
    pub unmeasured: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TierFactor {
    pub five_hour: Factor,
    pub seven_day: Factor,
}

/// The tier factor table, measured once: keyed by the tier string the profile reports (`default_claude_pro`,
/// `default_claude_max_5x` at 1, `default_claude_max_20x`); the Fable-scoped week is half the 7-day window on
/// every tier and needs no column.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TierFactors {
    pub measured: String,
    pub reference: String,
    pub tiers: BTreeMap<String, TierFactor>,
}

impl TierFactors {
    pub fn shipped() -> TierFactors {
        TierFactors::from_yaml(SHIPPED_TIER_FACTORS).expect("the shipped tier factors parse")
    }

    /// Refuses a tier without both windows, and a factor at or below zero, which no share can be divided by.
    pub fn from_yaml(text: &str) -> Result<TierFactors, LedgerError> {
        let factors: TierFactors =
            serde_norway::from_str(text).map_err(|e| LedgerError::Plan(e.to_string()))?;
        for (tier, row) in &factors.tiers {
            for (window, factor) in [(FIVE_HOUR, &row.five_hour), (SEVEN_DAY, &row.seven_day)] {
                if !factor.factor.is_finite() || factor.factor <= 0.0 {
                    return Err(LedgerError::Plan(format!(
                        "tier `{tier}` {window} factor {}: a factor is above zero",
                        factor.factor
                    )));
                }
            }
        }
        Ok(factors)
    }

    /// The tier's factor on `five_hour` or `seven_day` (`seven_day_fable` reads the 7-day factor), looked up as
    /// the profile spells it, then with `default_` taken off or put on; `None` for an unknown or unlisted tier.
    pub fn factor(&self, tier: &str, window: &str) -> Option<&Factor> {
        let row = tier_row(&self.tiers, tier)?;
        match window {
            FIVE_HOUR => Some(&row.five_hour),
            SEVEN_DAY | SEVEN_DAY_FABLE => Some(&row.seven_day),
            _ => None,
        }
    }
}

/// A priced charge: the share of the window it was priced for, and whether any cell or factor it read is flagged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Share {
    pub share: f64,
    pub unmeasured: bool,
}

/// The two tables together: what every plan-side pricer holds.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanTables {
    pub cost: CostTable,
    pub tiers: TierFactors,
}

impl PlanTables {
    pub fn shipped() -> PlanTables {
        PlanTables {
            cost: CostTable::shipped(),
            tiers: TierFactors::shipped(),
        }
    }

    /// The share of the account's own `window` (`five_hour`; `seven_day`; `seven_day_fable` as twice the 7-day
    /// share, the Fable week being half the 7-day window) that `usage` on the model consumes on `tier`: the
    /// Max 5x share over the tier's factor. `None` for a model the cost table lacks or a tier the factor table
    /// lacks; `unmeasured` when any cell or the factor read is flagged.
    pub fn share(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
    ) -> Option<Share> {
        let (priced, times) = match window {
            FIVE_HOUR | SEVEN_DAY => (window, 1.0),
            SEVEN_DAY_FABLE => (SEVEN_DAY, 2.0),
            _ => return None,
        };
        let max_5x = self.cost.max_5x_share(priced, canonical_model, usage)?;
        let factor = self.tiers.factor(tier, priced)?;
        Some(Share {
            share: times * max_5x.share / factor.factor,
            unmeasured: max_5x.unmeasured || factor.unmeasured,
        })
    }

    /// `share` on the 5-hour window weighed by the band at `at`: the meter's charge for a request completing then.
    pub fn five_hour_share_at(
        &self,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<Share> {
        let share = self.share(FIVE_HOUR, tier, canonical_model, usage)?;
        Some(Share {
            share: share.share * self.cost.band_factor(FIVE_HOUR, at),
            ..share
        })
    }

    /// The Max 5x share per token of `window` as the routing crate's table; a model the window lacks prices at
    /// its dearest cells, as the dollar table's `rate_table` does, so a burn never reads zero for an unknown
    /// model. A window the cost table lacks has no rows and prices every model at zero.
    pub fn rate_table(&self, window: &str) -> RateTable {
        let per_token = |cells: &ModelCells| Rates {
            input: cells.input.share_per_million / 1_000_000.0,
            cache_read: cells.cache_read.share_per_million / 1_000_000.0,
            write_5m: cells.cache_write_5m.share_per_million / 1_000_000.0,
            write_1h: cells.cache_write_1h.share_per_million / 1_000_000.0,
            output: cells.output.share_per_million / 1_000_000.0,
        };
        let rows: HashMap<String, Rates> = self
            .cost
            .windows
            .get(window)
            .into_iter()
            .flatten()
            .map(|(model, cells)| (model.clone(), per_token(cells)))
            .collect();
        let dearest = |of: fn(&Rates) -> f64| rows.values().map(of).fold(0.0, f64::max);
        let fallback = Rates {
            input: dearest(|r| r.input),
            cache_read: dearest(|r| r.cache_read),
            write_5m: dearest(|r| r.write_5m),
            write_1h: dearest(|r| r.write_1h),
            output: dearest(|r| r.output),
        };
        RateTable { rows, fallback }
    }

    pub fn tier_factor(&self, tier: &str, window: &str) -> Option<f64> {
        self.tiers.factor(tier, window).map(|f| f.factor)
    }

    /// Whether a figure priced on `window` (`seven_day_fable` reads the 7-day row) for the model on `tier` rests
    /// on a stand-in: any of the model's five cells or the tier's factor flagged, or a model the window lacks,
    /// which `rate_table` prices at the dearest cells. False for a tier the factor table lacks, since no share
    /// is priced there and every reader nulls the figure first.
    pub fn unmeasured(&self, window: &str, tier: &str, canonical_model: &str) -> bool {
        let priced = match window {
            SEVEN_DAY_FABLE => SEVEN_DAY,
            other => other,
        };
        let Some(factor) = self.tiers.factor(tier, priced) else {
            return false;
        };
        let Some(cells) = self.cost.cells(priced, canonical_model) else {
            return true;
        };
        factor.unmeasured
            || [
                &cells.cache_read,
                &cells.cache_write_1h,
                &cells.cache_write_5m,
                &cells.input,
                &cells.output,
            ]
            .iter()
            .any(|cell| cell.unmeasured)
    }
}
