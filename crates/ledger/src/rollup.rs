//! The hourly fold: each completed request adds its deltas to the rows for its hour and key, one per band, speed and
//! funding its answered attempts fell in, in the same transaction as its raw rows, so a rollup never disagrees with
//! the requests it summarises.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql, Transaction};

use phoebis_bus::Kind;
use phoebis_relay::{identifier_byte, EFFORT_LEVELS};
use phoebis_routing::{window_length_ms, PinUsage, DAY_MS, HOUR_MS, WEEK_MS};

use crate::db::LedgerDb;
#[cfg(test)]
use crate::histogram::bucket_index;
use crate::histogram::Histogram;
use crate::plan::{entrypoint_of_client, funding_of, Funding, PlanTables};
use crate::price::{is_free_classifier, RowKey};
use crate::pricing::{Band, PriceRevisions, Speed};
use crate::query::{
    hour_ceil, in_list, key_clause, pattern_any, pool_term, ProbeView, COOLDOWN_CLEARED,
    COOLDOWN_SET, LIMIT_RESET_CLAIMED, OVERAGE_ENDED, PIN_CREATED, PIN_MIGRATED, ROUTER_STARTED,
    SHEDDING_ROUND, SLOW_DOWN_ENDED, SLOW_DOWN_STARTED, WINDOW_CLEARED, WINDOW_DROPPED,
    WINDOW_RESET, WINDOW_SPENT,
};
use crate::reconcile::{newest_rows, ReconciliationRow};
use crate::record::{AttemptRecord, EventRecord, KeepaliveDelta, RequestRecord};
use crate::LedgerError;

/// The reserved id for a key, pool, or account a request never reached, so the failures of an exhausted hour
/// have a row and the raw-versus-rollup equality holds.
pub const UNATTRIBUTED: &str = phoebis_routing::UNATTRIBUTED_POOL;

/// The value a request with no effort files under, neither a per-turn control entry's level nor a top-level
/// `output_config.effort`.
pub const NO_EFFORT: &str = "none";

/// The value a request with no `x-claude-code-request-class` header files under, and a keepalive delta's own.
pub const NO_CLASS: &str = "-";

/// The reserved key of `cache_miss_reasons` that counts a request whose serving attempt read back under half the size
/// the selection handed on, under no reason or one of the relay's own words: its cache expired between turns. A group
/// row reads it as a count of its own.
pub const EXPIRED_BETWEEN_TURNS: &str = "expired_between_turns";

/// The share of its prompt, in percent, that an attempt read from the cache to count as a hit under its reason.
pub const HIT_SHARE_PERCENT: u64 = 90;

/// The suffix of the reserved key `<reason>/hit` of `cache_miss_reasons`, which counts the requests under the reason
/// whose serving attempt read `HIT_SHARE_PERCENT` percent or more of its prompt back. A group row reads them apart.
pub(crate) const HIT_SUFFIX: &str = "/hit";

/// The reserved key of `input_transformations` that counts a moved request whose serving attempt's answer dropped a
/// block under an account reason. An answer's key holds a dot and a reserved key none, so the two never meet.
pub(crate) const TRANSFORMATIONS_MOVED: &str = "moved";
/// The reserved key of `input_transformations` that counts a request whose serving attempt's answer listed no block.
pub(crate) const TRANSFORMATIONS_NONE: &str = "none";

/// The value a request files under when its answer named no `usage.speed` and it did not ask for `fast`; `fast`
/// is the other value.
pub const STANDARD_SPEED: &str = "standard";

/// The funding of an attempt the plan paid for: neither served fast nor reported in overage.
pub const PLAN_FUNDING: &str = "plan";
/// The funding of an attempt whose head's unified headers said overage was in use.
pub const OVERAGE_FUNDING: &str = "overage";
/// The funding of an attempt served fast, whose answer draws usage credits in dollars.
pub const FAST_FUNDING: &str = "fast";
/// The funding of an attempt an api_key account served: billed in dollars whatever its speed or the head's overage
/// flag, as `funding_of` reads the kind first.
pub const BILLED_FUNDING: &str = "billed";
/// The funding of an attempt a usage-based seat served: billed in dollars at the API rates whatever its speed or the
/// head's overage flag, as `funding_of` reads the class after the kind.
pub const USAGE_BASED_FUNDING: &str = "usage_based";

/// The prompt-length band a row's attempts priced at: the key word of `Band::Standard`.
pub const STANDARD_BAND: &str = "standard";
/// The key word of `Band::LongContext`.
pub const LONG_BAND: &str = "long";

/// The folded vocabulary the hour row is keyed on; the request row keeps the raw values. A caller picks its user
/// agent, its effort and its class, and the client is derived from both headers, so an unbounded value would grow
/// `rollups_hourly` by a row per request for as long as the rollups are kept.
pub const OTHER_DIMENSION: &str = phoebis_bus::OTHER_CLIENT;
/// The longest identifier or user-agent token the hour row keeps.
pub const DIMENSION_MAX_BYTES: usize = 64;

/// The header's first product token (`name/version`, up to the first space), cut to `DIMENSION_MAX_BYTES`, else
/// `OTHER_DIMENSION`: the build of a client is in the token, and the comment after it is free text.
pub fn fold_user_agent(raw: &str) -> String {
    let token = raw.split(' ').next().unwrap_or_default();
    let part = |s: &str| !s.is_empty() && s.bytes().all(|b| b != b'/' && identifier_byte(b));
    match token.split_once('/') {
        Some((name, version)) if part(name) && part(version) => {
            token[..token.floor_char_boundary(DIMENSION_MAX_BYTES)].to_string()
        }
        _ => OTHER_DIMENSION.into(),
    }
}

/// The speed an answered attempt's row files and prices under: the speed its answer reported, `STANDARD_SPEED` for an
/// answer that reported none, whatever the request asked for, so the row prices at the cells the attempt was charged.
fn answered_speed(a: &AttemptRecord) -> &str {
    a.speed.as_deref().unwrap_or(STANDARD_SPEED)
}

/// The speed of a request no attempt answered: the serving attempt's reported speed, else `fast` for a request that
/// asked for it, else `STANDARD_SPEED`. The request row keeps the body's `speed` text as sent, and any other value
/// would key a row of its own.
fn unanswered_speed<'a>(a: Option<&'a AttemptRecord>, requested: Option<&str>) -> &'a str {
    a.and_then(|a| a.speed.as_deref())
        .unwrap_or_else(|| Speed::of(requested).as_str())
}

/// The speed a request files under, read off its serving attempt `last`: the answer's own where it answered with
/// usage, `unanswered_speed` where it did not. The request row's `speed` column and the serving delta read this one
/// rule.
pub(crate) fn fold_speed<'a>(last: Option<&'a AttemptRecord>, requested: Option<&str>) -> &'a str {
    match last.filter(|a| a.has_usage()) {
        Some(a) => answered_speed(a),
        None => unanswered_speed(last, requested),
    }
}

/// One of `EFFORT_LEVELS`, `NO_EFFORT` for a request that asked for none, else `OTHER_DIMENSION`.
pub fn fold_effort(raw: Option<&str>) -> &'static str {
    match raw {
        None | Some(NO_EFFORT) => NO_EFFORT,
        Some(effort) => EFFORT_LEVELS
            .iter()
            .find(|level| **level == effort)
            .copied()
            .unwrap_or(OTHER_DIMENSION),
    }
}

/// The value itself while it holds to the identifier charset and `DIMENSION_MAX_BYTES`, else `OTHER_DIMENSION`.
pub fn fold_identifier(raw: &str) -> String {
    if (1..=DIMENSION_MAX_BYTES).contains(&raw.len()) && raw.bytes().all(identifier_byte) {
        raw.into()
    } else {
        OTHER_DIMENSION.into()
    }
}

/// How far past its announcement a cooldown is booked. The deadline is the upstream's to set, and the
/// classifier saturates an absurd `Retry-After` to `i64::MAX`, so booking every hour up to it would hold the
/// writer's transaction for good.
const COOLDOWN_BOOKING_HORIZON_MS: i64 = WEEK_MS;

pub fn hour_of(at_ms: i64) -> i64 {
    at_ms.div_euclid(1_000).div_euclid(3_600) * 3_600
}

/// The end of the hour that starts at `hour`, where a read up to the current hour ends rather than at
/// `hour_ceil(now)`, which is `now` itself on the hour's first millisecond and would leave out the hour a request just
/// landed in.
pub fn hour_end(hour: i64) -> i64 {
    hour + 3_600
}

/// The hours `[from, to)` of the day up to the current hour, one span so the keepalive overview and the attention
/// item's expired count read the same hours, the item counting some of their rows.
pub fn trailing_day_hours(now: i64) -> (i64, i64) {
    let hour = hour_of(now);
    (hour - 24 * 3_600, hour_end(hour))
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

    pub fn add(&mut self, o: &CostSplit) {
        self.input_usd += o.input_usd;
        self.cache_read_usd += o.cache_read_usd;
        self.cache_write_usd += o.cache_write_usd;
        self.output_usd += o.output_usd;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
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
    /// The class of the request the row folded, `UNATTRIBUTED` for a row folded before the column existed. A
    /// dimension, so it sits in the key.
    pub client: String,
    /// The request's effort as the drawer shows it (the last per-turn control entry's level, else the top-level
    /// `output_config.effort`), `NO_EFFORT` when it sent none; a dimension, so it sits in the key.
    pub effort: String,
    /// The class the client named, `NO_CLASS` for a classless request and for a keepalive credit; a dimension,
    /// so it sits in the key.
    pub request_class: String,
    /// The speed the row's attempts' answers reported, `STANDARD_SPEED` for an answer that reported none, so the row
    /// prices at the cells its attempts were charged. A request with no answered attempt files under its serving
    /// attempt's, else `fast` when the request asked for it, else `STANDARD_SPEED`. A dimension, so it sits in the key.
    pub speed: String,
    /// Who paid for the row's attempts, `funding_of`'s answer as a key word: `billed` for an api_key account's answer
    /// whatever its speed or overage flag, else `usage_based` for a usage-based seat's answer whatever its speed or
    /// overage flag, else `fast` for a plan account's answer served fast, which draws usage credits in dollars
    /// whatever the overage flag, else `overage` when the head's unified headers said overage was in use, else `plan`.
    /// Written with the row, so a reader needs no registry to tell billed spend from plan spend, and a deleted or
    /// recreated account moves none of its rows. A dimension, so it sits in the key.
    pub funding: String,
    /// The cells the row's attempts priced at (`STANDARD_BAND` or `LONG_BAND`), read against the threshold in
    /// force at the request's instant. A dimension, so it sits in the key.
    pub band: String,
    /// The serving attempt's account tier, `UNATTRIBUTED` for none. A dimension, so it sits in the key.
    pub tier: String,
    /// `client` or `client_off` for the client's own classifier call, as the request row names it, `-` otherwise. A
    /// dimension, so it sits in the key.
    pub classifier: String,
    pub requests: u64,
    pub succeeded: u64,
    pub rate_limited: u64,
    pub failed: u64,
    /// The requests no probe sent that ended canceled under `client_gone`, a client that left before its answer
    /// ended, counted in `requests` and apart from `failed` and `errors`.
    pub client_canceled: u64,
    pub exhausted: u64,
    pub incomplete: u64,
    pub rejected: u64,
    pub attempts: u64,
    pub failovers: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub uncached_requests: u64,
    /// The uncached requests whose marker fact reads false, a token count aside, a part of `uncached_requests`: their
    /// body marked no cache and the TTL policy added none, so no cache could answer them.
    pub uncacheable: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub web_search_requests: u64,
    pub web_fetch_requests: u64,
    pub kept_warm_requests: u64,
    /// The kept-warm requests' cache reads, booked under each request's own TTL: an hour's in `kept_warm_1h_tokens`,
    /// any other in `kept_warm_5m_tokens`, which also holds the whole count of a row folded before the split.
    pub kept_warm_5m_tokens: u64,
    pub kept_warm_1h_tokens: u64,
    pub probe_migrated: u64,
    pub wasted_probes: u64,
    /// The wasted probes' own five token counts, priced when read at the cells the row's band names.
    pub wasted_input_tokens: u64,
    pub wasted_cache_read_tokens: u64,
    pub wasted_cache_write_5m_tokens: u64,
    pub wasted_cache_write_1h_tokens: u64,
    pub wasted_output_tokens: u64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
    pub probes_skipped: u64,
    /// Probes the router refused with `503 ledger_behind` while its ledger caught up, sent again later; outside
    /// `failed`, since the account answered nothing.
    pub probes_held: u64,
    /// The slow-down's waits of the row's requests, each request's `held` waits under `slow_down` summed.
    pub slow_down_held_ms: u64,
    /// The row's requests that waited through a slow-down hold.
    pub slow_down_held: u64,
    pub ttfb_hist: Histogram,
    pub ttfc_hist: Histogram,
    pub duration_hist: Histogram,
    pub output_tps_hist: Histogram,
    pub errors: BTreeMap<String, u64>,
    /// The refused requests of the row by error code, each counted here and under no other figure of the row.
    pub refusals: BTreeMap<String, u64>,
    pub cache_miss_reasons: BTreeMap<String, u64>,
    /// The requests counted once under each `<type>.<reason>` key of the blocks their serving attempt's answer dropped,
    /// whatever the key's block count, with two reserved keys beside them: `TRANSFORMATIONS_MOVED` and
    /// `TRANSFORMATIONS_NONE`.
    pub input_transformations: BTreeMap<String, u64>,
}

impl RollupRow {
    /// Sixteen key parts: `band`, `tier` and `classifier` after `funding`.
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
        client: &str,
        effort: &str,
        request_class: &str,
        speed: &str,
        funding: &str,
        band: &str,
        tier: &str,
        classifier: &str,
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
            client: client.into(),
            effort: effort.into(),
            request_class: request_class.into(),
            speed: speed.into(),
            funding: funding.into(),
            band: band.into(),
            tier: tier.into(),
            classifier: classifier.into(),
            ..RollupRow::default()
        }
    }

    pub fn add(&mut self, o: &RollupRow) {
        self.each_count(o, |a, b| *a += b);
        add_counts(&mut self.wasted_probe_reasons, &o.wasted_probe_reasons);
        self.ttfb_hist.merge(&o.ttfb_hist);
        self.ttfc_hist.merge(&o.ttfc_hist);
        self.duration_hist.merge(&o.duration_hist);
        self.output_tps_hist.merge(&o.output_tps_hist);
        add_counts(&mut self.errors, &o.errors);
        add_counts(&mut self.refusals, &o.refusals);
        add_counts(&mut self.cache_miss_reasons, &o.cache_miss_reasons);
        add_counts(&mut self.input_transformations, &o.input_transformations);
    }

    /// Every counter of the row paired with `o`'s, so `add` and `subtract` list them once.
    fn each_count(&mut self, o: &RollupRow, mut f: impl FnMut(&mut u64, u64)) {
        f(&mut self.requests, o.requests);
        f(&mut self.succeeded, o.succeeded);
        f(&mut self.rate_limited, o.rate_limited);
        f(&mut self.failed, o.failed);
        f(&mut self.client_canceled, o.client_canceled);
        f(&mut self.exhausted, o.exhausted);
        f(&mut self.incomplete, o.incomplete);
        f(&mut self.rejected, o.rejected);
        f(&mut self.attempts, o.attempts);
        f(&mut self.failovers, o.failovers);
        f(&mut self.cache_hits, o.cache_hits);
        f(&mut self.cache_misses, o.cache_misses);
        f(&mut self.uncached_requests, o.uncached_requests);
        f(&mut self.uncacheable, o.uncacheable);
        f(&mut self.input_tokens, o.input_tokens);
        f(&mut self.cache_read_tokens, o.cache_read_tokens);
        f(&mut self.cache_write_5m_tokens, o.cache_write_5m_tokens);
        f(&mut self.cache_write_1h_tokens, o.cache_write_1h_tokens);
        f(&mut self.output_tokens, o.output_tokens);
        f(&mut self.thinking_tokens, o.thinking_tokens);
        f(&mut self.web_search_requests, o.web_search_requests);
        f(&mut self.web_fetch_requests, o.web_fetch_requests);
        f(&mut self.kept_warm_requests, o.kept_warm_requests);
        f(&mut self.kept_warm_5m_tokens, o.kept_warm_5m_tokens);
        f(&mut self.kept_warm_1h_tokens, o.kept_warm_1h_tokens);
        f(&mut self.probe_migrated, o.probe_migrated);
        f(&mut self.wasted_probes, o.wasted_probes);
        f(&mut self.wasted_input_tokens, o.wasted_input_tokens);
        f(
            &mut self.wasted_cache_read_tokens,
            o.wasted_cache_read_tokens,
        );
        f(
            &mut self.wasted_cache_write_5m_tokens,
            o.wasted_cache_write_5m_tokens,
        );
        f(
            &mut self.wasted_cache_write_1h_tokens,
            o.wasted_cache_write_1h_tokens,
        );
        f(&mut self.wasted_output_tokens, o.wasted_output_tokens);
        f(&mut self.probes_skipped, o.probes_skipped);
        f(&mut self.probes_held, o.probes_held);
        f(&mut self.slow_down_held_ms, o.slow_down_held_ms);
        f(&mut self.slow_down_held, o.slow_down_held);
    }

    /// Who paid for the row, its funding word read as `Funding::of_rollup` reads it, so every reader takes one rule.
    pub(crate) fn funding_kind(&self) -> Funding {
        Funding::of_rollup(&self.funding)
    }

    pub fn probe_view(&self) -> ProbeView {
        ProbeView {
            exhausted: self.exhausted,
            failed: self.failed,
            funding: self.funding_kind(),
            incomplete: self.incomplete,
            probe_migrated: self.probe_migrated,
            probes_held: self.probes_held,
            probes_skipped: self.probes_skipped,
            rate_limited: self.rate_limited,
            rejected: self.rejected,
            succeeded: self.succeeded,
        }
    }

    /// The price key of the row: its own model, speed, band and funding, its tier (`None` under `UNATTRIBUTED`) and the
    /// entrypoint its `client` names, read at `now`. Its instant is the last millisecond of its hour, so the entries in
    /// force at the end of the hour price it whole: a row cannot hold a boundary inside its hour, and this way the hour
    /// an entry starts in prices at that entry, so a model the entry adds reads its dollars from its first request on.
    /// The rollup keeps no completion, so the peak band reads at the hour's start.
    pub(crate) fn price_key(&self, now: i64) -> RowKey<'_> {
        RowKey {
            model: &self.canonical_model,
            speed: Speed::of(Some(self.speed.as_str())),
            band: Band::of_rollup(&self.band),
            funding: self.funding_kind(),
            tier: (self.tier != UNATTRIBUTED).then_some(self.tier.as_str()),
            entrypoint: entrypoint_of_client(&self.client),
            classifier: is_free_classifier(Some(self.classifier.as_str())),
            at: hour_end(self.hour) * 1_000 - 1,
            band_at: Some(self.hour * 1_000),
            now,
        }
    }

    /// The row's counts as the five-count usage `price_row` takes.
    pub fn usage(&self) -> PinUsage {
        PinUsage {
            input: self.input_tokens,
            cache_read: self.cache_read_tokens,
            write_5m: self.cache_write_5m_tokens,
            write_1h: self.cache_write_1h_tokens,
            output: self.output_tokens,
        }
    }

    /// The wasted probes' counts the same way.
    pub fn wasted_usage(&self) -> PinUsage {
        PinUsage {
            input: self.wasted_input_tokens,
            cache_read: self.wasted_cache_read_tokens,
            write_5m: self.wasted_cache_write_5m_tokens,
            write_1h: self.wasted_cache_write_1h_tokens,
            output: self.wasted_output_tokens,
        }
    }

    /// The kept-warm counts as the usage their saving prices: each count as the write it avoided at its TTL, and the
    /// two together as the read the requests paid instead.
    pub fn kept_warm_usage(&self) -> PinUsage {
        PinUsage {
            cache_read: self.kept_warm_5m_tokens + self.kept_warm_1h_tokens,
            write_5m: self.kept_warm_5m_tokens,
            write_1h: self.kept_warm_1h_tokens,
            ..PinUsage::default()
        }
    }

    /// Whether every count the fold adds is zero: the request counters (`requests`, `probes_held`, `slow_down_held`),
    /// `attempts`, the five token counts, `thinking_tokens`, `web_search_requests`, `web_fetch_requests`, the keepalive
    /// counters (`kept_warm_requests`, `kept_warm_5m_tokens`, `kept_warm_1h_tokens`, `probe_migrated`,
    /// `probes_skipped`, `wasted_probes`), the five wasted counts and the refusals map, since a row of refusals alone
    /// is a row a reader must list. `fold_request` deletes a row it subtracted from only when this holds: a
    /// non-serving band row has `requests == 0` while another request's attempts still sit in it.
    pub fn holds_nothing(&self) -> bool {
        let usage = self.usage();
        let wasted = self.wasted_usage();
        [
            self.requests,
            self.probes_held,
            self.slow_down_held,
            self.attempts,
            usage.input,
            usage.cache_read,
            usage.write_5m,
            usage.write_1h,
            usage.output,
            self.thinking_tokens,
            self.web_search_requests,
            self.web_fetch_requests,
            self.kept_warm_requests,
            self.kept_warm_5m_tokens,
            self.kept_warm_1h_tokens,
            self.probe_migrated,
            self.probes_skipped,
            self.wasted_probes,
            wasted.input,
            wasted.cache_read,
            wasted.write_5m,
            wasted.write_1h,
            wasted.output,
        ]
        .iter()
        .all(|count| *count == 0)
            && self.refusals.is_empty()
    }
}

impl RollupRow {
    /// The inverse of `add` for a contribution that was added before; counters saturate at zero so a row that
    /// drifted cannot go negative, and a count that reaches zero leaves the map.
    pub fn subtract(&mut self, o: &RollupRow) {
        self.each_count(o, |a, b| *a = a.saturating_sub(b));
        subtract_counts(&mut self.wasted_probe_reasons, &o.wasted_probe_reasons);
        self.ttfb_hist.subtract(&o.ttfb_hist);
        self.ttfc_hist.subtract(&o.ttfc_hist);
        self.duration_hist.subtract(&o.duration_hist);
        self.output_tps_hist.subtract(&o.output_tps_hist);
        subtract_counts(&mut self.errors, &o.errors);
        subtract_counts(&mut self.refusals, &o.refusals);
        subtract_counts(&mut self.cache_miss_reasons, &o.cache_miss_reasons);
        subtract_counts(&mut self.input_transformations, &o.input_transformations);
    }
}

pub(crate) fn add_counts(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (k, v) in from {
        match into.get_mut(k) {
            Some(n) => *n += v,
            None => {
                into.insert(k.clone(), *v);
            }
        }
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

/// The shortest span from the first content to the end that records a tokens-per-second sample.
pub const THROUGHPUT_FLOOR_MS: u64 = 500;

/// The fold rules a request's deltas are computed under. The landing applies `FoldRules::ALL`, and the re-fold
/// pass computes a stored request's old deltas under the rules it was folded under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRules {
    /// A tokens-per-second sample needs a span of at least `THROUGHPUT_FLOOR_MS`.
    pub throughput_floor: bool,
    /// A sample past 100,000 files in its own bucket of the 84 rather than in bucket 63.
    pub wide_edges: bool,
    /// The three millisecond samples leave out the request's slow-down wait.
    pub slow_down_hold: bool,
    /// A request whose serving attempt read `HIT_SHARE_PERCENT` percent or more of its prompt from the cache counts
    /// under its miss reason's `<reason>/hit` key too.
    pub hit_split: bool,
}

impl FoldRules {
    pub const ALL: FoldRules = FoldRules {
        throughput_floor: true,
        wide_edges: true,
        slow_down_hold: true,
        hit_split: true,
    };
}

/// One delta per distinct (band, speed, funding) triple among the attempts with usage, the band from the attempt's
/// own prompt against the revision in force at the request's `received_at`, the speed the attempt's own and the
/// funding `funding_of` reads off the attempt's four facts, each delta summing its attempts' five counts,
/// `thinking_tokens`, `web_search_requests`, `web_fetch_requests` and its attempt count. The request counters,
/// `failovers`, the cache figures, the histograms and the errors sit on the serving attempt's delta alone (its band,
/// its speed, its funding). A request with no attempt with usage files one delta under `STANDARD_BAND` keyed by the
/// serving attempt's speed and funding. The tier is the serving attempt's on every delta, so a fast retry beside a
/// standard answer prices at the fast cells and a standard one at the standard cells. The samples follow `rules`.
pub fn deltas_of(
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    prices: &PriceRevisions,
    rules: FoldRules,
) -> Vec<RollupRow> {
    let table = prices.in_force(request.received_at);
    let model = request.canonical_model.as_str();
    let last = attempts.last();

    // An attempt's funding whole, read off its own four facts, so the key tells billed spend from plan spend without
    // the registry. A request no attempt served files under `plan`.
    let funding_word =
        |a: Option<&AttemptRecord>| a.map_or(PLAN_FUNDING, |a| a.funding().rollup_word());
    let band_of = |a: &AttemptRecord| table.band_of(model, a.usage().prompt_tokens()).as_str();
    let base = RollupRow::key(
        hour_of(request.received_at),
        request.key_id.as_deref().unwrap_or(UNATTRIBUTED),
        request.key_generation,
        request.pool_id.as_deref().unwrap_or(UNATTRIBUTED),
        last.map(|a| a.account_id.as_str()).unwrap_or(UNATTRIBUTED),
        &request.canonical_model,
        &request.kind,
        &request
            .user_agent
            .as_deref()
            .map_or_else(|| UNATTRIBUTED.into(), fold_user_agent),
        &request
            .client
            .as_deref()
            .map_or_else(|| UNATTRIBUTED.into(), fold_identifier),
        fold_effort(request.effort.as_deref()),
        &request
            .request_class
            .as_deref()
            .map_or_else(|| NO_CLASS.into(), fold_identifier),
        STANDARD_SPEED,
        PLAN_FUNDING,
        STANDARD_BAND,
        last.and_then(|a| a.tier.as_deref()).unwrap_or(UNATTRIBUTED),
        &request
            .classifier
            .as_deref()
            .map_or_else(|| NO_CLASS.into(), fold_identifier),
    );
    let mut deltas: Vec<RollupRow> = Vec::new();
    // Every answered attempt adds its own counts at the cells its own prompt, speed and funding selected, so the row
    // prices them as the attempt was charged, and an attempt with no usage has no count to add.
    for a in attempts.iter().filter(|a| a.has_usage()) {
        let d = delta_for(
            &mut deltas,
            &base,
            band_of(a),
            answered_speed(a),
            funding_word(Some(a)),
        );
        d.attempts += 1;
        d.input_tokens += a.input_tokens.unwrap_or(0);
        d.cache_read_tokens += a.cache_read_tokens.unwrap_or(0);
        d.cache_write_5m_tokens += a.cache_write_5m_tokens.unwrap_or(0);
        d.cache_write_1h_tokens += a.cache_write_1h_tokens.unwrap_or(0);
        d.output_tokens += a.output_tokens.unwrap_or(0);
        d.thinking_tokens += a.thinking_tokens.unwrap_or(0);
        d.web_search_requests += a.web_search_requests.unwrap_or(0);
        d.web_fetch_requests += a.web_fetch_requests.unwrap_or(0);
    }
    // A fast request refused before any attempt served (the fast lane spent, the credits unavailable, a local
    // refusal) still asked for fast: its counters file under that speed, while its funding reads the served answer,
    // which spent nothing.
    let d = delta_for(
        &mut deltas,
        &base,
        last.filter(|a| a.has_usage())
            .map_or(STANDARD_BAND, band_of),
        fold_speed(last, request.requested_speed.as_deref()),
        funding_word(last),
    );
    // The attempts that never answered count beside the request's other counters, so a request's attempts sum to
    // every attempt it made whichever rows its answers landed in.
    d.attempts += attempts.iter().filter(|a| !a.has_usage()).count() as u64;
    // Read off the row's kind, status and code rather than its outcome: the relay files a refusal before any attempt
    // as `rejected`, the router's own `503 ledger_behind` among them, though no account was asked.
    let held = request.kind == Kind::Probe.as_str()
        && request.status == Some(503)
        && request.error_code.as_deref() == Some("ledger_behind");
    // A refusal before any account was asked files under the map alone, its slow-down hold among the figures it
    // leaves, so every other figure of the row reads the requests an account was asked for. A probe is never flagged,
    // so the probe arms stand.
    let counted = !held && !request.refused;
    // A client that left is no failure of the router's or an account's, so it stays out of the error figures. A probe
    // is the keepalive's own request, whose cancel stays a failure.
    let client_left = request.outcome == "canceled"
        && request.kind != Kind::Probe.as_str()
        && request.error_code.as_deref() == Some("client_gone");
    if held {
        d.probes_held = 1;
    } else if request.refused {
        if let Some(code) = &request.error_code {
            *d.refusals.entry(code.clone()).or_default() += 1;
        }
    } else {
        d.requests = 1;
        match request.outcome.as_str() {
            "succeeded" => d.succeeded = 1,
            "failed" if request.status == Some(429) => d.rate_limited = 1,
            _ if client_left => d.client_canceled = 1,
            "failed" | "canceled" => d.failed = 1,
            "exhausted" => d.exhausted = 1,
            "incomplete" => d.incomplete = 1,
            "rejected" => d.rejected = 1,
            _ => {}
        }
    }
    // A 401 replay and an unsent retry both run again on the account the request was already on, so only a
    // move between accounts counts.
    d.failovers = attempts
        .windows(2)
        .filter(|pair| pair[0].account_id != pair[1].account_id)
        .count() as u64;
    let read = last.and_then(|a| a.cache_read_tokens).unwrap_or(0);
    let wrote = last
        .and_then(AttemptRecord::cache_write_tokens)
        .unwrap_or(0);
    if let Some((a, reason)) = last.and_then(|a| Some((a, a.cache_miss_reason.as_ref()?))) {
        *d.cache_miss_reasons.entry(reason.clone()).or_default() += 1;
        if rules.hit_split && read_most_of_its_prompt(a) {
            *d.cache_miss_reasons
                .entry(format!("{reason}{HIT_SUFFIX}"))
                .or_default() += 1;
        }
    }
    if counted && last.is_some_and(|a| expired_between_turns(request, a)) {
        *d.cache_miss_reasons
            .entry(EXPIRED_BETWEEN_TURNS.into())
            .or_default() += 1;
    }
    // A request counts once under each key whatever the key's `blocks`, since the readers count requests.
    if let Some((a, dropped)) = last.and_then(|a| Some((a, a.input_transformations.as_ref()?))) {
        for key in dropped.keys() {
            *d.input_transformations.entry(key.clone()).or_default() += 1;
        }
        if dropped.is_empty() {
            *d.input_transformations
                .entry(TRANSFORMATIONS_NONE.into())
                .or_default() += 1;
        }
        if moved(a) && dropped.keys().any(|key| under_account_reason(key)) {
            *d.input_transformations
                .entry(TRANSFORMATIONS_MOVED.into())
                .or_default() += 1;
        }
    }
    // A held probe and a refused request asked no account and reached no cache, so neither counts under a request
    // figure, a timing or an error code the rate should count.
    if counted {
        let slowed_ms = slow_down_held_ms(request.held.as_deref());
        if let Some(held_ms) = slowed_ms {
            d.slow_down_held_ms = held_ms;
            d.slow_down_held = 1;
        }
        if read > 0 {
            d.cache_hits = 1
        } else if wrote > 0 {
            d.cache_misses = 1
        } else {
            d.uncached_requests = 1;
            // A token count joins at the read, off its row's kind, so the hours folded before the column count it too.
            if request.cache_marker == Some(false) && request.kind != Kind::CountTokens.as_str() {
                d.uncacheable = 1;
            }
        }
        let record = |hist: &mut Histogram, value: f64| {
            if rules.wide_edges {
                hist.record(value)
            } else {
                hist.record_narrow(value)
            }
        };
        // The slow-down's waits leave the samples, since the held chart shows them, and a `cap`, `reset` or
        // `rewrite` wait stays in, which no chart shows.
        let hold = slowed_ms.filter(|_| rules.slow_down_hold).unwrap_or(0);
        let less_hold = |ms: i64| u64::try_from(ms).unwrap_or(0).saturating_sub(hold) as f64;
        if let Some(t) = request.ttfb_ms {
            record(&mut d.ttfb_hist, less_hold(t));
        }
        if let Some(t) = request.ttfc_ms {
            record(&mut d.ttfc_hist, less_hold(t));
        }
        if let Some(t) = request.duration_ms {
            record(&mut d.duration_hist, less_hold(t));
        }
        // Both stored samples carry the same wait, so the span between them stands without the hold.
        if let (Some(out), Some(duration), Some(ttfc)) = (
            last.and_then(|a| a.output_tokens),
            request.duration_ms,
            request.ttfc_ms,
        ) {
            let floor = if rules.throughput_floor {
                THROUGHPUT_FLOOR_MS
            } else {
                1
            };
            let span_ms = u64::try_from(duration - ttfc).ok();
            if let Some(span_ms) = span_ms.filter(|span_ms| *span_ms >= floor) {
                record(
                    &mut d.output_tps_hist,
                    out as f64 / (span_ms as f64 / 1_000.0),
                );
            }
        }
        if let Some(code) = request.error_code.as_ref().filter(|_| !client_left) {
            *d.errors.entry(code.clone()).or_default() += 1;
        }
    }
    deltas
}

/// Whether the attempt read `HIT_SHARE_PERCENT` percent or more of its prompt (the read, both writes and the input)
/// from the cache, compared in whole tokens. An attempt that read nothing is no hit, an empty prompt included.
fn read_most_of_its_prompt(a: &AttemptRecord) -> bool {
    let prompt = a.usage();
    prompt.cache_read > 0
        && 100 * prompt.cache_read
            >= HIT_SHARE_PERCENT
                * (prompt.cache_read + prompt.write_5m + prompt.write_1h + prompt.input)
}

/// The miss reasons that leave a short read to the cache's expiry: none, and the relay's own words. A reason the
/// upstream names says why its comparison missed.
const EXPIRY_REASONS: [&str; 4] = ["none", "not_requested", "not_returned", "pending"];

/// Whether the cache of `request`'s session expired between its previous request and this one: a request that is
/// neither a probe nor a token count, whose serving attempt carries a usage report and the size the selection handed
/// on, read back under half that size, and named no reason or one of `EXPIRY_REASONS`. The selection hands on no size
/// for a landing on another account and for a compacted, lane-changed or restarted turn, so none of them counts.
fn expired_between_turns(request: &RequestRecord, serving: &AttemptRecord) -> bool {
    let Some(previous) = serving.previous_input_tokens else {
        return false;
    };
    request.kind != Kind::Probe.as_str()
        && request.kind != Kind::CountTokens.as_str()
        && serving.has_usage()
        && 2 * serving.cache_read_tokens.unwrap_or(0) < previous
        && serving
            .cache_miss_reason
            .as_deref()
            .is_none_or(|reason| EXPIRY_REASONS.contains(&reason))
}

/// The delta of `deltas` at `band`, `speed` and `funding`, pushed as `base` at those cells where none is yet.
fn delta_for<'d>(
    deltas: &'d mut Vec<RollupRow>,
    base: &RollupRow,
    band: &str,
    speed: &str,
    funding: &str,
) -> &'d mut RollupRow {
    let index = match deltas
        .iter()
        .position(|d| d.band == band && d.speed == speed && d.funding == funding)
    {
        Some(index) => index,
        None => {
            deltas.push(RollupRow {
                band: band.into(),
                speed: speed.into(),
                funding: funding.into(),
                ..base.clone()
            });
            deltas.len() - 1
        }
    };
    &mut deltas[index]
}

/// The request's slow-down waits summed off its `held` JSON, `None` for a request with none. The JSON is the routing
/// crate's `CohortHold`, which derives no `Deserialize`, so the ledger reads the waits by name, and a row written
/// before the waits names none.
fn slow_down_held_ms(held: Option<&str>) -> Option<u64> {
    let held: serde_json::Value = serde_json::from_str(held?).ok()?;
    let slowed: Vec<u64> = held
        .get("waits")?
        .as_array()?
        .iter()
        .filter(|wait| {
            wait.get("reason").and_then(serde_json::Value::as_str)
                == Some(phoebis_routing::HoldReason::SlowDown.as_str())
        })
        .map(|wait| {
            wait.get("held_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        })
        .collect();
    (!slowed.is_empty()).then(|| slowed.iter().sum())
}

/// The 16 columns of a probe or kept-warm rollup row the keepalive overview folds, which `read_overview_rollups`
/// answers. The overview reads a day of every row on each poll and holds a reader connection through the decode, so it
/// reads these alone in place of all 61 a `RollupRow` decodes, the four histograms among them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverviewRollupRow {
    pub kind: String,
    pub funding: String,
    pub pool_id: String,
    pub account_id: String,
    pub succeeded: u64,
    pub failed: u64,
    pub rate_limited: u64,
    pub exhausted: u64,
    pub rejected: u64,
    pub incomplete: u64,
    pub probes_held: u64,
    pub probe_migrated: u64,
    pub probes_skipped: u64,
    pub kept_warm_requests: u64,
    pub wasted_probes: u64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
}

impl OverviewRollupRow {
    pub fn probe_view(&self) -> ProbeView {
        ProbeView {
            exhausted: self.exhausted,
            failed: self.failed,
            funding: Funding::of_rollup(&self.funding),
            incomplete: self.incomplete,
            probe_migrated: self.probe_migrated,
            probes_held: self.probes_held,
            probes_skipped: self.probes_skipped,
            rate_limited: self.rate_limited,
            rejected: self.rejected,
            succeeded: self.succeeded,
        }
    }
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
    /// Cache-write tokens of the first request after each move that landed on the account in the hour.
    pub rewrite_tokens: u64,
    /// Moves that landed on the account in the hour, keyed `{reason}/{fit}` by the `pin_migrated` reason and the
    /// destination's fit, `unknown` for a move whose event names no fit.
    pub migrations: BTreeMap<String, u64>,
    /// The longest time a wall on the account arrived before its window's reset in the hour under a `slow_down`
    /// round, 0 where none: the stretch paces the demand to reach the ceiling at the reset, so an early wall is its
    /// failure, and the figure outlives the trailing day the `wall_reached` condition covers.
    pub wall_early_ms: u64,
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
        self.rewrite_tokens += o.rewrite_tokens;
        add_counts(&mut self.migrations, &o.migrations);
        self.wall_early_ms = self.wall_early_ms.max(o.wall_early_ms);
    }

    /// Whether the row holds no figure: the seven counts zero, the three maxima null and the moves map empty. Ledger
    /// step 4 deletes the stored rows in this state by the same eleven columns, so a stored row and a new one agree.
    fn holds_nothing(&self) -> bool {
        [
            self.overage_seconds,
            self.cooldown_seconds,
            self.pins_created,
            self.pins_migrated_in,
            self.pins_migrated_out,
            self.rewrite_tokens,
            self.wall_early_ms,
        ]
        .iter()
        .all(|count| *count == 0)
            && self.utilization_5h_max.is_none()
            && self.utilization_7d_max.is_none()
            && self.utilization_7d_oi_max.is_none()
            && self.migrations.is_empty()
    }
}

pub fn fold_request(
    tx: &Transaction<'_>,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    previous: Option<(&RequestRecord, &[AttemptRecord])>,
    prices: &PriceRevisions,
) -> Result<(), LedgerError> {
    // A replaced request first leaves the rows it was folded into, under the keys it had then: the same id can
    // land under another account or hour once its attempts are known. A request folded under its raw dimensions by
    // an earlier build and replaced after the upgrade keeps its old contribution in the raw row, while the
    // subtraction lands on whatever row the folded key names: none while no request wrote it, so the request counts
    // in both rows, and a later request's row otherwise, where the counters saturate at zero while the totals still
    // balance. Accepted for a record replaced across one restart.
    if let Some((old_request, old_attempts)) = previous {
        for old in deltas_of(old_request, old_attempts, prices, FoldRules::ALL) {
            unfold(tx, &old)?;
        }
    }
    for delta in deltas_of(request, attempts, prices, FoldRules::ALL) {
        fold_delta(tx, &delta)?;
    }
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
    // `finish` writes usage onto the serving attempt alone, so that is the one attempt a trace naming a move
    // can ever have anything to book: an earlier pass of the same leg (a retry in place, a strip retry, a 401
    // replay) republishes the same trace with no usage yet, and an earlier leg's own attempt carries its own
    // trace regardless of this one's.
    if let Some((_, old_attempts)) = previous {
        if let Some((a, tokens)) = serving_rewrite(old_attempts) {
            unfold_rewrite(tx, hour_of(a.started_at), &a.account_id, tokens)?;
        }
    }
    if let Some((a, tokens)) = serving_rewrite(attempts) {
        fold_account(
            tx,
            &AccountRollupRow {
                hour: hour_of(a.started_at),
                account_id: a.account_id.clone(),
                rewrite_tokens: tokens,
                ..Default::default()
            },
        )?;
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

/// Adds `delta` to its row, or writes `delta` as the row where none stands.
pub(crate) fn fold_delta(tx: &Transaction<'_>, delta: &RollupRow) -> Result<(), LedgerError> {
    match read_rollup(tx, delta)? {
        Some(mut row) => {
            row.add(delta);
            write_rollup(tx, &row)
        }
        None => write_rollup(tx, delta),
    }
}

/// Takes a delta that was added before back out of its row. A row left holding nothing of any request or probe
/// goes, since a row of zeros would surface as a phantom group in every reader that lists keys rather than summing
/// values. A row that keeps another request's counts (a band row its own request does not serve, which carries no
/// request counter) or a keepalive delta's stays.
pub(crate) fn unfold(tx: &Transaction<'_>, delta: &RollupRow) -> Result<(), LedgerError> {
    if let Some(mut row) = read_rollup(tx, delta)? {
        row.subtract(delta);
        if row.holds_nothing() {
            delete_rollup(tx, &row)?;
        } else {
            write_rollup(tx, &row)?;
        }
    }
    Ok(())
}

fn reads_utilization(a: &AttemptRecord) -> bool {
    a.ratelimit_5h_utilization.is_some()
        || a.ratelimit_7d_utilization.is_some()
        || a.ratelimit_7d_oi_utilization.is_some()
}

/// Whether the attempt's stored trace names a move: `shed`, `spent`, `overage`, `return_to_plan` and `drain`
/// from the selector alike, `cooldown`, `fast_lane` and `mark` from a fail-over's recovered reason, and the cause a
/// live pin a placement replaced on another account left for, such as `paused` or `unserved`.
fn moved(a: &AttemptRecord) -> bool {
    a.selection
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .is_some_and(|v| !v["moved"].is_null())
}

/// Whether a dropped-blocks key names one of the two account reasons, read off the key's suffix: `identifier_byte`
/// admits a dot inside a word, so a split at a dot could cut the type or the reason.
fn under_account_reason(key: &str) -> bool {
    [
        ".end_user_binding_mismatch",
        ".organization_binding_mismatch",
    ]
    .iter()
    .any(|reason| key.ends_with(reason))
}

/// The serving attempt (the last one; `finish` writes usage there and nowhere else) and its cache-write
/// tokens, when its own trace names a move and it wrote anything; `None` otherwise, so a moved leg with
/// nothing yet booked books nothing.
fn serving_rewrite(attempts: &[AttemptRecord]) -> Option<(&AttemptRecord, u64)> {
    let a = attempts.last()?;
    if !moved(a) {
        return None;
    }
    let tokens = a.cache_write_tokens().unwrap_or(0);
    (tokens > 0).then_some((a, tokens))
}

/// One hour and account's utilization maxima, taken again from the raw attempts and written back over the
/// three columns alone: the counters beside them come from cooldown, overage and pin events, which no
/// replaced request has anything to say about.
fn reread_utilization(
    tx: &Transaction<'_>,
    hour: i64,
    account_id: &str,
) -> Result<(), LedgerError> {
    let (five, seven, oi) = tx
        .prepare_cached(
            "SELECT MAX(ratelimit_5h_utilization), MAX(ratelimit_7d_utilization), MAX(ratelimit_7d_oi_utilization) \
             FROM attempts WHERE account_id = ?1 AND started_at >= ?2 AND started_at < ?3",
        )?
        .query_row(params![account_id, hour * 1_000, hour_end(hour) * 1_000], |r| {
            Ok((
                r.get::<_, Option<f64>>(0)?,
                r.get::<_, Option<f64>>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        })?;
    tx.prepare_cached(
        "UPDATE account_rollups_hourly SET utilization_5h_max = ?1, utilization_7d_max = ?2, utilization_7d_oi_max = ?3 \
         WHERE hour = ?4 AND account_id = ?5",
    )?
    .execute(params![five, seven, oi, hour, account_id])?;
    Ok(())
}

/// The keepalive engine's own delta: one row of its key, holding a kept-warm request's credit, a probe's migration
/// mark, a skip, and the counts a stopped recipe wasted. The delta keys the client and the user agent of the probe's
/// own request row, or of the credited request on a credit, each folded as `deltas_of` folds that row's, so the
/// Client and User agent filters and groupings reach the keepalive's figures and a row prices under its client's
/// regime. The row keys the delta's own instant, band and account tier, so a settlement filed after its probe sits in
/// the probe's hour and prices at the cells and the revision the probe ran at.
pub fn fold_keepalive(tx: &Transaction<'_>, delta: &KeepaliveDelta) -> Result<(), LedgerError> {
    let key = RollupRow::key(
        hour_of(delta.at),
        &delta.key_id,
        delta.key_generation,
        &delta.pool_id,
        &delta.account_id,
        &delta.canonical_model,
        &delta.kind,
        &delta
            .user_agent
            .as_deref()
            .map_or_else(|| UNATTRIBUTED.into(), fold_user_agent),
        // Folded as `deltas_of` folds the request row's, so a delta lands on the label its request's row keys, and an
        // empty client, a default-built delta's, where an unlabeled request does.
        &if delta.client.is_empty() {
            UNATTRIBUTED.into()
        } else {
            fold_identifier(&delta.client)
        },
        NO_EFFORT,
        NO_CLASS,
        delta.speed.as_str(),
        // A credit files beside its probe's request row, under the tier the router prices by as that row does (none
        // for a free organization or a usage-based seat), and in the order `funding_of` reads an attempt: `billed` on
        // an api_key account, `usage_based` on a usage-based seat, `fast` for a fast delta (a fast answer's request
        // row `deltas_of` keys `fast` whatever the overage flag), else the overage fact the probe's answer reported,
        // else `plan`.
        funding_of(
            delta.account_kind,
            delta.usage_based,
            Some(delta.overage_in_use),
            delta.speed,
        )
        .rollup_word(),
        delta.band.as_str(),
        delta.tier.as_deref().unwrap_or(UNATTRIBUTED),
        NO_CLASS,
    );
    fold_delta(
        tx,
        &RollupRow {
            kept_warm_requests: delta.kept_warm_requests,
            kept_warm_5m_tokens: delta.kept_warm_5m_tokens,
            kept_warm_1h_tokens: delta.kept_warm_1h_tokens,
            probe_migrated: delta.probe_migrated,
            probes_skipped: delta.probes_skipped,
            wasted_probes: delta.wasted_probes,
            wasted_input_tokens: delta.wasted_input_tokens,
            wasted_cache_read_tokens: delta.wasted_cache_read_tokens,
            wasted_cache_write_5m_tokens: delta.wasted_cache_write_5m_tokens,
            wasted_cache_write_1h_tokens: delta.wasted_cache_write_1h_tokens,
            wasted_output_tokens: delta.wasted_output_tokens,
            wasted_probe_reasons: delta.wasted_probe_reasons.clone(),
            ..key
        },
    )?;
    if let Some(id) = &delta.kept_warm_request_id {
        // The engine credits at completion while the writer batches, so the request's own row may land after
        // this mark; the insert consults the marks table for exactly that case.
        tx.prepare_cached("UPDATE requests SET kept_warm = 1 WHERE id = ?1")?
            .execute(params![id])?;
        tx.prepare_cached(
            "INSERT OR IGNORE INTO kept_warm_marks (request_id, marked_at) VALUES (?1, ?2)",
        )?
        .execute(params![id, delta.at])?;
    }
    Ok(())
}

/// An event's stored detail decoded, `Null` for a detail that is absent or not JSON, so a reader of a member reads
/// its fallback.
pub fn detail_json(detail: Option<&str>) -> serde_json::Value {
    detail
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// A `pin_migrated` event's reason, `unknown` where the detail names none.
pub(crate) fn migration_reason(detail: &serde_json::Value) -> &str {
    detail["reason"].as_str().unwrap_or("unknown")
}

pub fn fold_event(tx: &Transaction<'_>, event: &EventRecord) -> Result<(), LedgerError> {
    // Decoded in the arms that read it, so a kind that reads no detail parses none.
    let detail = || detail_json(event.detail.as_deref());
    let hour = hour_of(event.at);
    let row = |account_id: &str| AccountRollupRow {
        hour,
        account_id: account_id.into(),
        ..Default::default()
    };
    match (event.kind.as_str(), event.account_id.as_deref()) {
        (PIN_CREATED, Some(account)) => fold_account(
            tx,
            &AccountRollupRow {
                pins_created: 1,
                ..row(account)
            },
        ),
        (PIN_MIGRATED, Some(to)) => {
            let detail = detail();
            let reason = migration_reason(&detail);
            let fit = detail["fit"].as_str().unwrap_or("unknown");
            fold_account(
                tx,
                &AccountRollupRow {
                    pins_migrated_in: 1,
                    migrations: BTreeMap::from([(format!("{reason}/{fit}"), 1)]),
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
        (WINDOW_SPENT, Some(account)) => {
            let detail = detail();
            let window = detail["window"].as_str().unwrap_or("unknown");
            let round = newest_round(tx, account, window, event.at)?;
            // A wall at or after its reset came early by nothing, which the hour's longest already holds.
            match early_ms(round.as_ref(), &detail, event.at).filter(|early| *early > 0) {
                Some(early) => fold_account(
                    tx,
                    &AccountRollupRow {
                        wall_early_ms: early as u64,
                        ..row(account)
                    },
                ),
                None => Ok(()),
            }
        }
        (COOLDOWN_SET, Some(account)) => {
            let detail = detail();
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
                let end = booked.min(hour_end(bucket) * 1_000);
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
        (COOLDOWN_CLEARED, Some(account)) => {
            let detail = detail();
            let scope = detail["scope"].as_str().unwrap_or("account");
            let Some(until) = take_span(tx, account, scope)? else {
                return Ok(());
            };
            for (bucket, counted) in take_segments(tx, account, scope)? {
                // The part of this hour's segment that lies after the clear never elapsed.
                let seconds = unelapsed_segment_seconds(event.at, bucket, until, counted);
                if seconds > 0 {
                    unfold_cooldown(tx, bucket, account, seconds)?;
                }
            }
            Ok(())
        }
        (OVERAGE_ENDED, Some(account)) => {
            let seconds = detail()["since"]
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

/// How far back the routing conditions read: a condition holds while one qualifying event lies this close to the fold,
/// and `slowed_long` counts as well a slow-down span open from before this cutoff, which no row inside it names, and
/// sums the held time of the hours that start inside it.
const CONDITION_SPAN_MS: i64 = DAY_MS;

/// How soon after a move a spent window on its destination counts against the move.
const SPENT_AFTER_MOVE_MS: i64 = HOUR_MS;

// The pool settings a condition names, each spelled once for the lists below and for the attention reader, which
// adds a pool's value of each setting a row names.
pub const SHED_CAP_SETTING: &str = "shed_cap";
pub const SLOW_DOWN_STRETCH_SETTING: &str = "slow_down_stretch_seconds";
pub const FIVE_HOUR_CEILING_SETTING: &str = "five_hour_ceiling";
pub const WEEKLY_CEILING_SETTING: &str = "weekly_ceiling";

/// The settings each condition names, the ones an operator reads the condition against. The attention reader fills
/// a cleared condition's `setting` from the same lists. A pool's round reads a sibling's runway over windows of both
/// kinds, so `shed_no_runway` names both ceilings. A wall and a spent destination name the ceiling of the kind of
/// each window they list, after `WALL_SETTINGS` on a wall.
pub const CEILING_SETTINGS: &[&str] = &[FIVE_HOUR_CEILING_SETTING, WEEKLY_CEILING_SETTING];
pub const WALL_SETTINGS: &[&str] = &[SHED_CAP_SETTING];
pub const SLOW_SETTINGS: &[&str] = &[SLOW_DOWN_STRETCH_SETTING];

/// `lead`, then `five_hour_ceiling` where `windows` lists the 5-hour window and `weekly_ceiling` where it lists any
/// other, each once. The kinds are told as `window_length_ms` tells them, the rule the router's ceiling reads, so a
/// model week names the weekly ceiling.
fn window_settings(lead: &[&'static str], windows: &BTreeSet<&str>) -> Vec<&'static str> {
    let mut names = lead.to_vec();
    if windows
        .iter()
        .any(|window| window_length_ms(window) != WEEK_MS)
    {
        names.push(FIVE_HOUR_CEILING_SETTING);
    }
    if windows
        .iter()
        .any(|window| window_length_ms(window) == WEEK_MS)
    {
        names.push(WEEKLY_CEILING_SETTING);
    }
    names
}

/// The kind of a condition the fold writes, stored as its `as_str` word.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum RoutingKind {
    /// An account that read a window spent within the hour after a move landed on it.
    DestinationSpent,
    /// An account where the newest reconciled window of any kind reads off its priced share.
    MeterDrift,
    /// A pool whose shedding round marked nothing for want of a destination with runway.
    ShedNoRunway,
    /// An account whose requests the slow-down held past twice its pool's stretch threshold in a day, at least an hour.
    SlowedLong,
    /// An account that read a window spent.
    WallReached,
}

impl RoutingKind {
    const EVERY: [RoutingKind; 5] = [
        RoutingKind::DestinationSpent,
        RoutingKind::MeterDrift,
        RoutingKind::ShedNoRunway,
        RoutingKind::SlowedLong,
        RoutingKind::WallReached,
    ];

    /// The kind a stored word names, `None` for a word no kind writes.
    pub fn parse(word: &str) -> Option<RoutingKind> {
        RoutingKind::EVERY
            .into_iter()
            .find(|kind| kind.as_str() == word)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            RoutingKind::DestinationSpent => "destination_spent",
            RoutingKind::MeterDrift => "meter_drift",
            RoutingKind::ShedNoRunway => "shed_no_runway",
            RoutingKind::SlowedLong => "slowed_long",
            RoutingKind::WallReached => "wall_reached",
        }
    }
}

/// How many stretch thresholds of held time in the trailing day raise `slowed_long`. The slow-down holds an account's
/// requests while its demand runs ahead of its window, and the threshold is how long the pool lets it hold before a
/// round moves sessions, so two thresholds' worth of held time in a day says the pool's load exceeds the account's
/// windows under any threshold: the pool's membership, its ceiling or its threshold rather than one long turn.
const SLOWED_LONG_STRETCHES: i64 = 2;

/// The bar's floor, so a pool near the threshold's own floor raises nothing on a few minutes of slow-down.
const SLOWED_LONG_FLOOR_MS: i64 = HOUR_MS;

/// The detail of the newest `shedding_round` on the account's `window` inside the window's length before the wall at
/// `at`, read on the wall's own window since a round projects the window that fired it. A round a weekly window's
/// length back can sit outside the trailing day.
fn newest_round(
    tx: &Transaction<'_>,
    account: &str,
    window: &str,
    at: i64,
) -> Result<Option<serde_json::Value>, LedgerError> {
    let length = window_length_ms(window);
    Ok(tx
        .prepare_cached(
            "SELECT detail FROM events WHERE kind = ?5 AND account_id = ?1 \
             AND at >= ?2 AND at <= ?3 AND json_extract(detail, '$.window') = ?4 \
             ORDER BY at DESC, rowid DESC LIMIT 1",
        )?
        .query_row(
            params![account, at - length, at, window, SHEDDING_ROUND],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .and_then(|d| serde_json::from_str(&d).ok()))
}

/// How long before the reset its `window_spent` detail names a wall at `at` arrived, 0 at or after it, after a round
/// the stretch fired, and `None` after any other round, with none, or where the detail names no reset.
fn early_ms(round: Option<&serde_json::Value>, spent: &serde_json::Value, at: i64) -> Option<i64> {
    round.filter(|round| round["trigger"] == "slow_down")?;
    spent["reset"].as_i64().map(|reset| (reset - at).max(0))
}

/// One condition of the fold before it is written.
struct Held {
    pool_id: Option<String>,
    since: i64,
    count: u64,
    detail: serde_json::Value,
}

/// The five routing conditions, written whole into `routing_conditions`. Four read the trailing 24 hours of events:
/// `shed_no_runway` per pool from its rounds that marked nothing for want of a destination with runway, each until a
/// later event on its account refunds its window: a `window_reset`, `window_cleared` or `window_dropped` naming that
/// window, or a `limit_reset_claimed` whose `cleared` lists it,
/// `destination_spent` per account from the moves onto it that a spent window of any source followed within the hour,
/// with no `window_cleared` on that account and window after it,
/// `wall_reached` per account from its spent windows of any source, a predicted wall among them, and `slowed_long` per
/// account with a slow-down span inside the day, once the time the slow-down held its requests passes the bar:
/// `slow_down_held_ms` summed over the account's `rollups_hourly` rows across its pools and keys whose hour starts at
/// or after the cutoff, beside the same sum of `slow_down_held`, against `SLOWED_LONG_STRETCHES` times the
/// `threshold_ms` of the newest counted span's start row,
/// at least `SLOWED_LONG_FLOOR_MS`. The spans name the pool, the threshold and the start, so held time with no span in
/// the day raises nothing. `since` is the oldest qualifying event's time, and for `slowed_long` the own start of the
/// oldest span counted, which a span open from before the cutoff dates before the day. A wall reads the newest round on
/// the account's newest wall's window inside that window's length before the wall: after a projection round its
/// `projection_error` is one less the round's projection, after a stretch round its `early_ms` is the reset the wall's
/// `window_spent` names less the wall's instant, 0 at or after the reset, and after a round the Fable reservation fired
/// or with no round both are null. The fifth, `meter_drift`, reads the reconciled windows that ended inside
/// `DRIFT_HOLD_MS` of `now`: per account, the newest row of each window with a rise of at least `MIN_RISE`, held while
/// any reads outside `RATIO_LOW` to `RATIO_HIGH` over a priced share of at least `MIN_RISE`, `since` the oldest off
/// row's `window_end` or `now` where that end lies after it, its detail naming the off windows and the cost model
/// `tables` holds, since the fix is that file rather than a pool setting. The ledger reads no registry, so a detail
/// names its settings and the reader adds a pool's values.
pub fn fold_routing_conditions(
    tx: &Transaction<'_>,
    tables: &PlanTables,
    now: i64,
) -> Result<(), LedgerError> {
    struct Row {
        at: i64,
        kind: String,
        account_id: Option<String>,
        pool_id: Option<String>,
        detail: serde_json::Value,
    }
    let events: Vec<Row> = tx
        .prepare(
            "SELECT at, kind, account_id, pool_id, detail FROM events \
             WHERE kind IN (?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) AND at >= ?1 AND at <= ?2 \
             ORDER BY at, rowid",
        )?
        .query_map(
            params![
                now - CONDITION_SPAN_MS,
                now,
                SHEDDING_ROUND,
                PIN_MIGRATED,
                WINDOW_SPENT,
                WINDOW_CLEARED,
                ROUTER_STARTED,
                SLOW_DOWN_ENDED,
                SLOW_DOWN_STARTED,
                LIMIT_RESET_CLAIMED,
                WINDOW_DROPPED,
                WINDOW_RESET
            ],
            |r| {
                Ok(Row {
                    at: r.get(0)?,
                    kind: r.get(1)?,
                    account_id: r.get(2)?,
                    pool_id: r.get(3)?,
                    detail: detail_json(r.get::<_, Option<String>>(4)?.as_deref()),
                })
            },
        )?
        .collect::<Result<_, _>>()?;
    let mut held: BTreeMap<(RoutingKind, &'static str, String), Held> = BTreeMap::new();
    let setting = |names: &[&str]| serde_json::json!(names);

    // A round stops counting once a later event on its account refunds its window. Later is the events' order, as
    // for a spent window's clear below. Window names compare as stored, since the round, these events and a claim's
    // list all name the Fable week `seven_day_fable`.
    let refunds: Vec<(usize, &Row)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e.kind.as_str(),
                LIMIT_RESET_CLAIMED | WINDOW_CLEARED | WINDOW_DROPPED | WINDOW_RESET
            )
        })
        .collect();
    let refunded_after = |index: usize, account: &str, window: &str| {
        refunds.iter().any(|(position, e)| {
            *position > index
                && e.account_id.as_deref() == Some(account)
                && if e.kind == LIMIT_RESET_CLAIMED {
                    e.detail["cleared"]
                        .as_array()
                        .is_some_and(|cleared| cleared.iter().any(|named| named == window))
                } else {
                    e.detail["window"] == window
                }
        })
    };
    let mut rounds: BTreeMap<&str, (i64, u64, BTreeSet<&str>)> = BTreeMap::new();
    for (index, event) in events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == SHEDDING_ROUND)
    {
        let no_runway =
            event.detail["outcome"] == "nothing_to_mark" && event.detail["reason"] == "no_runway";
        let Some(pool) = event.pool_id.as_deref().filter(|_| no_runway) else {
            continue;
        };
        if event
            .account_id
            .as_deref()
            .zip(event.detail["window"].as_str())
            .is_some_and(|(account, window)| refunded_after(index, account, window))
        {
            continue;
        }
        let entry = rounds.entry(pool).or_insert((event.at, 0, BTreeSet::new()));
        entry.1 += 1;
        entry.2.extend(event.account_id.as_deref());
    }
    for (pool, (since, count, accounts)) in rounds {
        held.insert(
            (RoutingKind::ShedNoRunway, "pool", pool.to_string()),
            Held {
                pool_id: Some(pool.to_string()),
                since,
                count,
                detail: serde_json::json!({
                    "accounts": accounts,
                    "rounds": count,
                    "setting": setting(CEILING_SETTINGS),
                }),
            },
        );
    }

    let cleared: Vec<(usize, &str, &str)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == WINDOW_CLEARED)
        .filter_map(|(index, e)| {
            Some((
                index,
                e.account_id.as_deref()?,
                e.detail["window"].as_str().unwrap_or("unknown"),
            ))
        })
        .collect();
    // Each spent window with whether a `window_cleared` on the same account and window came after it in the events'
    // order. Every clear the query reads sits at or before `now`, so a spent window a clear followed reaches no move.
    let spent: Vec<(&str, i64, &str, &serde_json::Value, bool)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == WINDOW_SPENT)
        .filter_map(|(index, e)| {
            let account = e.account_id.as_deref()?;
            let window = e.detail["window"].as_str().unwrap_or("unknown");
            let ended = cleared
                .iter()
                .any(|(position, cleared_on, cleared_window)| {
                    *position > index && *cleared_on == account && *cleared_window == window
                });
            Some((account, e.at, window, &e.detail, ended))
        })
        .collect();
    type Moves<'a> = (i64, u64, BTreeMap<&'a str, u64>, BTreeSet<&'a str>);
    let mut moves: BTreeMap<&str, Moves> = BTreeMap::new();
    for event in events.iter().filter(|e| e.kind == PIN_MIGRATED) {
        let Some(to) = event.account_id.as_deref() else {
            continue;
        };
        let followed: Vec<&str> = spent
            .iter()
            .filter(|(account, at, _, _, ended)| {
                !ended && *account == to && *at >= event.at && *at - event.at <= SPENT_AFTER_MOVE_MS
            })
            .map(|(_, _, window, _, _)| *window)
            .collect();
        if followed.is_empty() {
            continue;
        }
        let entry = moves
            .entry(to)
            .or_insert((event.at, 0, BTreeMap::new(), BTreeSet::new()));
        entry.1 += 1;
        *entry
            .2
            .entry(event.detail["fit"].as_str().unwrap_or("unknown"))
            .or_default() += 1;
        entry.3.extend(followed);
    }
    for (account, (since, count, fits, windows)) in moves {
        held.insert(
            (
                RoutingKind::DestinationSpent,
                "account",
                account.to_string(),
            ),
            Held {
                pool_id: None,
                since,
                count,
                detail: serde_json::json!({
                    "fits": fits,
                    "moves": count,
                    "setting": window_settings(&[], &windows),
                    "windows": windows,
                }),
            },
        );
    }

    // Per account: the oldest wall, the count, the windows and the newest wall with its window and detail.
    type Walls<'a> = (
        i64,
        u64,
        BTreeSet<&'a str>,
        (i64, &'a str, &'a serde_json::Value),
    );
    let mut walls: BTreeMap<&str, Walls> = BTreeMap::new();
    for (account, at, window, detail, _) in &spent {
        let entry =
            walls
                .entry(account)
                .or_insert((*at, 0, BTreeSet::new(), (*at, window, detail)));
        entry.1 += 1;
        entry.2.insert(window);
        entry.3 = (*at, window, detail);
    }
    for (account, (since, count, windows, (newest, window, detail))) in walls {
        let round = newest_round(tx, account, window, newest)?;
        // A projection round's projection that came true reads 1.0 at the wall, so the error is how far under the wall
        // the round projected it, read off a round whose trigger is `projection` or that names no trigger, and off no
        // other round. A stretch round's projection is the demand's at a moved horizon and measures nothing at the
        // reset, where the slow-down paces the window to arrive, so its wall reads how early it came instead. The Fable
        // reservation fires its round on the account's room while the projection can read far under the wall, so no
        // projection predicted that wall.
        let projection = round
            .as_ref()
            .filter(|round| matches!(round["trigger"].as_str(), None | Some("projection")))
            .and_then(|round| round["projection"].as_f64());
        held.insert(
            (RoutingKind::WallReached, "account", account.to_string()),
            Held {
                pool_id: None,
                since,
                count,
                detail: serde_json::json!({
                    "early_ms": early_ms(round.as_ref(), detail, newest),
                    "projection_error": projection.map(|p| 1.0 - p),
                    "setting": window_settings(WALL_SETTINGS, &windows),
                    "walls": count,
                    "windows": windows,
                }),
            },
        );
    }

    // Each start pairs with the next end of its account and pool or the next restart, whichever comes first: a restart
    // loses the memory-only state and emits no end, so it closes every span standing at it.
    let cutoff = now - CONDITION_SPAN_MS;
    let mut standing = slow_down_open_at(tx, cutoff)?;
    let mut counted: BTreeMap<String, Vec<SlowSpan>> = BTreeMap::new();
    let mut count =
        |account: &str, pool: &str, since: i64, end: i64, open: bool, threshold_ms: Option<i64>| {
            // A span with no part inside the trailing day is not counted at all.
            if end.min(now) > since.max(cutoff) {
                counted
                    .entry(account.to_string())
                    .or_default()
                    .push(SlowSpan {
                        pool: pool.to_string(),
                        since,
                        open,
                        threshold_ms,
                    });
            }
        };
    for event in &events {
        if event.kind == ROUTER_STARTED {
            // The previous run's newest row is the last instant its slow-down can have held, so the span ends there
            // rather than at the restart. A run that stopped right after the start row has that row as its newest, and
            // the span it closes is of no length.
            let down_since = event.detail["down_since"].as_i64();
            for ((account, pool), (since, threshold_ms)) in std::mem::take(&mut standing) {
                let end = down_since.filter(|down| *down >= since).unwrap_or(event.at);
                count(&account, &pool, since, end, false, threshold_ms);
            }
            continue;
        }
        let (Some(account), Some(pool)) = (event.account_id.as_deref(), event.pool_id.as_deref())
        else {
            continue;
        };
        let key = (account.to_string(), pool.to_string());
        match event.kind.as_str() {
            SLOW_DOWN_STARTED => {
                // The router emits an end before every next start, so a second start before any end or restart
                // follows a lost row alone. It opens no second span, which would count the one span twice.
                standing
                    .entry(key)
                    .or_insert((event.at, event.detail["threshold_ms"].as_i64()));
            }
            SLOW_DOWN_ENDED => {
                // An end with no span standing closes nothing this fold counts: the state at the cutoff opens a span
                // only from a start newer than any end or restart, so its start was lost or a restart closed it.
                let Some((start, threshold_ms)) = standing.remove(&key) else {
                    continue;
                };
                // The end row lands after the wait that closed the span, so the span's own start and length come
                // from the detail rather than the row's time.
                let since = event.detail["since"].as_i64().unwrap_or(start);
                let end = event.detail["duration_ms"]
                    .as_i64()
                    .map_or(event.at, |duration| since + duration);
                count(account, pool, since, end, false, threshold_ms);
            }
            _ => {}
        }
    }
    for ((account, pool), (since, threshold_ms)) in &standing {
        count(account, pool, *since, now, true, *threshold_ms);
    }
    let held_time: BTreeMap<String, (i64, i64)> = if counted.is_empty() {
        BTreeMap::new()
    } else {
        tx.prepare(
            "SELECT account_id, SUM(slow_down_held_ms), SUM(slow_down_held) FROM rollups_hourly \
             WHERE hour >= ?1 GROUP BY account_id",
        )?
        .query_map(params![hour_ceil(cutoff)], |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))
        })?
        .collect::<Result<_, _>>()?
    };
    for (account, spans) in counted {
        let (Some(oldest), Some(newest)) = (
            spans.iter().map(|span| span.since).min(),
            spans.iter().max_by_key(|span| span.since),
        ) else {
            continue;
        };
        let bar_ms = newest
            .threshold_ms
            .map_or(0, |threshold| {
                threshold.saturating_mul(SLOWED_LONG_STRETCHES)
            })
            .max(SLOWED_LONG_FLOOR_MS);
        let (held_ms, requests) = held_time.get(&account).copied().unwrap_or_default();
        if held_ms <= bar_ms {
            continue;
        }
        held.insert(
            (RoutingKind::SlowedLong, "account", account),
            Held {
                pool_id: Some(newest.pool.clone()),
                since: oldest,
                count: spans.len() as u64,
                detail: serde_json::json!({
                    "bar_ms": bar_ms,
                    "held": requests,
                    "held_ms": held_ms,
                    "open": spans.iter().any(|span| span.open),
                    "setting": setting(SLOW_SETTINGS),
                    "spans": spans.len(),
                }),
            },
        );
    }

    for (account, rows) in newest_rows(tx, now)? {
        let off: Vec<&ReconciliationRow> = rows.iter().filter(|row| row.off()).collect();
        // A row stored before slid runs wrote none, or one a reading without an instant closed, can end after `now`.
        let Some(since) = off
            .iter()
            .map(|row| row.window_end)
            .min()
            .map(|end| end.min(now))
        else {
            continue;
        };
        let windows: Vec<serde_json::Value> = off
            .iter()
            .map(|row| {
                serde_json::json!({
                    "observed_rise": row.observed_rise,
                    "predicted_share": row.predicted_share,
                    "ratio": row.ratio,
                    "requests": row.requests,
                    "window": row.window,
                    "window_end": row.window_end,
                })
            })
            .collect();
        held.insert(
            (RoutingKind::MeterDrift, "account", account),
            Held {
                pool_id: None,
                since,
                count: off.len() as u64,
                detail: serde_json::json!({
                    "effective": tables.revisions.in_force(now).effective,
                    "source": tables.source,
                    "windows": windows,
                }),
            },
        );
    }

    tx.execute("DELETE FROM routing_conditions", [])?;
    for ((kind, subject_kind, subject_id), row) in &held {
        tx.execute(
            "INSERT INTO routing_conditions (kind, subject_kind, subject_id, pool_id, since, count, detail_json, \
             computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                kind.as_str(),
                subject_kind,
                subject_id,
                row.pool_id,
                row.since,
                row.count as i64,
                row.detail.to_string(),
                now
            ],
        )?;
    }
    Ok(())
}

/// One slow-down span the fold counts: its pool, its own start, whether it still stands and the stretch threshold its
/// start row read.
struct SlowSpan {
    pool: String,
    since: i64,
    open: bool,
    threshold_ms: Option<i64>,
}

/// The slow-down spans standing at a point of the fold, by account and pool: each one's own start and the stretch
/// threshold its start read.
type OpenSpans = BTreeMap<(String, String), (i64, Option<i64>)>;

/// The spans standing at `cutoff`, per account and pool, with each one's own start and the stretch threshold its start
/// read: the newest `slow_down_started` or `slow_down_ended` row before the cutoff where that row is a start newer than
/// the newest `router_started` row before it.
fn slow_down_open_at(tx: &Transaction<'_>, cutoff: i64) -> Result<OpenSpans, LedgerError> {
    let restart: Option<(i64, i64)> = tx
        .query_row(
            "SELECT at, rowid FROM events WHERE kind = ?2 AND at < ?1 \
             ORDER BY at DESC, rowid DESC LIMIT 1",
            params![cutoff, ROUTER_STARTED],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut stmt = tx.prepare(
        "SELECT at, id, account_id, pool_id, detail FROM ( \
           SELECT at, rowid AS id, kind, account_id, pool_id, detail, ROW_NUMBER() OVER ( \
             PARTITION BY account_id, pool_id ORDER BY at DESC, rowid DESC) AS newest \
           FROM events WHERE kind IN (?2, ?3) AND at < ?1 \
             AND account_id IS NOT NULL AND pool_id IS NOT NULL) \
         WHERE newest = 1 AND kind = ?3",
    )?;
    let starts = stmt
        .query_map(params![cutoff, SLOW_DOWN_ENDED, SLOW_DOWN_STARTED], |r| {
            Ok((
                (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?),
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(starts
        .into_iter()
        .filter(|(start, ..)| restart.is_none_or(|restart| restart < *start))
        .map(|((at, _), account, pool, detail)| {
            let threshold_ms = detail_json(detail.as_deref())["threshold_ms"].as_i64();
            ((account, pool), (at, threshold_ms))
        })
        .collect())
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
    static STATEMENT: LazyLock<String> = LazyLock::new(|| {
        format!("SELECT {ACCOUNT_COLUMNS} FROM account_rollups_hourly WHERE hour = ?1 AND account_id = ?2")
    });
    Ok(tx
        .prepare_cached(&STATEMENT)?
        .query_row(params![hour, account_id], row_to_account)
        .optional()?
        .unwrap_or_else(|| AccountRollupRow {
            hour,
            account_id: account_id.to_string(),
            ..Default::default()
        }))
}

fn write_account(tx: &Transaction<'_>, row: &AccountRollupRow) -> Result<(), LedgerError> {
    static STATEMENT: LazyLock<String> = LazyLock::new(|| {
        upsert_statement(
            "account_rollups_hourly",
            ACCOUNT_COLUMNS,
            "hour, account_id",
        )
    });
    tx.prepare_cached(&STATEMENT)?.execute(params![
        row.hour,
        row.account_id,
        row.utilization_5h_max,
        row.utilization_7d_max,
        row.utilization_7d_oi_max,
        row.overage_seconds as i64,
        row.cooldown_seconds as i64,
        row.pins_created as i64,
        row.pins_migrated_in as i64,
        row.pins_migrated_out as i64,
        row.rewrite_tokens as i64,
        counts_json(&row.migrations),
        row.wall_early_ms as i64,
    ])?;
    Ok(())
}

/// Takes back cooldown seconds an announcement counted that the account never spent. A row the take-back leaves
/// holding nothing is deleted, so a cooldown cleared early lists no hour of zeros.
fn unfold_cooldown(
    tx: &Transaction<'_>,
    hour: i64,
    account_id: &str,
    seconds: u64,
) -> Result<(), LedgerError> {
    let mut row = account_row(tx, hour, account_id)?;
    row.cooldown_seconds = row.cooldown_seconds.saturating_sub(seconds);
    if row.holds_nothing() {
        tx.prepare_cached(
            "DELETE FROM account_rollups_hourly WHERE hour = ?1 AND account_id = ?2",
        )?
        .execute(params![hour, account_id])?;
        return Ok(());
    }
    write_account(tx, &row)
}

/// Takes back a replaced request's rewrite booking, so a replay of the same request never doubles it.
fn unfold_rewrite(
    tx: &Transaction<'_>,
    hour: i64,
    account_id: &str,
    tokens: u64,
) -> Result<(), LedgerError> {
    let mut row = account_row(tx, hour, account_id)?;
    row.rewrite_tokens = row.rewrite_tokens.saturating_sub(tokens);
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
        .prepare_cached("SELECT until FROM cooldown_spans WHERE account_id = ?1 AND scope = ?2")?
        .query_row(params![account_id, scope], |r| r.get(0))
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
    tx.prepare_cached(
        "INSERT OR REPLACE INTO cooldown_spans (account_id, scope, hour, until) VALUES (?1, ?2, 0, ?3)",
    )?
    .execute(params![account_id, scope, until])?;
    Ok(())
}

fn take_span(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
) -> Result<Option<i64>, LedgerError> {
    let span = read_span(tx, account_id, scope)?;
    if span.is_some() {
        tx.prepare_cached("DELETE FROM cooldown_spans WHERE account_id = ?1 AND scope = ?2")?
            .execute(params![account_id, scope])?;
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
    tx.prepare_cached(
        "INSERT INTO cooldown_segments (account_id, scope, hour, seconds) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, scope, hour) DO UPDATE SET seconds = seconds + excluded.seconds",
    )?
    .execute(params![account_id, scope, hour, seconds as i64])?;
    Ok(())
}

/// Every hour a scope's open cooldown booked seconds into, removed so a clear reads each exactly once.
fn take_segments(
    tx: &Transaction<'_>,
    account_id: &str,
    scope: &str,
) -> Result<Vec<(i64, u64)>, LedgerError> {
    let segments = {
        let mut stmt = tx.prepare_cached(
            "SELECT hour, seconds FROM cooldown_segments WHERE account_id = ?1 AND scope = ?2",
        )?;
        let rows = stmt.query_map(params![account_id, scope], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.prepare_cached("DELETE FROM cooldown_segments WHERE account_id = ?1 AND scope = ?2")?
        .execute(params![account_id, scope])?;
    Ok(segments)
}

/// The seconds of a booked cooldown segment that have not elapsed at `at`: the part of the hour between `at` and
/// the earlier of the span's end and the hour's end, capped by the segment. `at` and `span_end` are Unix
/// milliseconds, and `hour` is the hour's start in seconds as `hour_of` answers it.
pub(crate) fn unelapsed_segment_seconds(
    at: i64,
    hour: i64,
    span_end: i64,
    segment_seconds: u64,
) -> u64 {
    let unelapsed = (span_end.min(hour_end(hour) * 1_000) - at.max(hour * 1_000)).max(0) / 1_000;
    (unelapsed as u64).min(segment_seconds)
}

/// Per account, the seconds of the current hour that its open cooldowns booked and that have not passed at `now`, an
/// instant in Unix milliseconds. Reads `cooldown_spans` and `cooldown_segments` and writes nothing.
pub fn open_cooldown_remainders(
    conn: &Connection,
    now: i64,
) -> Result<BTreeMap<String, i64>, LedgerError> {
    let hour = hour_of(now);
    let mut stmt = conn.prepare_cached(
        "SELECT s.account_id, s.until, g.seconds FROM cooldown_spans s JOIN cooldown_segments g \
         ON g.account_id = s.account_id AND g.scope = s.scope AND g.hour = ?1",
    )?;
    let segments = stmt.query_map(params![hour], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)? as u64,
        ))
    })?;
    let mut remainders = BTreeMap::new();
    for segment in segments {
        let (account_id, until, seconds) = segment?;
        *remainders.entry(account_id).or_insert(0) +=
            unelapsed_segment_seconds(now, hour, until, seconds) as i64;
    }
    Ok(remainders)
}

const COLUMNS: &str = "hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding, band, tier, requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, kept_warm_requests, kept_warm_5m_tokens, probe_migrated, wasted_probes, wasted_probe_reasons_json, probes_skipped, wasted_input_tokens, wasted_cache_read_tokens, wasted_cache_write_5m_tokens, wasted_cache_write_1h_tokens, wasted_output_tokens, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, errors_json, cache_miss_reasons_json, incomplete, probes_held, slow_down_held_ms, slow_down_held, kept_warm_1h_tokens, classifier, refusals_json, client_canceled, input_transformations_json, uncacheable";
const ACCOUNT_COLUMNS: &str = "hour, account_id, utilization_5h_max, utilization_7d_max, utilization_7d_oi_max, overage_seconds, cooldown_seconds, pins_created, pins_migrated_in, pins_migrated_out, rewrite_tokens, migrations_json, wall_early_ms";

/// The rollup key, the table's primary key in its order: the upsert's conflict target, the order a read answers in and
/// the columns `KEY_MATCH` names.
const KEY_COLUMNS: [&str; 16] = [
    "hour",
    "key_id",
    "key_generation",
    "pool_id",
    "account_id",
    "canonical_model",
    "kind",
    "user_agent",
    "client",
    "effort",
    "request_class",
    "speed",
    "funding",
    "band",
    "tier",
    "classifier",
];

/// The sixteen terms that match one rollup key, bound in `key_params`' order.
static KEY_MATCH: LazyLock<String> = LazyLock::new(|| {
    KEY_COLUMNS
        .iter()
        .enumerate()
        .map(|(i, column)| format!("{column} = ?{}", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ")
});

fn key_params(k: &RollupRow) -> [&dyn ToSql; 16] {
    [
        &k.hour,
        &k.key_id,
        &k.key_generation,
        &k.pool_id,
        &k.account_id,
        &k.canonical_model,
        &k.kind,
        &k.user_agent,
        &k.client,
        &k.effort,
        &k.request_class,
        &k.speed,
        &k.funding,
        &k.band,
        &k.tier,
        &k.classifier,
    ]
}

/// The upsert of `columns` into `table` on the conflict target `key`, every column outside `key` taking the written
/// row's value, bound in `columns`' order.
fn upsert_statement(table: &str, columns: &str, key: &str) -> String {
    let marks = (1..=columns.split(", ").count())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let set = columns
        .split(", ")
        .filter(|column| !key.split(", ").any(|k| k == *column))
        .map(|column| format!("{column} = excluded.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {table} ({columns}) VALUES ({marks}) ON CONFLICT ({key}) DO UPDATE SET {set}"
    )
}

/// A count map as its column's JSON, a map that does not encode writing as no count.
fn counts_json(m: &BTreeMap<String, u64>) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".into())
}

pub(crate) fn read_rollup(
    conn: &Connection,
    key: &RollupRow,
) -> Result<Option<RollupRow>, LedgerError> {
    static STATEMENT: LazyLock<String> =
        LazyLock::new(|| format!("SELECT {COLUMNS} FROM rollups_hourly WHERE {}", *KEY_MATCH));
    Ok(conn
        .prepare_cached(&STATEMENT)?
        .query_row(key_params(key), row_to_rollup)
        .optional()?)
}

pub(crate) fn delete_rollup(conn: &Connection, key: &RollupRow) -> Result<(), LedgerError> {
    static STATEMENT: LazyLock<String> =
        LazyLock::new(|| format!("DELETE FROM rollups_hourly WHERE {}", *KEY_MATCH));
    conn.prepare_cached(&STATEMENT)?.execute(key_params(key))?;
    Ok(())
}

pub(crate) fn write_rollup(conn: &Connection, r: &RollupRow) -> Result<(), LedgerError> {
    // An upsert on the sixteen-column key updates a folded row in place where a replace deletes and inserts it again,
    // rewriting both indexes' entries and the rowid the startup stamp pages by.
    static STATEMENT: LazyLock<String> =
        LazyLock::new(|| upsert_statement("rollups_hourly", COLUMNS, &KEY_COLUMNS.join(", ")));
    conn.prepare_cached(&STATEMENT)?.execute(params![
        r.hour,
        r.key_id,
        r.key_generation,
        r.pool_id,
        r.account_id,
        r.canonical_model,
        r.kind,
        r.user_agent,
        r.client,
        r.effort,
        r.request_class,
        r.speed,
        r.funding,
        r.band,
        r.tier,
        r.requests as i64,
        r.succeeded as i64,
        r.rate_limited as i64,
        r.failed as i64,
        r.exhausted as i64,
        r.rejected as i64,
        r.attempts as i64,
        r.failovers as i64,
        r.cache_hits as i64,
        r.cache_misses as i64,
        r.uncached_requests as i64,
        r.input_tokens as i64,
        r.cache_read_tokens as i64,
        r.cache_write_5m_tokens as i64,
        r.cache_write_1h_tokens as i64,
        r.output_tokens as i64,
        r.thinking_tokens as i64,
        r.web_search_requests as i64,
        r.web_fetch_requests as i64,
        r.kept_warm_requests as i64,
        r.kept_warm_5m_tokens as i64,
        r.probe_migrated as i64,
        r.wasted_probes as i64,
        counts_json(&r.wasted_probe_reasons),
        r.probes_skipped as i64,
        r.wasted_input_tokens as i64,
        r.wasted_cache_read_tokens as i64,
        r.wasted_cache_write_5m_tokens as i64,
        r.wasted_cache_write_1h_tokens as i64,
        r.wasted_output_tokens as i64,
        r.ttfb_hist.to_le_bytes(),
        r.ttfc_hist.to_le_bytes(),
        r.duration_hist.to_le_bytes(),
        r.output_tps_hist.to_le_bytes(),
        counts_json(&r.errors),
        counts_json(&r.cache_miss_reasons),
        r.incomplete as i64,
        r.probes_held as i64,
        r.slow_down_held_ms as i64,
        r.slow_down_held as i64,
        r.kept_warm_1h_tokens as i64,
        r.classifier,
        counts_json(&r.refusals),
        r.client_canceled as i64,
        counts_json(&r.input_transformations),
        r.uncacheable as i64,
    ])?;
    Ok(())
}

fn row_to_rollup(r: &rusqlite::Row<'_>) -> rusqlite::Result<RollupRow> {
    row_to_rollup_at(r, 0)
}

/// The row's columns read from `offset`, so a select that puts something of its own in front of `COLUMNS`
/// needs no second mapper.
fn row_to_rollup_at(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<RollupRow> {
    let u = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, i64>(offset + i)? as u64) };
    let counts = |i: usize| counts_at(r, offset + i);
    let hist = |i: usize| -> rusqlite::Result<Histogram> {
        Ok(Histogram::from_le_bytes(r.get_ref(offset + i)?.as_blob()?))
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
        client: r.get(offset + 8)?,
        effort: r.get(offset + 9)?,
        request_class: r.get(offset + 10)?,
        speed: r.get(offset + 11)?,
        funding: r.get(offset + 12)?,
        band: r.get(offset + 13)?,
        tier: r.get(offset + 14)?,
        requests: u(15)?,
        succeeded: u(16)?,
        rate_limited: u(17)?,
        failed: u(18)?,
        client_canceled: u(58)?,
        exhausted: u(19)?,
        rejected: u(20)?,
        attempts: u(21)?,
        failovers: u(22)?,
        cache_hits: u(23)?,
        cache_misses: u(24)?,
        uncached_requests: u(25)?,
        uncacheable: u(60)?,
        input_tokens: u(26)?,
        cache_read_tokens: u(27)?,
        cache_write_5m_tokens: u(28)?,
        cache_write_1h_tokens: u(29)?,
        output_tokens: u(30)?,
        thinking_tokens: u(31)?,
        web_search_requests: u(32)?,
        web_fetch_requests: u(33)?,
        kept_warm_requests: u(34)?,
        kept_warm_5m_tokens: u(35)?,
        probe_migrated: u(36)?,
        wasted_probes: u(37)?,
        wasted_probe_reasons: counts(38)?,
        probes_skipped: u(39)?,
        wasted_input_tokens: u(40)?,
        wasted_cache_read_tokens: u(41)?,
        wasted_cache_write_5m_tokens: u(42)?,
        wasted_cache_write_1h_tokens: u(43)?,
        wasted_output_tokens: u(44)?,
        ttfb_hist: hist(45)?,
        ttfc_hist: hist(46)?,
        duration_hist: hist(47)?,
        output_tps_hist: hist(48)?,
        errors: counts(49)?,
        cache_miss_reasons: counts(50)?,
        incomplete: u(51)?,
        probes_held: u(52)?,
        slow_down_held_ms: u(53)?,
        slow_down_held: u(54)?,
        kept_warm_1h_tokens: u(55)?,
        classifier: r.get(offset + 56)?,
        refusals: counts(57)?,
        input_transformations: counts(59)?,
    })
}

/// A JSON count map's column, read in place, an unparsable text reading as no count.
fn counts_at(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<BTreeMap<String, u64>> {
    Ok(serde_json::from_str(r.get_ref(i)?.as_str()?).unwrap_or_default())
}

fn expired_at(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
    Ok(counts_at(r, i)?
        .get(EXPIRED_BETWEEN_TURNS)
        .copied()
        .unwrap_or(0))
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
        rewrite_tokens: r.get::<_, i64>(10)? as u64,
        migrations: counts_at(r, 11)?,
        wall_early_ms: r.get::<_, i64>(12)? as u64,
    })
}

/// One key's rows: the pair `KEY_RANGE_PREDICATE` binds, or every generation of the id where a reader matches the
/// id alone (the reserved key, which has no lifetime of its own, and a series scope).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySelector {
    pub key_id: String,
    pub key_generation: Option<i64>,
}

/// The fixed filters the read narrows by in SQL: every list empty selects all.
#[derive(Clone, Debug, Default)]
pub struct RollupSelect {
    pub pools: Vec<String>,
    pub accounts: Vec<String>,
    pub kinds: Vec<String>,
    /// Canonical ids, which the rows carry lowercased.
    pub models: Vec<String>,
    pub keys: Vec<KeySelector>,
    /// Patterns under the wildcard grammar: a row matches when any of them matches its client in full.
    pub clients: Vec<String>,
    /// Patterns under the wildcard grammar: a row matches when any of them matches its agent in full.
    pub user_agents: Vec<String>,
    /// The probe rows the plan funded left out in SQL, for a read that counts no probe as a request and still sums a
    /// billed probe's dollars, so it decodes no plan probe to drop it.
    pub exclude_plan_probes: bool,
}

/// The clause of a read that counts no probe as a request: it leaves out the probe rows the plan funded and keeps a
/// billed probe's, whose dollars the cost sums count.
pub(crate) const NO_PLAN_PROBES: &str = "(kind != 'probe' OR funding != 'plan')";

/// The page of the paged read, `read_rollups_paged`, which takes each page as its own read and folds it into the
/// accumulators as it arrives: a long range holds one page in memory and releases a reader connection between pages.
pub const PAGE_HOURS: i64 = 24;

/// The selecting statement and its binds, spelled once so the plan test explains the statement the read runs. A pool
/// with an account bound names the member index: the primary key's hour range yields the order's leading terms, and
/// statistics can price that range below the member's seek, which on a long range reads one member's rows where the
/// range reads every member's. Every other form names nothing, the pool alone under its unary plus included, since on
/// a form that binds no leading column the named index forces a walk of the whole index and a sort.
pub(crate) fn select_statement(
    from_hour: i64,
    to_hour: i64,
    select: &RollupSelect,
) -> (String, Vec<Value>) {
    let mut binds = vec![Value::Integer(from_hour), Value::Integer(to_hour)];
    let mut clauses = vec!["hour >= ?1 AND hour < ?2".to_string()];
    pool_term(&select.pools, &select.accounts, &mut binds, &mut clauses);
    in_list("account_id", &select.accounts, &mut binds, &mut clauses);
    in_list("kind", &select.kinds, &mut binds, &mut clauses);
    if select.exclude_plan_probes {
        clauses.push(NO_PLAN_PROBES.into());
    }
    in_list("canonical_model", &select.models, &mut binds, &mut clauses);
    key_clause(&select.keys, false, &mut binds, &mut clauses);
    pattern_any("user_agent", &select.user_agents, &mut binds, &mut clauses);
    pattern_any("client", &select.clients, &mut binds, &mut clauses);
    let indexed = if select.pools.is_empty() || select.accounts.is_empty() {
        ""
    } else {
        " INDEXED BY rollups_hourly_member"
    };
    (
        format!(
            "SELECT {COLUMNS} FROM rollups_hourly{indexed} WHERE {} ORDER BY {}",
            clauses.join(" AND "),
            KEY_COLUMNS.join(", ")
        ),
        binds,
    )
}

pub fn read_rollups(
    conn: &Connection,
    from_hour: i64,
    to_hour: i64,
    select: &RollupSelect,
) -> Result<Vec<RollupRow>, LedgerError> {
    let mut rows = Vec::new();
    visit_rollups(conn, from_hour, to_hour, select, |row| rows.push(row))?;
    Ok(rows)
}

/// The rows of `[from_hour, to_hour)` under `select` on `conn`, handed to `visit` as the statement steps, so a caller
/// that folds them holds one row at a time.
pub(crate) fn visit_rollups(
    conn: &Connection,
    from_hour: i64,
    to_hour: i64,
    select: &RollupSelect,
    mut visit: impl FnMut(RollupRow),
) -> Result<(), LedgerError> {
    let (sql, binds) = select_statement(from_hour, to_hour, select);
    // A page loop reads one text a page, so a connection that served it once reuses the compiled statement.
    let mut stmt = conn.prepare_cached(&sql)?;
    for row in stmt.query_map(params_from_iter(binds), row_to_rollup)? {
        visit(row?);
    }
    Ok(())
}

/// The count every row of `[from_hour, to_hour)` files under `EXPIRED_BETWEEN_TURNS`, and the probe and kept-warm rows
/// among them in no order, since the overview's fold only sums them and lists no other row.
pub fn read_overview_rollups(
    conn: &Connection,
    from_hour: i64,
    to_hour: i64,
) -> Result<(u64, Vec<OverviewRollupRow>), LedgerError> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind, kept_warm_requests, cache_miss_reasons_json, funding, pool_id, account_id, succeeded, failed, \
         rate_limited, exhausted, rejected, incomplete, probes_held, probe_migrated, probes_skipped, wasted_probes, \
         wasted_probe_reasons_json FROM rollups_hourly WHERE hour >= ?1 AND hour < ?2",
    )?;
    let mut rows = stmt.query(params![from_hour, to_hour])?;
    let mut expired = 0u64;
    let mut kept = Vec::new();
    while let Some(r) = rows.next()? {
        expired += expired_at(r, 2)?;
        let u = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, i64>(i)? as u64) };
        let kept_warm_requests = u(1)?;
        let kind = r.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        if kind != Kind::Probe.as_str() && kept_warm_requests == 0 {
            continue;
        }
        kept.push(OverviewRollupRow {
            kind: kind.to_string(),
            funding: r.get(3)?,
            pool_id: r.get(4)?,
            account_id: r.get(5)?,
            succeeded: u(6)?,
            failed: u(7)?,
            rate_limited: u(8)?,
            exhausted: u(9)?,
            rejected: u(10)?,
            incomplete: u(11)?,
            probes_held: u(12)?,
            probe_migrated: u(13)?,
            probes_skipped: u(14)?,
            kept_warm_requests,
            wasted_probes: u(15)?,
            wasted_probe_reasons: counts_at(r, 16)?,
        });
    }
    Ok((expired, kept))
}

/// One pool and key's count of `read_expired_between_turns`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredBetweenTurns {
    pub pool_id: String,
    pub key_id: String,
    pub expired: u64,
}

/// The expiries between turns of `[from_hour, to_hour)` by the pool and the key their requests ran under, for a
/// reader that counts some pools and keys alone.
pub fn read_expired_between_turns(
    conn: &Connection,
    from_hour: i64,
    to_hour: i64,
) -> Result<Vec<ExpiredBetweenTurns>, LedgerError> {
    let mut stmt = conn.prepare_cached(
        "SELECT pool_id, key_id, cache_miss_reasons_json FROM rollups_hourly \
         WHERE hour >= ?1 AND hour < ?2",
    )?;
    let mut rows = stmt.query(params![from_hour, to_hour])?;
    let mut by_pair: BTreeMap<(String, String), u64> = BTreeMap::new();
    while let Some(r) = rows.next()? {
        let expired = expired_at(r, 2)?;
        if expired > 0 {
            *by_pair.entry((r.get(0)?, r.get(1)?)).or_default() += expired;
        }
    }
    Ok(by_pair
        .into_iter()
        .map(|((pool_id, key_id), expired)| ExpiredBetweenTurns {
            pool_id,
            key_id,
            expired,
        })
        .collect())
}

/// The rows of `[from_hour, to_hour)` under `select`, one `PAGE_HOURS` page per `read`, handed to `visit` in the
/// order one read of the whole range would answer them.
pub async fn read_rollups_paged(
    db: &Arc<LedgerDb>,
    select: &RollupSelect,
    from_hour: i64,
    to_hour: i64,
    mut visit: impl FnMut(&RollupRow),
) -> Result<(), LedgerError> {
    let select = Arc::new(select.clone());
    let mut page = from_hour;
    while page < to_hour {
        let end = page.saturating_add(PAGE_HOURS * 3_600).min(to_hour);
        let select = Arc::clone(&select);
        let rows = db
            .read(move |c| read_rollups(c, page, end, &select))
            .await?;
        rows.iter().for_each(&mut visit);
        page = end;
    }
    Ok(())
}

/// One page of one key's rows at generation 0, the unstamped rows the startup stamp moves onto a live key:
/// `rollups_hourly` is a rowid table, so the page after `after_rowid` is the next `limit` rows in rowid order and
/// the caller carries the last rowid forward. `from_hour` narrows to the rows at or after an hour, for an id whose
/// earlier holder's rows sit at generation 0 too.
pub(crate) fn read_unstamped_rollups_of_key(
    conn: &Connection,
    key_id: &str,
    from_hour: Option<i64>,
    after_rowid: i64,
    limit: usize,
) -> Result<Vec<(i64, RollupRow)>, LedgerError> {
    let mut stmt = conn.prepare(&format!("SELECT rowid, {COLUMNS} FROM rollups_hourly WHERE key_id = ?1 AND key_generation = 0 AND hour >= ?2 AND rowid > ?3 ORDER BY rowid LIMIT ?4"))?;
    let rows = stmt.query_map(
        params![
            key_id,
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

/// `read_rollups_for_key`'s statement, its order included, spelled once so the seeded plan test explains the
/// statement the read runs. It binds `KEY_RANGE_PREDICATE`'s four values.
pub(crate) fn key_range_statement() -> String {
    format!("SELECT {COLUMNS} FROM rollups_hourly WHERE {KEY_RANGE_PREDICATE} AND kind != 'probe' ORDER BY hour, pool_id, account_id, canonical_model, kind, user_agent, client, effort, request_class, speed, funding, band, tier, classifier")
}

/// The rows of one key over a range, probes left out, narrowed by SQLite rather than in memory: a reader that wants
/// a single key pays for that key's rows and not for every key's, and decodes no probe row to drop it.
pub fn read_rollups_for_key(
    conn: &Connection,
    key_id: &str,
    key_generation: i64,
    from_hour: i64,
    to_hour: i64,
) -> Result<Vec<RollupRow>, LedgerError> {
    let mut stmt = conn.prepare(&key_range_statement())?;
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
        "SELECT {ACCOUNT_COLUMNS} FROM account_rollups_hourly WHERE hour >= ?1 AND hour < ?2 ORDER BY hour, account_id"
    ))?;
    let rows = stmt.query_map(params![from_hour, to_hour], row_to_account)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use phoebis_store::AccountKind;

    use super::*;
    use crate::pricing::Band;
    use crate::record::*;

    /// The test price list. The requests here arrive before its first date, so each prices at its first entry.
    fn prices() -> PriceRevisions {
        crate::test_tables::api_prices()
    }

    /// A model the test list carries without a long-context block, so its attempts fold under the standard band.
    const SONNET: &str = "claude-sonnet-5-5";

    /// The test list's model priced by prompt length.
    const HAIKU: &str = "claude-haiku-5-5";

    /// `HAIKU`'s threshold in the entry in force at `at`.
    fn haiku_over(at: i64) -> u64 {
        prices().in_force(at).models[HAIKU]
            .long_context
            .as_ref()
            .expect("the test list's Haiku 5.5 is priced by prompt length")
            .over
    }

    /// The one delta a request files, for a case whose attempts fall in one band, speed and funding.
    fn one_delta(
        r: &RequestRecord,
        attempts: &[AttemptRecord],
        prices: &PriceRevisions,
    ) -> RollupRow {
        let mut deltas = deltas_of(r, attempts, prices, FoldRules::ALL);
        assert_eq!(deltas.len(), 1, "{deltas:?}");
        deltas.remove(0)
    }

    /// An answered attempt whose prompt is `prompt` uncached input tokens, with ten output tokens.
    fn prompted(n: u32, account: &str, prompt: u64) -> AttemptRecord {
        AttemptRecord {
            input_tokens: Some(prompt),
            ..attempt(n, account, None, None, Some(10))
        }
    }

    /// An attempt with no answer, so no usage: a 429 before the stream, say.
    fn unanswered(n: u32, account: &str) -> AttemptRecord {
        AttemptRecord {
            input_tokens: None,
            outcome: "failed".into(),
            upstream_status: Some(429),
            thinking_tokens: Some(7),
            ..attempt(n, account, None, None, None)
        }
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
            cache_miss_reason: Some("none".into()),
            ratelimit_5h_utilization: Some(0.5),
            ..Default::default()
        }
    }

    /// A caller picks its user agent, client hints, effort and class, so each reaches the hour row's key folded to
    /// a bounded vocabulary, and an absent one keeps the reserved id, which a keepalive delta that carries none shares.
    #[test]
    fn the_rollup_key_folds_each_caller_controlled_dimension_to_a_bounded_vocabulary() {
        let long = "x".repeat(300);
        assert_eq!(fold_effort(Some(&long)), OTHER_DIMENSION);
        assert_eq!(fold_effort(Some("HIGH")), OTHER_DIMENSION);
        for level in EFFORT_LEVELS.iter().chain(&[NO_EFFORT]) {
            assert_eq!(fold_effort(Some(level)), *level);
        }
        assert_eq!(fold_effort(None), NO_EFFORT);
        assert_eq!(fold_identifier(&long), OTHER_DIMENSION);
        assert_eq!(fold_identifier(&"a".repeat(64)), "a".repeat(64));
        assert_eq!(fold_identifier(&"a".repeat(65)), OTHER_DIMENSION);
        assert_eq!(fold_identifier("main thread"), OTHER_DIMENSION);
        assert_eq!(
            fold_identifier("claude-code/sdk-cli"),
            "claude-code/sdk-cli"
        );
        assert_eq!(fold_identifier(UNATTRIBUTED), UNATTRIBUTED);
        assert_eq!(
            fold_user_agent("claude-cli/2.1.286 (external, cli)"),
            "claude-cli/2.1.286"
        );
        assert_eq!(fold_user_agent("curl"), OTHER_DIMENSION);
        assert_eq!(fold_user_agent("(external, cli)"), OTHER_DIMENSION);
        assert_eq!(fold_user_agent("a/b/c"), OTHER_DIMENSION);
        assert_eq!(fold_user_agent("/2.1"), OTHER_DIMENSION);
        assert_eq!(fold_user_agent("claude-cli/"), OTHER_DIMENSION);
        assert_eq!(fold_user_agent(UNATTRIBUTED), OTHER_DIMENSION);
        assert_eq!(
            fold_user_agent(&format!("{}/1.0 (x)", "n".repeat(100))),
            "n".repeat(64)
        );
        let mut r = request("succeeded", Some(200), Some("k"));
        r.user_agent = Some("claude-cli/2.1.286 (external, cli)".into());
        r.client = Some(long.clone());
        r.effort = Some(long.clone());
        r.request_class = Some(long.clone());
        let d = one_delta(&r, &[], &prices());
        assert_eq!(
            (
                d.user_agent.as_str(),
                d.client.as_str(),
                d.effort.as_str(),
                d.request_class.as_str()
            ),
            (
                "claude-cli/2.1.286",
                OTHER_DIMENSION,
                OTHER_DIMENSION,
                OTHER_DIMENSION
            ),
            "the key files the folded values"
        );
        r.user_agent = Some("no product token".into());
        assert_eq!(one_delta(&r, &[], &prices()).user_agent, OTHER_DIMENSION);
        r.user_agent = None;
        r.client = None;
        r.effort = None;
        r.request_class = None;
        let d = one_delta(&r, &[], &prices());
        assert_eq!(
            (
                d.user_agent.as_str(),
                d.client.as_str(),
                d.effort.as_str(),
                d.request_class.as_str()
            ),
            (UNATTRIBUTED, UNATTRIBUTED, NO_EFFORT, NO_CLASS),
            "an absent value keeps the reserved id it always filed under"
        );
    }

    /// A fast request refused before any attempt served files under the speed it asked for, and its funding under
    /// `plan`, since nothing was spent; any other text the caller sent as its speed files under `standard`, and
    /// an answer's own speed outranks the one asked for.
    #[test]
    fn a_request_with_no_served_speed_files_under_fast_only_when_it_asked_for_fast() {
        let refused = RequestRecord {
            requested_speed: Some("fast".into()),
            ..request("exhausted", Some(429), Some("k"))
        };
        let d = one_delta(&refused, &[], &prices());
        assert_eq!(
            (d.speed.as_str(), d.funding.as_str()),
            ("fast", PLAN_FUNDING),
            "the refused fast request files under fast and spent nothing"
        );
        let caller_text = RequestRecord {
            requested_speed: Some("s".repeat(300)),
            ..refused.clone()
        };
        assert_eq!(
            one_delta(&caller_text, &[], &prices()).speed,
            STANDARD_SPEED,
            "the caller's text never keys a row"
        );
        let mut served = attempt(1, "a", None, None, Some(5));
        served.speed = Some(STANDARD_SPEED.into());
        assert_eq!(
            one_delta(&refused, &[served], &prices()).speed,
            STANDARD_SPEED,
            "the speed the answer reported outranks the one the request asked for"
        );
    }

    /// A probe the router held with `503 ledger_behind` asked no account, so it counts under its held column alone:
    /// no cache figure, `uncached_requests` included, and no timing sample, whatever times its row carries.
    #[test]
    fn a_held_probe_counts_under_no_request_figure_and_no_timing() {
        let held = RequestRecord {
            kind: "probe".into(),
            origin: "keepalive".into(),
            error_code: Some("ledger_behind".into()),
            attempts: 0,
            ..request("rejected", Some(503), Some("k"))
        };
        let d = one_delta(&held, &[], &prices());
        assert_eq!(
            (
                (d.probes_held, d.requests),
                (d.cache_hits, d.cache_misses, d.uncached_requests),
                (
                    d.ttfb_hist.count(),
                    d.ttfc_hist.count(),
                    d.duration_hist.count(),
                    d.output_tps_hist.count()
                ),
            ),
            ((1, 0), (0, 0, 0), (0, 0, 0, 0)),
            "the held probe files under its held column alone, with no cache figure and no timing sample"
        );
    }

    /// The fold reads the cancel's code: a client that left counts apart from the failures, and the router's own
    /// shutdown stays a failure with its code.
    #[test]
    fn a_client_that_left_counts_apart_from_the_failures_and_a_router_shutdown_stays_one() {
        let canceled = |code: &str| RequestRecord {
            error_code: Some(code.into()),
            ..request("canceled", None, Some("k"))
        };
        let answered = [attempt(1, "a", None, None, Some(10))];
        let d = one_delta(&canceled("client_gone"), &answered, &prices());
        assert_eq!(
            (
                d.requests,
                d.client_canceled,
                d.failed,
                d.errors.get("client_gone")
            ),
            (1, 1, 0, None),
            "a client's cancel counts in the requests and under no failure and no code"
        );
        let d = one_delta(&canceled("router_shutdown"), &answered, &prices());
        assert_eq!(
            (d.client_canceled, d.failed, d.errors.get("router_shutdown")),
            (0, 1, Some(&1)),
            "the router's own shutdown stays a failure with its code"
        );
    }

    /// A probe's cancel stays a failure whatever its code, so the keepalive's figures read every probe that did not
    /// answer.
    #[test]
    fn a_probe_canceled_under_client_gone_stays_a_failure_with_its_code() {
        let probe = RequestRecord {
            kind: "probe".into(),
            origin: "keepalive".into(),
            error_code: Some("client_gone".into()),
            ..request("canceled", None, Some("k"))
        };
        let d = one_delta(&probe, &[attempt(1, "a", None, None, Some(10))], &prices());
        assert_eq!(
            (d.client_canceled, d.failed, d.errors.get("client_gone")),
            (0, 1, Some(&1))
        );
    }

    /// A histogram holding the one sample `value`.
    fn sampled(value: f64) -> Histogram {
        let mut h = Histogram::new();
        h.record(value);
        h
    }

    /// `request`'s answer with its first content at 300 ms and its end at `duration_ms`, ten tokens out.
    #[test]
    fn a_tokens_per_second_sample_needs_500_ms_from_the_first_content_to_the_end() {
        let ended = |duration_ms: i64| RequestRecord {
            duration_ms: Some(duration_ms),
            ..request("succeeded", Some(200), Some("k"))
        };
        let answered = [attempt(1, "a", None, None, Some(10))];
        assert_eq!(
            one_delta(&ended(799), &answered, &prices())
                .output_tps_hist
                .count(),
            0,
            "a span of 499 ms records no sample"
        );
        assert_eq!(
            one_delta(&ended(800), &answered, &prices()).output_tps_hist,
            sampled(20.0),
            "ten tokens over 500 ms"
        );
    }

    /// A request whose one wait carries `reason`, held `held_ms` before its first byte at `ttfb_ms`.
    fn held(reason: &str, held_ms: u64, ttfb_ms: i64) -> RequestRecord {
        RequestRecord {
            held: Some(format!(
                r#"{{"waits":[{{"reason":"{reason}","held_ms":{held_ms}}}]}}"#
            )),
            ttfb_ms: Some(ttfb_ms),
            ..request("succeeded", Some(200), Some("k"))
        }
    }

    #[test]
    fn the_millisecond_samples_leave_out_the_slow_down_wait_and_keep_any_other() {
        let answered = [attempt(1, "a", None, None, Some(10))];
        assert_eq!(
            one_delta(&held("slow_down", 119_991, 121_318), &answered, &prices()).ttfb_hist,
            sampled(1_327.0)
        );
        assert_eq!(
            one_delta(&held("reset", 30_000, 31_000), &answered, &prices()).ttfb_hist,
            sampled(31_000.0)
        );
    }

    /// `request`'s duration is 900 ms.
    #[test]
    fn a_slow_down_wait_longer_than_the_duration_records_it_in_bucket_0() {
        let d = one_delta(
            &held("slow_down", 1_000, 100),
            &[attempt(1, "a", None, None, Some(10))],
            &prices(),
        );
        assert_eq!(d.duration_hist, sampled(0.0));
    }

    /// The fixed filters narrow the read in SQL: with a pool and an account named, the planner seeks the member
    /// index on both rather than walking every row of the range.
    #[test]
    fn the_selecting_read_binds_the_pool_and_account_filters() {
        let select = RollupSelect {
            pools: vec!["p".into()],
            accounts: vec!["a".into(), "b".into()],
            ..RollupSelect::default()
        };
        let (sql, binds) = select_statement(0, 3_600, &select);
        let plan = crate::test_tables::plan_of(&sql, binds);
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH rollups_hourly")
                    && step.contains("pool_id=?")
                    && step.contains("account_id=?")),
            "{plan:?}"
        );
    }

    /// A pool alone binds no leading column the hour range would not serve better: the member index would walk the
    /// pool's whole retention before the hour filter, so the hour range stays the outer search.
    #[test]
    fn the_pool_only_read_searches_the_hour_range_off_the_member_index() {
        let select = RollupSelect {
            pools: vec!["p".into()],
            ..RollupSelect::default()
        };
        let (sql, binds) = select_statement(0, 3_600, &select);
        let plan = crate::test_tables::plan_of(&sql, binds);
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

    #[test]
    fn the_key_is_the_serving_account_and_the_reserved_id_stands_in_for_what_is_missing() {
        let r = RequestRecord {
            canonical_model: SONNET.into(),
            ..request("succeeded", Some(200), Some("k"))
        };
        let d = one_delta(
            &r,
            &[
                attempt(1, "a", None, None, None),
                attempt(2, "b", Some(5), None, Some(20)),
            ],
            &prices(),
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
            (3_600, "k", "p", "b", SONNET, "inference")
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
        // Both attempts answered under one band, speed and funding, so both are counted on the serving account's
        // row: ten input tokens each, and the second's cache read and output.
        assert_eq!(
            (
                d.input_tokens,
                d.cache_read_tokens,
                d.output_tokens,
                d.band.as_str(),
                d.tier.as_str()
            ),
            (20, 5, 20, STANDARD_BAND, UNATTRIBUTED)
        );
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
        let d = one_delta(&request("rejected", Some(401), None), &[], &prices());
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
        let d = one_delta(
            &request("failed", Some(429), Some("k")),
            &[AttemptRecord {
                previous_input_tokens: Some(1_000),
                ..attempt(1, "a", None, Some(3), None)
            }],
            &prices(),
        );
        assert_eq!((d.rate_limited, d.failed, d.cache_misses), (1, 0, 1));
        assert_eq!(
            (
                d.cache_miss_reasons.get("none"),
                d.cache_miss_reasons.get("expired_between_turns")
            ),
            (Some(&1), Some(&1)),
            "a none with no cache read after a previous request of 1,000 tokens expired between turns"
        );
        let d = one_delta(
            &request("succeeded", Some(200), Some("k")),
            &[AttemptRecord {
                cache_miss_reason: Some("pending".into()),
                ..attempt(1, "a", None, Some(3), None)
            }],
            &prices(),
        );
        assert_eq!(
            (
                d.cache_miss_reasons.get("pending"),
                d.cache_miss_reasons.get("none"),
                d.cache_miss_reasons.get("expired_between_turns")
            ),
            (Some(&1), None, None),
            "a comparison still running is counted under its own word, and with no previous size it expired nothing"
        );
        let d = one_delta(
            &request("canceled", Some(503), Some("k")),
            &[attempt(1, "a", None, None, None)],
            &prices(),
        );
        assert_eq!((d.rate_limited, d.failed), (0, 1));
    }

    /// The serving delta's expired count for a request whose one attempt read `read` back after a previous request of
    /// 1,000 tokens, under `reason`, the request of `kind`.
    fn expired_after(kind: &str, reason: Option<&str>, read: u64) -> Option<u64> {
        one_delta(
            &RequestRecord {
                kind: kind.into(),
                ..request("succeeded", Some(200), Some("k"))
            },
            &[AttemptRecord {
                cache_miss_reason: reason.map(String::from),
                previous_input_tokens: Some(1_000),
                ..attempt(1, "a", Some(read), None, Some(20))
            }],
            &prices(),
        )
        .cache_miss_reasons
        .get(EXPIRED_BETWEEN_TURNS)
        .copied()
    }

    #[test]
    fn a_turn_that_read_under_half_the_previous_size_expired_between_turns() {
        assert_eq!(
            (
                expired_after("inference", None, 499),
                expired_after("inference", None, 500)
            ),
            (Some(1), None)
        );
    }

    #[test]
    fn a_reason_that_names_a_divergence_leaves_a_turn_under_half_out_of_the_expired_count() {
        assert_eq!(
            expired_after("inference", Some("messages_changed"), 100),
            None
        );
    }

    #[test]
    fn a_probe_or_a_token_count_under_half_stays_out_of_the_expired_count() {
        assert_eq!(
            expired_after("count_tokens", None, 100),
            None,
            "a token count"
        );
        assert_eq!(expired_after("probe", None, 100), None, "a probe");
    }

    /// The `messages_changed/hit` count of a request whose one attempt sent `input` uncached tokens, wrote `write` and
    /// read `read` back under the reason `messages_changed`.
    fn hit_under_reason(input: u64, write: u64, read: u64) -> Option<u64> {
        one_delta(
            &request("succeeded", Some(200), Some("k")),
            &[AttemptRecord {
                input_tokens: Some(input),
                cache_miss_reason: Some("messages_changed".into()),
                ..attempt(1, "a", Some(read), Some(write), Some(20))
            }],
            &prices(),
        )
        .cache_miss_reasons
        .get("messages_changed/hit")
        .copied()
    }

    /// 900 of 1,000 prompt tokens read is 90 %, 899 of 1,000 is not, and an empty prompt reads nothing.
    #[test]
    fn a_reasons_hit_count_takes_a_read_of_90_percent_of_the_prompt_writes_included() {
        assert_eq!(
            (
                hit_under_reason(50, 50, 900),
                hit_under_reason(51, 50, 899),
                hit_under_reason(0, 0, 0)
            ),
            (Some(1), None, None)
        );
    }

    #[test]
    fn an_unmarked_request_that_read_and_wrote_nothing_counts_as_uncacheable_and_a_marked_or_a_hit_does_not(
    ) {
        let uncacheable = |cache_marker: bool, read: Option<u64>| {
            one_delta(
                &RequestRecord {
                    cache_marker: Some(cache_marker),
                    ..request("succeeded", Some(200), Some("k"))
                },
                &[attempt(1, "a", read, None, Some(20))],
                &prices(),
            )
            .uncacheable
        };
        assert_eq!(
            (
                uncacheable(false, None),
                uncacheable(true, None),
                uncacheable(false, Some(5))
            ),
            (1, 0, 0)
        );
    }

    /// The serving delta's dropped-blocks map for a request whose one attempt holds `member` and the trace `selection`.
    fn transformations_of(
        member: Option<BTreeMap<String, InputTransformation>>,
        selection: Option<&str>,
    ) -> BTreeMap<String, u64> {
        one_delta(
            &request("succeeded", Some(200), Some("k")),
            &[AttemptRecord {
                input_transformations: member,
                selection: selection.map(String::from),
                ..attempt(1, "a", None, None, Some(10))
            }],
            &prices(),
        )
        .input_transformations
    }

    /// `blocks` dropped blocks under `key` alone, the first at message 0.
    fn dropped(key: &str, blocks: u64) -> Option<BTreeMap<String, InputTransformation>> {
        Some(BTreeMap::from([(
            key.to_string(),
            InputTransformation {
                blocks,
                first_message: 0,
            },
        )]))
    }

    const MOVED_BY_SHED: &str =
        r#"{"strategy":"pace","tier":[0,1,0],"renewed":false,"moved":"shed","skipped":[]}"#;
    const NOT_MOVED: &str =
        r#"{"strategy":"pace","tier":[0,1,0],"renewed":false,"moved":null,"skipped":[]}"#;

    /// The Analytics card's bars count requests, so an answer that dropped 154 blocks under one key counts its request
    /// once under that key.
    #[test]
    fn a_request_counts_once_under_a_key_its_answer_dropped_blocks_under_whatever_the_blocks() {
        assert_eq!(
            transformations_of(dropped("thinking.prefix_binding_mismatch", 154), None),
            BTreeMap::from([("thinking.prefix_binding_mismatch".to_string(), 1)])
        );
    }

    /// A move counts where its answer dropped a block under an account reason, never for the prefix key alone, and a
    /// request that did not move counts no move whatever its answer dropped.
    #[test]
    fn a_moved_request_counts_as_moved_only_where_its_answer_dropped_a_block_under_an_account_reason(
    ) {
        let moved = |key: &str, selection: &str| {
            transformations_of(dropped(key, 3), Some(selection))
                .get(TRANSFORMATIONS_MOVED)
                .copied()
        };
        assert_eq!(
            moved("thinking.prefix_binding_mismatch", MOVED_BY_SHED),
            None,
            "a moved request that lists a prefix key alone"
        );
        assert_eq!(
            moved("thinking.organization_binding_mismatch", MOVED_BY_SHED),
            Some(1),
            "a moved request that lists a key under an account reason"
        );
        assert_eq!(
            moved("thinking.organization_binding_mismatch", NOT_MOVED),
            None,
            "a request that did not move, whatever its answer dropped"
        );
    }

    /// An answer that listed no block counts under the empty count, and an answer without the member counts nowhere.
    #[test]
    fn an_empty_member_counts_under_none_and_a_missing_member_counts_nowhere() {
        assert_eq!(
            transformations_of(Some(BTreeMap::new()), None),
            BTreeMap::from([(TRANSFORMATIONS_NONE.to_string(), 1)]),
            "an answer that listed nothing"
        );
        assert_eq!(
            transformations_of(None, None),
            BTreeMap::new(),
            "an answer without the member"
        );
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

    /// Both event builders name account `a`, so each hour holds one row.
    fn folded(events: &[EventRecord]) -> u64 {
        folded_by_hour(events).values().sum()
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
    fn a_cooldown_cleared_seconds_after_its_set_leaves_no_empty_hour_and_keeps_an_hour_with_a_pin()
    {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        // The pin folds before the clear, so the take-back meets an hour that holds more than its cooldown.
        let events = [
            cooldown_set_event(0, 3 * HOUR),
            EventRecord {
                at: HOUR + 60_000,
                kind: PIN_CREATED.into(),
                account_id: Some("a".into()),
                ..Default::default()
            },
            cooldown_cleared_event(14_000),
        ];
        db.write(|c| {
            let tx = c.transaction()?;
            for event in &events {
                fold_event(&tx, event)?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db
            .read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
            .unwrap();
        let hour = |hour: i64| {
            rows.iter()
                .find(|r| r.hour == hour)
                .map(|r| (r.cooldown_seconds, r.pins_created))
        };
        assert_eq!(
            hour(7_200),
            None,
            "an hour that held only the time the clear gave back leaves"
        );
        assert_eq!(
            hour(3_600),
            Some((0, 1)),
            "an hour that also holds a created pin keeps its row with the pin"
        );
    }

    #[test]
    fn a_failover_is_a_move_between_accounts_not_every_extra_attempt() {
        let two = |first: &str, second: &str| {
            one_delta(
                &request("succeeded", Some(200), Some("k")),
                &[
                    attempt(1, first, None, None, None),
                    attempt(2, second, None, None, None),
                ],
                &prices(),
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
        // The replaced request is a client's cancel beside a success, its answer dropped a block, and its body marked no
        // cache it read or wrote, so its counts must leave with it.
        let (req, attempts) = (
            RequestRecord {
                canonical_model: SONNET.into(),
                error_code: Some("client_gone".into()),
                cache_marker: Some(false),
                ..request("canceled", Some(200), Some("k"))
            },
            vec![AttemptRecord {
                thinking_tokens: Some(25),
                web_search_requests: Some(2),
                web_fetch_requests: Some(1),
                input_transformations: dropped("thinking.organization_binding_mismatch", 2),
                ..attempt(1, "a", None, None, Some(20))
            }],
        );
        let (req2, attempts2) = (
            RequestRecord {
                id: "r2".into(),
                canonical_model: SONNET.into(),
                ..request("succeeded", Some(200), Some("k"))
            },
            vec![AttemptRecord {
                thinking_tokens: Some(25),
                web_search_requests: Some(2),
                web_fetch_requests: Some(1),
                ..attempt(1, "a", Some(5), None, Some(20))
            }],
        );
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &req, &attempts, None, &prices())?;
            fold_request(&tx, &req2, &attempts2, None, &prices())?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db
            .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
            .unwrap();
        assert_eq!(rows.len(), 1);
        let mut expected = one_delta(&req, &attempts, &prices());
        expected.add(&one_delta(&req2, &attempts2, &prices()));
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
        // The first request is written again with more output and a cache write: its earlier split goes, the new
        // one lands whole, never mixed with what it replaced.
        let more = vec![AttemptRecord {
            thinking_tokens: Some(30),
            web_search_requests: Some(1),
            web_fetch_requests: Some(0),
            input_transformations: Some(BTreeMap::new()),
            ..attempt(1, "a", Some(5), Some(100), Some(50))
        }];
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &req, &more, Some((&req, &attempts)), &prices())?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db
            .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
            .unwrap();
        let mut expected = one_delta(&req2, &attempts2, &prices());
        expected.add(&one_delta(&req, &more, &prices()));
        assert_eq!(
            rows[0], expected,
            "the replacement stands in for what it took back"
        );
        assert_eq!(
            (rows[0].requests, rows[0].output_tokens),
            (2, 70),
            "still two requests, and the output is 20 + 50"
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
        assert_eq!(
            rows[0].output_tps_hist, expected.output_tps_hist,
            "the old sample left the histogram"
        );
        // The second request's counts and the replacement's: ten input tokens and five cache reads each, the
        // replacement's hundred cache writes, and 20 + 50 output tokens.
        assert_eq!(
            (
                rows[0].input_tokens,
                rows[0].cache_read_tokens,
                rows[0].cache_write_5m_tokens
            ),
            (20, 10, 100),
            "the replaced request's counts land whole, unblended with what it replaced"
        );
        // A request whose two answered attempts fall in two bands, replaced by a version its serving account moved
        // under: both of its band rows go back to zero and leave, and the new row stands under the new account.
        let two_bands = RequestRecord {
            id: "r3".into(),
            canonical_model: HAIKU.into(),
            ..request("succeeded", Some(200), Some("k"))
        };
        let over = haiku_over(two_bands.received_at);
        let answered = vec![prompted(1, "c", over + 1), prompted(2, "c", over)];
        let moved = vec![prompted(1, "d", over)];
        let haiku_rows = |account: &str| -> Vec<(String, u64, u64)> {
            db.read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
                .unwrap()
                .into_iter()
                .filter(|r| r.canonical_model == HAIKU && r.account_id == account)
                .map(|r| (r.band, r.attempts, r.input_tokens))
                .collect()
        };
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &two_bands, &answered, None, &prices())?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            haiku_rows("c"),
            [
                (LONG_BAND.to_string(), 1, over + 1),
                (STANDARD_BAND.to_string(), 1, over)
            ],
            "one row per band before the replace"
        );
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(
                &tx,
                &two_bands,
                &moved,
                Some((&two_bands, &answered)),
                &prices(),
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            haiku_rows("c"),
            [],
            "both band rows went back to zero and left"
        );
        assert_eq!(
            haiku_rows("d"),
            [(STANDARD_BAND.to_string(), 1, over)],
            "the replacement files under its own serving account"
        );
    }

    /// An attempt whose answer named no cache counts reads its prompt as its input alone, so the input decides which
    /// side of the threshold its row falls on.
    #[test]
    fn an_attempts_missing_counts_read_as_zero_in_its_prompt_length() {
        let r = RequestRecord {
            canonical_model: HAIKU.into(),
            ..request("succeeded", Some(200), Some("k"))
        };
        let band = |input: u64| {
            let a = AttemptRecord {
                input_tokens: Some(input),
                cache_read_tokens: None,
                cache_write_5m_tokens: None,
                cache_write_1h_tokens: None,
                ..attempt(1, "a", None, None, None)
            };
            one_delta(&r, &[a], &prices()).band
        };
        let over = haiku_over(r.received_at);
        assert_eq!(
            (band(over), band(over + 1)),
            (STANDARD_BAND.to_string(), LONG_BAND.to_string())
        );
    }

    /// The fold reads a request's band at the threshold in force when it arrived, whichever revision is the latest:
    /// a prompt between the two entries' thresholds files under `long` before the second entry and `standard` from
    /// its instant on.
    #[test]
    fn the_fold_reads_a_requests_band_at_the_revision_in_force_when_it_arrived() {
        let february = crate::effective_instant("2026-02-01").expect("a date");
        let (january_over, february_over) = (haiku_over(february - 1), haiku_over(february));
        assert!(
            january_over < february_over,
            "the second entry raises the threshold"
        );
        let band = |received_at: i64| {
            let r = RequestRecord {
                received_at,
                canonical_model: HAIKU.into(),
                ..request("succeeded", Some(200), Some("k"))
            };
            one_delta(&r, &[prompted(1, "a", january_over + 1)], &prices()).band
        };
        assert_eq!(
            (band(february - 1), band(february)),
            (LONG_BAND.to_string(), STANDARD_BAND.to_string())
        );
    }

    /// Two answered attempts on either side of the threshold fold into one delta per band, each with its own
    /// attempt's counts and an attempt count of one, the request counters and the errors on the serving (last)
    /// attempt's delta alone and the serving attempt's tier on both. A request no attempt answered files one delta
    /// under the standard band.
    #[test]
    fn a_request_whose_answered_attempts_fall_in_two_bands_folds_one_delta_per_band() {
        let r = RequestRecord {
            canonical_model: HAIKU.into(),
            ..request("incomplete", Some(200), Some("k"))
        };
        let over = haiku_over(r.received_at);
        let deltas = deltas_of(
            &r,
            &[
                AttemptRecord {
                    tier: Some("default_claude_max_5x".into()),
                    ..prompted(1, "a", over + 1)
                },
                AttemptRecord {
                    tier: Some("default_claude_max_20x".into()),
                    ..prompted(2, "a", over)
                },
            ],
            &prices(),
            FoldRules::ALL,
        );
        let seen: Vec<_> = deltas
            .iter()
            .map(|d| {
                (
                    d.band.as_str(),
                    d.tier.as_str(),
                    d.input_tokens,
                    d.output_tokens,
                    d.attempts,
                    (d.requests, d.incomplete, d.errors.len()),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    LONG_BAND,
                    "default_claude_max_20x",
                    over + 1,
                    10,
                    1,
                    (0, 0, 0)
                ),
                (
                    STANDARD_BAND,
                    "default_claude_max_20x",
                    over,
                    10,
                    1,
                    (1, 1, 1)
                ),
            ]
        );
        let none = one_delta(
            &request("failed", Some(529), Some("k")),
            &[AttemptRecord {
                input_tokens: None,
                ..attempt(1, "a", None, None, None)
            }],
            &prices(),
        );
        assert_eq!(
            (none.band.as_str(), none.requests, none.failed),
            (STANDARD_BAND, 1, 1)
        );
    }

    /// An attempt without usage, a 429 before the answer, adds no count to any delta: the request's one delta holds
    /// the answered attempt's counts alone. Its attempt still counts beside the request's other counters, so a
    /// request's attempts sum to every attempt it made.
    #[test]
    fn an_attempt_without_usage_adds_no_counts_to_any_delta() {
        let d = one_delta(
            &RequestRecord {
                canonical_model: SONNET.into(),
                ..request("succeeded", Some(200), Some("k"))
            },
            &[unanswered(1, "a"), attempt(2, "b", Some(5), None, Some(20))],
            &prices(),
        );
        assert_eq!(
            (
                d.input_tokens,
                d.cache_read_tokens,
                d.output_tokens,
                d.thinking_tokens
            ),
            (10, 5, 20, 0),
            "the answered attempt's counts alone"
        );
        assert_eq!(
            (d.account_id.as_str(), d.attempts, d.failovers, d.requests),
            ("b", 2, 1, 1)
        );
    }

    /// A serving attempt without usage, a request that failed before an answer after an earlier attempt answered,
    /// files the request counters, the failover, the cache figures, the timings and the errors on a delta of their
    /// own under the standard band, keyed by the serving attempt's speed and funding, with no count in it.
    #[test]
    fn a_serving_attempt_without_usage_files_the_request_counters_under_standard_with_no_counts() {
        let r = RequestRecord {
            canonical_model: HAIKU.into(),
            ..request("failed", Some(529), Some("k"))
        };
        let over = haiku_over(r.received_at);
        let deltas = deltas_of(
            &r,
            &[prompted(1, "a", over + 1), unanswered(2, "b")],
            &prices(),
            FoldRules::ALL,
        );
        let seen: Vec<_> = deltas
            .iter()
            .map(|d| {
                (
                    (d.band.as_str(), d.speed.as_str(), d.funding.as_str()),
                    (d.input_tokens, d.output_tokens, d.thinking_tokens),
                    (d.attempts, d.requests, d.failed, d.failovers),
                    (d.uncached_requests, d.duration_hist.count(), d.errors.len()),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    (LONG_BAND, STANDARD_SPEED, PLAN_FUNDING),
                    (over + 1, 10, 0),
                    (1, 0, 0, 0),
                    (0, 0, 0)
                ),
                (
                    (STANDARD_BAND, STANDARD_SPEED, PLAN_FUNDING),
                    (0, 0, 0),
                    (1, 1, 1, 1),
                    (1, 1, 1)
                ),
            ]
        );
    }

    /// An answer that reported no speed was served standard whatever the request asked for, so its row files and
    /// prices at the standard cells, as the session burn prices the same attempt.
    #[test]
    fn an_answer_that_reported_no_speed_folds_and_prices_at_the_standard_cells_on_a_fast_request() {
        const OPUS: &str = "claude-opus-5-5";
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let r = RequestRecord {
            canonical_model: OPUS.into(),
            requested_speed: Some("fast".into()),
            ..request("succeeded", Some(200), Some("k"))
        };
        let answered = AttemptRecord {
            input_tokens: Some(1_000_000),
            speed: None,
            account_kind: AccountKind::Oauth,
            ..attempt(1, "a", None, None, None)
        };
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &r, std::slice::from_ref(&answered), None, &prices())?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows = db
            .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.speed.as_str(), row.funding.as_str(), row.input_tokens))
                .collect::<Vec<_>>(),
            [(STANDARD_SPEED, PLAN_FUNDING, 1_000_000)],
            "the answer's own speed keys the row"
        );
        let row = &rows[0];
        let costs = crate::test_tables::subscription_costs();
        let usd = crate::query::priced(row, &prices(), &costs, row.hour * 1_000)
            .usd
            .expect("the test list carries the model")
            .total();
        let standard = prices().in_force(r.received_at).models[OPUS].standard.input;
        let attempt_usd = prices()
            .in_force(r.received_at)
            .estimate(
                OPUS,
                Speed::of(answered.speed.as_deref()),
                PinUsage {
                    input: 1_000_000,
                    cache_read: 0,
                    write_5m: 0,
                    write_1h: 0,
                    output: 0,
                },
            )
            .expect("the test list carries the model")
            .total();
        assert!(
            (usd - standard).abs() < 1e-9 && (usd - attempt_usd).abs() < 1e-9,
            "{usd} against the standard input cell {standard} and the attempt's own price {attempt_usd}"
        );
    }

    /// A fast retry beside a standard answer folds into two deltas, each at its own speed and funding, so each
    /// prices at its own cells once the row carries no dollar: a million input tokens at the fast cell and a million
    /// at the standard one.
    #[test]
    fn a_fast_attempt_beside_a_standard_one_folds_into_two_deltas_priced_at_their_own_cells() {
        const OPUS: &str = "claude-opus-5-5";
        let r = RequestRecord {
            canonical_model: OPUS.into(),
            ..request("succeeded", Some(200), Some("k"))
        };
        let million = |n: u32, speed: &str| AttemptRecord {
            input_tokens: Some(1_000_000),
            speed: Some(speed.into()),
            account_kind: AccountKind::Oauth,
            ..attempt(n, "a", None, None, None)
        };
        let deltas = deltas_of(
            &r,
            &[million(1, "fast"), million(2, STANDARD_SPEED)],
            &prices(),
            FoldRules::ALL,
        );
        let seen: Vec<_> = deltas
            .iter()
            .map(|d| {
                (
                    (d.band.as_str(), d.speed.as_str(), d.funding.as_str()),
                    (d.input_tokens, d.attempts, d.requests),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ((STANDARD_BAND, "fast", FAST_FUNDING), (1_000_000, 1, 0)),
                (
                    (STANDARD_BAND, STANDARD_SPEED, PLAN_FUNDING),
                    (1_000_000, 1, 1)
                ),
            ]
        );
        let costs = crate::test_tables::subscription_costs();
        let priced: f64 = deltas
            .iter()
            .map(|d| {
                crate::query::priced(d, &prices(), &costs, d.hour * 1_000)
                    .usd
                    .expect("the test list carries the model")
                    .total()
            })
            .sum();
        let list = prices();
        let row = &list.in_force(r.received_at).models[OPUS];
        let fast = row
            .fast
            .as_ref()
            .expect("the test list's Opus 5.5 serves fast");
        assert!(
            (priced - (fast.input + row.standard.input)).abs() < 1e-9,
            "{priced} against the fast and the standard input cells over a million each"
        );
    }

    /// Two requests whose earlier attempts both fall under `long` and whose serving attempts fall under `standard`
    /// share the `long` row, which no request counter backs. Replacing one with a version without its earlier
    /// attempt takes its counts out of the shared row and leaves the other request's there, while its own standard
    /// row moves as any replace moves it.
    #[test]
    fn a_replaced_request_leaves_the_counts_another_request_holds_in_the_band_row_they_share() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let first = RequestRecord {
            canonical_model: HAIKU.into(),
            ..request("succeeded", Some(200), Some("k"))
        };
        let second = RequestRecord {
            id: "r2".into(),
            ..first.clone()
        };
        let over = haiku_over(first.received_at);
        let both = vec![prompted(1, "a", over + 1), prompted(2, "a", over)];
        let serving_alone = vec![prompted(1, "a", over)];
        db.write(|c| {
            let tx = c.transaction()?;
            fold_request(&tx, &first, &both, None, &prices())?;
            fold_request(&tx, &second, &both, None, &prices())?;
            fold_request(
                &tx,
                &first,
                &serving_alone,
                Some((&first, &both)),
                &prices(),
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows: Vec<_> = db
            .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
            .unwrap()
            .into_iter()
            .map(|r| (r.band, r.attempts, r.requests, r.input_tokens))
            .collect();
        assert_eq!(
            rows,
            [
                (LONG_BAND.to_string(), 1, 0, over + 1),
                (STANDARD_BAND.to_string(), 2, 2, 2 * over),
            ],
            "the shared long row keeps the second request's counts"
        );
    }

    /// A keepalive delta keys the probe's own hour, band and tier: a settlement carrying the wasted counts of a
    /// long-context probe files them under `long` at the probe's hour, and a delta with no band under `standard`.
    #[test]
    fn a_keepalive_delta_files_its_wasted_counts_under_its_own_hour_band_and_tier() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let settled = KeepaliveDelta {
            at: 7_300_000,
            key_id: "k".into(),
            pool_id: "p".into(),
            account_id: "a".into(),
            canonical_model: HAIKU.into(),
            kind: "probe".into(),
            wasted_probes: 1,
            wasted_input_tokens: 1,
            wasted_cache_read_tokens: 2,
            wasted_cache_write_5m_tokens: 3,
            wasted_cache_write_1h_tokens: 4,
            wasted_output_tokens: 5,
            tier: Some("default_claude_max_20x".into()),
            band: Band::LongContext,
            ..Default::default()
        };
        let skipped = KeepaliveDelta {
            probes_skipped: 1,
            tier: None,
            band: Band::Standard,
            ..settled.clone()
        };
        db.write(|c| {
            let tx = c.transaction()?;
            fold_keepalive(&tx, &settled)?;
            fold_keepalive(
                &tx,
                &KeepaliveDelta {
                    wasted_probes: 0,
                    wasted_input_tokens: 0,
                    wasted_cache_read_tokens: 0,
                    wasted_cache_write_5m_tokens: 0,
                    wasted_cache_write_1h_tokens: 0,
                    wasted_output_tokens: 0,
                    ..skipped
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let rows: Vec<_> = db
            .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
            .unwrap()
            .into_iter()
            .map(|r| {
                (
                    (r.hour, r.band.clone(), r.tier.clone()),
                    (r.wasted_probes, r.probes_skipped),
                    r.wasted_usage(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    (
                        hour_of(7_300_000),
                        LONG_BAND.to_string(),
                        "default_claude_max_20x".to_string()
                    ),
                    (1, 0),
                    PinUsage {
                        input: 1,
                        cache_read: 2,
                        write_5m: 3,
                        write_1h: 4,
                        output: 5
                    }
                ),
                (
                    (
                        hour_of(7_300_000),
                        STANDARD_BAND.to_string(),
                        UNATTRIBUTED.to_string()
                    ),
                    (0, 1),
                    PinUsage::default()
                ),
            ]
        );
    }

    /// A credit keys its request's client folded as the request row folds it, a label past the bound under
    /// `OTHER_DIMENSION`, and a delta whose client is empty, as a default-built one leaves it, keys the reserved id an
    /// unlabeled request folds under rather than an empty label of its own.
    #[test]
    fn a_keepalive_deltas_client_folds_as_a_request_rows_and_an_empty_one_under_the_reserved_id() {
        let db = crate::LedgerDb::open_in_memory().unwrap();
        let long = format!("claude-code/{}", "x".repeat(DIMENSION_MAX_BYTES));
        let credit = |client: &str| KeepaliveDelta {
            at: 7_300_000,
            key_id: "k".into(),
            pool_id: "p".into(),
            account_id: "a".into(),
            canonical_model: HAIKU.into(),
            kind: "inference".into(),
            client: client.into(),
            kept_warm_requests: 1,
            ..Default::default()
        };
        db.write(|c| {
            let tx = c.transaction()?;
            for client in ["", "claude-code/cli", &long] {
                fold_keepalive(&tx, &credit(client))?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
        let mut clients: Vec<_> = db
            .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
            .unwrap()
            .into_iter()
            .map(|r| (r.client, r.kept_warm_requests))
            .collect();
        clients.sort();
        assert_eq!(
            clients,
            [
                (UNATTRIBUTED.to_string(), 1),
                ("claude-code/cli".to_string(), 1),
                (OTHER_DIMENSION.to_string(), 1),
            ]
        );
    }

    /// A row holds something while any count the fold adds is above zero, a wasted count, a kept-warm count or an
    /// attempt's tokens alone among them, and nothing once every one is zero.
    #[test]
    fn a_row_holds_nothing_only_when_every_count_the_fold_adds_is_zero() {
        let zeros = RollupRow::key(
            3_600,
            "k",
            0,
            "p",
            "a",
            "m",
            "inference",
            "-",
            "-",
            "-",
            "-",
            "standard",
            "plan",
            "long",
            "-",
            "-",
        );
        assert!(zeros.holds_nothing());
        let with = |set: fn(&mut RollupRow)| {
            let mut row = zeros.clone();
            set(&mut row);
            row.holds_nothing()
        };
        assert!(!with(|r| r.wasted_cache_read_tokens = 1), "a wasted count");
        assert!(
            !with(|r| r.kept_warm_5m_tokens = 1),
            "a five-minute kept-warm count"
        );
        assert!(
            !with(|r| r.kept_warm_1h_tokens = 1),
            "an hour's kept-warm count"
        );
        assert!(
            !with(|r| {
                r.attempts = 1;
                r.input_tokens = 10;
            }),
            "an attempt's tokens"
        );
        assert!(!with(|r| r.output_tokens = 1), "a token count alone");
        assert!(
            !with(|r| {
                r.refusals.insert("guardrail_blocked".into(), 1);
            }),
            "a refusal alone"
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
            "claude-code/cli",
            "high",
            "subagent",
            "fast",
            "overage",
            "long",
            "default_claude_max_20x",
            "-",
        );
        r.requests = 10;
        r.succeeded = 11;
        r.rate_limited = 12;
        r.failed = 13;
        r.client_canceled = 58;
        r.exhausted = 14;
        r.incomplete = 48;
        r.rejected = 15;
        r.attempts = 16;
        r.failovers = 17;
        r.cache_hits = 18;
        r.cache_misses = 19;
        r.uncached_requests = 20;
        r.uncacheable = 61;
        r.input_tokens = 21;
        r.cache_read_tokens = 22;
        r.cache_write_5m_tokens = 23;
        r.cache_write_1h_tokens = 24;
        r.output_tokens = 40;
        r.thinking_tokens = 25;
        r.web_search_requests = 2;
        r.web_fetch_requests = 1;
        r.kept_warm_requests = 33;
        r.kept_warm_5m_tokens = 34;
        r.kept_warm_1h_tokens = 35;
        r.probe_migrated = 37;
        r.wasted_probes = 38;
        r.wasted_input_tokens = 50;
        r.wasted_cache_read_tokens = 51;
        r.wasted_cache_write_5m_tokens = 52;
        r.wasted_cache_write_1h_tokens = 53;
        r.wasted_output_tokens = 54;
        r.wasted_probe_reasons = BTreeMap::from([("idle_limit".to_string(), 40)]);
        r.probes_skipped = 41;
        r.probes_held = 49;
        r.slow_down_held_ms = 55;
        r.slow_down_held = 56;
        r.ttfb_hist.record(42.0);
        r.ttfc_hist.record(43.0);
        r.ttfc_hist.record(43.0);
        r.duration_hist.record(44.0);
        r.duration_hist.record(44.0);
        r.duration_hist.record(44.0);
        r.output_tps_hist.record(45.0);
        r.errors = BTreeMap::from([("overloaded".to_string(), 46)]);
        r.refusals = BTreeMap::from([("guardrail_blocked".to_string(), 57)]);
        r.cache_miss_reasons = BTreeMap::from([("none".to_string(), 47)]);
        r.input_transformations = BTreeMap::from([
            ("thinking.organization_binding_mismatch".to_string(), 59),
            (TRANSFORMATIONS_MOVED.to_string(), 60),
        ]);
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
                read.band.as_str(),
                read.tier.as_str(),
                read.wasted_usage(),
                read.cache_miss_reasons["none"]
            ),
            (
                "high",
                LONG_BAND,
                "default_claude_max_20x",
                PinUsage {
                    input: 50,
                    cache_read: 51,
                    write_5m: 52,
                    write_1h: 53,
                    output: 54
                },
                47
            )
        );
    }
}
