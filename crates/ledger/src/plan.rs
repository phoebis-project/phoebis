//! The plan side's two dated tables, shipped beside the price table and read like it: what one token of each
//! model and type costs on the 5-hour meter, and how large each tier's windows are and when the meter weighs a
//! dispatch more.

use std::collections::{BTreeMap, HashMap};

use phoebis_routing::{RateTable, Rates, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use jiff::Timestamp;

use crate::LedgerError;

const SHIPPED_FACTORS: &str = include_str!("../plan_factors.yaml");
const SHIPPED_WINDOWS: &str = include_str!("../plan_windows.yaml");

/// Meter tokens per token, by model and token type, dated; `factor` is the model's weight against the reference.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelFactor {
    pub factor: f64,
    pub input: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub output: f64,
    pub measured: Option<String>,
    #[serde(default)]
    pub unmeasured: bool,
}

impl ModelFactor {
    /// The counts weighted by the type columns alone, without the model factor: the unit a Fable-scoped window
    /// is sized in.
    pub(crate) fn typed_tokens(
        &self,
        input: u64,
        cache_read: u64,
        write_5m: u64,
        write_1h: u64,
        output: u64,
    ) -> f64 {
        input as f64 * self.input
            + cache_read as f64 * self.cache_read
            + write_5m as f64 * self.cache_write_5m
            + write_1h as f64 * self.cache_write_1h
            + output as f64 * self.output
    }

    fn rates(&self) -> Rates {
        Rates {
            input: self.factor * self.input,
            cache_read: self.factor * self.cache_read,
            write_5m: self.factor * self.cache_write_5m,
            write_1h: self.factor * self.cache_write_1h,
            output: self.factor * self.output,
        }
    }
}

/// The meter-token table: one plain input token of `reference` on the 5-hour meter is the unit.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PlanFactors {
    pub measured: String,
    pub reference: String,
    pub models: BTreeMap<String, ModelFactor>,
}

impl PlanFactors {
    pub fn shipped() -> PlanFactors {
        PlanFactors::from_yaml(SHIPPED_FACTORS).expect("the shipped plan factors parse")
    }

    pub fn from_yaml(text: &str) -> Result<PlanFactors, LedgerError> {
        serde_norway::from_str(text).map_err(|e| LedgerError::Plan(e.to_string()))
    }

    /// `phoebis_routing::listing::row_for(canonical_model, |k| self.models.get(k))`: the row for the id, else for
    /// the id with its `-YYYYMMDD` removed.
    pub fn factor(&self, canonical_model: &str) -> Option<&ModelFactor> {
        phoebis_routing::listing::row_for(canonical_model, |k| self.models.get(k))
    }

    /// `factor * (input + 0.026 * cache_read + ...)` for the model, `None` for a model the table lacks.
    pub fn meter_tokens(
        &self,
        canonical_model: &str,
        input: u64,
        cache_read: u64,
        write_5m: u64,
        write_1h: u64,
        output: u64,
    ) -> Option<f64> {
        let row = self.factor(canonical_model)?;
        Some(row.factor * row.typed_tokens(input, cache_read, write_5m, write_1h, output))
    }

    /// The same numbers as a per-token table for the routing task's burn; the fallback row is the reference's.
    pub fn rate_table(&self) -> RateTable {
        RateTable {
            rows: self
                .models
                .iter()
                .map(|(model, row)| (model.clone(), row.rates()))
                .collect::<HashMap<_, _>>(),
            fallback: self
                .models
                .get(&self.reference)
                .map_or(RateTable::unit().fallback, ModelFactor::rates),
        }
    }
}

/// The shared windows in meter tokens; `seven_day_fable` in Fable tokens, since the weekly meter may weigh Fable
/// differently from the 5-hour one and no single conversion factor exists.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TierWindows {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub seven_day_fable: Option<f64>,
    pub measured: Option<String>,
    #[serde(default)]
    pub unmeasured: bool,
}

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
}

impl Band {
    fn names(&self, window: &str) -> bool {
        self.windows.iter().any(|w| w == window)
    }

    fn zone(&self) -> Result<TimeZone, LedgerError> {
        TimeZone::get(&self.zone)
            .map_err(|e| LedgerError::Plan(format!("band zone `{}`: {e}", self.zone)))
    }

    /// The first instant the row is in force: the start of `effective_from` in the row's zone, `None` for a
    /// row in force since always.
    fn effective_at(&self) -> Result<Option<i64>, LedgerError> {
        let Some(date) = &self.effective_from else {
            return Ok(None);
        };
        let refused =
            |e: jiff::Error| LedgerError::Plan(format!("band effective_from `{date}`: {e}"));
        let day: Date = date.parse().map_err(refused)?;
        let start = day.to_zoned(self.zone()?).map_err(refused)?;
        Ok(Some(start.timestamp().as_millisecond()))
    }

    fn covers(&self, at: i64) -> bool {
        let (Ok(instant), Ok(zone)) = (Timestamp::from_millisecond(at), self.zone()) else {
            return false;
        };
        let local = instant.to_zoned(zone);
        if self.weekdays && matches!(local.weekday(), Weekday::Saturday | Weekday::Sunday) {
            return false;
        }
        let hour = i16::from(local.hour());
        i16::from(self.from_hour) <= hour && hour < i16::from(self.to_hour)
    }
}

/// The window sizes per subscription tier, the cold-start prior of the live fit, and the peak bands.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PlanWindows {
    pub measured: String,
    pub tiers: BTreeMap<String, TierWindows>,
    pub bands: Vec<Band>,
}

impl PlanWindows {
    pub fn shipped() -> PlanWindows {
        PlanWindows::from_yaml(SHIPPED_WINDOWS).expect("the shipped plan windows parse")
    }

    /// Refuses a band whose zone the time-zone database does not know or whose `effective_from` is not a date,
    /// so a lookup never meets either.
    pub fn from_yaml(text: &str) -> Result<PlanWindows, LedgerError> {
        let windows: PlanWindows =
            serde_norway::from_str(text).map_err(|e| LedgerError::Plan(e.to_string()))?;
        for band in &windows.bands {
            band.zone()?;
            band.effective_at()?;
        }
        Ok(windows)
    }

    /// The tier's size of `window` (`five_hour`, `seven_day` or `seven_day_fable`), `None` where the tier has
    /// no row or the row no cell. The tier is looked up as the profile stored it, then with its `default_`
    /// prefix taken off or put on, since only one tier's spelling has been seen on the wire.
    pub fn predicted(&self, tier: &str, window: &str) -> Option<f64> {
        let row = self
            .tiers
            .get(tier)
            .or_else(|| {
                tier.strip_prefix("default_")
                    .and_then(|t| self.tiers.get(t))
            })
            .or_else(|| self.tiers.get(&format!("default_{tier}")))?;
        match window {
            FIVE_HOUR => row.five_hour,
            SEVEN_DAY => row.seven_day,
            SEVEN_DAY_FABLE => row.seven_day_fable,
            _ => None,
        }
    }

    /// The weight the meter puts on a charge landing on `window` at `at`: among the rows naming the window, the
    /// one in force at `at` (the latest `effective_from` at or before it, a row without one being the oldest,
    /// the later in the file among equals), its factor when its hours cover `at`, else 1. The one lookup every
    /// reader of the band goes through, so how the rows spell their edges stays this function's business.
    pub fn band_factor(&self, window: &str, at: i64) -> f64 {
        let in_force = self
            .bands
            .iter()
            .filter(|band| band.names(window))
            .filter_map(|band| match band.effective_at() {
                Ok(from) if from.is_none_or(|from| from <= at) => Some((from, band)),
                _ => None,
            })
            .max_by_key(|(from, _)| *from)
            .map(|(_, band)| band);
        in_force
            .filter(|band| band.covers(at))
            .map_or(1.0, |band| band.factor)
    }
}
