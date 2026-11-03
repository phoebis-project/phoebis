//! The unused weekly allowance: the share of each weekly window an account left unused at its resets, read off the
//! stored `window_reset` events, with an account's term end read as a reset of the week it cut short.

use std::collections::BTreeMap;

use phoebis_routing::{SEVEN_DAY, SEVEN_DAY_FABLE};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection};

use crate::query::{in_list, WINDOW_RESET};
// The relay hands the router the head's overage-included week as `seven_day_fable`, so the events name the Fable week
// where the samples may name this key, and the read folds the two as the account view does (`account_window_id` in the
// admin routes).
use crate::quota::SEVEN_DAY_OVERAGE_INCLUDED;
use crate::LedgerError;

/// One weekly reset an account met in range, or its term's end read as one: what the unused allowance route lists.
#[derive(Clone, Debug, PartialEq)]
pub struct AllowanceReset {
    pub account_id: String,
    /// `seven_day` or a `seven_day_<model>` key.
    pub window: String,
    /// The reset's instant, the term's end for a `term_ended` row.
    pub reset_at: i64,
    /// The share left unused at the reset, `1 - utilization_before` clamped to the unit interval.
    pub share_left: f64,
    /// The instant the share was read: the event's `at` for a `sample` or `clock` reset, since the event's detail
    /// carries no reading instant, and the sample's own `observed_at` for `term_ended`.
    pub observed_at: i64,
    /// `clock`, `sample` or `term_ended`.
    pub source: String,
    /// The event's pair for a `sample` or `clock` reset, `None` where the event carries none and on a `term_ended`
    /// row.
    pub plan: Option<String>,
    pub tier: Option<String>,
}

/// The members of a `window_reset` row's detail the read takes.
#[derive(serde::Deserialize)]
struct ResetDetail {
    window: String,
    utilization_before: f64,
    reset_at: i64,
    observed: String,
    // A row written before the detail named the pair lacks both members, which read as none.
    plan: Option<String>,
    tier: Option<String>,
}

fn is_weekly(window: &str) -> bool {
    window == SEVEN_DAY || window.starts_with("seven_day_")
}

fn share_left(utilization: f64) -> f64 {
    (1.0 - utilization).clamp(0.0, 1.0)
}

/// The weekly resets of the listed accounts whose event `at` lies in `[from, to]`, read off the stored `window_reset`
/// events, plus one `term_ended` row per weekly window with a reading before each account's end in `term_ends` that
/// lies at or after `from`. The read keeps the listed accounts and answers no row for an empty list, since the
/// ledger holds no account kind: the caller lists the `oauth` accounts it answers for. A term end's row reads
/// the last reading before the end of a week still open at it (its reset at or after the end), the head's
/// overage-included week folded into `seven_day_fable` with the newer reading standing, and a reading with no
/// utilization skipped. A reading with no reset instant shows no open week. A reset of an account in `term_ends` at or
/// after its end is left out, so the week the end cut short counts once. Ordered by account, window, then `reset_at`
/// newest first. No rollup and no stored figure: the history is the events the purge still holds.
pub fn unused_allowance(
    conn: &Connection,
    accounts: &[String],
    from: i64,
    to: i64,
    term_ends: &BTreeMap<String, i64>,
) -> Result<Vec<AllowanceReset>, LedgerError> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let mut binds = vec![
        Value::Integer(from),
        Value::Integer(to),
        Value::Text(WINDOW_RESET.into()),
    ];
    let mut clauses = vec!["kind = ?3 AND at >= ?1 AND at <= ?2".to_string()];
    in_list("account_id", accounts, &mut binds, &mut clauses);
    let mut stmt = conn.prepare(&format!(
        "SELECT account_id, at, detail FROM events WHERE {} ORDER BY account_id, at DESC",
        clauses.join(" AND ")
    ))?;
    let events = stmt.query_map(params_from_iter(binds), |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut rows = Vec::new();
    for event in events {
        let (Some(account_id), at, Some(detail)) = event? else {
            continue;
        };
        let Ok(detail) = serde_json::from_str::<ResetDetail>(&detail) else {
            continue;
        };
        // An ended account's resets are left out by the reset's own instant: a late sample or a clock walk after a
        // restart announces a week that reset before the end only after it, and the week the end cut short is the
        // `term_ended` row's alone.
        if !is_weekly(&detail.window)
            || term_ends
                .get(account_id.as_str())
                .is_some_and(|&end_at| detail.reset_at >= end_at)
        {
            continue;
        }
        rows.push(AllowanceReset {
            account_id,
            window: detail.window,
            reset_at: detail.reset_at,
            share_left: share_left(detail.utilization_before),
            observed_at: at,
            source: detail.observed,
            plan: detail.plan,
            tier: detail.tier,
        });
    }
    // The primary key holds one row per bucket and window, so the newest qualifying bucket is one reading.
    let mut readings = conn.prepare(
        r"SELECT window, utilization, observed_at FROM (
            SELECT window, utilization, observed_at,
              ROW_NUMBER() OVER (PARTITION BY window ORDER BY bucket DESC) AS nth
            FROM quota_samples
            WHERE account_id = ?1 AND (window = 'seven_day' OR window LIKE 'seven_day\_%' ESCAPE '\')
              AND utilization IS NOT NULL AND reset_at >= ?2 AND observed_at < ?2)
          WHERE nth = 1 ORDER BY window",
    )?;
    // An end before the range drops the resets after it above, and its own row belongs to a range that holds it.
    for (account_id, end_at) in term_ends.iter().filter(|(_, end_at)| **end_at >= from) {
        let mut last: BTreeMap<String, (f64, i64)> = BTreeMap::new();
        let found = readings.query_map(params![account_id, end_at], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for reading in found {
            let (window, utilization, observed_at) = reading?;
            let window = if window == SEVEN_DAY_OVERAGE_INCLUDED {
                SEVEN_DAY_FABLE.to_string()
            } else {
                window
            };
            match last.get(&window) {
                Some(&(_, kept)) if kept >= observed_at => {}
                _ => {
                    last.insert(window, (utilization, observed_at));
                }
            }
        }
        rows.extend(
            last.into_iter()
                .map(|(window, (utilization, observed_at))| AllowanceReset {
                    account_id: account_id.clone(),
                    window,
                    reset_at: *end_at,
                    share_left: share_left(utilization),
                    observed_at,
                    source: "term_ended".into(),
                    plan: None,
                    tier: None,
                }),
        );
    }
    rows.sort_by(|a, b| {
        a.account_id
            .cmp(&b.account_id)
            .then_with(|| a.window.cmp(&b.window))
            .then_with(|| b.reset_at.cmp(&a.reset_at))
    });
    Ok(rows)
}
