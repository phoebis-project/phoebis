//! The per-window reconciliation: for each completed 5-hour, 7-day and Fable window of an account, the rise the
//! account's readings showed against the share the router priced for its own plan-funded attempts completed between
//! those readings, each attempt under its own regime. Nothing is corrected by it: the analytics show the ratio and the
//! `meter_drift` condition reads it.

use std::collections::{BTreeMap, HashMap};

use phoebis_bus::Classifier;
use phoebis_routing::{window_for, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Row, Transaction};

use crate::plan::{funding_of, Funding, PlanTables};
use crate::pricing::{prompt_tokens, PriceRevisions, Speed};
use crate::query::{price_usage, BurnPoint, UsageRow};
use crate::rollup::UNATTRIBUTED;
use crate::LedgerError;

/// The band a completed window's ratio must lie in for `meter_drift` to read it as agreement: symmetric in log terms
/// and wide enough for one reading step on a rise of `MIN_RISE`.
pub const RATIO_LOW: f64 = 0.8;
pub const RATIO_HIGH: f64 = 1.25;
/// The least rise a window must show before its ratio counts, since the readings come in whole percents and a smaller
/// rise is a few of the meter's quanta.
pub const MIN_RISE: f64 = 0.05;

/// Two readings whose resets differ by more than this name different windows. Within it they are one reset as the
/// head's whole seconds and the endpoint's timestamp spell it.
pub(crate) const RESET_TOLERANCE_MS: i64 = 60_000;

/// How long a reconciled window is kept, by its `window_end`: the fold's purge and the route's trailing read share it,
/// so the two agree at the edge.
pub(crate) const RECONCILIATION_RETENTION_MS: i64 = 30 * 24 * 3_600_000;

/// How long before a run's first reading an attempt completing inside the run may have started: it bounds the scan of
/// the account's attempts and lies well past any answer's duration.
const LONGEST_ATTEMPT_MS: i64 = 3_600_000;

/// The windows a run is read on: the two shared ones and the Fable week, the scarce window the pick prices in.
const WINDOWS: [&str; 3] = [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE];

const COLUMNS: &str =
    "account_id, window, window_end, first_reading_at, last_reading_at, observed_rise, \
                       predicted_share, ratio, requests, estimated, computed_at";

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
    /// Each part's last end less its first, summed, as a fraction of the window, rounded to six decimals so a
    /// whole-percent pair's rise compares against `MIN_RISE` as its percents read.
    pub observed_rise: f64,
    /// The share of the window the router priced its plan-funded attempts completed between each part's two ends at.
    pub predicted_share: f64,
    /// `observed_rise` over `predicted_share`, kept whatever the rise.
    pub ratio: f64,
    /// The attempts priced.
    pub requests: u32,
    /// The attempts among them priced at a model tier's estimate.
    pub estimated: u32,
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

/// The answer a reading came from. A response head truncates the window's percent and the usage poll rounds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Head,
    Poll,
}

#[derive(Clone, Copy, Debug)]
struct Reading {
    /// `None` for a reading stored before its instant was kept.
    at: Option<i64>,
    utilization: f64,
    reset: i64,
    source: Source,
}

impl Reading {
    /// Whether the reading falls under `last` and so opens a part, a head under the poll before it only past the
    /// sources' gap.
    fn falls_from(&self, last: &Reading) -> bool {
        let gap = if last.source == Source::Poll && self.source == Source::Head {
            SOURCES_GAP
        } else {
            0.0
        };
        rounded(last.utilization - self.utilization) > gap
    }
}

/// A run's readings between two falls: the parts of one key reconcile together as the window's one row.
#[derive(Debug)]
struct Part {
    readings: Vec<Reading>,
}

impl Part {
    /// The part's two ends under one source: its first and last polls where it holds two or more, else its first
    /// and last heads, `None` for a part with fewer than two readings of the chosen source.
    fn ends(&self) -> Option<(Reading, Reading)> {
        let polls = self.readings.iter().filter(|r| r.source == Source::Poll);
        let source = if polls.count() >= 2 {
            Source::Poll
        } else {
            Source::Head
        };
        let mut chosen = self.readings.iter().filter(|r| r.source == source);
        let first = *chosen.next()?;
        let last = *chosen.next_back()?;
        Some((first, last))
    }
}

/// The consecutive readings of one window that name one reset, in parts split at each fall.
#[derive(Debug)]
struct Run {
    parts: Vec<Part>,
    /// A later reading named another reset.
    closed: bool,
    /// That reading's instant, `None` for one stored before its instant was kept.
    closed_at: Option<i64>,
}

impl Run {
    fn open(reading: Reading) -> Run {
        Run {
            parts: vec![Part {
                readings: vec![reading],
            }],
            closed: false,
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
            Some(run) if (reading.reset - run.first().reset).abs() <= RESET_TOLERANCE_MS => {
                run.push(*reading);
            }
            // A window's reset only moves forward: a reading naming an earlier one is a stale answer.
            Some(run) if reading.reset < run.first().reset => {}
            Some(run) => {
                run.closed = true;
                run.closed_at = reading.at;
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

/// Closes the complete runs since the newest row of each account and window, prices their plan-funded attempts
/// through `price_usage` under each attempt's client, writes the rows with `INSERT OR IGNORE` and drops the rows whose
/// window ended more than 30 days before `now`, all on the caller's transaction. The hourly fold calls it before it
/// folds the routing conditions, so `meter_drift` reads the windows the same tick closed.
///
/// A run is complete once a later reset appears or its reset lies behind `now`, and one a later reset closed while its
/// own lay ahead slid rather than ended, so it writes no row. A run splits into parts at each fall, and the row sums
/// each part's rise between its two ends of one source against the attempts priced between them, a part without two
/// such ends or without a priced attempt adding nothing. The row is skipped when no part adds, when its first end is
/// older than the oldest retained request, whose rise the router's own requests would explain only in part, or when a
/// part holds a plan-funded attempt carrying tokens that the tables price to no share on the window, whose draw the
/// rise holds and the sum would miss. The ledger reads no registry: every account with a reading is read, and an
/// account whose attempts draw no plan window writes no row.
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
    let accounts: Vec<String> = tx
        .prepare("SELECT DISTINCT account_id FROM quota_samples ORDER BY account_id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for account in &accounts {
        let newest = newest_window_ends(tx, account)?;
        for window in WINDOWS {
            let newest = newest.get(window).copied();
            // A run the table already holds names a reset at or before its window's newest row, and a window that ended
            // before the cutoff would leave with the next purge, while every reading of a run past both names a reset
            // past them less the tolerance, so older readings need not be read at all.
            let floor = newest.unwrap_or(i64::MIN).max(cutoff) - RESET_TOLERANCE_MS;
            'runs: for run in runs(&readings_of(tx, account, window, floor)?) {
                let key = run.key();
                let complete = run.closed || key < now;
                if !complete || key < cutoff || newest.is_some_and(|n| key <= n) {
                    continue;
                }
                // The reset slid rather than ended. A closing reading stored before its instant was kept carries
                // none, so its run reconciles as one its own reset closed.
                if run.closed_at.is_some_and(|closed_at| key > closed_at) {
                    continue;
                }
                let (mut first_at, mut last_at) = (None, 0);
                let (mut rise, mut share, mut requests, mut estimated) = (0.0, 0.0, 0u32, 0u32);
                for part in &run.parts {
                    let Some((first, last)) = part.ends() else {
                        continue;
                    };
                    let (Some(from), Some(to)) = (first.at, last.at) else {
                        continue;
                    };
                    match predicted(tx, prices, tables, account, window, from, to)? {
                        Prediction::Unpriced => continue 'runs,
                        Prediction::Empty => {}
                        Prediction::Priced {
                            share: part_share,
                            requests: part_requests,
                            estimated: part_estimated,
                        } => {
                            first_at.get_or_insert(from);
                            last_at = to;
                            // A fall inside the sources' gap is the head's truncation and no drop, so it reads as none.
                            rise += rounded(last.utilization - first.utilization).max(0.0);
                            share += part_share;
                            requests += part_requests;
                            estimated += part_estimated;
                        }
                    }
                }
                let Some(first_at) = first_at else {
                    continue;
                };
                if first_at < earliest {
                    continue;
                }
                let rise = rounded(rise);
                tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO window_reconciliations ({COLUMNS}) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
                    ),
                    params![
                        account,
                        window,
                        key,
                        first_at,
                        last_at,
                        rise,
                        share,
                        rise / share,
                        requests,
                        estimated,
                        now
                    ],
                )?;
            }
        }
    }
    Ok(())
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

/// The account's readings of `window` whose reset lies past `floor`, in the order they were observed. A reading from
/// before the instant was kept sorts at its bucket's start.
fn readings_of(
    conn: &Connection,
    account: &str,
    window: &str,
    floor: i64,
) -> Result<Vec<Reading>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT utilization, reset_at, observed_at, source FROM quota_samples \
         WHERE account_id = ?1 AND window = ?2 AND utilization IS NOT NULL AND reset_at > ?3 \
         ORDER BY COALESCE(observed_at, bucket * 1000), rowid",
    )?;
    let rows = stmt.query_map(params![account, window, floor], |r| {
        Ok(Reading {
            utilization: r.get(0)?,
            reset: r.get(1)?,
            at: r.get(2)?,
            // The writer files every source but a head as a poll, and so does this read.
            source: if r.get::<_, String>(3)? == "head" {
                Source::Head
            } else {
                Source::Poll
            },
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
    /// The attempts' summed share, how many priced and how many of those at a model tier's estimate.
    Priced {
        share: f64,
        requests: u32,
        estimated: u32,
    },
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
    struct Attempt {
        model: String,
        tokens: [u64; 5],
        completed: i64,
        received_at: i64,
        speed: Option<String>,
        account_kind: String,
        overage_in_use: Option<bool>,
        kind: String,
        tier: Option<String>,
        client: Option<String>,
        classifier: Option<String>,
    }
    let mut stmt = conn.prepare(
        "SELECT r.canonical_model, a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, \
         a.cache_write_1h_tokens, a.output_tokens, COALESCE(a.finished_at, r.finished_at, a.started_at) AS completed, \
         r.received_at, a.speed, a.account_kind, a.overage_in_use, r.kind, a.tier, r.client, \
         r.classifier \
         FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE a.account_id = ?1 AND a.started_at > ?2 - ?4 AND a.started_at <= ?3 \
         AND completed > ?2 AND completed <= ?3",
    )?;
    let tokens = |r: &Row, i: usize| -> rusqlite::Result<u64> {
        Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0) as u64)
    };
    let attempts = stmt.query_map(params![account, from, to, LONGEST_ATTEMPT_MS], |r| {
        Ok(Attempt {
            model: r.get(0)?,
            tokens: [
                tokens(r, 1)?,
                tokens(r, 2)?,
                tokens(r, 3)?,
                tokens(r, 4)?,
                tokens(r, 5)?,
            ],
            completed: r.get(6)?,
            received_at: r.get(7)?,
            speed: r.get(8)?,
            account_kind: r.get(9)?,
            overage_in_use: r.get(10)?,
            kind: r.get(11)?,
            tier: r.get(12)?,
            client: r.get(13)?,
            classifier: r.get(14)?,
        })
    })?;
    let (mut share, mut requests, mut estimated) = (0.0, 0u32, 0u32);
    for attempt in attempts {
        let a = attempt?;
        let funding = funding_of(&a.account_kind, a.overage_in_use, a.speed.as_deref());
        let classifier = a.classifier.as_deref() == Some(Classifier::Client.as_str());
        // API-key, fast and overage answers draw no plan window, so they are no part of the rise either. A classifier
        // call draws no plan window either.
        if funding != Funding::Plan
            || (window == SEVEN_DAY_FABLE && window_for(&a.model) != SEVEN_DAY_FABLE)
            || classifier
        {
            continue;
        }
        let [input, cache_read, write_5m, write_1h, output] = a.tokens;
        let band = prices.in_force(a.received_at).band_of(
            &a.model,
            prompt_tokens(input, cache_read, write_5m, write_1h),
        );
        let mut point = BurnPoint::default();
        price_usage(
            &mut point,
            prices,
            tables,
            &UsageRow {
                canonical_model: &a.model,
                client: a.client.as_deref().unwrap_or(UNATTRIBUTED),
                input_tokens: input,
                cache_read_tokens: cache_read,
                cache_write_5m_tokens: write_5m,
                cache_write_1h_tokens: write_1h,
                output_tokens: output,
                at: a.completed,
                received_at: a.received_at,
                funding,
                speed: Speed::of(a.speed.as_deref()),
                band,
                kind: &a.kind,
                tier: a.tier.as_deref(),
                classifier,
            },
            true,
        );
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
            estimated += u32::from(point.estimated);
        }
    }
    Ok(if !share.is_finite() {
        Prediction::Unpriced
    } else if share > 0.0 {
        Prediction::Priced {
            share,
            requests,
            estimated,
        }
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
        estimated: r.get(9)?,
        computed_at: r.get(10)?,
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
    let filter = if accounts.is_empty() {
        String::new()
    } else {
        binds.extend(accounts.iter().cloned().map(Value::Text));
        format!(
            " AND account_id IN ({})",
            vec!["?"; accounts.len()].join(", ")
        )
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM window_reconciliations WHERE window_end >= ?{filter} \
         ORDER BY window_end DESC, account_id, window"
    ))?;
    let rows = stmt.query_map(params_from_iter(binds), row_to_reconciliation)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Per account, the newest row of each window among those whose rise is at least `MIN_RISE`, by window: what the hourly
/// fold reads for `meter_drift`. The ledger cannot tell an OAuth account, so every account with such a row answers,
/// and the attention reader gives OAuth accounts their series.
pub fn newest_rows(
    conn: &Connection,
) -> Result<BTreeMap<String, Vec<ReconciliationRow>>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM window_reconciliations w WHERE observed_rise >= ?1 AND window_end = \
         (SELECT MAX(window_end) FROM window_reconciliations \
          WHERE account_id = w.account_id AND window = w.window AND observed_rise >= ?1) \
         ORDER BY account_id, window"
    ))?;
    let mut out: BTreeMap<String, Vec<ReconciliationRow>> = BTreeMap::new();
    for row in stmt.query_map(params![MIN_RISE], row_to_reconciliation)? {
        let row = row?;
        out.entry(row.account_id.clone()).or_default().push(row);
    }
    Ok(out)
}
