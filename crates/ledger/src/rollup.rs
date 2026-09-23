//! The hourly fold: each completed request adds one delta to the row for its hour and its eight-part key, in
//! the same transaction as its raw rows, so a rollup never disagrees with the requests it summarises.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

#[cfg(test)]
use crate::histogram::bucket_index;
use crate::histogram::Histogram;
use crate::record::{AttemptRecord, EventRecord, KeepaliveDelta, RequestRecord};
use crate::LedgerError;

/// The reserved id for a key, pool, or account a request never reached, so the failures of an exhausted hour
/// have a row and the raw-versus-rollup equality holds.
pub const UNATTRIBUTED: &str = "-";

/// The value a request with no `output_config.effort` files under.
pub const NO_EFFORT: &str = "none";

/// How far past its announcement a cooldown is booked. The deadline is the upstream's to set, and the
/// classifier saturates an absurd `Retry-After` to `i64::MAX`, so booking every hour up to it would hold the
/// writer's transaction for good.
const COOLDOWN_BOOKING_HORIZON_MS: i64 = 7 * 24 * 3_600 * 1_000;

pub fn hour_of(at_ms: i64) -> i64 {
    at_ms.div_euclid(1_000).div_euclid(3_600) * 3_600
}

pub fn bucket_of(at_ms: i64) -> i64 {
    at_ms.div_euclid(1_000).div_euclid(300) * 300
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostSplit {
    pub input_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub output_usd: f64,
}

impl CostSplit {
    pub fn total(&self) -> f64 {
        self.input_usd + self.cache_read_usd + self.cache_write_usd + self.output_usd
    }

    /// The split a request was folded with is read back from its own rows, so a replace subtracts exactly what
    /// its first write added rather than a reconstruction.
    pub fn from_attempts(attempts: &[AttemptRecord]) -> CostSplit {
        let sum = |f: fn(&AttemptRecord) -> Option<f64>| attempts.iter().filter_map(f).sum::<f64>();
        CostSplit {
            input_usd: sum(|a| a.input_usd),
            cache_read_usd: sum(|a| a.cache_read_usd),
            cache_write_usd: sum(|a| a.cache_write_usd),
            output_usd: sum(|a| a.output_usd),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RollupRow {
    pub hour: i64,
    pub key_id: String,
    /// The `created_at` of the key the row belongs to, 0 for a row written before the column existed and never
    /// reconciled, or for the unattributed key.
    pub key_generation: i64,
    pub pool_id: String,
    pub account_id: String,
    pub canonical_model: String,
    pub kind: String,
    /// The `User-Agent` of the request the row folded, `UNATTRIBUTED` for a request that sent none.
    pub user_agent: String,
    /// The request's `output_config.effort`, `NO_EFFORT` when it sent none; a dimension, so it sits in the key.
    pub effort: String,
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
    pub uncached_requests: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub web_search_requests: u64,
    pub web_fetch_requests: u64,
    pub estimated_usd: f64,
    pub input_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub output_usd: f64,
    pub kept_warm_requests: u64,
    pub kept_warm_tokens: u64,
    pub saved_usd_gross: f64,
    pub probe_cost_usd: f64,
    pub probe_migrated: u64,
    pub wasted_probes: u64,
    pub wasted_probe_usd: f64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
    pub probes_skipped: u64,
    pub ttfb_hist: Histogram,
    pub ttfc_hist: Histogram,
    pub duration_hist: Histogram,
    pub output_tps_hist: Histogram,
    pub errors: BTreeMap<String, u64>,
    pub cache_miss_reasons: BTreeMap<String, u64>,
}

impl RollupRow {
    #[allow(clippy::too_many_arguments)]
    pub fn key(
        hour: i64,
        key_id: &str,
        key_generation: i64,
        pool_id: &str,
        account_id: &str,
        canonical_model: &str,
        kind: &str,
        user_agent: &str,
        effort: &str,
    ) -> RollupRow {
        RollupRow {
            hour,
            key_id: key_id.into(),
            key_generation,
            pool_id: pool_id.into(),
            account_id: account_id.into(),
            canonical_model: canonical_model.into(),
            kind: kind.into(),
            user_agent: user_agent.into(),
            effort: effort.into(),
            requests: 0,
            succeeded: 0,
            rate_limited: 0,
            failed: 0,
            exhausted: 0,
            rejected: 0,
            attempts: 0,
            failovers: 0,
            cache_hits: 0,
            cache_misses: 0,
            uncached_requests: 0,
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
            web_search_requests: 0,
            web_fetch_requests: 0,
            estimated_usd: 0.0,
            input_usd: 0.0,
            cache_read_usd: 0.0,
            cache_write_usd: 0.0,
            output_usd: 0.0,
            kept_warm_requests: 0,
            kept_warm_tokens: 0,
            saved_usd_gross: 0.0,
            probe_cost_usd: 0.0,
            probe_migrated: 0,
            wasted_probes: 0,
            wasted_probe_usd: 0.0,
            wasted_probe_reasons: BTreeMap::new(),
            probes_skipped: 0,
            ttfb_hist: Histogram::new(),
            ttfc_hist: Histogram::new(),
            duration_hist: Histogram::new(),
            output_tps_hist: Histogram::new(),
            errors: BTreeMap::new(),
            cache_miss_reasons: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, o: &RollupRow) {
        self.requests += o.requests;
        self.succeeded += o.succeeded;
        self.rate_limited += o.rate_limited;
        self.failed += o.failed;
        self.exhausted += o.exhausted;
        self.rejected += o.rejected;
        self.attempts += o.attempts;
        self.failovers += o.failovers;
        self.cache_hits += o.cache_hits;
        self.cache_misses += o.cache_misses;
        self.uncached_requests += o.uncached_requests;
        self.input_tokens += o.input_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.cache_write_5m_tokens += o.cache_write_5m_tokens;
        self.cache_write_1h_tokens += o.cache_write_1h_tokens;
        self.output_tokens += o.output_tokens;
        self.thinking_tokens += o.thinking_tokens;
        self.web_search_requests += o.web_search_requests;
        self.web_fetch_requests += o.web_fetch_requests;
        self.estimated_usd += o.estimated_usd;
        self.input_usd += o.input_usd;
        self.cache_read_usd += o.cache_read_usd;
        self.cache_write_usd += o.cache_write_usd;
        self.output_usd += o.output_usd;
        self.kept_warm_requests += o.kept_warm_requests;
        self.kept_warm_tokens += o.kept_warm_tokens;
        self.saved_usd_gross += o.saved_usd_gross;
        self.probe_cost_usd += o.probe_cost_usd;
        self.probe_migrated += o.probe_migrated;
        self.wasted_probes += o.wasted_probes;
        self.wasted_probe_usd += o.wasted_probe_usd;
        add_counts(&mut self.wasted_probe_reasons, &o.wasted_probe_reasons);
        self.probes_skipped += o.probes_skipped;
        self.ttfb_hist.merge(&o.ttfb_hist);
        self.ttfc_hist.merge(&o.ttfc_hist);
        self.duration_hist.merge(&o.duration_hist);
        self.output_tps_hist.merge(&o.output_tps_hist);
        add_counts(&mut self.errors, &o.errors);
        add_counts(&mut self.cache_miss_reasons, &o.cache_miss_reasons);
    }
}

impl RollupRow {
    /// The inverse of `add` for a contribution that was added before; counters saturate at zero so a row that
    /// drifted cannot go negative, and a count that reaches zero leaves the map.
    pub fn subtract(&mut self, o: &RollupRow) {
        self.requests = self.requests.saturating_sub(o.requests);
        self.succeeded = self.succeeded.saturating_sub(o.succeeded);
        self.rate_limited = self.rate_limited.saturating_sub(o.rate_limited);
        self.failed = self.failed.saturating_sub(o.failed);
        self.exhausted = self.exhausted.saturating_sub(o.exhausted);
        self.rejected = self.rejected.saturating_sub(o.rejected);
        self.attempts = self.attempts.saturating_sub(o.attempts);
        self.failovers = self.failovers.saturating_sub(o.failovers);
        self.cache_hits = self.cache_hits.saturating_sub(o.cache_hits);
        self.cache_misses = self.cache_misses.saturating_sub(o.cache_misses);
        self.uncached_requests = self.uncached_requests.saturating_sub(o.uncached_requests);
        self.input_tokens = self.input_tokens.saturating_sub(o.input_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_sub(o.cache_read_tokens);
        self.cache_write_5m_tokens = self
            .cache_write_5m_tokens
            .saturating_sub(o.cache_write_5m_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_sub(o.cache_write_1h_tokens);
        self.output_tokens = self.output_tokens.saturating_sub(o.output_tokens);
        self.thinking_tokens = self.thinking_tokens.saturating_sub(o.thinking_tokens);
        self.web_search_requests = self
            .web_search_requests
            .saturating_sub(o.web_search_requests);
        self.web_fetch_requests = self.web_fetch_requests.saturating_sub(o.web_fetch_requests);
        self.estimated_usd -= o.estimated_usd;
        self.input_usd -= o.input_usd;
        self.cache_read_usd -= o.cache_read_usd;
        self.cache_write_usd -= o.cache_write_usd;
        self.output_usd -= o.output_usd;
        self.kept_warm_requests = self.kept_warm_requests.saturating_sub(o.kept_warm_requests);
        self.kept_warm_tokens = self.kept_warm_tokens.saturating_sub(o.kept_warm_tokens);
        self.saved_usd_gross -= o.saved_usd_gross;
        self.probe_cost_usd -= o.probe_cost_usd;
        self.probe_migrated = self.probe_migrated.saturating_sub(o.probe_migrated);
        self.wasted_probes = self.wasted_probes.saturating_sub(o.wasted_probes);
        self.wasted_probe_usd -= o.wasted_probe_usd;
        subtract_counts(&mut self.wasted_probe_reasons, &o.wasted_probe_reasons);
        self.probes_skipped = self.probes_skipped.saturating_sub(o.probes_skipped);
        self.ttfb_hist.subtract(&o.ttfb_hist);
        self.ttfc_hist.subtract(&o.ttfc_hist);
        self.duration_hist.subtract(&o.duration_hist);
        self.output_tps_hist.subtract(&o.output_tps_hist);
        subtract_counts(&mut self.errors, &o.errors);
        subtract_counts(&mut self.cache_miss_reasons, &o.cache_miss_reasons);
    }
}

fn add_counts(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (k, v) in from {
        *into.entry(k.clone()).or_default() += v;
    }
}

fn subtract_counts(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (k, v) in from {
        if let Some(n) = into.get_mut(k) {
            *n = n.saturating_sub(*v);
            if *n == 0 {
                into.remove(k);
            }
        }
    }
}

pub fn delta_of(request: &RequestRecord, attempts: &[AttemptRecord]) -> RollupRow {
    let costs = CostSplit::from_attempts(attempts);
    let last = attempts.last();
    let mut d = RollupRow::key(
        hour_of(request.received_at),
        request.key_id.as_deref().unwrap_or(UNATTRIBUTED),
        request.key_generation,
        request.pool_id.as_deref().unwrap_or(UNATTRIBUTED),
        last.map(|a| a.account_id.as_str()).unwrap_or(UNATTRIBUTED),
        &request.canonical_model,
        &request.kind,
        request.user_agent.as_deref().unwrap_or(UNATTRIBUTED),
        request.effort.as_deref().unwrap_or(NO_EFFORT),
    );
    d.requests = 1;
    match request.outcome.as_str() {
        "succeeded" => d.succeeded = 1,
        "failed" if request.status == Some(429) => d.rate_limited = 1,
        "failed" | "canceled" => d.failed = 1,
        "exhausted" => d.exhausted = 1,
        "rejected" => d.rejected = 1,
        _ => {}
    }
    d.attempts = attempts.len() as u64;
    // A 401 replay and an unsent retry both run again on the account the request was already on, so only a
    // move between accounts counts.
    d.failovers = attempts
        .windows(2)
        .filter(|pair| pair[0].account_id != pair[1].account_id)
        .count() as u64;
    let read = last.and_then(|a| a.cache_read_tokens).unwrap_or(0);
    let wrote = last.and_then(|a| a.cache_write_5m_tokens).unwrap_or(0)
        + last.and_then(|a| a.cache_write_1h_tokens).unwrap_or(0);
    if read > 0 {
        d.cache_hits = 1
    } else if wrote > 0 {
        d.cache_misses = 1
    } else {
        d.uncached_requests = 1
    }
    if let Some(a) = last {
        d.input_tokens = a.input_tokens.unwrap_or(0);
        d.cache_read_tokens = a.cache_read_tokens.unwrap_or(0);
        d.cache_write_5m_tokens = a.cache_write_5m_tokens.unwrap_or(0);
        d.cache_write_1h_tokens = a.cache_write_1h_tokens.unwrap_or(0);
        d.output_tokens = a.output_tokens.unwrap_or(0);
        d.thinking_tokens = a.thinking_tokens.unwrap_or(0);
        d.web_search_requests = a.web_search_requests.unwrap_or(0);
        d.web_fetch_requests = a.web_fetch_requests.unwrap_or(0);
        if let Some(reason) = &a.cache_miss_reason {
            *d.cache_miss_reasons.entry(reason.clone()).or_default() += 1;
            if reason == "none" && read == 0 {
                *d.cache_miss_reasons
                    .entry("expired_between_turns".into())
                    .or_default() += 1;
            }
        }
    }
    // Folded onto a positive zero rather than summed: `Sum<f64>` starts at -0.0, so an hour of unpriced
    // requests would carry a signed zero into the rollups and out to the wire.
    d.estimated_usd = attempts
        .iter()
        .filter_map(|a| a.estimated_usd)
        .fold(0.0, |a, b| a + b);
    d.input_usd = costs.input_usd;
    d.cache_read_usd = costs.cache_read_usd;
    d.cache_write_usd = costs.cache_write_usd;
    d.output_usd = costs.output_usd;
    if let Some(t) = request.ttfb_ms {
        d.ttfb_hist.record(t as f64);
    }
    if let Some(t) = request.ttfc_ms {
        d.ttfc_hist.record(t as f64);
    }
    if let Some(t) = request.duration_ms {
        d.duration_hist.record(t as f64);
    }
    if let (Some(out), Some(duration), Some(ttfc)) = (
        last.and_then(|a| a.output_tokens),
        request.duration_ms,
        request.ttfc_ms,
    ) {
        let span_s = (duration - ttfc) as f64 / 1_000.0;
        if span_s > 0.0 {
            d.output_tps_hist.record(out as f64 / span_s);
        }
    }
    if let Some(code) = &request.error_code {
        *d.errors.entry(code.clone()).or_default() += 1;
    }
    d
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountRollupRow {
    pub hour: i64,
    pub account_id: String,
    pub utilization_5h_max: Option<f64>,
    pub utilization_7d_max: Option<f64>,
    pub utilization_7d_oi_max: Option<f64>,
    /// Seconds of overage that ended in this hour: the whole span lands where it ended, so a range total is
    /// right and one hour's value can exceed 3,600.
    pub overage_seconds: u64,
    /// Seconds of cooldown booked to the hour each part elapses in; more than one scope cooling down at once
    /// can still push one hour's value past 3,600.
    pub cooldown_seconds: u64,
    pub pins_created: u64,
    pub pins_migrated_in: u64,
    pub pins_migrated_out: u64,
}

impl AccountRollupRow {
    pub fn add(&mut self, o: &AccountRollupRow) {
        let max = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, None) => a,
            (None, b) => b,
        };
        self.utilization_5h_max = max(self.utilization_5h_max, o.utilization_5h_max);
        self.utilization_7d_max = max(self.utilization_7d_max, o.utilization_7d_max);
        self.utilization_7d_oi_max = max(self.utilization_7d_oi_max, o.utilization_7d_oi_max);
        self.overage_seconds += o.overage_seconds;
        self.cooldown_seconds += o.cooldown_seconds;
        self.pins_created += o.pins_created;
        self.pins_migrated_in += o.pins_migrated_in;
        self.pins_migrated_out += o.pins_migrated_out;
    }
}

pub fn fold_request(
    tx: &Transaction<'_>,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    previous: Option<(&RequestRecord, &[AttemptRecord])>,
) -> Result<(), LedgerError> {
    // A replaced request first leaves the row it was folded into, under the key it had then: the same id can
    // land under another account or hour once its attempts are known.
    if let Some((old_request, old_attempts)) = previous {
        let old = delta_of(old_request, old_attempts);
        if let Some(mut row) = read_rollup(tx, &old)? {
            row.subtract(&old);
            // Every delta sets `requests` to 1, so zero here means the key holds no request any more: a
            // replace that moved keys, not one that landed back on its own row. Leaving a row of zeros would
            // surface as a phantom group in every reader that lists keys rather than summing values.
            //
            // A request without a user agent shares the reserved row with its key's keepalive deltas, which
            // no request row backs; the row goes only when nothing of either is left in it.
            let keepalive_empty = row.kept_warm_requests == 0
                && row.kept_warm_tokens == 0
                && row.saved_usd_gross == 0.0
                && row.probe_cost_usd == 0.0
                && row.probe_migrated == 0
                && row.probes_skipped == 0
                && row.wasted_probes == 0
                && row.wasted_probe_usd == 0.0;
            if row.requests == 0 && keepalive_empty {
                delete_rollup(tx, &row)?;
            } else {
                write_rollup(tx, &row)?;
            }
        }
    }
    let delta = delta_of(request, attempts);
    let mut row = read_rollup(tx, &delta)?.unwrap_or_else(|| {
        RollupRow::key(
            delta.hour,
            &delta.key_id,
            delta.key_generation,
            &delta.pool_id,
            &delta.account_id,
            &delta.canonical_model,
            &delta.kind,
            &delta.user_agent,
            &delta.effort,
        )
    });
    row.add(&delta);
    write_rollup(tx, &row)?;
    for a in attempts {
        if reads_utilization(a) {
            fold_account(
                tx,
                &AccountRollupRow {
                    hour: hour_of(a.started_at),
                    account_id: a.account_id.clone(),
                    utilization_5h_max: a.ratelimit_5h_utilization,
                    utilization_7d_max: a.ratelimit_7d_utilization,
                    utilization_7d_oi_max: a.ratelimit_7d_oi_utilization,
                    ..Default::default()
                },
            )?;
        }
    }
    // The three utilization columns are maxima, so a replaced attempt's reading cannot be subtracted out of
    // them: every hour and account its old attempts fed is read again from the raw attempts that remain.
    if let Some((_, old_attempts)) = previous {
        let touched: BTreeSet<(i64, &str)> = old_attempts
            .iter()
            .filter(|a| reads_utilization(a))
            .map(|a| (hour_of(a.started_at), a.account_id.as_str()))
            .collect();
        for (hour, account_id) in touched {
            reread_utilization(tx, hour, account_id)?;
        }
    }
    Ok(())
}

fn reads_utilization(a: &AttemptRecord) -> bool {
    a.ratelimit_5h_utilization.is_some()
        || a.ratelimit_7d_utilization.is_some()
        || a.ratelimit_7d_oi_utilization.is_some()
}

/// One hour and account's utilization maxima, taken again from the raw attempts and written back over the
/// three columns alone: the counters beside them come from cooldown, overage and pin events, which no
/// replaced request has anything to say about.
fn reread_utilization(
    tx: &Transaction<'_>,
    hour: i64,
    account_id: &str,
) -> Result<(), LedgerError> {
    let (five, seven, oi) = tx.query_row(
        "SELECT MAX(ratelimit_5h_utilization), MAX(ratelimit_7d_utilization), MAX(ratelimit_7d_oi_utilization) \
         FROM attempts WHERE account_id = ?1 AND started_at >= ?2 AND started_at < ?3",
        params![account_id, hour * 1_000, (hour + 3_600) * 1_000],
        |r| {
            Ok((
                r.get::<_, Option<f64>>(0)?,
                r.get::<_, Option<f64>>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        },
    )?;
    tx.execute(
        "UPDATE account_rollups_hourly SET utilization_5h_max = ?1, utilization_7d_max = ?2, utilization_7d_oi_max = ?3 \
         WHERE hour = ?4 AND account_id = ?5",
        params![five, seven, oi, hour, account_id],
    )?;
    Ok(())
}

/// The keepalive engine's own delta: one row of its key, holding what a kept-warm request saved, what a probe
/// cost, and what a stopped recipe wasted. The user agent is the reserved id, since a probe's own request row
/// carries the agent the recipe replays and a credit belongs to the key rather than to one caller.
pub fn fold_keepalive(tx: &Transaction<'_>, delta: &KeepaliveDelta) -> Result<(), LedgerError> {
    let key = RollupRow::key(
        hour_of(delta.at),
        &delta.key_id,
        delta.key_generation,
        &delta.pool_id,
        &delta.account_id,
        &delta.canonical_model,
        &delta.kind,
        UNATTRIBUTED,
        NO_EFFORT,
    );
    let mut row = read_rollup(tx, &key)?.unwrap_or(key);
    row.kept_warm_requests += delta.kept_warm_requests;
    row.kept_warm_tokens += delta.kept_warm_tokens;
    row.saved_usd_gross += delta.saved_usd_gross;
    row.probe_cost_usd += delta.probe_cost_usd;
    row.probe_migrated += delta.probe_migrated;
    row.probes_skipped += delta.probes_skipped;
    row.wasted_probes += delta.wasted_probes;
    row.wasted_probe_usd += delta.wasted_probe_usd;
    add_counts(&mut row.wasted_probe_reasons, &delta.wasted_probe_reasons);
    write_rollup(tx, &row)?;
    if let Some(id) = &delta.kept_warm_request_id {
        // The engine credits at completion while the writer batches, so the request's own row may land after
        // this mark; the insert consults the marks table for exactly that case.
        tx.execute(
            "UPDATE requests SET kept_warm = 1 WHERE id = ?1",
            params![id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO kept_warm_marks (request_id, marked_at) VALUES (?1, ?2)",
            params![id, delta.at],
        )?;
    }
    Ok(())
}

pub fn fold_event(tx: &Transaction<'_>, event: &EventRecord) -> Result<(), LedgerError> {
    let detail: serde_json::Value = event
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or(serde_json::Value::Null);
    let hour = hour_of(event.at);
    let row = |account_id: &str| AccountRollupRow {
        hour,
        account_id: account_id.into(),
        ..Default::default()
    };
    match (event.kind.as_str(), event.account_id.as_deref()) {
        ("pin_created", Some(account)) => fold_account(
            tx,
            &AccountRollupRow {
                pins_created: 1,
                ..row(account)
            },
        ),
        ("pin_migrated", Some(to)) => {
            fold_account(
                tx,
                &AccountRollupRow {
                    pins_migrated_in: 1,
                    ..row(to)
                },
            )?;
            match detail["from_account"].as_str() {
                Some(from) => fold_account(
                    tx,
                    &AccountRollupRow {
                        pins_migrated_out: 1,
                        ..row(from)
                    },
                ),
                None => Ok(()),
            }
        }
        ("cooldown_set", Some(account)) => {
            let scope = detail["scope"].as_str().unwrap_or("account");
            let accounted = read_span(tx, account, scope)?;
            let Some(until) = detail["until"].as_i64() else {
                return Ok(());
            };
            // Only what this announcement adds past the end already counted, and past now, is new time in
            // cooldown; it is booked hour by hour so a clear can take back exactly the part each hour never
            // saw. The span keeps the booked end rather than the deadline, so a later announcement books on
            // from there and a clear takes back only what was booked.
            let booked = until.min(event.at.saturating_add(COOLDOWN_BOOKING_HORIZON_MS));
            let from = accounted.unwrap_or(event.at).max(event.at);
            let mut cursor = from;
            while cursor < booked {
                let bucket = hour_of(cursor);
                let end = booked.min((bucket + 3_600) * 1_000);
                let seconds = ((end - cursor) / 1_000) as u64;
                if seconds > 0 {
                    fold_account(
                        tx,
                        &AccountRollupRow {
                            hour: bucket,
                            cooldown_seconds: seconds,
                            ..row(account)
                        },
                    )?;
                    add_segment(tx, account, scope, bucket, seconds)?;
                }
                cursor = end;
            }
            write_span(tx, account, scope, accounted.unwrap_or(booked).max(booked))
        }
        ("cooldown_cleared", Some(account)) => {
            let scope = detail["scope"].as_str().unwrap_or("account");
            let Some(until) = take_span(tx, account, scope)? else {
                return Ok(());
            };
            for (bucket, counted) in take_segments(tx, account, scope)? {
                // The part of this hour's segment that lies after the clear never elapsed.
                let start_ms = bucket * 1_000;
                let end_ms = (bucket + 3_600) * 1_000;
                let unelapsed =
                    ((until.min(end_ms) - event.at.max(start_ms)).max(0) / 1_000) as u64;
                let seconds = unelapsed.min(counted);
                if seconds > 0 {
                    unfold_cooldown(tx, bucket, account, seconds)?;
                }
            }
            Ok(())
        }
        ("overage_ended", Some(account)) => {
            let seconds = detail["since"]
                .as_i64()
                .map(|since| (event.at - since).max(0) / 1_000)
                .unwrap_or(0) as u64;
            fold_account(
                tx,
                &AccountRollupRow {
                    overage_seconds: seconds,
                    ..row(account)
                },
            )
        }
        _ => Ok(()),
    }
}

pub fn fold_account(tx: &Transaction<'_>, delta: &AccountRollupRow) -> Result<(), LedgerError> {
    let mut row = account_row(tx, delta.hour, &delta.account_id)?;
    row.add(delta);
    write_account(tx, &row)
}

fn account_row(
    tx: &Transaction<'_>,
    hour: i64,
    account_id: &str,
) -> Result<AccountRollupRow, LedgerError> {
    Ok(tx
        .query_row(
            &format!("{ACCOUNT_SELECT} WHERE hour = ?1 AND account_id = ?2"),
            params![hour, account_id],
            row_to_account,
        )
        .optional()?
        .unwrap_or_else(|| AccountRollupRow {
            hour,
            account_id: account_id.to_string(),
            ..Default::default()
        }))
}

fn write_account(tx: &Transaction<'_>, row: &AccountRollupRow) -> Result<(), LedgerError> {
    tx.execute(
        "INSERT OR REPLACE INTO account_rollups_hourly (hour, account_id, utilization_5h_max, utilization_7d_max, utilization_7d_oi_max, overage_seconds, cooldown_seconds, pins_created, pins_migrated_in, pins_migrated_out) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![row.hour, row.account_id, row.utilization_5h_max, row.utilization_7d_max, row.utilization_7d_oi_max, row.overage_seconds as i64, row.cooldown_seconds as i64, row.pins_created as i64, row.pins_migrated_in as i64, row.pins_migrated_out as i64],
    )?;
    Ok(())
}

/// Takes back cooldown seconds an announcement counted that the account never spent.
fn unfold_cooldown(
    tx: &Transaction<'_>,
    hour: i64,
    account_id: &str,
    seconds: u64,
) -> Result<(), LedgerError> {
    let mut row = account_row(tx, hour, account_id)?;
    row.cooldown_seconds = row.cooldown_seconds.saturating_sub(seconds);
    write_account(tx, &row)
}

/// The end one scope's cooldown is currently accounted to, for the fold to measure the next announcement
/// against.
fn read_span(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
) -> Result<Option<i64>, LedgerError> {
    Ok(tx
        .query_row(
            "SELECT until FROM cooldown_spans WHERE account_id = ?1 AND scope = ?2",
            params![account_id, scope],
            |r| r.get(0),
        )
        .optional()?)
}

/// The `hour` column stays for a ledger opened at an older schema; nothing reads it now that `cooldown_segments`
/// books each hour's contribution on its own.
fn write_span(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
    until: i64,
) -> Result<(), LedgerError> {
    tx.execute(
        "INSERT OR REPLACE INTO cooldown_spans (account_id, scope, hour, until) VALUES (?1, ?2, 0, ?3)",
        params![account_id, scope, until],
    )?;
    Ok(())
}

fn take_span(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
) -> Result<Option<i64>, LedgerError> {
    let span = read_span(tx, account_id, scope)?;
    if span.is_some() {
        tx.execute(
            "DELETE FROM cooldown_spans WHERE account_id = ?1 AND scope = ?2",
            params![account_id, scope],
        )?;
    }
    Ok(span)
}

/// Adds to an hour's booked cooldown seconds for a scope, so a later announcement in the same hour accumulates
/// rather than overwrites.
fn add_segment(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
    hour: i64,
    seconds: u64,
) -> Result<(), LedgerError> {
    tx.execute(
        "INSERT INTO cooldown_segments (account_id, scope, hour, seconds) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, scope, hour) DO UPDATE SET seconds = seconds + excluded.seconds",
        params![account_id, scope, hour, seconds as i64],
    )?;
    Ok(())
}

/// Every hour a scope's open cooldown booked seconds into, removed so a clear reads each exactly once.
fn take_segments(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
) -> Result<Vec<(i64, u64)>, LedgerError> {
    let segments = {
        let mut stmt = tx.prepare(
            "SELECT hour, seconds FROM cooldown_segments WHERE account_id = ?1 AND scope = ?2",
        )?;
        let rows = stmt.query_map(params![account_id, scope], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute(
        "DELETE FROM cooldown_segments WHERE account_id = ?1 AND scope = ?2",
        params![account_id, scope],
    )?;
    Ok(segments)
}

const COLUMNS: &str = "hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, effort, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json";
const ACCOUNT_SELECT: &str = "SELECT hour, account_id, utilization_5h_max, utilization_7d_max, utilization_7d_oi_max, overage_seconds, cooldown_seconds, pins_created, pins_migrated_in, pins_migrated_out FROM account_rollups_hourly";

pub(crate) fn read_rollup(
    conn: &Connection,
    key: &RollupRow,
) -> Result<Option<RollupRow>, LedgerError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM rollups_hourly WHERE hour = ?1 AND key_id = ?2 AND pool_id = ?3 AND account_id = ?4 AND canonical_model = ?5 AND kind = ?6 AND user_agent = ?7 AND key_generation = ?8 AND effort = ?9"),
            params![key.hour, key.key_id, key.pool_id, key.account_id, key.canonical_model, key.kind, key.user_agent, key.key_generation, key.effort],
            row_to_rollup,
        )
        .optional()?)
}

pub(crate) fn delete_rollup(conn: &Connection, key: &RollupRow) -> Result<(), LedgerError> {
    conn.execute(
        "DELETE FROM rollups_hourly WHERE hour = ?1 AND key_id = ?2 AND pool_id = ?3 AND account_id = ?4 AND canonical_model = ?5 AND kind = ?6 AND user_agent = ?7 AND key_generation = ?8 AND effort = ?9",
        params![key.hour, key.key_id, key.pool_id, key.account_id, key.canonical_model, key.kind, key.user_agent, key.key_generation, key.effort],
    )?;
    Ok(())
}

pub(crate) fn write_rollup(conn: &Connection, r: &RollupRow) -> Result<(), LedgerError> {
    let counts =
        |m: &BTreeMap<String, u64>| serde_json::to_string(m).unwrap_or_else(|_| "{}".into());
    conn.execute(
        &format!("INSERT OR REPLACE INTO rollups_hourly ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48)"),
        params![
            r.hour, r.key_id, r.key_generation, r.pool_id, r.account_id, r.canonical_model, r.kind, r.user_agent, r.effort, r.requests as i64, r.succeeded as i64, r.rate_limited as i64, r.failed as i64,
            r.exhausted as i64, r.rejected as i64, r.attempts as i64, r.failovers as i64, r.cache_hits as i64, r.cache_misses as i64, r.uncached_requests as i64,
            r.input_tokens as i64, r.cache_read_tokens as i64, r.cache_write_5m_tokens as i64, r.cache_write_1h_tokens as i64, r.output_tokens as i64,
            r.thinking_tokens as i64, r.web_search_requests as i64, r.web_fetch_requests as i64,
            r.estimated_usd, r.input_usd, r.cache_read_usd, r.cache_write_usd, r.output_usd, r.kept_warm_requests as i64, r.kept_warm_tokens as i64,
            r.saved_usd_gross, r.probe_cost_usd, r.probe_migrated as i64, r.wasted_probes as i64, r.wasted_probe_usd, counts(&r.wasted_probe_reasons),
            r.probes_skipped as i64, r.ttfb_hist.to_le_bytes(), r.ttfc_hist.to_le_bytes(), r.duration_hist.to_le_bytes(), r.output_tps_hist.to_le_bytes(), counts(&r.errors), counts(&r.cache_miss_reasons),
        ],
    )?;
    Ok(())
}

fn row_to_rollup(r: &rusqlite::Row<'_>) -> rusqlite::Result<RollupRow> {
    row_to_rollup_at(r, 0)
}

/// The row's columns read from `offset`, so a select that puts something of its own in front of `COLUMNS`
/// needs no second mapper.
fn row_to_rollup_at(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<RollupRow> {
    let u = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, i64>(offset + i)? as u64) };
    let counts = |i: usize| -> rusqlite::Result<BTreeMap<String, u64>> {
        Ok(serde_json::from_str(&r.get::<_, String>(offset + i)?).unwrap_or_default())
    };
    let hist = |i: usize| -> rusqlite::Result<Histogram> {
        Ok(Histogram::from_le_bytes(&r.get::<_, Vec<u8>>(offset + i)?))
    };
    Ok(RollupRow {
        hour: r.get(offset)?,
        key_id: r.get(offset + 1)?,
        key_generation: r.get(offset + 2)?,
        pool_id: r.get(offset + 3)?,
        account_id: r.get(offset + 4)?,
        canonical_model: r.get(offset + 5)?,
        kind: r.get(offset + 6)?,
        user_agent: r.get(offset + 7)?,
        effort: r.get(offset + 8)?,
        requests: u(9)?,
        succeeded: u(10)?,
        rate_limited: u(11)?,
        failed: u(12)?,
        exhausted: u(13)?,
        rejected: u(14)?,
        attempts: u(15)?,
        failovers: u(16)?,
        cache_hits: u(17)?,
        cache_misses: u(18)?,
        uncached_requests: u(19)?,
        input_tokens: u(20)?,
        cache_read_tokens: u(21)?,
        cache_write_5m_tokens: u(22)?,
        cache_write_1h_tokens: u(23)?,
        output_tokens: u(24)?,
        thinking_tokens: u(25)?,
        web_search_requests: u(26)?,
        web_fetch_requests: u(27)?,
        estimated_usd: r.get(offset + 28)?,
        input_usd: r.get(offset + 29)?,
        cache_read_usd: r.get(offset + 30)?,
        cache_write_usd: r.get(offset + 31)?,
        output_usd: r.get(offset + 32)?,
        kept_warm_requests: u(33)?,
        kept_warm_tokens: u(34)?,
        saved_usd_gross: r.get(offset + 35)?,
        probe_cost_usd: r.get(offset + 36)?,
        probe_migrated: u(37)?,
        wasted_probes: u(38)?,
        wasted_probe_usd: r.get(offset + 39)?,
        wasted_probe_reasons: counts(40)?,
        probes_skipped: u(41)?,
        ttfb_hist: hist(42)?,
        ttfc_hist: hist(43)?,
        duration_hist: hist(44)?,
        output_tps_hist: hist(45)?,
        errors: counts(46)?,
        cache_miss_reasons: counts(47)?,
    })
}

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRollupRow> {
    Ok(AccountRollupRow {
        hour: r.get(0)?,
        account_id: r.get(1)?,
        utilization_5h_max: r.get(2)?,
        utilization_7d_max: r.get(3)?,
        utilization_7d_oi_max: r.get(4)?,
        overage_seconds: r.get::<_, i64>(5)? as u64,
        cooldown_seconds: r.get::<_, i64>(6)? as u64,
        pins_created: r.get::<_, i64>(7)? as u64,
        pins_migrated_in: r.get::<_, i64>(8)? as u64,
        pins_migrated_out: r.get::<_, i64>(9)? as u64,
    })
}

pub fn read_rollups(
    conn: &Connection,
    from_hour: i64,
    to_hour: i64,
) -> Result<Vec<RollupRow>, LedgerError> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM rollups_hourly WHERE hour >= ?1 AND hour < ?2 ORDER BY hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, effort"))?;
    let rows = stmt.query_map(params![from_hour, to_hour], row_to_rollup)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One page of one key's rows at one generation, for the startup stamp that moves the unstamped rows of a
/// live key: `rollups_hourly` is a rowid table, so the page after `after_rowid` is the next `limit` rows in
/// rowid order and the caller carries the last rowid forward. `from_hour` narrows to the rows at or after an
/// hour, for an id whose earlier holder's rows sit under the same generation.
pub(crate) fn read_rollups_of_key_generation(
    conn: &Connection,
    key_id: &str,
    key_generation: i64,
    from_hour: Option<i64>,
    after_rowid: i64,
    limit: usize,
) -> Result<Vec<(i64, RollupRow)>, LedgerError> {
    let mut stmt = conn.prepare(&format!("SELECT rowid, {COLUMNS} FROM rollups_hourly WHERE key_id = ?1 AND key_generation = ?2 AND hour >= ?3 AND rowid > ?4 ORDER BY rowid LIMIT ?5"))?;
    let rows = stmt.query_map(
        params![
            key_id,
            key_generation,
            from_hour.unwrap_or(i64::MIN),
            after_rowid,
            limit as i64
        ],
        |r| Ok((r.get::<_, i64>(0)?, row_to_rollup_at(r, 1)?)),
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What the keyed read narrows by, spelled once so the plan test explains the predicate the read runs and not
/// a copy of it that could drift from it.
pub const KEY_RANGE_PREDICATE: &str =
    "key_id = ?1 AND key_generation = ?2 AND hour >= ?3 AND hour < ?4";

/// The rows of one key over a range, narrowed by SQLite rather than in memory: a reader that wants a single
/// key pays for that key's rows and not for every key's.
pub fn read_rollups_for_key(
    conn: &Connection,
    key_id: &str,
    key_generation: i64,
    from_hour: i64,
    to_hour: i64,
) -> Result<Vec<RollupRow>, LedgerError> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM rollups_hourly WHERE {KEY_RANGE_PREDICATE} ORDER BY hour, pool_id, account_id, canonical_model, kind, user_agent, effort"))?;
    let rows = stmt.query_map(
        params![key_id, key_generation, from_hour, to_hour],
        row_to_rollup,
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn read_account_rollups(
    conn: &Connection,
    from_hour: i64,
    to_hour: i64,
) -> Result<Vec<AccountRollupRow>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "{ACCOUNT_SELECT} WHERE hour >= ?1 AND hour < ?2 ORDER BY hour, account_id"
    ))?;
    let rows = stmt.query_map(params![from_hour, to_hour], row_to_account)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::read_version;
    use crate::record::*;

    /// The rollup columns as version 2 held them, which is the shape a stored database still has when the
    /// migration runs over it.
    fn v2_columns() -> String {
        COLUMNS
            .replace(", user_agent", "")
            .replace(", effort", "")
            .replace(
                ", thinking_tokens, web_search_requests, web_fetch_requests",
                "",
            )
            .replace(", probes_skipped", "")
            .replace(", key_generation", "")
    }

    /// One row per version-2 column, in that order.
    const V2_ROW: &str = "3600, 'k', 'p', 'a', 'm', 'inference', 5, 5, 0, 0, 0, 0, 5, 0, 0, 0, 5, \
         1, 2, 3, 4, 6, 0.5, 0.1, 0.1, 0.1, 0.2, 0, 0, 0.0, 0.0, 0, 0, 0.0, '{}', \
         x'', x'', x'', x'', '{}', '{}'";

    /// A stored ledger has rollup rows under the six-part key, and SQLite cannot add a seventh to a primary
    /// key in place, so the migration rebuilds the table. What it copies has to survive that.
    #[test]
    fn a_version_two_ledger_keeps_its_rollup_rows_under_the_reserved_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(crate::schema::MIGRATIONS[0].1).unwrap();
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '2')",
                [],
            )
            .unwrap();
            tx.execute(
                &format!(
                    "INSERT INTO rollups_hourly ({}) VALUES ({V2_ROW})",
                    v2_columns()
                ),
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let db = crate::LedgerDb::open(&path).unwrap();
        assert_eq!(db.read_blocking(read_version).unwrap(), 14);
        let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
        assert_eq!(rows.len(), 1, "the row survived the rebuild");
        assert_eq!(
            (
                rows[0].user_agent.as_str(),
                rows[0].hour,
                rows[0].key_id.as_str(),
                rows[0].requests,
                rows[0].output_tokens
            ),
            (UNATTRIBUTED, 3_600, "k", 5, 6),
            "a row folded before the column existed takes the reserved id and keeps its counts"
        );
    }

    /// A stored ledger at version 3 has no span table. The migration adds it, and the fold that needs it
    /// runs on the file that was already there.
    #[test]
    fn a_version_three_ledger_gains_the_cooldown_span_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA).unwrap();
            let tx = conn.transaction().unwrap();
            for (_, sql) in crate::schema::MIGRATIONS.iter().filter(|(v, _)| *v <= 3) {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute_batch("DROP TABLE cooldown_spans;").unwrap();
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '3')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let db = crate::LedgerDb::open(&path).unwrap();
        assert_eq!(db.read_blocking(read_version).unwrap(), 14);
        db.write(|c| {
            let tx = c.transaction()?;
            fold_event(&tx, &cooldown_set_event(0, 60_000))?;
            fold_event(&tx, &cooldown_cleared_event(20_000))?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            db.read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
                .unwrap()[0]
                .cooldown_seconds,
            20,
            "the interval folds on a ledger that predates the table it is kept in"
        );
    }

    fn request(outcome: &str, status: Option<u16>, key: Option<&str>) -> RequestRecord {
        RequestRecord {
            id: "r".into(),
            received_at: 3_700_000,
            finished_at: Some(3_700_900),
            key_id: key.map(String::from),
            pool_id: key.map(|_| "p".into()),
            model: "m".into(),
            canonical_model: "m".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status,
            outcome: outcome.into(),
            attempts: 2,
            ttfb_ms: Some(100),
            ttfc_ms: Some(300),
            duration_ms: Some(900),
            error_code: (outcome != "succeeded").then(|| "upstream_unreachable".into()),
            ..Default::default()
        }
    }
    fn attempt(
        n: u32,
        account: &str,
        read: Option<u64>,
        write: Option<u64>,
        out: Option<u64>,
        usd: Option<f64>,
    ) -> AttemptRecord {
        AttemptRecord {
            request_id: "r".into(),
            attempt: n,
            account_id: account.into(),
            started_at: 1,
            outcome: "succeeded".into(),
            cache_read_tokens: read,
            cache_write_5m_tokens: write,
            output_tokens: out,
            input_tokens: Some(10),
            estimated_usd: usd,
            cache_miss_reason: Some("none".into()),
            ratelimit_5h_utilization: Some(0.5),
            ..Default::default()
        }
    }

    #[test]
    fn the_key_is_the_serving_account_and_the_reserved_id_stands_in_for_what_is_missing() {
        let d = delta_of(
            &request("succeeded", Some(200), Some("k")),
            &[
                attempt(1, "a", None, None, None, Some(0.1)),
                AttemptRecord {
                    input_usd: Some(0.1),
                    cache_read_usd: Some(0.05),
                    cache_write_usd: Some(0.0),
                    output_usd: Some(0.15),
                    ..attempt(2, "b", Some(5), None, Some(20), Some(0.2))
                },
            ],
        );
        assert_eq!(
            (
                d.hour,
                d.key_id.as_str(),
                d.pool_id.as_str(),
                d.account_id.as_str(),
                d.canonical_model.as_str(),
                d.kind.as_str()
            ),
            (3_600, "k", "p", "b", "m", "inference")
        );
        assert_eq!(
            (
                d.requests,
                d.succeeded,
                d.attempts,
                d.failovers,
                d.cache_hits,
                d.cache_misses,
                d.uncached_requests
            ),
            (1, 1, 2, 1, 1, 0, 0)
        );
        assert_eq!(
            (d.input_tokens, d.cache_read_tokens, d.output_tokens),
            (10, 5, 20)
        );
        assert!((d.estimated_usd - 0.3).abs() < 1e-9);
        assert!((d.input_usd - 0.1).abs() < 1e-9 && (d.output_usd - 0.15).abs() < 1e-9);
        assert_eq!(
            d.ttfb_hist.count()
                + d.ttfc_hist.count()
                + d.duration_hist.count()
                + d.output_tps_hist.count(),
            4
        );
        assert_eq!(
            d.output_tps_hist.buckets()[bucket_index(20.0 / 0.6)],
            1,
            "20 tokens over the 600 ms after the first delta"
        );
        assert_eq!(d.cache_miss_reasons.get("none"), Some(&1));
        assert_eq!(
            d.cache_miss_reasons.get("expired_between_turns"),
            None,
            "a none with a cache read is a hit"
        );
        assert!(d.errors.is_empty());
        let d = delta_of(&request("rejected", Some(401), None), &[]);
        assert_eq!(
            (
                d.key_id.as_str(),
                d.pool_id.as_str(),
                d.account_id.as_str(),
                d.rejected,
                d.attempts,
                d.uncached_requests
            ),
            (UNATTRIBUTED, UNATTRIBUTED, UNATTRIBUTED, 1, 0, 1)
        );
        assert_eq!(d.errors.get("upstream_unreachable"), Some(&1));
        let d = delta_of(
            &request("failed", Some(429), Some("k")),
            &[attempt(1, "a", None, Some(3), None, None)],
        );
        assert_eq!((d.rate_limited, d.failed, d.cache_misses), (1, 0, 1));
        assert_eq!(
            (
                d.cache_miss_reasons.get("none"),
                d.cache_miss_reasons.get("expired_between_turns")
            ),
            (Some(&1), Some(&1)),
            "a none with no cache read expired between turns"
        );
        let d = delta_of(
            &request("canceled", Some(503), Some("k")),
            &[attempt(1, "a", None, None, None, None)],
        );
        assert_eq!((d.rate_limited, d.failed), (0, 1));
    }

    fn cooldown_set_event(at: i64, until: i64) -> EventRecord {
        EventRecord {
            at,
            kind: "cooldown_set".into(),
            account_id: Some("a".into()),
            detail: Some(
                serde_json::json!({ "scope": "account", "until": until, "reason": "rate_limit" })
                    .to_string(),
            ),
            ..Default::default()
        }
    }

    fn cooldown_cleared_event(at: i64) -> EventRecord {
        EventRecord {
            at,
            kind: "cooldown_cleared".into(),
            account_id: Some("a".into()),
            detail: Some(
                serde_json::json!({ "cause": "recovered", "scope": "account" }).to_string(),
            ),
            ..Default::default()
        }
    }

    fn folded(events: &[EventRecord]) -> u64 {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        db.write(|c| {
            let tx = c.transaction()?;
            for event in events {
                fold_event(&tx, event)?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        db.read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
            .unwrap()
            .iter()
            .map(|r| r.cooldown_seconds)
            .sum()
    }

    #[test]
    fn an_extended_cooldown_folds_as_one_interval_rather_than_two_announcements() {
        assert_eq!(
            folded(&[
                cooldown_set_event(0, 60_000),
                cooldown_set_event(10_000, 70_000)
            ]),
            70,
            "the extension adds only the ten seconds past the end already accounted for"
        );
    }

    #[test]
    fn a_cleared_cooldown_keeps_only_the_time_it_actually_held() {
        assert_eq!(
            folded(&[
                cooldown_set_event(0, 60_000),
                cooldown_cleared_event(20_000)
            ]),
            20,
            "the forty seconds that never elapsed leave the hour that counted them"
        );
    }

    const HOUR: i64 = 3_600_000;

    fn folded_by_hour(events: &[EventRecord]) -> BTreeMap<i64, u64> {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        db.write(|c| {
            let tx = c.transaction()?;
            for event in events {
                fold_event(&tx, event)?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        db.read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
            .unwrap()
            .into_iter()
            .map(|r| (r.hour, r.cooldown_seconds))
            .collect()
    }

    #[test]
    fn an_extension_across_an_hour_boundary_cleared_early_counts_only_the_elapsed_seconds_per_hour()
    {
        // Set at 0 until 2h, extended at 1h until 3h, cleared at 1h30: 5,400 s elapsed, 3,600 in hour 0 and
        // 1,800 in hour 1, nothing in hour 2.
        let by_hour = folded_by_hour(&[
            cooldown_set_event(0, 2 * HOUR),
            cooldown_set_event(HOUR, 3 * HOUR),
            cooldown_cleared_event(HOUR + HOUR / 2),
        ]);
        assert_eq!(by_hour.get(&0).copied().unwrap_or(0), 3_600);
        assert_eq!(by_hour.get(&3_600).copied().unwrap_or(0), 1_800);
        assert_eq!(by_hour.get(&7_200).copied().unwrap_or(0), 0);
    }

    /// A stored ledger at version 13 has no segments table. The migration adds it, and the boundary case that
    /// needs it runs on the file that was already there.
    #[test]
    fn a_ledger_from_the_previous_schema_gains_the_segments_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA).unwrap();
            let tx = conn.transaction().unwrap();
            for (_, sql) in crate::schema::MIGRATIONS
                .iter()
                .filter(|(v, _)| *v < crate::schema::MIGRATIONS.last().unwrap().0)
            {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                params![(crate::schema::MIGRATIONS.last().unwrap().0 - 1).to_string()],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let db = crate::LedgerDb::open(&path).unwrap();
        assert_eq!(
            db.read_blocking(read_version).unwrap(),
            crate::schema::MIGRATIONS.last().unwrap().0
        );
        db.write(|c| {
            let tx = c.transaction()?;
            fold_event(&tx, &cooldown_set_event(0, 2 * HOUR))?;
            fold_event(&tx, &cooldown_set_event(HOUR, 3 * HOUR))?;
            fold_event(&tx, &cooldown_cleared_event(HOUR + HOUR / 2))?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db
            .read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
            .unwrap();
        let by_hour: BTreeMap<i64, u64> = rows
            .into_iter()
            .map(|r| (r.hour, r.cooldown_seconds))
            .collect();
        assert_eq!(
            (
                by_hour.get(&0).copied().unwrap_or(0),
                by_hour.get(&3_600).copied().unwrap_or(0),
                by_hour.get(&7_200).copied().unwrap_or(0)
            ),
            (3_600, 1_800, 0),
            "the interval folds per hour on a ledger that predates the table it is kept in"
        );
    }

    /// Folds `events` on a thread of its own and hands back each hour's cooldown seconds and the span left
    /// accounted for account "a": a fold that never ends fails the caller at the deadline instead of hanging it.
    fn folded_in_bounded_time(events: Vec<EventRecord>) -> (BTreeMap<i64, u64>, Option<i64>) {
        let (done, folded) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let db = crate::LedgerDb::open_in_memory().unwrap();
            let span = db
                .write(|c| {
                    let tx = c.transaction()?;
                    for event in &events {
                        fold_event(&tx, event)?;
                    }
                    let span = read_span(&tx, "a", "account")?;
                    tx.commit()?;
                    Ok(span)
                })
                .unwrap();
            let by_hour = db
                .read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
                .unwrap()
                .into_iter()
                .map(|r| (r.hour, r.cooldown_seconds))
                .collect();
            let _ = done.send((by_hour, span));
        });
        folded
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the fold ends in bounded time")
    }

    const WEEK: i64 = 7 * 24 * HOUR;

    /// `i64::MAX` is the deadline the classifier saturates an absurd `Retry-After` to.
    #[test]
    fn a_saturated_cooldown_deadline_books_seven_days_and_accounts_the_span_to_their_end() {
        let (by_hour, span) = folded_in_bounded_time(vec![cooldown_set_event(0, i64::MAX)]);
        assert_eq!(
            (by_hour.len(), by_hour.values().all(|s| *s == 3_600)),
            (168, true),
            "one full row per hour of the seven days booked"
        );
        assert_eq!(
            span,
            Some(WEEK),
            "the span ends where the booking did, not at the announced deadline"
        );
    }

    #[test]
    fn a_second_announcement_past_the_horizon_books_the_next_stretch() {
        let (by_hour, span) = folded_in_bounded_time(vec![
            cooldown_set_event(0, i64::MAX),
            cooldown_set_event(24 * HOUR, i64::MAX),
        ]);
        assert_eq!(
            (by_hour.len(), by_hour.values().sum::<u64>(), span),
            (192, 8 * 24 * 3_600, Some(WEEK + 24 * HOUR)),
            "the announcement a day later books the day past the first one's horizon"
        );
    }

    #[test]
    fn a_clear_after_a_saturated_deadline_takes_back_every_booked_hour_it_never_held() {
        let (by_hour, span) = folded_in_bounded_time(vec![
            cooldown_set_event(0, i64::MAX),
            cooldown_cleared_event(HOUR),
        ]);
        assert_eq!(
            (by_hour.values().sum::<u64>(), span),
            (3_600, None),
            "only the hour the cooldown held is left"
        );
    }

    #[test]
    fn a_failover_is_a_move_between_accounts_not_every_extra_attempt() {
        let two = |first: &str, second: &str| {
            delta_of(
                &request("succeeded", Some(200), Some("k")),
                &[
                    attempt(1, first, None, None, None, None),
                    attempt(2, second, None, None, None, None),
                ],
            )
        };
        let replayed = two("a", "a");
        assert_eq!(
            (replayed.attempts, replayed.failovers),
            (2, 0),
            "a 401 replay or an unsent retry runs again on the account it was already on"
        );
        assert_eq!(two("a", "b").failovers, 1);
    }

    #[test]
    fn folding_two_requests_adds_a_replacement_subtracts_its_old_self_and_the_row_round_trips() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let (req, attempts) = (
            request("succeeded", Some(200), Some("k")),
            vec![AttemptRecord {
                input_usd: Some(0.05),
                cache_read_usd: Some(0.02),
                cache_write_usd: Some(0.0),
                output_usd: Some(0.13),
                thinking_tokens: Some(25),
                web_search_requests: Some(2),
                web_fetch_requests: Some(1),
                ..attempt(1, "a", Some(5), None, Some(20), Some(0.2))
            }],
        );
        let (req2, attempts2) = (
            RequestRecord {
                id: "r2".into(),
                ..request("succeeded", Some(200), Some("k"))
            },
            vec![AttemptRecord {
                thinking_tokens: Some(25),
                web_search_requests: Some(2),
                web_fetch_requests: Some(1),
                ..attempt(1, "a", Some(5), None, Some(20), Some(0.2))
            }],
        );
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &req, &attempts, None)?;
            fold_request(&tx, &req2, &attempts2, None)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
        assert_eq!(rows.len(), 1);
        let mut expected = delta_of(&req, &attempts);
        expected.add(&delta_of(&req2, &attempts2));
        assert_eq!(rows[0], expected, "two requests, two contributions");
        let accounts = db
            .read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
            .unwrap();
        assert_eq!(
            (
                accounts[0].account_id.as_str(),
                accounts[0].utilization_5h_max
            ),
            ("a", Some(0.5))
        );
        // The first request is written again with more output and a different split: its earlier split goes,
        // the new one lands whole, never mixed with what it replaced.
        let more = vec![AttemptRecord {
            input_usd: Some(0.3),
            cache_read_usd: Some(0.1),
            cache_write_usd: Some(0.05),
            output_usd: Some(0.45),
            thinking_tokens: Some(30),
            web_search_requests: Some(1),
            web_fetch_requests: Some(0),
            ..attempt(1, "a", Some(5), None, Some(50), Some(0.9))
        }];
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &req, &more, Some((&req, &attempts)))?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
        let mut expected = delta_of(&req2, &attempts2);
        expected.add(&delta_of(&req, &more));
        assert_eq!(
            (rows[0].requests, rows[0].output_tokens),
            (2, 70),
            "still two requests; the output is 20 + 50"
        );
        assert_eq!(
            (
                rows[0].thinking_tokens,
                rows[0].web_search_requests,
                rows[0].web_fetch_requests
            ),
            (55, 3, 1),
            "the replacement's counts stand in for the ones it took back"
        );
        assert!((rows[0].estimated_usd - expected.estimated_usd).abs() < 1e-9);
        assert_eq!(
            rows[0].output_tps_hist, expected.output_tps_hist,
            "the old sample left the histogram"
        );
        assert!(
            (rows[0].input_usd - 0.3).abs() < 1e-9
                && (rows[0].cache_read_usd - 0.1).abs() < 1e-9
                && (rows[0].cache_write_usd - 0.05).abs() < 1e-9
                && (rows[0].output_usd - 0.45).abs() < 1e-9,
            "the replaced request's split lands whole, unblended with what it replaced"
        );
        assert!(
            rows[0].input_usd >= 0.0
                && rows[0].cache_read_usd >= 0.0
                && rows[0].cache_write_usd >= 0.0
                && rows[0].output_usd >= 0.0,
            "a replace never drives a stored split column negative"
        );
    }

    #[test]
    fn an_hour_of_unpriced_requests_rolls_up_an_unsigned_zero() {
        let delta = delta_of(&RequestRecord::default(), &[]);
        assert_eq!(
            serde_json::to_string(&delta.estimated_usd).unwrap(),
            "0.0",
            "a signed zero on the wire is read back as -$0.00"
        );
    }

    /// Every numeric column carries its own value, so a read off the wrong index cannot pass.
    fn distinct_row() -> RollupRow {
        let mut r = RollupRow::key(
            3_600,
            "k",
            7,
            "p",
            "a",
            "m",
            "inference",
            "claude-cli/2.1.269 (external, cli)",
            "high",
        );
        r.requests = 10;
        r.succeeded = 11;
        r.rate_limited = 12;
        r.failed = 13;
        r.exhausted = 14;
        r.rejected = 15;
        r.attempts = 16;
        r.failovers = 17;
        r.cache_hits = 18;
        r.cache_misses = 19;
        r.uncached_requests = 20;
        r.input_tokens = 21;
        r.cache_read_tokens = 22;
        r.cache_write_5m_tokens = 23;
        r.cache_write_1h_tokens = 24;
        r.output_tokens = 40;
        r.thinking_tokens = 25;
        r.web_search_requests = 2;
        r.web_fetch_requests = 1;
        r.estimated_usd = 28.5;
        r.input_usd = 29.5;
        r.cache_read_usd = 30.5;
        r.cache_write_usd = 31.5;
        r.output_usd = 32.5;
        r.kept_warm_requests = 33;
        r.kept_warm_tokens = 34;
        r.saved_usd_gross = 35.5;
        r.probe_cost_usd = 36.5;
        r.probe_migrated = 37;
        r.wasted_probes = 38;
        r.wasted_probe_usd = 39.5;
        r.wasted_probe_reasons = BTreeMap::from([("idle_limit".to_string(), 40)]);
        r.probes_skipped = 41;
        r.ttfb_hist.record(42.0);
        r.ttfc_hist.record(43.0);
        r.ttfc_hist.record(43.0);
        r.duration_hist.record(44.0);
        r.duration_hist.record(44.0);
        r.duration_hist.record(44.0);
        r.output_tps_hist.record(45.0);
        r.errors = BTreeMap::from([("overloaded".to_string(), 46)]);
        r.cache_miss_reasons = BTreeMap::from([("none".to_string(), 47)]);
        r
    }

    #[test]
    fn a_rollup_row_with_a_distinct_value_in_every_column_reads_back_as_written() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let row = distinct_row();
        db.write(|c| {
            let tx = c.transaction()?;
            write_rollup(&tx, &row)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let read = db
            .read_blocking(|c| read_rollup(c, &row))
            .unwrap()
            .expect("the row is found under its key");
        assert_eq!(
            read, row,
            "every column, the histograms and the JSON counts included, is read from its own index"
        );
        assert_eq!(
            (
                read.output_tokens,
                read.thinking_tokens,
                read.web_search_requests,
                read.web_fetch_requests
            ),
            (40, 25, 2, 1),
            "the three new sums sit after output_tokens and read as themselves"
        );
        assert_eq!(
            (
                read.effort.as_str(),
                read.estimated_usd,
                read.cache_miss_reasons["none"]
            ),
            ("high", 28.5, 47)
        );
    }

    #[test]
    fn two_folded_requests_sum_the_thinking_share_and_the_server_tool_calls() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let counted = |n: u32| AttemptRecord {
            thinking_tokens: Some(25),
            web_search_requests: Some(2),
            web_fetch_requests: Some(1),
            ..attempt(n, "a", Some(5), None, Some(40), Some(0.2))
        };
        let first = (request("succeeded", Some(200), Some("k")), vec![counted(1)]);
        let second = (
            RequestRecord {
                id: "r2".into(),
                ..request("succeeded", Some(200), Some("k"))
            },
            vec![counted(1)],
        );
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &first.0, &first.1, None)?;
            fold_request(&tx, &second.0, &second.1, None)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (
                rows[0].output_tokens,
                rows[0].thinking_tokens,
                rows[0].web_search_requests,
                rows[0].web_fetch_requests
            ),
            (80, 50, 4, 2),
            "each request adds its own counts to the hour's row"
        );
    }

    /// A populated version-7 ledger keeps every value through the version-9 rebuild and reads the new
    /// columns at zero and `none`; the migration's INSERT names every column on both sides, so a column
    /// added in the middle cannot shift the ones after it.
    #[test]
    fn a_version_seven_ledger_keeps_its_rollup_values_and_reads_the_new_columns_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let mut old = distinct_row();
        old.effort = String::new();
        old.thinking_tokens = 0;
        old.web_search_requests = 0;
        old.web_fetch_requests = 0;
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(crate::schema::SCHEMA).unwrap();
            let tx = conn.transaction().unwrap();
            for (_, sql) in crate::schema::MIGRATIONS.iter().filter(|(v, _)| *v <= 7) {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '7')",
                [],
            )
            .unwrap();
            // The version-7 column list: every column a later migration added, stripped back out of the live
            // constant, so a future column addition needs no matching edit here.
            let v7 = COLUMNS.replace(", effort", "").replace(
                ", thinking_tokens, web_search_requests, web_fetch_requests",
                "",
            );
            let counts = |m: &BTreeMap<String, u64>| serde_json::to_string(m).unwrap();
            tx.execute(
                &format!(
                    "INSERT INTO rollups_hourly ({v7}) VALUES ({})",
                    (1..=44)
                        .map(|i| format!("?{i}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                params![
                    old.hour,
                    old.key_id,
                    old.key_generation,
                    old.pool_id,
                    old.account_id,
                    old.canonical_model,
                    old.kind,
                    old.user_agent,
                    old.requests as i64,
                    old.succeeded as i64,
                    old.rate_limited as i64,
                    old.failed as i64,
                    old.exhausted as i64,
                    old.rejected as i64,
                    old.attempts as i64,
                    old.failovers as i64,
                    old.cache_hits as i64,
                    old.cache_misses as i64,
                    old.uncached_requests as i64,
                    old.input_tokens as i64,
                    old.cache_read_tokens as i64,
                    old.cache_write_5m_tokens as i64,
                    old.cache_write_1h_tokens as i64,
                    old.output_tokens as i64,
                    old.estimated_usd,
                    old.input_usd,
                    old.cache_read_usd,
                    old.cache_write_usd,
                    old.output_usd,
                    old.kept_warm_requests as i64,
                    old.kept_warm_tokens as i64,
                    old.saved_usd_gross,
                    old.probe_cost_usd,
                    old.probe_migrated as i64,
                    old.wasted_probes as i64,
                    old.wasted_probe_usd,
                    counts(&old.wasted_probe_reasons),
                    old.probes_skipped as i64,
                    old.ttfb_hist.to_le_bytes(),
                    old.ttfc_hist.to_le_bytes(),
                    old.duration_hist.to_le_bytes(),
                    old.output_tps_hist.to_le_bytes(),
                    counts(&old.errors),
                    counts(&old.cache_miss_reasons),
                ],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let db = crate::LedgerDb::open(&path).unwrap();
        assert_eq!(db.read_blocking(read_version).unwrap(), 14);
        let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
        assert_eq!(rows.len(), 1);
        let mut expected = old;
        expected.effort = NO_EFFORT.into();
        assert_eq!(
            rows[0], expected,
            "every old value is where it was; effort is none and the three sums are zero"
        );
    }
}
