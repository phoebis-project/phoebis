//! The unused weekly allowance: the share of each weekly window an account left unused at its resets, read off the
//! stored `window_reset` events, with a used limit reset that cleared its window read off the stored `window_dropped`
//! events as a reset inside its week, and an account's term end read as a reset of the week it cut short.

use std::collections::BTreeMap;

use phoebis_oauth::ClaimOutcome;
use phoebis_routing::{
    account_window_id, is_weekly_window, UtilizationSample, WindowReading, WINDOW_DROP_MARGIN,
};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection};

use crate::query::{in_list, LIMIT_RESET_CLAIMED, WINDOW_DROPPED, WINDOW_RESET};
use crate::LedgerError;

/// How long before a drop a used claim of its account still joins it. The drop is dated at the first poll after the
/// use, so it stands up to a poll interval behind the claim.
const CLAIM_JOIN_MS: i64 = 3_600_000;

/// A utilization in whole points, the unit the router reads a drop's margin in.
fn whole_points(utilization: f64) -> i64 {
    (utilization * 100.0).round() as i64
}

/// One weekly reset an account met in range, a used limit reset that cleared its window, or its term's end read as a
/// reset: what the unused allowance route lists.
#[derive(Clone, Debug, PartialEq)]
pub struct AllowanceReset {
    pub account_id: String,
    /// `seven_day` or a `seven_day_<model>` key.
    pub window: String,
    /// The reset's instant, the unchanged reset of the week the use fell in for a `claim` row, and the term's end for
    /// a `term_ended` row.
    pub reset_at: i64,
    /// The share left unused at the reset, or before the drop for a `claim` row, `1 - utilization_before` clamped to
    /// the unit interval.
    pub share_left: f64,
    /// The instant the share was read: the event's `at` for a `sample` or `clock` reset, since the event's detail
    /// carries no reading instant, the drop's instant, the first poll after the use, for a `claim` row, and the
    /// sample's own `observed_at` for `term_ended`.
    pub observed_at: i64,
    /// `claim`, `clock`, `sample` or `term_ended`.
    pub source: String,
    /// The event's pair for a `sample` or `clock` reset and for a `claim` row, `None` where the event carries none. A
    /// `term_ended` row carries the pair `unused_allowance` is handed for its window, the router's stored reading of
    /// the week from before the account's plan-ended flag, or any stored reading where the account carries no such
    /// flag, `None` where it is handed none.
    pub plan: Option<String>,
    pub tier: Option<String>,
}

/// The plan word and the tier a `term_ended` row carries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TermPair {
    pub plan: Option<String>,
    pub tier: Option<String>,
}

/// An ended account's term end with the pairs its `term_ended` rows carry, per weekly window as `account_window_id`
/// names it: the plan word and the tier of the router's stored reading of that window, where the account carries no
/// plan-ended flag or the reading was observed before it. A window `pairs` lacks answers null.
#[derive(Clone, Debug)]
pub struct TermEnd {
    pub end_at: i64,
    pub pairs: BTreeMap<String, TermPair>,
}

/// The term end at `end_at` with the pair of each weekly window's reading in the router's stored `sample`, keyed as
/// `unused_allowance` keys its rows, where the reading was observed before `plan_ended_at`, the account's plan-ended
/// flag, or wherever no flag stands. A reading's pair is the plan's as long as it landed while the stored profile still
/// named the plan, which the flag's instant ends: the poll reads an account past a saved end until the flag stops it.
/// Where two windows fold into one key, the newer reading stands. With no sample the term end holds no pair.
pub fn term_end_of(
    end_at: i64,
    sample: Option<&UtilizationSample>,
    plan_ended_at: Option<i64>,
) -> TermEnd {
    let Some(sample) = sample else {
        return TermEnd {
            end_at,
            pairs: BTreeMap::new(),
        };
    };
    let mut kept: BTreeMap<String, &WindowReading> = BTreeMap::new();
    for (window, reading) in &sample.windows {
        if !is_weekly_window(window) || plan_ended_at.is_some_and(|at| reading.observed_at >= at) {
            continue;
        }
        let key = account_window_id(window).to_string();
        if kept
            .get(&key)
            .is_none_or(|stood| stood.observed_at < reading.observed_at)
        {
            kept.insert(key, reading);
        }
    }
    let pairs = kept
        .into_iter()
        .map(|(window, reading)| {
            let pair = TermPair {
                plan: reading.plan.clone(),
                tier: reading.tier.clone(),
            };
            (window, pair)
        })
        .collect();
    TermEnd { end_at, pairs }
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

/// The members of a `window_dropped` row's detail the read takes.
#[derive(serde::Deserialize)]
struct DropDetail {
    window: String,
    utilization_before: f64,
    utilization: f64,
    reset: i64,
    plan: Option<String>,
    tier: Option<String>,
}

/// The members of a `limit_reset_claimed` row's detail the join reads. `result` is null where the router parsed no
/// answer, and `cleared` names the windows a parsed answer cleared.
#[derive(serde::Deserialize)]
struct ClaimDetail {
    cleared: Vec<String>,
    outcome: String,
    result: Option<String>,
}

fn share_left(utilization: f64) -> f64 {
    (1.0 - utilization).clamp(0.0, 1.0)
}

/// The `(account_id, at, detail)` of the listed accounts' events of `kind` whose `at` lies in `[from, to]`, each
/// account's newest first. A row with no account or no detail is left out.
fn events_of(
    conn: &Connection,
    kind: &str,
    accounts: &[String],
    from: i64,
    to: i64,
) -> Result<Vec<(String, i64, String)>, LedgerError> {
    let mut binds = vec![
        Value::Integer(from),
        Value::Integer(to),
        Value::Text(kind.into()),
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
        if let (Some(account_id), at, Some(detail)) = event? {
            rows.push((account_id, at, detail));
        }
    }
    Ok(rows)
}

/// The weekly resets of the listed accounts whose event `at` lies in `[from, to]`, read off the stored `window_reset`
/// events, one `claim` row per stored `window_dropped` event in that span that cleared its window, plus one
/// `term_ended` row per weekly window with a reading before each account's end in `term_ends` that lies at or after
/// `from`. The read keeps the listed accounts and answers no row for an empty list, since the ledger holds no account
/// kind: the caller lists the `oauth` accounts it answers for. A drop joins the newest `used` `limit_reset_claimed` of
/// its account in the `CLAIM_JOIN_MS` before it: where that claim carries a parsed answer, the drop cleared its window
/// where the answer's `cleared` list names it, and with no such claim, or one with no parsed answer, where the drop's
/// figure after reads under 5 points. A drop that only lowered its window lists no row. A term end's row reads the last
/// reading before the end of a week still open at it (its reset at or after the end), a row stored under
/// `seven_day_overage_included` folded into `seven_day_fable` with the newer reading standing, and a reading with no
/// utilization skipped. A reading with no reset instant shows no open week. A reset of an account in `term_ends` at or
/// after its end is left out, so the week the end cut short counts once, and so is a drop at or after the end. Ordered
/// by account, window, then `reset_at` and `observed_at`, each newest first, so a week's reset stands ahead of a use
/// inside that week. No rollup and no stored figure: the history is the events the purge still holds. Each `TermEnd`
/// holds the account's end and the pairs of its `term_ended` rows.
pub fn unused_allowance(
    conn: &Connection,
    accounts: &[String],
    from: i64,
    to: i64,
    term_ends: &BTreeMap<String, TermEnd>,
) -> Result<Vec<AllowanceReset>, LedgerError> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for (account_id, at, detail) in events_of(conn, WINDOW_RESET, accounts, from, to)? {
        let Ok(detail) = serde_json::from_str::<ResetDetail>(&detail) else {
            continue;
        };
        // An ended account's resets are left out by the reset's own instant: a late sample or a clock walk after a
        // restart announces a week that reset before the end only after it, and the week the end cut short is the
        // `term_ended` row's alone.
        if !is_weekly_window(&detail.window)
            || term_ends
                .get(account_id.as_str())
                .is_some_and(|term| detail.reset_at >= term.end_at)
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
    // A drop at the range's start keeps its join with a claim just before the start.
    let mut claims: BTreeMap<String, Vec<(i64, ClaimDetail)>> = BTreeMap::new();
    for (account_id, at, detail) in events_of(
        conn,
        LIMIT_RESET_CLAIMED,
        accounts,
        from.saturating_sub(CLAIM_JOIN_MS),
        to,
    )? {
        if let Ok(detail) = serde_json::from_str::<ClaimDetail>(&detail) {
            if detail.outcome == ClaimOutcome::Used.as_str() {
                claims.entry(account_id).or_default().push((at, detail));
            }
        }
    }
    for (account_id, at, detail) in events_of(conn, WINDOW_DROPPED, accounts, from, to)? {
        let Ok(detail) = serde_json::from_str::<DropDetail>(&detail) else {
            continue;
        };
        if !is_weekly_window(&detail.window)
            || term_ends
                .get(account_id.as_str())
                .is_some_and(|term| at >= term.end_at)
        {
            continue;
        }
        // Each account's claims stand newest first, so the first inside the span is the newest.
        let answered = claims
            .get(&account_id)
            .and_then(|claims| {
                claims
                    .iter()
                    .find(|(claimed, _)| *claimed <= at && at - *claimed <= CLAIM_JOIN_MS)
            })
            .map(|(_, claim)| claim)
            .filter(|claim| claim.result.is_some());
        // A request between the use and the poll can lift a cleared window off zero, so the test reads the margin.
        let cleared = match answered {
            Some(claim) => claim.cleared.contains(&detail.window),
            None => whole_points(detail.utilization) < whole_points(WINDOW_DROP_MARGIN),
        };
        if !cleared {
            continue;
        }
        rows.push(AllowanceReset {
            account_id,
            window: detail.window,
            reset_at: detail.reset,
            share_left: share_left(detail.utilization_before),
            observed_at: at,
            source: "claim".into(),
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
    for (account_id, term) in term_ends.iter().filter(|(_, term)| term.end_at >= from) {
        let mut last: BTreeMap<String, (f64, i64)> = BTreeMap::new();
        let found = readings.query_map(params![account_id, term.end_at], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for reading in found {
            let (window, utilization, observed_at) = reading?;
            let window = account_window_id(&window).to_string();
            match last.get(&window) {
                Some(&(_, kept)) if kept >= observed_at => {}
                _ => {
                    last.insert(window, (utilization, observed_at));
                }
            }
        }
        rows.extend(
            last.into_iter()
                .map(|(window, (utilization, observed_at))| {
                    let pair = term.pairs.get(&window).cloned().unwrap_or_default();
                    AllowanceReset {
                        account_id: account_id.clone(),
                        window,
                        reset_at: term.end_at,
                        share_left: share_left(utilization),
                        observed_at,
                        source: "term_ended".into(),
                        plan: pair.plan,
                        tier: pair.tier,
                    }
                }),
        );
    }
    rows.sort_by(|a, b| {
        a.account_id
            .cmp(&b.account_id)
            .then_with(|| a.window.cmp(&b.window))
            .then_with(|| b.reset_at.cmp(&a.reset_at))
            .then_with(|| b.observed_at.cmp(&a.observed_at))
    });
    Ok(rows)
}
