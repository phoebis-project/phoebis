//! The moved-threshold re-fold. A rollup row keys the prompt-length band its attempts priced at, read against the
//! threshold in force when each request arrived, so a price file whose threshold moves (a revision that lands late
//! with another `over`, a long-context block added or removed, a date moved) leaves the rows folded before it under
//! the old band. The first start after the change folds those requests again: the old deltas out, the new in.
//!
//! The mark holds every revision's thresholds rather than a hash of the file, since taking a request's old deltas
//! back out needs the band each attempt fell in then, which only the old thresholds give. It carries `since`, the
//! first start that wrote it, because the requests folded before it landed under the migration's `standard` and
//! never used an older band: taking an older band out of their rows would subtract what was never added. A request
//! received before `since` and landed after it folds at the thresholds loaded at its landing and is never re-folded
//! when a threshold moves later, by design, since every pass reads the requests received at or after `since`. The
//! pass in progress is a row of `meta` of its own, written by every page's transaction, so a start that fails after a
//! page committed resumes after that page rather than subtracting its old deltas a second time.
//!
//! A threshold move re-bands the request rows alone. The wasted-probe counts and the probes the keepalive still holds
//! keep the band they settled or were recorded in, which leaves the waste of a probe whose prompt falls between the old
//! and the new threshold, inside the retained window, priced at the band it settled in.
//!
//! A fold rule (the hits split off under each miss reason, the slow-down's hold left out of the millisecond samples,
//! the tokens-per-second floor, the wide edge set) changes what a request adds to its hour, so the first start of a
//! build that holds a rule folds again each retained request that landed before the rule's mark: its deltas under the
//! rules it was folded under come out and its deltas under every rule go in, at the thresholds its rows stand under.
//! That pass runs before the band re-fold and resumes from a cursor of its own.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

use crate::bodies::{first_kept_hour, request_cutoff};
use crate::db::{
    begin_write, row_to_request, with_attempts, LedgerDb, LANDING_STAMP_SQL, REQUEST_SELECT,
};
use crate::generations::PAGE;
use crate::pricing::{LongContext, ModelPrice, ModelRow, PriceRevisions, PriceTable};
use crate::record::{AttemptRecord, RequestRecord};
use crate::rollup::{deltas_of, fold_delta, read_rollup, unfold, FoldRules, RollupRow};
use crate::LedgerError;

/// One `meta` row per fold rule, `fold_rule_<name>`: `{"mark": <landing stamp>, "done": <bool>}`.
pub const FOLD_RULE_KEY_PREFIX: &str = "fold_rule_";

/// The `meta` key of a rules pass in progress, written by every page's transaction.
pub const FOLD_RULES_REFOLD_KEY: &str = "fold_rules_refold";

/// A rule's step from the rules a fold applied to the same rules less that one.
type Without = fn(FoldRules) -> FoldRules;

/// Each fold rule by the name its `meta` row carries, with the rules a request folded without it was folded under.
const FOLD_RULES: [(&str, Without); 4] = [
    ("hit_split", |rules| FoldRules {
        hit_split: false,
        ..rules
    }),
    ("slow_down_hold", |rules| FoldRules {
        slow_down_hold: false,
        ..rules
    }),
    ("throughput_floor", |rules| FoldRules {
        throughput_floor: false,
        ..rules
    }),
    ("wide_edges", |rules| FoldRules {
        wide_edges: false,
        ..rules
    }),
];

/// A fold rule's `meta` row. Until the pass is `done`, a request whose `landed_at` is under `mark`, or null, holds
/// the samples of a fold without the rule.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct RuleMark {
    mark: i64,
    done: bool,
}

fn rule_key(name: &str) -> String {
    format!("{FOLD_RULE_KEY_PREFIX}{name}")
}

/// Writes the mark of every rule that has none, before the ledger folds its first request of this start. The mark
/// is the landing watermark, the newest `landed_at` plus one, since a stored stamp can lie ahead of the clock.
pub fn mark_fold_rules(db: &LedgerDb) -> Result<(), LedgerError> {
    db.write(|c| {
        let tx = begin_write(c)?;
        let newest: i64 = tx
            .prepare_cached(LANDING_STAMP_SQL)?
            .query_row([], |r| r.get(0))?;
        for (name, _) in FOLD_RULES {
            let key = rule_key(name);
            if read_meta::<RuleMark>(&tx, &key)?.is_none() {
                write_meta(
                    &tx,
                    &key,
                    &RuleMark {
                        mark: newest + 1,
                        done: false,
                    },
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// Re-folds the retained requests that were folded without a rule whose pass is not done. Answers how many moved.
/// A start that finds the cursor of a pass that stopped finishes that pass for the cursor's rules first, so a rule
/// marked since then gets a pass of its own over every request. Each request folds at the thresholds its rows stand
/// under (`Bands::of`), and the band re-fold moves the rows after this pass. The walk is `refold_bands`'s, bounded by
/// the same retentions.
pub fn refold_rules(
    db: &LedgerDb,
    retention_days: u32,
    rollup_retention_days: u32,
    now: i64,
) -> Result<usize, LedgerError> {
    let (cutoff, guard) = walk_bounds(retention_days, rollup_retention_days, now);
    let (cursor, bands) = db.write(|c| {
        Ok((
            read_meta::<RulesCursor>(c, FOLD_RULES_REFOLD_KEY)?,
            Bands::read(c)?,
        ))
    })?;
    let walk = RulesWalk {
        from: cutoff.unwrap_or(i64::MIN),
        guard,
        bands,
    };
    let mut moved = 0;
    if let Some(cursor) = cursor {
        moved += rules_pass(db, &walk, Some(cursor.rules.as_slice()), Some(cursor.after))?;
    }
    moved += rules_pass(db, &walk, None, None)?;
    Ok(moved)
}

/// What a rules pass reads besides the rules: the walk's floor, the guard of `walk_bounds` and the thresholds.
struct RulesWalk {
    from: i64,
    guard: Option<i64>,
    bands: Bands,
}

/// A rule whose pass is not done.
struct OpenRule {
    name: &'static str,
    mark: RuleMark,
    without: Without,
}

fn open_rules(conn: &Connection) -> Result<Vec<OpenRule>, LedgerError> {
    let mut open = Vec::new();
    for (name, without) in FOLD_RULES {
        let mark = read_meta::<RuleMark>(conn, &rule_key(name))?;
        if let Some(mark) = mark.filter(|mark| !mark.done) {
            open.push(OpenRule {
                name,
                mark,
                without,
            });
        }
    }
    Ok(open)
}

/// One pass for the open rules `named` lists, every open rule where it lists none, resuming after `after`: a page per
/// transaction with its cursor, the targets marked done and the cursor cleared by the last.
fn rules_pass(
    db: &LedgerDb,
    walk: &RulesWalk,
    named: Option<&[String]>,
    mut after: Option<After>,
) -> Result<usize, LedgerError> {
    let open = db.write(|c| open_rules(c))?;
    let targets: Vec<&OpenRule> = open
        .iter()
        .filter(|rule| named.is_none_or(|names| names.iter().any(|name| name == rule.name)))
        .collect();
    if targets.is_empty() {
        return Ok(0);
    }
    let names: Vec<String> = targets.iter().map(|rule| rule.name.to_string()).collect();
    let mut moved = 0;
    loop {
        let done = db.write(|c| {
            let tx = begin_write(c)?;
            let requests = requests_after(&tx, walk.from, after.as_ref(), PAGE)?;
            let stamps = landed_after(&tx, walk.from, after.as_ref(), PAGE)?;
            for ((request, attempts), landed_at) in requests.iter().zip(stamps) {
                let (old, new) = folded_under(&open, &names, landed_at);
                if old == new {
                    continue;
                }
                let Some(prices) = walk.bands.of(request, landed_at) else {
                    continue;
                };
                let (was, now) = (
                    deltas_of(request, attempts, prices, old),
                    deltas_of(request, attempts, prices, new),
                );
                if was == now {
                    continue;
                }
                if walk.guard.is_some_and(|guard| request.received_at < guard)
                    && !rows_stand(&tx, &was)?
                {
                    continue;
                }
                for delta in &was {
                    unfold(&tx, delta)?;
                }
                for delta in &now {
                    fold_delta(&tx, delta)?;
                }
                moved += 1;
            }
            let last = requests.len() < PAGE;
            if last {
                tx.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    params![FOLD_RULES_REFOLD_KEY],
                )?;
                for rule in &targets {
                    write_meta(
                        &tx,
                        &rule_key(rule.name),
                        &RuleMark {
                            done: true,
                            ..rule.mark
                        },
                    )?;
                }
            } else if let Some((request, _)) = requests.last() {
                let next = After {
                    received_at: request.received_at,
                    id: request.id.clone(),
                };
                write_meta(
                    &tx,
                    FOLD_RULES_REFOLD_KEY,
                    &RulesCursor {
                        rules: names.clone(),
                        after: next.clone(),
                    },
                )?;
                after = Some(next);
            }
            tx.commit()?;
            Ok(last)
        })?;
        if done {
            return Ok(moved);
        }
    }
}

/// The rules a request was folded under and the rules the pass folds it under. An open rule whose mark lies past the
/// request's landing, or any open rule for a request from before the column, is missing from the first, and from the
/// second too where the pass does not target it.
fn folded_under(
    open: &[OpenRule],
    targets: &[String],
    landed_at: Option<i64>,
) -> (FoldRules, FoldRules) {
    let (mut old, mut new) = (FoldRules::ALL, FoldRules::ALL);
    for rule in open {
        if landed_at.is_some_and(|at| at >= rule.mark.mark) {
            continue;
        }
        old = (rule.without)(old);
        if !targets.iter().any(|name| name == rule.name) {
            new = (rule.without)(new);
        }
    }
    (old, new)
}

/// The thresholds the rows of the retained requests stand under, read before a rules pass.
struct Bands {
    /// A list with no threshold, which bands every prompt `standard`.
    standard: PriceRevisions,
    /// The band mark's `since` and thresholds, where a mark stands.
    mark: Option<(i64, PriceRevisions)>,
    /// A band re-fold that stopped mid-way: its source, its target and the last request it moved.
    cursor: Option<(PriceRevisions, PriceRevisions, After)>,
}

impl Bands {
    fn read(conn: &Connection) -> Result<Bands, LedgerError> {
        let mark = read_meta::<Mark>(conn, PRICE_BANDS_KEY)?
            .map(|mark| Ok::<_, LedgerError>((mark.since, revisions_of(&mark.revisions)?)))
            .transpose()?;
        let cursor = read_meta::<Cursor>(conn, PRICE_BANDS_REFOLD_KEY)?
            .map(|c| {
                Ok::<_, LedgerError>((revisions_of(&c.source)?, revisions_of(&c.target)?, c.after))
            })
            .transpose()?;
        Ok(Bands {
            standard: revisions_of(&[Thresholds {
                effective: "1970-01-01".into(),
                bands: BTreeMap::new(),
            }])?,
            mark,
            cursor,
        })
    }

    /// The list `request` bands by, `None` where the thresholds it landed under are stored nowhere. A request received
    /// before the mark's `since` landed under the migration's `standard` where it landed before `since` too, and a
    /// band re-fold that stopped mid-way moved the requests up to its cursor to its target alone.
    fn of(&self, request: &RequestRecord, landed_at: Option<i64>) -> Option<&PriceRevisions> {
        let Some((since, mark)) = &self.mark else {
            return Some(&self.standard);
        };
        let since = *since;
        if request.received_at < since {
            return landed_at
                .is_none_or(|at| at < since)
                .then_some(&self.standard);
        }
        Some(match &self.cursor {
            Some((source, target, after)) => {
                if (request.received_at, request.id.as_str())
                    <= (after.received_at, after.id.as_str())
                {
                    target
                } else {
                    source
                }
            }
            None => mark,
        })
    }
}

/// The `meta` key holding the mark: `{"since": <ms>, "revisions": [{"effective": "<effective>", "bands": {"<model>":
/// <over or null>}}]}`, `since` the instant the first mark was written (the first start after the migration) and the
/// list every revision's thresholds as the last start loaded them. Requests received before `since` landed under the
/// migration's `standard` and are never re-folded: nothing is backfilled.
pub const PRICE_BANDS_KEY: &str = "price_bands";

/// The `meta` key holding a pass in progress: `{"source": [..], "target": [..], "after": {"received_at": <ms>,
/// "id": "<request id>"}}`, written by every page's transaction and deleted by the last page's, in the same
/// transaction that writes the mark. A start that finds it resumes after `after` with the cursor's source and
/// target, whatever the file holds now, then compares the finished mark with the loaded list as usual.
pub const PRICE_BANDS_REFOLD_KEY: &str = "price_bands_refold";

/// One revision's thresholds: every model it lists, each with its long-context block's `over`, `None` for a row
/// without one. A row without the block is kept, since a dated row without it beside an undated row with it bands by
/// its own row, and leaving it out would resolve it to the undated row's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Thresholds {
    effective: String,
    bands: BTreeMap<String, Option<u64>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Mark {
    since: i64,
    revisions: Vec<Thresholds>,
}

/// The last request a committed page re-folded, in the pass's order.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct After {
    received_at: i64,
    id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    source: Vec<Thresholds>,
    target: Vec<Thresholds>,
    after: After,
}

/// A rules pass in progress: the rules it folds the requests under and the last request a committed page read.
#[derive(Debug, Serialize, Deserialize)]
struct RulesCursor {
    rules: Vec<String>,
    after: After,
}

/// Re-folds the requests whose band moved since the last start, in pages of 1,000 requests ordered by
/// `(received_at, id)`, one transaction per page carrying the page's rows and its cursor, the mark rewritten and the
/// cursor cleared in the last page's transaction. Answers how many requests moved: 0 on a first start (which writes
/// the mark with `since` and moves nothing) and on an unchanged file. Rows older than the retention cutoff and rows
/// received before `since` keep their split. Under a request retention of 0 the walk reads every request. A request
/// received before the first hour boundary at or after the retention cutoff, or under a request retention of 0 before
/// the first hour the rollup purge keeps, moves only where its hour still holds the rows it folded into, so an hour
/// the purge removed stays empty and an hour it kept for a budgeted member moves.
pub fn refold_bands(
    db: &LedgerDb,
    prices: &PriceRevisions,
    retention_days: u32,
    rollup_retention_days: u32,
    now: i64,
) -> Result<usize, LedgerError> {
    let loaded = thresholds_of(prices);
    let (cutoff, guard) = walk_bounds(retention_days, rollup_retention_days, now);
    let mut moved = 0;
    let (mark, cursor) = db.write(|c| {
        Ok((
            read_meta::<Mark>(c, PRICE_BANDS_KEY)?,
            read_meta::<Cursor>(c, PRICE_BANDS_REFOLD_KEY)?,
        ))
    })?;
    let Some(mut mark) = mark else {
        db.write(|c| {
            let tx = begin_write(c)?;
            write_meta(
                &tx,
                PRICE_BANDS_KEY,
                &Mark {
                    since: now,
                    revisions: loaded,
                },
            )?;
            tx.commit()?;
            Ok(())
        })?;
        return Ok(0);
    };
    if let Some(cursor) = cursor {
        moved += pass(
            db,
            mark.since,
            cutoff,
            guard,
            cursor.source,
            cursor.target.clone(),
            Some(cursor.after),
        )?;
        mark.revisions = cursor.target;
    }
    if mark.revisions != loaded {
        moved += pass(db, mark.since, cutoff, guard, mark.revisions, loaded, None)?;
    }
    Ok(moved)
}

/// The request cutoff that bounds a re-fold's walk, and the instant before which a request moves only where its rows
/// stand.
fn walk_bounds(
    retention_days: u32,
    rollup_retention_days: u32,
    now: i64,
) -> (Option<i64>, Option<i64>) {
    // Each purge's own arithmetic, a zero keeping its rows for good. A nonzero request retention bounds the walk,
    // since the configuration holds a nonzero rollup retention at least as long as the request one, so the rollup
    // purge drops at most the hour the request cutoff splits. The first hour boundary at or after the request cutoff
    // guards each request before it rather than bounding the walk, since that hour's rows stand for every member under
    // a rollup retention of 0 or a longer one, and a budgeted member's under an equal one too. Under a request
    // retention of 0 the first hour the rollup purge keeps guards each request before it rather than bounding the walk,
    // since the purge keeps a budgeted member's hours from its period's start, before that hour, and `member_spends`
    // prices them by band.
    match (retention_days, rollup_retention_days) {
        (0, 0) => (None, None),
        (0, days) => (None, Some(first_kept_hour(now, days))),
        (days, _) => (request_cutoff(days, now), Some(first_kept_hour(now, days))),
    }
}

/// One pass from `source` to `target` over the requests received at or after the later of `since`, the cutoff and the
/// first instant the two lists' band functions differ, resuming after `after` when a start left one halfway. A request
/// received before `guard` moves only where every row its old deltas name still stands.
fn pass(
    db: &LedgerDb,
    since: i64,
    cutoff: Option<i64>,
    guard: Option<i64>,
    source: Vec<Thresholds>,
    target: Vec<Thresholds>,
    mut after: Option<After>,
) -> Result<usize, LedgerError> {
    let (old, new) = (revisions_of(&source)?, revisions_of(&target)?);
    let floor = cutoff.map_or(since, |cutoff| cutoff.max(since));
    let first = first_difference(&old, &new, floor);
    let page = page_size();
    let mut moved = 0;
    let mut pages = 0;
    loop {
        let done = db.write(|c| {
            let tx = begin_write(c)?;
            let requests = match first {
                Some(first) => requests_after(&tx, first, after.as_ref(), page)?,
                None => Vec::new(),
            };
            for (request, attempts) in &requests {
                let (was, now) = (
                    deltas_of(request, attempts, &old, FoldRules::ALL),
                    deltas_of(request, attempts, &new, FoldRules::ALL),
                );
                if was == now {
                    continue;
                }
                // The purge removes a member's hour whole, so a request whose rows are gone stays out of the hour
                // rather than writing the new band's rows into it.
                if guard.is_some_and(|guard| request.received_at < guard) && !rows_stand(&tx, &was)?
                {
                    continue;
                }
                for delta in &was {
                    unfold(&tx, delta)?;
                }
                for delta in &now {
                    fold_delta(&tx, delta)?;
                }
                moved += 1;
            }
            let last = requests.len() < page;
            if last {
                tx.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    params![PRICE_BANDS_REFOLD_KEY],
                )?;
                write_meta(
                    &tx,
                    PRICE_BANDS_KEY,
                    &Mark {
                        since,
                        revisions: target.clone(),
                    },
                )?;
            } else if let Some((request, _)) = requests.last() {
                let next = After {
                    received_at: request.received_at,
                    id: request.id.clone(),
                };
                write_meta(
                    &tx,
                    PRICE_BANDS_REFOLD_KEY,
                    &Cursor {
                        source: source.clone(),
                        target: target.clone(),
                        after: next.clone(),
                    },
                )?;
                after = Some(next);
            }
            tx.commit()?;
            Ok(last)
        })?;
        pages += 1;
        fail_after_page(pages)?;
        if done {
            return Ok(moved);
        }
    }
}

/// Whether the row each delta names stands.
fn rows_stand(conn: &Connection, deltas: &[RollupRow]) -> Result<bool, LedgerError> {
    for delta in deltas {
        if read_rollup(conn, delta)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every revision's thresholds, in the list's order.
pub(crate) fn thresholds_of(prices: &PriceRevisions) -> Vec<Thresholds> {
    prices
        .iter()
        .map(|table| Thresholds {
            effective: table.effective.clone(),
            bands: table
                .models
                .iter()
                .map(|(model, row)| {
                    (
                        model.clone(),
                        row.long_context.as_ref().map(|long| long.over),
                    )
                })
                .collect(),
        })
        .collect()
}

/// A list whose rows carry the thresholds alone, every cell zero: `deltas_of` reads a band from it and prices nothing.
pub(crate) fn revisions_of(list: &[Thresholds]) -> Result<PriceRevisions, LedgerError> {
    let tables = list
        .iter()
        .map(|revision| PriceTable {
            effective: revision.effective.clone(),
            models: revision
                .bands
                .iter()
                .map(|(model, over)| {
                    (
                        model.clone(),
                        ModelRow {
                            standard: ModelPrice::default(),
                            fast: None,
                            long_context: over.map(|over| LongContext {
                                over,
                                cells: ModelPrice::default(),
                            }),
                        },
                    )
                })
                .collect::<IndexMap<_, _>>(),
        })
        .collect();
    PriceRevisions::new(tables, "mark")
}

/// The first instant at or after `floor` at which the two lists answer another threshold for any model, `None` where
/// they agree from `floor` on.
pub(crate) fn first_difference(
    old: &PriceRevisions,
    new: &PriceRevisions,
    floor: i64,
) -> Option<i64> {
    let mut instants: Vec<i64> = old
        .instants()
        .iter()
        .chain(new.instants())
        .copied()
        .filter(|instant| *instant > floor)
        .chain(std::iter::once(floor))
        .collect();
    instants.sort_unstable();
    instants.dedup();
    // Through the exact id, then the undated one, as `band_of` resolves a row: a dated row added or removed moves the
    // dated id onto or off its undated row's threshold, which a lookup by the exact id alone reads as no change.
    let threshold = |table: &PriceTable, model: &str| {
        phoebis_routing::listing::row_for(model, |key| table.models.get(key))
            .and_then(|row| row.long_context.as_ref())
            .map(|long| long.over)
    };
    let models: BTreeSet<&str> = old
        .iter()
        .chain(new.iter())
        .flat_map(|table| table.models.keys().map(String::as_str))
        .collect();
    instants.into_iter().find(|at| {
        let (was, now) = (old.in_force(*at), new.in_force(*at));
        models
            .iter()
            .any(|model| threshold(was, model) != threshold(now, model))
    })
}

/// A page of the walk: the requests received at or after `?1`, past `(?2, ?3)`, at most `?4` of them in
/// `(received_at, id)` order.
const WALK_PAGE: &str =
    "WHERE received_at >= ?1 AND (received_at, id) > (?2, ?3) ORDER BY received_at, id LIMIT ?4";

/// The `(received_at, id)` a page starts past.
fn past(after: Option<&After>) -> (i64, &str) {
    after.map_or((i64::MIN, ""), |a| (a.received_at, a.id.as_str()))
}

/// The next page of requests received at or after `from`, past `after`, in `(received_at, id)` order, each with its
/// attempts.
fn requests_after(
    conn: &Connection,
    from: i64,
    after: Option<&After>,
    page: usize,
) -> Result<Vec<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
    let (after_at, after_id) = past(after);
    let mut stmt = conn.prepare(&format!("{REQUEST_SELECT} {WALK_PAGE}"))?;
    let requests = stmt
        .query_map(
            params![from, after_at, after_id, page as i64],
            row_to_request,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    with_attempts(conn, requests)
}

/// The `landed_at` of each request `requests_after` answers for the same arguments, in its order: run in the same
/// transaction, the two statements read the same rows.
fn landed_after(
    conn: &Connection,
    from: i64,
    after: Option<&After>,
    page: usize,
) -> Result<Vec<Option<i64>>, LedgerError> {
    let (after_at, after_id) = past(after);
    let mut stmt = conn.prepare(&format!("SELECT landed_at FROM requests {WALK_PAGE}"))?;
    let stamps = stmt
        .query_map(params![from, after_at, after_id, page as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(stamps)
}

pub(crate) fn read_meta<T: for<'de> Deserialize<'de>>(
    conn: &Connection,
    key: &str,
) -> Result<Option<T>, LedgerError> {
    let text = phoebis_store::migrations::meta_value(conn, key)?;
    Ok(text.map(|text| serde_json::from_str(&text)).transpose()?)
}

pub(crate) fn write_meta(
    tx: &Transaction<'_>,
    key: &str,
    value: &impl Serialize,
) -> Result<(), LedgerError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn page_size() -> usize {
    PAGE
}

#[cfg(test)]
pub(crate) fn page_size() -> usize {
    TEST_PAGE.get().unwrap_or(PAGE)
}

#[cfg(not(test))]
pub(crate) fn fail_after_page(_pages: usize) -> Result<(), LedgerError> {
    Ok(())
}

/// The failure a test injects once a pass has committed the pages it names, so the resume has a pass to finish.
#[cfg(test)]
pub(crate) fn fail_after_page(pages: usize) -> Result<(), LedgerError> {
    if FAIL_AFTER_PAGES.get() == Some(pages) {
        return Err(LedgerError::Io(std::io::Error::other(
            "injected failure after a committed page",
        )));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The page size a test sets in place of `PAGE`, so a pass of three requests runs three pages.
    pub(crate) static TEST_PAGE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// The pages a pass commits before it fails, which a test sets to interrupt a pass after a committed page.
    pub(crate) static FAIL_AFTER_PAGES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::insert_request_in;
    use crate::histogram::Histogram;
    use crate::pricing::{Band, PriceTable, Speed};
    use crate::query::{member_spends, BudgetedMember};
    use crate::record::{AttemptRecord, RequestRecord};
    use crate::rollup::{read_rollups, RollupRow, RollupSelect, LONG_BAND, STANDARD_BAND};
    use phoebis_store::AccountKind;

    const HAIKU: &str = "claude-haiku-5-5";
    const DAY: i64 = 86_400_000;
    /// 2026-01-01 00:00 UTC, the test list's one entry.
    const NEW_YEAR: i64 = 1_767_225_600_000;

    /// The test fixture's first entry alone, its Haiku 5.5 threshold at `over`, or with no long-context block.
    fn list(over: Option<u64>) -> PriceRevisions {
        let mut table: PriceTable = crate::test_tables::api_prices().in_force(NEW_YEAR).clone();
        let row = table
            .models
            .get_mut(HAIKU)
            .expect("the fixture lists Haiku 5.5");
        match over {
            Some(over) => {
                row.long_context
                    .as_mut()
                    .expect("the fixture prices Haiku 5.5 by prompt length")
                    .over = over;
            }
            None => row.long_context = None,
        }
        PriceRevisions::new(vec![table], "test").expect("one entry")
    }

    /// A Haiku 5.5 request with one answered attempt of `prompt` uncached input tokens.
    fn haiku(id: &str, received_at: i64, prompt: u64) -> (RequestRecord, Vec<AttemptRecord>) {
        (
            RequestRecord {
                id: id.into(),
                received_at,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                model: HAIKU.into(),
                canonical_model: HAIKU.into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            vec![AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: AccountKind::Oauth,
                started_at: received_at,
                outcome: "succeeded".into(),
                input_tokens: Some(prompt),
                output_tokens: Some(10),
                ..Default::default()
            }],
        )
    }

    fn land(db: &LedgerDb, request: &(RequestRecord, Vec<AttemptRecord>), prices: &PriceRevisions) {
        db.insert_request_at(&request.0, &request.1, None, prices)
            .unwrap();
    }

    fn rows(db: &LedgerDb) -> Vec<RollupRow> {
        db.read_blocking(|c| read_rollups(c, i64::MIN, i64::MAX, &RollupSelect::default()))
            .unwrap()
    }

    /// Each row's hour and band with its request and token counts.
    fn bands(db: &LedgerDb) -> Vec<(i64, String, u64, u64)> {
        rows(db)
            .into_iter()
            .map(|r| (r.hour, r.band, r.requests, r.input_tokens))
            .collect()
    }

    fn meta(db: &LedgerDb, key: &str) -> Option<serde_json::Value> {
        db.read_blocking(|c| read_meta(c, key)).unwrap()
    }

    /// A page's attempts land under their own request in attempt order: a failover's two among requests with one, its
    /// second attempt written first.
    #[test]
    fn a_page_files_each_attempt_under_its_request_in_attempt_order() {
        let db = LedgerDb::open_in_memory().unwrap();
        let prices = list(None);
        land(&db, &haiku("r1", NEW_YEAR + 1, 10), &prices);
        let (request, mut attempts) = haiku("r2", NEW_YEAR + 2, 10);
        attempts.insert(
            0,
            AttemptRecord {
                request_id: "r2".into(),
                attempt: 2,
                account_id: "b".into(),
                started_at: NEW_YEAR + 3,
                outcome: "succeeded".into(),
                ..Default::default()
            },
        );
        land(&db, &(request, attempts), &prices);
        let (request, mut attempts) = haiku("r3", NEW_YEAR + 3, 10);
        attempts[0].account_id = "c".into();
        land(&db, &(request, attempts), &prices);
        let page = db
            .read_blocking(|c| requests_after(c, NEW_YEAR, None, 10))
            .unwrap();
        let filed: Vec<(&str, Vec<(u32, &str)>)> = page
            .iter()
            .map(|(r, attempts)| {
                (
                    r.id.as_str(),
                    attempts
                        .iter()
                        .map(|a| (a.attempt, a.account_id.as_str()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            filed,
            [
                ("r1", vec![(1, "a")]),
                ("r2", vec![(1, "a"), (2, "b")]),
                ("r3", vec![(1, "c")]),
            ]
        );
    }

    /// The thresholds a mark or a cursor holds for Haiku 5.5 in its one entry.
    fn haiku_over(list: &serde_json::Value) -> Option<u64> {
        list[0]["bands"][HAIKU].as_u64()
    }

    /// A threshold that moves re-folds the requests the ledger still holds past the retention cutoff: a prompt of
    /// 1,500 tokens moves from `long` to `standard` when `over` goes from 1,000 to 2,000, and the request older than
    /// the cutoff keeps its split.
    #[test]
    fn a_moved_threshold_refolds_the_rows_past_the_cutoff_and_leaves_the_older_ones() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 20 * DAY);
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 7, 0, first).unwrap(),
            0,
            "the first start writes the mark"
        );
        let (older, newer) = (now - 10 * DAY, now - DAY);
        land(&db, &haiku("old", older, 1_500), &list(Some(1_000)));
        land(&db, &haiku("new", newer, 1_500), &list(Some(1_000)));
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 7, 0, now).unwrap(),
            1,
            "one request moved"
        );
        assert_eq!(
            bands(&db),
            [
                (
                    crate::rollup::hour_of(older),
                    LONG_BAND.to_string(),
                    1,
                    1_500
                ),
                (
                    crate::rollup::hour_of(newer),
                    STANDARD_BAND.to_string(),
                    1,
                    1_500
                ),
            ]
        );
        let mark = meta(&db, PRICE_BANDS_KEY).expect("the mark");
        assert_eq!(
            (mark["since"].as_i64(), haiku_over(&mark["revisions"])),
            (Some(first), Some(2_000)),
            "the mark keeps its first start and holds the loaded thresholds"
        );
    }

    const DATED: &str = "claude-haiku-5-5-20260101";

    /// `list(Some(over))` with a row of its own for `DATED`: Haiku 5.5's cells without the long-context block.
    fn with_dated(over: u64) -> PriceRevisions {
        let mut table = list(Some(over)).in_force(NEW_YEAR).clone();
        let mut row = table.models[HAIKU].clone();
        row.long_context = None;
        table.models.insert(DATED.into(), row);
        PriceRevisions::new(vec![table], "test").expect("one entry")
    }

    /// `haiku` on `DATED`.
    fn on_dated(id: &str, received_at: i64, prompt: u64) -> (RequestRecord, Vec<AttemptRecord>) {
        let (mut request, attempts) = haiku(id, received_at, prompt);
        request.model = DATED.into();
        request.canonical_model = DATED.into();
        (request, attempts)
    }

    /// A dated id the file lists without a long-context block, beside its undated id with one, bands by its own row,
    /// so a threshold that moves on the undated id leaves the dated id's request where it landed.
    #[test]
    fn a_dated_row_without_a_block_keeps_its_band_when_its_undated_rows_threshold_moves() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        assert_eq!(
            refold_bands(&db, &with_dated(1_000), 0, 0, first).unwrap(),
            0
        );
        land(
            &db,
            &on_dated("r", first + 1_000, 1_500),
            &with_dated(1_000),
        );
        let before = bands(&db);
        assert_eq!(
            before,
            [(
                crate::rollup::hour_of(first + 1_000),
                STANDARD_BAND.to_string(),
                1,
                1_500
            )]
        );
        assert_eq!(
            refold_bands(&db, &with_dated(2_000), 0, 0, now).unwrap(),
            0,
            "no request moved"
        );
        assert_eq!(bands(&db), before);
    }

    /// Removing a dated row without a long-context block hands the dated id to its undated row's threshold, which an
    /// unchanged undated row leaves where it was: the pass still re-folds the dated id's request above it.
    #[test]
    fn removing_a_dated_row_without_a_block_refolds_its_requests_under_the_undated_threshold() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        assert_eq!(
            refold_bands(&db, &with_dated(1_000), 0, 0, first).unwrap(),
            0
        );
        let received = first + 1_000;
        land(&db, &on_dated("r", received, 1_500), &with_dated(1_000));
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 0, now).unwrap(),
            1,
            "the dated id's request moved"
        );
        assert_eq!(
            bands(&db),
            [(
                crate::rollup::hour_of(received),
                LONG_BAND.to_string(),
                1,
                1_500
            )]
        );
    }

    /// Adding a dated row without a long-context block takes the dated id off its undated row's threshold, so the
    /// pass re-folds the dated id's request that landed above it.
    #[test]
    fn adding_a_dated_row_without_a_block_refolds_its_requests_out_of_the_undated_threshold() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (first, now) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 0, first).unwrap(),
            0
        );
        let received = first + 1_000;
        land(&db, &on_dated("r", received, 1_500), &list(Some(1_000)));
        assert_eq!(
            refold_bands(&db, &with_dated(1_000), 0, 0, now).unwrap(),
            1,
            "the dated id's request moved"
        );
        assert_eq!(
            bands(&db),
            [(
                crate::rollup::hour_of(received),
                STANDARD_BAND.to_string(),
                1,
                1_500
            )]
        );
    }

    /// A first start writes the mark with `since` at its clock and moves nothing, and the same list again moves
    /// nothing.
    #[test]
    fn a_first_start_writes_the_mark_and_an_unchanged_list_moves_nothing() {
        let db = LedgerDb::open_in_memory().unwrap();
        land(&db, &haiku("r", NEW_YEAR + DAY, 1_500), &list(Some(1_000)));
        let before = rows(&db);
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 0, NEW_YEAR + 2 * DAY).unwrap(),
            0
        );
        let mark = meta(&db, PRICE_BANDS_KEY).expect("the first start writes the mark");
        assert_eq!(
            (mark["since"].as_i64(), haiku_over(&mark["revisions"])),
            (Some(NEW_YEAR + 2 * DAY), Some(2_000))
        );
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 0, NEW_YEAR + 3 * DAY).unwrap(),
            0
        );
        assert_eq!(rows(&db), before, "nothing moved: nothing is backfilled");
    }

    /// The retention purge at `now` under the two retentions, the `budgeted` members' periods kept, over requests
    /// landed at the thresholds of `prices`.
    fn purge(
        db: &LedgerDb,
        retention_days: u32,
        rollup_retention_days: u32,
        now: i64,
        budgeted: &[BudgetedMember],
        prices: &PriceRevisions,
    ) {
        let config = crate::LedgerConfig {
            retention_days,
            rollup_retention_days,
            ..Default::default()
        };
        crate::bodies::run_retention_blocking(db, &config, now, budgeted, prices).unwrap();
    }

    /// A request retention of 0 keeps the requests for good while a rollup retention of seven days drops whole hours:
    /// the purge removes the hour its cutoff splits, and the re-fold writes no row back into it for the request
    /// received there after the cutoff, while the request inside the first kept hour moves. With both retentions 0
    /// nothing is purged and every request moves.
    #[test]
    fn under_a_zero_request_retention_an_hour_the_rollup_purge_removed_stays_empty_through_the_refold(
    ) {
        const HOUR: i64 = 3_600_000;
        // The purge's cutoff, seven days back from `now`, falls half an hour into `split_hour`.
        let split_hour = NEW_YEAR + 13 * DAY;
        let now = split_hour + 7 * DAY + HOUR / 2;
        let (dropped, kept) = (split_hour + 3 * HOUR / 4, split_hour + HOUR + 60_000);
        let refolded = |rollup_retention_days: u32| {
            let db = LedgerDb::open_in_memory().unwrap();
            assert_eq!(
                refold_bands(
                    &db,
                    &list(Some(1_000)),
                    0,
                    rollup_retention_days,
                    NEW_YEAR + DAY
                )
                .unwrap(),
                0,
                "the first start writes the mark"
            );
            land(&db, &haiku("dropped", dropped, 1_500), &list(Some(1_000)));
            land(&db, &haiku("kept", kept, 1_500), &list(Some(1_000)));
            purge(&db, 0, rollup_retention_days, now, &[], &list(Some(1_000)));
            let moved =
                refold_bands(&db, &list(Some(2_000)), 0, rollup_retention_days, now).unwrap();
            (moved, bands(&db))
        };
        let band = |received_at: i64, band: &str| {
            (
                crate::rollup::hour_of(received_at),
                band.to_string(),
                1,
                1_500,
            )
        };
        assert_eq!(
            refolded(7),
            (1, vec![band(kept, STANDARD_BAND)]),
            "the hour the purge removed gets no row back"
        );
        assert_eq!(
            refolded(0),
            (
                2,
                vec![band(dropped, STANDARD_BAND), band(kept, STANDARD_BAND)]
            ),
            "with both retentions 0 every request moves"
        );
    }

    /// Under a request retention of 0 and a rollup retention of seven days the purge keeps a monthly budgeted
    /// member's hours from the month's first while it removes another member's hour beside them. A threshold that
    /// moves re-folds the kept hour, which `member_spends` then prices at the new band, and writes no row back into
    /// the other member's.
    #[test]
    fn under_a_zero_request_retention_a_budgeted_members_kept_hours_move_with_the_threshold() {
        const HOUR: i64 = 3_600_000;
        let db = LedgerDb::open_in_memory().unwrap();
        let now = NEW_YEAR + 20 * DAY;
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 7, NEW_YEAR + DAY).unwrap(),
            0,
            "the first start writes the mark"
        );
        let day_four = NEW_YEAR + 4 * DAY + HOUR / 2;
        land(&db, &haiku("budgeted", day_four, 1_500), &list(Some(1_000)));
        let mut other = haiku("other", day_four + 1_000, 1_500);
        other.0.pool_id = Some("q".into());
        other.1[0].account_id = "b".into();
        land(&db, &other, &list(Some(1_000)));
        let budgeted = [BudgetedMember {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: phoebis_store::BudgetPeriod::Month,
            metered_only: false,
        }];
        purge(&db, 0, 7, now, &budgeted, &list(Some(1_000)));
        let hour = crate::rollup::hour_of(day_four);
        assert_eq!(
            bands(&db),
            [(hour, LONG_BAND.to_string(), 1, 1_500)],
            "the purge kept the budgeted member's hour and removed the other member's"
        );
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 7, now).unwrap(),
            1,
            "the budgeted member's request moved"
        );
        assert_eq!(
            bands(&db),
            [(hour, STANDARD_BAND.to_string(), 1, 1_500)],
            "the kept hour moved to the new band and the removed hour got no row back"
        );
        let prices = list(Some(2_000));
        let usage = phoebis_routing::PinUsage {
            input: 1_500,
            output: 10,
            ..Default::default()
        };
        let at_band = |band: Band| {
            prices
                .in_force(now)
                .estimate_banded(HAIKU, Speed::Standard, band, usage)
                .expect("the list prices Haiku 5.5")
                .total()
        };
        assert_ne!(
            at_band(Band::Standard),
            at_band(Band::LongContext),
            "the two bands price the prompt apart"
        );
        let spent = db
            .read_blocking(|c| member_spends(c, &budgeted, now, &prices))
            .unwrap();
        assert_eq!(
            spent.iter().map(|s| s.spent_usd).collect::<Vec<_>>(),
            [at_band(Band::Standard)],
            "the month's spend prices the kept hour at the new band"
        );
    }

    const HOUR: i64 = 3_600_000;
    /// The hour a seven-day cutoff back from `SPLIT_NOW` falls half an hour into.
    const SPLIT_HOUR: i64 = NEW_YEAR + 13 * DAY;
    const SPLIT_NOW: i64 = SPLIT_HOUR + 7 * DAY + HOUR / 2;
    /// A request received inside the split hour after the cutoff, and one inside the next hour.
    const IN_THE_SPLIT_HOUR: i64 = SPLIT_HOUR + 3 * HOUR / 4;
    const IN_THE_NEXT_HOUR: i64 = SPLIT_HOUR + HOUR + 60_000;

    /// The ledger under a request retention of seven days and `rollup_retention_days` as the purge at `SPLIT_NOW`
    /// leaves it, the `budgeted` members' periods kept, re-folded once the threshold moves. Answers how many requests
    /// moved and the rows' bands.
    fn refolded_past_a_seven_day_cutoff(
        rollup_retention_days: u32,
        budgeted: &[BudgetedMember],
    ) -> (usize, Vec<(i64, String, u64, u64)>) {
        let db = LedgerDb::open_in_memory().unwrap();
        assert_eq!(
            refold_bands(
                &db,
                &list(Some(1_000)),
                7,
                rollup_retention_days,
                NEW_YEAR + DAY
            )
            .unwrap(),
            0,
            "the first start writes the mark"
        );
        land(
            &db,
            &haiku("split", IN_THE_SPLIT_HOUR, 1_500),
            &list(Some(1_000)),
        );
        land(
            &db,
            &haiku("next", IN_THE_NEXT_HOUR, 1_500),
            &list(Some(1_000)),
        );
        purge(
            &db,
            7,
            rollup_retention_days,
            SPLIT_NOW,
            budgeted,
            &list(Some(1_000)),
        );
        let moved =
            refold_bands(&db, &list(Some(2_000)), 7, rollup_retention_days, SPLIT_NOW).unwrap();
        (moved, bands(&db))
    }

    fn standard(received_at: i64) -> (i64, String, u64, u64) {
        (
            crate::rollup::hour_of(received_at),
            STANDARD_BAND.to_string(),
            1,
            1_500,
        )
    }

    /// Under equal request and rollup retentions the rollup purge drops the hour the request cutoff splits while the
    /// requests received after the cutoff stand: the re-fold writes no row back into that hour, and the request in the
    /// next hour moves.
    #[test]
    fn under_equal_retentions_the_hour_the_rollup_purge_dropped_at_the_request_cutoff_stays_empty_through_the_refold(
    ) {
        assert_eq!(
            refolded_past_a_seven_day_cutoff(7, &[]),
            (1, vec![standard(IN_THE_NEXT_HOUR)]),
            "the dropped hour gets no row back"
        );
    }

    /// Under equal retentions the purge keeps a monthly budgeted member's hour at the request cutoff, so the request
    /// received there after the cutoff moves with the threshold beside the one in the next hour.
    #[test]
    fn under_equal_retentions_a_budgeted_members_kept_hour_at_the_request_cutoff_moves_with_the_threshold(
    ) {
        let budgeted = [BudgetedMember {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: phoebis_store::BudgetPeriod::Month,
            metered_only: false,
        }];
        assert_eq!(
            refolded_past_a_seven_day_cutoff(7, &budgeted),
            (
                2,
                vec![standard(IN_THE_SPLIT_HOUR), standard(IN_THE_NEXT_HOUR)]
            ),
            "the kept hour moved"
        );
    }

    /// With the rollups kept for good the hour the request cutoff splits keeps its rows, so the request received there
    /// after the cutoff moves with the threshold.
    #[test]
    fn under_a_zero_rollup_retention_the_request_in_the_hour_the_request_cutoff_splits_moves_with_the_threshold(
    ) {
        assert_eq!(
            refolded_past_a_seven_day_cutoff(0, &[]),
            (
                2,
                vec![standard(IN_THE_SPLIT_HOUR), standard(IN_THE_NEXT_HOUR)]
            ),
            "the split hour's rows stand, so its request moved"
        );
    }

    /// A pass that fails after a committed page leaves its cursor and the old mark, and the next start resumes
    /// after the cursor with the cursor's own lists rather than subtracting the committed page again: the rows then
    /// equal an uninterrupted pass's.
    #[test]
    fn a_pass_that_fails_after_a_committed_page_resumes_after_its_cursor_and_sums_as_an_uninterrupted_pass_does(
    ) {
        let seed = |db: &LedgerDb| {
            assert_eq!(
                refold_bands(db, &list(Some(1_000)), 0, 0, NEW_YEAR).unwrap(),
                0
            );
            for (n, id) in ["a", "b", "c"].into_iter().enumerate() {
                land(
                    db,
                    &haiku(id, NEW_YEAR + (n as i64 + 1) * DAY, 1_500),
                    &list(Some(1_000)),
                );
            }
        };
        let whole = LedgerDb::open_in_memory().unwrap();
        seed(&whole);
        assert_eq!(
            refold_bands(&whole, &list(Some(2_000)), 0, 0, NEW_YEAR + 9 * DAY).unwrap(),
            3
        );
        let interrupted = LedgerDb::open_in_memory().unwrap();
        seed(&interrupted);
        TEST_PAGE.set(Some(1));
        FAIL_AFTER_PAGES.set(Some(1));
        let failed = refold_bands(&interrupted, &list(Some(2_000)), 0, 0, NEW_YEAR + 9 * DAY);
        FAIL_AFTER_PAGES.set(None);
        assert!(failed.is_err(), "the injected failure after the first page");
        let cursor =
            meta(&interrupted, PRICE_BANDS_REFOLD_KEY).expect("the cursor of the committed page");
        assert_eq!(
            (
                cursor["after"]["id"].as_str(),
                haiku_over(&cursor["source"]),
                haiku_over(&cursor["target"])
            ),
            (Some("a"), Some(1_000), Some(2_000))
        );
        assert_eq!(
            haiku_over(&meta(&interrupted, PRICE_BANDS_KEY).expect("the mark")["revisions"]),
            Some(1_000),
            "the mark still holds the source"
        );
        assert_eq!(
            refold_bands(&interrupted, &list(Some(2_000)), 0, 0, NEW_YEAR + 10 * DAY).unwrap(),
            2,
            "the resumed pass moves the two requests after the cursor"
        );
        TEST_PAGE.set(None);
        assert_eq!(
            meta(&interrupted, PRICE_BANDS_REFOLD_KEY),
            None,
            "the cursor is cleared"
        );
        assert_eq!(
            haiku_over(&meta(&interrupted, PRICE_BANDS_KEY).expect("the mark")["revisions"]),
            Some(2_000)
        );
        assert_eq!(rows(&interrupted), rows(&whole));
    }

    /// Requests received before the mark's `since` landed under the migration's `standard` and never used an older
    /// band, so a threshold correction after the upgrade leaves them where they are: the pass reads only requests
    /// received at or after `since`, and no token count over the table changes.
    #[test]
    fn an_upgrade_then_a_threshold_correction_leaves_the_rows_folded_before_the_mark_alone() {
        let db = LedgerDb::open_in_memory().unwrap();
        // Folded as the migration's default rows file them: under `standard`, which a list with no long-context
        // block selects for every prompt.
        land(&db, &haiku("before", NEW_YEAR + DAY, 1_500), &list(None));
        let upgrade = NEW_YEAR + 2 * DAY;
        assert_eq!(
            refold_bands(&db, &list(Some(1_000)), 0, 0, upgrade).unwrap(),
            0
        );
        land(
            &db,
            &haiku("after", NEW_YEAR + 3 * DAY, 1_500),
            &list(Some(1_000)),
        );
        let tokens = |db: &LedgerDb| -> u64 { rows(db).iter().map(|r| r.input_tokens).sum() };
        let total = tokens(&db);
        assert_eq!(
            refold_bands(&db, &list(Some(2_000)), 0, 0, NEW_YEAR + 4 * DAY).unwrap(),
            1,
            "only the request after the mark moves"
        );
        assert_eq!(tokens(&db), total, "no count was doubled or lost");
        assert_eq!(
            bands(&db),
            [
                (
                    crate::rollup::hour_of(NEW_YEAR + DAY),
                    STANDARD_BAND.to_string(),
                    1,
                    1_500
                ),
                (
                    crate::rollup::hour_of(NEW_YEAR + 3 * DAY),
                    STANDARD_BAND.to_string(),
                    1,
                    1_500
                ),
            ]
        );
    }

    /// The fold of a build that held none of the rules.
    const NO_RULES: FoldRules = FoldRules {
        throughput_floor: false,
        wide_edges: false,
        slow_down_hold: false,
        hit_split: false,
    };

    /// `request` written with its landing stamp from `now` and folded under `rules` at the thresholds of `prices`,
    /// as a build that applied those rules filed it.
    fn store(
        db: &LedgerDb,
        request: &(RequestRecord, Vec<AttemptRecord>),
        prices: &PriceRevisions,
        rules: FoldRules,
        now: i64,
    ) {
        db.write(|c| {
            let tx = begin_write(c)?;
            insert_request_in(&tx, &request.0, &request.1, None, now)?;
            for delta in deltas_of(&request.0, &request.1, prices, rules) {
                fold_delta(&tx, &delta)?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    }

    /// `haiku`'s request with its first byte at `ttfb_ms`, its first content at `ttfc_ms` and its end at
    /// `duration_ms`.
    fn timed(
        id: &str,
        received_at: i64,
        prompt: u64,
        (ttfb_ms, ttfc_ms, duration_ms): (i64, i64, i64),
    ) -> (RequestRecord, Vec<AttemptRecord>) {
        let (mut request, attempts) = haiku(id, received_at, prompt);
        request.ttfb_ms = Some(ttfb_ms);
        request.ttfc_ms = Some(ttfc_ms);
        request.duration_ms = Some(duration_ms);
        (request, attempts)
    }

    /// `request` held `held_ms` by the slow-down.
    fn slowed(
        mut request: (RequestRecord, Vec<AttemptRecord>),
        held_ms: u64,
    ) -> (RequestRecord, Vec<AttemptRecord>) {
        request.0.held = Some(format!(
            r#"{{"waits":[{{"reason":"slow_down","held_ms":{held_ms}}}]}}"#
        ));
        request
    }

    /// The row of the hour holding `received_at` in `band`.
    fn row(db: &LedgerDb, received_at: i64, band: &str) -> RollupRow {
        let hour = crate::rollup::hour_of(received_at);
        rows(db)
            .into_iter()
            .find(|r| r.hour == hour && r.band == band)
            .expect("the hour's row in the band")
    }

    /// A histogram holding `values`.
    fn sampled(values: &[f64]) -> Histogram {
        let mut h = Histogram::new();
        for value in values {
            h.record(*value);
        }
        h
    }

    /// Ten tokens over 40 ms read as 250 a second under no rule.
    #[test]
    fn the_rules_pass_takes_a_short_spans_tokens_per_second_sample_out_of_a_retained_hour() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let request = timed("r", at, 100, (200, 1_000, 1_040));
        store(&db, &request, &list(Some(1_000)), NO_RULES, 0);
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(row(&db, at, STANDARD_BAND).output_tps_hist.count(), 0);
    }

    #[test]
    fn the_rules_pass_moves_a_retained_sample_past_100_seconds_out_of_bucket_63() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let request = timed("r", at, 100, (200, 1_000, 150_000));
        store(&db, &request, &list(Some(1_000)), NO_RULES, 0);
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(
            row(&db, at, STANDARD_BAND).duration_hist,
            sampled(&[150_000.0])
        );
    }

    #[test]
    fn the_rules_pass_moves_a_retained_held_requests_first_byte_to_its_time_less_the_hold() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let request = slowed(timed("r", at, 100, (21_000, 21_500, 30_000)), 20_000);
        store(&db, &request, &list(Some(1_000)), NO_RULES, 0);
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(row(&db, at, STANDARD_BAND).ttfb_hist, sampled(&[1_000.0]));
    }

    /// The late request's 40 ms span reads 250 tokens a second under no rule, the rate the other request of its
    /// hour records, which no rule moves.
    #[test]
    fn the_rules_pass_leaves_a_request_that_landed_after_the_marks_as_folded() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let mut other = timed("a", at, 100, (100, 500, 1_500));
        other.1[0].output_tokens = Some(250);
        store(&db, &other, &list(Some(1_000)), NO_RULES, 0);
        mark_fold_rules(&db).unwrap();
        let late = timed("b", at + 1_000, 100, (200, 1_000, 1_040));
        store(&db, &late, &list(Some(1_000)), FoldRules::ALL, 0);
        let histograms =
            |r: RollupRow| (r.ttfb_hist, r.ttfc_hist, r.duration_hist, r.output_tps_hist);
        let folded = histograms(row(&db, at, STANDARD_BAND));
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(histograms(row(&db, at, STANDARD_BAND)), folded);
    }

    /// The state a pass that stopped after its first page leaves: the first request as the pass moved it, the second
    /// as the old fold filed it and the cursor at the first, beside a request of the same hour that no rule moves.
    #[test]
    fn a_rules_pass_resumes_after_its_cursor_and_moves_each_request_once() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let first = slowed(timed("r1", at, 100, (21_000, 21_500, 30_000)), 20_000);
        let second = slowed(
            timed("r2", at + 1_000, 100, (32_000, 32_500, 40_000)),
            30_000,
        );
        let other = timed("r3", at + 2_000, 100, (5_000, 5_500, 9_000));
        store(&db, &first, &list(Some(1_000)), FoldRules::ALL, 0);
        store(&db, &second, &list(Some(1_000)), NO_RULES, 0);
        store(&db, &other, &list(Some(1_000)), NO_RULES, 0);
        mark_fold_rules(&db).unwrap();
        db.write(|c| {
            let tx = begin_write(c)?;
            write_meta(
                &tx,
                FOLD_RULES_REFOLD_KEY,
                &RulesCursor {
                    rules: vec![
                        "hit_split".into(),
                        "slow_down_hold".into(),
                        "throughput_floor".into(),
                        "wide_edges".into(),
                    ],
                    after: After {
                        received_at: at,
                        id: "r1".into(),
                    },
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(
            row(&db, at, STANDARD_BAND).ttfb_hist,
            sampled(&[1_000.0, 2_000.0, 5_000.0])
        );
    }

    /// One start with a moved threshold and an open rule: the held request's 1,500 prompt tokens fell in the long
    /// band at 1,000 and fall in the standard one at 2,000, while the long row's other request of 2,500 and the
    /// standard row's of 500 stay where they are.
    #[test]
    fn the_rules_pass_folds_under_the_stored_thresholds_before_the_band_refold_moves_the_request() {
        let db = LedgerDb::open_in_memory().unwrap();
        let now = NEW_YEAR + 3 * DAY;
        refold_bands(&db, &list(Some(1_000)), 0, 0, NEW_YEAR + DAY).unwrap();
        let at = NEW_YEAR + 2 * DAY;
        let held = slowed(timed("r", at, 1_500, (21_000, 21_500, 30_000)), 20_000);
        let long = timed("l", at + 1_000, 2_500, (5_000, 5_500, 9_000));
        let short = timed("s", at + 2_000, 500, (6_000, 6_500, 9_000));
        for request in [&held, &long, &short] {
            store(&db, request, &list(Some(1_000)), NO_RULES, 0);
        }
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, now).unwrap();
        refold_bands(&db, &list(Some(2_000)), 0, 0, now).unwrap();
        assert_eq!(
            (
                row(&db, at, STANDARD_BAND).ttfb_hist,
                row(&db, at, LONG_BAND).ttfb_hist
            ),
            (sampled(&[1_000.0, 6_000.0]), sampled(&[5_000.0]))
        );
    }

    /// 2100-01-01, past the start's clock and the host's.
    const AHEAD: i64 = 4_102_444_800_000;

    /// A landing before a clock set back leaves a stamp ahead of the clock.
    #[test]
    fn the_rules_pass_folds_again_a_request_whose_stamp_lies_ahead_of_the_clock() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let request = timed("r", at, 100, (200, 1_000, 1_040));
        store(&db, &request, &list(Some(1_000)), NO_RULES, AHEAD);
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 7, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(row(&db, at, STANDARD_BAND).output_tps_hist.count(), 0);
    }

    /// The state a band re-fold that stopped after its first page leaves, from 1,000 to 2,000: the first held request
    /// of 1,500 prompt tokens in the standard band, as the pass moved it, the second a day later in the long band,
    /// each folded under no rule, and a request that stays in each row. The start then loads 1,200, which bands
    /// 1,500 long.
    #[test]
    fn the_rules_pass_bands_each_request_by_the_side_of_a_stopped_band_refolds_cursor_it_stands_on()
    {
        let db = LedgerDb::open_in_memory().unwrap();
        let (day1, day2) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        let (source, target) = (list(Some(1_000)), list(Some(2_000)));
        let first = slowed(timed("r1", day1, 1_500, (21_000, 21_500, 30_000)), 20_000);
        let second = slowed(timed("r2", day2, 1_500, (22_000, 22_500, 30_000)), 20_000);
        store(&db, &first, &target, NO_RULES, 0);
        store(&db, &second, &source, NO_RULES, 0);
        for request in [
            timed("s1", day1 + 1_000, 500, (6_000, 6_500, 9_000)),
            timed("l1", day1 + 2_000, 2_500, (5_000, 5_500, 9_000)),
            timed("s2", day2 + 1_000, 500, (6_000, 6_500, 9_000)),
            timed("l2", day2 + 2_000, 2_500, (5_000, 5_500, 9_000)),
        ] {
            store(&db, &request, &source, NO_RULES, 0);
        }
        db.write(|c| {
            let tx = begin_write(c)?;
            write_meta(
                &tx,
                PRICE_BANDS_KEY,
                &Mark {
                    since: NEW_YEAR,
                    revisions: thresholds_of(&source),
                },
            )?;
            write_meta(
                &tx,
                PRICE_BANDS_REFOLD_KEY,
                &Cursor {
                    source: thresholds_of(&source),
                    target: thresholds_of(&target),
                    after: After {
                        received_at: day1,
                        id: "r1".into(),
                    },
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let now = NEW_YEAR + 3 * DAY;
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, now).unwrap();
        refold_bands(&db, &list(Some(1_200)), 0, 0, now).unwrap();
        let ttfb = |at: i64| {
            (
                row(&db, at, LONG_BAND).ttfb_hist,
                row(&db, at, STANDARD_BAND).ttfb_hist,
            )
        };
        assert_eq!(
            (ttfb(day1), ttfb(day2)),
            (
                (sampled(&[1_000.0, 5_000.0]), sampled(&[6_000.0])),
                (sampled(&[2_000.0, 5_000.0]), sampled(&[6_000.0]))
            )
        );
    }

    /// The state a pass that stopped before the other rules were marked leaves: its cursor names the slow-down
    /// hold alone and stands at the first held request, which it folded under the hold alone, and the second held
    /// request stands as the old fold filed it, beside a request that no rule moves. Each held request's duration lies
    /// past 100,000 once the hold of 20,000 comes out.
    #[test]
    fn a_resumed_pass_folds_for_its_cursors_rules_and_a_second_pass_for_the_other_open_rules() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let first = slowed(timed("r1", at, 100, (21_000, 21_500, 150_000)), 20_000);
        let second = slowed(
            timed("r2", at + 1_000, 100, (22_000, 22_500, 160_000)),
            20_000,
        );
        let other = timed("r3", at + 2_000, 100, (5_000, 5_500, 9_000));
        let hold_alone = FoldRules {
            slow_down_hold: true,
            ..NO_RULES
        };
        store(&db, &first, &list(Some(1_000)), hold_alone, 0);
        store(&db, &second, &list(Some(1_000)), NO_RULES, 0);
        store(&db, &other, &list(Some(1_000)), NO_RULES, 0);
        mark_fold_rules(&db).unwrap();
        db.write(|c| {
            let tx = begin_write(c)?;
            write_meta(
                &tx,
                FOLD_RULES_REFOLD_KEY,
                &RulesCursor {
                    rules: vec!["slow_down_hold".into()],
                    after: After {
                        received_at: at,
                        id: "r1".into(),
                    },
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        let hour = row(&db, at, STANDARD_BAND);
        assert_eq!(
            (hour.duration_hist, hour.ttfb_hist),
            (
                sampled(&[130_000.0, 140_000.0, 9_000.0]),
                sampled(&[1_000.0, 2_000.0, 5_000.0])
            ),
            "each held request once, less the hold, with the rules the cursor does not name applied to both"
        );
    }

    /// `haiku`'s request whose attempt read `read` of its 100 prompt tokens back under the reason `messages_changed`.
    fn under_reason(id: &str, received_at: i64, read: u64) -> (RequestRecord, Vec<AttemptRecord>) {
        let (request, mut attempts) = haiku(id, received_at, 100 - read);
        attempts[0].cache_read_tokens = Some(read);
        attempts[0].cache_miss_reason = Some("messages_changed".into());
        (request, attempts)
    }

    /// Both requests stand in one row under one reason, the second reading nothing back, so the rule moves the first
    /// alone.
    #[test]
    fn the_rules_pass_splits_a_retained_requests_hit_off_under_its_reason() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = NEW_YEAR + DAY;
        let without_split = FoldRules {
            hit_split: false,
            ..FoldRules::ALL
        };
        store(
            &db,
            &under_reason("hit", at, 95),
            &list(Some(1_000)),
            without_split,
            0,
        );
        store(
            &db,
            &under_reason("miss", at + 1_000, 0),
            &list(Some(1_000)),
            without_split,
            0,
        );
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 2 * DAY).unwrap();
        assert_eq!(
            row(&db, at, STANDARD_BAND).cache_miss_reasons,
            BTreeMap::from([
                ("messages_changed".to_string(), 2),
                ("messages_changed/hit".to_string(), 1)
            ])
        );
    }

    /// A request retention of 0 keeps both requests while a rollup retention of seven days drops the hour the purge's
    /// cutoff splits. Each request sends ten tokens over 40 ms.
    #[test]
    fn the_rules_pass_writes_no_row_back_into_an_hour_the_rollup_purge_removed() {
        let db = LedgerDb::open_in_memory().unwrap();
        let dropped = timed("dropped", IN_THE_SPLIT_HOUR, 100, (200, 1_000, 1_040));
        let kept = timed("kept", IN_THE_NEXT_HOUR, 100, (200, 1_000, 1_040));
        store(&db, &dropped, &list(Some(1_000)), NO_RULES, 0);
        store(&db, &kept, &list(Some(1_000)), NO_RULES, 0);
        purge(&db, 0, 7, SPLIT_NOW, &[], &list(Some(1_000)));
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 7, SPLIT_NOW).unwrap();
        assert_eq!(
            rows(&db)
                .iter()
                .map(|r| (r.hour, r.output_tps_hist.count()))
                .collect::<Vec<_>>(),
            [(crate::rollup::hour_of(IN_THE_NEXT_HOUR), 0)],
            "the hour the purge removed gets no row back, and the kept hour loses its sample"
        );
    }

    /// A band mark whose `since` lies after both held requests of 1,500 prompt tokens: the first landed before `since`
    /// under the migration's standard band, the second after it in the long band at 1,000. Each row holds a request
    /// that stays, of 500 and of 2,500 prompt tokens.
    #[test]
    fn the_rules_pass_bands_a_request_received_before_the_band_mark_by_when_it_landed() {
        let db = LedgerDb::open_in_memory().unwrap();
        let (at, since) = (NEW_YEAR + DAY, NEW_YEAR + 2 * DAY);
        let early = slowed(timed("r1", at, 1_500, (21_000, 21_500, 30_000)), 20_000);
        let late = slowed(
            timed("r2", at + 1_000, 1_500, (32_000, 32_500, 40_000)),
            20_000,
        );
        let short = timed("s", at + 2_000, 500, (6_000, 6_500, 9_000));
        let long = timed("l", at + 3_000, 2_500, (5_000, 5_500, 9_000));
        store(&db, &early, &list(None), NO_RULES, 0);
        store(&db, &short, &list(None), NO_RULES, 0);
        refold_bands(&db, &list(Some(1_000)), 0, 0, since).unwrap();
        store(&db, &late, &list(Some(1_000)), NO_RULES, since + 1_000);
        store(&db, &long, &list(Some(1_000)), NO_RULES, since + 2_000);
        mark_fold_rules(&db).unwrap();
        refold_rules(&db, 0, 0, NEW_YEAR + 3 * DAY).unwrap();
        assert_eq!(
            (
                row(&db, at, STANDARD_BAND).ttfb_hist,
                row(&db, at, LONG_BAND).ttfb_hist
            ),
            (sampled(&[1_000.0, 6_000.0]), sampled(&[32_000.0, 5_000.0])),
            "the request that landed before the mark moves in the standard band, the one that landed after stays as \
             folded"
        );
    }
}
