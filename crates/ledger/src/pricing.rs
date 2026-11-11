//! The API price file as a list of revisions keyed by the date each takes effect, and the list prices per million
//! tokens of one revision, which price an attempt's stored counts whenever a reader asks for its dollars, at the
//! revision in force when its request arrived. Every estimate is an estimate: tier and batch modifiers are not known
//! here, and subscription quota is the rate-limit windows, never this number.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use phoebis_routing::PinUsage;

use crate::rollup::{CostSplit, LONG_BAND, STANDARD_BAND, STANDARD_SPEED};
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Speed {
    Fast,
    #[default]
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

    /// The rollup key word: `fast` or `STANDARD_SPEED`.
    pub fn as_str(self) -> &'static str {
        match self {
            Speed::Fast => "fast",
            Speed::Standard => STANDARD_SPEED,
        }
    }
}

/// Which cells a prompt's length selects on a row that carries a long-context block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Band {
    #[default]
    Standard,
    LongContext,
}

impl Band {
    /// The rollup key word: `STANDARD_BAND` or `LONG_BAND`.
    pub fn as_str(self) -> &'static str {
        match self {
            Band::Standard => STANDARD_BAND,
            Band::LongContext => LONG_BAND,
        }
    }

    /// `LONG_BAND` reads `LongContext`, any other word `Standard`.
    pub fn of_rollup(word: &str) -> Band {
        match word {
            LONG_BAND => Band::LongContext,
            _ => Band::Standard,
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

/// One revision of the price file: the date it takes effect and the table of that date.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub effective: String,
    pub models: IndexMap<String, ModelRow>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceFile {
    revisions: Vec<PriceTable>,
}

/// A file's revisions sorted ascending by instant, with the instants precomputed at load so `in_force` is a binary
/// search. Serializes as its one `revisions` key. The price list and the cost model each hold one.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Revisions<T> {
    revisions: Vec<T>,
    #[serde(skip)]
    instants: Vec<i64>,
}

// `new` refuses an empty list, so a list never reads empty.
#[allow(clippy::len_without_is_empty)]
impl<T> Revisions<T> {
    /// Sorts by each entry's `effective` date, refuses an empty list, a date not in `YYYY-MM-DD` form and two entries
    /// dated the same through `refuse`, and computes the instants.
    pub(crate) fn new(
        revisions: Vec<T>,
        effective: impl Fn(&T) -> &str,
        refuse: impl FnOnce(String) -> LedgerError,
    ) -> Result<Revisions<T>, LedgerError> {
        let dates: Vec<&str> = revisions.iter().map(effective).collect();
        let instants = sorted_instants(&dates).map_err(refuse)?;
        // The dates carry the order, and the file's own is not refused.
        let mut pairs: Vec<(i64, T)> = instants.into_iter().zip(revisions).collect();
        pairs.sort_by_key(|(instant, _)| *instant);
        let (instants, revisions) = pairs.into_iter().unzip();
        Ok(Revisions {
            revisions,
            instants,
        })
    }

    /// The last entry whose instant is at or before `at_ms`, else the first, since a blank history is worse than the
    /// earliest known table.
    pub fn in_force(&self, at_ms: i64) -> &T {
        &self.revisions[self
            .instants
            .partition_point(|instant| *instant <= at_ms)
            .saturating_sub(1)]
    }

    pub fn latest(&self) -> &T {
        &self.revisions[self.revisions.len() - 1]
    }

    /// The first entry dated after `now_ms` that `in_force` does not answer, held until its instant: before the first
    /// instant the first entry is in force, so the entry held then is the second.
    pub fn pending(&self, now_ms: i64) -> Option<&T> {
        self.revisions.get(
            self.instants
                .partition_point(|instant| *instant <= now_ms)
                .max(1),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.revisions.iter()
    }

    /// Each entry's instant, in the entries' order.
    pub fn instants(&self) -> &[i64] {
        &self.instants
    }

    pub fn len(&self) -> usize {
        self.revisions.len()
    }
}

/// The price file's revisions and its source (`shipped`, `file` or `test`). Serializes as the file's one key.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PriceRevisions {
    #[serde(flatten)]
    revisions: Revisions<PriceTable>,
    #[serde(skip)]
    pub source: String,
}

impl std::ops::Deref for PriceRevisions {
    type Target = Revisions<PriceTable>;

    fn deref(&self) -> &Revisions<PriceTable> {
        &self.revisions
    }
}

impl PriceRevisions {
    /// Sorts, refuses an empty list and two entries dated the same, and computes the instants.
    pub fn new(revisions: Vec<PriceTable>, source: &str) -> Result<PriceRevisions, LedgerError> {
        Ok(PriceRevisions {
            revisions: Revisions::new(revisions, |r| r.effective.as_str(), LedgerError::Yaml)?,
            source: source.into(),
        })
    }

    /// Refuses a cell that is not a finite non-negative number, and a `long_context.over` that is zero, each under
    /// the entry's date: a negative or `NaN` price makes every estimate negative or `NaN`, which a budget then never
    /// trips on, and a zero threshold would price every prompt at the long-context cells.
    pub fn from_yaml(text: &str, source: &str) -> Result<PriceRevisions, LedgerError> {
        // The typed map reads a key as its text and keeps the last of two equal keys, so the untyped pass runs
        // first and refuses a repeat, a tagged one included. The typed parse stays second so serde's field errors
        // keep their line and column.
        crate::keys::untagged_string_keys(text).map_err(LedgerError::Yaml)?;
        let file: PriceFile =
            serde_norway::from_str(text).map_err(|e| LedgerError::Yaml(e.to_string()))?;
        for table in &file.revisions {
            table
                .check()
                .map_err(|e| LedgerError::Yaml(format!("revision {}: {e}", table.effective)))?;
        }
        PriceRevisions::new(file.revisions, source)
    }

    pub fn shipped() -> PriceRevisions {
        PriceRevisions::from_yaml(SHIPPED_API_PRICES, "shipped")
            .expect("the shipped price file parses")
    }
}

/// 00:00:00 UTC of a `YYYY-MM-DD` date in Unix milliseconds, `None` for any other text (through jiff's civil date).
pub fn effective_instant(date: &str) -> Option<i64> {
    // The shape first, since jiff's parser also reads other forms of a date (a basic `YYYYMMDD`, a date-time) that
    // the files never write.
    let shaped = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    if !shaped {
        return None;
    }
    let day: jiff::civil::Date = date.parse().ok()?;
    let midnight = day.to_zoned(jiff::tz::TimeZone::UTC).ok()?;
    Some(midnight.timestamp().as_millisecond())
}

/// The instant of each entry's date, in the file's order: refuses an empty list, a date not in `YYYY-MM-DD` form and
/// two entries dated the same, with the messages both revision files share.
fn sorted_instants(dates: &[&str]) -> Result<Vec<i64>, String> {
    if dates.is_empty() {
        return Err("revisions: at least one entry".to_string());
    }
    let mut seen = HashSet::new();
    dates
        .iter()
        .enumerate()
        .map(|(i, date)| {
            let instant = effective_instant(date).ok_or_else(|| {
                format!("revisions[{i}].effective: a date in YYYY-MM-DD form, not `{date}`")
            })?;
            if !seen.insert(instant) {
                return Err(format!("revisions: two entries dated {date}"));
            }
            Ok(instant)
        })
        .collect()
}

fn band_in(row: &ModelRow, prompt_tokens: u64) -> Band {
    if row
        .long_context
        .as_ref()
        .is_some_and(|l| prompt_tokens > l.over)
    {
        Band::LongContext
    } else {
        Band::Standard
    }
}

/// The one cell selection `price_at` and `estimate_banded` share, so the cells a test reads are the ones production
/// prices at: the `fast` block for a fast answer on a row that has one, else the long-context cells for
/// `LongContext` on a row that carries them, else the standard cells.
fn cells(row: &ModelRow, speed: Speed, band: Band) -> &ModelPrice {
    match (speed, row.fast.as_ref(), band, row.long_context.as_ref()) {
        (Speed::Fast, Some(fast), _, _) => fast,
        (_, _, Band::LongContext, Some(long)) => &long.cells,
        _ => &row.standard,
    }
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
    /// The cell rules `PriceRevisions::from_yaml` holds each entry to.
    fn check(&self) -> Result<(), String> {
        for (model, row) in &self.models {
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
                    return Err(format!(
                        "model {model}, cell {prefix}{cell} must be a finite non-negative number"
                    ));
                }
            }
            // A zero would price every prompt at the long-context cells, which is a table that should carry them
            // as its standard ones. Serde already refuses a negative or fractional count before this runs.
            if row.long_context.as_ref().is_some_and(|l| l.over == 0) {
                return Err(format!(
                    "model {model}, cell long_context.over must be a whole number of tokens above zero"
                ));
            }
        }
        Ok(())
    }

    /// The row for the id, else the row for the id with a trailing `-YYYYMMDD` removed, with the key it sits
    /// under: a dated id the catalog lists prices as the undated id the table ships. `price` and `model_label`
    /// both read this, so a label never names another row than the one that priced the request.
    fn entry(&self, canonical_model: &str) -> Option<(&str, &ModelRow)> {
        phoebis_routing::listing::row_for(canonical_model, |k| self.models.get_key_value(k))
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
        self.entry(canonical_model)
            .map(|(_, row)| cells(row, speed, band_in(row, prompt_tokens)))
    }

    /// `LongContext` when the row carries `long_context` and the prompt is longer than its `over`, else `Standard`.
    pub fn band_of(&self, canonical_model: &str, prompt_tokens: u64) -> Band {
        self.entry(canonical_model)
            .map_or(Band::Standard, |(_, row)| band_in(row, prompt_tokens))
    }

    /// The cells `speed` and `band` select in `price_at`'s order: the `fast` block for a fast answer, else the
    /// long-context cells for `LongContext` on a row that carries them, else the standard cells.
    pub fn estimate_banded(
        &self,
        canonical_model: &str,
        speed: Speed,
        band: Band,
        counts: phoebis_routing::PinUsage,
    ) -> Option<CostSplit> {
        let (_, row) = self.entry(canonical_model)?;
        Some(split_of(counts, cells(row, speed, band)))
    }

    /// The dearest listed standard cell of each type, per million tokens: the fallback rates `rate_table` hands the
    /// routing for a model the revision does not list.
    pub fn dearest_cells(&self) -> ModelPrice {
        let dearest = |of: fn(&ModelPrice) -> f64| {
            self.models
                .values()
                .map(|row| of(&row.standard))
                .fold(0.0, f64::max)
        };
        ModelPrice {
            input: dearest(|p| p.input),
            cache_write_5m: dearest(|p| p.cache_write_5m),
            cache_write_1h: dearest(|p| p.cache_write_1h),
            cache_read: dearest(|p| p.cache_read),
            output: dearest(|p| p.output),
        }
    }

    /// The metrics label: a priced model reads as the table key that priced it, everything else is one bucket,
    /// so a caller cannot grow the label set by naming models.
    pub fn model_label(&self, canonical_model: &str) -> &str {
        self.entry(canonical_model).map_or("other", |(key, _)| key)
    }

    /// Dollars per token, the routing task's api_key burn unit: the standard cells per row and the long-context
    /// cells of every row that carries them. A model the table lacks prices at `dearest_cells`, so a model the table
    /// has not learned never looks cheap to the shedding order.
    pub fn rate_table(&self) -> phoebis_routing::RateTable {
        let rows = self.per_token_rows(|row| &row.standard);
        // The division by a million rounds correctly and so keeps the order of the cells: the quotient of the
        // dearest cell is the dearest of the quotients, bit for bit.
        let fallback = per_token(&self.dearest_cells());
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
    /// `rate_table`'s, so a pin's fast usage on a model the table lacks prices at the same fallback as the pin's
    /// dollar copy.
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

    /// The entry point for one attempt's counts: the band `band_of` reads from the prompt they sum to, then
    /// `estimate_banded`.
    pub fn estimate(&self, model: &str, speed: Speed, counts: PinUsage) -> Option<CostSplit> {
        let band = self.band_of(model, counts.prompt_tokens());
        self.estimate_banded(model, speed, band, counts)
    }
}

/// The four figures of five counts at the per-million cells `p`, the two writes summed.
pub(crate) fn split_of(counts: phoebis_routing::PinUsage, p: &ModelPrice) -> CostSplit {
    let at = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
    CostSplit {
        input_usd: at(counts.input, p.input),
        cache_read_usd: at(counts.cache_read, p.cache_read),
        cache_write_usd: at(counts.write_5m, p.cache_write_5m)
            + at(counts.write_1h, p.cache_write_1h),
        output_usd: at(counts.output, p.output),
    }
}

/// The operator's file at `path`: `None` when no path is given or nothing is there (`NotFound`), the I/O error for any
/// other failure to read it, so a path that exists and cannot be read refuses the start rather than selecting the
/// shipped table, the way `Path::exists` would by reading false whenever the metadata cannot be read.
pub(crate) fn read_override(path: Option<&Path>) -> Result<Option<String>, LedgerError> {
    let Some(path) = path else {
        return Ok(None);
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LedgerError::Io(e)),
    }
}

pub struct Pricing {
    revisions: Arc<PriceRevisions>,
    unknown: AtomicU64,
}

impl Pricing {
    pub fn new(revisions: PriceRevisions) -> Pricing {
        Pricing {
            revisions: Arc::new(revisions),
            unknown: AtomicU64::new(0),
        }
    }

    /// The operator's file when one is at the path, the shipped file when nothing is (`NotFound`), and the I/O
    /// error for any other failure to read it. The operator's file replaces the whole list.
    pub fn load(override_path: Option<&Path>) -> Result<Pricing, LedgerError> {
        let text = read_override(override_path)?;
        Ok(Pricing::new(match text {
            Some(text) => PriceRevisions::from_yaml(&text, "file")?,
            None => PriceRevisions::shipped(),
        }))
    }

    pub fn revisions(&self) -> Arc<PriceRevisions> {
        self.revisions.clone()
    }

    pub fn unknown_models(&self) -> u64 {
        self.unknown.load(Ordering::Relaxed)
    }

    pub(crate) fn unknown_counter(&self) -> &AtomicU64 {
        &self.unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tables::{FEBRUARY_2026, JANUARY_2026};

    /// The refusal `from_yaml` answers for a file whose one revision's one model carries `standard` and, when given,
    /// `fast` cells and a `long_context` block.
    fn refusal(standard: &str, fast: Option<&str>, long_context: Option<&str>) -> String {
        let fast = fast.map(|f| format!(", fast: {{{f}}}")).unwrap_or_default();
        let long_context = long_context
            .map(|l| format!(", long_context: {{{l}}}"))
            .unwrap_or_default();
        PriceRevisions::from_yaml(
            &format!(
                "revisions:\n  - effective: 2026-10-04\n    models:\n      m: {{{standard}{fast}{long_context}}}\n"
            ),
            "file",
        )
        .unwrap_err()
        .to_string()
    }

    /// A list of one-model revisions, one per date, in the order given.
    fn dated(dates: &[&str]) -> String {
        let entries: String = dates
            .iter()
            .map(|date| format!("  - effective: {date}\n    models:\n      m: {{{CELLS}}}\n"))
            .collect();
        format!("revisions:\n{entries}")
    }

    fn refused(text: &str) -> String {
        match PriceRevisions::from_yaml(text, "file") {
            Err(error) => error.to_string(),
            Ok(revisions) => panic!("loads: {revisions:?}"),
        }
    }

    const CELLS: &str = "input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1";

    /// A negative price makes every estimate negative, which a budget never trips on.
    #[test]
    fn a_negative_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(&CELLS.replace("input: 1", "input: -1"), None, None),
            "pricing table: revision 2026-10-04: model m, cell input must be a finite non-negative number"
        );
    }

    #[test]
    fn a_nan_price_cell_is_refused_naming_the_model_and_the_cell() {
        assert_eq!(
            refusal(&CELLS.replace("output: 1", "output: .nan"), None, None),
            "pricing table: revision 2026-10-04: model m, cell output must be a finite non-negative number"
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
            "pricing table: revision 2026-10-04: model m, cell cache_read must be a finite non-negative number"
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
            "pricing table: revision 2026-10-04: model m, cell fast.cache_write_1h must be a finite non-negative number"
        );
    }

    /// The long-context block repeats the standard cells' names, so its cell is named under `long_context.`.
    #[test]
    fn a_bad_long_context_cell_is_refused_under_its_long_context_name() {
        let block = format!("over: 1000, {}", CELLS.replace("input: 1", "input: -1"));
        assert_eq!(
            refusal(CELLS, None, Some(&block)),
            "pricing table: revision 2026-10-04: model m, cell long_context.input must be a finite non-negative number"
        );
    }

    /// A zero threshold would price every prompt at the long-context cells, which is a table that should carry
    /// them as its standard ones.
    #[test]
    fn a_zero_threshold_is_refused_naming_long_context_over() {
        assert_eq!(
            refusal(CELLS, None, Some(&format!("over: 0, {CELLS}"))),
            "pricing table: revision 2026-10-04: model m, cell long_context.over must be a whole number of tokens above zero"
        );
    }

    /// A negative or fractional count is no token count: serde refuses it under the field's path before the rule
    /// runs.
    #[test]
    fn a_negative_or_fractional_threshold_is_refused_under_its_path() {
        for over in ["-1", "1.5"] {
            let message = refusal(CELLS, None, Some(&format!("over: {over}, {CELLS}")));
            assert!(
                message.starts_with("pricing table: ") && message.contains("long_context.over"),
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
                && message.contains("missing field `output`"),
            "the refusal names the cell the block leaves out: {message}"
        );
    }

    /// A map that kept the last of two rows would price a model at a figure the file does not say.
    #[test]
    fn a_repeated_model_key_is_refused_by_name() {
        let text = crate::test_tables::API_PRICES.replacen(
            "      claude-sonnet-5-5:",
            "      claude-haiku-4-5: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, output: 10}\n      \
             claude-sonnet-5-5:",
            1,
        );
        assert_ne!(text, crate::test_tables::API_PRICES);
        match PriceRevisions::from_yaml(&text, "file") {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.starts_with("pricing table: ") && message.contains("claude-haiku-4-5"),
                    "{message}"
                );
            }
            Ok(revisions) => panic!("a repeated model key loads: {revisions:?}"),
        }
    }

    /// The test file with one more row, tagged with `!price`, after the last row of its last revision.
    fn with_tagged_row(model: &str) -> String {
        format!(
            "{}      !price {model}: {{input: 2, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.2, \
             output: 10}}\n",
            crate::test_tables::API_PRICES
        )
    }

    /// The typed map reads a tagged key as its plain text, so a tagged repeat would load as the model's price.
    #[test]
    fn a_tagged_repeat_of_a_model_key_is_refused_by_name() {
        match PriceRevisions::from_yaml(&with_tagged_row("claude-haiku-4-5"), "file") {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.starts_with("pricing table: ")
                        && message.contains("duplicate")
                        && message.contains("claude-haiku-4-5"),
                    "{message}"
                );
            }
            Ok(revisions) => panic!("a tagged repeat loads: {revisions:?}"),
        }
    }

    #[test]
    fn a_tagged_model_key_is_refused_by_name() {
        match PriceRevisions::from_yaml(&with_tagged_row("claude-sonnet-4"), "file") {
            Err(error) => assert_eq!(
                error.to_string(),
                "pricing table: revisions[1].models.!price claude-sonnet-4: a key is an untagged string"
            ),
            Ok(revisions) => panic!("a tagged key loads: {revisions:?}"),
        }
    }

    #[test]
    fn an_empty_list_is_refused() {
        assert_eq!(
            refused("revisions: []\n"),
            "pricing table: revisions: at least one entry"
        );
    }

    /// Two tables in force from one instant leave no rule for which one prices it.
    #[test]
    fn two_entries_dated_the_same_day_are_refused_naming_the_date() {
        assert_eq!(
            refused(&dated(&["2026-10-04", "2026-01-01", "2026-10-04"])),
            "pricing table: revisions: two entries dated 2026-10-04"
        );
    }

    /// jiff's parser reads other forms of a date too, so the shape is held to `YYYY-MM-DD` before the calendar.
    #[test]
    fn a_date_not_in_yyyy_mm_dd_form_is_refused_naming_its_index_and_its_text() {
        for (date, written) in [
            ("20261004", "\"20261004\""),
            ("2026-1-4", "2026-1-4"),
            ("2026-10-04T00:00", "2026-10-04T00:00"),
            ("October", "October"),
            ("2026-02-30", "2026-02-30"),
        ] {
            assert_eq!(effective_instant(date), None, "{date}");
            assert_eq!(
                refused(&dated(&["2026-01-01", written])),
                format!(
                    "pricing table: revisions[1].effective: a date in YYYY-MM-DD form, not `{date}`"
                ),
            );
        }
        assert_eq!(effective_instant("2026-02-01"), Some(1_769_904_000_000));
    }

    /// The dates carry the order, so the file's own is not refused.
    #[test]
    fn a_file_listing_a_later_date_first_loads_sorted_ascending() {
        let revisions =
            PriceRevisions::from_yaml(&dated(&["2026-02-01", "2026-01-01"]), "file").unwrap();
        let dates: Vec<&str> = revisions.iter().map(|r| r.effective.as_str()).collect();
        assert_eq!(dates, ["2026-01-01", "2026-02-01"]);
        assert_eq!(revisions.latest().effective, "2026-02-01");
    }

    /// Before its date the first entry is already in force, since an instant before the list reads the first, so a
    /// restart before it changes nothing: the entry held is the second, and a list of one entry holds none.
    #[test]
    fn a_list_dated_wholly_after_now_holds_its_second_entry_and_a_list_of_one_holds_none() {
        let revisions =
            PriceRevisions::from_yaml(&dated(&["2098-01-01", "2099-01-01"]), "file").unwrap();
        assert_eq!(revisions.in_force(JANUARY_2026).effective, "2098-01-01");
        assert_eq!(
            revisions
                .pending(JANUARY_2026)
                .map(|r| r.effective.as_str()),
            Some("2099-01-01")
        );
        let alone = PriceRevisions::from_yaml(&dated(&["2099-01-01"]), "file").unwrap();
        assert_eq!(alone.in_force(JANUARY_2026).effective, "2099-01-01");
        assert_eq!(alone.pending(JANUARY_2026), None);
    }

    /// An entry is in force from 00:00 UTC of its date, and an instant before the first entry reads the first.
    #[test]
    fn an_entry_is_in_force_from_midnight_utc_of_its_date_and_before_the_first_the_first_reads() {
        let revisions = crate::test_tables::api_prices();
        let february = 1_769_904_000_000;
        assert_eq!(revisions.in_force(february).effective, "2026-02-01");
        assert_eq!(revisions.in_force(february - 1).effective, "2026-01-01");
        let new_year_2025 = effective_instant("2025-01-01").unwrap();
        assert_eq!(revisions.in_force(new_year_2025).effective, "2026-01-01");
        assert_eq!(revisions.pending(february), None);
        assert_eq!(
            revisions
                .pending(february - 1)
                .map(|r| r.effective.as_str()),
            Some("2026-02-01")
        );
    }

    #[test]
    fn a_second_top_level_key_is_refused_by_serde() {
        let message = refused(&format!("{}source: x\n", dated(&["2026-01-01"])));
        assert!(
            message.starts_with("pricing table: ") && message.contains("unknown field `source`"),
            "{message}"
        );
    }

    // The test table's Sonnet 5.5 row: input 3, cache read 0.3, 5-minute write 3.75, output 15 per million.
    #[test]
    fn an_estimate_is_the_list_price_per_million_tokens_of_each_type() {
        let revisions = crate::test_tables::api_prices();
        let table = revisions.in_force(JANUARY_2026);
        let split = table
            .estimate(
                "claude-sonnet-5-5",
                Speed::Standard,
                PinUsage {
                    input: 1_000_000,
                    cache_read: 500_000,
                    write_5m: 100_000,
                    write_1h: 0,
                    output: 10_000,
                },
            )
            .unwrap();
        let usd = split.total();
        assert!(
            (split.input_usd - 3.0).abs() < 1e-9
                && (split.cache_read_usd - 0.15).abs() < 1e-9
                && (split.cache_write_usd - 0.375).abs() < 1e-9
                && (split.output_usd - 0.15).abs() < 1e-9,
            "{split:?}"
        );
        assert!((usd - 3.675).abs() < 1e-9, "{usd}");
        assert!(table
            .estimate(
                "gpt-5",
                Speed::Standard,
                PinUsage {
                    input: 1,
                    cache_read: 0,
                    write_5m: 0,
                    write_1h: 0,
                    output: 1,
                },
            )
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
        let revisions = crate::test_tables::api_prices();
        let table = revisions.in_force(JANUARY_2026);
        let input = |model: &str, speed: Speed| {
            table
                .estimate(
                    model,
                    speed,
                    PinUsage {
                        input: 1_000_000,
                        cache_read: 0,
                        write_5m: 0,
                        write_1h: 0,
                        output: 0,
                    },
                )
                .unwrap()
                .total()
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
        let prompt = phoebis_routing::PinUsage::prompt_tokens;
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
        let revisions = crate::test_tables::api_prices();
        let table = revisions.in_force(JANUARY_2026);
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
        let both = PriceRevisions::from_yaml(
            "revisions:\n  - effective: 2026-01-01\n    models:\n      m: {input: 1, cache_write_5m: 1, cache_write_1h: 1, \
             cache_read: 1, output: 1, \
             fast: {input: 2, cache_write_5m: 2, cache_write_1h: 2, cache_read: 2, output: 2}, \
             long_context: {over: 10, input: 3, cache_write_5m: 3, cache_write_1h: 3, cache_read: 3, output: 3}}\n",
            "test",
        )
        .unwrap()
        .latest()
        .clone();
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
        let revisions = crate::test_tables::api_prices();
        let table = revisions.in_force(JANUARY_2026);
        let at = |input| {
            table
                .estimate(
                    "claude-haiku-5-5",
                    Speed::Standard,
                    PinUsage {
                        input,
                        cache_read: 250,
                        write_5m: 250,
                        write_1h: 250,
                        output: 100,
                    },
                )
                .unwrap()
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let split = at(250);
        let usd = split.total();
        assert!(
            close(split.input_usd, 250e-6)
                && close(split.cache_read_usd, 25e-6)
                && close(split.cache_write_usd, 812.5e-6)
                && close(split.output_usd, 500e-6),
            "{split:?}"
        );
        assert!(close(usd, 1_587.5e-6), "{usd}");
        let split = at(251);
        let usd = split.total();
        assert!(
            close(split.input_usd, 1_255e-6)
                && close(split.cache_read_usd, 125e-6)
                && close(split.cache_write_usd, 4_062.5e-6)
                && close(split.output_usd, 2_500e-6),
            "one token over the threshold prices every count, the output included, at the long-context cells: \
             {split:?}"
        );
        assert!(close(usd, 7_942.5e-6), "{usd}");
    }

    fn input(tokens: u64) -> phoebis_routing::PinUsage {
        phoebis_routing::PinUsage {
            input: tokens,
            ..Default::default()
        }
    }

    // Haiku 5.5's block starts past 1,000 prompt tokens on the first entry and past 2,000 on the second, and Sonnet
    // 5.5 carries no block.
    #[test]
    fn the_band_reads_long_context_past_the_rows_threshold_in_the_revision_and_standard_otherwise()
    {
        let revisions = crate::test_tables::api_prices();
        let january = revisions.in_force(JANUARY_2026);
        let february = revisions.in_force(FEBRUARY_2026);
        assert_eq!(january.band_of("claude-haiku-5-5", 1_000), Band::Standard);
        assert_eq!(
            january.band_of("claude-haiku-5-5", 1_001),
            Band::LongContext
        );
        for prompt in [0, 1_001, u64::MAX] {
            assert_eq!(
                january.band_of("claude-sonnet-5-5", prompt),
                Band::Standard,
                "a row without the block at {prompt}"
            );
            assert_eq!(
                january.band_of("claude-mystery-9", prompt),
                Band::Standard,
                "an unlisted model at {prompt}"
            );
        }
        assert_eq!(
            january.band_of("claude-haiku-5-5", 1_500),
            Band::LongContext
        );
        assert_eq!(
            february.band_of("claude-haiku-5-5", 1_500),
            Band::Standard,
            "the second entry moves the threshold to 2,000"
        );
    }

    #[test]
    fn a_band_reads_back_from_its_rollup_word() {
        for band in [Band::Standard, Band::LongContext] {
            assert_eq!(Band::of_rollup(band.as_str()), band);
        }
        assert_eq!(Band::Standard.as_str(), "standard");
        assert_eq!(Band::LongContext.as_str(), "long");
    }

    // One million input tokens read the input cell: Opus 5.5's fast 20, Haiku 5.5's long-context 5 and standard 1,
    // and Sonnet 5.5's standard 3, since its row carries no long-context block.
    #[test]
    fn the_speed_and_the_band_select_the_fast_the_long_context_or_the_standard_cells() {
        let revisions = crate::test_tables::api_prices();
        let table = revisions.in_force(JANUARY_2026);
        let usd = |model: &str, speed: Speed, band: Band| {
            table
                .estimate_banded(model, speed, band, input(1_000_000))
                .map(|split| split.input_usd)
        };
        assert_eq!(
            usd("claude-opus-5-5", Speed::Fast, Band::Standard),
            Some(20.0)
        );
        assert_eq!(
            usd("claude-haiku-5-5", Speed::Standard, Band::LongContext),
            Some(5.0)
        );
        assert_eq!(
            usd("claude-haiku-5-5", Speed::Standard, Band::Standard),
            Some(1.0)
        );
        assert_eq!(
            usd("claude-sonnet-5-5", Speed::Standard, Band::LongContext),
            Some(3.0)
        );
        assert_eq!(
            usd("claude-mystery-9", Speed::Standard, Band::Standard),
            None
        );
    }

    #[test]
    fn the_fallback_cells_are_the_dearest_listed_standard_cell_of_each_type_in_the_revision_in_force(
    ) {
        // The test table's dearest standard row is Opus 5.5 in both entries: input, cache read, the two writes and
        // output at 10, 1, 12.5, 20 and 50 per million in January, each doubled in February. Its January fast
        // cells are dearer still, at 20 input, and the fallback leaves them out.
        let revisions = crate::test_tables::api_prices();
        let cells = |at: i64| {
            let p = revisions.in_force(at).dearest_cells();
            [
                p.input,
                p.cache_read,
                p.cache_write_5m,
                p.cache_write_1h,
                p.output,
            ]
        };
        assert_eq!(cells(JANUARY_2026), [10.0, 1.0, 12.5, 20.0, 50.0]);
        assert_eq!(
            cells(FEBRUARY_2026),
            [20.0, 2.0, 25.0, 40.0, 100.0],
            "the second entry doubles every standard cell"
        );
    }

    #[test]
    fn a_serialized_row_carries_long_context_only_when_the_file_did() {
        let revisions = crate::test_tables::api_prices();
        let table = revisions.in_force(JANUARY_2026);
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
        let rates = crate::test_tables::api_prices()
            .in_force(JANUARY_2026)
            .rate_table();
        let one = |pick: fn(&mut phoebis_routing::PinUsage)| {
            let mut usage = phoebis_routing::PinUsage::default();
            pick(&mut usage);
            usage
        };
        let prompt = phoebis_routing::PinUsage::prompt_tokens;
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
        let fast = crate::test_tables::api_prices()
            .in_force(JANUARY_2026)
            .fast_rate_table();
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
            effective: "2026-01-01".into(),
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
            effective: "2026-01-01".into(),
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
    fn the_override_file_beats_the_shipped_table() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Pricing::load(Some(&dir.path().join("missing.yaml")))
                .unwrap()
                .revisions()
                .source,
            "shipped"
        );
        let path = dir.path().join("api_prices.yaml");
        std::fs::write(&path, "revisions:\n  - effective: 2026-10-01\n    models:\n      claude-sonnet-5: {input: 1, cache_write_5m: 1, cache_write_1h: 1, cache_read: 1, output: 1}\n").unwrap();
        let from_file = Pricing::load(Some(&path)).unwrap().revisions();
        assert_eq!((from_file.source.as_str(), from_file.len()), ("file", 1));
        for at in [0, JANUARY_2026, i64::MAX] {
            let table = from_file.in_force(at);
            assert_eq!(
                (table.effective.as_str(), table.models.len()),
                ("2026-10-01", 1),
                "one entry prices every instant: {at}"
            );
        }
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
