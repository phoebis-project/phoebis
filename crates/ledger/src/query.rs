//! Reads for the admin routes: the request log with its filters and cursor, the rollup series and breakdown
//! with merged histograms, the dashboard's day, and the newest quota sample per window.

use std::collections::{BTreeMap, HashMap};

use phoebis_routing::FIVE_HOUR;
use phoebis_store::AccountKind;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{row_to_attempt, row_to_request, ATTEMPT_SELECT, REQUEST_SELECT};
use crate::histogram::Histogram;
use crate::plan::{PlanFactors, PlanWindows};
use crate::record::{
    AttemptRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord, RequestRecord,
};
use crate::rollup::{read_account_rollups, read_rollups, RollupRow, UNATTRIBUTED};
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
    pub keys: Vec<String>,
    pub pools: Vec<String>,
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
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub include_probes: bool,
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
        args.push(Box::new(format!("%{text}%")));
        let n = args.len();
        clauses.push(format!("(id LIKE ?{n} OR session_id LIKE ?{n} OR key_id LIKE ?{n} OR model LIKE ?{n} OR error_code LIKE ?{n} OR EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id AND a.account_id LIKE ?{n}))"));
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
        let marks: Vec<String> = filter
            .accounts
            .iter()
            .map(|v| {
                args.push(Box::new(v.clone()));
                format!("?{}", args.len())
            })
            .collect();
        let served = format!("EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id AND a.account_id IN ({}))", marks.join(", "));
        // The rollups file a request that never reached an account under the reserved id.
        if filter.accounts.iter().any(|v| v == UNATTRIBUTED) {
            clauses.push(format!("({served} OR NOT EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id))"));
        } else {
            clauses.push(served);
        }
    }
    if let Some(agent) = filter.user_agent.as_deref().filter(|a| !a.is_empty()) {
        let (like, escape) = crate::pattern::to_like(agent);
        args.push(Box::new(like));
        let at_like = args.len();
        args.push(Box::new(escape.to_string()));
        // The rollups and the console's fold already read an absent agent as `-`; without the coalesce a
        // nullable column excludes such a row from both the wildcard and the exact marker.
        clauses.push(format!(
            "COALESCE(user_agent, '-') LIKE ?{at_like} ESCAPE ?{}",
            at_like + 1
        ));
    }
    if let Some(client) = filter.client.as_deref().filter(|c| !c.is_empty()) {
        let (like, escape) = crate::pattern::to_like(client);
        args.push(Box::new(like));
        let at_like = args.len();
        args.push(Box::new(escape.to_string()));
        clauses.push(format!(
            "COALESCE(client, '-') LIKE ?{at_like} ESCAPE ?{}",
            at_like + 1
        ));
    }
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
    args.push(Box::new(limit as i64));
    let sql = format!(
        "{REQUEST_SELECT}{where} ORDER BY received_at DESC, id DESC LIMIT ?{}",
        args.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    let requests = stmt
        .query_map(
            params_from_iter(args.iter().map(|a| a.as_ref())),
            row_to_request,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
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
    let next = (out.len() == limit)
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

pub fn request_events(
    conn: &Connection,
    request: &RequestRecord,
) -> Result<Vec<EventRecord>, LedgerError> {
    let to = request.finished_at.unwrap_or(i64::MAX);
    let marker = format!("%\"source_request_id\":\"{}\"%", request.id);
    let mut stmt = conn.prepare("SELECT at, kind, account_id, key_id, session_id, model, actor, detail FROM events WHERE at >= ?1 AND at <= ?2 AND ((session_id IS NOT NULL AND session_id = ?3) OR detail LIKE ?4) ORDER BY at, rowid")?;
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
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const RECENT_EVENTS_OF_KIND: &str = "SELECT at, kind, account_id, key_id, session_id, model, actor, detail FROM events WHERE kind = ?1 ORDER BY at DESC, rowid DESC LIMIT ?2";

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
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    pub migrated: u64,
    pub ok: u64,
    pub skipped: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Savings {
    pub gross_usd: f64,
    pub probe_cost_usd: f64,
    pub wasted_probes: u64,
    pub wasted_probe_usd: f64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GroupRow {
    pub key: String,
    pub label: String,
    pub requests: u64,
    pub succeeded: u64,
    pub rate_limited: u64,
    pub failed: u64,
    pub exhausted: u64,
    pub rejected: u64,
    pub attempts: u64,
    pub failovers: u64,
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
    pub savings: Savings,
}

#[derive(Clone, Debug, Serialize)]
pub struct SeriesPoint {
    pub at: i64,
    pub groups: Vec<GroupRow>,
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
}

impl RollupFilter {
    /// Whether a probe row counts as a request: the flag, or a kind filter that asked for probes outright.
    /// The probe side of a group is summed either way, so the savings panel reads the same under both.
    pub fn probes_counted(&self) -> bool {
        self.include_probes || self.kinds.iter().any(|k| k == "probe")
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

    /// The label of a key group alone (every other dimension keeps its group key as its label): the id,
    /// `<id> (deleted)` for a generation no live key holds, `unattributed` for the reserved id.
    pub fn label_of(&self, key: &str) -> String {
        if key == UNATTRIBUTED {
            return "unattributed".into();
        }
        // A key id never carries `@` (the admin's slug rule), so the split is unambiguous.
        match key.rsplit_once('@') {
            Some((id, _)) => format!("{id} (deleted)"),
            None => key.into(),
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

fn label_of(key: &str) -> String {
    if key == UNATTRIBUTED {
        "unattributed".into()
    } else {
        key.into()
    }
}

/// The rows a query selects, probe rows among them: whether those count as requests is `probes_counted`,
/// which the sums take, so a probe's cost reaches the savings panel under every filter.
pub fn apply_filter(rows: Vec<RollupRow>, f: &RollupFilter, live: &LiveKeys) -> Vec<RollupRow> {
    let lower: Vec<String> = f.models.iter().map(|m| m.to_ascii_lowercase()).collect();
    rows.into_iter()
        .filter(|r| f.keys.is_empty() || f.keys.iter().any(|k| live.selects(k, r)))
        .filter(|r| f.pools.is_empty() || f.pools.contains(&r.pool_id))
        .filter(|r| f.accounts.is_empty() || f.accounts.contains(&r.account_id))
        .filter(|r| lower.is_empty() || lower.contains(&r.canonical_model))
        .filter(|r| f.kinds.is_empty() || f.kinds.contains(&r.kind))
        .filter(|r| {
            f.user_agents.is_empty()
                || f.user_agents
                    .iter()
                    .any(|p| crate::pattern::matches(p, &r.user_agent))
        })
        .filter(|r| {
            f.clients.is_empty()
                || f.clients
                    .iter()
                    .any(|p| crate::pattern::matches(p, &r.client))
        })
        .collect()
}

fn group_row(key: &str, sum: &RollupRow, probes: &RollupRow, label: String) -> GroupRow {
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
        requests: sum.requests,
        succeeded: sum.succeeded,
        rate_limited: sum.rate_limited,
        failed: sum.failed,
        exhausted: sum.exhausted,
        rejected: sum.rejected,
        attempts: sum.attempts,
        failovers: sum.failovers,
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
            failed: probes.failed + probes.rate_limited + probes.exhausted + probes.rejected,
            migrated: probes.probe_migrated,
            ok: probes.succeeded,
            skipped: probes.probes_skipped,
        },
        // The gross saving rides on the inference rows and the rest on the probe rows, so the probe side
        // comes from the probe sum: what the cache cost to keep warm is not a request the filter can drop.
        savings: Savings {
            gross_usd: sum.saved_usd_gross,
            probe_cost_usd: probes.probe_cost_usd,
            wasted_probes: probes.wasted_probes,
            wasted_probe_usd: probes.wasted_probe_usd,
            wasted_probe_reasons: probes.wasted_probe_reasons.clone(),
        },
    }
}

fn sum_groups(
    rows: &[&RollupRow],
    group_by: &str,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
) -> Vec<GroupRow> {
    // Two sums per group, and whether a non-probe row ever joined it: the request side, which a probe row
    // joins only when probes count as requests; the probe rows on their own, which the derived probe counts
    // and the probe side of the savings read; and the flag a keepalive credit needs, since it carries a real
    // kind but never adds to `requests`.
    let mut sums: BTreeMap<String, (RollupRow, RollupRow, bool)> = BTreeMap::new();
    for row in rows {
        let key = group_of(row, group_by, live, agent_groups);
        let entry = sums.entry(key.clone()).or_insert_with(|| {
            (
                RollupRow::key(0, &key, 0, "", "", "", "", "", "", "", ""),
                RollupRow::key(0, &key, 0, "", "", "", "", "", "", "", ""),
                false,
            )
        });
        if row.kind == "probe" {
            entry.1.add(row);
        } else {
            entry.2 = true;
        }
        if probes_counted || row.kind != "probe" {
            entry.0.add(row);
        }
    }
    // A group whose rows in the range are all probes never joins the request side under the default filter;
    // without this it would report zero requests rather than staying out of a breakdown that does not count
    // probes as requests. A group with any non-probe row, a keepalive credit included, stays.
    sums.retain(|_, (_, _, saw_non_probe)| probes_counted || *saw_non_probe);
    let mut out: Vec<GroupRow> = sums
        .iter()
        .map(|(k, (sum, probes, _))| {
            let label = if group_by == "key" {
                live.label_of(k)
            } else {
                label_of(k)
            };
            group_row(k, sum, probes, label)
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label).then(a.key.cmp(&b.key)));
    out
}

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
) -> Result<Vec<SeriesPoint>, LedgerError> {
    let first = (from / 1_000).div_euclid(bucket_seconds) * bucket_seconds;
    let last = hour_ceil(to);
    let count = (last - first).checked_div(bucket_seconds).unwrap_or(0);
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points; the most a chart carries is {MAX_POINTS}"
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
            groups: sum_groups(&in_bucket, group_by, probes_counted, live, agent_groups),
        });
        at += bucket_seconds;
    }
    Ok(points)
}

pub fn breakdown(
    rows: &[RollupRow],
    group_by: &str,
    probes_counted: bool,
    live: &LiveKeys,
    agent_groups: &[String],
) -> Vec<GroupRow> {
    sum_groups(
        &rows.iter().collect::<Vec<_>>(),
        group_by,
        probes_counted,
        live,
        agent_groups,
    )
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

pub fn dashboard(conn: &Connection, now: i64) -> Result<Dashboard, LedgerError> {
    let to_hour = (now / 1_000).div_euclid(3_600) * 3_600 + 3_600;
    let from_hour = to_hour - 24 * 3_600;
    let rows = read_rollups(conn, from_hour, to_hour)?;
    let rows: Vec<RollupRow> = rows.into_iter().filter(|r| r.kind != "probe").collect();
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut hourly = Vec::new();
    for hour in (from_hour..to_hour).step_by(3_600) {
        let mut point = HourPoint {
            at: hour * 1_000,
            cost: 0.0,
            errors: 0,
            requests: 0,
            tokens: 0,
        };
        for r in rows.iter().filter(|r| r.hour == hour) {
            point.requests += r.requests;
            point.errors += r.requests.saturating_sub(r.succeeded);
            point.cost += r.estimated_usd;
            point.tokens += r.input_tokens
                + r.cache_read_tokens
                + r.cache_write_5m_tokens
                + r.cache_write_1h_tokens
                + r.output_tokens;
        }
        hourly.push(point);
    }
    for r in &rows {
        for (code, n) in &r.errors {
            *errors.entry(code.clone()).or_default() += n;
        }
    }
    let requests: u64 = hourly.iter().map(|h| h.requests).sum();
    let failed: u64 = hourly.iter().map(|h| h.errors).sum();
    let (recent, _) = list_requests(
        conn,
        &RequestFilter::default(),
        &LiveKeys::default(),
        None,
        20,
    )?;
    Ok(Dashboard {
        requests_24h: requests,
        tokens_24h: hourly.iter().map(|h| h.tokens).sum(),
        tokens_cached_24h: rows.iter().map(|r| r.cache_read_tokens).sum(),
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
    let mut stmt = conn.prepare("SELECT q.account_id, q.bucket, q.window, q.utilization, q.reset_at, q.source FROM quota_samples q WHERE q.bucket >= ?1 AND q.bucket = (SELECT MAX(bucket) FROM quota_samples WHERE account_id = q.account_id AND window = q.window AND bucket >= ?1) ORDER BY q.account_id, q.window")?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(QuotaSampleRecord {
            account_id: r.get(0)?,
            bucket: r.get(1)?,
            window: r.get(2)?,
            utilization: r.get(3)?,
            reset_at: r.get(4)?,
            source: r.get(5)?,
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
    let mut stmt = conn.prepare(
        "SELECT r.key_id, r.key_generation, r.session_id, MIN(r.received_at), MAX(r.received_at), COUNT(*), \
         (SELECT canonical_model FROM requests r2 WHERE r2.key_id = r.key_id AND r2.key_generation = r.key_generation AND r2.session_id = r.session_id AND r2.kind != 'probe' ORDER BY r2.received_at DESC, r2.id DESC LIMIT 1), \
         (SELECT a.account_id FROM attempts a WHERE a.request_id = (SELECT r3.id FROM requests r3 WHERE r3.key_id = r.key_id AND r3.key_generation = r.key_generation AND r3.session_id = r.session_id AND r3.kind != 'probe' ORDER BY r3.received_at DESC, r3.id DESC LIMIT 1) ORDER BY a.attempt DESC LIMIT 1), \
         (SELECT r3.presented_retired FROM requests r3 WHERE r3.key_id = r.key_id AND r3.key_generation = r.key_generation AND r3.session_id = r.session_id AND r3.kind != 'probe' ORDER BY r3.received_at DESC, r3.id DESC LIMIT 1), \
         (SELECT r3.rotation_at FROM requests r3 WHERE r3.key_id = r.key_id AND r3.key_generation = r.key_generation AND r3.session_id = r.session_id AND r3.kind != 'probe' ORDER BY r3.received_at DESC, r3.id DESC LIMIT 1) \
         FROM requests r WHERE r.key_id IS NOT NULL AND r.session_id IS NOT NULL AND r.kind != 'probe' AND r.received_at >= ?1 \
         GROUP BY r.key_id, r.key_generation, r.session_id ORDER BY MAX(r.received_at) DESC",
    )?;
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrafficBucket {
    pub at: i64,
    pub requests: u64,
    pub failed: u64,
    pub tokens: u64,
    pub cost_usd: f64,
}

/// Every bucket in the range from the rollups of one pool, probes left out.
pub fn pool_traffic(
    conn: &Connection,
    pool_id: &str,
    from_hour: i64,
    to_hour: i64,
    bucket_seconds: i64,
) -> Result<Vec<TrafficBucket>, LedgerError> {
    let first = from_hour.div_euclid(bucket_seconds) * bucket_seconds;
    let count = (to_hour - first).checked_div(bucket_seconds).unwrap_or(0);
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points; the most a chart carries is {MAX_POINTS}"
        )));
    }
    let rows: Vec<RollupRow> = read_rollups(conn, from_hour, to_hour)?
        .into_iter()
        .filter(|r| r.pool_id == pool_id && r.kind != "probe")
        .collect();
    let mut out = Vec::new();
    let mut at = first;
    while at < to_hour {
        let mut bucket = TrafficBucket {
            at: at * 1_000,
            requests: 0,
            failed: 0,
            tokens: 0,
            cost_usd: 0.0,
        };
        for r in rows
            .iter()
            .filter(|r| r.hour >= at && r.hour < at + bucket_seconds)
        {
            bucket.requests += r.requests;
            bucket.failed += r.requests.saturating_sub(r.succeeded);
            bucket.tokens += r.input_tokens
                + r.cache_read_tokens
                + r.cache_write_5m_tokens
                + r.cache_write_1h_tokens
                + r.output_tokens;
            bucket.cost_usd += r.estimated_usd;
        }
        out.push(bucket);
        at += bucket_seconds;
    }
    Ok(out)
}

/// The range and scope of a series the query reads from the ledger itself, where `RollupFilter` narrows rows
/// a caller already loaded.
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
    let count = (last - first)
        .checked_div(filter.bucket_seconds)
        .unwrap_or(0);
    if !(0..=MAX_POINTS).contains(&count) {
        return Err(LedgerError::Yaml(format!(
            "the range needs {count} points; the most a chart carries is {MAX_POINTS}"
        )));
    }
    Ok((first, last))
}

/// One point of the burn series: the row's tokens as meter tokens (a subscription account) and dollars.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct BurnPoint {
    /// Milliseconds, like every other series' `at`.
    pub at: i64,
    pub meter_tokens: f64,
    pub usd: f64,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
}

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
    /// Milliseconds: the row's own hour for a rollup row, `started_at` for an attempt, since a bucket spanning
    /// more than one band edge needs the charge's own instant to say which side it landed on.
    at: i64,
}

/// Meter tokens (banded at the row's own instant) for an oauth account, `estimated_usd` for an api_key one; a
/// row whose account is not in `kinds` carries no price the caller could attribute, so it adds nothing at all.
fn price_usage(
    point: &mut BurnPoint,
    kind: Option<AccountKind>,
    factors: &PlanFactors,
    windows: &PlanWindows,
    row: &UsageRow,
) {
    match kind {
        Some(AccountKind::Oauth) => {
            if let Some(tokens) = factors.meter_tokens(
                row.canonical_model,
                row.input_tokens,
                row.cache_read_tokens,
                row.cache_write_5m_tokens,
                row.cache_write_1h_tokens,
                row.output_tokens,
            ) {
                point.meter_tokens += tokens * windows.band_factor(FIVE_HOUR, row.at);
            }
        }
        Some(AccountKind::ApiKey) => point.usd += row.estimated_usd,
        None => return,
    }
    point.cached_tokens += row.cache_read_tokens;
    point.uncached_tokens += row.input_tokens;
}

/// A subscription row's meter tokens carry `windows.band_factor(FIVE_HOUR, row.hour * 1_000)`, the weight the
/// fitted scale they are later divided by was built with; read per row rather than per bucket, since a
/// six-hour or daily bucket spans more than one band edge and only the row's own hour says which side of an
/// edge it fell on.
pub fn burn_series(
    conn: &Connection,
    factors: &PlanFactors,
    windows: &PlanWindows,
    kinds: &HashMap<String, AccountKind>,
    filter: &SeriesFilter,
) -> Result<Vec<BurnPoint>, LedgerError> {
    let (first, last) = bucket_range(filter)?;
    let rows = read_rollups(conn, first, last)?;
    let matches = |r: &RollupRow| {
        (filter.keys.is_empty() || filter.keys.contains(&r.key_id))
            && (filter.pools.is_empty() || filter.pools.contains(&r.pool_id))
            && (filter.accounts.is_empty() || filter.accounts.contains(&r.account_id))
            && (filter.request_classes.is_empty()
                || filter.request_classes.contains(&r.request_class))
    };
    let mut points = Vec::new();
    let mut at = first;
    while at < last {
        let mut point = BurnPoint {
            at: at * 1_000,
            ..BurnPoint::default()
        };
        for row in rows
            .iter()
            .filter(|r| r.hour >= at && r.hour < at + filter.bucket_seconds && matches(r))
        {
            price_usage(
                &mut point,
                kinds.get(&row.account_id).copied(),
                factors,
                windows,
                &UsageRow {
                    canonical_model: &row.canonical_model,
                    input_tokens: row.input_tokens,
                    cache_read_tokens: row.cache_read_tokens,
                    cache_write_5m_tokens: row.cache_write_5m_tokens,
                    cache_write_1h_tokens: row.cache_write_1h_tokens,
                    output_tokens: row.output_tokens,
                    estimated_usd: row.estimated_usd,
                    at: row.hour * 1_000,
                },
            );
        }
        points.push(point);
        at += filter.bucket_seconds;
    }
    Ok(points)
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

/// The `limit` sessions with the most tokens over `[from, to)`, ranked in SQL rather than in memory, so a
/// caller never reads an attempt row belonging to a session it will not answer.
fn top_sessions_by_tokens(
    conn: &Connection,
    from: i64,
    to: i64,
    limit: usize,
) -> Result<Vec<String>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT r.session_id FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE r.session_id IS NOT NULL AND a.started_at >= ?1 AND a.started_at < ?2 \
         GROUP BY r.session_id \
         ORDER BY SUM(COALESCE(a.input_tokens, 0) + COALESCE(a.cache_read_tokens, 0) \
             + COALESCE(a.cache_write_5m_tokens, 0) + COALESCE(a.cache_write_1h_tokens, 0) \
             + COALESCE(a.output_tokens, 0)) DESC, r.session_id \
         LIMIT ?3",
    )?;
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
    /// Milliseconds, `attempts.started_at`.
    at: i64,
    canonical_model: String,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
    estimated_usd: f64,
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
    let placeholders = sessions.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT r.session_id, a.account_id, a.started_at, r.canonical_model, \
         a.input_tokens, a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens, \
         a.output_tokens, a.estimated_usd \
         FROM attempts a JOIN requests r ON r.id = a.request_id \
         WHERE r.session_id IN ({placeholders}) AND a.started_at >= ? AND a.started_at < ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut bind: Vec<rusqlite::types::Value> = sessions
        .iter()
        .map(|s| rusqlite::types::Value::Text(s.clone()))
        .collect();
    bind.push(rusqlite::types::Value::Integer(from));
    bind.push(rusqlite::types::Value::Integer(to));
    let tokens = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
    let rows = stmt.query_map(params_from_iter(bind), |row| {
        Ok(SessionAttempt {
            session_id: row.get(0)?,
            account_id: row.get(1)?,
            at: row.get(2)?,
            canonical_model: row.get(3)?,
            input_tokens: tokens(row.get(4)?),
            cache_read_tokens: tokens(row.get(5)?),
            cache_write_5m_tokens: tokens(row.get(6)?),
            cache_write_1h_tokens: tokens(row.get(7)?),
            output_tokens: tokens(row.get(8)?),
            estimated_usd: row.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(LedgerError::from)
}

/// The twenty sessions with the most tokens over the filter's range, each priced per bucket the way
/// `burn_series` prices an account row. One pass over the range's attempts: each lands in its bucket by index,
/// rather than every session re-scanning every attempt for every bucket.
pub fn session_burn_series(
    conn: &Connection,
    factors: &PlanFactors,
    windows: &PlanWindows,
    kinds: &HashMap<String, AccountKind>,
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
            kinds.get(&a.account_id).copied(),
            factors,
            windows,
            &UsageRow {
                canonical_model: &a.canonical_model,
                input_tokens: a.input_tokens,
                cache_read_tokens: a.cache_read_tokens,
                cache_write_5m_tokens: a.cache_write_5m_tokens,
                cache_write_1h_tokens: a.cache_write_1h_tokens,
                output_tokens: a.output_tokens,
                estimated_usd: a.estimated_usd,
                at: a.at,
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
}
