//! Reads for the admin routes: the request log with its filters and cursor, the rollup series and breakdown
//! with merged histograms, the dashboard's day, and the newest quota sample per window.

use std::collections::BTreeMap;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};

use crate::db::{row_to_attempt, row_to_request, ATTEMPT_SELECT, REQUEST_SELECT};
use crate::histogram::Histogram;
use crate::record::{
    AttemptRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord, RequestRecord,
};
use crate::rollup::{read_rollups, RollupRow, UNATTRIBUTED};
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
    /// Matched as a substring, because a client's agent carries a build and a platform around the version an
    /// operator is usually looking for.
    pub user_agent: Option<String>,
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
        args.push(Box::new(format!("%{agent}%")));
        clauses.push(format!("user_agent LIKE ?{}", args.len()));
    }
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

#[derive(Clone, Debug, Default, Serialize)]
pub struct TokenSplit {
    pub cache_read: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub output: u64,
    pub uncached_input: u64,
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
    /// Matched in full, the way accounts are: an analytics drill-down comes from a group the breakdown named.
    pub user_agents: Vec<String>,
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
    "key",
    "kind",
    "model",
    "pool",
    "user_agent",
];

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

pub fn group_of(row: &RollupRow, group_by: &str, live: &LiveKeys) -> String {
    match group_by {
        "key" => live.key_of(row),
        "pool" => row.pool_id.clone(),
        "account" => row.account_id.clone(),
        "model" => row.canonical_model.clone(),
        "kind" => row.kind.clone(),
        "user_agent" => row.user_agent.clone(),
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
        .filter(|r| f.user_agents.is_empty() || f.user_agents.contains(&r.user_agent))
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
            uncached_input: sum.input_tokens,
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
) -> Vec<GroupRow> {
    // Two sums per group: the request side, which a probe row joins only when probes count as requests, and
    // the probe rows on their own, which the derived probe counts and the probe side of the savings read.
    let mut sums: BTreeMap<String, (RollupRow, RollupRow)> = BTreeMap::new();
    for row in rows {
        let key = group_of(row, group_by, live);
        let entry = sums.entry(key.clone()).or_insert_with(|| {
            (
                RollupRow::key(0, &key, 0, "", "", "", "", ""),
                RollupRow::key(0, &key, 0, "", "", "", "", ""),
            )
        });
        if row.kind == "probe" {
            entry.1.add(row);
        }
        if probes_counted || row.kind != "probe" {
            entry.0.add(row);
        }
    }
    let mut out: Vec<GroupRow> = sums
        .iter()
        .map(|(k, (sum, probes))| {
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

pub fn series(
    rows: &[RollupRow],
    from: i64,
    to: i64,
    bucket_seconds: i64,
    group_by: &str,
    probes_counted: bool,
    live: &LiveKeys,
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
            groups: sum_groups(&in_bucket, group_by, probes_counted, live),
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
) -> Vec<GroupRow> {
    sum_groups(
        &rows.iter().collect::<Vec<_>>(),
        group_by,
        probes_counted,
        live,
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
    pub session_id: String,
    pub first_request_at: i64,
    pub last_request_at: i64,
    pub requests: u64,
    pub model: String,
    pub account_id: Option<String>,
}

/// Every (key, session) with a client request since `since`, with the canonical model and the serving account
/// of its newest request.
pub fn session_activity(
    conn: &Connection,
    since: i64,
) -> Result<Vec<SessionActivity>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT r.key_id, r.session_id, MIN(r.received_at), MAX(r.received_at), COUNT(*), \
         (SELECT canonical_model FROM requests r2 WHERE r2.key_id = r.key_id AND r2.session_id = r.session_id AND r2.kind != 'probe' ORDER BY r2.received_at DESC, r2.id DESC LIMIT 1), \
         (SELECT a.account_id FROM attempts a WHERE a.request_id = (SELECT r3.id FROM requests r3 WHERE r3.key_id = r.key_id AND r3.session_id = r.session_id AND r3.kind != 'probe' ORDER BY r3.received_at DESC, r3.id DESC LIMIT 1) ORDER BY a.attempt DESC LIMIT 1) \
         FROM requests r WHERE r.key_id IS NOT NULL AND r.session_id IS NOT NULL AND r.kind != 'probe' AND r.received_at >= ?1 \
         GROUP BY r.key_id, r.session_id ORDER BY MAX(r.received_at) DESC",
    )?;
    let rows = stmt.query_map(params![since], |r| {
        Ok(SessionActivity {
            key_id: r.get(0)?,
            session_id: r.get(1)?,
            first_request_at: r.get(2)?,
            last_request_at: r.get(3)?,
            requests: r.get::<_, i64>(4)? as u64,
            model: r.get(5)?,
            account_id: r.get(6)?,
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
