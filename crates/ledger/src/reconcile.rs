//! The per-window reconciliation: for each completed 5-hour, 7-day and Fable window of an account, the rise the
//! account's readings showed against the share the router priced for its own plan-funded attempts completed between
//! those readings, each attempt under its own regime. Nothing is corrected by it: the analytics show the ratio and the
//! `meter_drift` condition reads it.

use std::collections::{BTreeMap, HashMap};

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
    /// The last reading's utilization less the first's, as a fraction of the window, rounded to six decimals so a
    /// whole-percent pair's rise compares against `MIN_RISE` as its percents read.
    pub observed_rise: f64,
    /// The share of the window the router priced its plan-funded attempts completed between the two readings at.
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
    /// Outside `RATIO_LOW..=RATIO_HIGH` after a rise of at least `MIN_RISE`: what `meter_drift` holds on.
    pub(crate) fn off(&self) -> bool {
        self.observed_rise >= MIN_RISE && !(RATIO_LOW..=RATIO_HIGH).contains(&self.ratio)
    }
}

#[derive(Clone, Copy, Debug)]
struct Reading {
    /// `None` for a reading stored before its instant was kept.
    at: Option<i64>,
    utilization: f64,
    reset: i64,
}

/// The consecutive readings of one window that name one reset.
#[derive(Debug)]
struct Run {
    first: Reading,
    last: Reading,
    readings: usize,
    /// A later reading named another reset, so the window has ended.
    closed: bool,
}

impl Run {
    fn open(reading: Reading) -> Run {
        Run {
            first: reading,
            last: reading,
            readings: 1,
            closed: false,
        }
    }

    /// The first reading's reset in whole seconds, since a head names its reset in whole seconds and the endpoint's
    /// may carry a fraction.
    fn key(&self) -> i64 {
        (self.first.reset + 500).div_euclid(1_000) * 1_000
    }
}

/// The runs of one window's readings, oldest first.
fn runs(readings: &[Reading]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for reading in readings {
        match out.last_mut() {
            Some(run) if (reading.reset - run.first.reset).abs() <= RESET_TOLERANCE_MS => {
                run.last = *reading;
                run.readings += 1;
            }
            // A window's reset only moves forward: a reading naming an earlier one is a stale answer.
            Some(run) if reading.reset < run.first.reset => {}
            Some(run) => {
                run.closed = true;
                out.push(Run::open(*reading));
            }
            None => out.push(Run::open(*reading)),
        }
    }
    out
}

/// Closes the complete runs since the newest row of each account and window, prices their plan-funded attempts
/// through `price_usage` under each attempt's client, writes the rows with `INSERT OR IGNORE` and drops the rows whose
/// window ended more than 30 days before `now`, all on the caller's transaction. The hourly fold calls it before it
/// folds the routing conditions, so `meter_drift` reads the windows the same tick closed.
///
/// A run is complete once a later reset appears or its reset lies behind `now`. It is skipped with fewer than two
/// readings, with a fall in utilization (a reset the readings missed), with a first reading older than the oldest
/// retained request, whose rise the router's own requests would explain only in part, or with a plan-funded attempt
/// carrying tokens that the tables price to no share on the window, whose draw the rise holds and the sum would miss.
/// The ledger reads no registry: every account with a reading is read, and an account whose attempts draw no plan
/// window writes no row.
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
            for run in runs(&readings_of(tx, account, window, floor)?) {
                let key = run.key();
                let complete = run.closed || key < now;
                if !complete || run.readings < 2 || key < cutoff || newest.is_some_and(|n| key <= n)
                {
                    continue;
                }
                let (Some(first_at), Some(last_at)) = (run.first.at, run.last.at) else {
                    continue;
                };
                // Rounded to six decimals, since 0.15 less 0.10 is 0.04999999999999999 in a double, two ulps under
                // `MIN_RISE`, and a whole-percent pair's rise must compare against the bar as its percents read.
                let rise = ((run.last.utilization - run.first.utilization) * 1e6).round() / 1e6;
                if rise < 0.0 || first_at < earliest {
                    continue;
                }
                let Some((predicted, requests, estimated)) =
                    predicted(tx, prices, tables, account, window, first_at, last_at)?
                else {
                    continue;
                };
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
                        predicted,
                        rise / predicted,
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
        "SELECT utilization, reset_at, observed_at FROM quota_samples \
         WHERE account_id = ?1 AND window = ?2 AND utilization IS NOT NULL AND reset_at > ?3 \
         ORDER BY COALESCE(observed_at, bucket * 1000), rowid",
    )?;
    let rows = stmt.query_map(params![account, window, floor], |r| {
        Ok(Reading {
            utilization: r.get(0)?,
            reset: r.get(1)?,
            at: r.get(2)?,
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

/// The share of `window` the account's plan-funded attempts completed after `from` and at or before `to` were priced
/// at, each under the regime its request's client names and the band at its completion, with how many priced and how
/// many of those at a model tier's estimate. `None` when none priced to a share above zero, or when an attempt with
/// tokens priced to no share on the window. The Fable week prices the attempts on a model that draws it alone, as the
/// meter charges no other model to that week.
fn predicted(
    conn: &Connection,
    prices: &PriceRevisions,
    tables: &PlanTables,
    account: &str,
    window: &str,
    from: i64,
    to: i64,
) -> Result<Option<(f64, u32, u32)>, LedgerError> {
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
    }
    let mut stmt = conn.prepare(
        "SELECT r.canonical_model, a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, \
         a.cache_write_1h_tokens, a.output_tokens, COALESCE(a.finished_at, r.finished_at, a.started_at) AS completed, \
         r.received_at, a.speed, a.account_kind, a.overage_in_use, r.kind, a.tier, r.client \
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
        })
    })?;
    let (mut share, mut requests, mut estimated) = (0.0, 0u32, 0u32);
    for attempt in attempts {
        let a = attempt?;
        let funding = funding_of(&a.account_kind, a.overage_in_use, a.speed.as_deref());
        // API-key, fast and overage answers draw no plan window, so they are no part of the rise either.
        if funding != Funding::Plan
            || (window == SEVEN_DAY_FABLE && window_for(&a.model) != SEVEN_DAY_FABLE)
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
            return Ok(None);
        }
        if let Some(priced) = priced {
            share += priced;
            requests += 1;
            estimated += u32::from(point.estimated);
        }
    }
    Ok((share > 0.0 && share.is_finite()).then_some((share, requests, estimated)))
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
