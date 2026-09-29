//! The per-window reconciliation: for each completed 5-hour and 7-day window of a subscription account, the rise
//! the account's readings showed against the share the router predicted for its own requests completed between
//! those readings. Nothing is corrected by it: the analytics show the ratio and the alert reads it.

use std::collections::HashMap;
use std::time::Duration;

use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::AccountKind;
use rusqlite::{params, Connection, Row};

use crate::plan::PlanTables;
use crate::query::{price_usage, BurnPoint, Tiers, UsageRow};
use crate::quota::POLL_INTERVAL;
use crate::LedgerError;

/// How often the ledger looks for completed windows; three polls, so a window closes within a quarter hour.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(900);

/// The band a completed window's ratio must lie in, and the least rise a window must show, for the analytics
/// and the alert to read it as agreement; a smaller rise is a few one-percent quanta of the meter.
pub const RATIO_LOW: f64 = 0.8;
pub const RATIO_HIGH: f64 = 1.25;
pub const MIN_RISE: f64 = 0.05;

/// Two readings whose resets differ by more than this name different windows; within it they are one reset as
/// the head's whole seconds and the endpoint's timestamp spell it.
pub const RESET_TOLERANCE_MS: i64 = 60_000;

/// How long before a run's first reading an attempt completing inside the run may have started: it bounds the
/// scan of the account's attempts and lies well past any answer's duration.
const LONGEST_ATTEMPT_MS: i64 = 3_600_000;

const COLUMNS: &str = "account_id, computed_at, first_reading_at, last_reading_at, observed_rise, \
                       predicted_share, ratio, requests, unmeasured, window, window_end";

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ReconciliationRow {
    pub account_id: String,
    pub computed_at: i64,
    pub first_reading_at: i64,
    pub last_reading_at: i64,
    pub observed_rise: f64,
    pub predicted_share: f64,
    pub ratio: f64,
    pub requests: u64,
    pub unmeasured: bool,
    /// `five_hour` or `seven_day`.
    pub window: String,
    /// The reset the run's readings named, in milliseconds.
    pub window_end: i64,
}

impl ReconciliationRow {
    /// Outside `RATIO_LOW..=RATIO_HIGH` after a rise of at least `MIN_RISE`: what the alert fires on.
    pub fn off(&self) -> bool {
        self.observed_rise >= MIN_RISE && !(RATIO_LOW..=RATIO_HIGH).contains(&self.ratio)
    }
}

/// The window a sample record's key names: the head's `5h`, `7d` and `7d_oi`, the poll's `fable`, and the
/// canonical names the endpoint and the store spell; `None` for any other key.
pub(crate) fn window_key(key: &str) -> Option<&'static str> {
    match key {
        "5h" | FIVE_HOUR => Some(FIVE_HOUR),
        "7d" | SEVEN_DAY => Some(SEVEN_DAY),
        "7d_oi" | "fable" | SEVEN_DAY_FABLE => Some(SEVEN_DAY_FABLE),
        _ => None,
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

    /// The first reading's reset in whole seconds, since a head names its reset in whole seconds and the
    /// endpoint's may carry a fraction.
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

/// The completed runs of every subscription account since each account's newest row, priced through `tables`
/// and `tiers`; a run whose first reading lies before the earliest retained request is skipped, since its
/// requests are gone; the caller inserts what it answers (`INSERT OR IGNORE` on the key).
pub fn reconcile(
    conn: &Connection,
    tables: &PlanTables,
    tiers: &Tiers,
    kinds: &HashMap<String, AccountKind>,
    now: i64,
) -> Result<Vec<ReconciliationRow>, LedgerError> {
    let earliest: Option<i64> =
        conn.query_row("SELECT MIN(received_at) FROM requests", [], |r| r.get(0))?;
    let Some(earliest) = earliest else {
        return Ok(Vec::new());
    };
    let mut accounts: Vec<&String> = kinds
        .iter()
        .filter(|(_, kind)| **kind == AccountKind::Oauth)
        .map(|(id, _)| id)
        .collect();
    accounts.sort();
    let mut out = Vec::new();
    for account in accounts {
        let Some(tier) = tiers.get(account).and_then(Option::as_deref) else {
            continue;
        };
        let newest = newest_window_ends(conn, account)?;
        // A run the table already holds names a reset at or before its window's newest row, and every reading of
        // a later run names one past it less the tolerance, so older readings need not be read at all.
        let floor = match (newest.get(FIVE_HOUR), newest.get(SEVEN_DAY)) {
            (Some(five), Some(seven)) => five.min(seven) - RESET_TOLERANCE_MS,
            _ => i64::MIN,
        };
        let readings = readings_of(conn, account, floor)?;
        for window in [FIVE_HOUR, SEVEN_DAY] {
            let Some(readings) = readings.get(window) else {
                continue;
            };
            for run in runs(readings) {
                let key = run.key();
                let complete = run.closed || now - key > POLL_INTERVAL.as_millis() as i64;
                if !complete || run.readings < 2 || newest.get(window).is_some_and(|n| key <= *n) {
                    continue;
                }
                let (Some(first_at), Some(last_at)) = (run.first.at, run.last.at) else {
                    continue;
                };
                let rise = run.last.utilization - run.first.utilization;
                // A fall is a reset the readings missed; a run that starts before the oldest request kept would
                // compare the rise with only part of what the router sent.
                if rise < 0.0 || first_at < earliest {
                    continue;
                }
                let Some((predicted, requests, unmeasured)) =
                    predicted(conn, tables, tier, account, window, first_at, last_at)?
                else {
                    continue;
                };
                out.push(ReconciliationRow {
                    account_id: account.clone(),
                    computed_at: now,
                    first_reading_at: first_at,
                    last_reading_at: last_at,
                    observed_rise: rise,
                    predicted_share: predicted,
                    ratio: rise / predicted,
                    requests,
                    unmeasured,
                    window: window.to_string(),
                    window_end: key,
                });
            }
        }
    }
    Ok(out)
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

/// The account's readings of the 5-hour and 7-day windows whose reset lies past `floor`, each window's in the
/// order they were observed; a reading from before the instant was kept sorts at its bucket's start.
fn readings_of(
    conn: &Connection,
    account: &str,
    floor: i64,
) -> Result<HashMap<&'static str, Vec<Reading>>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT window, utilization, reset_at, observed_at FROM quota_samples \
         WHERE account_id = ?1 AND utilization IS NOT NULL AND reset_at > ?2 \
         ORDER BY COALESCE(observed_at, bucket * 1000), rowid",
    )?;
    let rows = stmt.query_map(params![account, floor], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Reading {
                utilization: r.get(1)?,
                reset: r.get(2)?,
                at: r.get(3)?,
            },
        ))
    })?;
    let mut out: HashMap<&'static str, Vec<Reading>> = HashMap::new();
    for row in rows {
        let (key, reading) = row?;
        if !reading.utilization.is_finite() {
            continue;
        }
        if let Some(window @ (FIVE_HOUR | SEVEN_DAY)) = window_key(&key) {
            out.entry(window).or_default().push(reading);
        }
    }
    Ok(out)
}

/// The share of `window` the account's attempts completed after `from` and at or before `to` were priced at,
/// as the analytics price them, with how many priced and whether any read a stand-in; `None` when none priced
/// to a share above zero.
fn predicted(
    conn: &Connection,
    tables: &PlanTables,
    tier: &str,
    account: &str,
    window: &str,
    from: i64,
    to: i64,
) -> Result<Option<(f64, u64, bool)>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT r.canonical_model, a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, \
         a.cache_write_1h_tokens, a.output_tokens, a.estimated_usd, \
         COALESCE(a.finished_at, r.finished_at, a.started_at) AS completed \
         FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE a.account_id = ?1 AND a.started_at > ?2 - ?4 AND a.started_at <= ?3 \
         AND completed > ?2 AND completed <= ?3 \
         AND (a.input_tokens IS NOT NULL OR a.output_tokens IS NOT NULL OR a.cache_read_tokens IS NOT NULL \
              OR a.cache_write_5m_tokens IS NOT NULL OR a.cache_write_1h_tokens IS NOT NULL)",
    )?;
    let tokens = |r: &Row, i: usize| -> rusqlite::Result<u64> {
        Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0) as u64)
    };
    let attempts = stmt.query_map(params![account, from, to, LONGEST_ATTEMPT_MS], |r| {
        Ok((
            r.get::<_, String>(0)?,
            [
                tokens(r, 1)?,
                tokens(r, 2)?,
                tokens(r, 3)?,
                tokens(r, 4)?,
                tokens(r, 5)?,
            ],
            r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
            r.get::<_, i64>(7)?,
        ))
    })?;
    let (mut share, mut requests, mut unmeasured) = (0.0, 0, false);
    for attempt in attempts {
        let (model, [input, cache_read, write_5m, write_1h, output], usd, completed) = attempt?;
        let mut point = BurnPoint::default();
        price_usage(
            &mut point,
            Some(AccountKind::Oauth),
            Some(tier),
            tables,
            &UsageRow {
                canonical_model: &model,
                input_tokens: input,
                cache_read_tokens: cache_read,
                cache_write_5m_tokens: write_5m,
                cache_write_1h_tokens: write_1h,
                output_tokens: output,
                estimated_usd: usd,
                at: completed,
            },
        );
        let priced = if window == FIVE_HOUR {
            point
                .five_hour_share
                .map(|s| (s, point.five_hour_unmeasured))
        } else {
            point
                .seven_day_share
                .map(|s| (s, point.seven_day_unmeasured))
        };
        if let Some((charge, flagged)) = priced {
            share += charge;
            requests += 1;
            unmeasured |= flagged;
        }
    }
    Ok((share > 0.0 && share.is_finite()).then_some((share, requests, unmeasured)))
}

fn row_to_reconciliation(r: &Row) -> rusqlite::Result<ReconciliationRow> {
    Ok(ReconciliationRow {
        account_id: r.get(0)?,
        computed_at: r.get(1)?,
        first_reading_at: r.get(2)?,
        last_reading_at: r.get(3)?,
        observed_rise: r.get(4)?,
        predicted_share: r.get(5)?,
        ratio: r.get(6)?,
        requests: r.get::<_, i64>(7)?.max(0) as u64,
        unmeasured: r.get(8)?,
        window: r.get(9)?,
        window_end: r.get(10)?,
    })
}

/// Every row whose `window_end` lies at or after `since`, newest first: the analytics' read, and the alert
/// poller's at its start, which reads the newest row of each account and window off the front.
pub fn read_reconciliations(
    conn: &Connection,
    since: i64,
) -> Result<Vec<ReconciliationRow>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM window_reconciliations WHERE window_end >= ?1 \
         ORDER BY window_end DESC, account_id, window"
    ))?;
    let rows = stmt.query_map(params![since], row_to_reconciliation)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn read_reconciliations_computed_after(
    conn: &Connection,
    computed_at: i64,
) -> Result<Vec<ReconciliationRow>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM window_reconciliations WHERE computed_at > ?1 \
         ORDER BY computed_at, account_id, window, window_end"
    ))?;
    let rows = stmt.query_map(params![computed_at], row_to_reconciliation)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One transaction for the whole pass: every row of a pass carries one `computed_at`, and the alert poller's
/// watermark read must never see half of them.
pub(crate) fn insert_reconciliations_in(
    conn: &mut Connection,
    rows: &[ReconciliationRow],
) -> Result<(), LedgerError> {
    let tx = conn.transaction()?;
    for row in rows {
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO window_reconciliations ({COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                row.account_id,
                row.computed_at,
                row.first_reading_at,
                row.last_reading_at,
                row.observed_rise,
                row.predicted_share,
                row.ratio,
                row.requests as i64,
                row.unmeasured,
                row.window,
                row.window_end,
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}
