//! The per-window reconciliation: for each completed 5-hour, 7-day and Fable window of an account, the rise the
//! account's readings showed against the share the router priced for its own plan-funded attempts completed between
//! those readings, each attempt under its own regime. Nothing is corrected by it: the analytics show the ratio and the
//! `meter_drift` condition reads it.

use std::collections::{BTreeMap, HashMap};

use phoebis_routing::{
    window_for, FIVE_HOUR, HOUR_MS, RESET_SLACK_MS, SEVEN_DAY, SEVEN_DAY_FABLE, WEEK_MS,
};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Row, Transaction};

use crate::plan::{Funding, PlanTables};
use crate::pricing::PriceRevisions;
use crate::query::{in_list, price_usage, BurnSums, PricedAttempt, PRICED_ATTEMPT_COLUMNS};
use crate::record::SampleSource;
use crate::rollup::bucket_of;
use crate::LedgerError;

/// The band a completed window's ratio must lie in for `meter_drift` to read it as agreement: symmetric in log terms
/// and wide enough for one reading step on a rise of `MIN_RISE`.
pub const RATIO_LOW: f64 = 0.8;
pub const RATIO_HIGH: f64 = 1.25;
/// The least rise a window must show, and the least share the router must have priced in it, before its ratio counts:
/// the readings come in whole percents, so a smaller rise is a few of the meter's quanta and one reading step over a
/// smaller priced share reads far off.
pub const MIN_RISE: f64 = 0.05;

/// How long a reconciled window is kept, by its `window_end`: the fold's purge and the route's trailing read share it,
/// so the two agree at the edge.
pub(crate) const RECONCILIATION_RETENTION_MS: i64 = 30 * 24 * 3_600_000;

/// An account that serves nothing writes no new row, so its stale row leaves the condition once this old, while a
/// serving account completes a 7-day window inside it.
pub(crate) const DRIFT_HOLD_MS: i64 = 8 * 24 * 3_600_000;

/// How long before a run's first reading an attempt completing inside the run may have started: it bounds the scan of
/// the account's attempts and lies well past any answer's duration.
const LONGEST_ATTEMPT_MS: i64 = 3_600_000;

/// The windows a run is read on, each with its length: the two shared ones and the Fable week, the scarce window the
/// pick prices in. The length bounds the buckets `readings_of` seeks, so a window joins the list with its own.
const WINDOWS: [(&str, i64); 3] = [
    (FIVE_HOUR, 5 * HOUR_MS),
    (SEVEN_DAY, WEEK_MS),
    (SEVEN_DAY_FABLE, WEEK_MS),
];

const COLUMNS: &str =
    "account_id, window, window_end, first_reading_at, last_reading_at, observed_rise, \
                       predicted_share, ratio, requests, computed_at";

/// One completed window of an account, reconciled.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ReconciliationRow {
    pub account_id: String,
    /// `five_hour`, `seven_day` or `seven_day_fable`.
    pub window: String,
    /// The reset the run's readings named, in milliseconds.
    pub window_end: i64,
    pub first_reading_at: i64,
    pub last_reading_at: i64,
    /// Each part's last end less its first, floored at zero, summed over the parts holding a priced attempt, as a
    /// fraction of the window, rounded to six decimals so a whole-percent pair's rise compares against `MIN_RISE` as
    /// its percents read.
    pub observed_rise: f64,
    /// The share of the window the router priced its plan-funded attempts completed between each part's two ends at.
    pub predicted_share: f64,
    /// `observed_rise` over `predicted_share`, kept whatever the rise.
    pub ratio: f64,
    /// The attempts priced.
    pub requests: u32,
    pub computed_at: i64,
}

impl ReconciliationRow {
    /// Outside `RATIO_LOW..=RATIO_HIGH` once the rise and the priced share each reach `MIN_RISE`, since one reading
    /// step over a smaller priced share reads far off: what `meter_drift` holds on.
    pub(crate) fn off(&self) -> bool {
        self.observed_rise >= MIN_RISE
            && self.predicted_share >= MIN_RISE
            && !(RATIO_LOW..=RATIO_HIGH).contains(&self.ratio)
    }
}

/// How far a head may read under the poll before it and still join the run: the head truncates the percent the poll
/// rounds, so one reading step down between the two is the same draw.
const SOURCES_GAP: f64 = 0.01;

#[derive(Clone, Copy, Debug)]
struct Reading {
    at: i64,
    utilization: f64,
    reset: i64,
    source: SampleSource,
}

impl Reading {
    /// Whether the reading falls under `last` and so opens a part, a head under the poll before it only past the
    /// sources' gap.
    fn falls_from(&self, last: &Reading) -> bool {
        let gap = if last.source == SampleSource::Poll && self.source == SampleSource::Head {
            SOURCES_GAP
        } else {
            0.0
        };
        rounded(last.utilization - self.utilization) > gap
    }
}

/// A run's readings between two falls, except across a faulty dip `drop_faulty_dips` joins: the parts of one key
/// reconcile together as the window's one row.
#[derive(Debug)]
struct Part {
    readings: Vec<Reading>,
}

/// The source a stretch of readings is read under: the polls where two or more stand, else the heads.
fn chosen_source<'a>(readings: impl IntoIterator<Item = &'a Reading>) -> SampleSource {
    let polls = readings
        .into_iter()
        .filter(|r| r.source == SampleSource::Poll)
        .count();
    if polls >= 2 {
        SampleSource::Poll
    } else {
        SampleSource::Head
    }
}

impl Part {
    /// The part's two ends under one source: its first and last polls where it holds two or more, else its first
    /// and last heads, `None` for a part with fewer than two readings of the chosen source.
    fn ends(&self) -> Option<(Reading, Reading)> {
        let source = chosen_source(&self.readings);
        let mut chosen = self.readings.iter().filter(|r| r.source == source);
        let first = *chosen.next()?;
        let last = *chosen.next_back()?;
        Some((first, last))
    }
}

/// The consecutive readings of one window that name one reset, in parts split at each fall except across a faulty dip
/// `drop_faulty_dips` joins.
#[derive(Debug)]
struct Run {
    parts: Vec<Part>,
    /// The instant of the later reading that named another reset, `None` while none has.
    closed_at: Option<i64>,
}

impl Run {
    fn open(reading: Reading) -> Run {
        Run {
            parts: vec![Part {
                readings: vec![reading],
            }],
            closed_at: None,
        }
    }

    fn first(&self) -> Reading {
        self.parts[0].readings[0]
    }

    fn push(&mut self, reading: Reading) {
        let last = self.parts.last().and_then(|part| part.readings.last());
        if last.is_some_and(|last| reading.falls_from(last)) {
            self.parts.push(Part {
                readings: vec![reading],
            });
        } else if let Some(part) = self.parts.last_mut() {
            part.readings.push(reading);
        }
    }

    /// The run's first reading's reset in whole seconds, whatever that reading's source or part, since a head names its
    /// reset in whole seconds and the endpoint's may carry a fraction.
    fn key(&self) -> i64 {
        (self.first().reset + 500).div_euclid(1_000) * 1_000
    }
}

/// The runs of one window's readings, oldest first.
fn runs(readings: &[Reading]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for reading in readings {
        match out.last_mut() {
            Some(run) if (reading.reset - run.first().reset).abs() <= RESET_SLACK_MS => {
                run.push(*reading);
            }
            // A window's reset only moves forward: a reading naming an earlier one is a stale answer.
            Some(run) if reading.reset < run.first().reset => {}
            Some(run) => {
                run.closed_at = Some(reading.at);
                out.push(Run::open(*reading));
            }
            None => out.push(Run::open(*reading)),
        }
    }
    out
}

/// Rounded to six decimals, since 0.15 less 0.10 is 0.04999999999999999 in a double, two ulps under `MIN_RISE`, and a
/// whole-percent pair's rise or fall must compare as its percents read.
fn rounded(value: f64) -> f64 {
    (value * 1e6).round() / 1e6
}

/// Drops from the run every closed dip whose climb the attempts between its two ends cannot explain, joining the
/// readings around it into one part, so a transient zero never counts a rise twice. A dip that never closes or whose
/// climb the prediction matches keeps its split.
///
/// A failed poll reads like a free reset: the window falls to near zero, for one poll or several, at whatever value
/// the failure left. Neither the dip's length nor its depth tells the two apart, so the attempts between its ends do.
/// A reset climbs back only as far as they draw, while a failed poll's dip returns to where the readings stood with
/// nothing served to explain it. A climb under `MIN_RISE`, a few of the meter's quanta, keeps its split, and so does a
/// dip the fold cannot price, since nothing judges it.
fn drop_faulty_dips(
    tx: &Transaction<'_>,
    prices: &PriceRevisions,
    tables: &PlanTables,
    account: &str,
    window: &str,
    run: &mut Run,
) -> Result<(), LedgerError> {
    /// Indices into the run's readings: the reading before the dip, whose level a later reading must reach to close
    /// it, and the dip's first reading.
    struct Dip {
        level: usize,
        first: usize,
    }
    // Each reading with the label of the part it sits in. Labels rise along the run, so relabeling every reading
    // between two ends with the first end's label joins the parts between them.
    let mut readings: Vec<(usize, Reading)> = run
        .parts
        .iter()
        .enumerate()
        .flat_map(|(label, part)| part.readings.iter().map(move |r| (label, *r)))
        .collect();
    let source = chosen_source(readings.iter().map(|(_, r)| r));
    let chosen: Vec<usize> = readings
        .iter()
        .enumerate()
        .filter(|(_, (_, r))| r.source == source)
        .map(|(i, _)| i)
        .collect();
    let mut dropped = vec![false; readings.len()];
    let mut open: Vec<Dip> = Vec::new();
    for pair in chosen.windows(2) {
        let (before, at) = (pair[0], pair[1]);
        let close = readings[at].1;
        // A dip opens below every open dip's level, so the levels fall from the outermost to the innermost and the
        // dips a reading closes are the innermost ones, judged from the outermost of them.
        let outermost = open
            .iter()
            .position(|dip| rounded(close.utilization - readings[dip.level].1.utilization) >= 0.0);
        let Some(outermost) = outermost else {
            if rounded(readings[before].1.utilization - close.utilization) > 0.0 {
                open.push(Dip {
                    level: before,
                    first: at,
                });
            }
            continue;
        };
        for dip in open.drain(outermost..) {
            let first = readings[dip.first].1;
            let climb = rounded(close.utilization - first.utilization);
            if climb < MIN_RISE {
                continue;
            }
            let share = match predicted(tx, prices, tables, account, window, first.at, close.at)? {
                Prediction::Unpriced => continue,
                Prediction::Empty => 0.0,
                Prediction::Priced { share, .. } => share,
            };
            if climb <= rounded(share * RATIO_HIGH) {
                continue;
            }
            for &i in chosen.iter().filter(|&&i| (dip.first..at).contains(&i)) {
                dropped[i] = true;
            }
            let (level_part, close_part) = (readings[dip.level].0, readings[at].0);
            for (label, _) in readings.iter_mut() {
                if (level_part..=close_part).contains(label) {
                    *label = level_part;
                }
            }
            // The dips inside this one lie among the readings that left, so the drain drops them unjudged.
            break;
        }
    }
    let mut parts: Vec<Part> = Vec::new();
    let mut last_label = None;
    for ((label, reading), dropped) in readings.into_iter().zip(dropped) {
        if dropped {
            continue;
        }
        match parts.last_mut() {
            Some(part) if last_label == Some(label) => part.readings.push(reading),
            _ => parts.push(Part {
                readings: vec![reading],
            }),
        }
        last_label = Some(label);
    }
    run.parts = parts;
    Ok(())
}

/// Closes the complete runs since the newest row of each account and window, prices their plan-funded attempts
/// through `price_usage` under each attempt's client, writes the rows with `INSERT OR IGNORE` and drops the rows whose
/// window ended more than 30 days before `now`, all on the caller's transaction. The hourly fold calls it before it
/// folds the routing conditions, so `meter_drift` reads the windows the same tick closed.
///
/// A run is complete once a later reset appears or its reset lies behind `now`, and one a later reset closed while its
/// own lay ahead by more than the reset tolerance slid rather than ended, so it writes no row. A run splits into parts
/// at each fall except a reversed dip the attempts between its ends cannot explain, which leaves the run, and the row
/// is the parts' `run_sum`. The row is skipped where `run_sum` answers none or its first end is older than the oldest
/// retained request, whose rise the router's own requests would explain only in part. The ledger reads no registry:
/// every account with a reading is read, and an account whose attempts draw no plan window writes no row.
pub fn reconcile(
    tx: &Transaction<'_>,
    prices: &PriceRevisions,
    tables: &PlanTables,
    now: i64,
) -> Result<(), LedgerError> {
    let cutoff = now - RECONCILIATION_RETENTION_MS;
    tx.execute(
        "DELETE FROM window_reconciliations WHERE window_end < ?1",
        params![cutoff],
    )?;
    let earliest: Option<i64> =
        tx.query_row("SELECT MIN(received_at) FROM requests", [], |r| r.get(0))?;
    let Some(earliest) = earliest else {
        return Ok(());
    };
    // One index seek per account from the one before it, where `DISTINCT` would walk every stored sample.
    let accounts: Vec<String> = tx
        .prepare(
            "WITH RECURSIVE a(id) AS ( \
               SELECT MIN(account_id) FROM quota_samples \
               UNION ALL \
               SELECT (SELECT MIN(account_id) FROM quota_samples WHERE account_id > a.id) FROM a \
               WHERE a.id IS NOT NULL) \
             SELECT id FROM a WHERE id IS NOT NULL",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for account in &accounts {
        let newest = newest_window_ends(tx, account)?;
        for (window, length) in WINDOWS {
            let newest = newest.get(window).copied();
            // A run the table already holds names a reset at or before its window's newest row, and a window that ended
            // before the cutoff would leave with the next purge, while every reading of a run past both names a reset
            // past them less the tolerance, so older readings need not be read at all.
            let floor = newest.unwrap_or(i64::MIN).max(cutoff) - RESET_SLACK_MS;
            for mut run in runs(&readings_of(tx, account, window, floor, length)?) {
                let key = run.key();
                let complete = run.closed_at.is_some() || key < now;
                if !complete || key < cutoff || newest.is_some_and(|n| key <= n) {
                    continue;
                }
                // The reset slid rather than ended. The margin is the reset tolerance: the key rounds a fractional poll
                // reset up by as much as half a second and a router clock that lags moves the closing instant earlier,
                // so a run its own reset closed can read just short of its key, while a live slide moves the reset by
                // whole 10-minute steps.
                if run
                    .closed_at
                    .is_some_and(|closed_at| key > closed_at + RESET_SLACK_MS)
                {
                    continue;
                }
                drop_faulty_dips(tx, prices, tables, account, window, &mut run)?;
                let Some(sum) = run_sum(tx, prices, tables, account, window, &run)? else {
                    continue;
                };
                if sum.first_at < earliest {
                    continue;
                }
                tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO window_reconciliations ({COLUMNS}) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
                    ),
                    params![
                        account,
                        window,
                        key,
                        sum.first_at,
                        sum.last_at,
                        sum.rise,
                        sum.share,
                        sum.rise / sum.share,
                        sum.requests,
                        now
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// A run's priced parts summed: the first part's first end, the last part's last end, the rises, the shares and the
/// attempts priced.
struct RunSum {
    first_at: i64,
    last_at: i64,
    rise: f64,
    share: f64,
    requests: u32,
}

/// Each part's rise between its two ends of one source against the attempts priced between them, a part without two
/// such ends or without a priced attempt adding nothing, the rise rounded once over the sum. `None` where no part
/// adds, or where a part holds a plan-funded attempt carrying tokens that the tables price to no share on the window,
/// whose draw the rise holds and the sum would miss.
fn run_sum(
    tx: &Transaction<'_>,
    prices: &PriceRevisions,
    tables: &PlanTables,
    account: &str,
    window: &str,
    run: &Run,
) -> Result<Option<RunSum>, LedgerError> {
    let mut sum: Option<RunSum> = None;
    for part in &run.parts {
        let Some((first, last)) = part.ends() else {
            continue;
        };
        let (from, to) = (first.at, last.at);
        match predicted(tx, prices, tables, account, window, from, to)? {
            Prediction::Unpriced => return Ok(None),
            Prediction::Empty => {}
            Prediction::Priced { share, requests } => {
                let sum = sum.get_or_insert(RunSum {
                    first_at: from,
                    last_at: to,
                    rise: 0.0,
                    share: 0.0,
                    requests: 0,
                });
                sum.last_at = to;
                // A fall inside the sources' gap is the head's truncation and no drop, so it reads as none.
                sum.rise += rounded(last.utilization - first.utilization).max(0.0);
                sum.share += share;
                sum.requests += requests;
            }
        }
    }
    Ok(sum.map(|sum| RunSum {
        rise: rounded(sum.rise),
        ..sum
    }))
}

/// The newest `window_end` the table holds per window of the account.
fn newest_window_ends(
    conn: &Connection,
    account: &str,
) -> Result<HashMap<String, i64>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT window, MAX(window_end) FROM window_reconciliations WHERE account_id = ?1 GROUP BY window",
    )?;
    let rows = stmt.query_map(params![account], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The statement `readings_of` runs and its binds: the account, the window, the floor the reset lies past and the
/// first bucket a reading naming such a reset sits in. A reading names the reset of a window open when it was read,
/// so the reset lies at most the window's `length` past the reading, and the bucket bound lets the
/// `(account_id, window, bucket)` index seek where the reset alone would walk the window's whole retention.
fn readings_statement(
    account: &str,
    window: &str,
    floor: i64,
    length: i64,
) -> (&'static str, Vec<Value>) {
    (
        "SELECT utilization, reset_at, observed_at, source FROM quota_samples \
         WHERE account_id = ?1 AND window = ?2 AND utilization IS NOT NULL AND reset_at > ?3 AND bucket >= ?4 \
         ORDER BY observed_at, rowid",
        vec![
            Value::Text(account.to_string()),
            Value::Text(window.to_string()),
            Value::Integer(floor),
            Value::Integer(bucket_of(floor - length)),
        ],
    )
}

/// The account's readings of `window` whose reset lies past `floor`, in the order they were observed.
fn readings_of(
    conn: &Connection,
    account: &str,
    window: &str,
    floor: i64,
    length: i64,
) -> Result<Vec<Reading>, LedgerError> {
    let (sql, binds) = readings_statement(account, window, floor, length);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(binds), |r| {
        Ok(Reading {
            utilization: r.get(0)?,
            reset: r.get(1)?,
            at: r.get(2)?,
            source: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for reading in rows {
        let reading = reading?;
        if reading.utilization.is_finite() {
            out.push(reading);
        }
    }
    Ok(out)
}

/// What `predicted` found between two ends.
enum Prediction {
    /// The attempts' summed share and how many priced.
    Priced { share: f64, requests: u32 },
    /// No attempt priced to a share above zero.
    Empty,
    /// An attempt with tokens priced to no share on the window, or a sum that is not finite.
    Unpriced,
}

/// The share of `window` the account's plan-funded attempts completed after `from` and at or before `to` were priced
/// at, each under the regime its request's client names and the band at its completion. The Fable week prices the
/// attempts on a model that draws it alone, as the meter charges no other model to that week.
fn predicted(
    conn: &Connection,
    prices: &PriceRevisions,
    tables: &PlanTables,
    account: &str,
    window: &str,
    from: i64,
    to: i64,
) -> Result<Prediction, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PRICED_ATTEMPT_COLUMNS} FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE a.account_id = ?1 AND a.started_at > ?2 - ?4 AND a.started_at <= ?3 \
         AND completed > ?2 AND completed <= ?3"
    ))?;
    let attempts = stmt.query_map(params![account, from, to, LONGEST_ATTEMPT_MS], |r| {
        PricedAttempt::read(r, 0)
    })?;
    let (mut share, mut requests) = (0.0, 0u32);
    for attempt in attempts {
        let attempt = attempt?;
        // The prediction reads the shares alone, which take no anchor row, so the part's end stands in for the read's
        // instant.
        let (key, usage) = attempt.usage(prices, to);
        // API-key, usage-based, fast and overage answers draw no plan window, so they are no part of the rise either. A
        // classifier call draws no plan window either.
        if key.funding != Funding::Plan
            || (window == SEVEN_DAY_FABLE && window_for(key.model) != SEVEN_DAY_FABLE)
            || key.classifier
        {
            continue;
        }
        let mut sums = BurnSums::default();
        price_usage(&mut sums, prices, tables, key, usage, true);
        let point = sums.point();
        let priced = match window {
            FIVE_HOUR => point.five_hour_share,
            SEVEN_DAY => point.seven_day_share,
            _ => point.model_week_share,
        };
        // The meter's rise carries what this attempt drew, so a sum of the others alone would read as drift.
        if priced.is_none() && point.tokens > 0 {
            return Ok(Prediction::Unpriced);
        }
        if let Some(priced) = priced {
            share += priced;
            requests += 1;
        }
    }
    Ok(if !share.is_finite() {
        Prediction::Unpriced
    } else if share > 0.0 {
        Prediction::Priced { share, requests }
    } else {
        Prediction::Empty
    })
}

fn row_to_reconciliation(r: &Row) -> rusqlite::Result<ReconciliationRow> {
    Ok(ReconciliationRow {
        account_id: r.get(0)?,
        window: r.get(1)?,
        window_end: r.get(2)?,
        first_reading_at: r.get(3)?,
        last_reading_at: r.get(4)?,
        observed_rise: r.get(5)?,
        predicted_share: r.get(6)?,
        ratio: r.get(7)?,
        requests: r.get(8)?,
        computed_at: r.get(9)?,
    })
}

/// The rows whose window ended inside the trailing 30 days at `now`, newest first, the named accounts' alone where
/// `accounts` names any: what the analytics route answers.
pub fn read_reconciliations(
    conn: &Connection,
    accounts: &[String],
    now: i64,
) -> Result<Vec<ReconciliationRow>, LedgerError> {
    let mut binds = vec![Value::Integer(now - RECONCILIATION_RETENTION_MS)];
    let mut clauses = vec!["window_end >= ?1".to_string()];
    in_list("account_id", accounts, &mut binds, &mut clauses);
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM window_reconciliations WHERE {} \
         ORDER BY window_end DESC, account_id, window",
        clauses.join(" AND ")
    ))?;
    let rows = stmt.query_map(params_from_iter(binds), row_to_reconciliation)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Per account, the newest row of each window among those whose rise is at least `MIN_RISE` and whose window ended
/// after `now - DRIFT_HOLD_MS`, by window: what the hourly fold reads for `meter_drift`. Both selects read the bound,
/// so a window's newest row is its newest inside it, and a row outside it holds nothing while the route still answers
/// it for its 30 days. The ledger cannot tell an OAuth account, so every account with such a row answers, and the
/// attention reader gives OAuth accounts their series.
pub fn newest_rows(
    conn: &Connection,
    now: i64,
) -> Result<BTreeMap<String, Vec<ReconciliationRow>>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM window_reconciliations w WHERE observed_rise >= ?1 AND window_end > ?2 \
         AND window_end = (SELECT MAX(window_end) FROM window_reconciliations \
          WHERE account_id = w.account_id AND window = w.window AND observed_rise >= ?1 AND window_end > ?2) \
         ORDER BY account_id, window"
    ))?;
    let mut out: BTreeMap<String, Vec<ReconciliationRow>> = BTreeMap::new();
    for row in stmt.query_map(
        params![MIN_RISE, now - DRIFT_HOLD_MS],
        row_to_reconciliation,
    )? {
        let row = row?;
        out.entry(row.account_id.clone()).or_default().push(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tables::plan_of;

    #[test]
    fn the_readings_of_a_window_seek_its_buckets_from_the_floor_less_its_length() {
        let (sql, binds) = readings_statement("a", SEVEN_DAY, 30 * 86_400_000, WEEK_MS);
        let plan = plan_of(sql, binds);
        assert!(
            plan.iter()
                .any(|step| step.contains("(account_id=? AND window=? AND bucket>?)")),
            "{plan:?}"
        );
    }
}
