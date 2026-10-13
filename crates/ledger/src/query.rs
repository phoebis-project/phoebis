//! Reads for the admin routes: the request log with its filters and cursor, the rollup series and breakdown
//! with merged histograms, the dashboard's day, and the newest quota sample per window.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use indexmap::IndexMap;
use phoebis_routing::{MemberSpend, PinUsage, FIVE_HOUR, REFRESHING_CLASSES, SEVEN_DAY};
use phoebis_store::{AccountKind, BudgetPeriod};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{row_to_attempt, row_to_request, LedgerDb, ATTEMPT_SELECT, REQUEST_SELECT};
use crate::histogram::Histogram;
use crate::plan::{funding_of, price_shares, Funding, PlanTables, WindowShares};
use crate::pricing::{PriceTable, Speed};
use crate::record::{
    AttemptRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord, RequestRecord,
};
#[cfg(any(test, feature = "test-support"))]
use crate::rollup::read_rollups;
use crate::rollup::{
    hour_of, read_account_rollups, read_rollups_paged, KeySelector, RollupRow, RollupSelect,
    DIMENSION_MAX_BYTES, EFFORTS, NO_CLASS, NO_EFFORT, OTHER_DIMENSION, PLAN_FUNDING, UNATTRIBUTED,
};
use crate::LedgerError;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

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

/// The widest custom range: rollups are kept for 400 days, so a wider one reads nothing more and would only
/// size the chart.
pub const MAX_RANGE_MS: i64 = 400 * DAY_MS;

/// The most points one series or traffic answer may carry; every allowed range at its bucket size stays
/// under this, so the check is a guard against arithmetic, not a limit a caller meets.
pub const MAX_POINTS: i64 = 10_000;

/// The first hour boundary at or after `at_ms`, in seconds: the exclusive upper bound of a range in hours.
pub fn hour_ceil(at_ms: i64) -> i64 {
    let seconds = at_ms.div_euclid(1_000);
    let floor = seconds.div_euclid(3_600) * 3_600;
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

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RequestFilter {
    pub text: Option<String>,
    /// Keep the one request whose id equals the value, literally.
    pub request: Option<String>,
    /// Keep the requests whose session id equals the value, literally.
    pub session: Option<String>,
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
    pub user_agent: Option<String>,
    /// A pattern under the wildcard grammar over the row's `client`; a row with none matches `-`.
    pub client: Option<String>,
    pub agent_types: Vec<String>,
    pub turn_origins: Vec<String>,
    /// `fast`, `standard`: the serving attempt's `speed`, else `fast` for a request that asked for it and
    /// `standard` otherwise.
    pub speeds: Vec<String>,
    /// Keep the requests that waited for a slot (`held` on the request row).
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
}

fn in_list(
    column: &str,
    values: &[String],
    args: &mut Vec<Box<dyn ToSql>>,
    clauses: &mut Vec<String>,
) {
    if values.is_empty() {
        return;
    }
    let marks: Vec<String> = values
        .iter()
        .map(|v| {
            args.push(Box::new(v.clone()));
            format!("?{}", args.len())
        })
        .collect();
    clauses.push(format!("{column} IN ({})", marks.join(", ")));
}

fn bind_all(values: &[String], args: &mut Vec<Box<dyn ToSql>>) -> String {
    values
        .iter()
        .map(|v| {
            args.push(Box::new(v.clone()));
            format!("?{}", args.len())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `column` folded as `fold_effort` folds a request's effort, in SQL.
pub(crate) fn effort_fold_sql(column: &str) -> String {
    let levels = EFFORTS
        .iter()
        .map(|l| format!("'{l}'"))
        .collect::<Vec<_>>()
        .join(", ");
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
    args: &mut Vec<Box<dyn ToSql>>,
    clauses: &mut Vec<String>,
) {
    if !values.iter().any(|v| v == UNATTRIBUTED) {
        return in_list(column, values, args, clauses);
    }
    let mut inner = Vec::new();
    in_list(column, values, args, &mut inner);
    clauses.push(format!("({} OR {column} IS NULL)", inner.remove(0)));
}

/// The request log reads a `keys` selector as the analytics do: a bare id is the live generation,
/// `<id>@<generation>` that one, and the reserved id reaches the NULL it stands for. A bare id no live key holds
/// selects nothing, as `LiveKeys::selects` answers it, so a filter made of such ids answers no row.
fn key_lifetimes(
    selectors: &[String],
    live: &LiveKeys,
    args: &mut Vec<Box<dyn ToSql>>,
    clauses: &mut Vec<String>,
) {
    if selectors.is_empty() {
        return;
    }
    let mut parts = Vec::new();
    for selector in selectors {
        if selector == UNATTRIBUTED {
            args.push(Box::new(selector.clone()));
            parts.push(format!("key_id = ?{} OR key_id IS NULL", args.len()));
            continue;
        }
        let Some((id, generation)) = live.lifetime_of(selector) else {
            continue;
        };
        args.push(Box::new(id.to_string()));
        let at_id = args.len();
        args.push(Box::new(generation));
        parts.push(format!(
            "(key_id = ?{at_id} AND key_generation = ?{})",
            at_id + 1
        ));
    }
    clauses.push(if parts.is_empty() {
        "0".into()
    } else {
        format!("({})", parts.join(" OR "))
    });
}

/// `text` with `\`, `%` and `_` escaped for `LIKE .. ESCAPE '\'`, so every typed character matches itself.
pub fn literal_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// One clause matching any of `patterns` against `column` under the wildcard grammar of `crate::pattern`;
/// `column` reads as `-` where it is NULL, as the rollup fold already does. No clause when `patterns` is empty.
fn pattern_any(
    column: &str,
    patterns: &[String],
    args: &mut Vec<Box<dyn ToSql>>,
    clauses: &mut Vec<String>,
) {
    if patterns.is_empty() {
        return;
    }
    let parts: Vec<String> = patterns
        .iter()
        .map(|p| {
            let (like, escape) = crate::pattern::to_like(p);
            args.push(Box::new(like));
            let at_like = args.len();
            args.push(Box::new(escape.to_string()));
            format!(
                "COALESCE({column}, '-') LIKE ?{at_like} ESCAPE ?{}",
                at_like + 1
            )
        })
        .collect();
    clauses.push(format!("({})", parts.join(" OR ")));
}

/// The request log's key filter reads the selectors the analytics do, against the row's `(key_id,
/// key_generation)`; `live` says which generation a bare id names.
pub fn list_requests(
    conn: &Connection,
    filter: &RequestFilter,
    live: &LiveKeys,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<RequestWithAttempts>, Option<String>), LedgerError> {
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if let Some(text) = filter.text.as_deref().filter(|t| !t.is_empty()) {
        args.push(Box::new(format!("%{}%", literal_like(text))));
        let n = args.len();
        clauses.push(format!("(id LIKE ?{n} ESCAPE '\\' OR session_id LIKE ?{n} ESCAPE '\\' OR key_id LIKE ?{n} ESCAPE '\\' OR model LIKE ?{n} ESCAPE '\\' OR error_code LIKE ?{n} ESCAPE '\\' OR EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id AND (a.account_id LIKE ?{n} ESCAPE '\\' OR a.upstream_request_id LIKE ?{n} ESCAPE '\\' OR a.upstream_message_id LIKE ?{n} ESCAPE '\\')))"));
    }
    if let Some(request) = filter.request.as_deref().filter(|r| !r.is_empty()) {
        args.push(Box::new(request.to_string()));
        clauses.push(format!("id = ?{}", args.len()));
    }
    if let Some(session) = filter.session.as_deref().filter(|s| !s.is_empty()) {
        args.push(Box::new(session.to_string()));
        clauses.push(format!("session_id = ?{}", args.len()));
    }
    key_lifetimes(&filter.keys, live, &mut args, &mut clauses);
    in_list_or_null("pool_id", &filter.pools, &mut args, &mut clauses);
    in_list(
        "canonical_model",
        &filter
            .models
            .iter()
            .map(|m| m.to_ascii_lowercase())
            .collect::<Vec<_>>(),
        &mut args,
        &mut clauses,
    );
    in_list("kind", &filter.kinds, &mut args, &mut clauses);
    in_list("outcome", &filter.outcomes, &mut args, &mut clauses);
    in_list(
        "status",
        &filter
            .statuses
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        &mut args,
        &mut clauses,
    );
    if !filter.accounts.is_empty() {
        let marks = bind_all(&filter.accounts, &mut args);
        // The serving attempt is the last, the account the rollups file a request under, so a request that failed
        // over lists under the account that answered it. A request no attempt served reads as the reserved id.
        clauses.push(format!(
            "COALESCE((SELECT a.account_id FROM attempts a WHERE a.request_id = requests.id \
             ORDER BY a.attempt DESC LIMIT 1), '{UNATTRIBUTED}') IN ({marks})"
        ));
    }
    if !filter.speeds.is_empty() {
        let marks: Vec<String> = filter
            .speeds
            .iter()
            .map(|v| {
                args.push(Box::new(v.clone()));
                format!("?{}", args.len())
            })
            .collect();
        // The serving attempt is the last: an earlier 429 carries no speed and must not read as standard. A
        // request with no served speed reads as the rollups fold it.
        clauses.push(format!(
            "COALESCE((SELECT a.speed FROM attempts a WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1), CASE WHEN requests.requested_speed = 'fast' THEN 'fast' ELSE 'standard' END) IN ({})",
            marks.join(", ")
        ));
    }
    if !filter.cache_miss_reasons.is_empty() {
        let marks = bind_all(&filter.cache_miss_reasons, &mut args);
        // The serving attempt is the last, the one the rollups count a request's reason on. A request with no
        // attempt or a null serving reason matches nothing.
        clauses.push(format!(
            "(SELECT a.cache_miss_reason FROM attempts a WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1) IN ({marks})"
        ));
    }
    if !filter.efforts.is_empty() {
        let marks = bind_all(&filter.efforts, &mut args);
        clauses.push(format!(
            "{} IN ({marks})",
            effort_fold_sql("requests.effort")
        ));
    }
    if !filter.request_classes.is_empty() {
        let marks = bind_all(&filter.request_classes, &mut args);
        clauses.push(format!(
            "{} IN ({marks})",
            class_fold_sql("requests.request_class")
        ));
    }
    if filter.held {
        clauses.push("requests.held IS NOT NULL".into());
    }
    // The rollups and the console's fold already read an absent agent or client as `-`; `pattern_any`'s
    // coalesce keeps a nullable column from excluding such a row from both the wildcard and the exact marker.
    pattern_any(
        "user_agent",
        &Vec::from_iter(
            filter
                .user_agent
                .as_deref()
                .filter(|a| !a.is_empty())
                .map(String::from),
        ),
        &mut args,
        &mut clauses,
    );
    pattern_any(
        "client",
        &Vec::from_iter(
            filter
                .client
                .as_deref()
                .filter(|c| !c.is_empty())
                .map(String::from),
        ),
        &mut args,
        &mut clauses,
    );
    in_list("agent_type", &filter.agent_types, &mut args, &mut clauses);
    in_list("turn_origin", &filter.turn_origins, &mut args, &mut clauses);
    if !filter.include_probes && !filter.kinds.iter().any(|k| k == "probe") {
        clauses.push("kind != 'probe'".into());
    }
    if let Some(from) = filter.from {
        args.push(Box::new(from));
        clauses.push(format!("received_at >= ?{}", args.len()));
    }
    if let Some(to) = filter.to {
        args.push(Box::new(to));
        clauses.push(format!("received_at <= ?{}", args.len()));
    }
    if let Some(cursor) = cursor {
        let (at, id) = cursor
            .split_once(':')
            .ok_or_else(|| LedgerError::Yaml("cursor is <received_at>:<id>".into()))?;
        let at: i64 = at
            .parse()
            .map_err(|_| LedgerError::Yaml("cursor is <received_at>:<id>".into()))?;
        args.push(Box::new(at));
        let n = args.len();
        args.push(Box::new(id.to_string()));
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
    args.push(Box::new((limit + 1) as i64));
    let sql = format!(
        "{REQUEST_SELECT}{where} ORDER BY received_at DESC, id DESC LIMIT ?{}",
        args.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut requests = stmt
        .query_map(
            params_from_iter(args.iter().map(|a| a.as_ref())),
            row_to_request,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = requests.len() > limit;
    requests.truncate(limit);
    let mut out = Vec::with_capacity(requests.len());
    let mut attempts_stmt = conn.prepare(&format!(
        "{ATTEMPT_SELECT} WHERE request_id = ?1 ORDER BY attempt"
    ))?;
    for request in requests {
        let attempts = attempts_stmt
            .query_map(params![request.id], row_to_attempt)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push((request, attempts));
    }
    let next = more
        .then(|| {
            out.last()
                .map(|(r, _)| format!("{}:{}", r.received_at, r.id))
        })
        .flatten();
    Ok((out, next))
}

/// The same two queries `LedgerDb::request` runs, over a connection the caller already holds.
pub fn request_by_id(
    conn: &Connection,
    id: &str,
) -> Result<Option<RequestWithAttempts>, LedgerError> {
    let request = conn
        .query_row(
            &format!("{REQUEST_SELECT} WHERE id = ?1"),
            params![id],
            row_to_request,
        )
        .optional()?;
    let Some(request) = request else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(&format!(
        "{ATTEMPT_SELECT} WHERE request_id = ?1 ORDER BY attempt"
    ))?;
    let attempts = stmt
        .query_map(params![id], row_to_attempt)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some((request, attempts)))
}

/// The events of the request's window under its own session, and
/// every event that names the request itself.
pub fn request_events(
    conn: &Connection,
    request: &RequestRecord,
) -> Result<Vec<EventRecord>, LedgerError> {
    let to = request.finished_at.unwrap_or(i64::MAX);
    let marker = format!("%\"source_request_id\":\"{}\"%", request.id);
    let mut stmt = conn.prepare("SELECT at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id FROM events WHERE at >= ?1 AND at <= ?2 AND ((session_id IS NOT NULL AND session_id = ?3) OR detail LIKE ?4) ORDER BY at, rowid")?;
    let rows = stmt.query_map(
        params![request.received_at, to, request.session_id, marker],
        |r| {
            Ok(EventRecord {
                at: r.get(0)?,
                kind: r.get(1)?,
                account_id: r.get(2)?,
                key_id: r.get(3)?,
                session_id: r.get(4)?,
                model: r.get(5)?,
                actor: r.get(6)?,
                detail: r.get(7)?,
                pool_id: r.get(8)?,
                agent_id: r.get(9)?,
                request_id: r.get(10)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const RECENT_EVENTS_OF_KIND: &str = "SELECT at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id FROM events WHERE kind = ?1 ORDER BY at DESC, rowid DESC LIMIT ?2";

/// The newest `limit` events of one kind, newest first.
pub fn recent_events_of_kind(
    conn: &Connection,
    kind: &str,
    limit: usize,
) -> Result<Vec<EventRecord>, LedgerError> {
    let mut stmt = conn.prepare(RECENT_EVENTS_OF_KIND)?;
    let rows = stmt.query_map(params![kind, limit as i64], |r| {
        Ok(EventRecord {
            at: r.get(0)?,
            kind: r.get(1)?,
            account_id: r.get(2)?,
            key_id: r.get(3)?,
            session_id: r.get(4)?,
            model: r.get(5)?,
            actor: r.get(6)?,
            detail: r.get(7)?,
            pool_id: r.get(8)?,
            agent_id: r.get(9)?,
            request_id: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const RECENT_GAPS: &str = "SELECT at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id FROM events WHERE kind = 'ledger_gap' AND at >= ?1 ORDER BY at DESC LIMIT ?2";

/// The newest `limit` `ledger_gap` events at or after `since`, newest first: what the system page lists beside the
/// writer's gap counter.
pub fn recent_gaps(
    conn: &Connection,
    since: i64,
    limit: usize,
) -> Result<Vec<EventRecord>, LedgerError> {
    let mut stmt = conn.prepare(RECENT_GAPS)?;
    let rows = stmt.query_map(params![since, limit as i64], |r| {
        Ok(EventRecord {
            at: r.get(0)?,
            kind: r.get(1)?,
            account_id: r.get(2)?,
            key_id: r.get(3)?,
            session_id: r.get(4)?,
            model: r.get(5)?,
            actor: r.get(6)?,
            detail: r.get(7)?,
            pool_id: r.get(8)?,
            agent_id: r.get(9)?,
            request_id: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every event kind the ledger writes, alphabetical: the `kinds` key is validated against it and the decision
/// kinds, and a 400 names a kind outside both.
pub const EVENT_KINDS: &[&str] = &[
    "account_state_changed",
    "budget_period_ended",
    "budget_reached",
    "catalog_changed",
    "cooldown_cleared",
    "cooldown_set",
    "fast_credits_changed",
    "ledger_gap",
    "model_permitted",
    "model_rejected",
    "overage_ended",
    "overage_started",
    "pin_created",
    "pin_expired",
    "pin_migrated",
    "pin_released",
    "poll_backoff_ended",
    "poll_backoff_started",
    "refresh_failed",
    "refresh_succeeded",
    "refresh_uncertain",
    "registry_changed",
    "router_started",
    "shedding_round",
    "warm_prefix_expired",
    "wind_down_started",
    "window_reset",
    "window_spent",
];

pub const DECISION_KINDS: &[&str] = &["exhausted", "held", "moved", "placed", "refused", "renewed"];

/// The pseudo-kind that names a probe's decision of any kind in `kinds`.
pub const PROBE_KIND: &str = "probe";

/// The decision kinds that sent the request upstream, and so have an attempt that answered for them: the same three
/// as `COMPLETING_ATTEMPT` names.
const DISPATCHING_KINDS: &[&str] = &["moved", "placed", "renewed"];

#[derive(Clone, Debug, Default)]
pub struct RoutingLogFilter {
    pub from: i64,
    pub to: i64,
    pub request: Option<String>,
    /// A routing session id or its prefix.
    pub session: Option<String>,
    pub keys: Vec<String>,
    pub pools: Vec<String>,
    pub accounts: Vec<String>,
    pub models: Vec<String>,
    /// `None` is every decision and event kind, the probe decisions among them. `Some` is exactly the kinds named,
    /// `probe` the pseudo-kind for a probe decision of any kind.
    pub kinds: Option<Vec<String>>,
}

/// `<at>:<rank>:<id>`: rank 1 for a decision (`id` is `<request_id>/<seq>`), 0 for an event (`id` its rowid).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingLogCursor {
    pub at: i64,
    pub rank: u8,
    pub request_id: Option<String>,
    pub seq: Option<u32>,
    pub rowid: Option<i64>,
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
                Some(RoutingLogCursor {
                    at,
                    rank: 1,
                    request_id: Some(request_id.to_string()),
                    seq: Some(seq.parse().ok()?),
                    rowid: None,
                })
            }
            "0" => Some(RoutingLogCursor {
                at,
                rank: 0,
                request_id: None,
                seq: None,
                rowid: Some(id.parse().ok()?),
            }),
            _ => None,
        }
    }

    pub fn encode(&self) -> String {
        match (&self.request_id, self.seq, self.rowid) {
            (Some(request_id), Some(seq), _) => format!("{}:1:{request_id}/{seq}", self.at),
            (_, _, rowid) => format!("{}:0:{}", self.at, rowid.unwrap_or_default()),
        }
    }

    fn of(row: &RoutingLogRow) -> RoutingLogCursor {
        match row {
            RoutingLogRow::Decision(d) => RoutingLogCursor {
                at: d.at,
                rank: 1,
                request_id: Some(d.request_id.clone()),
                seq: Some(d.seq),
                rowid: None,
            },
            RoutingLogRow::Event(e) => RoutingLogCursor {
                at: e.at,
                rank: 0,
                request_id: None,
                seq: None,
                rowid: Some(e.id),
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
const COMPLETING_ATTEMPT: &str = "CASE WHEN d.kind IN ('placed', 'renewed', 'moved') THEN (SELECT MAX(x.attempt) FROM attempts x WHERE x.request_id = d.request_id AND x.account_id = d.account_id AND x.attempt >= d.attempt AND x.attempt < COALESCE((SELECT MIN(n.attempt) FROM decisions n WHERE n.request_id = d.request_id AND n.seq > d.seq), 9223372036854775807)) END";

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
struct LogStatement {
    clauses: Vec<String>,
    args: Vec<Box<dyn ToSql>>,
}

impl LogStatement {
    fn new(alias: &str, filter: &RoutingLogFilter) -> LogStatement {
        let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(filter.from), Box::new(filter.to)];
        let mut clauses = vec![format!("{alias}.at BETWEEN ?1 AND ?2")];
        if let Some(request) = &filter.request {
            args.push(Box::new(request.clone()));
            clauses.push(format!("{alias}.request_id = ?{}", args.len()));
        }
        if let Some(session) = &filter.session {
            args.push(Box::new(session.clone()));
            let exact = args.len();
            args.push(Box::new(format!("{}%", literal_like(session))));
            clauses.push(format!(
                "({alias}.session_id = ?{exact} OR {alias}.session_id LIKE ?{} ESCAPE '\\')",
                exact + 1
            ));
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

    fn params(&self) -> impl Iterator<Item = &dyn ToSql> {
        self.args.iter().map(|a| a.as_ref())
    }
}

/// `None` when the kinds name no decision, so the statement is never run.
fn decision_statement(filter: &RoutingLogFilter) -> Option<LogStatement> {
    let mut statement = LogStatement::new("d", filter);
    if !filter.accounts.is_empty() {
        let mut marks = Vec::new();
        for account in &filter.accounts {
            statement.args.push(Box::new(account.clone()));
            marks.push(format!("?{}", statement.args.len()));
        }
        let marks = marks.join(", ");
        statement.clauses.push(format!(
            "(d.account_id IN ({marks}) OR d.from_account_id IN ({marks}))"
        ));
    }
    if let Some(kinds) = &filter.kinds {
        let named: Vec<String> = kinds
            .iter()
            .filter(|k| DECISION_KINDS.contains(&k.as_str()))
            .cloned()
            .collect();
        let probes = kinds.iter().any(|k| k == PROBE_KIND);
        let mut of_kind = Vec::new();
        in_list("d.kind", &named, &mut statement.args, &mut of_kind);
        let clause = match (of_kind.pop(), probes) {
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
        let named: Vec<String> = kinds
            .iter()
            .filter(|k| EVENT_KINDS.contains(&k.as_str()))
            .cloned()
            .collect();
        if named.is_empty() {
            return None;
        }
        in_list(
            "e.kind",
            &named,
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
    // One row past the page says a next page exists, so a page of exactly `limit` rows offers no cursor that would
    // only lead to an empty page.
    let next_cursor = if rows.len() > limit {
        limit
            .checked_sub(1)
            .and_then(|last| rows.get(last))
            .map(|row| RoutingLogCursor::of(row).encode())
    } else {
        None
    };
    rows.truncate(limit);
    Ok(RoutingLogPage {
        rows,
        next_cursor,
        counts,
    })
}

/// Up to `page` decisions past `cursor`, each with the facts of the attempt that completed it.
fn decision_rows(
    conn: &Connection,
    mut statement: LogStatement,
    cursor: Option<&RoutingLogCursor>,
    page: i64,
) -> Result<Vec<DecisionListRow>, LedgerError> {
    if let Some(cursor) = cursor {
        let n = statement.args.len() + 1;
        statement.args.push(Box::new(cursor.at));
        statement.args.push(Box::new(i64::from(cursor.rank)));
        statement.args.push(Box::new(cursor.request_id.clone()));
        statement.args.push(Box::new(cursor.seq));
        statement.clauses.push(format!(
            "(d.at < ?{n} OR (d.at = ?{n} AND ?{} = 1 AND (d.request_id < ?{} OR (d.request_id = ?{} AND d.seq < ?{}))))",
            n + 1,
            n + 2,
            n + 2,
            n + 3
        ));
    }
    statement.args.push(Box::new(page));
    let sql = format!(
        "SELECT d.request_id, d.seq, d.attempt, d.at, d.kind, d.probe, d.key_id, d.pool_id, d.session_id, \
         d.agent_id, d.model, d.account_id, d.from_account_id, d.summary, r.attempts, a.upstream_status, \
         a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens \
         FROM decisions d LEFT JOIN requests r ON r.id = d.request_id \
         LEFT JOIN attempts a ON a.request_id = d.request_id AND a.attempt = ({COMPLETING_ATTEMPT}) \
         WHERE {} ORDER BY d.at DESC, d.request_id DESC, d.seq DESC LIMIT ?{}",
        statement.sql(),
        statement.args.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(statement.params()), |r| {
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
    if let Some(cursor) = cursor {
        let n = statement.args.len() + 1;
        statement.args.push(Box::new(cursor.at));
        statement.args.push(Box::new(i64::from(cursor.rank)));
        statement.args.push(Box::new(cursor.rowid));
        statement.clauses.push(format!(
            "(e.at < ?{n} OR (e.at = ?{n} AND (?{} = 1 OR e.rowid < ?{})))",
            n + 1,
            n + 2
        ));
    }
    statement.args.push(Box::new(page));
    let sql = format!(
        "SELECT e.rowid, e.at, e.kind, e.account_id, e.key_id, e.pool_id, e.session_id, e.agent_id, e.model, \
         e.request_id, e.actor, e.detail FROM events e WHERE {} ORDER BY e.at DESC, e.rowid DESC LIMIT ?{}",
        statement.sql(),
        statement.args.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(statement.params()), |r| {
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
            detail: r
                .get::<_, Option<String>>(11)?
                .and_then(|d| serde_json::from_str(&d).ok())
                .unwrap_or(Value::Null),
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
        params_from_iter(statement.params()),
        |r| r.get(0),
    )?;
    Ok(count as u64)
}

/// Record v1 decoded from its frame with `outcome.completion` merged in, or `None` when the row is absent.
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
                 LEFT JOIN attempts a ON a.request_id = d.request_id AND a.attempt = ({COMPLETING_ATTEMPT}) \
                 WHERE d.request_id = ?1 AND d.seq = ?2"
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
        let held: Option<Value> = held.as_deref().and_then(|h| serde_json::from_str(h).ok());
        // A wait that ran out puts its account among the attempted, so a dispatch to an account the request waited on
        // is the one its freed slot took.
        let freed = held.as_ref().is_some_and(|h| {
            h["waited_on"]
                .as_array()
                .is_some_and(|waited| waited.iter().any(|w| w.as_str() == account_id.as_deref()))
        });
        serde_json::json!({
            "status": answered.status,
            "cache_read_tokens": cache.as_ref().map(|c| c.read_tokens),
            "input_tokens": cache.as_ref().map(|c| c.input_tokens),
            "promised_tokens": summary["rung"]["prefix_tokens"].as_u64(),
            "hit": cache.as_ref().map(|c| c.hit),
            "held_ms": held.as_ref().map_or(Value::Null, |h| h["held_ms"].clone()),
            "freed": freed,
            "cache_map_written": cache_map.as_deref().and_then(|m| serde_json::from_str::<Value>(m).ok()),
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

/// The `pin_migrated` events in `[from, to)` for the given accounts, counted by the move's `detail.reason`
/// (`unknown` when the event named none). Reads the events table's `(kind, at)` index directly rather than an
/// hourly rollup bucket, so a pool card's "last hour" figure is the last hour exactly. An empty `accounts` list
/// answers an empty map, unlike `SeriesFilter`'s scopes; a memberless pool skips the read outright rather than
/// spend one on this.
pub fn pin_migrations_by_reason(
    conn: &Connection,
    from: i64,
    to: i64,
    accounts: &[String],
) -> Result<BTreeMap<String, u64>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT account_id, detail FROM events WHERE kind = 'pin_migrated' AND at >= ?1 AND at < ?2",
    )?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (account_id, detail) = row?;
        if !account_id.is_some_and(|id| accounts.contains(&id)) {
            continue;
        }
        let parsed: Value = detail
            .as_deref()
            .and_then(|d| serde_json::from_str(d).ok())
            .unwrap_or(Value::Null);
        let reason = parsed["reason"].as_str().unwrap_or("unknown").to_string();
        *out.entry(reason).or_insert(0u64) += 1;
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
    pub tokens: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProbeCounts {
    pub failed: u64,
    pub held: u64,
    pub migrated: u64,
    pub ok: u64,
    pub skipped: u64,
}

/// What prices a group's shares: the cost model, each account's kind and each oauth account's tier, read once per
/// route call; `None` under a grouping other than account, where no share is answered.
#[derive(Clone, Copy)]
pub struct SharePricing<'a> {
    pub tables: &'a PlanTables,
    pub kinds: &'a HashMap<String, AccountKind>,
    pub tiers: &'a Tiers,
}

impl SharePricing<'_> {
    /// The tier `account` prices under: `None` for an api_key account, an oauth account with no tier on file and
    /// an account the registry does not hold, none of which answers a share.
    fn tier_of(&self, account: &str) -> Option<&str> {
        match self.kinds.get(account)? {
            AccountKind::Oauth => self.tiers.get(account)?.as_deref(),
            AccountKind::ApiKey => None,
        }
    }
}

/// The read and write halves of a window's cache share, as fractions of the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CacheShare {
    pub read: f64,
    pub write: f64,
}

/// The probe rows' share of a window and the wasted probes' part of it: the group's probe share times the ratio of
/// its wasted-probe dollars to its plan-funded probe-cost dollars over its accounting rows, since the rollups keep
/// no token counts for the wasted probes alone. `wasted` is `None` while the group's plan-funded probe dollars are
/// zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ProbeShare {
    pub cost: f64,
    pub wasted: Option<f64>,
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
    pub cache_miss_reasons: BTreeMap<String, u64>,
    pub histograms: Histograms,
    pub percentiles: Percentiles,
    pub kept_warm: KeptWarm,
    pub probes: ProbeCounts,
    pub probe_cost_usd: f64,
    pub wasted_probes: u64,
    pub wasted_probe_usd: f64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
    /// Keyed by window id, `five_hour`, `seven_day` and the model week. A window is absent where a plan row a known
    /// tier holds could not be priced on it, never for a row whose model the window does not apply to. `None` for
    /// an api_key group, an oauth group with no tier on file, an account the registry does not hold, and under
    /// every grouping but account.
    pub cost_shares: Option<IndexMap<&'static str, f64>>,
    pub cache_shares: Option<IndexMap<&'static str, CacheShare>>,
    pub probe_shares: Option<IndexMap<&'static str, ProbeShare>>,
    /// The plan-funded requests and output tokens the shares were summed over, the denominators of a share per
    /// request and per 1k output tokens; `None` where `cost_shares` is.
    pub plan_requests: Option<u64>,
    pub plan_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SeriesPoint {
    pub at: i64,
    pub groups: Vec<GroupRow>,
}

/// Which rows a read keeps by who paid for them, off the funding each row was recorded under: `Billed` the billed,
/// overage and fast rows, `Plan` the plan rows. The two partition the rows, so their sums add up to the unfiltered
/// sum, and the registry's later state moves no row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingFilter {
    Billed,
    Plan,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RollupFilter {
    /// A value is a key id, which names the rows of the key holding it now, or `<id>@<generation>`, which
    /// names an earlier holder's.
    pub keys: Vec<String>,
    pub pools: Vec<String>,
    pub accounts: Vec<String>,
    pub models: Vec<String>,
    pub kinds: Vec<String>,
    /// Patterns under the wildcard grammar; a row matches when any pattern matches its agent in full.
    pub user_agents: Vec<String>,
    /// Under `group_by=user_agent`, each pattern names a group and a row falls in the first that matches it;
    /// a row no pattern matches groups under `other`. Empty means every agent is its own group.
    pub user_agent_groups: Vec<String>,
    /// Patterns under the wildcard grammar; a row matches when any pattern matches its client in full.
    pub clients: Vec<String>,
    pub include_probes: bool,
    /// Applied in memory beside the pattern filters, off the key's funding word; `None` keeps every row.
    pub funding: Option<FundingFilter>,
}

impl RollupFilter {
    /// Whether a probe row counts as a request: the flag, or a kind filter that asked for probes outright.
    /// The probe side of a group is summed either way, so the probe cost and the wasted probes read the same under
    /// both.
    pub fn probes_counted(&self) -> bool {
        self.include_probes || self.kinds.iter().any(|k| k == "probe")
    }

    /// The fixed filters as the read binds them, each key selector resolved as `LiveKeys::selects` reads it: the
    /// reserved id at any generation, every other selector at the lifetime `lifetime_of` answers. `None` when the
    /// filter names keys and none of them names a lifetime, which selects no row rather than every row.
    pub fn rollup_select(&self, live: &LiveKeys) -> Option<RollupSelect> {
        let keys: Vec<KeySelector> = self
            .keys
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
            .collect();
        if !self.keys.is_empty() && keys.is_empty() {
            return None;
        }
        Some(RollupSelect {
            pools: self.pools.clone(),
            accounts: self.accounts.clone(),
            kinds: self.kinds.clone(),
            models: self.models.iter().map(|m| m.to_ascii_lowercase()).collect(),
            keys,
        })
    }
}

pub const GROUP_BYS: &[&str] = &[
    "account",
    "all",
    "client",
    "effort",
    "key",
    "kind",
    "model",
    "pool",
    "request_class",
    "speed",
    "user_agent",
];

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

    /// The group a rollup row falls in under `group_by=key`: the id alone for the live generation,
    /// `<id>@<generation>` for any other, the reserved id as itself.
    pub fn key_of(&self, row: &RollupRow) -> String {
        if row.key_id == UNATTRIBUTED || self.0.get(&row.key_id) == Some(&row.key_generation) {
            return row.key_id.clone();
        }
        format!("{}@{}", row.key_id, row.key_generation)
    }

    /// The label of a key group alone (every other dimension keeps its group key as its label): the id;
    /// `<id> (deleted, created 2026-09-14 09:31 UTC)` for a generation no live key holds, the generation being that
    /// key's `created_at` so two earlier holders read apart; `<id> (deleted)` for generation 0, the rows the
    /// startup stamp left unstamped, which carry no creation time; `unattributed` for the reserved id.
    pub fn label_of(&self, key: &str) -> String {
        if key == UNATTRIBUTED {
            return "unattributed".into();
        }
        // A key id never carries `@` (the admin's slug rule), so the split is unambiguous.
        match key.rsplit_once('@') {
            Some((id, generation)) => match created_stamp(generation) {
                Some(stamp) => format!("{id} (deleted, created {stamp})"),
                None => format!("{id} (deleted)"),
            },
            None => key.into(),
        }
    }

    /// The holder a key group names beside its label, for a reader that prints the creation instant in its own
    /// zone: the id, and an earlier holder's generation. The live key and the reserved id carry no generation.
    pub fn holder_of(&self, key: &str) -> (String, Option<i64>) {
        match key.rsplit_once('@') {
            Some((id, generation)) => (id.into(), generation.parse().ok()),
            None => (key.into(), None),
        }
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

pub fn group_of(
    row: &RollupRow,
    group_by: &str,
    live: &LiveKeys,
    agent_groups: &[String],
) -> String {
    match group_by {
        "key" => live.key_of(row),
        "pool" => row.pool_id.clone(),
        "account" => row.account_id.clone(),
        "model" => row.canonical_model.clone(),
        "kind" => row.kind.clone(),
        "client" => row.client.clone(),
        "effort" => row.effort.clone(),
        "request_class" => row.request_class.clone(),
        "speed" => row.speed.clone(),
        "user_agent" => {
            if agent_groups.is_empty() {
                return row.user_agent.clone();
            }
            agent_groups
                .iter()
                .find(|pattern| crate::pattern::matches(pattern, &row.user_agent))
                .cloned()
                .unwrap_or_else(|| OTHER_AGENTS.into())
        }
        _ => "all".into(),
    }
}

/// Minute precision in UTC; the `<id>@<generation>` selector, never the label, tells two holders created in
/// the same minute apart. `None` for generation 0, which is no creation time, and for a generation that is no
/// millisecond instant, so the console and the ledger agree on every generation a key can carry.
fn created_stamp(generation: &str) -> Option<String> {
    generation
        .parse::<i64>()
        .ok()
        .filter(|&ms| ms != 0)
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
}

fn label_of(key: &str) -> String {
    if key == UNATTRIBUTED {
        "unattributed".into()
    } else {
        key.into()
    }
}

/// Whether a row passes the filter's two pattern filters, which SQL does not express under the wildcard grammar.
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
        Some(FundingFilter::Billed) => r.funding != PLAN_FUNDING,
        Some(FundingFilter::Plan) => r.funding == PLAN_FUNDING,
    }
}

/// The rows of a read the fixed filters already narrowed (`RollupFilter::rollup_select`) that the pattern filters
/// keep, probe rows among them: whether those count as requests is `probes_counted`, which the sums take, so a
/// probe's cost reaches the group under every filter.
#[cfg(any(test, feature = "test-support"))]
pub fn apply_filter(rows: Vec<RollupRow>, f: &RollupFilter) -> Vec<RollupRow> {
    rows.into_iter().filter(|r| passes_patterns(r, f)).collect()
}

fn group_row(
    key: &str,
    sum: &RollupRow,
    probes: &RollupRow,
    label: String,
    shares: GroupShares,
) -> GroupRow {
    let expired = sum
        .cache_miss_reasons
        .get("expired_between_turns")
        .copied()
        .unwrap_or(0);
    let mut reasons = sum.cache_miss_reasons.clone();
    reasons.remove("expired_between_turns");
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
        cost: CostSplitJson {
            cache_read: sum.cache_read_usd,
            cache_write: sum.cache_write_usd,
            input: sum.input_usd,
            output: sum.output_usd,
        },
        cost_usd: sum.estimated_usd,
        errors: sum.errors.clone(),
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
            tokens: sum.kept_warm_tokens,
        },
        // Derived from the group's probe rows alone: a probe's own request row carries its outcome, and the
        // engine's deltas carry what never ran.
        probes: ProbeCounts {
            failed: probes.failed
                + probes.rate_limited
                + probes.exhausted
                + probes.rejected
                + probes.incomplete,
            held: probes.probes_held,
            migrated: probes.probe_migrated,
            ok: probes.succeeded,
            skipped: probes.probes_skipped,
        },
        // From the probe sum: what the cache cost to keep warm is not a request the filter can drop.
        probe_cost_usd: probes.probe_cost_usd,
        wasted_probes: probes.wasted_probes,
        wasted_probe_usd: probes.wasted_probe_usd,
        wasted_probe_reasons: probes.wasted_probe_reasons.clone(),
        cost_shares: shares.cost_shares,
        cache_shares: shares.cache_shares,
        probe_shares: shares.probe_shares,
        plan_requests: shares.plan_requests,
        plan_output_tokens: shares.plan_output_tokens,
    }
}

/// One window's running share and whether a row a known tier holds could not be priced on it: a latched window is
/// absent from the answered map, as `price_usage` nulls a bucket's, so a partial sum never reads as the share.
#[derive(Default)]
struct WindowSum {
    sum: f64,
    unpriced: bool,
}

impl WindowSum {
    fn add(&mut self, share: Option<f64>) {
        match share {
            Some(share) => self.sum += share,
            None => self.unpriced = true,
        }
    }
}

/// The read and write halves of one window's cache share, latched together: both are priced under the one tier,
/// model and hour, so neither can be unpriced without the other.
#[derive(Default)]
struct CacheSum {
    read: f64,
    write: f64,
    unpriced: bool,
}

/// The five share members of a `GroupRow`, folded from a group's `ShareSums` or all `None`.
#[derive(Default)]
struct GroupShares {
    cost_shares: Option<IndexMap<&'static str, f64>>,
    cache_shares: Option<IndexMap<&'static str, CacheShare>>,
    probe_shares: Option<IndexMap<&'static str, ProbeShare>>,
    plan_requests: Option<u64>,
    plan_output_tokens: Option<u64>,
}

/// A group's shares, summed row by row as the rows arrive: the summed keys of `GroupSum` blank the account, the
/// model and the hour a share is priced from, so the shares fold here from the incoming row. Keyed by window in the
/// order the rows introduced them.
#[derive(Default)]
struct ShareSums {
    cost: IndexMap<&'static str, WindowSum>,
    cache: IndexMap<&'static str, CacheSum>,
    probe_cost: IndexMap<&'static str, WindowSum>,
    plan_requests: u64,
    plan_output_tokens: u64,
    /// The dollars of the group's plan-funded accounting rows, the two sides of the wasted ratio. A settlement is one
    /// wasted probe filed under the funding its own cost row took, one row per funding, so an overage probe's credit
    /// and its waste both file under `overage` and enter neither side, and the ratio reads the plan-funded probes'
    /// waste against their own dollars. A fast recipe's credits and settlements file under `fast` and enter neither.
    plan_probe_cost_usd: f64,
    plan_wasted_probe_usd: f64,
}

impl ShareSums {
    /// Folds one plan-funded row in: the request side's shares and counts when `counted`, as the request dollars
    /// take the row, and the probe cost when the row is a probe's, each window priced at the row's own hour.
    fn add(&mut self, row: &RollupRow, tier: &str, tables: &PlanTables, counted: bool) {
        self.add_priced(row, counted, |usage| {
            price_shares(tables, tier, &row.canonical_model, usage, row.hour * 1_000)
        });
    }

    /// `add` with the pricer passed in, so a test can count the pricings a row costs.
    fn add_priced(
        &mut self,
        row: &RollupRow,
        counted: bool,
        price: impl Fn(PinUsage) -> WindowShares,
    ) {
        if counted {
            self.plan_requests += row.requests;
            self.plan_output_tokens += row.output_tokens;
        }
        if row.kind == "probe" {
            self.plan_probe_cost_usd += row.probe_cost_usd;
            self.plan_wasted_probe_usd += row.wasted_probe_usd;
        }
        let usage = PinUsage {
            input: row.input_tokens,
            cache_read: row.cache_read_tokens,
            write_5m: row.cache_write_5m_tokens,
            write_1h: row.cache_write_1h_tokens,
            output: row.output_tokens,
        };
        // A failed request books a row with no token and an accounting row carries dollars alone, while the model
        // refuses a model it has no weight for before it reads the usage: such a row would null a priced window
        // for a charge of nothing.
        if usage.input + usage.cache_read + usage.write_5m + usage.write_1h + usage.output == 0 {
            return;
        }
        let priced = price(usage);
        if counted {
            for (window, share) in &priced {
                self.cost.entry(*window).or_default().add(*share);
            }
            let read = price(PinUsage {
                cache_read: usage.cache_read,
                ..PinUsage::default()
            });
            let write = price(PinUsage {
                write_5m: usage.write_5m,
                write_1h: usage.write_1h,
                ..PinUsage::default()
            });
            for ((window, read), (_, write)) in read.into_iter().zip(write) {
                let sum = self.cache.entry(window).or_default();
                match (read, write) {
                    (Some(read), Some(write)) => {
                        sum.read += read;
                        sum.write += write;
                    }
                    _ => sum.unpriced = true,
                }
            }
        }
        if row.kind == "probe" {
            for (window, share) in priced {
                self.probe_cost.entry(window).or_default().add(share);
            }
        }
    }

    fn finish(self) -> GroupShares {
        let ratio = (self.plan_probe_cost_usd > 0.0)
            .then(|| self.plan_wasted_probe_usd / self.plan_probe_cost_usd);
        GroupShares {
            cost_shares: Some(
                self.cost
                    .into_iter()
                    .filter(|(_, s)| !s.unpriced)
                    .map(|(w, s)| (w, s.sum))
                    .collect(),
            ),
            cache_shares: Some(
                self.cache
                    .into_iter()
                    .filter(|(_, s)| !s.unpriced)
                    .map(|(w, s)| {
                        (
                            w,
                            CacheShare {
                                read: s.read,
                                write: s.write,
                            },
                        )
                    })
                    .collect(),
            ),
            probe_shares: Some(
                self.probe_cost
                    .into_iter()
                    .filter(|(_, s)| !s.unpriced)
                    .map(|(w, s)| {
                        (
                            w,
                            ProbeShare {
                                cost: s.sum,
                                wasted: ratio.map(|r| s.sum * r),
                            },
                        )
                    })
                    .collect(),
            ),
            plan_requests: Some(self.plan_requests),
            plan_output_tokens: Some(self.plan_output_tokens),
        }
    }
}

/// One group's running sums. Rows join one at a time, so a paged read folds each page in as it arrives.
struct GroupSum {
    /// The request side, which a probe row joins only when probes count as requests.
    sum: RollupRow,
    /// The probe rows on their own, which the derived probe counts, the probe cost and the wasted probes read.
    probes: RollupRow,
    /// Whether a non-probe row ever joined: a keepalive credit carries a real kind but never adds to `requests`.
    saw_non_probe: bool,
    /// The shares, for an oauth account with a tier under the account grouping.
    shares: Option<ShareSums>,
}

#[derive(Default)]
struct GroupSums(BTreeMap<String, GroupSum>);

impl GroupSums {
    fn add(
        &mut self,
        row: &RollupRow,
        group_by: &str,
        probes_counted: bool,
        live: &LiveKeys,
        agent_groups: &[String],
        pricing: Option<SharePricing<'_>>,
    ) {
        let key = group_of(row, group_by, live, agent_groups);
        let entry = self.0.entry(key.clone()).or_insert_with(|| GroupSum {
            sum: RollupRow::key(0, &key, 0, "", "", "", "", "", "", "", "", "", ""),
            probes: RollupRow::key(0, &key, 0, "", "", "", "", "", "", "", "", "", ""),
            saw_non_probe: false,
            shares: None,
        });
        if row.kind == "probe" {
            entry.probes.add(row);
        } else {
            entry.saw_non_probe = true;
        }
        let counted = probes_counted || row.kind != "probe";
        if counted {
            entry.sum.add(row);
        }
        // Under the account grouping alone, where the group is one account and its tier prices every row of it.
        let Some(pricing) = pricing.filter(|_| group_by == "account") else {
            return;
        };
        let Some(tier) = pricing.tier_of(&row.account_id) else {
            return;
        };
        let shares = entry.shares.get_or_insert_with(ShareSums::default);
        if Funding::of_rollup(&row.funding) == Funding::Plan {
            shares.add(row, tier, pricing.tables, counted);
        }
    }

    fn finish(mut self, group_by: &str, probes_counted: bool, live: &LiveKeys) -> Vec<GroupRow> {
        // A group whose rows in the range are all probes never joins the request side under the default filter;
        // without this it would report zero requests rather than staying out of a breakdown that does not count
        // probes as requests. A group with any non-probe row, a keepalive credit included, stays.
        self.0
            .retain(|_, group| probes_counted || group.saw_non_probe);
        let mut out: Vec<GroupRow> = self
            .0
            .into_iter()
            .map(|(k, group)| {
                let shares = group
                    .shares
                    .map_or_else(GroupShares::default, ShareSums::finish);
                if group_by == "key" {
                    let mut row =
                        group_row(&k, &group.sum, &group.probes, live.label_of(&k), shares);
                    let (key_id, key_created_at) = live.holder_of(&k);
                    row.key_id = Some(key_id);
                    row.key_created_at = key_created_at;
                    row
                } else {
                    group_row(&k, &group.sum, &group.probes, label_of(&k), shares)
                }
            })
            .collect();
        out.sort_by(|a, b| a.label.cmp(&b.label).then(a.key.cmp(&b.key)));
        out
    }
}

#[cfg(any(test, feature = "test-support"))]
fn sum_groups(
    rows: &[&RollupRow],
    group_by: &str,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
    pricing: Option<SharePricing<'_>>,
) -> Vec<GroupRow> {
    let mut sums = GroupSums::default();
    for row in rows {
        sums.add(row, group_by, probes_counted, live, agent_groups, pricing);
    }
    sums.finish(group_by, probes_counted, live)
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::too_many_arguments)]
pub fn series(
    rows: &[RollupRow],
    from: i64,
    to: i64,
    bucket_seconds: i64,
    group_by: &str,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
    pricing: Option<SharePricing<'_>>,
) -> Result<Vec<SeriesPoint>, LedgerError> {
    let first = (from / 1_000).div_euclid(bucket_seconds) * bucket_seconds;
    let last = hour_ceil(to);
    let count = (last - first).checked_div(bucket_seconds).unwrap_or(0);
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points, and the most a chart carries is {MAX_POINTS}"
        )));
    }
    let mut points = Vec::new();
    let mut at = first;
    while at < last {
        let in_bucket: Vec<&RollupRow> = rows
            .iter()
            .filter(|r| r.hour >= at && r.hour < at + bucket_seconds)
            .collect();
        points.push(SeriesPoint {
            at: at * 1_000,
            groups: sum_groups(
                &in_bucket,
                group_by,
                probes_counted,
                live,
                agent_groups,
                pricing,
            ),
        });
        at += bucket_seconds;
    }
    Ok(points)
}

#[cfg(any(test, feature = "test-support"))]
pub fn breakdown(
    rows: &[RollupRow],
    group_by: &str,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
    pricing: Option<SharePricing<'_>>,
) -> Vec<GroupRow> {
    sum_groups(
        &rows.iter().collect::<Vec<_>>(),
        group_by,
        probes_counted,
        live,
        agent_groups,
        pricing,
    )
}

/// `series` over `[from, to)` (milliseconds) under `filter`, read through `read_rollups_paged` and folded into each
/// bucket's group sums as the pages arrive, so a long range holds one page of rows at a time. `pricing` prices the
/// groups' shares under the account grouping.
#[allow(clippy::too_many_arguments)]
pub async fn series_paged(
    db: &Arc<LedgerDb>,
    filter: &RollupFilter,
    live: &LiveKeys,
    from: i64,
    to: i64,
    bucket_seconds: i64,
    group_by: &str,
    pricing: Option<SharePricing<'_>>,
) -> Result<Vec<SeriesPoint>, LedgerError> {
    if bucket_seconds <= 0 {
        return Err(LedgerError::Yaml(
            "bucket_seconds must be positive".to_string(),
        ));
    }
    let first = (from / 1_000).div_euclid(bucket_seconds) * bucket_seconds;
    let last = hour_ceil(to);
    let count = (last - first).checked_div(bucket_seconds).unwrap_or(0);
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points, and the most a chart carries is {MAX_POINTS}"
        )));
    }
    let starts: Vec<i64> = (0..)
        .map(|i| first + i * bucket_seconds)
        .take_while(|at| *at < last)
        .collect();
    let mut buckets: Vec<GroupSums> = starts.iter().map(|_| GroupSums::default()).collect();
    let probes_counted = filter.probes_counted();
    if let Some(select) = filter.rollup_select(live) {
        read_rollups_paged(db, &select, hour_of(from), last, |row| {
            if !passes_patterns(row, filter) || !passes_funding(row, filter) || row.hour < first {
                return;
            }
            let index = ((row.hour - first) / bucket_seconds) as usize;
            if let Some(sums) = buckets.get_mut(index) {
                sums.add(
                    row,
                    group_by,
                    probes_counted,
                    live,
                    &filter.user_agent_groups,
                    pricing,
                );
            }
        })
        .await?;
    }
    Ok(starts
        .into_iter()
        .zip(buckets)
        .map(|(at, sums)| SeriesPoint {
            at: at * 1_000,
            groups: sums.finish(group_by, probes_counted, live),
        })
        .collect())
}

/// `breakdown` over `[from, to)` (milliseconds) under `filter`, read through `read_rollups_paged` and folded into the
/// group sums as the pages arrive. `pricing` prices the groups' shares under the account grouping.
pub async fn breakdown_paged(
    db: &Arc<LedgerDb>,
    filter: &RollupFilter,
    live: &LiveKeys,
    from: i64,
    to: i64,
    group_by: &str,
    pricing: Option<SharePricing<'_>>,
) -> Result<Vec<GroupRow>, LedgerError> {
    let mut sums = GroupSums::default();
    let probes_counted = filter.probes_counted();
    if let Some(select) = filter.rollup_select(live) {
        read_rollups_paged(db, &select, hour_of(from), hour_ceil(to), |row| {
            if passes_patterns(row, filter) && passes_funding(row, filter) {
                sums.add(
                    row,
                    group_by,
                    probes_counted,
                    live,
                    &filter.user_agent_groups,
                    pricing,
                );
            }
        })
        .await?;
    }
    Ok(sums.finish(group_by, probes_counted, live))
}

#[derive(Clone, Debug, Serialize)]
pub struct HourPoint {
    pub at: i64,
    pub cost: f64,
    pub errors: u64,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Dashboard {
    pub requests_24h: u64,
    pub tokens_24h: u64,
    pub tokens_cached_24h: u64,
    pub cost_24h: f64,
    pub error_rate_24h: Option<f64>,
    pub top_error_code: Option<String>,
    pub hourly: Vec<HourPoint>,
    #[serde(skip)]
    pub recent: Vec<RequestWithAttempts>,
}

/// The last day's rollups folded hour by hour through `read_rollups_paged`, and the newest requests.
pub async fn dashboard(db: &Arc<LedgerDb>, now: i64) -> Result<Dashboard, LedgerError> {
    let to_hour = (now / 1_000).div_euclid(3_600) * 3_600 + 3_600;
    let from_hour = to_hour - 24 * 3_600;
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut hourly: Vec<HourPoint> = (from_hour..to_hour)
        .step_by(3_600)
        .map(|hour| HourPoint {
            at: hour * 1_000,
            cost: 0.0,
            errors: 0,
            requests: 0,
            tokens: 0,
        })
        .collect();
    let mut tokens_cached = 0;
    read_rollups_paged(db, &RollupSelect::default(), from_hour, to_hour, |r| {
        if r.kind == "probe" {
            return;
        }
        if let Some(point) = hourly.get_mut(((r.hour - from_hour) / 3_600) as usize) {
            point.requests += r.requests;
            point.errors += r.requests.saturating_sub(r.succeeded);
            point.cost += r.estimated_usd;
            point.tokens += r.input_tokens
                + r.cache_read_tokens
                + r.cache_write_5m_tokens
                + r.cache_write_1h_tokens
                + r.output_tokens;
        }
        for (code, n) in &r.errors {
            *errors.entry(code.clone()).or_default() += n;
        }
        tokens_cached += r.cache_read_tokens;
    })
    .await?;
    let requests: u64 = hourly.iter().map(|h| h.requests).sum();
    let failed: u64 = hourly.iter().map(|h| h.errors).sum();
    let (recent, _) = db
        .read(|c| list_requests(c, &RequestFilter::default(), &LiveKeys::default(), None, 20))
        .await?;
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
        hourly,
        recent,
    })
}

pub fn latest_quota(conn: &Connection, now: i64) -> Result<Vec<QuotaSampleRecord>, LedgerError> {
    let since = now / 1_000 - 86_400;
    let mut stmt = conn.prepare("SELECT q.account_id, q.bucket, q.window, q.utilization, q.reset_at, q.source, q.observed_at FROM quota_samples q WHERE q.bucket >= ?1 AND q.bucket = (SELECT MAX(bucket) FROM quota_samples WHERE account_id = q.account_id AND window = q.window AND bucket >= ?1) ORDER BY q.account_id, q.window")?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(QuotaSampleRecord {
            account_id: r.get(0)?,
            bucket: r.get(1)?,
            window: r.get(2)?,
            utilization: r.get(3)?,
            reset_at: r.get(4)?,
            source: r.get(5)?,
            observed_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The last quota sample of one account's window inside one hour, as `GET /admin/analytics/accounts` reads it.
/// `observed_at` is the sample's own instant (`quota_samples.observed_at`, Unix milliseconds), which the lead and the
/// wire's `sampled_at` read, since `bucket` is the 300-second floor the sampler files the row under. A row written
/// before the column existed carries none, and the reader falls back to the bucket's floor in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct HourlyWindowSample {
    pub account_id: String,
    /// The hour the bucket falls in, in seconds, floored as `hour_of` floors an instant.
    pub hour: i64,
    pub window: String,
    pub bucket: i64,
    pub observed_at: Option<i64>,
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
}

/// Every account's last sample per window and hour over `[from, to)` in Unix seconds, ordered by account, hour,
/// window.
pub fn hourly_window_samples(
    conn: &Connection,
    from: i64,
    to: i64,
) -> Result<Vec<HourlyWindowSample>, LedgerError> {
    // SQLite's `/` truncates toward zero, so the hour is floored through a modulus kept positive, the hour `hour_of`
    // files a bucket before the epoch under.
    const HOUR: &str = "(q.bucket - (((q.bucket % 3600) + 3600) % 3600))";
    let mut stmt = conn.prepare(&format!(
        "SELECT q.account_id, {HOUR} AS hour, q.window, q.bucket, q.observed_at, q.utilization, q.reset_at FROM quota_samples q WHERE q.bucket >= ?1 AND q.bucket < ?2 AND q.bucket = (SELECT MAX(bucket) FROM quota_samples WHERE account_id = q.account_id AND window = q.window AND bucket >= {HOUR} AND bucket < {HOUR} + 3600) ORDER BY q.account_id, hour, q.window"
    ))?;
    let rows = stmt.query_map(params![from, to], |r| {
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

pub fn latest_extra_usage(
    conn: &Connection,
    now: i64,
) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
    let since = now / 1_000 - 86_400;
    let mut stmt = conn.prepare("SELECT account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, disabled_reason, overage_reset, source FROM extra_usage_samples e WHERE bucket >= ?1 AND bucket = (SELECT MAX(bucket) FROM extra_usage_samples WHERE account_id = e.account_id AND bucket >= ?1) ORDER BY account_id")?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(ExtraUsageSampleRecord {
            account_id: r.get(0)?,
            bucket: r.get(1)?,
            is_enabled: r.get(2)?,
            in_use: r.get(3)?,
            monthly_limit: r.get(4)?,
            used_credits: r.get(5)?,
            utilization: r.get(6)?,
            currency: r.get(7)?,
            disabled_reason: r.get(8)?,
            overage_reset: r.get(9)?,
            source: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Totals {
    pub cost_usd: f64,
    pub requests: u64,
    pub tokens: u64,
}

/// Request, token, and cost totals per pool or account since an hour, probe rows left out. One aggregate
/// query rather than a rollup read, because the account list asks three times per call. A caller wanting every
/// row the rollups hold passes a value at or below the earliest hour they can carry.
pub fn request_totals(
    conn: &Connection,
    column: &str,
    since_hour: i64,
) -> Result<BTreeMap<String, Totals>, LedgerError> {
    // The column is one of the two identifiers this function names, never a caller's string.
    assert!(
        matches!(column, "account_id" | "pool_id"),
        "request_totals column"
    );
    let mut stmt = conn.prepare(&format!("SELECT {column}, SUM(requests), SUM(input_tokens + cache_read_tokens + cache_write_5m_tokens + cache_write_1h_tokens + output_tokens), SUM(estimated_usd) FROM rollups_hourly WHERE kind != 'probe' AND hour >= ?1 GROUP BY {column}"))?;
    let rows = stmt.query_map(params![since_hour], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Totals {
                requests: r.get::<_, i64>(1)? as u64,
                tokens: r.get::<_, i64>(2)? as u64,
                cost_usd: r.get::<_, f64>(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
}

/// Request, token, and cost totals per live key since an hour, probe rows left out: the pair is grouped in
/// SQL and only the live generation of each id is answered, so a key's row never carries an earlier holder's.
pub fn key_request_totals(
    conn: &Connection,
    since_hour: i64,
    live: &LiveKeys,
) -> Result<BTreeMap<String, Totals>, LedgerError> {
    let mut stmt = conn.prepare("SELECT key_id, key_generation, SUM(requests), SUM(input_tokens + cache_read_tokens + cache_write_5m_tokens + cache_write_1h_tokens + output_tokens), SUM(estimated_usd) FROM rollups_hourly WHERE kind != 'probe' AND hour >= ?1 GROUP BY key_id, key_generation")?;
    let rows = stmt.query_map(params![since_hour], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            Totals {
                requests: r.get::<_, i64>(2)? as u64,
                tokens: r.get::<_, i64>(3)? as u64,
                cost_usd: r.get::<_, f64>(4)?,
            },
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (id, generation, totals) = row?;
        if live.0.get(&id) == Some(&generation) {
            out.insert(id, totals);
        }
    }
    Ok(out)
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
/// merges its sessions with an earlier holder's.
pub fn session_activity(
    conn: &Connection,
    since: i64,
) -> Result<Vec<SessionActivity>, LedgerError> {
    activity(conn, since, "session_id")
}

/// One pass over the window's client requests, grouped by `group` (a column expression over `requests`): the
/// newest request of a group is inside the window whenever any of its requests is, so the window alone answers
/// both the counts and the newest request's facts.
fn activity(
    conn: &Connection,
    since: i64,
    group: &str,
) -> Result<Vec<SessionActivity>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "WITH recent AS (SELECT id, key_id, key_generation, session_id, {group} AS grp, received_at, canonical_model, \
         presented_retired, rotation_at, identified_by, identity_root FROM requests \
         WHERE key_id IS NOT NULL AND session_id IS NOT NULL AND kind != 'probe' AND received_at >= ?1), \
         ranked AS (SELECT recent.*, \
         ROW_NUMBER() OVER (PARTITION BY key_id, key_generation, grp ORDER BY received_at DESC, id DESC) AS nth, \
         MIN(received_at) OVER (PARTITION BY key_id, key_generation, grp) AS first_at, \
         COUNT(*) OVER (PARTITION BY key_id, key_generation, grp) AS n \
         FROM recent WHERE grp IS NOT NULL) \
         SELECT key_id, key_generation, grp, first_at, received_at, n, canonical_model, \
         (SELECT a.account_id FROM attempts a WHERE a.request_id = ranked.id ORDER BY a.attempt DESC LIMIT 1), \
         presented_retired, rotation_at, session_id, identified_by, identity_root \
         FROM ranked WHERE nth = 1 ORDER BY received_at DESC"
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

fn session_identities_chunk(
    conn: &Connection,
    sessions: &[(String, i64, String)],
) -> Result<Vec<SessionIdentity>, LedgerError> {
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<Box<dyn ToSql>> = Vec::with_capacity(sessions.len() * 3);
    let values: Vec<String> = sessions
        .iter()
        .map(|(key_id, key_generation, session_id)| {
            args.push(Box::new(key_id.clone()));
            args.push(Box::new(*key_generation));
            args.push(Box::new(session_id.clone()));
            let n = args.len();
            format!("(?{}, ?{}, ?{n})", n - 2, n - 1)
        })
        .collect();
    // The client's quota probe is an `inference` row ahead of the session's first main request and names no
    // fork, so the marker is read off the first refreshing or unclassed request, as the router records it.
    let refreshing = REFRESHING_CLASSES
        .iter()
        .map(|class| format!("'{class}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "WITH named(key_id, key_generation, session_id) AS (VALUES {}) \
         SELECT n.key_id, n.key_generation, n.session_id, \
         (SELECT r.forked_from FROM requests r WHERE r.key_id = n.key_id AND r.key_generation = n.key_generation AND r.session_id = n.session_id AND r.kind != 'probe' AND (r.request_class IS NULL OR r.request_class IN ({refreshing})) ORDER BY r.received_at ASC, r.id ASC LIMIT 1), \
         (SELECT r.fork_depth FROM requests r WHERE r.key_id = n.key_id AND r.key_generation = n.key_generation AND r.session_id = n.session_id AND r.kind != 'probe' AND (r.request_class IS NULL OR r.request_class IN ({refreshing})) ORDER BY r.received_at ASC, r.id ASC LIMIT 1) \
         FROM named n \
         WHERE EXISTS (SELECT 1 FROM requests r WHERE r.key_id = n.key_id AND r.key_generation = n.key_generation AND r.session_id = n.session_id AND r.kind != 'probe')",
        values.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter().map(|a| a.as_ref())), |r| {
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
        BudgetPeriod::Month => {
            let first = date.first_of_month();
            (
                midnight(first),
                midnight(
                    first.checked_add(jiff::Span::new().months(1)).expect(
                        "one month past any first-of-month date stays in jiff's date range",
                    ),
                ),
            )
        }
    }
}

/// One budgeted period of one member, as the spend report and the retention take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetedMember {
    pub pool_id: String,
    pub account_id: String,
    pub period: BudgetPeriod,
    /// Sum the metered rows alone, those filed under `overage` or `fast`: a subscription member's plan-funded traffic
    /// is no spend.
    pub metered_only: bool,
}

/// One budgeted period's spend of one member in its current period, summed from the hourly rollups and priced at
/// read time, or at the booked dollars for a model priced by prompt length, a keepalive probe's cost included
/// since it draws real money from an API-key member's budget the same as any other request's. With
/// `metered_only`, a subscription member's metered rows alone count, those filed under `overage` or `fast`. The
/// row names the period kind and its UTC bounds in ms (`period_end` is the next day or month boundary). The booked
/// dollars are read for a model priced by prompt length because an hour's sums carry no per-attempt prompt, and
/// every other group is re-priced because a budget must move with the table in force, not the price an attempt
/// happened to be booked at.
pub fn member_spends(
    conn: &Connection,
    members: &[BudgetedMember],
    now: i64,
    table: &PriceTable,
) -> Result<Vec<MemberSpend>, LedgerError> {
    let mut out = Vec::with_capacity(members.len());
    // Prepared once outside the per-member loop: a monthly member's period scans up to a month of rows, and
    // re-parsing the same statement text for every member in a pool would add nothing but overhead.
    let mut stmt = conn.prepare(
        "SELECT canonical_model, speed, SUM(input_tokens), SUM(cache_read_tokens), SUM(cache_write_5m_tokens), \
         SUM(cache_write_1h_tokens), SUM(output_tokens), SUM(estimated_usd) FROM rollups_hourly \
         WHERE pool_id = ?1 AND account_id = ?2 AND hour >= ?3 AND hour < ?4 \
         AND (?5 = 0 OR funding IN ('overage', 'fast')) \
         GROUP BY canonical_model, speed",
    )?;
    for BudgetedMember {
        pool_id,
        account_id,
        period,
        metered_only,
    } in members
    {
        let (period_start, period_end) = period_bounds(*period, now);
        let rows = stmt.query_map(
            params![
                pool_id,
                account_id,
                period_start / 1_000,
                period_end / 1_000,
                metered_only
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                    r.get::<_, i64>(4)? as u64,
                    r.get::<_, i64>(5)? as u64,
                    r.get::<_, i64>(6)? as u64,
                    r.get::<_, f64>(7)?,
                ))
            },
        )?;
        let mut spent_usd = 0.0;
        for row in rows {
            let (model, speed, input, cache_read, write_5m, write_1h, output, booked_usd) = row?;
            // An hour's sums carry no per-attempt prompt, so a model priced by prompt length cannot be re-priced
            // here: its group adds the dollars its attempts were booked at, and every other group moves with the
            // table in force.
            if table.priced_by_prompt_length(&model) {
                spent_usd += booked_usd;
                continue;
            }
            // A model the table cannot price contributes nothing: the analytics already read such a row as
            // unpriced, and a budget must not be tripped by a sum it cannot itself account for.
            if let Some((usd, _)) = table.estimate(
                &model,
                Speed::of(Some(&speed)),
                input,
                cache_read,
                write_5m,
                write_1h,
                output,
            ) {
                spent_usd += usd;
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
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(since), Box::new(until)];
    let mut clauses: Vec<String> = Vec::new();
    key_lifetimes(&filter.keys, live, &mut args, &mut clauses);
    in_list_or_null("pool_id", &filter.pools, &mut args, &mut clauses);
    in_list("account_id", &filter.accounts, &mut args, &mut clauses);
    in_list(
        "canonical_model",
        &filter
            .models
            .iter()
            .map(|m| m.to_ascii_lowercase())
            .collect::<Vec<_>>(),
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
    let rows = stmt.query_map(params_from_iter(args.iter().map(|a| a.as_ref())), |r| {
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
}

/// Every bucket in the range from the rollups of one pool, probes left out, read through `read_rollups_paged` with
/// the pool bound in SQL.
pub async fn pool_traffic(
    db: &Arc<LedgerDb>,
    pool_id: &str,
    from_hour: i64,
    to_hour: i64,
    bucket_seconds: i64,
) -> Result<Vec<TrafficBucket>, LedgerError> {
    if bucket_seconds <= 0 {
        return Err(LedgerError::Yaml(
            "bucket_seconds must be positive".to_string(),
        ));
    }
    let first = from_hour.div_euclid(bucket_seconds) * bucket_seconds;
    let count = (to_hour - first).checked_div(bucket_seconds).unwrap_or(0);
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points, and the most a chart carries is {MAX_POINTS}"
        )));
    }
    let mut out: Vec<TrafficBucket> = (0..)
        .map(|i| first + i * bucket_seconds)
        .take_while(|at| *at < to_hour)
        .map(|at| TrafficBucket {
            at: at * 1_000,
            requests: 0,
            failed: 0,
            tokens: 0,
            cost_usd: 0.0,
        })
        .collect();
    let select = RollupSelect {
        pools: vec![pool_id.to_string()],
        ..RollupSelect::default()
    };
    read_rollups_paged(db, &select, from_hour, to_hour, |r| {
        if r.kind == "probe" || r.hour < first {
            return;
        }
        if let Some(bucket) = out.get_mut(((r.hour - first) / bucket_seconds) as usize) {
            bucket.requests += r.requests;
            bucket.failed += r.requests.saturating_sub(r.succeeded);
            bucket.tokens += r.input_tokens
                + r.cache_read_tokens
                + r.cache_write_5m_tokens
                + r.cache_write_1h_tokens
                + r.output_tokens;
            bucket.cost_usd += r.estimated_usd;
        }
    })
    .await?;
    Ok(out)
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

/// The bucket edges a `SeriesFilter` reads over, and the `MAX_POINTS` guard every series function shares.
fn bucket_range(filter: &SeriesFilter) -> Result<(i64, i64), LedgerError> {
    // Checked before the division below it: `SeriesFilter::default()` carries `bucket_seconds: 0`, and
    // `div_euclid` panics on that rather than answering a range the guard below could still refuse.
    if filter.bucket_seconds <= 0 {
        return Err(LedgerError::Yaml(
            "bucket_seconds must be positive".to_string(),
        ));
    }
    let first = (filter.from / 1_000).div_euclid(filter.bucket_seconds) * filter.bucket_seconds;
    let last = hour_ceil(filter.to);
    // The ceiling: the series' `while at < last` loops walk a trailing partial bucket as a point of its own.
    let count = (last - first).saturating_add(filter.bucket_seconds - 1) / filter.bucket_seconds;
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points, and the most a chart carries is {MAX_POINTS}"
        )));
    }
    Ok((first, last))
}

/// One point of the burn series: a subscription row's tokens as the share of its account's own two windows
/// (the 5-hour share weighed by the band at the row's instant, the 7-day share unbanded), an api_key row's in
/// dollars; `None` on a window while no subscription row of the bucket could be priced on it, or while any row of
/// the bucket a known tier holds could not be.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct BurnPoint {
    /// Milliseconds, like every other series' `at`.
    pub at: i64,
    pub cached_tokens: u64,
    pub five_hour_share: Option<f64>,
    pub seven_day_share: Option<f64>,
    /// A row of the bucket a known tier could not price on the window, so its share reads `None` whatever the other
    /// rows priced: a partial sum would read as the bucket's share. Never on the wire.
    #[serde(skip)]
    pub five_hour_unpriced: bool,
    #[serde(skip)]
    pub seven_day_unpriced: bool,
    /// Every token of the bucket's rows, all five types: zero for a bucket with no traffic the ledger can attribute,
    /// since `price_usage` adds nothing for a row whose account is no longer registered.
    pub tokens: u64,
    pub uncached_tokens: u64,
    pub usd: f64,
}

/// The tier each account prices under, `None` for an account with none: read once per route call.
pub type Tiers = HashMap<String, Option<String>>;

/// The columns `burn_series` and `session_burn_series` price into a `BurnPoint`: a rollup row and a session
/// attempt carry the same five token counts and a dollar estimate under different names.
struct UsageRow<'a> {
    canonical_model: &'a str,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
    estimated_usd: f64,
    /// Milliseconds, the instant the band is read at: the row's own hour for a rollup row, which folds by the
    /// request's hour and keeps no completion, and the completion for an attempt, where the meter charges it. Per
    /// row, since a bucket spanning a band edge cannot say which side a charge fell on.
    at: i64,
    /// Who paid for the row: a rollup row's key word, an attempt's three facts. Never the rollup's speed, which
    /// also files a request that asked for fast mode and was served without it.
    funding: Funding,
}

/// The row's funding decides its unit, so the kind the registry holds for the account now has no say and a deleted
/// or recreated account moves no dollar into a share: `estimated_usd` for a row an api_key account served, the shares
/// of a subscription's two windows (each banded at the row's own instant) for a plan row when `tier` names one the
/// tables size, and no share for a plan row with no tier on file, an account the registry no longer holds among them.
/// A fast row and one in overage add their `estimated_usd` and a zero share where the tables size the tier: fast mode
/// spends usage credits and overage is billed in dollars, neither draws a plan window, and the zero still tells a
/// reader the account is priced in its window unit. A bucket holding a row the table cannot price answers `None` on
/// that window, whichever row came first, and a row with no token has nothing to price and leaves both windows as the
/// other rows set them.
fn price_usage(point: &mut BurnPoint, tier: Option<&str>, tables: &PlanTables, row: &UsageRow) {
    let tokens = row.input_tokens
        + row.cache_read_tokens
        + row.cache_write_5m_tokens
        + row.cache_write_1h_tokens
        + row.output_tokens;
    match row.funding {
        Funding::Fast | Funding::Overage => {
            point.usd += row.estimated_usd;
            // Ahead of the model's weights: a dollar-billed row prices in dollars whatever the model weighs it at,
            // so the zero rests on the tier alone.
            if let Some(tier) = tier.filter(|_| tokens > 0) {
                if tables.tier_factor(tier, FIVE_HOUR).is_some() && !point.five_hour_unpriced {
                    point.five_hour_share.get_or_insert(0.0);
                }
                if tables.tier_factor(tier, SEVEN_DAY).is_some() && !point.seven_day_unpriced {
                    point.seven_day_share.get_or_insert(0.0);
                }
            }
        }
        Funding::Plan => {
            let usage = PinUsage {
                input: row.input_tokens,
                cache_read: row.cache_read_tokens,
                write_5m: row.cache_write_5m_tokens,
                write_1h: row.cache_write_1h_tokens,
                output: row.output_tokens,
            };
            // A failed request books a row with no token, and the model refuses a model it has no weight for before
            // it reads the usage: such a row would null a priced hour for a charge of nothing.
            if let Some(tier) = tier.filter(|_| tokens > 0) {
                let shares = price_shared_windows(tables, tier, row.canonical_model, usage, row.at);
                let shared = |window: &str| shares.get(window).copied().flatten();
                match shared(FIVE_HOUR) {
                    Some(share) if !point.five_hour_unpriced => {
                        *point.five_hour_share.get_or_insert(0.0) += share;
                    }
                    Some(_) => {}
                    None => {
                        point.five_hour_unpriced = true;
                        point.five_hour_share = None;
                    }
                }
                match shared(SEVEN_DAY) {
                    Some(share) if !point.seven_day_unpriced => {
                        *point.seven_day_share.get_or_insert(0.0) += share;
                    }
                    Some(_) => {}
                    None => {
                        point.seven_day_unpriced = true;
                        point.seven_day_share = None;
                    }
                }
            }
        }
        Funding::ApiKey => point.usd += row.estimated_usd,
    }
    point.cached_tokens += row.cache_read_tokens;
    point.tokens += tokens;
    point.uncached_tokens += row.input_tokens;
}

/// `price_shares` for the two shared windows alone, the five-hour and the seven-day: the burn chart draws no model
/// week, so none is built.
fn price_shared_windows(
    tables: &PlanTables,
    tier: &str,
    canonical_model: &str,
    usage: PinUsage,
    at: i64,
) -> WindowShares {
    [FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .map(|window| {
            (
                window,
                tables.share_at(window, tier, canonical_model, usage, at),
            )
        })
        .collect()
}

/// The rollup rows `burn_series` prices for `filter`: every hour of its bucket range, whatever its scopes, so one
/// read serves `burn_series_over` for every scope over the same range and bucket. The burn route reads through
/// `burn_groups_paged` instead, which holds a page at a time.
#[cfg(any(test, feature = "test-support"))]
pub fn burn_rows(conn: &Connection, filter: &SeriesFilter) -> Result<Vec<RollupRow>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    read_rollups(conn, first, last, &RollupSelect::default())
}

/// `burn_series_over` on the rows `burn_rows` reads for `filter`.
#[cfg(any(test, feature = "test-support"))]
pub fn burn_series(
    conn: &Connection,
    tables: &PlanTables,
    tiers: &Tiers,
    filter: &SeriesFilter,
) -> Result<Vec<BurnPoint>, LedgerError> {
    let rows = burn_rows(conn, filter)?;
    burn_series_over(&rows, tables, tiers, filter)
}

/// The burn series of `filter`'s scopes over `rows`, which `burn_rows` read for the same range and bucket. A
/// subscription row's 5-hour share carries the band of the hour the rollup folded the request under, since the
/// rollup keeps no completion; read per row rather than per bucket, since a six-hour or daily bucket can straddle a
/// band edge and only the row's own hour says which side of it the row fell on. One pass: each row lands in its
/// bucket by index.
pub fn burn_series_over(
    rows: &[RollupRow],
    tables: &PlanTables,
    tiers: &Tiers,
    filter: &SeriesFilter,
) -> Result<Vec<BurnPoint>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let mut points = empty_burn_points(first, last, filter.bucket_seconds);
    for row in rows.iter().filter(|r| burn_scope_holds(filter, r)) {
        price_burn_row(
            &mut points,
            first,
            filter.bucket_seconds,
            row,
            tables,
            tiers,
        );
    }
    Ok(points)
}

/// Every bucket of `[first, last)`, empty. Ceiling, as `session_burn_series` counts them: a trailing partial bucket
/// is a point of its own.
fn empty_burn_points(first: i64, last: i64, bucket_seconds: i64) -> Vec<BurnPoint> {
    let count = ((last - first + bucket_seconds - 1) / bucket_seconds).max(0) as usize;
    (0..count)
        .map(|i| BurnPoint {
            at: (first + i as i64 * bucket_seconds) * 1_000,
            ..BurnPoint::default()
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
fn price_burn_row(
    points: &mut [BurnPoint],
    first: i64,
    bucket_seconds: i64,
    row: &RollupRow,
    tables: &PlanTables,
    tiers: &Tiers,
) {
    if row.hour < first {
        return;
    }
    let Some(point) = points.get_mut(((row.hour - first) / bucket_seconds) as usize) else {
        return;
    };
    price_usage(
        point,
        tiers.get(&row.account_id).and_then(Option::as_deref),
        tables,
        &UsageRow {
            canonical_model: &row.canonical_model,
            input_tokens: row.input_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_write_5m_tokens: row.cache_write_5m_tokens,
            cache_write_1h_tokens: row.cache_write_1h_tokens,
            output_tokens: row.output_tokens,
            estimated_usd: row.estimated_usd,
            at: row.hour * 1_000,
            funding: Funding::of_rollup(&row.funding),
        },
    );
}

/// The burn series of every group under `group_by` (`account`, `key` or `request_class`, else `account`) within
/// `filter`'s scopes, read through `read_rollups_paged` and priced into the group's buckets as the pages arrive: each
/// group's points are the ones `burn_series_over` answers for a scope of that group alone. The groups are the ones
/// the range itself holds; the read reaches back to the first bucket's edge, which may start before it.
pub async fn burn_groups_paged(
    db: &Arc<LedgerDb>,
    tables: &PlanTables,
    tiers: &Tiers,
    filter: &SeriesFilter,
    group_by: &str,
) -> Result<Vec<(String, Vec<BurnPoint>)>, LedgerError> {
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
    let mut groups: BTreeMap<String, (Vec<BurnPoint>, bool)> = BTreeMap::new();
    read_rollups_paged(db, &select, first, last, |row| {
        if !burn_scope_holds(filter, row) {
            return;
        }
        let group = match group_by {
            "key" => &row.key_id,
            "request_class" => &row.request_class,
            _ => &row.account_id,
        };
        let (points, held) = groups
            .entry(group.clone())
            .or_insert_with(|| (empty_burn_points(first, last, filter.bucket_seconds), false));
        *held |= row.hour >= from_hour && row.hour < last;
        price_burn_row(points, first, filter.bucket_seconds, row, tables, tiers);
    })
    .await?;
    Ok(groups
        .into_iter()
        .filter(|(_, (_, held))| *held)
        .map(|(group, (points, _))| (group, points))
        .collect())
}

/// The cache-write tokens the account rollups booked as a rewrite, per bucket; the bucket's `at` is
/// milliseconds, like `BurnPoint.at`.
pub fn rewrite_series(
    conn: &Connection,
    filter: &SeriesFilter,
) -> Result<Vec<(i64, u64)>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let rows = read_account_rollups(conn, first, last)?;
    let mut points = Vec::new();
    let mut at = first;
    while at < last {
        let sum = rows
            .iter()
            .filter(|r| r.hour >= at && r.hour < at + filter.bucket_seconds)
            .filter(|r| filter.accounts.is_empty() || filter.accounts.contains(&r.account_id))
            .map(|r| r.rewrite_tokens)
            .sum();
        points.push((at * 1_000, sum));
        at += filter.bucket_seconds;
    }
    Ok(points)
}

/// The moves the account rollups counted over the range, summed by reason across every bucket and account the
/// filter admits.
pub fn moves_by_reason(
    conn: &Connection,
    filter: &SeriesFilter,
) -> Result<BTreeMap<String, u64>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let rows = read_account_rollups(conn, first, last)?;
    let mut out = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|r| filter.accounts.is_empty() || filter.accounts.contains(&r.account_id))
    {
        for (reason, count) in &row.migrations {
            *out.entry(reason.clone()).or_default() += count;
        }
    }
    Ok(out)
}

/// How many sessions `session_burn_series` answers: the burn chart's own row cap, so a busy range's read stays
/// bounded by a caller-independent constant rather than the range's size.
const SESSION_BURN_LIMIT: usize = 20;

/// `top_sessions_by_tokens`' statement, spelled once so the plan test explains the statement the read runs. A
/// request is admitted within an hour of any attempt's start, so the request's own index carries the range. The
/// index is named: the ledger keeps no planner statistics, and without them SQLite prices a scan of every attempt
/// below this range search.
const TOP_SESSIONS_BY_TOKENS: &str = "SELECT r.session_id FROM attempts a \
     JOIN requests r INDEXED BY requests_received ON r.id = a.request_id \
     WHERE r.session_id IS NOT NULL AND a.started_at >= ?1 AND a.started_at < ?2 \
     AND r.received_at >= ?1 - 3600000 AND r.received_at < ?2 \
     GROUP BY r.session_id \
     ORDER BY SUM(COALESCE(a.input_tokens, 0) + COALESCE(a.cache_read_tokens, 0) \
         + COALESCE(a.cache_write_5m_tokens, 0) + COALESCE(a.cache_write_1h_tokens, 0) \
         + COALESCE(a.output_tokens, 0)) DESC, r.session_id \
     LIMIT ?3";

/// The `limit` sessions with the most tokens over `[from, to)`, ranked in SQL rather than in memory, so a
/// caller never reads an attempt row belonging to a session it will not answer.
fn top_sessions_by_tokens(
    conn: &Connection,
    from: i64,
    to: i64,
    limit: usize,
) -> Result<Vec<String>, LedgerError> {
    let mut stmt = conn.prepare(TOP_SESSIONS_BY_TOKENS)?;
    let rows = stmt.query_map(params![from, to, limit as i64], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(LedgerError::from)
}

/// One attempt's tokens toward a session's burn series: the rollups carry no session dimension, so this reads
/// the request and attempt rows directly, for the given sessions alone.
struct SessionAttempt {
    session_id: String,
    account_id: String,
    /// Milliseconds, `attempts.started_at`: the bucket the attempt lands in.
    at: i64,
    /// Milliseconds: the attempt's completion, `attempts.finished_at`, else the request's, where the meter charges
    /// it; the band that prices it is read here.
    completed_at: i64,
    canonical_model: String,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
    estimated_usd: f64,
    /// `attempts.speed`, the speed the answer reported.
    speed: Option<String>,
    /// `attempts.account_kind` and `attempts.overage_in_use`, the two facts beside the speed that say who paid.
    account_kind: String,
    overage_in_use: Option<bool>,
}

/// `read_session_attempts`' statement for `sessions` ids and its binds, spelled once so the plan test explains the
/// statement the read runs. A request is admitted within an hour of any attempt's start, so the request's own index
/// carries the range.
fn session_attempts_statement(
    from: i64,
    to: i64,
    sessions: &[String],
) -> (String, Vec<rusqlite::types::Value>) {
    let placeholders = sessions.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT r.session_id, a.account_id, a.started_at, \
         COALESCE(a.finished_at, r.finished_at, a.started_at), r.canonical_model, \
         a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens, \
         a.output_tokens, a.estimated_usd, a.speed, a.account_kind, a.overage_in_use \
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
    bind.push(rusqlite::types::Value::Integer(from - 3_600_000));
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
    let tokens = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
    let rows = stmt.query_map(params_from_iter(bind), |row| {
        Ok(SessionAttempt {
            session_id: row.get(0)?,
            account_id: row.get(1)?,
            at: row.get(2)?,
            completed_at: row.get(3)?,
            canonical_model: row.get(4)?,
            input_tokens: tokens(row.get(5)?),
            cache_read_tokens: tokens(row.get(6)?),
            cache_write_5m_tokens: tokens(row.get(7)?),
            cache_write_1h_tokens: tokens(row.get(8)?),
            output_tokens: tokens(row.get(9)?),
            estimated_usd: row.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
            speed: row.get(11)?,
            account_kind: row.get(12)?,
            overage_in_use: row.get(13)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(LedgerError::from)
}

/// The twenty sessions with the most tokens over the filter's range, each priced per bucket the way
/// `burn_series` prices an account row, save that an attempt lands in the bucket it started in and reads the band
/// at its completion. One pass over the range's attempts: each lands in its bucket by index, rather than every
/// session re-scanning every attempt for every bucket.
pub fn session_burn_series(
    conn: &Connection,
    tables: &PlanTables,
    tiers: &Tiers,
    filter: &SeriesFilter,
) -> Result<Vec<(String, Vec<BurnPoint>)>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let sessions = top_sessions_by_tokens(conn, first * 1_000, last * 1_000, SESSION_BURN_LIMIT)?;
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    // Ceiling, not floor: `last` is only rounded up to the hour (`hour_ceil`), so a 6-hour or daily bucket can
    // leave a trailing partial one, the same partial bucket `burn_series`'s `while at < last` loop still walks.
    let bucket_count =
        ((last - first + filter.bucket_seconds - 1) / filter.bucket_seconds) as usize;
    let empty_series = || {
        (0..bucket_count)
            .map(|i| BurnPoint {
                at: (first + i as i64 * filter.bucket_seconds) * 1_000,
                ..BurnPoint::default()
            })
            .collect::<Vec<_>>()
    };
    let mut by_session: HashMap<String, Vec<BurnPoint>> = sessions
        .iter()
        .map(|id| (id.clone(), empty_series()))
        .collect();
    for a in read_session_attempts(conn, first * 1_000, last * 1_000, &sessions)? {
        let Some(series) = by_session.get_mut(&a.session_id) else {
            continue;
        };
        let bucket_at = a.at / 1_000;
        if bucket_at < first || bucket_at >= last {
            continue;
        }
        let index = ((bucket_at - first) / filter.bucket_seconds) as usize;
        let Some(point) = series.get_mut(index) else {
            continue;
        };
        price_usage(
            point,
            tiers.get(&a.account_id).and_then(Option::as_deref),
            tables,
            &UsageRow {
                canonical_model: &a.canonical_model,
                input_tokens: a.input_tokens,
                cache_read_tokens: a.cache_read_tokens,
                cache_write_5m_tokens: a.cache_write_5m_tokens,
                cache_write_1h_tokens: a.cache_write_1h_tokens,
                output_tokens: a.output_tokens,
                estimated_usd: a.estimated_usd,
                at: a.completed_at,
                funding: funding_of(&a.account_kind, a.overage_in_use, a.speed.as_deref()),
            },
        );
    }
    Ok(sessions
        .into_iter()
        .map(|id| {
            let series = by_session.remove(&id).unwrap_or_default();
            (id, series)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(sql: &str, binds: Vec<rusqlite::types::Value>) -> Vec<String> {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        db.read_blocking(|c| {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let rows = stmt
                .query_map(params_from_iter(binds), |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap()
    }

    /// The session burn chart's two reads bound the request by its own `received_at` beside the attempt's start,
    /// which no index leads with, so the request side seeks the range rather than walking every retained session.
    #[test]
    fn the_session_burn_reads_search_the_requests_by_their_received_range() {
        let ranged = |plan: &[String]| {
            plan.iter().any(|step| {
                step.starts_with("SEARCH r USING ")
                    && step.contains("received_at>?")
                    && step.contains("received_at<?")
            }) && !plan.iter().any(|step| step.starts_with("SCAN"))
        };
        let top = plan_of(
            TOP_SESSIONS_BY_TOKENS,
            vec![
                rusqlite::types::Value::Integer(0),
                rusqlite::types::Value::Integer(3_600_000),
                rusqlite::types::Value::Integer(20),
            ],
        );
        assert!(ranged(&top), "{top:?}");
        let (sql, binds) =
            session_attempts_statement(0, 3_600_000, &["s1".to_string(), "s2".to_string()]);
        let attempts = plan_of(&sql, binds);
        assert!(ranged(&attempts), "{attempts:?}");
    }

    #[test]
    fn the_newest_events_of_a_kind_are_read_by_seeking_to_the_kind() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let plan: Vec<String> = db
            .read_blocking(|c| {
                let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {RECENT_EVENTS_OF_KIND}"))?;
                let rows = stmt
                    .query_map(params!["model_rejected", 50], |r| r.get::<_, String>(3))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
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

    /// 2026-01-15 00:00 UTC in seconds, inside the test cost model's effective range.
    const HOUR: i64 = 1_768_435_200;
    const TIER: &str = "default_claude_max_20x";

    /// The burn chart draws the two shared windows alone, so a model with a week of its own is priced on those two
    /// and no entry is built for its week.
    #[test]
    fn a_burn_row_of_a_model_with_its_own_week_prices_the_two_shared_windows_alone() {
        let tables = crate::test_tables::subscription_costs();
        let model = "claude-fable-5-1";
        assert_ne!(
            phoebis_routing::window_for(model),
            SEVEN_DAY,
            "the model has a week of its own"
        );
        let usage = PinUsage {
            input: 1_000,
            output: 100,
            ..PinUsage::default()
        };
        let at = HOUR * 1_000;
        let shares = price_shared_windows(&tables, TIER, model, usage, at);
        assert_eq!(
            shares.keys().copied().collect::<Vec<_>>(),
            [FIVE_HOUR, SEVEN_DAY],
            "no model-week entry: {shares:?}"
        );
        for window in [FIVE_HOUR, SEVEN_DAY] {
            let priced = tables.share_at(window, TIER, model, usage, at);
            assert!(priced.is_some(), "the test model prices {window}");
            assert_eq!(shares[window], priced, "{window}");
        }
    }

    /// A counted probe row is one usage for both sides, so it is priced once and the request side's cost and the
    /// probe side's cost read the same shares.
    #[test]
    fn a_counted_probe_row_is_priced_once_for_its_cost_and_its_probe_cost() {
        let tables = crate::test_tables::subscription_costs();
        let model = "claude-sonnet-5-5";
        let mut row = RollupRow::key(
            HOUR, "k", 0, "p", "a", model, "probe", "-", "-", "-", "-", "standard", "plan",
        );
        row.requests = 1;
        row.input_tokens = 1_000;
        row.output_tokens = 100;
        let usage = PinUsage {
            input: 1_000,
            output: 100,
            ..PinUsage::default()
        };
        let whole = std::cell::Cell::new(0);
        let mut sums = ShareSums::default();
        sums.add_priced(&row, true, |priced| {
            if priced == usage {
                whole.set(whole.get() + 1);
            }
            price_shares(&tables, TIER, model, priced, HOUR * 1_000)
        });
        assert_eq!(whole.get(), 1, "the row's usage is priced once");
        let sums_of = |windows: &IndexMap<&'static str, WindowSum>| {
            windows
                .iter()
                .map(|(window, sum)| (*window, sum.sum, sum.unpriced))
                .collect::<Vec<_>>()
        };
        assert_eq!(sums_of(&sums.cost), sums_of(&sums.probe_cost));
        assert_eq!(sums.cost.len(), 2, "both shared windows priced");
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
