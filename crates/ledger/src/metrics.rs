//! Prometheus text format, rendered by hand: a handful of counters and gauges with bounded label sets, and
//! one histogram over the same 84 buckets the rollups use.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use phoebis_bus::{Bus, FeatureCall, FeatureEnd, SubscriberStats};
use phoebis_oauth::Clock;
use phoebis_routing::{RouterHandle, WriterStatus, FIVE_HOUR, SEVEN_DAY};
use phoebis_store::CooldownScope;

use crate::histogram::{bucket_edges, bucket_index, top_edge, BUCKET_COUNT};
use crate::pricing::{PriceTable, Pricing};
use crate::record::{AttemptRecord, QuotaSampleRecord, RequestRecord};
use crate::rollup::UNATTRIBUTED;
use crate::task::Ledger;

const LOCK_POISONED: &str = "metrics registry poisoned by a prior panic";

/// The feature word of the domain check, which acts for no feature of its own.
const DOMAIN_CHECK: &str = "domain_check";

/// The feature word of a refusal that matched no row. A 401 is one of them, since the key check runs ahead of the
/// table.
const NO_FEATURE: &str = "none";

type Labels = Vec<(&'static str, String)>;

/// One model's TTFB series: the 84 finite buckets, the observations past the last edge, and the sum in seconds.
/// The overflow is kept apart so `+Inf` can carry what no finite bound answers for.
struct Series {
    buckets: [u64; BUCKET_COUNT],
    overflow: u64,
    sum: f64,
}

impl Default for Series {
    fn default() -> Series {
        Series {
            buckets: [0; BUCKET_COUNT],
            overflow: 0,
            sum: 0.0,
        }
    }
}

#[derive(Default)]
struct Registry {
    counters: BTreeMap<Family, BTreeMap<Labels, f64>>,
    /// `phoebis_account_ratelimit_utilization` by account and window, in the order its labels render.
    utilization: BTreeMap<(String, String), f64>,
    /// The shared windows each account's newest clean poll carried, which bound what a head may set.
    polled_windows: BTreeMap<String, BTreeSet<&'static str>>,
    ttfb: BTreeMap<String, Series>,
}

impl Registry {
    /// Before the process's first clean poll of an account no set is kept, so a head sets both shared windows, as the
    /// router reads both until that poll.
    fn poll_lacks(&self, account: &str, window: &str) -> bool {
        if window != FIVE_HOUR && window != SEVEN_DAY {
            return false;
        }
        self.polled_windows
            .get(account)
            .is_some_and(|shared| !shared.contains(window))
    }
}

pub struct Metrics {
    inner: Mutex<Registry>,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics::new()
    }
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics {
            inner: Mutex::new(Registry::default()),
        }
    }

    fn add(&self, family: Family, labels: Labels, by: f64) {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        *inner
            .counters
            .entry(family)
            .or_default()
            .entry(labels)
            .or_default() += by;
    }

    pub fn observe_request(
        &self,
        table: &PriceTable,
        request: &RequestRecord,
        attempts: &[AttemptRecord],
    ) {
        let key = request
            .key_id
            .clone()
            .unwrap_or_else(|| UNATTRIBUTED.into());
        // A refused request asked no account and has no attempt row, so it counts under the refusals family alone
        // and the attempt loop below sees none of it.
        if request.refused {
            self.add(
                Family::RefusalsTotal,
                vec![
                    ("code", request.error_code.clone().unwrap_or_default()),
                    ("key", key),
                    ("kind", request.kind.clone()),
                ],
                1.0,
            );
            return;
        }
        self.add(
            Family::RequestsTotal,
            vec![
                ("key", key),
                ("kind", request.kind.clone()),
                ("outcome", request.outcome.clone()),
            ],
            1.0,
        );
        let model = table.model_label(&request.canonical_model).to_string();
        for a in attempts {
            self.add(
                Family::AttemptsTotal,
                vec![
                    ("account", a.account_id.clone()),
                    ("model", model.clone()),
                    ("outcome", a.outcome.clone()),
                ],
                1.0,
            );
            for (kind, tokens) in [
                ("cache_read", a.cache_read_tokens),
                ("cache_write_1h", a.cache_write_1h_tokens),
                ("cache_write_5m", a.cache_write_5m_tokens),
                ("input", a.input_tokens),
                ("output", a.output_tokens),
            ] {
                if let Some(n) = tokens {
                    self.add(
                        Family::TokensTotal,
                        vec![
                            ("account", a.account_id.clone()),
                            ("kind", kind.into()),
                            ("model", model.clone()),
                        ],
                        n as f64,
                    );
                }
            }
            if let Some(ttfb) = a.ttfb_ms {
                let mut inner = self.inner.lock().expect(LOCK_POISONED);
                let entry = inner.ttfb.entry(model.clone()).or_default();
                let value = ttfb as f64;
                if value > top_edge() {
                    entry.overflow += 1;
                } else {
                    entry.buckets[bucket_index(value)] += 1;
                }
                entry.sum += value / 1_000.0;
            }
        }
    }

    /// One feature endpoint request at its end: a call sent upstream, the domain check among them, or a refusal. No
    /// label holds a path, a status or an account, so the series stay bounded by the keys.
    pub fn observe_feature_call(&self, call: &FeatureCall) {
        let (feature, key, outcome) = match &call.end {
            FeatureEnd::Refused {
                code,
                feature,
                key_id,
            } => {
                let key = key_id.clone().unwrap_or_else(|| UNATTRIBUTED.into());
                let labels = vec![
                    ("code", (*code).into()),
                    ("feature", feature.unwrap_or(NO_FEATURE).into()),
                    ("key", key),
                ];
                self.add(Family::FeatureRefusalsTotal, labels, 1.0);
                return;
            }
            FeatureEnd::DomainCheck { outcome } => (DOMAIN_CHECK, UNATTRIBUTED, outcome),
            FeatureEnd::Sent {
                feature,
                key_id,
                outcome,
                ..
            } => (*feature, key_id.as_str(), outcome),
        };
        let labels = vec![
            ("feature", feature.into()),
            ("key", key.into()),
            ("outcome", outcome.as_str().into()),
        ];
        self.add(Family::FeatureCallsTotal, labels, 1.0);
    }

    /// Sets each sample's series and, under `poll`, removes the `phoebis_account_ratelimit_utilization` series of a
    /// shared window the account's samples lack where they carry at least one shared window, so a dropped window's
    /// series leaves with it. A head removes nothing, and it sets no shared window the account's newest clean poll
    /// lacked, so it cannot restore a series that poll removed.
    pub fn observe_samples(&self, quota: &[QuotaSampleRecord], poll: bool) {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        let mut carried: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for q in quota {
            if let Some(u) = q.utilization {
                if !poll && inner.poll_lacks(&q.account_id, &q.window) {
                    continue;
                }
                inner
                    .utilization
                    .insert((q.account_id.clone(), q.window.clone()), u);
                if poll {
                    carried.entry(&q.account_id).or_default().insert(&q.window);
                }
            }
        }
        if !poll {
            return;
        }
        // A poll that carried neither shared window is no reading of the account, so its series stand as the
        // router's windows do.
        for (account, windows) in carried {
            if !windows.contains(FIVE_HOUR) && !windows.contains(SEVEN_DAY) {
                continue;
            }
            let shared: BTreeSet<&'static str> = [FIVE_HOUR, SEVEN_DAY]
                .into_iter()
                .filter(|window| windows.contains(window))
                .collect();
            inner.polled_windows.insert(account.to_string(), shared);
            for window in [FIVE_HOUR, SEVEN_DAY] {
                if !windows.contains(window) {
                    inner
                        .utilization
                        .remove(&(account.to_string(), window.to_string()));
                }
            }
        }
    }

    pub fn observe_drop(&self, table: &'static str) {
        self.add(
            Family::LedgerDroppedTotal,
            vec![("table", table.into())],
            1.0,
        );
    }

    pub fn observe_probe(&self, pool: &str, outcome: &str) {
        self.add(
            Family::KeepaliveProbesTotal,
            vec![("outcome", outcome.into()), ("pool", pool.into())],
            1.0,
        );
    }

    pub fn observe_skipped(&self, pool: &str, state: &str) {
        self.add(
            Family::KeepaliveSkippedTotal,
            vec![("pool", pool.into()), ("state", state.into())],
            1.0,
        );
    }

    /// One contradiction between the keepalive engine's chain model and the upstream's cache, per client version.
    pub fn observe_chain_contradiction(&self, client_version: &str) {
        self.add(
            Family::KeepaliveChainContradictionsTotal,
            vec![("client_version", client_version.into())],
            1.0,
        );
    }

    pub fn render(
        &self,
        gauges: &RoutingGauges,
        bus: &[(&'static str, Arc<SubscriberStats>)],
        pricing_unknown: u64,
    ) -> String {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        let mut out = String::new();
        for family in Family::ALL.iter().copied() {
            let name = family.name();
            let _ = writeln!(out, "# TYPE {name} {}", family.kind());
            let line = |out: &mut String, value: u64| {
                let _ = writeln!(out, "{name} {value}");
            };
            match family {
                Family::AttemptsTotal
                | Family::FeatureCallsTotal
                | Family::FeatureRefusalsTotal
                | Family::KeepaliveChainContradictionsTotal
                | Family::KeepaliveProbesTotal
                | Family::KeepaliveSkippedTotal
                | Family::LedgerDroppedTotal
                | Family::RefusalsTotal
                | Family::RequestsTotal
                | Family::TokensTotal => {
                    if let Some(series) = inner.counters.get(&family) {
                        for (labels, value) in series {
                            let _ = writeln!(
                                out,
                                "{name}{} {}",
                                render_labels(labels),
                                render_value(*value)
                            );
                        }
                    }
                }
                Family::LedgerBehindRefusalsTotal => {
                    line(&mut out, gauges.ledger_behind_refusals);
                }
                Family::LedgerDegraded => {
                    line(&mut out, u64::from(gauges.ledger_degraded));
                }
                Family::LedgerParkedPricedTotal => {
                    line(&mut out, gauges.ledger_parked_priced);
                }
                Family::LedgerQueued => {
                    line(&mut out, gauges.ledger_queued);
                }
                Family::PricingUnknownModelTotal => {
                    line(&mut out, pricing_unknown);
                }
                Family::Attention => {
                    // The kind is one of the reader's own static names, so only the subject needs escaping.
                    for (kind, subject, holds) in &gauges.attention.conditions {
                        let _ = writeln!(
                            out,
                            "{name}{{kind=\"{kind}\",subject=\"{}\"}} {}",
                            escape(subject),
                            u8::from(*holds)
                        );
                    }
                }
                Family::BusDroppedTotal | Family::BusSubscriberPanicsTotal => {
                    for (subscriber, stats) in bus {
                        let counter = match family {
                            Family::BusDroppedTotal => &stats.dropped,
                            _ => &stats.panics,
                        };
                        let _ = writeln!(
                            out,
                            "{name}{{subscriber=\"{}\"}} {}",
                            escape(subscriber),
                            counter.load(std::sync::atomic::Ordering::Relaxed)
                        );
                    }
                }
                Family::RouterDroppedReportsTotal => {
                    for (kind, dropped) in &gauges.dropped_reports {
                        let _ = writeln!(out, "{name}{{kind=\"{kind}\"}} {dropped}");
                    }
                }
                Family::RouterWriterDegraded => {
                    line(&mut out, u64::from(gauges.writer.degraded));
                }
                Family::RouterWriterDepth => {
                    line(&mut out, gauges.writer.depth as u64);
                }
                Family::AccountCooldownSeconds => {
                    for (account, scope, seconds) in &gauges.cooldowns {
                        let _ = writeln!(
                            out,
                            "{name}{{account=\"{}\",scope=\"{}\"}} {seconds}",
                            escape(account),
                            escape(scope)
                        );
                    }
                }
                Family::AccountOverageInUse => {
                    for (account, in_use) in &gauges.overage {
                        let _ = writeln!(
                            out,
                            "{name}{{account=\"{}\"}} {}",
                            escape(account),
                            u8::from(*in_use)
                        );
                    }
                }
                Family::AccountRatelimitUtilization => {
                    // An account added later under a deleted id has had no clean poll and no reading of its own, so
                    // the deleted account's set must not bound its heads and its last readings must not scrape
                    // under it.
                    inner
                        .polled_windows
                        .retain(|account, _| gauges.live_accounts.contains(account));
                    inner
                        .utilization
                        .retain(|(account, _), _| gauges.live_accounts.contains(account));
                    for ((account, window), value) in &inner.utilization {
                        let _ = writeln!(
                            out,
                            "{name}{{account=\"{}\",window=\"{}\"}} {}",
                            escape(account),
                            escape(window),
                            render_value(*value)
                        );
                    }
                }
                Family::KeepaliveAgents => {
                    // Always the three series, so a scrape reads zero rather than absent when the engine
                    // retains nothing.
                    for (state, count) in [
                        ("busy", gauges.keepalive_agents.busy),
                        ("idle", gauges.keepalive_agents.idle),
                        ("parked", gauges.keepalive_agents.parked),
                    ] {
                        let _ = writeln!(out, "{name}{{state=\"{state}\"}} {count}");
                    }
                }
                Family::PinsActive => {
                    for (pool, count) in &gauges.pins_by_pool {
                        let _ = writeln!(out, "{name}{{pool=\"{}\"}} {count}", escape(pool));
                    }
                }
                Family::UpstreamTtfbSeconds => {
                    let edges = bucket_edges();
                    let bounds: Vec<String> = edges[1..]
                        .iter()
                        .map(|e| render_value(e / 1_000.0))
                        .collect();
                    for (model, series) in &inner.ttfb {
                        let model = escape(model);
                        let mut cumulative = 0u64;
                        for (i, n) in series.buckets.iter().enumerate() {
                            cumulative += n;
                            let _ = writeln!(
                                out,
                                "{name}_bucket{{model=\"{model}\",le=\"{}\"}} {cumulative}",
                                bounds[i]
                            );
                        }
                        let total = cumulative + series.overflow;
                        // The only bound an observation past the last edge is under.
                        let _ = writeln!(
                            out,
                            "{name}_bucket{{model=\"{model}\",le=\"+Inf\"}} {total}"
                        );
                        let _ = writeln!(
                            out,
                            "{name}_sum{{model=\"{model}\"}} {}",
                            render_value(series.sum)
                        );
                        let _ = writeln!(out, "{name}_count{{model=\"{model}\"}} {total}");
                    }
                }
            }
        }
        out
    }
}

fn render_labels(labels: &Labels) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&(&'static str, String)> = labels.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    format!(
        "{{{}}}",
        sorted
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn render_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

// One invocation declares each family's variant, name and type, so a counter files its series under a variant the
// renderer's exhaustive match reads, and the table the test walks lists the same families.
macro_rules! metric_families {
    ($($variant:ident => ($name:literal, $kind:literal)),* $(,)?) => {
        /// A metric family, declared in name order, the order the scrape renders the families in.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        enum Family {
            $($variant),*
        }

        impl Family {
            const ALL: &'static [Family] = &[$(Family::$variant),*];

            fn name(self) -> &'static str {
                match self {
                    $(Family::$variant => $name),*
                }
            }

            fn kind(self) -> &'static str {
                match self {
                    $(Family::$variant => $kind),*
                }
            }
        }

        /// Every family this renderer emits, in name order, with its type: the test walks it against the scrape's
        /// TYPE lines.
        pub const FAMILIES: &[(&str, &str)] = &[$(($name, $kind)),*];
    };
}

metric_families! {
    AccountCooldownSeconds => ("phoebis_account_cooldown_seconds", "gauge"),
    AccountOverageInUse => ("phoebis_account_overage_in_use", "gauge"),
    AccountRatelimitUtilization => ("phoebis_account_ratelimit_utilization", "gauge"),
    AttemptsTotal => ("phoebis_attempts_total", "counter"),
    Attention => ("phoebis_attention", "gauge"),
    BusDroppedTotal => ("phoebis_bus_dropped_total", "counter"),
    BusSubscriberPanicsTotal => ("phoebis_bus_subscriber_panics_total", "counter"),
    FeatureCallsTotal => ("phoebis_feature_calls_total", "counter"),
    FeatureRefusalsTotal => ("phoebis_feature_refusals_total", "counter"),
    KeepaliveAgents => ("phoebis_keepalive_agents", "gauge"),
    KeepaliveChainContradictionsTotal => ("phoebis_keepalive_chain_contradictions_total", "counter"),
    KeepaliveProbesTotal => ("phoebis_keepalive_probes_total", "counter"),
    KeepaliveSkippedTotal => ("phoebis_keepalive_skipped_total", "counter"),
    LedgerBehindRefusalsTotal => ("phoebis_ledger_behind_refusals_total", "counter"),
    LedgerDegraded => ("phoebis_ledger_degraded", "gauge"),
    LedgerDroppedTotal => ("phoebis_ledger_dropped_total", "counter"),
    LedgerParkedPricedTotal => ("phoebis_ledger_parked_priced_total", "counter"),
    LedgerQueued => ("phoebis_ledger_queued", "gauge"),
    PinsActive => ("phoebis_pins_active", "gauge"),
    PricingUnknownModelTotal => ("phoebis_pricing_unknown_model_total", "counter"),
    RefusalsTotal => ("phoebis_refusals_total", "counter"),
    RequestsTotal => ("phoebis_requests_total", "counter"),
    RouterDroppedReportsTotal => ("phoebis_router_dropped_reports_total", "counter"),
    RouterWriterDegraded => ("phoebis_router_writer_degraded", "gauge"),
    RouterWriterDepth => ("phoebis_router_writer_depth", "gauge"),
    TokensTotal => ("phoebis_tokens_total", "counter"),
    UpstreamTtfbSeconds => ("phoebis_upstream_ttfb_seconds", "histogram"),
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct KeepaliveAgents {
    pub busy: u64,
    pub idle: u64,
    pub parked: u64,
}

/// A closure over the keepalive engine's own lock, read fresh on every scrape rather than pushed: the ledger
/// crate has no dependency on the keepalive crate, so the binary supplies this instead of a type it can name.
pub type KeepaliveGaugeSource = Arc<dyn Fn() -> KeepaliveAgents + Send + Sync>;

/// One series per condition the attention reader answers: its kind, its subject, and whether it holds.
#[derive(Clone, Debug, Default)]
pub struct AttentionGauges {
    pub conditions: Vec<(&'static str, String, bool)>,
}

/// A closure over the attention reader, read fresh on every scrape: the attention crate depends on this one for these
/// types, so the binary supplies the closure rather than a type the ledger could name, and the reader's error crosses
/// boxed for the same reason. It answers a future, so the read waits for its ledger connection on the runtime, where a
/// wait holds no blocking worker.
pub type AttentionGaugeSource = Arc<
    dyn Fn() -> Pin<
            Box<
                dyn Future<
                        Output = Result<AttentionGauges, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

#[derive(Clone, Debug, Default)]
pub struct RoutingGauges {
    pub cooldowns: Vec<(String, String, i64)>,
    pub overage: Vec<(String, bool)>,
    pub pins_by_pool: Vec<(String, u64)>,
    pub keepalive_agents: KeepaliveAgents,
    pub attention: AttentionGauges,
    /// The accounts the registry still holds, for `render` to drop a deleted account's stale utilization
    /// reading rather than scrape it forever.
    pub live_accounts: BTreeSet<String>,
    /// The router's drop counters at scrape, per report kind, from `RouterHandle::dropped_reports`.
    pub dropped_reports: Vec<(&'static str, u64)>,
    /// Records waiting in the ledger writer channel at scrape, from `Ledger::queued`.
    pub ledger_queued: u64,
    /// Requests answered `ledger_behind` at admission, from `LedgerStats::behind_refusals`.
    pub ledger_behind_refusals: u64,
    /// The ledger's degraded state at scrape, from `Ledger::degraded`: from a failed batch until a batch lands and
    /// the spend is read again.
    pub ledger_degraded: bool,
    /// Priced records parked since start, from `Ledger::parked_priced`.
    pub ledger_parked_priced: u64,
    /// The routing state writer at scrape, from `RouterHandle::writer_status`.
    pub writer: WriterStatus,
}

pub async fn gather(
    router: &RouterHandle,
    table: &PriceTable,
    now: i64,
    fallback_ms: i64,
    keepalive: Option<&KeepaliveGaugeSource>,
    attention: AttentionGauges,
    ledger: Option<&Ledger>,
) -> RoutingGauges {
    let snapshot = router.snapshot();
    let mut gauges = RoutingGauges {
        keepalive_agents: keepalive.map(|k| k()).unwrap_or_default(),
        attention,
        dropped_reports: router.dropped_reports(),
        ledger_queued: ledger.map_or(0, |l| l.queued() as u64),
        ledger_behind_refusals: ledger.map_or(0, |l| {
            l.stats()
                .behind_refusals
                .load(std::sync::atomic::Ordering::Relaxed)
        }),
        ledger_degraded: ledger.is_some_and(Ledger::degraded),
        ledger_parked_priced: ledger.map_or(0, Ledger::parked_priced),
        writer: router.writer_status(),
        ..RoutingGauges::default()
    };
    if let Ok(cooldowns) = router.cooldowns().await {
        // Two models the table does not price both become `model:other`; one sample per label set, and the
        // longer remaining wait is the one that matters.
        let mut coalesced: BTreeMap<(String, String), i64> = BTreeMap::new();
        for c in cooldowns {
            let scope = match &c.scope {
                CooldownScope::Model(model) => {
                    CooldownScope::Model(table.model_label(model).to_string()).to_string()
                }
                other => other.to_string(),
            };
            let remaining = ((c.until - now) / 1_000).max(0);
            let entry = coalesced.entry((c.account_id, scope)).or_insert(0);
            *entry = (*entry).max(remaining);
        }
        gauges.cooldowns = coalesced
            .into_iter()
            .map(|((account, scope), seconds)| (account, scope, seconds))
            .collect();
    }
    // One ask for the whole map: a scrape must not queue one command per account in front of selection.
    let overages: BTreeMap<String, bool> = router
        .overages()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, e)| (id, e.stands(now, fallback_ms)))
        .collect();
    let live_accounts: BTreeSet<String> = snapshot.accounts().map(|a| a.id.clone()).collect();
    for id in &live_accounts {
        let in_use = overages.get(id).copied().unwrap_or(false);
        gauges.overage.push((id.clone(), in_use));
    }
    gauges.live_accounts = live_accounts;
    if let Ok(pins_by_pool) = router.pins_by_pool().await {
        gauges.pins_by_pool = pins_by_pool;
    }
    gauges
}

#[allow(clippy::too_many_arguments)]
pub fn metrics_router(
    metrics: Arc<Metrics>,
    router: RouterHandle,
    pricing: Arc<Pricing>,
    bus: Bus,
    clock: Clock,
    cooldown_fallback: Duration,
    keepalive: Option<KeepaliveGaugeSource>,
    attention: Option<AttentionGaugeSource>,
    ledger: Option<Ledger>,
) -> Router {
    let fallback_ms = cooldown_fallback.as_millis() as i64;
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let (metrics, router, pricing, bus, clock, keepalive, attention, ledger) = (
                    metrics.clone(),
                    router.clone(),
                    pricing.clone(),
                    bus.clone(),
                    clock.clone(),
                    keepalive.clone(),
                    attention.clone(),
                    ledger.clone(),
                );
                async move {
                    // A scrape answered without the conditions would read 0 for a held one and clear the alarm a
                    // rule fires on, so the scraper records a failed scrape instead. The listener asks for no
                    // credential, so the store's or the ledger's words stay in the log. The read waits for its ledger
                    // connection on the runtime, where a wait holds no blocking worker, and runs on a blocking worker,
                    // so it parks no runtime thread.
                    let attention = match attention {
                        None => AttentionGauges::default(),
                        Some(source) => match source().await {
                            Ok(gauges) => gauges,
                            Err(error) => {
                                tracing::warn!(%error, "the attention read failed, so the scrape answers 500");
                                return (StatusCode::INTERNAL_SERVER_ERROR, "attention read failed")
                                    .into_response();
                            }
                        },
                    };
                    let revisions = pricing.revisions();
                    let gauges = gather(
                        &router,
                        revisions.in_force(clock()),
                        clock(),
                        fallback_ms,
                        keepalive.as_ref(),
                        attention,
                        ledger.as_ref(),
                    )
                    .await;
                    let text = metrics.render(&gauges, &bus.stats(), pricing.unknown_models());
                    (
                        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                        text,
                    )
                        .into_response()
                }
            }),
        )
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND.into_response() })
}
