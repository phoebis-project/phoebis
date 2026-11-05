//! Reads for the admin routes: the request log with its filters and cursor, the rollup series and breakdown
//! with merged histograms, the dashboard's day, and the newest quota sample per window.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use phoebis_bus::{Classifier, Kind};
use phoebis_relay::EFFORT_LEVELS;
use phoebis_routing::{
    model_week, MemberSpend, PinUsage, DAY_MS, FIVE_HOUR, HOUR_MS, REFRESHING_CLASSES, SEVEN_DAY,
};
use phoebis_store::{canonical_id, AccountKind, AttachmentSubject, BudgetPeriod};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{
    event_select, fill_guardrails, request_guardrails, row_to_event, row_to_extra_usage_sample,
    row_to_quota_sample, row_to_request, with_attempts, LedgerDb, LANDING_STAMP_SQL,
    REQUEST_SELECT,
};
use crate::histogram::Histogram;
use crate::pattern::{literal_like, Patterns, ESCAPE};
use crate::plan::{
    entrypoint_of_client, free_equivalent, free_shares, funding_of, price_equivalent, price_shares,
    CostRevisions, Funding, PlanTables,
};
use crate::price::{price_row, price_usd, Priced, RowKey};
use crate::pricing::{Band, PriceRevisions, Speed};
use crate::record::{
    AttemptRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord, RequestRecord,
};
use crate::rollup::{
    add_counts, detail_json, fold_identifier, fold_user_agent, hour_of, migration_reason,
    read_account_rollups, read_rollups_paged, visit_rollups, CostSplit, KeySelector, RollupRow,
    RollupSelect, RoutingKind, DIMENSION_MAX_BYTES, EXPIRED_BETWEEN_TURNS, NO_CLASS, NO_EFFORT,
    OTHER_DIMENSION, UNATTRIBUTED,
};
use crate::LedgerError;

pub fn parse_range(range: Option<&str>, now: i64) -> Result<(i64, i64), String> {
    match range.unwrap_or("24h") {
        "1h" => Ok((now - HOUR_MS, now)),
        "24h" => Ok((now - DAY_MS, now)),
        "7d" => Ok((now - 7 * DAY_MS, now)),
        "30d" => Ok((now - 30 * DAY_MS, now)),
        custom => {
            let (from, to) = custom.split_once(',').ok_or_else(|| {
                "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds".to_string()
            })?;
            let (from, to) = (
                from.trim()
                    .parse::<i64>()
                    .map_err(|_| "from is not a number".to_string())?,
                to.trim()
                    .parse::<i64>()
                    .map_err(|_| "to is not a number".to_string())?,
            );
            if from >= to {
                return Err("from must be before to".into());
            }
            if to > now + DAY_MS {
                return Err("to is past the ledger's lifetime".into());
            }
            // Saturating: a `from` near `i64::MIN` would overflow the subtraction, and the span it names is
            // refused here either way.
            if to.saturating_sub(from) > MAX_RANGE_MS {
                return Err("a custom range spans at most 400 days".into());
            }
            Ok((from, to))
        }
    }
}

/// The widest custom range, 400 days, which outlasts the rollups' 365-day default retention: under the default a
/// wider one reads nothing more and would only size the chart.
pub const MAX_RANGE_MS: i64 = 400 * DAY_MS;

/// The most points one series or traffic answer may carry; every allowed range at its bucket size stays
/// under this, so the check is a guard against arithmetic, not a limit a caller meets.
pub const MAX_POINTS: i64 = 10_000;

/// The first hour boundary at or after `at_ms`, in seconds: the exclusive upper bound of a range in hours.
pub fn hour_ceil(at_ms: i64) -> i64 {
    let floor = hour_of(at_ms);
    if floor * 1_000 == at_ms {
        floor
    } else {
        floor + 3_600
    }
}

/// Coarser buckets for longer ranges, so a 30-day chart is a few hundred points rather than seven hundred
/// hourly ones per group.
pub fn bucket_seconds(from: i64, to: i64) -> i64 {
    let span = to - from;
    if span <= 48 * HOUR_MS {
        3_600
    } else if span <= 30 * DAY_MS {
        21_600
    } else {
        86_400
    }
}

/// A request with its attempts, in attempt order.
pub type RequestWithAttempts = (RequestRecord, Vec<AttemptRecord>);

#[derive(Clone, Debug, Default)]
pub struct RequestFilter {
    pub text: Option<String>,
    /// Keep the one request whose id equals the value, literally.
    pub request: Option<String>,
    /// Keep the requests whose session id equals the value, literally.
    pub session: Option<String>,
    /// Keep the requests whose `agent_id` equals the value, literally.
    pub agent_id: Option<String>,
    /// Keep the rows whose `landed_at` is above the value.
    pub landed_after: Option<i64>,
    pub keys: Vec<String>,
    pub pools: Vec<String>,
    /// Keep the requests whose serving attempt, the last one, ran on an account in the list: the account the
    /// rollups file a request under. `UNATTRIBUTED` keeps a request no attempt served.
    pub accounts: Vec<String>,
    pub models: Vec<String>,
    pub kinds: Vec<String>,
    pub statuses: Vec<u16>,
    pub outcomes: Vec<String>,
    /// A pattern under the wildcard grammar of `crate::pattern`; a value with no wildcard matches in full.
    pub user_agent: Patterns,
    /// A pattern under the wildcard grammar over the row's `client`; a row with none matches `-`.
    pub client: Patterns,
    /// Patterns under the wildcard grammar over the row's `client` folded as the rollup folds it, as the series read
    /// them: a row matches when any pattern matches. Read by `refusal_categories`, the request log reading the
    /// singular `client`.
    pub clients: Patterns,
    /// Patterns under the wildcard grammar over the agent's first product token, as the series read them: a row
    /// matches when any pattern matches. Read by `refusal_categories`, the request log reading the singular
    /// `user_agent` raw.
    pub user_agents: Patterns,
    pub agent_types: Vec<String>,
    pub turn_origins: Vec<String>,
    /// `fast`, `standard`: the serving attempt's `speed`, else `fast` for a request that asked for it and
    /// `standard` otherwise.
    pub speeds: Vec<String>,
    /// Keep the requests that waited through a hold (`held` on the request row).
    pub held: bool,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub include_probes: bool,
    /// Keep the requests whose serving attempt, the last one, reads a reason in the list. A request with no
    /// attempt or a null serving reason never matches. Any name, as the route passes any name the beta sent.
    pub cache_miss_reasons: Vec<String>,
    /// Keep the requests whose effort, folded as the rollups fold it (`none` for none, `other` past the six),
    /// is in the list.
    pub efforts: Vec<String>,
    /// Keep the requests whose class, folded as the rollups fold it (`-` for none, `other` outside the
    /// identifier charset or past the bound), is in the list.
    pub request_classes: Vec<String>,
    /// Keep the requests whose classifier standing, `client` or `-` for none, is in the list.
    pub classifiers: Vec<String>,
    /// Keep the requests a listed guardrail matched, `-` the requests no guardrail matched.
    pub guardrails: Vec<String>,
    /// Keep the requests whose serving attempt, the last one, answered a stop reason in the list, `-` a request whose
    /// serving attempt carries none or that no attempt served.
    pub stop_reasons: Vec<String>,
    /// Keep the requests whose serving attempt answered a refusal category in the list, `-` as for `stop_reasons`.
    pub refusal_categories: Vec<String>,
    /// The range's `to` is fixed, so `landed_through` may pass it: a row received before `to` lands after it, and a
    /// poll over a fixed range reads it once. False for a live range, whose `to` moves with the clock.
    pub fixed_end: bool,
}

pub(crate) fn in_list<T: Clone + Into<rusqlite::types::Value>>(
    column: &str,
    values: &[T],
    args: &mut Vec<rusqlite::types::Value>,
    clauses: &mut Vec<String>,
) {
    if values.is_empty() {
        return;
    }
    let marks = bind_all(values, args);
    clauses.push(format!("{column} IN ({marks})"));
}

/// A model filter as the ledger's reads match it, each model folded through `canonical_id`.
fn canonical_models(models: &[String]) -> Vec<String> {
    models.iter().map(|m| canonical_id(m)).collect()
}

/// The pool term of a rollups read. The member index leads with the pool, so a pool bound alone would have the
/// planner walk the pool's whole retention before the hour filter: the unary plus keeps that term off the index, as
/// `top_sessions_statement` keeps its accounts off theirs, and a pool with an account bound keeps the member index's
/// full prefix.
pub(crate) fn pool_term(
    pools: &[String],
    accounts: &[String],
    args: &mut Vec<rusqlite::types::Value>,
    clauses: &mut Vec<String>,
) {
    let column = if accounts.is_empty() {
        "+pool_id"
    } else {
        "pool_id"
    };
    in_list(column, pools, args, clauses);
}

pub(crate) fn bind_all<T: Clone + Into<rusqlite::types::Value>>(
    values: &[T],
    args: &mut Vec<rusqlite::types::Value>,
) -> String {
    values
        .iter()
        .map(|v| {
            args.push(v.clone().into());
            format!("?{}", args.len())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `values` as the quoted literals of a SQL list, for an `IN` over a set the code names.
fn sql_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("'{v}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `column` folded as `fold_effort` folds a request's effort, in SQL.
pub(crate) fn effort_fold_sql(column: &str) -> String {
    let levels: Vec<&str> = EFFORT_LEVELS.into_iter().chain([NO_EFFORT]).collect();
    let levels = sql_list(&levels);
    format!("CASE WHEN {column} IS NULL THEN '{NO_EFFORT}' WHEN {column} IN ({levels}) THEN {column} ELSE '{OTHER_DIMENSION}' END")
}

/// `column` folded as `fold_identifier` folds a request's class, in SQL, `NO_CLASS` for NULL.
pub(crate) fn class_fold_sql(column: &str) -> String {
    // The charset is `identifier_byte`'s. The GLOB keeps the value to ASCII, so SQLite's character length is its
    // byte length and the bound reads as the rollups' `DIMENSION_MAX_BYTES`.
    format!(
        "CASE WHEN {column} IS NULL THEN '{NO_CLASS}' WHEN length({column}) BETWEEN 1 AND {DIMENSION_MAX_BYTES} \
         AND {column} NOT GLOB '*[^A-Za-z0-9./:_-]*' THEN {column} ELSE '{OTHER_DIMENSION}' END"
    )
}

/// `key_id` and `pool_id` are NULL where the rollups say `-`, so a drill-down from the unattributed analytics
/// row must reach the NULL the reserved id stands for.
fn in_list_or_null(
    column: &str,
    values: &[String],
    args: &mut Vec<rusqlite::types::Value>,
    clauses: &mut Vec<String>,
) {
    if !values.iter().any(|v| v == UNATTRIBUTED) {
        return in_list(column, values, args, clauses);
    }
    let marks = bind_all(values, args);
    clauses.push(format!("({column} IN ({marks}) OR {column} IS NULL)"));
}

/// The request log reads a `keys` selector as the analytics do: a bare id is the live generation,
/// `<id>@<generation>` that one, and the reserved id reaches the NULL it stands for. A bare id no live key holds
/// selects nothing, as `LiveKeys::selects` answers it, so a filter made of such ids answers no row.
fn key_lifetimes(
    selectors: &[String],
    live: &LiveKeys,
    args: &mut Vec<rusqlite::types::Value>,
    clauses: &mut Vec<String>,
) {
    if selectors.is_empty() {
        return;
    }
    let keys = key_selectors(selectors, live);
    if keys.is_empty() {
        clauses.push("0".into());
    } else {
        key_clause(&keys, true, args, clauses);
    }
}

/// Each selector as `LiveKeys::selects` reads it: the reserved id at any generation, every other selector at the
/// lifetime `lifetime_of` answers. A selector that names no lifetime drops out.
fn key_selectors(selectors: &[String], live: &LiveKeys) -> Vec<KeySelector> {
    selectors
        .iter()
        .filter_map(|selector| {
            if selector == UNATTRIBUTED {
                return Some(KeySelector {
                    key_id: UNATTRIBUTED.into(),
                    key_generation: None,
                });
            }
            live.lifetime_of(selector)
                .map(|(id, generation)| KeySelector {
                    key_id: id.into(),
                    key_generation: Some(generation),
                })
        })
        .collect()
}

/// One clause matching any of `keys`, a selector with a generation on the pair and one without on the id alone. With
/// `reserved_is_null` a selector without a generation also reaches the NULL `key_id` the requests table stores for
/// the reserved id, where the rollups store the id itself. No clause when `keys` is empty.
pub(crate) fn key_clause(
    keys: &[KeySelector],
    reserved_is_null: bool,
    args: &mut Vec<rusqlite::types::Value>,
    clauses: &mut Vec<String>,
) {
    if keys.is_empty() {
        return;
    }
    let parts: Vec<String> = keys
        .iter()
        .map(|k| {
            args.push(k.key_id.clone().into());
            let at_id = args.len();
            match k.key_generation {
                Some(generation) => {
                    args.push(generation.into());
                    format!("(key_id = ?{at_id} AND key_generation = ?{})", at_id + 1)
                }
                None if reserved_is_null => format!("key_id = ?{at_id} OR key_id IS NULL"),
                None => format!("key_id = ?{at_id}"),
            }
        })
        .collect();
    clauses.push(format!("({})", parts.join(" OR ")));
}

/// One clause matching any of `patterns` against `column` under the wildcard grammar of `crate::pattern`;
/// `column` reads as `-` where it is NULL, as the rollup fold already does. No clause when `patterns` is empty.
pub(crate) fn pattern_any(
    column: &str,
    patterns: &[String],
    args: &mut Vec<rusqlite::types::Value>,
    clauses: &mut Vec<String>,
) {
    if patterns.is_empty() {
        return;
    }
    let parts: Vec<String> = patterns
        .iter()
        .map(|p| {
            args.push(crate::pattern::to_like(p).into());
            format!(
                "COALESCE({column}, '-') LIKE ?{} ESCAPE '{ESCAPE}'",
                args.len()
            )
        })
        .collect();
    clauses.push(format!("({})", parts.join(" OR ")));
}

/// `expr` read off the serving attempt of the request whose id `request_id` names: its last attempt, the one the
/// rollups file the request under.
fn serving_attempt(expr: &str, request_id: &str) -> String {
    format!("(SELECT {expr} FROM attempts a WHERE a.request_id = {request_id} ORDER BY a.attempt DESC LIMIT 1)")
}

/// The `WHERE` clauses `filter` sets on the requests table with the values they bind, numbered in the order bound.
/// The key filter reads the selectors the analytics do, against the row's `(key_id, key_generation)`, and `live` says
/// which generation a bare id names.
fn filter_clauses(
    filter: &RequestFilter,
    live: &LiveKeys,
) -> (Vec<rusqlite::types::Value>, Vec<String>) {
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if let Some(text) = filter.text.as_deref() {
        args.push(format!("%{}%", literal_like(text)).into());
        let n = args.len();
        clauses.push(format!("(id LIKE ?{n} ESCAPE '{ESCAPE}' OR session_id LIKE ?{n} ESCAPE '{ESCAPE}' OR key_id LIKE ?{n} ESCAPE '{ESCAPE}' OR model LIKE ?{n} ESCAPE '{ESCAPE}' OR error_code LIKE ?{n} ESCAPE '{ESCAPE}' OR EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id AND (a.account_id LIKE ?{n} ESCAPE '{ESCAPE}' OR a.upstream_request_id LIKE ?{n} ESCAPE '{ESCAPE}' OR a.upstream_message_id LIKE ?{n} ESCAPE '{ESCAPE}')))"));
    }
    if let Some(request) = filter.request.as_deref() {
        args.push(request.to_string().into());
        clauses.push(format!("id = ?{}", args.len()));
    }
    if let Some(session) = filter.session.as_deref() {
        args.push(session.to_string().into());
        clauses.push(format!("session_id = ?{}", args.len()));
    }
    if let Some(agent_id) = filter.agent_id.as_deref() {
        args.push(agent_id.to_string().into());
        clauses.push(format!("agent_id = ?{}", args.len()));
    }
    if let Some(landed_after) = filter.landed_after {
        args.push(landed_after.into());
        clauses.push(format!("landed_at > ?{}", args.len()));
    }
    key_lifetimes(&filter.keys, live, &mut args, &mut clauses);
    in_list_or_null("pool_id", &filter.pools, &mut args, &mut clauses);
    in_list(
        "canonical_model",
        &canonical_models(&filter.models),
        &mut args,
        &mut clauses,
    );
    in_list("kind", &filter.kinds, &mut args, &mut clauses);
    in_list("outcome", &filter.outcomes, &mut args, &mut clauses);
    in_list("status", &filter.statuses, &mut args, &mut clauses);
    // The serving attempt is the last, the account the rollups file a request under, so a request that failed over
    // lists under the account that answered it. A request no attempt served reads as the reserved id.
    in_list(
        &format!(
            "COALESCE({}, '{UNATTRIBUTED}')",
            serving_attempt("a.account_id", "requests.id")
        ),
        &filter.accounts,
        &mut args,
        &mut clauses,
    );
    // The serving attempt is the last: an earlier 429 carries no speed and must not read as standard. The speed
    // follows the rollups' fold: the speed the serving attempt's answer reported, `standard` for an answer with
    // usage that reported none, and the speed the request asked for where no attempt answered.
    let speed = serving_attempt(
        "COALESCE(a.speed, CASE WHEN COALESCE(a.input_tokens, a.output_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens) IS NOT NULL THEN 'standard' END)",
        "requests.id",
    );
    in_list(
        &format!(
            "COALESCE({speed}, CASE WHEN requests.requested_speed = 'fast' THEN 'fast' ELSE 'standard' END)"
        ),
        &filter.speeds,
        &mut args,
        &mut clauses,
    );
    // The serving attempt is the last, the one the rollups count a request's reason on. A request with no attempt or a
    // null serving reason matches nothing.
    in_list(
        &serving_attempt("a.cache_miss_reason", "requests.id"),
        &filter.cache_miss_reasons,
        &mut args,
        &mut clauses,
    );
    // The serving attempt is the last, as for the accounts. A NULL matches no list, so a serving attempt that names no
    // word, or a request no attempt served, reads as `-`.
    in_list(
        &format!(
            "COALESCE({}, '-')",
            serving_attempt("a.stop_reason", "requests.id")
        ),
        &filter.stop_reasons,
        &mut args,
        &mut clauses,
    );
    in_list(
        &format!(
            "COALESCE({}, '-')",
            serving_attempt("a.refusal_category", "requests.id")
        ),
        &filter.refusal_categories,
        &mut args,
        &mut clauses,
    );
    in_list(
        &effort_fold_sql("requests.effort"),
        &filter.efforts,
        &mut args,
        &mut clauses,
    );
    in_list(
        &class_fold_sql("requests.request_class"),
        &filter.request_classes,
        &mut args,
        &mut clauses,
    );
    in_list(
        &class_fold_sql("requests.classifier"),
        &filter.classifiers,
        &mut args,
        &mut clauses,
    );
    if filter.held {
        clauses.push("requests.held IS NOT NULL".into());
    }
    // The rollups and the console's fold already read an absent agent or client as `-`; `pattern_any`'s
    // coalesce keeps a nullable column from excluding such a row from both the wildcard and the exact marker.
    pattern_any("user_agent", &filter.user_agent, &mut args, &mut clauses);
    pattern_any("client", &filter.client, &mut args, &mut clauses);
    in_list("agent_type", &filter.agent_types, &mut args, &mut clauses);
    in_list("turn_origin", &filter.turn_origins, &mut args, &mut clauses);
    if !filter.guardrails.is_empty() {
        let ids: Vec<String> = filter
            .guardrails
            .iter()
            .filter(|g| *g != "-")
            .cloned()
            .collect();
        let mut either = Vec::new();
        // The child table's primary key leads with the request, so each lookup is a seek per row while the outer
        // walk keeps its own index.
        if !ids.is_empty() {
            let marks = bind_all(&ids, &mut args);
            either.push(format!(
                "EXISTS (SELECT 1 FROM request_guardrails g WHERE g.request_id = requests.id AND g.guardrail_id IN ({marks}))"
            ));
        }
        if filter.guardrails.iter().any(|g| g == "-") {
            either.push(
                "NOT EXISTS (SELECT 1 FROM request_guardrails g WHERE g.request_id = requests.id)"
                    .to_string(),
            );
        }
        clauses.push(format!("({})", either.join(" OR ")));
    }
    if !filter.include_probes && !filter.kinds.iter().any(|k| k == Kind::Probe.as_str()) {
        clauses.push("kind != 'probe'".into());
    }
    if let Some(from) = filter.from {
        args.push(from.into());
        clauses.push(format!("received_at >= ?{}", args.len()));
    }
    if let Some(to) = filter.to {
        args.push(to.into());
        clauses.push(format!("received_at <= ?{}", args.len()));
    }
    (args, clauses)
}

/// The request list's statement and its binds, spelled once so the seeded plan test explains the statement the
/// read runs under each filter.
pub(crate) fn list_requests_statement(
    filter: &RequestFilter,
    live: &LiveKeys,
    cursor: Option<&RequestCursor>,
    limit: usize,
) -> (String, Vec<rusqlite::types::Value>) {
    let (mut args, mut clauses) = filter_clauses(filter, live);
    if let Some(cursor) = cursor {
        args.push(cursor.received_at.into());
        let n = args.len();
        args.push(cursor.id.clone().into());
        clauses.push(format!(
            "(received_at < ?{n} OR (received_at = ?{n} AND id < ?{}))",
            n + 1
        ));
    }
    let r#where = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    // One row past the page says whether a next page exists, so a page of exactly `limit` rows offers no
    // cursor that would only lead to an empty page.
    args.push(((limit + 1) as i64).into());
    let sql = format!(
        "{REQUEST_SELECT}{where} ORDER BY received_at DESC, id DESC LIMIT ?{}",
        args.len()
    );
    (sql, args)
}

/// The request log's page under `filter`, newest first, from `cursor` on.
pub fn list_requests(
    conn: &Connection,
    filter: &RequestFilter,
    live: &LiveKeys,
    cursor: Option<&RequestCursor>,
    limit: usize,
) -> Result<(Vec<RequestWithAttempts>, Option<String>), LedgerError> {
    let (sql, args) = list_requests_statement(filter, live, cursor, limit);
    let mut stmt = conn.prepare(&sql)?;
    let mut requests = stmt
        .query_map(params_from_iter(args), row_to_request)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next = next_page(&mut requests, limit, |r| {
        RequestCursor::of(&r.id, r.received_at).encode()
    });
    let mut out = with_attempts(conn, requests)?;
    fill_guardrails(conn, &mut out)?;
    Ok((out, next))
}

/// Truncates `rows`, read one row past `limit`, to the page, and answers the cursor of the page's last row while a row
/// lay beyond it, so a page of exactly `limit` rows offers no cursor that would only lead to an empty page.
fn next_page<T, C>(rows: &mut Vec<T>, limit: usize, cursor_of: impl FnOnce(&T) -> C) -> Option<C> {
    let more = rows.len() > limit;
    rows.truncate(limit);
    rows.last().filter(|_| more).map(cursor_of)
}

/// One page of `list_request_page`.
#[derive(Clone, Debug)]
pub struct RequestPage {
    pub rows: Vec<RequestWithAttempts>,
    /// The cursor past the page's last row, set only while a row lies beyond it.
    pub next: Option<String>,
    /// The newest `landed_at` among the rows the page's filters admit at this read, `landed_after` itself where none
    /// above it has landed and 0 where neither is set, capped at `to` where the read names one and `fixed_end` is
    /// false, read in the page's transaction.
    pub landed_through: i64,
}

/// `list_requests` with the newest write instant among the rows its filters admit. Both reads share one transaction,
/// so every row written after this read carries a stamp above `landed_through`, and a poll that passes it back as
/// `landed_after` misses no row.
pub fn list_request_page(
    conn: &Connection,
    filter: &RequestFilter,
    live: &LiveKeys,
    cursor: Option<&RequestCursor>,
    limit: usize,
) -> Result<RequestPage, LedgerError> {
    // A read hands a shared connection, which only the unchecked form opens a transaction on.
    let tx = conn.unchecked_transaction()?;
    let (rows, next) = list_requests(&tx, filter, live, cursor, limit)?;
    // For a live range, whose `to` moves with the clock, a row the filters leave out, such as one received after `to`
    // and written before this read, stays above the watermark, so the next poll's later bound still finds it. The
    // writer stamps a row from its clock, so a row received after `to` lands after it: the cap at `to` keeps a later
    // admitted row's stamp from passing it. Under `fixed_end` the cap is off: the range's `to` is fixed, so a row
    // received before it and landed after it is read once by the poll that passes the stamp back.
    let (mut args, clauses) = filter_clauses(filter, live);
    args.push(filter.landed_after.unwrap_or(0).into());
    let r#where = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let landed_through: i64 = tx.query_row(
        &format!(
            "SELECT COALESCE(MAX(landed_at), ?{}) FROM requests{where}",
            args.len()
        ),
        params_from_iter(args),
        |r| r.get(0),
    )?;
    let landed_through = if filter.fixed_end {
        landed_through
    } else {
        filter
            .to
            .map_or(landed_through, |to| landed_through.min(to))
    };
    Ok(RequestPage {
        rows,
        next,
        landed_through,
    })
}

/// The requests in `[from, to]` under `filter` whose serving attempt's `stop_reason` is `refusal`, counted per
/// category (`-` for a refusal with none), the largest count first and then the category. Read off the attempts at the
/// request, since no rollup carries the category. The `clients` and `user_agents` patterns match each row's client and
/// agent folded as the rollups fold them, so the count narrows as the series do.
pub fn refusal_categories(
    conn: &Connection,
    filter: &RequestFilter,
    live: &LiveKeys,
    from: i64,
    to: i64,
) -> Result<Vec<(String, u64)>, LedgerError> {
    let filter = RequestFilter {
        from: Some(from),
        to: Some(to),
        ..filter.clone()
    };
    let (args, mut clauses) = filter_clauses(&filter, live);
    clauses.push(format!(
        "{} = 'refusal'",
        serving_attempt("a.stop_reason", "requests.id")
    ));
    // SQL does not express the fold, so the patterns match each raw pair in memory, grouped first so a pair repeated
    // across many refusals folds once per category.
    let sql = format!(
        "SELECT COALESCE({}, '-') AS category, \
         client, user_agent, COUNT(*) FROM requests WHERE {} GROUP BY category, client, user_agent",
        serving_attempt("a.refusal_category", "requests.id"),
        clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)? as u64,
        ))
    })?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        let (category, client, user_agent, requests) = row?;
        if passes_request_patterns(&filter, client.as_deref(), user_agent.as_deref()) {
            *counts.entry(category).or_default() += requests;
        }
    }
    let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
    // The map hands the categories in byte order, as SQLite compares text, and the stable sort keeps that order among
    // equal counts.
    counts.sort_by_key(|(_, requests)| std::cmp::Reverse(*requests));
    Ok(counts)
}

/// Whether a request row passes the filter's `clients` and `user_agents`, each value folded as `deltas_of` keys the
/// hour row (`-` for none, the agent to its first product token), so a pattern reads as `passes_patterns` reads it.
fn passes_request_patterns(
    filter: &RequestFilter,
    client: Option<&str>,
    user_agent: Option<&str>,
) -> bool {
    let passes = |patterns: &[String], raw: Option<&str>, fold: fn(&str) -> String| {
        if patterns.is_empty() {
            return true;
        }
        let value = raw.map_or_else(|| UNATTRIBUTED.to_string(), fold);
        patterns.iter().any(|p| crate::pattern::matches(p, &value))
    };
    passes(&filter.user_agents, user_agent, fold_user_agent)
        && passes(&filter.clients, client, fold_identifier)
}

/// The same three queries `LedgerDb::request` runs, over a connection the caller already holds.
pub fn request_by_id(
    conn: &Connection,
    id: &str,
) -> Result<Option<RequestWithAttempts>, LedgerError> {
    static STATEMENT: LazyLock<String> =
        LazyLock::new(|| format!("{REQUEST_SELECT} WHERE id = ?1"));
    let request = conn
        .prepare_cached(&STATEMENT)?
        .query_row(params![id], row_to_request)
        .optional()?;
    let Some(request) = request else {
        return Ok(None);
    };
    let (mut request, attempts) = with_attempts(conn, vec![request])?
        .pop()
        .expect("with_attempts answers one row per request");
    request.guardrails = request_guardrails(conn, id)?;
    Ok(Some((request, attempts)))
}

/// One guardrail's matches over a range: the ones that let the request through, the ones that blocked it, and the
/// blocked requests' bytes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GuardrailCountRow {
    pub guardrail_id: String,
    pub observed: u64,
    pub blocked: u64,
    pub blocked_bytes: u64,
}

/// The two statements `guardrail_counts` prepares, exported so a test can read their query plans against the index.
pub const GUARDRAIL_COUNTS_SQL: &str = "SELECT guardrail_id, SUM(blocked = 0), SUM(blocked = 1) FROM request_guardrails WHERE received_at >= ?1 AND received_at <= ?2 GROUP BY guardrail_id ORDER BY guardrail_id";
pub const GUARDRAIL_BLOCKED_BYTES_SQL: &str = "SELECT COALESCE(SUM(r.request_bytes), 0) FROM request_guardrails g JOIN requests r ON r.id = g.request_id WHERE g.guardrail_id = ?1 AND g.blocked = 1 AND g.received_at >= ?2 AND g.received_at <= ?3";

/// One row per guardrail that matched a request in `[from, to]`, ordered by id, through the child table's index led
/// by `received_at`, which the counts read alone and the bytes read walks over the same range.
pub fn guardrail_counts(
    conn: &Connection,
    from: i64,
    to: i64,
) -> Result<Vec<GuardrailCountRow>, LedgerError> {
    // The index led by `received_at` carries the guardrail and the verdict too, so the planner reads the index alone
    // over the range and sorts its rows for the group, and seeks the same range for the bytes, whose join reaches
    // `requests` by its primary key.
    let mut rows = conn
        .prepare(GUARDRAIL_COUNTS_SQL)?
        .query_map(params![from, to], |r| {
            Ok(GuardrailCountRow {
                guardrail_id: r.get(0)?,
                observed: r.get::<_, i64>(1)? as u64,
                blocked: r.get::<_, i64>(2)? as u64,
                blocked_bytes: 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut bytes = conn.prepare(GUARDRAIL_BLOCKED_BYTES_SQL)?;
    for row in rows.iter_mut().filter(|row| row.blocked > 0) {
        row.blocked_bytes =
            bytes.query_row(params![row.guardrail_id, from, to], |r| r.get::<_, i64>(0))? as u64;
    }
    Ok(rows)
}

/// The columns the dry run reads off a request row: the facts subjects and the five hints the row keeps.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleRow {
    pub id: String,
    pub received_at: i64,
    pub kind: String,
    pub canonical_model: String,
    pub client: Option<String>,
    pub request_class: Option<String>,
    pub agent_type: Option<String>,
    pub user_agent: Option<String>,
    pub compaction: Option<String>,
    pub context_compacted: Option<String>,
}

/// The pool's sample page before its cursor clause and its order, walked on `requests_received`, kept as a constant
/// so this module's plan tests pin each statement's plan.
pub const GUARDRAIL_SAMPLE_POOL_SQL: &str = "SELECT r.id, r.received_at, r.kind, r.canonical_model, r.client, r.request_class, r.agent_type, r.user_agent, r.compaction, r.context_compacted FROM requests r WHERE r.pool_id = ?1 AND r.bodies_captured = 1 AND r.kind != 'probe' AND r.received_at >= ?2";
/// The key's, seeking `requests_key`.
pub const GUARDRAIL_SAMPLE_KEY_SQL: &str = "SELECT r.id, r.received_at, r.kind, r.canonical_model, r.client, r.request_class, r.agent_type, r.user_agent, r.compaction, r.context_compacted FROM requests r WHERE r.key_id = ?1 AND r.bodies_captured = 1 AND r.kind != 'probe' AND r.received_at >= ?2";
/// The clause a page past the first appends, its two binds after the subject's and the window's.
pub const GUARDRAIL_SAMPLE_CURSOR_SQL: &str =
    " AND (r.received_at < ?3 OR (r.received_at = ?3 AND r.id < ?4))";
/// The order and the limit every page appends last, the limit's bind index written by the caller.
pub const GUARDRAIL_SAMPLE_ORDER_SQL: &str = " ORDER BY r.received_at DESC, r.id DESC LIMIT ?";

/// The newest captured, non-probe rows of the subject since `since`, newest first, at most `limit`, with the next
/// cursor in `list_requests`'s spelling where more exist. Walks `requests_received` for a pool and `requests_key`
/// for a key.
pub fn guardrail_sample_page(
    conn: &Connection,
    subject: &AttachmentSubject,
    since: i64,
    cursor: Option<&RequestCursor>,
    limit: usize,
) -> Result<(Vec<SampleRow>, Option<RequestCursor>), LedgerError> {
    // The pool's walk takes `requests_received` and the key's `requests_key`, both bounded below by the capture
    // window, so a pool or a key whose capture is off scans no months of rows before it answers an empty page.
    let base = match subject {
        AttachmentSubject::Key(_) => GUARDRAIL_SAMPLE_KEY_SQL,
        AttachmentSubject::Pool(_) => GUARDRAIL_SAMPLE_POOL_SQL,
    };
    let mut args: Vec<rusqlite::types::Value> = vec![subject.id().to_string().into(), since.into()];
    let mut after = "";
    if let Some(cursor) = cursor {
        args.push(cursor.received_at.into());
        args.push(cursor.id.clone().into());
        after = GUARDRAIL_SAMPLE_CURSOR_SQL;
    }
    // One row past the page says whether a next page exists, as `list_requests` reads it.
    args.push(((limit + 1) as i64).into());
    let sql = format!("{base}{after}{GUARDRAIL_SAMPLE_ORDER_SQL}{}", args.len());
    let mut rows = conn
        .prepare(&sql)?
        .query_map(params_from_iter(args), |r| {
            Ok(SampleRow {
                id: r.get(0)?,
                received_at: r.get(1)?,
                kind: r.get(2)?,
                canonical_model: r.get(3)?,
                client: r.get(4)?,
                request_class: r.get(5)?,
                agent_type: r.get(6)?,
                user_agent: r.get(7)?,
                compaction: r.get(8)?,
                context_compacted: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next = next_page(&mut rows, limit, |r| {
        RequestCursor::of(&r.id, r.received_at)
    });
    Ok((rows, next))
}

/// The captured request body by primary key, `None` where the body row has aged out or its expiry is at or before
/// `now`, since the purge deletes an expired row only on its next pass.
pub fn request_body(conn: &Connection, id: &str, now: i64) -> Result<Option<Vec<u8>>, LedgerError> {
    Ok(conn
        .query_row(
            "SELECT request_body FROM bodies WHERE request_id = ?1 AND expires_at > ?2",
            params![id, now],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether `request_body` would answer a body, without copying it out: the same row under the same expiry.
pub fn request_body_held(conn: &Connection, id: &str, now: i64) -> Result<bool, LedgerError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM bodies WHERE request_id = ?1 AND expires_at > ?2",
            params![id, now],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The events of the request's window under its own session, the one its pin is keyed by, and every event that
/// names the request itself, by its `request_id` column. A request still running is read up to `now`, the caller's
/// clock: no event is stamped in the future, so the bound loses nothing and the read stops at the present rather than
/// at the table's end.
pub fn request_events(
    conn: &Connection,
    request: &RequestRecord,
    now: i64,
) -> Result<Vec<EventRecord>, LedgerError> {
    let to = request.finished_at.unwrap_or(now);
    let mut stmt = conn.prepare(concat!(
        event_select!(),
        " WHERE at >= ?1 AND at <= ?2 AND (session_id = ?3 OR request_id = ?4) ORDER BY at, rowid"
    ))?;
    let rows = stmt.query_map(
        params![request.received_at, to, request.session_id, request.id],
        row_to_event,
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) const RECENT_EVENTS_OF_KIND: &str = concat!(
    event_select!(),
    " WHERE kind = ?1 ORDER BY at DESC, rowid DESC LIMIT ?2"
);

/// The newest `limit` events of one kind, newest first.
pub fn recent_events_of_kind(
    conn: &Connection,
    kind: &str,
    limit: usize,
) -> Result<Vec<EventRecord>, LedgerError> {
    let mut stmt = conn.prepare(RECENT_EVENTS_OF_KIND)?;
    let rows = stmt.query_map(params![kind, limit as i64], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const RECENT_GAPS: &str = concat!(
    event_select!(),
    " WHERE kind = ?3 AND at >= ?1 ORDER BY at DESC LIMIT ?2"
);

/// The newest `limit` `ledger_gap` events at or after `since`, newest first: what the system page lists beside the
/// writer's gap counter.
pub fn recent_gaps(
    conn: &Connection,
    since: i64,
    limit: usize,
) -> Result<Vec<EventRecord>, LedgerError> {
    let mut stmt = conn.prepare(RECENT_GAPS)?;
    let rows = stmt.query_map(params![since, limit as i64, LEDGER_GAP], row_to_event)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// One invocation declares each kind's constant and the table, so a writer, a fold or a statement that names a kind
// names one the admin's `kinds` filter accepts.
macro_rules! event_kinds {
    ($($name:ident => $word:expr),* $(,)?) => {
        $(pub const $name: &str = $word;)*

        /// Every event kind the ledger writes, alphabetical: the `kinds` key is validated against it and the decision
        /// kinds, and a 400 names a kind outside both.
        pub const EVENT_KINDS: &[&str] = &[$($name),*];
    };
}

event_kinds! {
    ACCOUNT_STATE_CHANGED => "account_state_changed",
    BUDGET_PERIOD_ENDED => "budget_period_ended",
    BUDGET_REACHED => "budget_reached",
    CATALOG_CHANGED => "catalog_changed",
    COOLDOWN_CLEARED => "cooldown_cleared",
    COOLDOWN_SET => "cooldown_set",
    FAST_CREDITS_CHANGED => "fast_credits_changed",
    LEDGER_GAP => "ledger_gap",
    MARK_CLEARED => "mark_cleared",
    MODEL_PERMITTED => "model_permitted",
    MODEL_REJECTED => "model_rejected",
    OVERAGE_ENDED => "overage_ended",
    OVERAGE_STARTED => "overage_started",
    PIN_CREATED => phoebis_routing::PIN_CREATED,
    PIN_EXPIRED => phoebis_routing::PIN_EXPIRED,
    PIN_MIGRATED => phoebis_routing::PIN_MIGRATED,
    PIN_RELEASED => "pin_released",
    PLAN_ENDED => "plan_ended",
    PLAN_RESTORED => "plan_restored",
    POLL_BACKOFF_ENDED => "poll_backoff_ended",
    POLL_BACKOFF_STARTED => "poll_backoff_started",
    POLL_FAILED => "poll_failed",
    REFRESH_FAILED => "refresh_failed",
    REFRESH_SUCCEEDED => "refresh_succeeded",
    REFRESH_UNCERTAIN => "refresh_uncertain",
    REGISTRY_CHANGED => "registry_changed",
    RESERVATION_EXPIRED => "reservation_expired",
    ROUTER_STARTED => "router_started",
    SHEDDING_ROUND => "shedding_round",
    SLOW_DOWN_ENDED => "slow_down_ended",
    SLOW_DOWN_STARTED => "slow_down_started",
    STREAM_INTERRUPTED => "stream_interrupted",
    WARM_PREFIX_EXPIRED => "warm_prefix_expired",
    WIND_DOWN_STARTED => "wind_down_started",
    WINDOW_CLEARED => "window_cleared",
    WINDOW_RESET => "window_reset",
    WINDOW_SPENT => "window_spent",
}

pub const DECISION_KINDS: &[&str] = &["exhausted", "held", "moved", "placed", "refused", "renewed"];

/// The pseudo-kind that names a probe's decision of any kind in `kinds`.
pub const PROBE_KIND: &str = "probe";

/// The decision kinds that sent the request upstream, and so have an attempt that answered for them: the kinds
/// `completing_attempt` reads.
const DISPATCHING_KINDS: &[&str] = &["moved", "placed", "renewed"];

#[derive(Clone, Debug, Default)]
pub struct RoutingLogFilter {
    pub from: i64,
    pub to: i64,
    pub request: Option<String>,
    /// A session id or its prefix.
    pub session: Option<String>,
    /// Keep the decisions and the events whose `agent_id` equals the value, literally, so a session-level event,
    /// which names no agent, is left out.
    pub agent_id: Option<String>,
    pub keys: Vec<String>,
    pub pools: Vec<String>,
    pub accounts: Vec<String>,
    pub models: Vec<String>,
    /// `None` is every decision and event kind, the probe decisions among them. `Some` is exactly the kinds named,
    /// `probe` the pseudo-kind for a probe decision of any kind.
    pub kinds: Option<LogKinds>,
}

/// The kinds a routing log filter names, sorted once into the statement that reads each.
#[derive(Clone, Debug, Default)]
pub struct LogKinds {
    pub decisions: Vec<String>,
    pub events: Vec<String>,
    /// `PROBE_KIND` was named: a probe decision of any kind.
    pub probes: bool,
}

impl LogKinds {
    /// The names sorted by the statement that reads them, the error the first name no kind and no `PROBE_KIND`
    /// answers to.
    pub fn parse(names: &[String]) -> Result<LogKinds, String> {
        let mut kinds = LogKinds::default();
        for name in names {
            let kind = name.as_str();
            let decision = DECISION_KINDS.contains(&kind);
            let event = EVENT_KINDS.contains(&kind);
            if kind != PROBE_KIND && !decision && !event {
                return Err(name.clone());
            }
            kinds.probes |= kind == PROBE_KIND;
            if decision {
                kinds.decisions.push(name.clone());
            }
            if event {
                kinds.events.push(name.clone());
            }
        }
        Ok(kinds)
    }
}

/// `<received_at>:<id>`, the request list's and the guardrail sample's cursor: the page after it starts below that
/// row in the `received_at DESC, id DESC` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestCursor {
    pub id: String,
    pub received_at: i64,
}

impl RequestCursor {
    /// The id may hold a `:` of its own, so the instant is read off the first.
    pub fn parse(s: &str) -> Option<RequestCursor> {
        let (received_at, id) = s.split_once(':')?;
        Some(RequestCursor {
            id: id.to_string(),
            received_at: received_at.parse().ok()?,
        })
    }

    pub fn encode(&self) -> String {
        format!("{}:{}", self.received_at, self.id)
    }

    fn of(id: &str, received_at: i64) -> RequestCursor {
        RequestCursor {
            id: id.to_string(),
            received_at,
        }
    }
}

/// `<at>:<rank>:<id>`: rank 1 for a decision (`id` is `<request_id>/<seq>`), 0 for an event (`id` its rowid).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingLogCursor {
    Decision {
        at: i64,
        request_id: String,
        seq: u32,
    },
    Event {
        at: i64,
        rowid: i64,
    },
}

impl RoutingLogCursor {
    /// The request id may hold a `/` or a `:` of its own, so the seq is read off the last `/`.
    pub fn parse(s: &str) -> Option<RoutingLogCursor> {
        let (at, rest) = s.split_once(':')?;
        let (rank, id) = rest.split_once(':')?;
        let at = at.parse::<i64>().ok()?;
        match rank {
            "1" => {
                let (request_id, seq) = id.rsplit_once('/')?;
                if request_id.is_empty() {
                    return None;
                }
                Some(RoutingLogCursor::Decision {
                    at,
                    request_id: request_id.to_string(),
                    seq: seq.parse().ok()?,
                })
            }
            "0" => Some(RoutingLogCursor::Event {
                at,
                rowid: id.parse().ok()?,
            }),
            _ => None,
        }
    }

    pub fn encode(&self) -> String {
        match self {
            RoutingLogCursor::Decision {
                at,
                request_id,
                seq,
            } => format!("{at}:1:{request_id}/{seq}"),
            RoutingLogCursor::Event { at, rowid } => format!("{at}:0:{rowid}"),
        }
    }

    fn of(row: &RoutingLogRow) -> RoutingLogCursor {
        match row {
            RoutingLogRow::Decision(d) => RoutingLogCursor::Decision {
                at: d.at,
                request_id: d.request_id.clone(),
                seq: d.seq,
            },
            RoutingLogRow::Event(e) => RoutingLogCursor::Event {
                at: e.at,
                rowid: e.id,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RoutingLogRow {
    Decision(DecisionListRow),
    Event(EventListRow),
}

/// The summary column parsed, with the joined facts of the completing attempt (the newest attempt row on the
/// decision's account at or after the decision's `attempt` and before the request's next decision's, by `seq`).
/// `attempt` stays the decision's own, `cache` is `None` until the completing attempt has usage, and a `held`,
/// `exhausted` or `refused` decision joins no attempt (`status` and `cache` null).
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionListRow {
    pub request_id: String,
    pub seq: u32,
    pub attempt: u32,
    pub attempt_count: Option<u32>,
    pub at: i64,
    pub kind: String,
    pub probe: bool,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub account_id: Option<String>,
    pub from_account_id: Option<String>,
    pub summary: Value,
    pub cache: Option<CacheFacts>,
    pub status: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CacheFacts {
    pub hit: bool,
    pub read_tokens: u64,
    pub promised_tokens: Option<u64>,
    pub input_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventListRow {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub request_id: Option<String>,
    pub actor: Option<String>,
    pub detail: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutingLogPage {
    pub rows: Vec<RoutingLogRow>,
    pub next_cursor: Option<String>,
    /// `(decisions, events)` under the whole filter, answered only under a `request` filter, the one bounded set.
    pub counts: Option<(u64, u64)>,
}

/// The attempt a dispatching decision completed on: the newest attempt row on the decision's own account at or after
/// its attempt and below the attempt of the request's next decision, so a retry in place, a refresh replay or a strip
/// retry reads the pass that answered. A successor on the same attempt (a credential failure fails over before the
/// wire counter advances) leaves the decision no attempt of its own, and the account match keeps a successor whose
/// decision never reached the ledger from lending its answer.
fn completing_attempt() -> String {
    format!(
        "CASE WHEN d.kind IN ({}) THEN (SELECT MAX(x.attempt) FROM attempts x WHERE x.request_id = d.request_id AND x.account_id = d.account_id AND x.attempt >= d.attempt AND x.attempt < COALESCE((SELECT MIN(n.attempt) FROM decisions n WHERE n.request_id = d.request_id AND n.seq > d.seq), 9223372036854775807)) END",
        sql_list(DISPATCHING_KINDS)
    )
}

/// The completing attempt's columns the log reads, every field `None` where none joined.
struct Answered {
    status: Option<u16>,
    input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_5m_tokens: Option<u64>,
    cache_write_1h_tokens: Option<u64>,
}

impl Answered {
    /// The five columns from `at` on, in the order both statements select them.
    fn read(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Answered> {
        let tokens = |i: usize| -> rusqlite::Result<Option<u64>> {
            Ok(r.get::<_, Option<i64>>(i)?.map(|n| n as u64))
        };
        Ok(Answered {
            status: r.get::<_, Option<i64>>(at)?.map(|n| n as u16),
            input_tokens: tokens(at + 1)?,
            cache_read_tokens: tokens(at + 2)?,
            cache_write_5m_tokens: tokens(at + 3)?,
            cache_write_1h_tokens: tokens(at + 4)?,
        })
    }

    /// `finish` writes usage onto the attempt that answered alone, so an attempt without any is not one to judge.
    fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_5m_tokens.is_some()
            || self.cache_write_1h_tokens.is_some()
    }

    /// The hit by the rule `placements` counts: a `content` rung reads up to the entry's promise, and anything else
    /// hits on a read at all, since a static entry's size is not on the usage.
    fn cache(&self, summary: &Value) -> Option<CacheFacts> {
        if !self.has_usage() {
            return None;
        }
        let read = self.cache_read_tokens.unwrap_or(0);
        let promised = summary["rung"]["prefix_tokens"].as_u64();
        let hit = match summary["rung"]["rung"].as_str() {
            Some("content") => read >= promised.unwrap_or(0),
            _ => read > 0,
        };
        Some(CacheFacts {
            hit,
            read_tokens: read,
            promised_tokens: promised,
            input_tokens: self.input_tokens.unwrap_or(0),
        })
    }
}

/// One statement's clauses and their arguments, over alias `d` for the decisions or `e` for the events.
pub(crate) struct LogStatement {
    clauses: Vec<String>,
    args: Vec<rusqlite::types::Value>,
}

impl LogStatement {
    fn new(alias: &str, filter: &RoutingLogFilter) -> LogStatement {
        let mut args: Vec<rusqlite::types::Value> = vec![filter.from.into(), filter.to.into()];
        let mut clauses = vec![format!("{alias}.at BETWEEN ?1 AND ?2")];
        if let Some(request) = &filter.request {
            args.push(request.clone().into());
            clauses.push(format!("{alias}.request_id = ?{}", args.len()));
        }
        if let Some(session) = &filter.session {
            args.push(session.clone().into());
            let exact = args.len();
            args.push(format!("{}%", literal_like(session)).into());
            clauses.push(format!(
                "({alias}.session_id = ?{exact} OR {alias}.session_id LIKE ?{} ESCAPE '{ESCAPE}')",
                exact + 1
            ));
        }
        if let Some(agent_id) = &filter.agent_id {
            args.push(agent_id.clone().into());
            clauses.push(format!("{alias}.agent_id = ?{}", args.len()));
        }
        in_list(
            &format!("{alias}.key_id"),
            &filter.keys,
            &mut args,
            &mut clauses,
        );
        in_list(
            &format!("{alias}.pool_id"),
            &filter.pools,
            &mut args,
            &mut clauses,
        );
        in_list(
            &format!("{alias}.model"),
            &filter.models,
            &mut args,
            &mut clauses,
        );
        LogStatement { clauses, args }
    }

    fn sql(&self) -> String {
        self.clauses.join(" AND ")
    }
}

/// `None` when the kinds name no decision, so the statement is never run.
pub(crate) fn decision_statement(filter: &RoutingLogFilter) -> Option<LogStatement> {
    let mut statement = LogStatement::new("d", filter);
    if !filter.accounts.is_empty() {
        let marks = bind_all(&filter.accounts, &mut statement.args);
        statement.clauses.push(format!(
            "(d.account_id IN ({marks}) OR d.from_account_id IN ({marks}))"
        ));
    }
    if let Some(kinds) = &filter.kinds {
        let of_kind = (!kinds.decisions.is_empty()).then(|| {
            format!(
                "d.kind IN ({})",
                bind_all(&kinds.decisions, &mut statement.args)
            )
        });
        let clause = match (of_kind, kinds.probes) {
            (None, false) => return None,
            (None, true) => "d.probe = 1".to_string(),
            (Some(kind), false) => format!("{kind} AND d.probe = 0"),
            (Some(kind), true) => format!("(({kind} AND d.probe = 0) OR d.probe = 1)"),
        };
        statement.clauses.push(clause);
    }
    Some(statement)
}

/// `None` when the kinds name no event kind, so the statement is never run.
fn event_statement(filter: &RoutingLogFilter) -> Option<LogStatement> {
    let mut statement = LogStatement::new("e", filter);
    in_list(
        "e.account_id",
        &filter.accounts,
        &mut statement.args,
        &mut statement.clauses,
    );
    if let Some(kinds) = &filter.kinds {
        if kinds.events.is_empty() {
            return None;
        }
        in_list(
            "e.kind",
            &kinds.events,
            &mut statement.args,
            &mut statement.clauses,
        );
    }
    Some(statement)
}

/// The page of decisions and events under `filter`, newest first, decisions before events at one `at`. Two
/// statements rather than a `UNION ALL`, so each walks its own index, merged here by the same key the cursor names.
pub fn list_routing_log(
    conn: &Connection,
    filter: &RoutingLogFilter,
    cursor: Option<&RoutingLogCursor>,
    limit: usize,
) -> Result<RoutingLogPage, LedgerError> {
    let decisions = decision_statement(filter);
    let events = event_statement(filter);
    let counts = match &filter.request {
        Some(_) => Some((
            count_of(conn, "decisions d", decisions.as_ref())?,
            count_of(conn, "events e", events.as_ref())?,
        )),
        None => None,
    };
    let page = (limit + 1) as i64;
    let mut decided = match decisions {
        Some(statement) => decision_rows(conn, statement, cursor, page)?,
        None => Vec::new(),
    }
    .into_iter()
    .peekable();
    let mut evented = match events {
        Some(statement) => event_rows(conn, statement, cursor, page)?,
        None => Vec::new(),
    }
    .into_iter()
    .peekable();
    // Both lists arrive in the page's order, so the merge takes a decision whenever it is at or after the next
    // event: decisions rank before events at one `at`.
    let mut rows = Vec::with_capacity(limit + 1);
    while rows.len() <= limit {
        let take_decision = match (decided.peek(), evented.peek()) {
            (Some(d), Some(e)) => d.at >= e.at,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        rows.push(if take_decision {
            RoutingLogRow::Decision(decided.next().expect("peeked"))
        } else {
            RoutingLogRow::Event(evented.next().expect("peeked"))
        });
    }
    let next_cursor = next_page(&mut rows, limit, |row| RoutingLogCursor::of(row).encode());
    Ok(RoutingLogPage {
        rows,
        next_cursor,
        counts,
    })
}

/// The routing log's decision page statement and its binds, spelled once so the seeded plan test explains the statement
/// the read runs, its binds and its order included.
pub(crate) fn decision_rows_statement(
    mut statement: LogStatement,
    cursor: Option<&RoutingLogCursor>,
    page: i64,
) -> (String, Vec<rusqlite::types::Value>) {
    let n = statement.args.len() + 1;
    match cursor {
        None => {}
        Some(RoutingLogCursor::Decision {
            at,
            request_id,
            seq,
        }) => {
            statement.args.push((*at).into());
            statement.args.push(request_id.clone().into());
            statement.args.push((*seq).into());
            statement.clauses.push(format!(
                "(d.at < ?{n} OR (d.at = ?{n} AND (d.request_id < ?{} OR (d.request_id = ?{} AND d.seq < ?{}))))",
                n + 1,
                n + 1,
                n + 2
            ));
        }
        // Decisions rank before events at one `at`, so an event's cursor already passed every decision at its own.
        Some(RoutingLogCursor::Event { at, .. }) => {
            statement.args.push((*at).into());
            statement.clauses.push(format!("d.at < ?{n}"));
        }
    }
    statement.args.push(page.into());
    let sql = format!(
        "SELECT d.request_id, d.seq, d.attempt, d.at, d.kind, d.probe, d.key_id, d.pool_id, d.session_id, \
         d.agent_id, d.model, d.account_id, d.from_account_id, d.summary, r.attempts, a.upstream_status, \
         a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens \
         FROM decisions d LEFT JOIN requests r ON r.id = d.request_id \
         LEFT JOIN attempts a ON a.request_id = d.request_id AND a.attempt = ({completing}) \
         WHERE {} ORDER BY d.at DESC, d.request_id DESC, d.seq DESC LIMIT ?{}",
        statement.sql(),
        statement.args.len(),
        completing = completing_attempt(),
    );
    (sql, statement.args)
}

/// Up to `page` decisions past `cursor`, each with the facts of the attempt that completed it.
fn decision_rows(
    conn: &Connection,
    statement: LogStatement,
    cursor: Option<&RoutingLogCursor>,
    page: i64,
) -> Result<Vec<DecisionListRow>, LedgerError> {
    let (sql, args) = decision_rows_statement(statement, cursor, page);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args), |r| {
            Ok((
                DecisionListRow {
                    request_id: r.get(0)?,
                    seq: r.get(1)?,
                    attempt: r.get(2)?,
                    attempt_count: r.get(14)?,
                    at: r.get(3)?,
                    kind: r.get(4)?,
                    probe: r.get(5)?,
                    key_id: r.get(6)?,
                    pool_id: r.get(7)?,
                    session_id: r.get(8)?,
                    agent_id: r.get(9)?,
                    model: r.get(10)?,
                    account_id: r.get(11)?,
                    from_account_id: r.get(12)?,
                    summary: Value::Null,
                    cache: None,
                    status: None,
                },
                r.get::<_, String>(13)?,
                Answered::read(r, 15)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(row, summary, answered)| {
            let summary: Value = serde_json::from_str(&summary)?;
            Ok(DecisionListRow {
                cache: answered.cache(&summary),
                status: answered.status,
                summary,
                ..row
            })
        })
        .collect()
}

/// Up to `page` events past `cursor`, each with its detail decoded.
fn event_rows(
    conn: &Connection,
    mut statement: LogStatement,
    cursor: Option<&RoutingLogCursor>,
    page: i64,
) -> Result<Vec<EventListRow>, LedgerError> {
    let n = statement.args.len() + 1;
    match cursor {
        None => {}
        // Decisions rank before events at one `at`, so a decision's cursor leaves every event at its own to read.
        Some(RoutingLogCursor::Decision { at, .. }) => {
            statement.args.push((*at).into());
            statement.clauses.push(format!("e.at <= ?{n}"));
        }
        Some(RoutingLogCursor::Event { at, rowid }) => {
            statement.args.push((*at).into());
            statement.args.push((*rowid).into());
            statement.clauses.push(format!(
                "(e.at < ?{n} OR (e.at = ?{n} AND e.rowid < ?{}))",
                n + 1
            ));
        }
    }
    statement.args.push(page.into());
    let sql = format!(
        "SELECT e.rowid, e.at, e.kind, e.account_id, e.key_id, e.pool_id, e.session_id, e.agent_id, e.model, \
         e.request_id, e.actor, e.detail FROM events e WHERE {} ORDER BY e.at DESC, e.rowid DESC LIMIT ?{}",
        statement.sql(),
        statement.args.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(&statement.args), |r| {
        Ok(EventListRow {
            id: r.get(0)?,
            at: r.get(1)?,
            kind: r.get(2)?,
            account_id: r.get(3)?,
            key_id: r.get(4)?,
            pool_id: r.get(5)?,
            session_id: r.get(6)?,
            agent_id: r.get(7)?,
            model: r.get(8)?,
            request_id: r.get(9)?,
            actor: r.get(10)?,
            detail: detail_json(r.get::<_, Option<String>>(11)?.as_deref()),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn count_of(
    conn: &Connection,
    table: &str,
    statement: Option<&LogStatement>,
) -> Result<u64, LedgerError> {
    let Some(statement) = statement else {
        return Ok(0);
    };
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {}", statement.sql()),
        params_from_iter(&statement.args),
        |r| r.get(0),
    )?;
    Ok(count as u64)
}

/// Record v2 decoded from its frame with `outcome.completion` merged in, or `None` when the row is absent.
pub fn decision_record(
    conn: &Connection,
    request_id: &str,
    seq: u32,
) -> Result<Option<Value>, LedgerError> {
    let row = conn
        .query_row(
            &format!(
                "SELECT d.kind, d.account_id, d.summary, d.record, d.cache_map, r.held, a.upstream_status, \
                 a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens, a.finished_at \
                 FROM decisions d LEFT JOIN requests r ON r.id = d.request_id \
                 LEFT JOIN attempts a ON a.request_id = d.request_id AND a.attempt = ({completing}) \
                 WHERE d.request_id = ?1 AND d.seq = ?2",
                completing = completing_attempt(),
            ),
            params![request_id, seq],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    Answered::read(r, 6)?,
                    r.get::<_, Option<i64>>(11)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, account_id, summary, frame, cache_map, held, answered, completed_at)) = row
    else {
        return Ok(None);
    };
    let mut record = crate::record::decode_record(&frame)?;
    let completion = if DISPATCHING_KINDS.contains(&kind.as_str()) {
        let summary: Value = serde_json::from_str(&summary)?;
        let cache = answered.cache(&summary);
        let held = detail_json(held.as_deref());
        // A wait that ran out puts its account among the attempted, so a dispatch to an account the request waited on
        // is the one its freed slot took.
        let freed = held["waited_on"]
            .as_array()
            .is_some_and(|waited| waited.iter().any(|w| w.as_str() == account_id.as_deref()));
        serde_json::json!({
            "status": answered.status,
            "cache_read_tokens": cache.as_ref().map(|c| c.read_tokens),
            "input_tokens": cache.as_ref().map(|c| c.input_tokens),
            "promised_tokens": summary["rung"]["prefix_tokens"].as_u64(),
            "hit": cache.as_ref().map(|c| c.hit),
            "held_ms": held["held_ms"].clone(),
            "freed": freed,
            "cache_map_written": detail_json(cache_map.as_deref()),
            "completed_at": completed_at,
        })
    } else {
        Value::Null
    };
    if let Some(outcome) = record.get_mut("outcome").and_then(Value::as_object_mut) {
        outcome.insert("completion".into(), completion);
    }
    let mut answer = serde_json::Map::new();
    answer.insert("request_id".into(), request_id.into());
    answer.insert("seq".into(), seq.into());
    answer.extend(record);
    Ok(Some(Value::Object(answer)))
}

/// The `pin_migrated` events of `[from, to)` counted by reason per account, so a page with several pools reads the
/// events once and sums each pool's members in memory. An account without a move is absent. The reason is the move's
/// `detail.reason`, `unknown` when the event named none, and the read takes the events table's `(kind, at)` index
/// rather than an hourly rollup bucket, so a pool card's "last hour" figure is the last hour exactly.
pub fn pin_migrations_by_account_and_reason(
    conn: &Connection,
    from: i64,
    to: i64,
) -> Result<BTreeMap<String, BTreeMap<String, u64>>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT account_id, detail FROM events WHERE kind = ?3 AND at >= ?1 AND at < ?2",
    )?;
    let rows = stmt.query_map(params![from, to, PIN_MIGRATED], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ))
    })?;
    let mut out: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for row in rows {
        let (account_id, detail) = row?;
        let Some(account_id) = account_id else {
            continue;
        };
        let reason = migration_reason(&detail_json(detail.as_deref())).to_string();
        *out.entry(account_id)
            .or_default()
            .entry(reason)
            .or_insert(0) += 1;
    }
    Ok(out)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TokenSplit {
    pub cache_read: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub output: u64,
    pub thinking: u64,
    pub uncached_input: u64,
    pub web_fetch: u64,
    pub web_search: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CostSplitJson {
    pub cache_read: f64,
    pub cache_write: f64,
    pub input: f64,
    pub output: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Histograms {
    pub duration_ms: Vec<u64>,
    pub output_tps: Vec<u64>,
    pub ttfb_ms: Vec<u64>,
    pub ttft_ms: Vec<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Quartet {
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

impl Quartet {
    fn of(h: &Histogram) -> Quartet {
        Quartet {
            p50: h.percentile(0.5),
            p90: h.percentile(0.9),
            p95: h.percentile(0.95),
            p99: h.percentile(0.99),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Percentiles {
    pub duration_ms: Quartet,
    pub output_tps: Quartet,
    pub ttfb_ms: Quartet,
    pub ttft_ms: Quartet,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct KeptWarm {
    pub requests: u64,
    pub tokens_5m: u64,
    pub tokens_1h: u64,
    /// The credits whose request a plan account served, so a reader tells an unpriced plan credit from billed ones.
    pub plan_requests: u64,
    /// The plan-served credits' reads, split by TTL as `tokens_5m` and `tokens_1h` are.
    pub plan_tokens_5m: u64,
    pub plan_tokens_1h: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProbeCounts {
    pub failed: u64,
    pub held: u64,
    pub migrated: u64,
    pub ok: u64,
    /// The answered probes a plan account served.
    pub plan_ok: u64,
    pub skipped: u64,
}

/// The part of one probe row the probe counts read: its outcome counters and its funding. The ledger's group fold
/// reads it off a `RollupRow` and the keepalive overview's fold off the narrower row it reads, so both take one rule.
#[derive(Clone, Copy, Debug)]
pub struct ProbeView {
    pub exhausted: u64,
    pub failed: u64,
    pub funding: Funding,
    pub incomplete: u64,
    pub probe_migrated: u64,
    pub probes_held: u64,
    pub probes_skipped: u64,
    pub rate_limited: u64,
    pub rejected: u64,
    pub succeeded: u64,
}

impl ProbeCounts {
    /// Folds in the counts of one probe row: a probe's own request row carries its outcome, and the engine's deltas
    /// carry what never ran. `plan_ok` reads the row's funding, which a sum of rows would no longer key.
    pub fn add_row(&mut self, row: ProbeView) {
        self.failed +=
            row.failed + row.rate_limited + row.exhausted + row.rejected + row.incomplete;
        self.held += row.probes_held;
        self.migrated += row.probe_migrated;
        self.ok += row.succeeded;
        if row.funding == Funding::Plan {
            self.plan_ok += row.succeeded;
        }
        self.skipped += row.probes_skipped;
    }
}

/// Whether every group of a read holds one account's rows, the one place the own-window shares add: the account
/// grouping, or a filter naming exactly one account under any grouping. A read answers the shares only there, since
/// shares of different accounts' windows do not add, and each row's own account tier prices it.
fn holds_one_account(by: GroupBy, accounts: &[String]) -> bool {
    by == GroupBy::Account || names_one_account(accounts)
}

/// Whether a filter names exactly one account, a name given twice counting once. The burn route gates its `unit` on
/// it, so the unit and the shares answer under one condition.
pub fn names_one_account(accounts: &[String]) -> bool {
    accounts.iter().collect::<BTreeSet<_>>().len() == 1
}

/// The read and write halves of a group's plan-funded cache in the Pro-week figure.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CacheEquivalent {
    pub read: f64,
    pub write: f64,
}

/// The plan-funded probe rows in the Pro-week figure and the wasted probes' own counts priced in it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ProbeEquivalent {
    pub cost: f64,
    pub wasted: f64,
}

/// One Pro-week figure summed row by row: `None` once a plan row with tokens priced nothing, so a partial sum never
/// reads as the whole, `Some(0.0)` while no plan row has been added.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct EquivalentSum(Latch);

impl EquivalentSum {
    /// A `None` latches the sum only on a row with tokens, as a row with no token charged nothing.
    fn add(&mut self, figure: Option<f64>, tokens: u64) {
        if figure.is_some() || tokens > 0 {
            self.0.add(figure);
        }
    }

    pub(crate) fn figure(self) -> Option<f64> {
        match self.0 {
            Latch::Idle => Some(0.0),
            latch => latch.figure(),
        }
    }
}

/// Two sums of the Pro-week figure added: a `None` stays `None`, since either sum already met a plan row that priced
/// nothing.
fn add_figures(sum: Option<f64>, figure: Option<f64>) -> Option<f64> {
    sum.zip(figure).map(|(sum, figure)| sum + figure)
}

/// One row priced for a reader that sums across models, its figures as `shared_of` reads them.
fn shared_figures(
    row: &RollupRow,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> (f64, f64, Option<f64>) {
    shared_of(
        &priced(row, prices, costs, now),
        row.funding_kind(),
        row.usage(),
    )
}

/// A priced usage's dollars, the billed part of them, and its Pro-week figure as a sum of one row, so a billed row
/// reads zero and a plan row with tokens the cost model cannot price `None`. A billed row's figure is `None` too,
/// since dollars fund it, and adds nothing.
fn shared_of(priced: &Priced, funding: Funding, usage: PinUsage) -> (f64, f64, Option<f64>) {
    let usd = priced.usd.as_ref().map_or(0.0, |usd| usd.total());
    let mut equivalent = EquivalentSum::default();
    if funding == Funding::Plan {
        equivalent.add(priced.equivalent, tokens_of(usage));
    }
    let billed = if funding == Funding::Plan { 0.0 } else { usd };
    (usd, billed, equivalent.figure())
}

#[derive(Clone, Debug, Serialize)]
pub struct GroupRow {
    pub key: String,
    pub label: String,
    /// The key's id under `group_by=key`, so a reader prints the holder's creation in its own zone. Absent for
    /// another dimension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// An earlier holder's `created_at` in Unix milliseconds, 0 for a generation with no creation time. Absent for
    /// the live key, the reserved id and another dimension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_created_at: Option<i64>,
    pub requests: u64,
    pub succeeded: u64,
    pub rate_limited: u64,
    pub failed: u64,
    pub exhausted: u64,
    pub rejected: u64,
    pub attempts: u64,
    pub failovers: u64,
    pub incomplete: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub uncached: u64,
    pub expired_between_turns: u64,
    pub tokens: TokenSplit,
    pub cost: CostSplitJson,
    pub cost_usd: f64,
    pub errors: BTreeMap<String, u64>,
    /// The refused requests of the group, the sum of `refusals`, not in `requests`.
    pub refused: u64,
    /// The refused requests of the group by error code.
    pub refusals: BTreeMap<String, u64>,
    pub cache_miss_reasons: BTreeMap<String, u64>,
    pub histograms: Histograms,
    pub percentiles: Percentiles,
    pub kept_warm: KeptWarm,
    pub probes: ProbeCounts,
    pub probe_cost_usd: f64,
    pub wasted_probes: u64,
    pub wasted_probe_usd: f64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
    /// Keyed by window id, `five_hour`, `seven_day` and the model week, each plan row priced under the account tier
    /// it recorded. A window is absent where a plan row with a tier could not be priced on it, never for a row whose
    /// model the window does not apply to. `None` for a group whose rows carry no tier (an api_key group, an oauth
    /// group whose profile was not read, the rows folded before the tier was recorded) and wherever the read's
    /// groups do not each hold one account's rows.
    pub cost_shares: Option<IndexMap<&'static str, f64>>,
    /// The plan-funded requests and output tokens the shares and the equivalent are summed over, the denominators
    /// of a figure per request and per 1k output tokens. Answered under every grouping.
    pub plan_requests: u64,
    pub plan_output_tokens: u64,
    /// The plan rows as the Pro-week figure, under every grouping: `None` where a plan row with tokens priced
    /// nothing, a model without a weight, and 0 for a group with no plan row.
    pub cost_equivalent: Option<f64>,
    /// The plan rows' cache reads and writes in the Pro-week figure, `None` as `cost_equivalent` is.
    pub cache_equivalent: Option<CacheEquivalent>,
    /// The plan-funded probe rows and their wasted counts in the Pro-week figure, `None` where a probe row or a wasted
    /// count priced nothing.
    pub probe_equivalent: Option<ProbeEquivalent>,
    /// The kept-warm requests' saving, gross of the probes: the write each count avoided at its TTL's rate less the
    /// read it paid, summed over the group's rows.
    pub saving_usd: f64,
    /// The saving in the Pro-week figure, under every grouping, `None` where a kept-warm count priced nothing.
    pub saving_equivalent: Option<f64>,
    /// The dollars of the billed rows alone (`fast`, `overage`, `billed`).
    pub billed_usd: f64,
    pub billed: CostSplitJson,
    /// The plan rows' Pro-week figure by token kind, `input`, `cache_read`, `cache_write` and `output`, which sum to
    /// `cost_equivalent`, `None` where that is.
    pub equivalent_split: Option<IndexMap<&'static str, f64>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SeriesPoint {
    pub at: i64,
    pub groups: Vec<GroupRow>,
}

/// Which rows a read keeps by who paid for them, off the funding each row was recorded under: `Billed` the billed,
/// overage, fast and usage-based rows, `Plan` the plan rows. The two partition the rows, so their sums add up to the
/// unfiltered sum, and the registry's later state moves no row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingFilter {
    Billed,
    Plan,
}

#[derive(Clone, Debug, Default)]
pub struct RollupFilter {
    /// A value is a key id, which names the rows of the key holding it now, or `<id>@<generation>`, which
    /// names an earlier holder's.
    pub keys: Vec<String>,
    pub pools: Vec<String>,
    pub accounts: Vec<String>,
    pub models: Vec<String>,
    pub kinds: Vec<String>,
    /// Patterns under the wildcard grammar; a row matches when any pattern matches its agent in full.
    pub user_agents: Patterns,
    /// Under `group_by=user_agent`, each pattern names a group and a row falls in the first that matches it;
    /// a row no pattern matches groups under `other`. Empty means every agent is its own group.
    pub user_agent_groups: Patterns,
    /// Patterns under the wildcard grammar; a row matches when any pattern matches its client in full.
    pub clients: Patterns,
    pub include_probes: bool,
    /// Applied in memory, off the key's funding word. `None` keeps every row.
    pub funding: Option<FundingFilter>,
}

impl RollupFilter {
    /// Whether a probe row counts as a request: the flag, or a kind filter that asked for probes outright.
    /// The probe side of a group is summed either way, so the probe cost and the wasted probes read the same under
    /// both.
    pub fn probes_counted(&self) -> bool {
        self.include_probes || self.kinds.iter().any(|k| k == Kind::Probe.as_str())
    }

    /// The fixed filters and the pattern filters as the read binds them, each key selector resolved as
    /// `LiveKeys::selects` reads it: the reserved id at any generation, every other selector at the lifetime
    /// `lifetime_of` answers. `None` when the filter names keys and none of them names a lifetime, which selects no row
    /// rather than every row.
    pub fn rollup_select(&self, live: &LiveKeys) -> Option<RollupSelect> {
        let keys = key_selectors(&self.keys, live);
        if !self.keys.is_empty() && keys.is_empty() {
            return None;
        }
        Some(RollupSelect {
            pools: self.pools.clone(),
            accounts: self.accounts.clone(),
            kinds: self.kinds.clone(),
            models: canonical_models(&self.models),
            keys,
            clients: self.clients.to_vec(),
            user_agents: self.user_agents.to_vec(),
            ..RollupSelect::default()
        })
    }
}

/// A dimension the analytics group their rows by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    Account,
    All,
    Classifier,
    Client,
    Effort,
    Key,
    Kind,
    Model,
    Pool,
    RequestClass,
    Speed,
    UserAgent,
}

impl GroupBy {
    const EVERY: [GroupBy; 12] = [
        GroupBy::Account,
        GroupBy::All,
        GroupBy::Classifier,
        GroupBy::Client,
        GroupBy::Effort,
        GroupBy::Key,
        GroupBy::Kind,
        GroupBy::Model,
        GroupBy::Pool,
        GroupBy::RequestClass,
        GroupBy::Speed,
        GroupBy::UserAgent,
    ];

    /// The dimension a `group_by` word names, `None` for a word no dimension takes.
    pub fn parse(name: &str) -> Option<GroupBy> {
        GroupBy::EVERY.into_iter().find(|by| by.as_str() == name)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            GroupBy::Account => "account",
            GroupBy::All => "all",
            GroupBy::Classifier => "classifier",
            GroupBy::Client => "client",
            GroupBy::Effort => "effort",
            GroupBy::Key => "key",
            GroupBy::Kind => "kind",
            GroupBy::Model => "model",
            GroupBy::Pool => "pool",
            GroupBy::RequestClass => "request_class",
            GroupBy::Speed => "speed",
            GroupBy::UserAgent => "user_agent",
        }
    }
}

/// The `group_by` words, one per `GroupBy` in its order.
pub const GROUP_BYS: &[&str] = &{
    let mut words = [""; GroupBy::EVERY.len()];
    let mut i = 0;
    while i < words.len() {
        words[i] = GroupBy::EVERY[i].as_str();
        i += 1;
    }
    words
};

/// The group a pattern list files an agent under when no pattern matches.
pub const OTHER_AGENTS: &str = "other";

/// The live keys' generations by id, from the registry snapshot: what tells a row of the current holder from a
/// row of an earlier one wherever a figure is attributed to a key.
#[derive(Clone, Debug, Default)]
pub struct LiveKeys(pub BTreeMap<String, i64>);

impl LiveKeys {
    pub fn from_snapshot(snapshot: &phoebis_routing::RegistrySnapshot) -> LiveKeys {
        LiveKeys(
            snapshot
                .keys()
                .map(|k| (k.id.clone(), k.created_at))
                .collect(),
        )
    }

    /// The totals of each id's live generation out of an answer per key and generation, so a key's figures never
    /// carry an earlier holder's. An id no live key holds, and a live key whose generation read no rows, are left out.
    pub fn live_totals(
        &self,
        by_generation: BTreeMap<String, BTreeMap<i64, Totals>>,
    ) -> BTreeMap<String, Totals> {
        by_generation
            .into_iter()
            .filter_map(|(id, mut generations)| {
                let totals = generations.remove(self.0.get(&id)?)?;
                Some((id, totals))
            })
            .collect()
    }

    /// The group a rollup row falls in under `group_by=key`: the id alone for the live generation,
    /// `<id>@<generation>` for any other, the reserved id as itself.
    pub fn key_of(&self, row: &RollupRow) -> String {
        if row.key_id == UNATTRIBUTED || self.0.get(&row.key_id) == Some(&row.key_generation) {
            return row.key_id.clone();
        }
        format!("{}@{}", row.key_id, row.key_generation)
    }

    /// The lifetime a `keys=` selector names: a bare id is `(id, the live generation)`, `<id>@<generation>` is
    /// that pair; `None` for a bare id no live key holds, a malformed generation, or the reserved id.
    pub fn lifetime_of<'s>(&self, selector: &'s str) -> Option<(&'s str, i64)> {
        if selector == UNATTRIBUTED {
            return None;
        }
        match selector.rsplit_once('@') {
            // The reserved id has no generation of its own; `-` alone names its rows.
            Some((id, _)) if id == UNATTRIBUTED => None,
            Some((id, generation)) => generation.parse().ok().map(|g| (id, g)),
            None => self.0.get(selector).map(|g| (selector, *g)),
        }
    }

    /// Whether a `keys=` selector names the row: the lifetime `lifetime_of` answers, or every row of the
    /// reserved id for a bare `-`, whatever its generation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn selects(&self, selector: &str, row: &RollupRow) -> bool {
        // The reserved id has no lifetime: what the router never attributed is one group whatever the
        // generation column says.
        if selector == UNATTRIBUTED {
            return row.key_id == UNATTRIBUTED;
        }
        self.lifetime_of(selector)
            .is_some_and(|(id, generation)| row.key_id == id && row.key_generation == generation)
    }
}

/// How a read files its rows into groups: the dimension, the live keys the key dimension reads a row's generation
/// against, and the patterns that name the agent groups.
#[derive(Clone, Copy)]
struct Grouping<'a> {
    agent_groups: &'a [String],
    by: GroupBy,
    live: &'a LiveKeys,
}

impl Grouping<'_> {
    /// The group a row falls in.
    fn group_of(&self, row: &RollupRow) -> String {
        match self.by {
            GroupBy::Account => row.account_id.clone(),
            GroupBy::All => "all".into(),
            GroupBy::Classifier => row.classifier.clone(),
            GroupBy::Client => row.client.clone(),
            GroupBy::Effort => row.effort.clone(),
            GroupBy::Key => self.live.key_of(row),
            GroupBy::Kind => row.kind.clone(),
            GroupBy::Model => row.canonical_model.clone(),
            GroupBy::Pool => row.pool_id.clone(),
            GroupBy::RequestClass => row.request_class.clone(),
            GroupBy::Speed => row.speed.clone(),
            GroupBy::UserAgent => {
                if self.agent_groups.is_empty() {
                    return row.user_agent.clone();
                }
                self.agent_groups
                    .iter()
                    .find(|pattern| crate::pattern::matches(pattern, &row.user_agent))
                    .cloned()
                    .unwrap_or_else(|| OTHER_AGENTS.into())
            }
        }
    }

    /// A group's answered row under its label, the key dimension's row naming the holder its key names, for a reader
    /// that prints the holder's creation in its own zone.
    fn row_of(&self, key: &str, group: GroupSum) -> GroupRow {
        let mut row = group_row(key, group, label_of(key, self.by));
        if self.by == GroupBy::Key {
            let (key_id, key_created_at) = holder_of(key);
            row.key_id = Some(key_id);
            row.key_created_at = key_created_at;
        }
        row
    }
}

/// The label of a group: the group key itself, `none` for the classifier grouping's reserved id, the requests with no
/// classifier call, as the Requests facet names them, `unattributed` for the reserved id under every other dimension,
/// and "no model" for the model grouping's empty key, the requests the router rejected before routing. Each of these
/// groups keeps its key, so a filter on it keeps working. Under the key dimension, the live generation reads its id
/// and every generation no live key holds, generation 0 included, reads `<id> (deleted)`. Two earlier holders of one id
/// share the label, and `holder_of` answers each one's creation instant for a reader to print in its own zone.
fn label_of(key: &str, by: GroupBy) -> String {
    if by == GroupBy::Key {
        // A key id never carries `@` (the admin's slug rule), so the split is unambiguous.
        if let Some((id, _)) = key.rsplit_once('@') {
            return format!("{id} (deleted)");
        }
    }
    match (key, by) {
        (UNATTRIBUTED, GroupBy::Classifier) => "none".into(),
        (UNATTRIBUTED, _) => "unattributed".into(),
        ("", GroupBy::Model) => "no model".into(),
        _ => key.into(),
    }
}

/// The holder a key group names beside its label, for a reader that prints the creation instant in its own zone: the
/// id, and an earlier holder's generation. The live key and the reserved id carry no generation.
fn holder_of(key: &str) -> (String, Option<i64>) {
    match key.rsplit_once('@') {
        Some((id, generation)) => (id.into(), generation.parse().ok()),
        None => (key.into(), None),
    }
}

/// Whether a row passes the filter's two pattern filters, as the rollup read's `LIKE` terms pass it.
#[cfg(any(test, feature = "test-support"))]
fn passes_patterns(r: &RollupRow, f: &RollupFilter) -> bool {
    (f.user_agents.is_empty()
        || f.user_agents
            .iter()
            .any(|p| crate::pattern::matches(p, &r.user_agent)))
        && (f.clients.is_empty()
            || f.clients
                .iter()
                .any(|p| crate::pattern::matches(p, &r.client)))
}

/// Whether a row passes the filter's funding predicate, read off the key's funding word alone: the key carries the
/// served attempt's funding whole, an api_key account's rows under `billed`, so the registry's current state moves
/// no row. `Billed` and `Plan` are complements, so the two reads partition the rows.
fn passes_funding(r: &RollupRow, f: &RollupFilter) -> bool {
    match f.funding {
        None => true,
        Some(FundingFilter::Billed) => r.funding_kind() != Funding::Plan,
        Some(FundingFilter::Plan) => r.funding_kind() == Funding::Plan,
    }
}

/// The rows the filter's two pattern filters keep, for the unpaged twins `breakdown` and `series`, which read rows
/// held in memory where the paged readers' read binds the patterns in SQL (`RollupFilter::rollup_select`). Probe rows
/// stay among them: whether those count as requests is `probes_counted`, which the sums take, so a probe's cost
/// reaches the group under every filter.
#[cfg(any(test, feature = "test-support"))]
pub fn apply_filter(rows: Vec<RollupRow>, f: &RollupFilter) -> Vec<RollupRow> {
    rows.into_iter().filter(|r| passes_patterns(r, f)).collect()
}

/// One rollup row priced through `price_row` at its own key with no tier, a plain call that fills nothing in after it:
/// a probe row on a model the revision lacks takes its dearest-cell dollars inside `price_row`. Its reader,
/// `shared_figures`, reads its `usd` and `equivalent` and not its `shares`, since the shares a route answers come from
/// `ShareSums::add` where the read's groups each hold one account's rows. So the key carries no tier, as
/// `price_group`'s does, and `price_row` prices no share window.
pub(crate) fn priced(
    row: &RollupRow,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Priced {
    let key = RowKey {
        tier: None,
        ..row.price_key(now)
    };
    price_row(prices, costs, &key, row.usage())
}

fn cost_split_json(usd: &CostSplit) -> CostSplitJson {
    CostSplitJson {
        cache_read: usd.cache_read_usd,
        cache_write: usd.cache_write_usd,
        input: usd.input_usd,
        output: usd.output_usd,
    }
}

fn group_row(key: &str, group: GroupSum, label: String) -> GroupRow {
    let GroupSum {
        sum,
        probes,
        probe_counts,
        dollars,
        shares,
        plan,
        ..
    } = group;
    let cost_shares = shares.map(ShareSums::finish);
    let equivalent = plan.equivalent.finish(plan.split);
    let mut reasons = sum.cache_miss_reasons;
    let expired = reasons.remove(EXPIRED_BETWEEN_TURNS).unwrap_or(0);
    GroupRow {
        key: key.into(),
        label,
        key_id: None,
        key_created_at: None,
        requests: sum.requests,
        succeeded: sum.succeeded,
        rate_limited: sum.rate_limited,
        failed: sum.failed,
        exhausted: sum.exhausted,
        rejected: sum.rejected,
        attempts: sum.attempts,
        failovers: sum.failovers,
        incomplete: sum.incomplete,
        cache_hits: sum.cache_hits,
        cache_misses: sum.cache_misses,
        uncached: sum.uncached_requests,
        expired_between_turns: expired,
        tokens: TokenSplit {
            cache_read: sum.cache_read_tokens,
            cache_write_1h: sum.cache_write_1h_tokens,
            cache_write_5m: sum.cache_write_5m_tokens,
            output: sum.output_tokens,
            thinking: sum.thinking_tokens,
            uncached_input: sum.input_tokens,
            web_fetch: sum.web_fetch_requests,
            web_search: sum.web_search_requests,
        },
        cost: cost_split_json(&dollars.usd),
        cost_usd: dollars.usd.total(),
        errors: sum.errors,
        refused: sum.refusals.values().sum(),
        refusals: sum.refusals,
        cache_miss_reasons: reasons,
        histograms: Histograms {
            duration_ms: sum.duration_hist.buckets(),
            output_tps: sum.output_tps_hist.buckets(),
            ttfb_ms: sum.ttfb_hist.buckets(),
            ttft_ms: sum.ttfc_hist.buckets(),
        },
        percentiles: Percentiles {
            duration_ms: Quartet::of(&sum.duration_hist),
            output_tps: Quartet::of(&sum.output_tps_hist),
            ttfb_ms: Quartet::of(&sum.ttfb_hist),
            ttft_ms: Quartet::of(&sum.ttfc_hist),
        },
        kept_warm: KeptWarm {
            requests: sum.kept_warm_requests,
            tokens_5m: sum.kept_warm_5m_tokens,
            tokens_1h: sum.kept_warm_1h_tokens,
            plan_requests: plan.kept_warm_requests,
            plan_tokens_5m: plan.kept_warm_5m_tokens,
            plan_tokens_1h: plan.kept_warm_1h_tokens,
        },
        probes: probe_counts,
        // From the probe rows: what the cache cost to keep warm is not a request the filter can drop.
        probe_cost_usd: dollars.probe_usd,
        wasted_probes: probes.wasted_probes,
        wasted_probe_usd: dollars.wasted_usd,
        wasted_probe_reasons: probes.wasted_probe_reasons,
        cost_shares,
        plan_requests: plan.requests,
        plan_output_tokens: plan.output_tokens,
        cost_equivalent: equivalent.cost,
        cache_equivalent: equivalent.cache,
        probe_equivalent: equivalent.probe,
        saving_usd: dollars.saving_usd,
        saving_equivalent: equivalent.saving,
        billed_usd: dollars.billed.total(),
        billed: cost_split_json(&dollars.billed),
        equivalent_split: equivalent.split,
    }
}

/// The five equivalent members of a `GroupRow`, folded from a group's `EquivalentSums` and its split.
struct GroupEquivalent {
    cost: Option<f64>,
    cache: Option<CacheEquivalent>,
    probe: Option<ProbeEquivalent>,
    saving: Option<f64>,
    split: Option<IndexMap<&'static str, f64>>,
}

/// A group's shares summed row by row as the rows arrive: the summed keys of `GroupSum` blank the account, the model
/// and the hour a figure is priced from, so the figures fold here from the incoming row. Keyed by window in the order
/// the rows introduced them.
#[derive(Default)]
struct ShareSums {
    cost: IndexMap<&'static str, Latch>,
}

impl ShareSums {
    /// Folds one plan-funded row in when `counted`, as the request dollars take the row, each window priced at the
    /// row's own hour under `tier` and the regime the row's `client` names, so a group that mixes clients prices each
    /// row under its own.
    fn add(
        &mut self,
        row: &RollupRow,
        key: &RowKey<'_>,
        tier: &str,
        tables: &PlanTables,
        counted: bool,
    ) {
        let usage = row.usage();
        // A failed request books a row with no token and an accounting row carries the wasted counts alone, while the
        // cost model refuses a model it has no weight for before it reads the usage: such a row would null a priced
        // window for a charge of nothing.
        if !counted || tokens_of(usage) == 0 {
            return;
        }
        let shares = if key.classifier {
            free_shares(tables, key.entrypoint, tier, key.model, key.at)
        } else {
            price_shares(
                tables,
                key.entrypoint,
                tier,
                key.model,
                usage,
                key.at,
                key.at,
            )
        };
        for (window, share) in shares {
            self.cost
                .entry(window)
                .or_insert(Latch::Sum(0.0))
                .add(share);
        }
    }

    /// A window a row a known tier holds could not be priced on is absent from the answered map, as `price_usage`
    /// nulls a bucket's, so a partial sum never reads as the share.
    fn finish(self) -> IndexMap<&'static str, f64> {
        self.cost
            .into_iter()
            .filter_map(|(w, s)| s.figure().map(|sum| (w, sum)))
            .collect()
    }
}

/// A group's plan-funded side in the Pro-week figure, summed row by row as `ShareSums` sums the shares: every row it
/// takes is a plan row, as `PlanSums` folds them.
#[derive(Default)]
struct EquivalentSums {
    cost: EquivalentSum,
    cache_read: EquivalentSum,
    cache_write: EquivalentSum,
    probe_cost: EquivalentSum,
    /// The wasted probes' own counts. A settlement files under the funding its probe's own row took, so an overage
    /// probe's waste files under `overage` and never reaches the plan's figure.
    wasted: EquivalentSum,
    /// The kept-warm counts' saving, the write they avoided less the read they paid, both halves priced under the one
    /// model and hour.
    saving: EquivalentSum,
}

impl EquivalentSums {
    /// Folds one plan row in, its own usage at `whole`, the figure `price_row` answered on its counts, so only the
    /// cache halves, the wasted counts and the kept-warm counts are priced again, through `price`. Answers the cache
    /// halves' figures where it priced them, for a counted row with tokens.
    fn add_priced(
        &mut self,
        row: &RollupRow,
        counted: bool,
        whole: Option<f64>,
        price: impl Fn(PinUsage) -> Option<f64>,
    ) -> Option<CacheHalves> {
        let usage = row.usage();
        if row.kind == Kind::Probe.as_str() {
            let wasted = row.wasted_usage();
            if tokens_of(wasted) > 0 {
                self.wasted.add(price(wasted), tokens_of(wasted));
            }
        }
        // Priced ahead of the return below: a credit's row carries no request token, since it keys the reserved agent,
        // class and effort apart from its request's own row.
        let kept = row.kept_warm_usage();
        if tokens_of(kept) > 0 {
            let (read, write) = cache_parts(kept);
            let (read, write) = (price(read), price(write));
            self.saving.add(
                read.zip(write).map(|(read, write)| write - read),
                tokens_of(kept),
            );
        }
        // A failed request books a row with no token and an accounting row carries the wasted counts alone: such a row
        // charged nothing and leaves the sums as they stand.
        let tokens = tokens_of(usage);
        if tokens == 0 {
            return None;
        }
        let halves = counted.then(|| {
            self.cost.add(whole, tokens);
            let (read, write) = cache_parts(usage);
            let halves = (price(read), price(write));
            // Both halves read the one model, so a row that priced nothing latches both, whatever tokens each carries.
            self.cache_read.add(halves.0, tokens);
            self.cache_write.add(halves.1, tokens);
            halves
        });
        if row.kind == Kind::Probe.as_str() {
            self.probe_cost.add(whole, tokens);
        }
        halves
    }

    fn finish(self, split: SplitSum) -> GroupEquivalent {
        // The wasted part rides its probe cost: a wasted count that priced nothing leaves the pair `None`, as a cost
        // that did.
        let probe = self
            .probe_cost
            .figure()
            .zip(self.wasted.figure())
            .map(|(cost, wasted)| ProbeEquivalent { cost, wasted });
        GroupEquivalent {
            cost: self.cost.figure(),
            cache: self
                .cache_read
                .figure()
                .zip(self.cache_write.figure())
                .map(|(read, write)| CacheEquivalent { read, write }),
            probe,
            saving: self.saving.figure(),
            split: split.finish(),
        }
    }
}

/// A plan row's cache read and cache write halves in the Pro-week figure, as `EquivalentSums` priced them.
type CacheHalves = (Option<f64>, Option<f64>);

/// The plan rows' Pro-week figure by token kind, latched as the figure is: a row that priced nothing leaves the split
/// `None`, as it leaves `cost_equivalent`.
#[derive(Default)]
struct SplitSum {
    input: f64,
    cache_read: f64,
    cache_write: f64,
    output: f64,
    unpriced: bool,
}

impl SplitSum {
    /// The cache parts are the halves `EquivalentSums` priced for the row through the same `week`, so only the input
    /// and output parts are priced here.
    fn add(
        &mut self,
        usage: PinUsage,
        (read_part, write_part): CacheHalves,
        week: impl Fn(PinUsage) -> Option<f64>,
    ) {
        let parts = [
            week(PinUsage {
                input: usage.input,
                ..PinUsage::default()
            }),
            read_part,
            write_part,
            week(PinUsage {
                output: usage.output,
                ..PinUsage::default()
            }),
        ];
        match parts {
            [Some(input), Some(cache_read), Some(cache_write), Some(output)] => {
                self.input += input;
                self.cache_read += cache_read;
                self.cache_write += cache_write;
                self.output += output;
            }
            _ => self.unpriced = true,
        }
    }

    fn finish(self) -> Option<IndexMap<&'static str, f64>> {
        (!self.unpriced).then(|| {
            IndexMap::from([
                ("input", self.input),
                ("cache_read", self.cache_read),
                ("cache_write", self.cache_write),
                ("output", self.output),
            ])
        })
    }
}

/// A group's plan-funded side under every grouping: the counts the shares and the figure are summed over, the
/// Pro-week figure and its split by token kind, and the kept-warm credits the plan served.
#[derive(Default)]
struct PlanSums {
    requests: u64,
    output_tokens: u64,
    equivalent: EquivalentSums,
    split: SplitSum,
    kept_warm_requests: u64,
    kept_warm_5m_tokens: u64,
    kept_warm_1h_tokens: u64,
}

impl PlanSums {
    /// Folds one plan-funded row in: its counts and its split when `counted`, as the request dollars take the row, and
    /// its figure as `EquivalentSums` folds it from the one `price_row` answered on its counts, against the anchor row
    /// in force at the key's `now`.
    fn add(
        &mut self,
        row: &RollupRow,
        key: &RowKey<'_>,
        priced: Priced,
        costs: &PlanTables,
        counted: bool,
    ) {
        if counted {
            self.requests += row.requests;
            self.output_tokens += row.output_tokens;
        }
        // A classifier call's every part reads zero with no pricing call, so a model the cost model cannot price marks
        // nothing unpriced for a charge of nothing. A rollup row keeps no completion, so its hour is its band instant
        // too, and each part prices under the regime the row's `client` names.
        let price = |usage: PinUsage| {
            if key.classifier {
                Some(free_equivalent())
            } else {
                price_equivalent(
                    costs,
                    key.entrypoint,
                    key.model,
                    usage,
                    key.at,
                    key.at,
                    key.now,
                )
            }
        };
        let halves = self
            .equivalent
            .add_priced(row, counted, priced.equivalent, price);
        // The halves are priced for a counted row with tokens alone, the rows the split takes.
        if let Some(halves) = halves {
            self.split.add(row.usage(), halves, price);
        }
    }
}

/// The five counts summed, the guard every reader keeps before it prices a usage.
fn tokens_of(usage: PinUsage) -> u64 {
    usage.input + usage.cache_read + usage.write_5m + usage.write_1h + usage.output
}

/// The cache read part of `usage` and its write part over both lifetimes, the halves the Pro-week folds price apart.
fn cache_parts(usage: PinUsage) -> (PinUsage, PinUsage) {
    (
        PinUsage {
            cache_read: usage.cache_read,
            ..PinUsage::default()
        },
        PinUsage {
            write_5m: usage.write_5m,
            write_1h: usage.write_1h,
            ..PinUsage::default()
        },
    )
}

/// A group's dollars, each row priced at its own key as it joins: the counted rows' split, the billed rows' part of
/// it, the probe rows' total, the wasted counts' total and the kept-warm counts' saving.
#[derive(Default)]
struct GroupDollars {
    usd: CostSplit,
    billed: CostSplit,
    probe_usd: f64,
    wasted_usd: f64,
    saving_usd: f64,
}

/// One group's running sums. Rows join one at a time, so a paged read folds each page in as it arrives.
#[derive(Default)]
struct GroupSum {
    /// The request side, which a probe row joins only when probes count as requests.
    sum: RollupRow,
    /// The probe rows on their own, which the wasted probes read.
    probes: RollupRow,
    /// The probe rows' counts, folded per row, since the plan-served count reads each row's funding.
    probe_counts: ProbeCounts,
    dollars: GroupDollars,
    /// Whether a non-probe row ever joined: a keepalive credit carries a real kind but never adds to `requests`.
    saw_non_probe: bool,
    /// The shares, where the read's groups each hold one account's rows and a row of the group recorded a tier.
    shares: Option<ShareSums>,
    plan: PlanSums,
}

/// What one read folds every row under: the grouping, whether probes count as requests, whether the groups answer
/// their shares and the tables and instant every row prices at. Built once per read.
struct GroupRead<'a> {
    costs: &'a PlanTables,
    grouping: Grouping<'a>,
    now: i64,
    prices: &'a PriceRevisions,
    probes_counted: bool,
    /// Whether each group holds one account's rows, the one place a read answers the shares.
    shares: bool,
}

impl<'a> GroupRead<'a> {
    /// The read of a paged reader, whose `filter` decides its agent groups, its probe counting and its shares.
    fn paged(
        filter: &'a RollupFilter,
        live: &'a LiveKeys,
        by: GroupBy,
        prices: &'a PriceRevisions,
        costs: &'a PlanTables,
        now: i64,
    ) -> GroupRead<'a> {
        GroupRead {
            costs,
            grouping: Grouping {
                agent_groups: &filter.user_agent_groups,
                by,
                live,
            },
            now,
            prices,
            probes_counted: filter.probes_counted(),
            shares: holds_one_account(by, &filter.accounts),
        }
    }
}

#[derive(Default)]
struct GroupSums(BTreeMap<String, GroupSum>);

impl GroupSums {
    fn add(&mut self, read: &GroupRead<'_>, row: &RollupRow) {
        let GroupRead {
            costs,
            grouping,
            now,
            prices,
            probes_counted,
            shares,
        } = *read;
        let key = grouping.group_of(row);
        let entry = self.0.entry(key).or_default();
        let own_key = row.price_key(now);
        // Keyed as `priced` keys the row, since the group's shares come from `ShareSums::add` and a tier would price
        // share windows nothing reads.
        let price_key = RowKey {
            tier: None,
            ..own_key
        };
        let row_priced = price_row(prices, costs, &price_key, row.usage());
        let usd = row_priced.usd.clone().unwrap_or_default();
        let plan = own_key.funding == Funding::Plan;
        if row.kind == Kind::Probe.as_str() {
            entry.probes.add(row);
            entry.probe_counts.add_row(row.probe_view());
            entry.dollars.probe_usd += usd.total();
        } else {
            entry.saw_non_probe = true;
        }
        let counted = probes_counted || row.kind != Kind::Probe.as_str();
        if counted {
            entry.sum.add(row);
            entry.dollars.usd.add(&usd);
            if !plan {
                entry.dollars.billed.add(&usd);
            }
        }
        if tokens_of(row.wasted_usage()) > 0 {
            let wasted = price_usd(prices, &price_key, row.wasted_usage());
            entry.dollars.wasted_usd += wasted.map_or(0.0, |usd| usd.total());
        }
        if tokens_of(row.kept_warm_usage()) > 0 {
            let kept = price_usd(prices, &price_key, row.kept_warm_usage());
            entry.dollars.saving_usd +=
                kept.map_or(0.0, |usd| usd.cache_write_usd - usd.cache_read_usd);
        }
        if plan {
            // The summed rows `group_row` reads key no funding, so the plan-served counts fold here per row, the
            // credits under the gate `kept_warm` takes and ahead of the token test that skips a credit's row.
            if counted {
                entry.plan.kept_warm_requests += row.kept_warm_requests;
                entry.plan.kept_warm_5m_tokens += row.kept_warm_5m_tokens;
                entry.plan.kept_warm_1h_tokens += row.kept_warm_1h_tokens;
            }
            entry.plan.add(row, &own_key, row_priced, costs, counted);
        }
        // Each row prices under the account tier it recorded. A tier of `UNATTRIBUTED` (a profile not yet read, a row
        // folded before the tier was recorded, an api_key account's row) answers no share.
        if !shares {
            return;
        }
        let Some(tier) = own_key.tier else {
            return;
        };
        let sums = entry.shares.get_or_insert_with(ShareSums::default);
        if plan {
            sums.add(row, &own_key, tier, costs, counted);
        }
    }

    fn finish(mut self, read: &GroupRead<'_>) -> Vec<GroupRow> {
        let GroupRead {
            grouping,
            probes_counted,
            ..
        } = *read;
        // A group whose rows in the range are all probes never joins the request side under the default filter;
        // without this it would report zero requests rather than staying out of a breakdown that does not count
        // probes as requests. A group with any non-probe row, a keepalive credit included, stays.
        self.0
            .retain(|_, group| probes_counted || group.saw_non_probe);
        let mut out: Vec<GroupRow> = self
            .0
            .into_iter()
            .map(|(k, group)| grouping.row_of(&k, group))
            .collect();
        out.sort_by(|a, b| a.label.cmp(&b.label).then(a.key.cmp(&b.key)));
        out
    }
}

#[cfg(any(test, feature = "test-support"))]
fn sum_groups(rows: &[&RollupRow], read: &GroupRead<'_>) -> Vec<GroupRow> {
    let mut sums = GroupSums::default();
    for row in rows {
        sums.add(read, row);
    }
    sums.finish(read)
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::too_many_arguments)]
pub fn series(
    rows: &[RollupRow],
    from: i64,
    to: i64,
    bucket_seconds: i64,
    group_by: GroupBy,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<Vec<SeriesPoint>, LedgerError> {
    let read = GroupRead {
        costs,
        grouping: Grouping {
            agent_groups,
            by: group_by,
            live,
        },
        now,
        prices,
        probes_counted,
        // The rows come with no filter, so only the account grouping holds one account's rows per group.
        shares: holds_one_account(group_by, &[]),
    };
    let (first, last) = series_edges(from, to, bucket_seconds)?;
    Ok(bucket_starts(first, last, bucket_seconds)
        .map(|at| {
            let in_bucket: Vec<&RollupRow> = rows
                .iter()
                .filter(|r| r.hour >= at && r.hour < at + bucket_seconds)
                .collect();
            SeriesPoint {
                at: at * 1_000,
                groups: sum_groups(&in_bucket, &read),
            }
        })
        .collect())
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::too_many_arguments)]
pub fn breakdown(
    rows: &[RollupRow],
    group_by: GroupBy,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Vec<GroupRow> {
    let read = GroupRead {
        costs,
        grouping: Grouping {
            agent_groups,
            by: group_by,
            live,
        },
        now,
        prices,
        probes_counted,
        // The rows come with no filter, so only the account grouping holds one account's rows per group.
        shares: holds_one_account(group_by, &[]),
    };
    sum_groups(&rows.iter().collect::<Vec<_>>(), &read)
}

/// `series` over `[from, to)` (milliseconds) under `filter`, read through `read_rollups_paged` and folded into each
/// bucket's group sums as the pages arrive, so a long range holds one page of rows at a time. The groups' shares are
/// answered where each group holds one account's rows, and every row's shares, dollars and Pro-week figure price from
/// its counts on `prices` and `costs`, the figure against the anchor row in force at `now`, the read's instant.
#[allow(clippy::too_many_arguments)]
pub async fn series_paged(
    db: &Arc<LedgerDb>,
    filter: &RollupFilter,
    live: &LiveKeys,
    from: i64,
    to: i64,
    bucket_seconds: i64,
    group_by: GroupBy,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<Vec<SeriesPoint>, LedgerError> {
    let (first, last) = series_edges(from, to, bucket_seconds)?;
    let starts: Vec<i64> = bucket_starts(first, last, bucket_seconds).collect();
    let mut buckets: Vec<GroupSums> = starts.iter().map(|_| GroupSums::default()).collect();
    let read = GroupRead::paged(filter, live, group_by, prices, costs, now);
    if let Some(select) = filter.rollup_select(live) {
        read_rollups_paged(db, &select, hour_of(from), last, |row| {
            if !passes_funding(row, filter) {
                return;
            }
            if let Some(sums) = bucket_slot(&mut buckets, first, bucket_seconds, row.hour) {
                sums.add(&read, row);
            }
        })
        .await?;
    }
    Ok(starts
        .into_iter()
        .zip(buckets)
        .map(|(at, sums)| SeriesPoint {
            at: at * 1_000,
            groups: sums.finish(&read),
        })
        .collect())
}

/// `breakdown` over `[from, to)` (milliseconds) under `filter`, read through `read_rollups_paged` and folded into the
/// group sums as the pages arrive. The groups' shares are answered where each group holds one account's rows, and
/// every row's shares, dollars and Pro-week figure price from its counts on `prices` and `costs`, the figure against
/// the anchor row in force at `now`, the read's instant.
#[allow(clippy::too_many_arguments)]
pub async fn breakdown_paged(
    db: &Arc<LedgerDb>,
    filter: &RollupFilter,
    live: &LiveKeys,
    from: i64,
    to: i64,
    group_by: GroupBy,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<Vec<GroupRow>, LedgerError> {
    let mut sums = GroupSums::default();
    let read = GroupRead::paged(filter, live, group_by, prices, costs, now);
    if let Some(select) = filter.rollup_select(live) {
        read_rollups_paged(db, &select, hour_of(from), hour_ceil(to), |row| {
            if passes_funding(row, filter) {
                sums.add(&read, row);
            }
        })
        .await?;
    }
    Ok(sums.finish(&read))
}

#[derive(Clone, Debug, Serialize)]
pub struct HourPoint {
    pub at: i64,
    pub cost: f64,
    pub errors: u64,
    pub refused: u64,
    pub requests: u64,
    pub tokens: u64,
    /// The billed rows' dollars of the hour, a part of `cost`.
    pub billed: f64,
    /// The plan rows of the hour as the Pro-week figure, `None` where a plan row with tokens priced nothing.
    pub equivalent: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Dashboard {
    pub requests_24h: u64,
    pub tokens_24h: u64,
    pub tokens_cached_24h: u64,
    pub cost_24h: f64,
    /// The requests that did not succeed over the requests. The refused requests are in neither, since the fold never
    /// counted them.
    pub error_rate_24h: Option<f64>,
    /// The most frequent error code of the window.
    pub top_error_code: Option<String>,
    /// The requests the router refused before any account was asked over the window, not in `requests_24h`.
    pub refused_24h: u64,
    /// The most frequent code of the window's refusals, `None` with none.
    pub top_refusal_code: Option<String>,
    pub hourly: Vec<HourPoint>,
    /// The billed rows' dollars of the day, a part of `cost_24h`.
    pub billed_24h: f64,
    /// The plan rows of the day as the Pro-week figure, `None` where a plan row with tokens priced nothing.
    pub equivalent_24h: Option<f64>,
}

/// The dashboard's day on `conn`, the last day's rollups folded hour by hour, each row's dollars and Pro-week figure
/// priced from its counts, the figure against the anchor row in force at `now`. The day is one page of `PAGE_HOURS`,
/// so one read holds the connection no longer than a page of `read_rollups_paged` would, and a caller folds another
/// read into the same hold.
pub fn dashboard_on(
    conn: &Connection,
    now: i64,
    prices: &PriceRevisions,
    costs: &PlanTables,
) -> Result<Dashboard, LedgerError> {
    let to_hour = hour_of(now) + 3_600;
    let from_hour = to_hour - 24 * 3_600;
    let fold = TrafficFold::new(from_hour, to_hour, 3_600, prices, costs, now)?;
    let mut buckets = fold.zero.clone();
    let mut refused = vec![0_u64; buckets.len()];
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut refusals: BTreeMap<String, u64> = BTreeMap::new();
    let mut tokens_cached = 0;
    let select = RollupSelect {
        exclude_probes: true,
        ..RollupSelect::default()
    };
    visit_rollups(conn, from_hour, to_hour, &select, |r| {
        fold.add(&mut buckets, &r);
        if let Some(slot) = bucket_slot(&mut refused, from_hour, 3_600, r.hour) {
            *slot += r.refusals.values().sum::<u64>();
        }
        add_counts(&mut errors, &r.errors);
        add_counts(&mut refusals, &r.refusals);
        tokens_cached += r.cache_read_tokens;
    })?;
    let hourly: Vec<HourPoint> = buckets
        .into_iter()
        .zip(refused)
        .map(|(bucket, refused)| HourPoint {
            at: bucket.at,
            cost: bucket.cost_usd,
            errors: bucket.failed,
            refused,
            requests: bucket.requests,
            tokens: bucket.tokens,
            billed: bucket.billed_usd,
            equivalent: bucket.equivalent,
        })
        .collect();
    let requests: u64 = hourly.iter().map(|h| h.requests).sum();
    let failed: u64 = hourly.iter().map(|h| h.errors).sum();
    Ok(Dashboard {
        requests_24h: requests,
        tokens_24h: hourly.iter().map(|h| h.tokens).sum(),
        tokens_cached_24h: tokens_cached,
        cost_24h: hourly.iter().map(|h| h.cost).sum(),
        error_rate_24h: (requests > 0).then(|| failed as f64 / requests as f64),
        top_error_code: errors
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(c, _)| c.clone()),
        refused_24h: refusals.values().sum(),
        top_refusal_code: refusals
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(c, _)| c.clone()),
        billed_24h: hourly.iter().map(|h| h.billed).sum(),
        equivalent_24h: hourly
            .iter()
            .try_fold(0.0, |day, h| Some(day + h.equivalent?)),
        hourly,
    })
}

/// The accounts of `quota_samples`, by a skip over an index that leads with the account: one seek per account where
/// `DISTINCT` would walk every stored sample. The recursion's last row is the `NULL` that ends it, which every reader
/// filters out.
macro_rules! quota_accounts_cte {
    () => {
        "WITH RECURSIVE \
           accounts(account_id) AS ( \
             SELECT MIN(account_id) FROM quota_samples \
             UNION ALL \
             SELECT (SELECT MIN(q.account_id) FROM quota_samples q WHERE q.account_id > accounts.account_id) \
               FROM accounts WHERE accounts.account_id IS NOT NULL)"
    };
}
pub(crate) use quota_accounts_cte;

/// `quota_accounts_cte!` and each account's windows, by a skip over the account index's second column: one seek per
/// window rather than a walk of the table, since no index leads with `bucket`. The windows' recursion ends on a `NULL`
/// too, which every reader filters out.
macro_rules! quota_account_windows_cte {
    () => {
        concat!(
            quota_accounts_cte!(),
            ", \
           windows(account_id, window) AS ( \
             SELECT account_id, (SELECT MIN(q.window) FROM quota_samples q WHERE q.account_id = accounts.account_id) \
               FROM accounts WHERE accounts.account_id IS NOT NULL \
             UNION ALL \
             SELECT account_id, (SELECT MIN(q.window) FROM quota_samples q \
                                  WHERE q.account_id = windows.account_id AND q.window > windows.window) \
               FROM windows WHERE windows.window IS NOT NULL)"
        )
    };
}

/// The newest sample of the last day per account and window. Binds `?1`, the day's first bucket. The seek names the
/// account index, since the primary key leads with the account too but ranges over the bucket before the window.
pub(crate) const LATEST_QUOTA_SQL: &str = concat!(
    quota_account_windows_cte!(),
    " SELECT q.account_id, q.bucket, q.window, q.utilization, q.reset_at, q.source, q.observed_at \
        FROM windows w \
        JOIN quota_samples q ON q.rowid = ( \
          SELECT s.rowid FROM quota_samples s INDEXED BY quota_samples_account \
           WHERE s.account_id = w.account_id AND s.window = w.window AND s.bucket >= ?1 \
           ORDER BY s.bucket DESC LIMIT 1) \
       WHERE w.window IS NOT NULL \
       ORDER BY q.account_id, q.window"
);

pub fn latest_quota(conn: &Connection, now: i64) -> Result<Vec<QuotaSampleRecord>, LedgerError> {
    let since = now / 1_000 - 86_400;
    let mut stmt = conn.prepare(LATEST_QUOTA_SQL)?;
    let rows = stmt.query_map(params![since], row_to_quota_sample)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The last quota sample of one account's window inside one hour, as `GET /admin/analytics/accounts` reads it.
/// `observed_at` is the sample's own instant (`quota_samples.observed_at`, Unix milliseconds), which the lead and the
/// wire's `sampled_at` read, since `bucket` is the 300-second floor the sampler files the row under.
#[derive(Clone, Debug, PartialEq)]
pub struct HourlyWindowSample {
    pub account_id: String,
    /// The hour the bucket falls in, in seconds, floored as `hour_of` floors an instant.
    pub hour: i64,
    pub window: String,
    pub bucket: i64,
    pub observed_at: i64,
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
}

/// The hour of `q.bucket` in seconds. SQLite's `/` truncates toward zero, so the hour is floored through a modulus kept
/// positive, the hour `hour_of` files a bucket before the epoch under.
macro_rules! hour_floor {
    () => {
        "(q.bucket - (((q.bucket % 3600) + 3600) % 3600))"
    };
}

/// Binds `?1` and `?2`, the range, and `?3` and `?4`, the hour floor of `?1` and the hour ceiling of `?2`. The
/// ranking reads the whole hours, so a sample of the range is the last of its hour against the hour's samples outside
/// the range too.
pub(crate) const HOURLY_WINDOW_SAMPLES_SQL: &str = concat!(
    quota_account_windows_cte!(),
    ", ranked AS ( \
         SELECT q.account_id, q.window, q.bucket, q.observed_at, q.utilization, q.reset_at, ",
    hour_floor!(),
    " AS hour, ROW_NUMBER() OVER (PARTITION BY q.account_id, q.window, ",
    hour_floor!(),
    " ORDER BY q.bucket DESC) AS nth \
           FROM windows w JOIN quota_samples q INDEXED BY quota_samples_account \
             ON q.account_id = w.account_id AND q.window = w.window \
          WHERE w.window IS NOT NULL AND q.bucket >= ?3 AND q.bucket < ?4) \
     SELECT account_id, hour, window, bucket, observed_at, utilization, reset_at \
       FROM ranked WHERE nth = 1 AND bucket >= ?1 AND bucket < ?2 \
      ORDER BY account_id, hour, window"
);

/// Every account's last sample per window and hour over `[from, to)` in Unix seconds, ordered by account, hour,
/// window.
pub fn hourly_window_samples(
    conn: &Connection,
    from: i64,
    to: i64,
) -> Result<Vec<HourlyWindowSample>, LedgerError> {
    let (first_hour, past_hour) = (hour_of(from * 1_000), hour_ceil(to * 1_000));
    let mut stmt = conn.prepare(HOURLY_WINDOW_SAMPLES_SQL)?;
    let rows = stmt.query_map(params![from, to, first_hour, past_hour], |r| {
        Ok(HourlyWindowSample {
            account_id: r.get(0)?,
            hour: r.get(1)?,
            window: r.get(2)?,
            bucket: r.get(3)?,
            observed_at: r.get(4)?,
            utilization: r.get(5)?,
            reset_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The accounts of `extra_usage_samples`, by the same skip over its primary key, which leads with the account. The
/// recursion's last row is the `NULL` that ends it, which every reader filters out.
macro_rules! extra_usage_accounts_cte {
    () => {
        "WITH RECURSIVE \
           accounts(account_id) AS ( \
             SELECT MIN(account_id) FROM extra_usage_samples \
             UNION ALL \
             SELECT (SELECT MIN(e.account_id) FROM extra_usage_samples e WHERE e.account_id > accounts.account_id) \
               FROM accounts WHERE accounts.account_id IS NOT NULL)"
    };
}

/// The newest row of the last day per account, `$filter` narrowing the rows the newest is taken from and `$rank`
/// breaking a tie inside the newest bucket. Binds `?1`, the day's first bucket.
macro_rules! latest_extra_usage_sql {
    ($filter:literal, $rank:literal) => {
        concat!(
            extra_usage_accounts_cte!(),
            " SELECT e.account_id, e.bucket, e.is_enabled, e.in_use, e.monthly_limit, e.used_credits, e.utilization, \
                 e.currency, e.disabled_reason, e.overage_reset, e.source, e.spend_limit_minor, e.spend_used_minor, \
                 e.spend_percent, e.spend_currency, e.spend_exponent \
            FROM accounts a \
            JOIN extra_usage_samples e ON e.rowid = ( \
              SELECT s.rowid FROM extra_usage_samples s \
               WHERE s.account_id = a.account_id AND s.bucket >= ?1",
            $filter,
            " ORDER BY s.bucket DESC",
            $rank,
            " LIMIT 1) \
           WHERE a.account_id IS NOT NULL \
           ORDER BY e.account_id"
        )
    };
}

/// Binds `?1`, the day's first bucket.
pub(crate) const LATEST_EXTRA_USAGE_SQL: &str =
    latest_extra_usage_sql!("", ", s.source = 'head' DESC");

/// The rows of one `latest_extra_usage_sql!` statement over the day before `now`.
fn latest_extra_usage_of(
    conn: &Connection,
    sql: &str,
    now: i64,
) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
    let since = now / 1_000 - 86_400;
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![since], row_to_extra_usage_sample)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The newest row of the last day per account, one row each. A head and a poll may share the newest bucket, and the
/// head row answers for it, as the insert ranks a head the newest truth of its bucket.
pub fn latest_extra_usage(
    conn: &Connection,
    now: i64,
) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
    latest_extra_usage_of(conn, LATEST_EXTRA_USAGE_SQL, now)
}

/// Binds `?1`, the day's first bucket.
pub(crate) const LATEST_EXTRA_USAGE_POLLED_SQL: &str =
    latest_extra_usage_sql!(" AND s.source = 'poll'", "");

/// The newest extra-usage row of the last day per account whose source is `poll`: the row that carries the amounts,
/// which a later head row never hides.
pub fn latest_extra_usage_polled(
    conn: &Connection,
    now: i64,
) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
    latest_extra_usage_of(conn, LATEST_EXTRA_USAGE_POLLED_SQL, now)
}

/// Binds `?1`, the day's first bucket.
pub(crate) const LATEST_EXTRA_USAGE_SHARE_SQL: &str =
    latest_extra_usage_sql!(" AND s.utilization IS NOT NULL", ", s.source = 'head' DESC");

/// The newest extra-usage row of the last day per account that carries a share, ranked as `latest_extra_usage` ranks
/// a bucket's rows: a head that names the overage reset alone writes a row with none, which hides no earlier share.
pub fn latest_extra_usage_share(
    conn: &Connection,
    now: i64,
) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
    latest_extra_usage_of(conn, LATEST_EXTRA_USAGE_SHARE_SQL, now)
}

/// One span of hours over which both lists' revisions are constant: the SQL bounds (seconds, half-open) and the
/// instant the span prices at in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub from_hour: i64,
    pub to_hour: i64,
    /// The span's start, raised to the earliest instant of either list when the start lies below it (a lifetime
    /// read passes `i64::MIN` as `from_hour`), so the sentinel is not multiplied and `in_force(at)` is the
    /// revision every hour of the span reads.
    pub at: i64,
}

/// The spans between `from_hour` and `to_hour` split at every revision boundary of `prices` and, when given, of
/// `costs`, so a SQL sum runs once per span and is priced at that span's revisions, and one span when no revision
/// starts inside the range. `costs` is `None` for a dollar sum. The first span's `from_hour` is the caller's (so the
/// rows before the first revision's instant, which `in_force` reads at the first entry, stay in the sum) and its `at`
/// is clamped as `Span` says.
pub fn revision_spans(
    prices: &PriceRevisions,
    costs: Option<&CostRevisions>,
    from_hour: i64,
    to_hour: i64,
) -> Vec<Span> {
    let price_instants = prices.instants();
    let cost_instants = costs.map_or(&[][..], |costs| costs.instants());
    // Before a list's first entry `in_force` already reads that entry, so only the later entries move a revision.
    let mut edges: Vec<i64> = [price_instants, cost_instants]
        .into_iter()
        .flat_map(|instants| instants.iter().skip(1))
        .map(|ms| ms.div_euclid(1_000))
        .filter(|hour| *hour > from_hour && *hour < to_hour)
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let earliest = price_instants
        .iter()
        .chain(cost_instants)
        .min()
        .copied()
        .unwrap_or(i64::MIN);
    let starts = std::iter::once(from_hour).chain(edges.iter().copied());
    let ends = edges.iter().copied().chain(std::iter::once(to_hour));
    starts
        .zip(ends)
        .map(|(from, to)| Span {
            from_hour: from,
            to_hour: to,
            at: from
                .checked_mul(1_000)
                .map_or(earliest, |ms| ms.max(earliest)),
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Totals {
    pub cost_usd: f64,
    pub requests: u64,
    pub tokens: u64,
    /// The billed rows' dollars, a part of `cost_usd`.
    pub billed_usd: f64,
    /// The plan rows as the Pro-week figure, `None` where a plan row with tokens priced nothing.
    pub equivalent: Option<f64>,
}

/// No row, so the figure is the sum of no plan row: zero, which `add` keeps until a row that priced nothing joins.
impl Default for Totals {
    fn default() -> Self {
        Totals {
            cost_usd: 0.0,
            requests: 0,
            tokens: 0,
            billed_usd: 0.0,
            equivalent: Some(0.0),
        }
    }
}

/// The price columns and the hour a SQL sum groups by beside its own column, and the sums it answers in the order
/// `price_group` reads them, spelled once for both totals. The client is a price column, since its entrypoint names
/// the regime a group's equivalent prices under, and so is the classifier, whose call prices at no share.
const PRICE_GROUP_COLUMNS: &str = "canonical_model, speed, band, funding, client, classifier, hour";
const PRICE_GROUP_SUMS: &str = "SUM(requests), SUM(input_tokens), SUM(cache_read_tokens), \
     SUM(cache_write_5m_tokens), SUM(cache_write_1h_tokens), SUM(output_tokens)";

/// A summed count read from column `i`, clamped at zero.
fn clamped_count(r: &Row<'_>, i: usize) -> rusqlite::Result<u64> {
    Ok(r.get::<_, i64>(i)?.max(0) as u64)
}

/// The five summed token counts read from column `at` on, input, cache read, both writes and output, each clamped at
/// zero.
fn summed_usage(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<PinUsage> {
    Ok(PinUsage {
        input: clamped_count(r, at)?,
        cache_read: clamped_count(r, at + 1)?,
        write_5m: clamped_count(r, at + 2)?,
        write_1h: clamped_count(r, at + 3)?,
        output: clamped_count(r, at + 4)?,
    })
}

/// One group of a priced SQL sum read from `offset`: its hour, and its requests, its five counts and its dollars,
/// priced through `price_row` at the group's own hour as an inference row (the sums keep their `kind != 'probe'`
/// clause), so the revision is the span's and the peak factor the hour's, the Pro-week figure against the anchor row at
/// `now`.
fn price_group(
    r: &rusqlite::Row<'_>,
    offset: usize,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> rusqlite::Result<(i64, Totals)> {
    // `UNATTRIBUTED` reads as no tier: the sums group by no tier, so one group can hold several.
    let group = RollupRow {
        canonical_model: r.get(offset)?,
        speed: r.get(offset + 1)?,
        band: r.get(offset + 2)?,
        funding: r.get(offset + 3)?,
        client: r.get(offset + 4)?,
        classifier: r.get(offset + 5)?,
        hour: r.get(offset + 6)?,
        kind: Kind::Inference.as_str().into(),
        tier: UNATTRIBUTED.into(),
        ..RollupRow::default()
    };
    let usage = summed_usage(r, offset + 8)?;
    let key = group.price_key(now);
    let (cost_usd, billed_usd, equivalent) =
        shared_of(&price_row(prices, costs, &key, usage), key.funding, usage);
    Ok((
        group.hour,
        Totals {
            cost_usd,
            requests: clamped_count(r, offset + 7)?,
            tokens: tokens_of(usage),
            billed_usd,
            equivalent,
        },
    ))
}

impl Totals {
    pub fn add(&mut self, o: &Totals) {
        self.cost_usd += o.cost_usd;
        self.requests += o.requests;
        self.tokens += o.tokens;
        self.billed_usd += o.billed_usd;
        self.equivalent = add_figures(self.equivalent, o.equivalent);
    }
}

/// `equivalent` reads null when a part is unpriced, as `Totals::add` keeps it.
impl std::ops::AddAssign<&Totals> for Totals {
    fn add_assign(&mut self, other: &Totals) {
        Totals::add(self, other);
    }
}

/// The column `request_totals` groups by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalsBy {
    Account,
    Pool,
}

impl TotalsBy {
    pub fn column(self) -> &'static str {
        match self {
            TotalsBy::Account => "account_id",
            TotalsBy::Pool => "pool_id",
        }
    }
}

/// Request, token, and cost totals per pool or account over `[since_hour, to_hour)`, probe rows left out. One
/// aggregate query per span of `revision_spans` rather than a rollup read. Grouped by the column, the price columns
/// and the hour, each group priced in Rust at its own hour. A lifetime caller passes `i64::MIN` as `since_hour`,
/// which the first span's `at` clamps. `now` is the read's instant, whose anchor row divides the Pro-week figure.
pub fn request_totals(
    conn: &Connection,
    by: TotalsBy,
    since_hour: i64,
    to_hour: i64,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<BTreeMap<String, Totals>, LedgerError> {
    grouped_totals(
        conn,
        &[by.column()],
        (since_hour, to_hour),
        prices,
        costs,
        now,
        |r| r.get(0).map(Some),
    )
}

/// Request, token, and cost totals per live key over `[since_hour, to_hour)`, probe rows left out: the answer of
/// `key_request_totals_by_generation` with only the live generation of each id kept, so a key's row never carries an
/// earlier holder's. `now` is the read's instant, whose anchor row divides the Pro-week figure.
pub fn key_request_totals(
    conn: &Connection,
    since_hour: i64,
    to_hour: i64,
    live: &LiveKeys,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<BTreeMap<String, Totals>, LedgerError> {
    grouped_totals(
        conn,
        &["key_id", "key_generation"],
        (since_hour, to_hour),
        prices,
        costs,
        now,
        |r| {
            let (id, generation) = (r.get::<_, String>(0)?, r.get::<_, i64>(1)?);
            Ok((live.0.get(&id) == Some(&generation)).then_some(id))
        },
    )
}

/// Request, token, and cost totals per key and generation over `[since_hour, to_hour)`, probe rows left out: the pair
/// is grouped in SQL beside the price columns and the hour, one query per span of `revision_spans`. Every generation
/// is answered, so a caller that keeps the answer across a key's creation or deletion filters the live generations
/// itself, and a key created again under an id reads none of its earlier holder's rows. `now` is the read's instant,
/// whose anchor row divides the Pro-week figure.
pub fn key_request_totals_by_generation(
    conn: &Connection,
    since_hour: i64,
    to_hour: i64,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<BTreeMap<String, BTreeMap<i64, Totals>>, LedgerError> {
    let pairs = grouped_totals(
        conn,
        &["key_id", "key_generation"],
        (since_hour, to_hour),
        prices,
        costs,
        now,
        |r| Ok(Some((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))),
    )?;
    let mut out: BTreeMap<String, BTreeMap<i64, Totals>> = BTreeMap::new();
    for ((id, generation), totals) in pairs {
        out.entry(id).or_default().insert(generation, totals);
    }
    Ok(out)
}

/// The priced SQL sum of both totals reads over the hour range: grouped by `group_columns`, the price columns and the
/// hour, one query per span of `revision_spans`, each group's price columns read after the group columns and its key by
/// `key_of`, a group whose key is `None` left out unpriced. The statement orders the groups as it groups them, so a key
/// adds its groups' figures in one order whichever plan SQLite picks.
fn grouped_totals<K: Ord>(
    conn: &Connection,
    group_columns: &[&str],
    (since_hour, to_hour): (i64, i64),
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
    key_of: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Option<K>>,
) -> Result<BTreeMap<K, Totals>, LedgerError> {
    let offset = group_columns.len();
    let group_columns = group_columns.join(", ");
    let mut stmt = conn.prepare(&format!("SELECT {group_columns}, {PRICE_GROUP_COLUMNS}, {PRICE_GROUP_SUMS} FROM rollups_hourly WHERE kind != 'probe' AND hour >= ?1 AND hour < ?2 GROUP BY {group_columns}, {PRICE_GROUP_COLUMNS} ORDER BY {group_columns}, {PRICE_GROUP_COLUMNS}"))?;
    let mut out: BTreeMap<K, Totals> = BTreeMap::new();
    for span in revision_spans(prices, Some(&costs.revisions), since_hour, to_hour) {
        let rows = stmt.query_map(params![span.from_hour, span.to_hour], |r| {
            let Some(key) = key_of(r)? else {
                return Ok(None);
            };
            let (_, totals) = price_group(r, offset, prices, costs, now)?;
            Ok(Some((key, totals)))
        })?;
        for row in rows {
            if let Some((key, totals)) = row? {
                out.entry(key).or_default().add(&totals);
            }
        }
    }
    Ok(out)
}

/// The requests per account over `[since_hour, to_hour)`, probe rows left out: the `requests` that `request_totals`
/// answers per account, summed in one statement with no pricing for a caller that reads the counts alone. A lifetime
/// caller passes `i64::MIN` as `since_hour`.
pub fn account_request_counts(
    conn: &Connection,
    since_hour: i64,
    to_hour: i64,
) -> Result<BTreeMap<String, u64>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT account_id, SUM(requests) FROM rollups_hourly \
         WHERE kind != 'probe' AND hour >= ?1 AND hour < ?2 GROUP BY account_id",
    )?;
    let rows = stmt.query_map(params![since_hour, to_hour], |r| {
        Ok((r.get::<_, String>(0)?, clamped_count(r, 1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionActivity {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub first_request_at: i64,
    pub last_request_at: i64,
    pub requests: u64,
    pub model: String,
    pub account_id: Option<String>,
    /// The newest request of the session presented the retired secret, and under which rotation.
    pub on_retired_secret: bool,
    pub rotation_at: Option<i64>,
}

/// Every (key, key generation, session) with a client request since `since`, with the canonical model and the
/// serving account of its newest request. Grouped by generation as well as id, so a key id created again never
/// merges its sessions with an earlier holder's. One pass over the window's client requests: the newest request of
/// a session is inside the window whenever any of its requests is, so the window alone answers both the counts and
/// the newest request's facts. The count leaves the refused requests out, while their rows stay in the partition,
/// so a session whose every request was refused keeps its newest request and is listed.
pub fn session_activity(
    conn: &Connection,
    since: i64,
) -> Result<Vec<SessionActivity>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "WITH recent AS (SELECT id, key_id, key_generation, session_id AS grp, received_at, canonical_model, \
         presented_retired, rotation_at, refused FROM requests \
         WHERE key_id IS NOT NULL AND session_id IS NOT NULL AND kind != 'probe' AND received_at >= ?1), \
         ranked AS (SELECT recent.*, \
         ROW_NUMBER() OVER (PARTITION BY key_id, key_generation, grp ORDER BY received_at DESC, id DESC) AS nth, \
         MIN(received_at) OVER (PARTITION BY key_id, key_generation, grp) AS first_at, \
         SUM(CASE WHEN refused = 0 THEN 1 ELSE 0 END) OVER (PARTITION BY key_id, key_generation, grp) AS n \
         FROM recent) \
         SELECT key_id, key_generation, grp, first_at, received_at, n, canonical_model, \
         {}, \
         presented_retired, rotation_at \
         FROM ranked WHERE nth = 1 ORDER BY received_at DESC",
        serving_attempt("a.account_id", "ranked.id")
    ))?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(SessionActivity {
            key_id: r.get(0)?,
            key_generation: r.get(1)?,
            session_id: r.get(2)?,
            first_request_at: r.get(3)?,
            last_request_at: r.get(4)?,
            requests: r.get::<_, i64>(5)? as u64,
            model: r.get(6)?,
            account_id: r.get(7)?,
            on_retired_secret: r.get(8)?,
            rotation_at: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The model one agent of a session named on its newest conversation request, as the client spelled it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModel {
    pub key_id: String,
    pub key_generation: i64,
    /// The client's own session, as `session_activity` keys it.
    pub session_id: String,
    /// `None` on a request that carried no agent.
    pub agent_id: Option<String>,
    pub received_at: i64,
    /// The `model` column, which keeps the `[1m]` suffix of a long-context twin that `canonical_model` folds out.
    pub model: String,
}

/// The newest conversation request's own model spelling per (key, key generation, session, agent) since `since`,
/// over the window and the requests `session_activity` reads, newest first, so a session's first row is its newest
/// conversation request. A conversation request is every request whose class is not `auxiliary`, a raw caller's
/// request with no class among them, since an auxiliary request names a model of its own.
pub fn agent_models(conn: &Connection, since: i64) -> Result<Vec<AgentModel>, LedgerError> {
    let mut stmt = conn.prepare(
        "WITH recent AS (SELECT id, key_id, key_generation, session_id AS grp, agent_id, received_at, model \
         FROM requests \
         WHERE key_id IS NOT NULL AND session_id IS NOT NULL AND kind != 'probe' AND received_at >= ?1 \
         AND (request_class IS NULL OR request_class != 'auxiliary')), \
         ranked AS (SELECT recent.*, \
         ROW_NUMBER() OVER (PARTITION BY key_id, key_generation, grp, agent_id ORDER BY received_at DESC, id DESC) \
         AS nth FROM recent) \
         SELECT key_id, key_generation, grp, agent_id, received_at, model FROM ranked WHERE nth = 1 \
         ORDER BY received_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(AgentModel {
            key_id: r.get(0)?,
            key_generation: r.get(1)?,
            session_id: r.get(2)?,
            agent_id: r.get(3)?,
            received_at: r.get(4)?,
            model: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A session's fork facts, read off its first client request. A session the ledger holds no client request for is
/// absent from the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub forked_from: Option<String>,
    pub fork_depth: Option<u32>,
}

/// At three bound parameters per named session, 900 sessions bind 2,700 parameters, a wide margin under the
/// bundled SQLite's 32,766-parameter default cap: a call over more sessions than this chunks and merges rather
/// than tripping the cap.
const SESSION_IDENTITY_CHUNK: usize = 900;

/// For each (key id, key generation, session id) named, with no time bound: a pin-only session (one whose
/// requests, if any, are all older than any activity window a caller applies elsewhere) is answered the same
/// as a fresh one, since this query reads no `since` bound of its own.
pub fn session_identities(
    conn: &Connection,
    sessions: &[(String, i64, String)],
) -> Result<Vec<SessionIdentity>, LedgerError> {
    let mut out = Vec::with_capacity(sessions.len());
    for chunk in sessions.chunks(SESSION_IDENTITY_CHUNK) {
        out.extend(session_identities_chunk(conn, chunk)?);
    }
    Ok(out)
}

/// `session_identities_chunk`'s statement and its binds, spelled once so the plan test explains the statement the
/// read runs. Each named session costs two seeks of the session index, the `EXISTS` and the marking request's lookup,
/// each stopping early, and the two fork facts are read off the marking row by primary key. The index is named: the
/// lookup binds a key and a session, two indexes lead with one of them each, and the planner weighs them by
/// statistics the file may lack or that a key holding most rows can mislead.
pub(crate) fn session_identities_statement(
    sessions: &[(String, i64, String)],
) -> (String, Vec<rusqlite::types::Value>) {
    let mut binds = Vec::with_capacity(sessions.len() * 3);
    let values: Vec<String> = sessions
        .iter()
        .map(|(key_id, key_generation, session_id)| {
            format!(
                "({})",
                bind_all(
                    &[
                        rusqlite::types::Value::Text(key_id.clone()),
                        rusqlite::types::Value::Integer(*key_generation),
                        rusqlite::types::Value::Text(session_id.clone()),
                    ],
                    &mut binds,
                )
            )
        })
        .collect();
    // The client's quota probe is an `inference` row ahead of the session's first main request and names no
    // fork, so the marker is read off the first refreshing or unclassed request, as the router records it.
    let refreshing = sql_list(REFRESHING_CLASSES);
    let sql = format!(
        "WITH named(key_id, key_generation, session_id) AS (VALUES {}), \
         marked AS ( \
           SELECT n.key_id, n.key_generation, n.session_id, \
                  (SELECT r.id FROM requests r INDEXED BY requests_session \
                    WHERE r.session_id = n.session_id AND r.key_id = n.key_id AND r.key_generation = n.key_generation \
                      AND r.kind != 'probe' AND (r.request_class IS NULL OR r.request_class IN ({refreshing})) \
                    ORDER BY r.received_at ASC, r.id ASC LIMIT 1) AS marking_id \
             FROM named n \
            WHERE EXISTS (SELECT 1 FROM requests r INDEXED BY requests_session \
                           WHERE r.session_id = n.session_id AND r.key_id = n.key_id \
                             AND r.key_generation = n.key_generation AND r.kind != 'probe')) \
         SELECT m.key_id, m.key_generation, m.session_id, r.forked_from, r.fork_depth \
           FROM marked m LEFT JOIN requests r ON r.id = m.marking_id",
        values.join(", ")
    );
    (sql, binds)
}

fn session_identities_chunk(
    conn: &Connection,
    sessions: &[(String, i64, String)],
) -> Result<Vec<SessionIdentity>, LedgerError> {
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let (sql, binds) = session_identities_statement(sessions);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(binds), |r| {
        Ok(SessionIdentity {
            key_id: r.get(0)?,
            key_generation: r.get(1)?,
            session_id: r.get(2)?,
            forked_from: r.get(3)?,
            fork_depth: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The UTC bounds of the budget period containing `now`, in milliseconds: a day is its own midnight to the
/// next, a month its first to the next month's first.
pub(crate) fn period_bounds(period: BudgetPeriod, now: i64) -> (i64, i64) {
    let midnight = |date: jiff::civil::Date| {
        date.to_zoned(jiff::tz::TimeZone::UTC)
            .expect("every civil midnight exists in UTC")
            .timestamp()
            .as_millisecond()
    };
    let date = jiff::Timestamp::from_millisecond(now)
        .expect("a routed clock value stays in jiff's timestamp range")
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    match period {
        BudgetPeriod::Day => (
            midnight(date),
            midnight(
                date.checked_add(jiff::Span::new().days(1))
                    .expect("a day past any date stays in jiff's date range"),
            ),
        ),
        BudgetPeriod::Month => (
            midnight(date.first_of_month()),
            phoebis_relay::next_month_start(now),
        ),
    }
}

/// One budgeted period of one member, as the spend report and the retention take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetedMember {
    pub pool_id: String,
    pub account_id: String,
    pub period: BudgetPeriod,
    /// Sum the metered rows alone, those filed under `overage` or `fast` and a usage-based seat's under
    /// `usage_based`: a subscription member's plan-funded traffic is no spend.
    pub metered_only: bool,
}

/// One budgeted period's spend of one member in its current period, summed from the hourly rollups and priced at
/// read time, a keepalive probe's cost included since it draws real money from an API-key member's budget the same
/// as any other request's. With `metered_only`, a subscription member's metered rows alone count, those filed under
/// `overage` or `fast`, and a usage-based seat's under `usage_based`. The row names the period kind and its UTC bounds
/// in ms (`period_end` is the next day or month boundary). Grouped by `canonical_model`, `speed` and `band`, one query
/// per span of `revision_spans(prices, None, ..)`, each group priced through `estimate_banded` at its band on
/// `prices.in_force(span.at)`, so a band row prices exactly at the cells its attempts selected and a budget moves with
/// the revision in force. A model the revision lacks adds nothing.
pub fn member_spends(
    conn: &Connection,
    members: &[BudgetedMember],
    now: i64,
    prices: &PriceRevisions,
) -> Result<Vec<MemberSpend>, LedgerError> {
    let mut out = Vec::with_capacity(members.len());
    // Prepared once outside the per-member loop: a monthly member's period scans up to a month of rows, and
    // re-parsing the same statement text for every member in a pool would add nothing but overhead.
    let mut stmt = conn.prepare(
        "SELECT canonical_model, speed, band, SUM(input_tokens), SUM(cache_read_tokens), \
         SUM(cache_write_5m_tokens), SUM(cache_write_1h_tokens), SUM(output_tokens) FROM rollups_hourly \
         WHERE pool_id = ?1 AND account_id = ?2 AND hour >= ?3 AND hour < ?4 \
         AND (?5 = 0 OR funding IN ('overage', 'fast', 'usage_based')) \
         GROUP BY canonical_model, speed, band",
    )?;
    for BudgetedMember {
        pool_id,
        account_id,
        period,
        metered_only,
    } in members
    {
        let (period_start, period_end) = period_bounds(*period, now);
        let mut spent_usd = 0.0;
        for span in revision_spans(prices, None, period_start / 1_000, period_end / 1_000) {
            let table = prices.in_force(span.at);
            let rows = stmt.query_map(
                params![
                    pool_id,
                    account_id,
                    span.from_hour,
                    span.to_hour,
                    metered_only
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        summed_usage(r, 3)?,
                    ))
                },
            )?;
            for row in rows {
                let (model, speed, band, usage) = row?;
                // A model the revision cannot price contributes nothing: the analytics already read such a row as
                // unpriced, and a budget must not be tripped by a sum it cannot itself account for.
                if let Some(split) = table.estimate_banded(
                    &model,
                    Speed::of(Some(&speed)),
                    Band::of_rollup(&band),
                    usage,
                ) {
                    spent_usd += split.total();
                }
            }
        }
        out.push(MemberSpend {
            pool_id: pool_id.clone(),
            account_id: account_id.clone(),
            period: *period,
            period_start,
            period_end,
            spent_usd,
            observed_at: now,
        });
    }
    Ok(out)
}

/// One pool's (or the pool-less records') parked rows: the accounts named, the count and the oldest `parked_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedSummary {
    pub pool_id: Option<String>,
    pub account_ids: Vec<String>,
    pub count: u64,
    pub oldest_at: i64,
}

/// Every pool's parked rows, the pool-less records first and the pools in id order, each pool's accounts in id order.
/// Parked rows are rare, so the table is read whole rather than aggregated in SQL.
pub fn parked_summary(conn: &Connection) -> Result<Vec<ParkedSummary>, LedgerError> {
    let mut stmt = conn.prepare("SELECT pool_id, account_id, parked_at FROM parked_records")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut pools: BTreeMap<Option<String>, (BTreeSet<String>, u64, i64)> = BTreeMap::new();
    for row in rows {
        let (pool_id, account_id, parked_at) = row?;
        let (accounts, count, oldest_at) = pools
            .entry(pool_id)
            .or_insert_with(|| (BTreeSet::new(), 0, parked_at));
        accounts.extend(account_id);
        *count += 1;
        *oldest_at = (*oldest_at).min(parked_at);
    }
    Ok(pools
        .into_iter()
        .map(|(pool_id, (accounts, count, oldest_at))| ParkedSummary {
            pool_id,
            account_ids: accounts.into_iter().collect(),
            count,
            oldest_at,
        })
        .collect())
}

pub fn parked_count(conn: &Connection) -> Result<u64, LedgerError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM parked_records", [], |r| r.get(0))?;
    Ok(count as u64)
}

/// Placements by rung over `[since, until)`: `rung`, `placements`, `hits`, `cache_read_tokens`, `promised_tokens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlacementRow {
    pub rung: String,
    pub placements: u64,
    pub hits: u64,
    pub cache_read_tokens: u64,
    pub promised_tokens: u64,
}

/// A `content` placement is a hit when the response's cache-read tokens reach the entry's promised
/// `prefix_tokens`; a `static` or `with_main` one is a hit when the response read anything, since the static
/// tier's own size is not on the usage and a read is the only fact the upstream gives. Counts the serving
/// attempt of a placed leg alone: a retry in place, a strip retry, or a 401 replay publishes the same trace on
/// every earlier pass, and `finish` writes usage onto the serving attempt alone, so a pass with none is not a
/// placement of its own, let alone a miss.
pub fn placements(
    conn: &Connection,
    filter: &RollupFilter,
    live: &LiveKeys,
    since: i64,
    until: i64,
) -> Result<Vec<PlacementRow>, LedgerError> {
    let mut args: Vec<rusqlite::types::Value> = vec![since.into(), until.into()];
    let mut clauses: Vec<String> = Vec::new();
    key_lifetimes(&filter.keys, live, &mut args, &mut clauses);
    in_list_or_null("pool_id", &filter.pools, &mut args, &mut clauses);
    in_list("account_id", &filter.accounts, &mut args, &mut clauses);
    in_list(
        "canonical_model",
        &canonical_models(&filter.models),
        &mut args,
        &mut clauses,
    );
    in_list("kind", &filter.kinds, &mut args, &mut clauses);
    pattern_any("user_agent", &filter.user_agents, &mut args, &mut clauses);
    pattern_any("client", &filter.clients, &mut args, &mut clauses);
    if !filter.probes_counted() {
        clauses.push("kind != 'probe'".into());
    }
    let extra = if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT json_extract(a.selection, '$.placement.rung'), COUNT(*), \
         SUM(CASE WHEN json_extract(a.selection, '$.placement.rung') = 'content' \
                  THEN CASE WHEN COALESCE(a.cache_read_tokens, 0) >= COALESCE(json_extract(a.selection, '$.placement.prefix_tokens'), 0) THEN 1 ELSE 0 END \
                  ELSE CASE WHEN COALESCE(a.cache_read_tokens, 0) > 0 THEN 1 ELSE 0 END END), \
         SUM(COALESCE(a.cache_read_tokens, 0)), SUM(COALESCE(json_extract(a.selection, '$.placement.prefix_tokens'), 0)) \
         FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE json_extract(a.selection, '$.placement') IS NOT NULL \
         AND (a.input_tokens IS NOT NULL OR a.output_tokens IS NOT NULL OR a.cache_read_tokens IS NOT NULL \
              OR a.cache_write_5m_tokens IS NOT NULL OR a.cache_write_1h_tokens IS NOT NULL) \
         AND r.received_at >= ?1 AND r.received_at < ?2{extra} GROUP BY 1 ORDER BY 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok(PlacementRow {
            rung: r.get(0)?,
            placements: r.get::<_, i64>(1)? as u64,
            hits: r.get::<_, i64>(2)? as u64,
            cache_read_tokens: r.get::<_, i64>(3)? as u64,
            promised_tokens: r.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrafficBucket {
    pub at: i64,
    pub requests: u64,
    pub failed: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    /// The billed rows' dollars, a part of `cost_usd`.
    pub billed_usd: f64,
    /// The plan rows as the Pro-week figure, `None` where a plan row with tokens priced nothing.
    pub equivalent: Option<f64>,
}

/// No row, so the figure is the sum of no plan row: zero, which the fold keeps until a row that priced nothing joins.
impl Default for TrafficBucket {
    fn default() -> Self {
        TrafficBucket {
            at: 0,
            requests: 0,
            failed: 0,
            tokens: 0,
            cost_usd: 0.0,
            billed_usd: 0.0,
            equivalent: Some(0.0),
        }
    }
}

/// Every bucket in the range from the rollups of one pool, probes left out, read as `pools_traffic` reads it for the
/// pool alone.
#[allow(clippy::too_many_arguments)]
pub async fn pool_traffic(
    db: &Arc<LedgerDb>,
    pool_id: &str,
    from_hour: i64,
    to_hour: i64,
    bucket_seconds: i64,
    prices: Arc<PriceRevisions>,
    costs: Arc<PlanTables>,
    now: i64,
) -> Result<Vec<TrafficBucket>, LedgerError> {
    let pool_ids = vec![pool_id.to_string()];
    let mut traffic = db
        .read(move |c| {
            pools_traffic(
                c,
                &pool_ids,
                from_hour,
                to_hour,
                bucket_seconds,
                &prices,
                &costs,
                now,
            )
        })
        .await?;
    Ok(traffic.remove(pool_id).unwrap_or_default())
}

/// Every bucket in the range per pool of `pool_ids`, probes left out, from one statement on `conn` that sums each
/// pool's rows by the price columns and the hour, so several pools read the table once and no row is decoded. Each
/// group's dollars and Pro-week figure are priced from its summed counts through `price_group`, the figure against the
/// anchor row in force at `now`. The statement orders the groups as it groups them, so a bucket adds its groups'
/// figures in one order whichever plan SQLite picks. A pool without a row answers its zero buckets, and an empty
/// `pool_ids` answers an empty map and runs no statement.
#[allow(clippy::too_many_arguments)]
pub fn pools_traffic(
    conn: &Connection,
    pool_ids: &[String],
    from_hour: i64,
    to_hour: i64,
    bucket_seconds: i64,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> Result<BTreeMap<String, Vec<TrafficBucket>>, LedgerError> {
    if pool_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let fold = TrafficFold::new(from_hour, to_hour, bucket_seconds, prices, costs, now)?;
    let mut out: BTreeMap<String, Vec<TrafficBucket>> = pool_ids
        .iter()
        .map(|pool_id| (pool_id.clone(), fold.zero.clone()))
        .collect();
    let mut args: Vec<rusqlite::types::Value> = vec![from_hour.into(), to_hour.into()];
    let mut clauses = vec!["kind != 'probe' AND hour >= ?1 AND hour < ?2".to_string()];
    pool_term(pool_ids, &[], &mut args, &mut clauses);
    let mut stmt = conn.prepare(&format!(
        "SELECT pool_id, SUM(MAX(requests - succeeded, 0)), {PRICE_GROUP_COLUMNS}, {PRICE_GROUP_SUMS} \
         FROM rollups_hourly WHERE {} GROUP BY pool_id, {PRICE_GROUP_COLUMNS} \
         ORDER BY pool_id, {PRICE_GROUP_COLUMNS}",
        clauses.join(" AND ")
    ))?;
    let groups = stmt.query_map(params_from_iter(args), |r| {
        Ok((
            r.get::<_, String>(0)?,
            clamped_count(r, 1)?,
            price_group(r, 2, prices, costs, now)?,
        ))
    })?;
    for group in groups {
        let (pool_id, failed, (hour, totals)) = group?;
        if let Some(buckets) = out.get_mut(&pool_id) {
            fold.add_sums(buckets, hour, failed, &totals);
        }
    }
    Ok(out)
}

/// The zero buckets of a traffic range and the fold that adds figures to the bucket of their hour, nothing before the
/// first bucket or past the last. The pool readers add each priced group of their sum, and the dashboard's day each
/// rollup row of its read, which leaves the probe rows out.
struct TrafficFold<'a> {
    first: i64,
    bucket_seconds: i64,
    zero: Vec<TrafficBucket>,
    prices: &'a PriceRevisions,
    costs: &'a PlanTables,
    now: i64,
}

impl<'a> TrafficFold<'a> {
    fn new(
        from_hour: i64,
        to_hour: i64,
        bucket_seconds: i64,
        prices: &'a PriceRevisions,
        costs: &'a PlanTables,
        now: i64,
    ) -> Result<Self, LedgerError> {
        let first = first_edge(from_hour, to_hour, bucket_seconds)?;
        let zero = bucket_starts(first, to_hour, bucket_seconds)
            .map(|at| TrafficBucket {
                at: at * 1_000,
                ..TrafficBucket::default()
            })
            .collect();
        Ok(TrafficFold {
            first,
            bucket_seconds,
            zero,
            prices,
            costs,
            now,
        })
    }

    fn add(&self, out: &mut [TrafficBucket], r: &RollupRow) {
        let (usd, billed, equivalent) = shared_figures(r, self.prices, self.costs, self.now);
        let totals = Totals {
            cost_usd: usd,
            requests: r.requests,
            tokens: tokens_of(r.usage()),
            billed_usd: billed,
            equivalent,
        };
        self.add_sums(out, r.hour, r.requests.saturating_sub(r.succeeded), &totals);
    }

    fn add_sums(&self, out: &mut [TrafficBucket], hour: i64, failed: u64, totals: &Totals) {
        if let Some(bucket) = bucket_slot(out, self.first, self.bucket_seconds, hour) {
            bucket.requests += totals.requests;
            bucket.failed += failed;
            bucket.tokens += totals.tokens;
            bucket.cost_usd += totals.cost_usd;
            bucket.billed_usd += totals.billed_usd;
            bucket.equivalent = add_figures(bucket.equivalent, totals.equivalent);
        }
    }
}

/// The range and scope of a burn, rewrite or moves series; `RollupFilter` is the analytics routes' filter.
#[derive(Clone, Debug, Default)]
pub struct SeriesFilter {
    /// Milliseconds; the range is `from` inclusive to `to` exclusive.
    pub from: i64,
    pub to: i64,
    /// One point per bucket, from `bucket_seconds(from, to)`; buckets align with the hour edges as `series` aligns them.
    pub bucket_seconds: i64,
    /// Account ids; empty means every account. The one scope `rewrite_series` and `moves_by_reason` read, since the account rollups carry no other dimension.
    pub accounts: Vec<String>,
    /// Key ids, matched on `key_id` alone; empty means every key. `rollups_hourly` only.
    pub keys: Vec<String>,
    /// Pool ids; empty means every pool. `rollups_hourly` only.
    pub pools: Vec<String>,
    /// Request classes, `NO_CLASS` naming the classless rows; empty means every class. `rollups_hourly` only.
    pub request_classes: Vec<String>,
}

/// The first bucket's edge at or below `start`, in seconds, once the range up to `end` passes the guard every series
/// reader shares: the bucket must be positive, and the points from the edge to `end` at most `MAX_POINTS`.
fn first_edge(start: i64, end: i64, bucket_seconds: i64) -> Result<i64, LedgerError> {
    // Checked before the division below it: `div_euclid` panics on a zero bucket, which `SeriesFilter::default()`
    // carries, rather than answering a range the guard below could still refuse.
    if bucket_seconds <= 0 {
        return Err(LedgerError::Range(
            "bucket_seconds must be positive".to_string(),
        ));
    }
    let first = start.div_euclid(bucket_seconds) * bucket_seconds;
    // The ceiling: the readers answer a trailing partial bucket as a point of its own.
    let count = (end - first).saturating_add(bucket_seconds - 1) / bucket_seconds;
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Range(format!(
            "the range needs {count} points, and the most a chart carries is {MAX_POINTS}"
        )));
    }
    Ok(first)
}

/// The first and the last bucket edge of `[from, to)` in milliseconds, in seconds, past the guard every series reader
/// shares.
fn series_edges(from: i64, to: i64, bucket_seconds: i64) -> Result<(i64, i64), LedgerError> {
    let last = hour_ceil(to);
    let first = first_edge(from / 1_000, last, bucket_seconds)?;
    Ok((first, last))
}

/// The bucket edges a `SeriesFilter` reads over, past the guard every series reader shares.
fn bucket_range(filter: &SeriesFilter) -> Result<(i64, i64), LedgerError> {
    series_edges(filter.from, filter.to, filter.bucket_seconds)
}

/// Every bucket start of `[first, last)`, a trailing partial bucket included. The bucket must be positive.
fn bucket_starts(first: i64, last: i64, bucket_seconds: i64) -> impl Iterator<Item = i64> {
    (first..last).step_by(bucket_seconds as usize)
}

/// The point of `points` whose bucket holds `hour`, the first bucket starting at `first`: `None` past the last point
/// and before `first`, where the truncating division would file the hour in the first bucket.
fn bucket_slot<T>(points: &mut [T], first: i64, bucket_seconds: i64, hour: i64) -> Option<&mut T> {
    if hour < first {
        return None;
    }
    points.get_mut(((hour - first) / bucket_seconds) as usize)
}

/// One point of the burn series: a subscription row's tokens as the share of the account tier's own windows it
/// recorded (the 5-hour share weighed by the band at the row's instant, the 7-day share unbanded), answered where the
/// read's groups each hold one account's rows, and as the Pro-week figure under every grouping, an api_key row's in
/// dollars. A share is `None` on a window while no subscription row of the bucket could be priced on it, or while any
/// row of the bucket with a tier could not be.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct BurnPoint {
    /// Milliseconds, like every other series' `at`.
    pub at: i64,
    pub cached_tokens: u64,
    pub five_hour_share: Option<f64>,
    pub seven_day_share: Option<f64>,
    /// The model week's share beside the two shared windows, where the 5-hour and 7-day shares are answered. `None`
    /// where no row of the bucket is on a model with a week of its own, and latched as the shared windows are.
    pub model_week_share: Option<f64>,
    /// Every token of the bucket's rows, all five types, whatever the tables priced: zero for an idle bucket, so a
    /// reader tells an idle bucket from one whose traffic no table priced.
    pub tokens: u64,
    pub uncached_tokens: u64,
    pub usd: f64,
    /// The bucket's plan rows as the Pro-week figure under every grouping. A billed row adds a zero, so a bucket with
    /// tokens and no plan row reads 0, `None` for an idle bucket and for one holding a plan row the cost model cannot
    /// price.
    pub equivalent: Option<f64>,
    /// True for a bucket holding a plan row with tokens that priced nothing, so a `None` figure with the flag false is
    /// an idle bucket.
    pub unpriced: bool,
}

/// One figure of a burn bucket summed row by row: `Idle` until a row adds to it, and `Unpriced` once a row it should
/// price priced nothing, whichever row came first, so a partial sum never reads as the bucket's figure.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Latch {
    #[default]
    Idle,
    Sum(f64),
    Unpriced,
}

impl Latch {
    fn add(&mut self, figure: Option<f64>) {
        *self = match (*self, figure) {
            (Latch::Unpriced, _) | (_, None) => Latch::Unpriced,
            (Latch::Idle, Some(figure)) => Latch::Sum(figure),
            (Latch::Sum(sum), Some(figure)) => Latch::Sum(sum + figure),
        };
    }

    /// A zero on a figure no row of the bucket has latched.
    fn zero(&mut self) {
        if *self == Latch::Idle {
            *self = Latch::Sum(0.0);
        }
    }

    fn figure(self) -> Option<f64> {
        match self {
            Latch::Sum(sum) => Some(sum),
            Latch::Idle | Latch::Unpriced => None,
        }
    }
}

/// A burn bucket's figures as its rows are priced in, read out as its `BurnPoint` once they are all in.
#[derive(Clone, Debug, Default)]
pub(crate) struct BurnSums {
    at: i64,
    cached_tokens: u64,
    five_hour_share: Latch,
    seven_day_share: Latch,
    model_week_share: Latch,
    tokens: u64,
    uncached_tokens: u64,
    usd: f64,
    equivalent: Latch,
}

impl BurnSums {
    /// The point, its `unpriced` the figure's latch.
    pub(crate) fn point(self) -> BurnPoint {
        BurnPoint {
            at: self.at,
            cached_tokens: self.cached_tokens,
            five_hour_share: self.five_hour_share.figure(),
            seven_day_share: self.seven_day_share.figure(),
            model_week_share: self.model_week_share.figure(),
            tokens: self.tokens,
            uncached_tokens: self.uncached_tokens,
            usd: self.usd,
            equivalent: self.equivalent.figure(),
            unpriced: self.equivalent == Latch::Unpriced,
        }
    }
}

/// The row's funding decides its unit, so the kind the registry holds for the account now has no say and a deleted
/// or recreated account moves no dollar into a share. One `price_row` call answers every figure: the row's read-time
/// dollars for a billed row, and for a plan row the Pro-week figure against the anchor row in force at the key's `now`,
/// and, where `shares` holds and the row recorded a tier, the shares of that tier's windows (each banded at the key's
/// `band_at`, else its `at`). A plan row with no tier recorded prices no share and still its figure. A fast row and
/// one in overage add their dollars and a zero share where the tables size the tier: fast mode spends usage credits
/// and overage is billed in dollars, neither draws a plan window, and the zero still tells a reader the account is
/// priced in its window unit. Every billed row adds a zero figure, so a bucket with tokens and no plan row reads 0
/// there and only an idle bucket reads `None`. A bucket holding a row the table cannot price answers `None` on that
/// window and in the figure, whichever row came first, and a row with no token has nothing to price and leaves every
/// figure as the other rows set it. The figures are `price_request`'s, which the session fold prices a request through
/// too, so a session's spend and its burn series price one way.
pub(crate) fn price_usage(
    point: &mut BurnSums,
    prices: &PriceRevisions,
    tables: &PlanTables,
    key: RowKey<'_>,
    usage: PinUsage,
    shares: bool,
) {
    let key = RowKey {
        tier: key.tier.filter(|_| shares),
        ..key
    };
    let Some(price) = price_request(prices, tables, key, usage) else {
        return;
    };
    // A window the price leaves out keeps what the bucket's other rows set.
    let shares = price.shares.unwrap_or_default();
    let windows = [
        (Some(FIVE_HOUR), &mut point.five_hour_share),
        (Some(SEVEN_DAY), &mut point.seven_day_share),
        (model_week(key.model), &mut point.model_week_share),
    ]
    .into_iter()
    .filter_map(|(window, latch)| Some((*shares.get(window?)?, latch)));
    match price.funding {
        Funding::Plan => {
            windows.for_each(|(share, latch)| latch.add(share));
            point.equivalent.add(price.equivalent);
        }
        _ => {
            point.usd += price.usd;
            windows.for_each(|(_, latch)| latch.zero());
            point.equivalent.zero();
        }
    }
    point.cached_tokens += usage.cache_read;
    point.tokens += tokens_of(usage);
    point.uncached_tokens += usage.input;
}

/// What one request's serving attempt costs, as `price_usage` prices a usage row: one `price_row` call under the
/// funding, the kind, the tier, the entrypoint, the classifier and the band at the attempt's completion.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestPrice {
    pub funding: Funding,
    /// The serving attempt's list dollars through `price_usd`.
    pub usd: f64,
    /// A plan row's Pro-week figure, `None` where the cost model prices nothing for it, and `Some(0.0)` on a billed
    /// row.
    pub equivalent: Option<f64>,
    /// A plan row's shares of its tier's windows, keyed `five_hour`, `seven_day`, `seven_day_<family>`, a window the
    /// tables do not size `None`, and a billed row's zero on each window its tier sizes, none for an api_key row.
    /// `None` without a tier.
    pub shares: Option<BTreeMap<&'static str, Option<f64>>>,
}

/// `None` for a usage with no token, which prices nothing, as `price_usage` returns early for it.
pub fn price_request(
    prices: &PriceRevisions,
    tables: &PlanTables,
    key: RowKey<'_>,
    usage: PinUsage,
) -> Option<RequestPrice> {
    // A failed request books a row with no token, and the cost model refuses a model it has no weight for before it
    // reads the usage: such a row would null a priced hour for a charge of nothing.
    if tokens_of(usage) == 0 {
        return None;
    }
    let priced = price_row(prices, tables, &key, usage);
    let windows = [FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .chain(model_week(key.model));
    let (equivalent, shares) = match key.funding {
        // A tier the cost revision does not size answers no map, and every window it would share reads unpriced.
        Funding::Plan => (
            priced.equivalent,
            key.tier.map(|_| {
                windows
                    .map(|window| {
                        let share = priced
                            .shares
                            .as_ref()
                            .and_then(|shares| shares.get(window).copied().flatten());
                        (window, share)
                    })
                    .collect()
            }),
        ),
        // Ahead of the model's weights: a dollar-billed row prices in dollars whatever the model weighs it at, so the
        // zero share rests on the tier alone. An api_key account has no window to share.
        funding => (
            Some(0.0),
            key.tier.filter(|_| funding != Funding::ApiKey).map(|tier| {
                windows
                    .filter(|window| {
                        tables
                            .tier_factor(tier, window, key.entrypoint, key.at)
                            .is_some()
                    })
                    .map(|window| (window, Some(0.0)))
                    .collect()
            }),
        ),
    };
    Some(RequestPrice {
        funding: key.funding,
        usd: priced.usd.as_ref().map_or(0.0, CostSplit::total),
        equivalent,
        shares,
    })
}

/// The rollup rows `burn_series` prices for `filter`: every hour of its bucket range, whatever its scopes, so one
/// read serves `burn_series_over` for every scope over the same range and bucket. The burn route reads through
/// `burn_groups_paged` instead, which holds a page at a time.
#[cfg(any(test, feature = "test-support"))]
pub fn burn_rows(conn: &Connection, filter: &SeriesFilter) -> Result<Vec<RollupRow>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    crate::rollup::read_rollups(conn, first, last, &RollupSelect::default())
}

/// `burn_series_over` on the rows `burn_rows` reads for `filter`, read at `now`.
#[cfg(any(test, feature = "test-support"))]
pub fn burn_series(
    conn: &Connection,
    tables: &PlanTables,
    prices: &PriceRevisions,
    filter: &SeriesFilter,
    now: i64,
) -> Result<Vec<BurnPoint>, LedgerError> {
    let rows = burn_rows(conn, filter)?;
    burn_series_over(&rows, tables, prices, filter, now)
}

/// The burn series of `filter`'s scopes over `rows`, which `burn_rows` read for the same range and bucket, its shares
/// answered where the filter names one account. A subscription row's 5-hour share and figure carry the band of the
/// hour the rollup folded the request under, since the rollup keeps no completion. The band reads per row rather than
/// per bucket, since a six-hour or daily bucket can straddle a band edge and only the row's own hour says which side of
/// it the row fell on. The figure divides by the anchor row in force at `now`, the read's instant. One pass: each row
/// lands in its bucket by index.
pub fn burn_series_over(
    rows: &[RollupRow],
    tables: &PlanTables,
    prices: &PriceRevisions,
    filter: &SeriesFilter,
    now: i64,
) -> Result<Vec<BurnPoint>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let mut sums = empty_burn_sums(first, last, filter.bucket_seconds);
    let shares = names_one_account(&filter.accounts);
    for row in rows.iter().filter(|r| burn_scope_holds(filter, r)) {
        price_burn_row(
            &mut sums,
            first,
            filter.bucket_seconds,
            row,
            tables,
            prices,
            shares,
            now,
        );
    }
    Ok(sums.into_iter().map(BurnSums::point).collect())
}

/// Every bucket of `[first, last)`, empty. Ceiling, not floor: `last` is only rounded up to the hour (`hour_ceil`),
/// so a 6-hour or daily bucket can leave a trailing partial bucket, which is a point of its own.
fn empty_burn_sums(first: i64, last: i64, bucket_seconds: i64) -> Vec<BurnSums> {
    bucket_starts(first, last, bucket_seconds)
        .map(|at| BurnSums {
            at: at * 1_000,
            ..BurnSums::default()
        })
        .collect()
}

fn burn_scope_holds(filter: &SeriesFilter, r: &RollupRow) -> bool {
    (filter.keys.is_empty() || filter.keys.contains(&r.key_id))
        && (filter.pools.is_empty() || filter.pools.contains(&r.pool_id))
        && (filter.accounts.is_empty() || filter.accounts.contains(&r.account_id))
        && (filter.request_classes.is_empty() || filter.request_classes.contains(&r.request_class))
}

/// One rollup row priced into the bucket it falls in, nothing for a row before the first bucket's edge.
#[allow(clippy::too_many_arguments)]
fn price_burn_row(
    points: &mut [BurnSums],
    first: i64,
    bucket_seconds: i64,
    row: &RollupRow,
    tables: &PlanTables,
    prices: &PriceRevisions,
    shares: bool,
    now: i64,
) {
    let Some(point) = bucket_slot(points, first, bucket_seconds, row.hour) else {
        return;
    };
    // The rollup keeps no completion, so the key's `band_at` is `None` and the band reads at the row's hour.
    price_usage(
        point,
        prices,
        tables,
        row.price_key(now),
        row.usage(),
        shares,
    );
}

/// The burn series of every group under `group_by`, any dimension `GroupBy` names, within `filter`'s scopes, read
/// through `read_rollups_paged` and priced into the group's buckets as the pages arrive: each group's points are the
/// ones `burn_series_over` answers for that group's rows alone, the shares answered where each group holds one
/// account's rows. The groups are the ones the range itself holds, and the read reaches back to the first bucket's
/// edge, which may start before it.
#[allow(clippy::too_many_arguments)]
pub async fn burn_groups_paged(
    db: &Arc<LedgerDb>,
    tables: &PlanTables,
    prices: &PriceRevisions,
    filter: &SeriesFilter,
    group_by: GroupBy,
    live: &LiveKeys,
    agent_groups: &[String],
    now: i64,
) -> Result<Vec<(String, Vec<BurnPoint>)>, LedgerError> {
    let shares = holds_one_account(group_by, &filter.accounts);
    let grouping = Grouping {
        agent_groups,
        by: group_by,
        live,
    };
    let (first, last) = bucket_range(filter)?;
    let from_hour = hour_of(filter.from);
    let select = RollupSelect {
        pools: filter.pools.clone(),
        accounts: filter.accounts.clone(),
        keys: filter
            .keys
            .iter()
            .map(|id| KeySelector {
                key_id: id.clone(),
                key_generation: None,
            })
            .collect(),
        ..RollupSelect::default()
    };
    // Each group's points, and whether a row of the range itself joined it.
    let mut groups: BTreeMap<String, (Vec<BurnSums>, bool)> = BTreeMap::new();
    read_rollups_paged(db, &select, first, last, |row| {
        if !burn_scope_holds(filter, row) {
            return;
        }
        let (points, held) = groups
            .entry(grouping.group_of(row))
            .or_insert_with(|| (empty_burn_sums(first, last, filter.bucket_seconds), false));
        *held |= row.hour >= from_hour && row.hour < last;
        price_burn_row(
            points,
            first,
            filter.bucket_seconds,
            row,
            tables,
            prices,
            shares,
            now,
        );
    })
    .await?;
    Ok(groups
        .into_iter()
        .filter(|(_, (_, held))| *held)
        .map(|(group, (sums, _))| (group, sums.into_iter().map(BurnSums::point).collect()))
        .collect())
}

/// The cache-write tokens the account rollups booked as a rewrite, per bucket; the bucket's `at` is
/// milliseconds, like `BurnPoint.at`. The sums are SQLite's, so the read decodes no account row.
pub fn rewrite_series(
    conn: &Connection,
    filter: &SeriesFilter,
) -> Result<Vec<(i64, u64)>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let mut args: Vec<rusqlite::types::Value> = vec![first.into(), last.into()];
    let mut clauses = vec!["hour >= ?1 AND hour < ?2".to_string()];
    in_list("account_id", &filter.accounts, &mut args, &mut clauses);
    let mut stmt = conn.prepare(&format!(
        "SELECT hour, SUM(rewrite_tokens) FROM account_rollups_hourly WHERE {} GROUP BY hour",
        clauses.join(" AND ")
    ))?;
    let hours = stmt
        .query_map(params_from_iter(args), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut points: Vec<(i64, u64)> = bucket_starts(first, last, filter.bucket_seconds)
        .map(|at| (at * 1_000, 0))
        .collect();
    for (hour, tokens) in hours {
        if let Some(point) = bucket_slot(&mut points, first, filter.bucket_seconds, hour) {
            point.1 += tokens;
        }
    }
    Ok(points)
}

/// `held_series`'s statement and its binds over the hours `[first, last)`, spelled once so the plan test explains the
/// statement the read runs.
pub(crate) fn held_series_statement(
    filter: &SeriesFilter,
    first: i64,
    last: i64,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut args: Vec<rusqlite::types::Value> = vec![first.into(), last.into()];
    let mut clauses = vec!["hour >= ?1 AND hour < ?2".to_string()];
    pool_term(&filter.pools, &filter.accounts, &mut args, &mut clauses);
    in_list("account_id", &filter.accounts, &mut args, &mut clauses);
    (
        format!(
            "SELECT hour, SUM(slow_down_held_ms), SUM(slow_down_held) FROM rollups_hourly WHERE {} GROUP BY hour",
            clauses.join(" AND ")
        ),
        args,
    )
}

/// The slow-down's held time per bucket over the filter's pools and accounts: `(at, held_ms, requests)`, the hourly
/// rollups' slow-down waits summed and the requests that waited, the bucket's `at` in milliseconds as `rewrite_series`
/// answers it. The sums are SQLite's, so the read carries no row of the table into memory.
pub fn held_series(
    conn: &Connection,
    filter: &SeriesFilter,
) -> Result<Vec<(i64, u64, u64)>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let (sql, args) = held_series_statement(filter, first, last);
    let mut stmt = conn.prepare(&sql)?;
    let hours = stmt
        .query_map(params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut points: Vec<(i64, u64, u64)> = bucket_starts(first, last, filter.bucket_seconds)
        .map(|at| (at * 1_000, 0, 0))
        .collect();
    for (hour, held_ms, requests) in hours {
        if let Some(point) = bucket_slot(&mut points, first, filter.bucket_seconds, hour) {
            point.1 += held_ms;
            point.2 += requests;
        }
    }
    Ok(points)
}

/// The moves the account rollups counted over the range, summed by reason across every bucket and account the
/// filter admits: each reason's sum over its fits.
pub fn moves_by_reason(
    conn: &Connection,
    filter: &SeriesFilter,
) -> Result<BTreeMap<String, u64>, LedgerError> {
    Ok(moves_by_reason_and_fit(conn, filter)?
        .into_iter()
        .map(|(reason, fits)| (reason, fits.values().sum()))
        .collect())
}

/// The moves of the range by reason, then by destination fit (`fits`, `rewrite_only`, `least_bad`, `unknown`), from
/// the rollups' `{reason}/{fit}` key, so a chart can split each reason by its own fits. A key without `/` reads fit
/// `unknown`.
pub fn moves_by_reason_and_fit(
    conn: &Connection,
    filter: &SeriesFilter,
) -> Result<BTreeMap<String, BTreeMap<String, u64>>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let rows = read_account_rollups(conn, first, last)?;
    let mut out: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|r| filter.accounts.is_empty() || filter.accounts.contains(&r.account_id))
    {
        for (key, count) in &row.migrations {
            let (reason, fit) = key.split_once('/').unwrap_or((key, "unknown"));
            *out.entry(reason.to_string())
                .or_default()
                .entry(fit.to_string())
                .or_default() += count;
        }
    }
    Ok(out)
}

/// One routing condition the hourly fold wrote: the kind, its subject, the oldest qualifying event's time (for
/// `slowed_long` the own start of the oldest span counted, for `meter_drift` the `window_end` of the oldest off
/// window, at most the fold's instant), the count and the kind's detail. `pool_id` is the pool the condition's events
/// name, which an account's `slowed_long` carries as the pool of its newest span, and null for an account's condition
/// whose events name none.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutingCondition {
    pub kind: RoutingKind,
    pub subject_kind: String,
    pub subject_id: String,
    pub pool_id: Option<String>,
    pub since: i64,
    pub count: u64,
    pub detail: Value,
    pub computed_at: i64,
}

/// Every row of `routing_conditions`, by kind and subject: what the last fold found over its trailing day and in the
/// newest reconciled windows. A row whose word names no kind is skipped.
pub fn routing_conditions(conn: &Connection) -> Result<Vec<RoutingCondition>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT kind, subject_kind, subject_id, pool_id, since, count, detail_json, computed_at \
         FROM routing_conditions ORDER BY kind, subject_kind, subject_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let Some(kind) = RoutingKind::parse(&r.get::<_, String>(0)?) else {
                return Ok(None);
            };
            Ok(Some(RoutingCondition {
                kind,
                subject_kind: r.get(1)?,
                subject_id: r.get(2)?,
                pool_id: r.get(3)?,
                since: r.get(4)?,
                count: clamped_count(r, 5)?,
                detail: detail_json(Some(&r.get::<_, String>(6)?)),
                computed_at: r.get(7)?,
            }))
        })?
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// How many sessions `session_burn_series` answers: the burn chart's own row cap, so a busy range's read stays
/// bounded by a caller-independent constant rather than the range's size.
const SESSION_BURN_LIMIT: usize = 20;

/// `top_sessions_by_tokens`' statement and its binds, spelled once so the plan test explains the statement the read
/// runs. A request is admitted within an hour of any attempt's start, so the request's own index carries the range.
/// The index is named: without statistics, as on a fresh file, SQLite prices a scan of every attempt below this range
/// search. Named accounts rank by their own attempts alone, the attempts the series then prices, and the unary plus
/// keeps them off the attempts' account index so the requests' range stays the outer loop.
pub(crate) fn top_sessions_statement(
    from: i64,
    to: i64,
    limit: usize,
    accounts: &[String],
) -> (String, Vec<rusqlite::types::Value>) {
    let named = if accounts.is_empty() {
        String::new()
    } else {
        let marks = (0..accounts.len())
            .map(|i| format!("?{}", i + 4))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" AND +a.account_id IN ({marks})")
    };
    let sql = format!(
        "SELECT r.session_id FROM attempts a \
         JOIN requests r INDEXED BY requests_received ON r.id = a.request_id \
         WHERE r.session_id IS NOT NULL AND a.started_at >= ?1 AND a.started_at < ?2 \
         AND r.received_at >= ?1 - {HOUR_MS} AND r.received_at < ?2{named} \
         GROUP BY r.session_id \
         ORDER BY SUM(COALESCE(a.input_tokens, 0) + COALESCE(a.cache_read_tokens, 0) \
             + COALESCE(a.cache_write_5m_tokens, 0) + COALESCE(a.cache_write_1h_tokens, 0) \
             + COALESCE(a.output_tokens, 0)) DESC, r.session_id \
         LIMIT ?3"
    );
    let mut bind = vec![
        rusqlite::types::Value::Integer(from),
        rusqlite::types::Value::Integer(to),
        rusqlite::types::Value::Integer(limit as i64),
    ];
    bind.extend(
        accounts
            .iter()
            .map(|account| rusqlite::types::Value::Text(account.clone())),
    );
    (sql, bind)
}

/// The `limit` sessions with the most tokens over `[from, to)` on `accounts` (every account when empty), ranked in
/// SQL rather than in memory, so a caller never reads an attempt row belonging to a session it will not answer.
fn top_sessions_by_tokens(
    conn: &Connection,
    from: i64,
    to: i64,
    limit: usize,
    accounts: &[String],
) -> Result<Vec<String>, LedgerError> {
    let (sql, bind) = top_sessions_statement(from, to, limit, accounts);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bind), |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(LedgerError::from)
}

/// The columns of `attempts a JOIN requests r` an attempt is priced from, in the order `PricedAttempt::read` reads
/// them. The completion is aliased `completed`, so a statement's `WHERE` can bound it.
pub(crate) const PRICED_ATTEMPT_COLUMNS: &str =
    "COALESCE(a.finished_at, r.finished_at, a.started_at) AS completed, r.canonical_model, \
     a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens, a.output_tokens, \
     r.received_at, a.speed, a.account_kind, a.overage_in_use, r.kind, a.tier, r.client, r.classifier, \
     a.usage_based";

/// One attempt's facts toward the share and the dollars it is priced at, read off its attempt and request rows.
pub(crate) struct PricedAttempt {
    /// Milliseconds: the attempt's completion, `attempts.finished_at`, else the request's, where the meter charges
    /// it; the band that prices it is read here.
    completed_at: i64,
    canonical_model: String,
    usage: PinUsage,
    /// Milliseconds, `requests.received_at`: the instant that names the revisions the attempt's dollars and shares
    /// read and the threshold its band reads.
    received_at: i64,
    /// `attempts.speed`, the speed the answer reported.
    speed: Option<String>,
    /// `attempts.account_kind`, `attempts.usage_based` and `attempts.overage_in_use`, the three facts beside the speed
    /// that say who paid.
    account_kind: String,
    usage_based: bool,
    overage_in_use: Option<bool>,
    /// `requests.kind`, so a probe's attempt prices as a probe row does.
    kind: String,
    /// `attempts.tier`, the account tier the attempt recorded at its landing.
    tier: Option<String>,
    /// `requests.client`, `None` on a row from before the column existed.
    client: Option<String>,
    /// `requests.classifier`, `None` for a request the marker did not hold and on a row from before the column.
    classifier: Option<String>,
}

impl PricedAttempt {
    /// The attempt from `PRICED_ATTEMPT_COLUMNS` read from column `first` on, a missing or negative token count read
    /// as zero.
    pub(crate) fn read(row: &Row, first: usize) -> rusqlite::Result<PricedAttempt> {
        let tokens = |i: usize| -> rusqlite::Result<u64> {
            Ok(row.get::<_, Option<i64>>(first + i)?.unwrap_or(0).max(0) as u64)
        };
        Ok(PricedAttempt {
            completed_at: row.get(first)?,
            canonical_model: row.get(first + 1)?,
            usage: PinUsage {
                input: tokens(2)?,
                cache_read: tokens(3)?,
                write_5m: tokens(4)?,
                write_1h: tokens(5)?,
                output: tokens(6)?,
            },
            received_at: row.get(first + 7)?,
            speed: row.get(first + 8)?,
            account_kind: row.get(first + 9)?,
            overage_in_use: row.get(first + 10)?,
            kind: row.get(first + 11)?,
            tier: row.get(first + 12)?,
            client: row.get(first + 13)?,
            classifier: row.get(first + 14)?,
            usage_based: row.get(first + 15)?,
        })
    }

    /// The key and the usage `price_usage` prices the attempt at, `now` the read's instant.
    pub(crate) fn usage(&self, prices: &PriceRevisions, now: i64) -> (RowKey<'_>, PinUsage) {
        let facts = AttemptFacts {
            canonical_model: &self.canonical_model,
            kind: &self.kind,
            received_at: self.received_at,
            completed_at: self.completed_at,
            usage: self.usage,
            speed: self.speed.as_deref(),
            account_kind: &self.account_kind,
            usage_based: self.usage_based,
            overage_in_use: self.overage_in_use,
            tier: self.tier.as_deref(),
            client: self.client.as_deref(),
            classifier: self.classifier.as_deref(),
        };
        (row_key(facts, prices, now), self.usage)
    }
}

/// An attempt's facts and its request's that a `RowKey` is built from, borrowed from whichever read holds them, so the
/// burn series and the session fold build one key from one fact.
struct AttemptFacts<'a> {
    canonical_model: &'a str,
    kind: &'a str,
    received_at: i64,
    completed_at: i64,
    usage: PinUsage,
    speed: Option<&'a str>,
    account_kind: &'a str,
    usage_based: bool,
    overage_in_use: Option<bool>,
    tier: Option<&'a str>,
    client: Option<&'a str>,
    classifier: Option<&'a str>,
}

/// The attempt's revision is its request's, and its band the completion's, where the meter charges it.
fn row_key<'a>(facts: AttemptFacts<'a>, prices: &PriceRevisions, now: i64) -> RowKey<'a> {
    let band = prices
        .in_force(facts.received_at)
        .band_of(facts.canonical_model, facts.usage.prompt_tokens());
    let speed = Speed::of(facts.speed);
    RowKey {
        model: facts.canonical_model,
        speed,
        band,
        funding: funding_of(
            AccountKind::parse(facts.account_kind),
            facts.usage_based,
            facts.overage_in_use,
            speed,
        ),
        kind: facts.kind,
        tier: facts.tier,
        entrypoint: entrypoint_of_client(facts.client.unwrap_or(UNATTRIBUTED)),
        classifier: facts.classifier == Some(Classifier::Client.as_str()),
        at: facts.received_at,
        band_at: Some(facts.completed_at),
        now,
    }
}

/// One attempt toward a session's burn series: the rollups carry no session dimension, so this reads the request and
/// attempt rows directly, for the given sessions alone.
struct SessionAttempt {
    session_id: String,
    account_id: String,
    /// Milliseconds, `attempts.started_at`: the bucket the attempt lands in.
    at: i64,
    priced: PricedAttempt,
}

/// `read_session_attempts`' statement for `sessions` ids and its binds, spelled once so the plan test explains the
/// statement the read runs. A request is admitted within an hour of any attempt's start, so the request's own index
/// carries the range.
pub(crate) fn session_attempts_statement(
    from: i64,
    to: i64,
    sessions: &[String],
) -> (String, Vec<rusqlite::types::Value>) {
    let placeholders = sessions.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT r.session_id, a.account_id, a.started_at, {PRICED_ATTEMPT_COLUMNS} \
         FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE r.session_id IN ({placeholders}) AND a.started_at >= ? AND a.started_at < ? \
         AND r.received_at >= ? AND r.received_at < ?"
    );
    let mut bind: Vec<rusqlite::types::Value> = sessions
        .iter()
        .map(|s| rusqlite::types::Value::Text(s.clone()))
        .collect();
    bind.push(rusqlite::types::Value::Integer(from));
    bind.push(rusqlite::types::Value::Integer(to));
    bind.push(rusqlite::types::Value::Integer(from - HOUR_MS));
    bind.push(rusqlite::types::Value::Integer(to));
    (sql, bind)
}

fn read_session_attempts(
    conn: &Connection,
    from: i64,
    to: i64,
    sessions: &[String],
) -> Result<Vec<SessionAttempt>, LedgerError> {
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let (sql, bind) = session_attempts_statement(from, to, sessions);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bind), |row| {
        Ok(SessionAttempt {
            session_id: row.get(0)?,
            account_id: row.get(1)?,
            at: row.get(2)?,
            priced: PricedAttempt::read(row, 3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(LedgerError::from)
}

/// The request facts and the attempt the session fold prices, one row per attempt of a request and one row with no
/// attempt for a request no account served. The first fill answers them by session, `received_at` and request id, the
/// incremental read by `landed_at`, and both keep a request's attempts one run in start order.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSpendRow {
    pub request_id: String,
    /// `None` on a request that named no session, which the fold passes over.
    pub session_id: Option<String>,
    pub key_id: Option<String>,
    pub key_generation: i64,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub user_agent: Option<String>,
    pub request_class: Option<String>,
    pub canonical_model: String,
    pub kind: String,
    pub refused: bool,
    pub received_at: i64,
    pub landed_at: i64,
    pub attempt: Option<SpendAttempt>,
}

/// One attempt's facts the fold prices, `PricedAttempt`'s with the account and the start beside them, so a test outside
/// this crate builds one by its fields.
#[derive(Clone, Debug, PartialEq)]
pub struct SpendAttempt {
    pub account_id: String,
    pub account_kind: String,
    pub started_at: i64,
    /// The completion the meter charges at, `attempts.finished_at`, else the request's, else the start.
    pub completed_at: i64,
    pub speed: Option<String>,
    pub overage_in_use: Option<bool>,
    pub usage_based: bool,
    pub tier: Option<String>,
    pub client: Option<String>,
    pub classifier: Option<String>,
    pub usage: PinUsage,
}

impl SpendAttempt {
    /// The attempt priced under its request's receipt, model and kind, which the row carries, through the key the burn
    /// series builds for an attempt, `now` the read's instant.
    pub fn price(
        &self,
        received_at: i64,
        canonical_model: &str,
        kind: &str,
        prices: &PriceRevisions,
        tables: &PlanTables,
        now: i64,
    ) -> Option<RequestPrice> {
        let facts = AttemptFacts {
            canonical_model,
            kind,
            received_at,
            completed_at: self.completed_at,
            usage: self.usage,
            speed: self.speed.as_deref(),
            account_kind: &self.account_kind,
            usage_based: self.usage_based,
            overage_in_use: self.overage_in_use,
            tier: self.tier.as_deref(),
            client: self.client.as_deref(),
            classifier: self.classifier.as_deref(),
        };
        price_request(prices, tables, row_key(facts, prices, now), self.usage)
    }
}

/// The request half of the session spend reads, ahead of `PRICED_ATTEMPT_COLUMNS`, in the order
/// `SessionSpendRow::read` reads them.
const SESSION_SPEND_COLUMNS: &str =
    "r.id, r.session_id, r.key_id, r.key_generation, r.agent_id, r.agent_type, \
     r.user_agent, r.request_class, r.refused, r.landed_at, a.account_id, a.started_at";

/// The column `PRICED_ATTEMPT_COLUMNS` starts at in the session spend reads.
const SESSION_SPEND_PRICED_FROM: usize = 12;

impl SessionSpendRow {
    /// A request no account served joins no attempt, so its attempt half reads null and must not reach
    /// `PricedAttempt::read`. Its model, receipt and kind are request columns inside `PRICED_ATTEMPT_COLUMNS`, read by
    /// their place there whether an attempt joined or not.
    fn read(row: &Row) -> rusqlite::Result<SessionSpendRow> {
        let first = SESSION_SPEND_PRICED_FROM;
        let attempt = match row.get::<_, Option<String>>(10)? {
            Some(account_id) => {
                let priced = PricedAttempt::read(row, first)?;
                Some(SpendAttempt {
                    account_id,
                    account_kind: priced.account_kind,
                    started_at: row.get(11)?,
                    completed_at: priced.completed_at,
                    speed: priced.speed,
                    overage_in_use: priced.overage_in_use,
                    usage_based: priced.usage_based,
                    tier: priced.tier,
                    client: priced.client,
                    classifier: priced.classifier,
                    usage: priced.usage,
                })
            }
            None => None,
        };
        Ok(SessionSpendRow {
            request_id: row.get(0)?,
            session_id: row.get(1)?,
            key_id: row.get(2)?,
            key_generation: row.get(3)?,
            agent_id: row.get(4)?,
            agent_type: row.get(5)?,
            user_agent: row.get(6)?,
            request_class: row.get(7)?,
            refused: row.get(8)?,
            landed_at: row.get(9)?,
            canonical_model: row.get(first + 1)?,
            received_at: row.get(first + 7)?,
            kind: row.get(first + 11)?,
            attempt,
        })
    }
}

/// `session_spend_rows_since`'s statement: every request landed after the watermark with its attempts, oldest landing
/// first, through the landing index the writer's poll rule names. The index is named so the seek holds whatever
/// statistics a file carries. The request id keeps the two statements' orders one shape, and a request's attempts run
/// by start, then by attempt index, since two attempts can start in one millisecond and the run's last must be the
/// attempt the request log takes as the serving one.
pub(crate) fn session_spend_since_statement(
    landed_after: i64,
) -> (String, Vec<rusqlite::types::Value>) {
    let sql = format!(
        "SELECT {SESSION_SPEND_COLUMNS}, {PRICED_ATTEMPT_COLUMNS} \
         FROM requests r INDEXED BY requests_landed_at LEFT JOIN attempts a ON a.request_id = r.id \
         WHERE r.landed_at > ? ORDER BY r.landed_at, r.id, a.started_at, a.attempt"
    );
    (sql, vec![rusqlite::types::Value::Integer(landed_after)])
}

/// `session_spend_rows_of`'s statement for one chunk of named sessions: every request of each under its key lifetime
/// with its attempts, seeking the session index per named session as the identities read does, each session's rows
/// oldest first with each request's attempts one run (the request id before the attempt start, since two requests of
/// a session can share `received_at`, and the attempt index after it, as the incremental read orders). The key and its
/// generation scope the rows, as `session_identities_statement` scopes its own. The named sessions hold the outer loop
/// through `CROSS JOIN`: the session index yields the order the statement sorts by, and on statistics that make a
/// session look large the planner would walk the whole index in that order to spare the sort.
pub(crate) fn session_spend_of_statement(
    sessions: &[(String, i64, String)],
) -> (String, Vec<rusqlite::types::Value>) {
    let mut binds = Vec::with_capacity(sessions.len() * 3);
    let values: Vec<String> = sessions
        .iter()
        .map(|(key_id, key_generation, session_id)| {
            format!(
                "({})",
                bind_all(
                    &[
                        rusqlite::types::Value::Text(key_id.clone()),
                        rusqlite::types::Value::Integer(*key_generation),
                        rusqlite::types::Value::Text(session_id.clone()),
                    ],
                    &mut binds,
                )
            )
        })
        .collect();
    let sql = format!(
        "WITH named(key_id, key_generation, session_id) AS (VALUES {}) \
         SELECT {SESSION_SPEND_COLUMNS}, {PRICED_ATTEMPT_COLUMNS} \
         FROM named n CROSS JOIN requests r INDEXED BY requests_session ON r.session_id = n.session_id \
         AND r.key_id = n.key_id AND r.key_generation = n.key_generation \
         LEFT JOIN attempts a ON a.request_id = r.id \
         ORDER BY r.session_id, r.received_at, r.id, a.started_at, a.attempt",
        values.join(", ")
    );
    (sql, binds)
}

/// Every request landed after `landed_after` with its attempts, of any session or none.
pub fn session_spend_rows_since(
    conn: &Connection,
    landed_after: i64,
) -> Result<Vec<SessionSpendRow>, LedgerError> {
    let (sql, binds) = session_spend_since_statement(landed_after);
    read_session_spend_rows(conn, &sql, binds)
}

/// Chunks of `SESSION_IDENTITY_CHUNK` named sessions, merged, as `session_identities` chunks its own.
pub fn session_spend_rows_of(
    conn: &Connection,
    sessions: &[(String, i64, String)],
) -> Result<Vec<SessionSpendRow>, LedgerError> {
    let mut out = Vec::new();
    for chunk in sessions.chunks(SESSION_IDENTITY_CHUNK) {
        let (sql, binds) = session_spend_of_statement(chunk);
        out.extend(read_session_spend_rows(conn, &sql, binds)?);
    }
    Ok(out)
}

fn read_session_spend_rows(
    conn: &Connection,
    sql: &str,
    binds: Vec<rusqlite::types::Value>,
) -> Result<Vec<SessionSpendRow>, LedgerError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(binds), SessionSpendRow::read)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The newest `landed_at` the ledger holds, 0 with no request: the watermark a session fold starts from, so its
/// incremental read walks what lands after the start and never the history (the first fill answers a listed session
/// whole). The writer's `LANDING_STAMP_SQL`, which the planner serves off `requests_landed_at`.
pub fn newest_landed_at(conn: &Connection) -> Result<i64, LedgerError> {
    Ok(conn.query_row(LANDING_STAMP_SQL, [], |r| r.get(0))?)
}

/// The twenty sessions with the most tokens over the filter's range, each priced per bucket the way
/// `burn_series` prices an account row, save that an attempt lands in the bucket it started in, reads the band at
/// its completion and prices under the account tier it recorded. A session can move between accounts, so its shares
/// are answered where the filter names one account, whose attempts alone the series then holds. The Pro-week figure
/// divides by the anchor row in force at `now`. One pass over the range's attempts: each lands in its bucket by index,
/// rather than every session re-scanning every attempt for every bucket.
pub fn session_burn_series(
    conn: &Connection,
    tables: &PlanTables,
    prices: &PriceRevisions,
    filter: &SeriesFilter,
    now: i64,
) -> Result<Vec<(String, Vec<BurnPoint>)>, LedgerError> {
    let shares = names_one_account(&filter.accounts);
    let (first, last) = bucket_range(filter)?;
    let sessions = top_sessions_by_tokens(
        conn,
        first * 1_000,
        last * 1_000,
        SESSION_BURN_LIMIT,
        &filter.accounts,
    )?;
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_session: HashMap<String, Vec<BurnSums>> = sessions
        .iter()
        .map(|id| {
            (
                id.clone(),
                empty_burn_sums(first, last, filter.bucket_seconds),
            )
        })
        .collect();
    for a in read_session_attempts(conn, first * 1_000, last * 1_000, &sessions)? {
        if !filter.accounts.is_empty() && !filter.accounts.contains(&a.account_id) {
            continue;
        }
        let Some(series) = by_session.get_mut(&a.session_id) else {
            continue;
        };
        let bucket_at = a.at / 1_000;
        // The last bucket can reach past `last`, and an attempt there falls outside the range.
        if bucket_at >= last {
            continue;
        }
        let Some(point) = bucket_slot(series, first, filter.bucket_seconds, bucket_at) else {
            continue;
        };
        // `price_usage` leaves the point as it is for an attempt with no token, so its band is never read.
        if tokens_of(a.priced.usage) == 0 {
            continue;
        }
        let (key, usage) = a.priced.usage(prices, now);
        price_usage(point, prices, tables, key, usage, shares);
    }
    Ok(sessions
        .into_iter()
        .map(|id| {
            let series = by_session.remove(&id).unwrap_or_default();
            (id, series.into_iter().map(BurnSums::point).collect())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pricing::effective_instant;
    use crate::test_tables::{plan_of, plan_under};

    /// A pool alone binds no leading column the hour range would not serve better: the member index would walk the
    /// pool's whole retention before the hour filter, so the held time's hour range stays the outer search.
    #[test]
    fn the_pool_only_held_series_searches_the_hour_range_off_the_member_index() {
        let filter = SeriesFilter {
            pools: vec!["p".into()],
            ..SeriesFilter::default()
        };
        let (sql, binds) = held_series_statement(&filter, 0, 3_600);
        let plan = plan_of(&sql, binds);
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH rollups_hourly")
                    && step.contains("hour>?")
                    && step.contains("hour<?")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|step| step.contains("rollups_hourly_member")),
            "{plan:?}"
        );
    }

    /// At least two seeks of the requests go through the session index and no step walks the table, on a ledger whose
    /// statistics make the key index look selective: the named sessions are a chunk of the page's rows, and each costs
    /// the first rows of its own session.
    #[test]
    fn the_session_identities_read_seeks_the_session_index_for_each_named_session() {
        let sessions = vec![
            ("k".to_string(), 0, "s1".to_string()),
            ("k".to_string(), 0, "s2".to_string()),
        ];
        let seeks_the_session_index = |plan: &[String]| {
            // SQLite runs the `EXISTS` as a join and prints its seek as `SEARCH r EXISTS USING INDEX ...`.
            assert!(
                plan.iter()
                    .filter(|step| step.starts_with("SEARCH r ")
                        && step.contains(" USING INDEX requests_session "))
                    .count()
                    >= 2,
                "{plan:?}"
            );
            assert!(
                !plan
                    .iter()
                    .any(|step| step.starts_with("SCAN r") || step.contains("SCAN requests")),
                "a named session never walks the table: {plan:?}"
            );
        };
        let (sql, binds) = session_identities_statement(&sessions);
        seeks_the_session_index(&plan_under(
            &sql,
            binds,
            &[
                ("requests", "requests_key", "1000 1 1"),
                ("requests", "requests_session", "1000 1000 1"),
            ],
        ));
    }

    /// The session burn chart's two reads bound the request by its own `received_at` beside the attempt's start,
    /// which no index leads with, so the request side seeks the range rather than walking every retained session.
    #[test]
    fn the_session_burn_reads_search_the_requests_by_their_received_range() {
        // The requests' range is the outer loop and each of its requests looks its attempts up by id: an attempts index
        // leading the join would walk the whole request range once per attempt it found.
        let ranged = |plan: &[String]| {
            let mut loops = plan
                .iter()
                .filter(|step| step.starts_with("SEARCH") || step.starts_with("SCAN"));
            loops.next().is_some_and(|step| {
                step.starts_with("SEARCH r USING INDEX ")
                    && step.contains("received_at>?")
                    && step.contains("received_at<?")
            }) && loops.next().is_some_and(|step| {
                step.starts_with("SEARCH a USING ") && step.contains("request_id=?")
            }) && !plan.iter().any(|step| step.starts_with("SCAN"))
        };
        for accounts in [Vec::new(), vec!["a".to_string(), "b".to_string()]] {
            let (sql, binds) = top_sessions_statement(0, 3_600_000, 20, &accounts);
            let top = plan_of(&sql, binds);
            assert!(
                ranged(&top) && top[0].starts_with("SEARCH r USING INDEX requests_received "),
                "{accounts:?}: {top:?}"
            );
        }
        let (sql, binds) =
            session_attempts_statement(0, 3_600_000, &["s1".to_string(), "s2".to_string()]);
        let attempts = plan_of(&sql, binds);
        assert!(ranged(&attempts), "{attempts:?}");
    }

    /// The incremental read seeks the landing index and walks no table, on statistics that make the received index
    /// look selective: a poll reads the rows written since its watermark and nothing older.
    #[test]
    fn the_session_spend_read_since_a_landing_seeks_the_landing_index() {
        let (sql, binds) = session_spend_since_statement(1_000);
        let plan = plan_under(
            &sql,
            binds,
            &[
                ("requests", "requests_received", "1000 1"),
                ("requests", "requests_landed_at", "1000 1000"),
            ],
        );
        assert!(
            plan.iter().any(
                |s| s.starts_with("SEARCH r USING INDEX requests_landed_at ")
                    && s.contains("landed_at>?")
            ),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|step| step.starts_with("SCAN r") || step.contains("SCAN requests")),
            "{plan:?}"
        );
    }

    /// The first fill seeks the session index once per named session and walks no table, as the identities read does.
    #[test]
    fn the_session_spend_first_fill_seeks_the_session_index_for_each_named_session() {
        let sessions = vec![
            ("k".to_string(), 0, "s1".to_string()),
            ("k".to_string(), 0, "s2".to_string()),
        ];
        let (sql, binds) = session_spend_of_statement(&sessions);
        let plan = plan_under(
            &sql,
            binds,
            &[
                ("requests", "requests_key", "1000 1 1"),
                ("requests", "requests_session", "1000 1000 1"),
            ],
        );
        assert!(
            plan.iter()
                .filter(|s| s.starts_with("SEARCH r ") && s.contains("INDEX requests_session "))
                .count()
                >= 1,
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|step| step.starts_with("SCAN r") || step.contains("SCAN requests")),
            "{plan:?}"
        );
    }

    /// The fold takes a request's last attempt as the one that served it, so each request's attempts must reach it as
    /// one run in start order. Two requests of a session received at one instant tie on every first-fill order column
    /// before the request id. The writer stamps each landing above the last, so the incremental read's boundary is
    /// what can slip: a request landed at the watermark was folded by the poll that set it.
    #[test]
    fn the_session_spend_reads_keep_a_requests_attempts_one_run() {
        let db = LedgerDb::open_in_memory().unwrap();
        let request = |id: &str| RequestRecord {
            id: id.into(),
            received_at: 1_000,
            key_id: Some("k".into()),
            session_id: Some("s".into()),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            ..Default::default()
        };
        let attempt = |id: &str, attempt: u32, started_at: i64| AttemptRecord {
            request_id: id.into(),
            attempt,
            account_id: "a".into(),
            started_at,
            ..Default::default()
        };
        db.insert_request(
            &request("r1"),
            &[attempt("r1", 1, 100), attempt("r1", 2, 300)],
            None,
        )
        .unwrap();
        db.insert_request(&request("r2"), &[attempt("r2", 1, 200)], None)
            .unwrap();
        let runs = |rows: Vec<SessionSpendRow>| -> Vec<(String, Option<i64>)> {
            rows.into_iter()
                .map(|row| (row.request_id, row.attempt.map(|a| a.started_at)))
                .collect()
        };
        let sessions = [("k".to_string(), 0, "s".to_string())];
        let filled = db
            .read_blocking(|c| session_spend_rows_of(c, &sessions))
            .unwrap();
        assert_eq!(
            runs(filled),
            [
                ("r1".to_string(), Some(100)),
                ("r1".to_string(), Some(300)),
                ("r2".to_string(), Some(200)),
            ]
        );
        let since = db
            .read_blocking(|c| session_spend_rows_since(c, 1))
            .unwrap();
        assert_eq!(runs(since), [("r2".to_string(), Some(200))]);
    }

    #[test]
    fn the_newest_events_of_a_kind_are_read_by_seeking_to_the_kind() {
        let plan = plan_of(
            RECENT_EVENTS_OF_KIND,
            vec![
                rusqlite::types::Value::Text("model_rejected".into()),
                rusqlite::types::Value::Integer(50),
            ],
        );
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH events USING")
                    && step.contains("events_kind_at")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN")),
            "a poll never walks the whole history: {plan:?}"
        );
    }

    /// The plans of a dry run's sample page for `subject`, on the first page and on a page past its cursor.
    fn sample_page_plans(base: &str, subject: &str) -> [Vec<String>; 2] {
        use rusqlite::types::Value;
        let first = plan_of(
            &format!("{base}{GUARDRAIL_SAMPLE_ORDER_SQL}3"),
            vec![
                Value::from(subject.to_string()),
                Value::from(0_i64),
                Value::from(50_i64),
            ],
        );
        let later = plan_of(
            &format!("{base}{GUARDRAIL_SAMPLE_CURSOR_SQL}{GUARDRAIL_SAMPLE_ORDER_SQL}5"),
            vec![
                Value::from(subject.to_string()),
                Value::from(0_i64),
                Value::from(3_600_000_i64),
                Value::from("r1".to_string()),
                Value::from(50_i64),
            ],
        );
        [first, later]
    }

    /// Whether the plan's first seek goes through `index` and no step walks a whole table.
    fn seeks_through(plan: &[String], index: &str) -> bool {
        plan.iter()
            .find(|step| step.starts_with("SEARCH"))
            .is_some_and(|step| step.starts_with(&format!("SEARCH r USING INDEX {index} ")))
            && !plan.iter().any(|step| step.starts_with("SCAN"))
    }

    /// No index leads with `pool_id`, so a pool's page ranges over the received times from the window's start and
    /// checks the pool row by row, and a rewritten predicate that lost the range would walk every request.
    #[test]
    fn the_pool_sample_page_walks_requests_received() {
        for plan in sample_page_plans(GUARDRAIL_SAMPLE_POOL_SQL, "main") {
            assert!(seeks_through(&plan, "requests_received"), "{plan:?}");
        }
    }

    #[test]
    fn the_key_sample_page_seeks_requests_key() {
        for plan in sample_page_plans(GUARDRAIL_SAMPLE_KEY_SQL, "k1") {
            assert!(seeks_through(&plan, "requests_key"), "{plan:?}");
        }
    }

    /// Whether the plan seeks `table` through `index` on every column of `seek`, and no step walks `table` under its
    /// own name or under any of `aliases`. A `SCAN` step names the alias where the statement has one, and a correlated
    /// seek on an inner reference satisfies the seek half on its own, so both halves are checked against the
    /// statement's aliases and the columns the seek must bind.
    fn seeks_without_a_scan(
        plan: &[String],
        table: &str,
        aliases: &[&str],
        index: &str,
        seek: &[&str],
    ) -> bool {
        let walks = |name: &str| {
            plan.iter().any(|step| {
                *step == format!("SCAN {name}") || step.starts_with(&format!("SCAN {name} "))
            })
        };
        plan.iter().any(|step| {
            step.starts_with("SEARCH ")
                && step.contains(index)
                && seek.iter().all(|column| step.contains(column))
        }) && !walks(table)
            && !aliases.iter().any(|alias| walks(alias))
    }

    /// The seek's terms as `EXPLAIN QUERY PLAN` prints them: the account and the bucket's range.
    const EXTRA_USAGE_SEEK: &[&str] = &["account_id=?", "bucket>?"];

    // Negative controls: each walks its table under its alias with a correlated seek per row, which the helper must
    // reject before its verdict on a rewrite counts.
    const LATEST_EXTRA_USAGE_WALK: &str =
        "SELECT account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, \
         disabled_reason, overage_reset, source, spend_limit_minor, spend_used_minor, spend_percent, spend_currency, \
         spend_exponent FROM extra_usage_samples e WHERE bucket >= ?1 AND rowid = (SELECT rowid FROM \
         extra_usage_samples WHERE account_id = e.account_id AND bucket >= ?1 ORDER BY bucket DESC, source = 'head' \
         DESC LIMIT 1) ORDER BY account_id";
    const LATEST_EXTRA_USAGE_POLLED_WALK: &str =
        "SELECT account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, \
         disabled_reason, overage_reset, source, spend_limit_minor, spend_used_minor, spend_percent, spend_currency, \
         spend_exponent FROM extra_usage_samples e WHERE bucket >= ?1 AND source = 'poll' AND rowid = (SELECT rowid \
         FROM extra_usage_samples WHERE account_id = e.account_id AND bucket >= ?1 AND source = 'poll' ORDER BY bucket \
         DESC LIMIT 1) ORDER BY account_id";
    const LATEST_EXTRA_USAGE_SHARE_WALK: &str =
        "SELECT account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, \
         disabled_reason, overage_reset, source, spend_limit_minor, spend_used_minor, spend_percent, spend_currency, \
         spend_exponent FROM extra_usage_samples e WHERE bucket >= ?1 AND utilization IS NOT NULL AND rowid = (SELECT \
         rowid FROM extra_usage_samples WHERE account_id = e.account_id AND bucket >= ?1 AND utilization IS NOT NULL \
         ORDER BY bucket DESC, source = 'head' DESC LIMIT 1) ORDER BY account_id";

    #[test]
    fn the_latest_extra_usage_reads_seek_each_accounts_newest_row() {
        const PRIMARY_KEY: &str = "sqlite_autoindex_extra_usage_samples_1";
        for before in [
            LATEST_EXTRA_USAGE_WALK,
            LATEST_EXTRA_USAGE_POLLED_WALK,
            LATEST_EXTRA_USAGE_SHARE_WALK,
        ] {
            let plan = plan_of(before, vec![rusqlite::types::Value::from(0_i64)]);
            assert!(
                !seeks_without_a_scan(
                    &plan,
                    "extra_usage_samples",
                    &["e"],
                    PRIMARY_KEY,
                    EXTRA_USAGE_SEEK
                ),
                "the helper rejects the walk under its alias: {before}: {plan:?}"
            );
        }
        for sql in [
            LATEST_EXTRA_USAGE_SQL,
            LATEST_EXTRA_USAGE_POLLED_SQL,
            LATEST_EXTRA_USAGE_SHARE_SQL,
        ] {
            let plan = plan_of(sql, vec![rusqlite::types::Value::from(0_i64)]);
            assert!(
                seeks_without_a_scan(
                    &plan,
                    "extra_usage_samples",
                    &["e", "s"],
                    PRIMARY_KEY,
                    EXTRA_USAGE_SEEK
                ),
                "{sql}: {plan:?}"
            );
        }
    }

    /// 2026-01-15 00:00 UTC in seconds, inside the test cost model's effective range.
    const HOUR: i64 = 1_768_435_200;
    const TIER: &str = "default_claude_max_20x";

    /// A plan row's figure over its whole usage is the one `price_row` answered on its counts, so the group's sums
    /// price only its cache halves again.
    #[test]
    fn a_plan_rows_whole_equivalent_comes_from_its_priced_row_and_is_not_priced_again() {
        let tables = crate::test_tables::subscription_costs();
        let model = "claude-sonnet-5-5";
        let mut row = RollupRow::key(
            HOUR,
            "k",
            0,
            "p",
            "a",
            model,
            "inference",
            "-",
            "-",
            "-",
            "-",
            "standard",
            "plan",
            "standard",
            "-",
            "-",
        );
        row.requests = 1;
        row.input_tokens = 1_000;
        row.cache_read_tokens = 500;
        row.output_tokens = 100;
        let at = HOUR * 1_000;
        let entrypoint = entrypoint_of_client(&row.client);
        // A figure no pricing of the row's usage answers, so a sum that priced it again reads another one.
        let mut sums = EquivalentSums::default();
        sums.add_priced(&row, true, Some(0.0625), |priced| {
            price_equivalent(&tables, entrypoint, model, priced, at, at, at)
        });
        assert_eq!(
            sums.cost.figure(),
            Some(0.0625),
            "the whole usage is not priced again"
        );
    }

    /// No reader of a priced row reads its shares, since the shares a route answers come from the share fold, so
    /// `priced` prices no share window of a plan row whose account tier the cost model sizes, while the same row at its
    /// own key prices them.
    #[test]
    fn priced_prices_no_share_window_of_a_plan_row_on_a_sized_tier() {
        let prices = crate::test_tables::api_prices();
        let costs = crate::test_tables::subscription_costs();
        let mut row = RollupRow::key(
            HOUR,
            "k",
            0,
            "p",
            "a",
            "claude-opus-5-5",
            "inference",
            "-",
            "-",
            "-",
            "-",
            "standard",
            "plan",
            "standard",
            TIER,
            "-",
        );
        row.requests = 1;
        row.input_tokens = 1_000;
        row.output_tokens = 100;
        let own_key = price_row(&prices, &costs, &row.price_key(HOUR * 1_000), row.usage());
        assert!(
            own_key.shares.is_some(),
            "the tier is sized, so the row's own key prices its windows: {own_key:?}"
        );
        let read = priced(&row, &prices, &costs, HOUR * 1_000);
        assert_eq!(read.shares, None, "{read:?}");
        assert_eq!(
            (&read.usd, &read.equivalent),
            (&own_key.usd, &own_key.equivalent),
            "the dollars and the equivalent stand"
        );
    }

    /// The plan sums fold the figure the row's priced figures carry, so a figure no pricing of the row's usage answers
    /// reads back from the group's sums. A fold that priced the usage again would read the same figure the priced row
    /// holds in production, which leaves only this sentinel to tell the two apart.
    #[test]
    fn the_plan_sums_fold_the_priced_rows_equivalent_and_never_price_its_usage_again() {
        let costs = crate::test_tables::subscription_costs();
        let model = "claude-sonnet-5-5";
        let mut row = RollupRow::key(
            HOUR,
            "k",
            0,
            "p",
            "a",
            model,
            "inference",
            "-",
            "-",
            "-",
            "-",
            "standard",
            "plan",
            "standard",
            "-",
            "-",
        );
        row.requests = 1;
        row.input_tokens = 1_000;
        row.output_tokens = 100;
        let sentinel = 0.375;
        let at = HOUR * 1_000;
        let repriced = price_equivalent(
            &costs,
            entrypoint_of_client(&row.client),
            model,
            row.usage(),
            at,
            at,
            at,
        );
        assert_ne!(
            repriced,
            Some(sentinel),
            "the sentinel is a figure the pricer does not answer for this usage"
        );
        let mut plan = PlanSums::default();
        plan.add(
            &row,
            &row.price_key(at),
            Priced {
                equivalent: Some(sentinel),
                ..Priced::default()
            },
            &costs,
            true,
        );
        assert_eq!(
            plan.equivalent.cost.figure(),
            Some(sentinel),
            "the sums read the priced row's figure"
        );
    }

    /// The hour of a date's midnight, in seconds.
    fn hour_at(date: &str) -> i64 {
        effective_instant(date).expect("a date") / 1_000
    }

    /// The price fixture's two entries take effect on 2026-01-01 and 2026-02-01, so a range across February splits
    /// there and nowhere else: before the first entry `in_force` already reads the first, so its date is no edge. A
    /// span that starts before the earliest entry prices at that entry's instant.
    #[test]
    fn revision_spans_split_a_range_where_the_price_revision_in_force_changes() {
        let prices = crate::test_tables::api_prices();
        let (from, january, february, to) = (
            hour_at("2025-12-15"),
            hour_at("2026-01-01"),
            hour_at("2026-02-01"),
            hour_at("2026-03-01"),
        );
        assert_eq!(
            revision_spans(&prices, None, from, to),
            [
                Span {
                    from_hour: from,
                    to_hour: february,
                    at: january * 1_000
                },
                Span {
                    from_hour: february,
                    to_hour: to,
                    at: february * 1_000
                },
            ],
            "two spans meeting at the second entry's hour"
        );
        let (inside_from, inside_to) = (hour_at("2026-02-02"), hour_at("2026-02-20"));
        assert_eq!(
            revision_spans(&prices, None, inside_from, inside_to),
            [Span {
                from_hour: inside_from,
                to_hour: inside_to,
                at: inside_from * 1_000
            }],
            "one span when no revision starts inside the range"
        );
    }

    /// The cost list's dates split too: a cost entry dated 2026-01-15 splits the January span in two.
    #[test]
    fn revision_spans_split_at_the_cost_lists_dates_when_given() {
        let prices = crate::test_tables::api_prices();
        let first = crate::test_tables::first_entry_costs_text();
        let (_, entry) = first
            .split_once("revisions:\n")
            .expect("the fixture opens with its list");
        let costs = CostRevisions::from_yaml(&format!(
            "{first}{}",
            entry.replacen("effective: 2026-01-01", "effective: 2026-01-15", 1)
        ))
        .expect("the cost list with a mid-January entry parses");
        let (from, mid_january, february, to) = (
            hour_at("2026-01-02"),
            hour_at("2026-01-15"),
            hour_at("2026-02-01"),
            hour_at("2026-03-01"),
        );
        let bounds: Vec<(i64, i64)> = revision_spans(&prices, Some(&costs), from, to)
            .into_iter()
            .map(|span| (span.from_hour, span.to_hour))
            .collect();
        assert_eq!(
            bounds,
            [(from, mid_january), (mid_january, february), (february, to)]
        );
    }

    /// A lifetime read passes `i64::MIN`: the first span keeps the sentinel as its SQL bound, so `hour >= ?` still
    /// clears every row, and prices at the earliest instant of either list, so nothing multiplies the sentinel.
    #[test]
    fn a_lifetime_span_keeps_the_sentinel_bound_and_prices_at_the_earliest_entry() {
        let prices = crate::test_tables::api_prices();
        let costs = crate::test_tables::subscription_costs();
        let to = hour_at("2026-03-01");
        let spans = revision_spans(&prices, Some(&costs.revisions), i64::MIN, to);
        assert_eq!(
            (spans[0].from_hour, spans[0].at),
            (i64::MIN, hour_at("2026-01-01") * 1_000)
        );
        assert_eq!(spans.last().map(|span| span.to_hour), Some(to));
    }

    /// A group of rows on a model the revision lacks prices at nothing, and its dollars start from a positive zero: a
    /// signed zero on the wire is read back as -$0.00.
    #[test]
    fn a_group_of_unpriced_rows_answers_an_unsigned_zero() {
        let mut row = RollupRow::key(
            HOUR,
            "k",
            0,
            "p",
            "a",
            "gpt-5",
            "inference",
            "-",
            "-",
            "-",
            "-",
            "standard",
            "plan",
            "standard",
            "-",
            "-",
        );
        row.requests = 1;
        row.input_tokens = 10;
        let groups = breakdown(
            &[row],
            GroupBy::All,
            false,
            &LiveKeys::default(),
            &[],
            &crate::test_tables::api_prices(),
            &crate::test_tables::subscription_costs(),
            HOUR * 1_000,
        );
        assert_eq!(serde_json::to_string(&groups[0].cost_usd).unwrap(), "0.0");
    }

    #[test]
    fn the_sql_folds_agree_with_the_rust_folds_on_every_branch() {
        use crate::rollup::{fold_effort, fold_identifier};
        let conn = Connection::open_in_memory().unwrap();
        let fold = |expr: &str, sample: Option<&str>| -> String {
            conn.query_row(&format!("SELECT {expr}"), params![sample], |r| r.get(0))
                .unwrap()
        };
        let effort = effort_fold_sql("?1");
        for sample in [
            None,
            Some("high"),
            Some("none"),
            Some("HIGH"),
            Some("ultra"),
        ] {
            assert_eq!(fold(&effort, sample), fold_effort(sample), "{sample:?}");
        }
        let class = class_fold_sql("?1");
        let (bound, past) = ("x".repeat(64), "x".repeat(65));
        for sample in [
            None,
            Some(""),
            Some("main"),
            Some("a/b:c.d_e-f"),
            Some("bad class"),
            Some(bound.as_str()),
            Some(past.as_str()),
            Some("caf\u{e9}"),
        ] {
            let rust = sample.map_or_else(|| NO_CLASS.to_string(), fold_identifier);
            assert_eq!(fold(&class, sample), rust, "{sample:?}");
        }
    }
}
