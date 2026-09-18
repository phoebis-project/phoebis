//! Prometheus text format, rendered by hand: a handful of counters and gauges with bounded label sets, and
//! one histogram over the same 64 buckets the rollups use.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use phoebis_bus::{Bus, SubscriberStats};
use phoebis_oauth::Clock;
use phoebis_routing::RouterHandle;

use crate::histogram::{bucket_edges, bucket_index, top_edge, BUCKET_COUNT};
use crate::pricing::{PriceTable, Pricing};
use crate::record::{AttemptRecord, QuotaSampleRecord, RequestRecord};
use crate::rollup::UNATTRIBUTED;

const LOCK_POISONED: &str = "metrics registry poisoned by a prior panic";

type Labels = Vec<(&'static str, String)>;

/// One model's TTFB series: the 64 finite buckets, the observations past the last edge, the sum in seconds,
/// and the count. The overflow is kept apart so `+Inf` can carry what no finite bound answers for.
struct Series {
    buckets: [u64; BUCKET_COUNT],
    count: u64,
    overflow: u64,
    sum: f64,
}

impl Default for Series {
    fn default() -> Series {
        Series {
            buckets: [0; BUCKET_COUNT],
            count: 0,
            overflow: 0,
            sum: 0.0,
        }
    }
}

#[derive(Default)]
struct Registry {
    counters: BTreeMap<&'static str, BTreeMap<Labels, f64>>,
    gauges: BTreeMap<&'static str, BTreeMap<Labels, f64>>,
    ttfb: BTreeMap<String, Series>,
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

    fn add(&self, name: &'static str, labels: Labels, by: f64) {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        *inner
            .counters
            .entry(name)
            .or_default()
            .entry(labels)
            .or_default() += by;
    }

    fn set(&self, name: &'static str, labels: Labels, value: f64) {
        let mut inner = self.inner.lock().expect(LOCK_POISONED);
        inner.gauges.entry(name).or_default().insert(labels, value);
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
        self.add(
            "phoebis_requests_total",
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
                "phoebis_attempts_total",
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
                        "phoebis_tokens_total",
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
                entry.count += 1;
            }
        }
    }

    pub fn observe_samples(&self, quota: &[QuotaSampleRecord]) {
        for q in quota {
            if let Some(u) = q.utilization {
                self.set(
                    "phoebis_account_ratelimit_utilization",
                    vec![
                        ("account", q.account_id.clone()),
                        ("window", q.window.clone()),
                    ],
                    u,
                );
            }
        }
    }

    pub fn observe_drop(&self, table: &'static str) {
        self.add(
            "phoebis_ledger_dropped_total",
            vec![("table", table.into())],
            1.0,
        );
    }

    pub fn observe_probe(&self, account: &str, outcome: &str) {
        self.add(
            "phoebis_keepalive_probes_total",
            vec![("account", account.into()), ("outcome", outcome.into())],
            1.0,
        );
    }

    pub fn observe_saving(&self, pool: &str, usd: f64) {
        self.add(
            "phoebis_keepalive_saved_usd_total",
            vec![("pool", pool.into())],
            usd,
        );
    }

    pub fn observe_alert_delivery(&self, rule: &str, outcome: &str) {
        self.add(
            "phoebis_alert_deliveries_total",
            vec![("outcome", outcome.into()), ("rule", rule.into())],
            1.0,
        );
    }

    pub fn render(
        &self,
        gauges: &RoutingGauges,
        bus: &[(&'static str, Arc<SubscriberStats>)],
        pricing_unknown: u64,
    ) -> String {
        let inner = self.inner.lock().expect(LOCK_POISONED);
        let mut out = String::new();
        for (name, kind) in FAMILIES.iter().copied() {
            out.push_str(&format!("# TYPE {name} {kind}\n"));
            match name {
                "phoebis_alert_deliveries_total"
                | "phoebis_attempts_total"
                | "phoebis_keepalive_probes_total"
                | "phoebis_keepalive_saved_usd_total"
                | "phoebis_ledger_dropped_total"
                | "phoebis_requests_total"
                | "phoebis_tokens_total" => {
                    if let Some(series) = inner.counters.get(name) {
                        for (labels, value) in series {
                            out.push_str(&format!(
                                "{name}{} {}\n",
                                render_labels(labels),
                                render_value(*value)
                            ));
                        }
                    }
                }
                "phoebis_pricing_unknown_model_total" => {
                    out.push_str(&format!(
                        "phoebis_pricing_unknown_model_total {pricing_unknown}\n"
                    ));
                }
                "phoebis_bus_dropped_total" => {
                    for (subscriber, stats) in bus {
                        out.push_str(&format!(
                            "phoebis_bus_dropped_total{{subscriber=\"{}\"}} {}\n",
                            escape(subscriber),
                            stats.dropped.load(std::sync::atomic::Ordering::Relaxed)
                        ));
                    }
                }
                "phoebis_bus_subscriber_panics_total" => {
                    for (subscriber, stats) in bus {
                        out.push_str(&format!(
                            "phoebis_bus_subscriber_panics_total{{subscriber=\"{}\"}} {}\n",
                            escape(subscriber),
                            stats.panics.load(std::sync::atomic::Ordering::Relaxed)
                        ));
                    }
                }
                "phoebis_account_cooldown_seconds" => {
                    for (account, scope, seconds) in &gauges.cooldowns {
                        out.push_str(&format!(
                            "phoebis_account_cooldown_seconds{{account=\"{}\",scope=\"{}\"}} {seconds}\n",
                            escape(account),
                            escape(scope)
                        ));
                    }
                }
                "phoebis_account_overage_in_use" => {
                    for (account, in_use) in &gauges.overage {
                        out.push_str(&format!(
                            "phoebis_account_overage_in_use{{account=\"{}\"}} {}\n",
                            escape(account),
                            u8::from(*in_use)
                        ));
                    }
                }
                "phoebis_account_ratelimit_utilization" => {
                    if let Some(series) = inner.gauges.get("phoebis_account_ratelimit_utilization") {
                        for (labels, value) in series {
                            out.push_str(&format!(
                                "phoebis_account_ratelimit_utilization{} {}\n",
                                render_labels(labels),
                                render_value(*value)
                            ));
                        }
                    }
                }
                "phoebis_keepalive_agents" => {
                    // Always the three series, so a scrape reads zero rather than absent when the engine
                    // retains nothing.
                    for (state, count) in [
                        ("busy", gauges.keepalive_agents.busy),
                        ("idle", gauges.keepalive_agents.idle),
                        ("parked", gauges.keepalive_agents.parked),
                    ] {
                        out.push_str(&format!(
                            "phoebis_keepalive_agents{{state=\"{state}\"}} {count}\n"
                        ));
                    }
                }
                "phoebis_pins_active" => {
                    for (pool, count) in &gauges.pins_by_pool {
                        out.push_str(&format!(
                            "phoebis_pins_active{{pool=\"{}\"}} {count}\n",
                            escape(pool)
                        ));
                    }
                }
                "phoebis_upstream_ttfb_seconds" => {
                    let edges = bucket_edges();
                    for (model, series) in &inner.ttfb {
                        let mut cumulative = 0u64;
                        for (i, n) in series.buckets.iter().enumerate() {
                            cumulative += n;
                            out.push_str(&format!(
                                "phoebis_upstream_ttfb_seconds_bucket{{model=\"{}\",le=\"{}\"}} {cumulative}\n",
                                escape(model),
                                render_value(edges[i + 1] / 1_000.0)
                            ));
                        }
                        // The only bound an observation past the last edge is under.
                        out.push_str(&format!(
                            "phoebis_upstream_ttfb_seconds_bucket{{model=\"{}\",le=\"+Inf\"}} {}\n",
                            escape(model),
                            cumulative + series.overflow
                        ));
                        out.push_str(&format!(
                            "phoebis_upstream_ttfb_seconds_sum{{model=\"{}\"}} {}\n",
                            escape(model),
                            render_value(series.sum)
                        ));
                        out.push_str(&format!(
                            "phoebis_upstream_ttfb_seconds_count{{model=\"{}\"}} {}\n",
                            escape(model),
                            series.count
                        ));
                    }
                }
                _ => unreachable!("every family in FAMILIES has a renderer arm"),
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

/// Every family this renderer emits, in name order, with its type. The renderer walks this list for its TYPE
/// lines and the test walks it against the text, so a family registered without a renderer arm, or rendered
/// without a registration, fails there rather than vanishing from the scrape.
pub const FAMILIES: &[(&str, &str)] = &[
    ("phoebis_account_cooldown_seconds", "gauge"),
    ("phoebis_account_overage_in_use", "gauge"),
    ("phoebis_account_ratelimit_utilization", "gauge"),
    ("phoebis_alert_deliveries_total", "counter"),
    ("phoebis_attempts_total", "counter"),
    ("phoebis_bus_dropped_total", "counter"),
    ("phoebis_bus_subscriber_panics_total", "counter"),
    ("phoebis_keepalive_agents", "gauge"),
    ("phoebis_keepalive_probes_total", "counter"),
    ("phoebis_keepalive_saved_usd_total", "counter"),
    ("phoebis_ledger_dropped_total", "counter"),
    ("phoebis_pins_active", "gauge"),
    ("phoebis_pricing_unknown_model_total", "counter"),
    ("phoebis_requests_total", "counter"),
    ("phoebis_tokens_total", "counter"),
    ("phoebis_upstream_ttfb_seconds", "histogram"),
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeepaliveAgents {
    pub busy: u64,
    pub idle: u64,
    pub parked: u64,
}

/// A closure over the keepalive engine's own lock, read fresh on every scrape rather than pushed: the ledger
/// crate has no dependency on the keepalive crate, so the binary supplies this instead of a type it can name.
pub type KeepaliveGaugeSource = Arc<dyn Fn() -> KeepaliveAgents + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct RoutingGauges {
    pub cooldowns: Vec<(String, String, i64)>,
    pub overage: Vec<(String, bool)>,
    pub pins_by_pool: Vec<(String, u64)>,
    pub keepalive_agents: KeepaliveAgents,
}

pub async fn gather(
    router: &RouterHandle,
    table: &PriceTable,
    now: i64,
    keepalive: Option<&KeepaliveGaugeSource>,
) -> RoutingGauges {
    let snapshot = router.snapshot();
    let mut gauges = RoutingGauges {
        keepalive_agents: keepalive.map(|k| k()).unwrap_or_default(),
        ..RoutingGauges::default()
    };
    if let Ok(cooldowns) = router.cooldowns().await {
        // Two models the table does not price both become `model:other`; one sample per label set, and the
        // longer remaining wait is the one that matters.
        let mut coalesced: BTreeMap<(String, String), i64> = BTreeMap::new();
        for c in cooldowns {
            let scope = match c.scope.strip_prefix("model:") {
                Some(model) => format!("model:{}", table.model_label(model)),
                None => c.scope.clone(),
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
        .map(|(id, e)| (id, e.in_use))
        .collect();
    let mut accounts: Vec<String> = snapshot.accounts().map(|a| a.id.clone()).collect();
    accounts.sort();
    for id in accounts {
        let in_use = overages.get(&id).copied().unwrap_or(false);
        gauges.overage.push((id, in_use));
    }
    if let Ok(pins) = router.pins().await {
        let mut by_pool: BTreeMap<String, u64> = BTreeMap::new();
        for pin in pins {
            let pool = snapshot
                .key(&pin.key_id)
                .map(|k| k.pool_id.clone())
                .unwrap_or_else(|| UNATTRIBUTED.into());
            *by_pool.entry(pool).or_default() += 1;
        }
        gauges.pins_by_pool = by_pool.into_iter().collect();
    }
    gauges
}

pub fn metrics_router(
    metrics: Arc<Metrics>,
    router: RouterHandle,
    pricing: Arc<Pricing>,
    bus: Bus,
    clock: Clock,
    keepalive: Option<KeepaliveGaugeSource>,
) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let (metrics, router, pricing, bus, clock, keepalive) = (
                    metrics.clone(),
                    router.clone(),
                    pricing.clone(),
                    bus.clone(),
                    clock.clone(),
                    keepalive.clone(),
                );
                async move {
                    let table = pricing.current();
                    let gauges = gather(&router, &table, clock(), keepalive.as_ref()).await;
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
