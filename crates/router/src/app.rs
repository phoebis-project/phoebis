//! The running process: the store and the credential manager the three listeners share, and the drain that
//! ends them.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use axum::Router;
use phoebis_admin::{AdminSecrets, AdminState, PairingVault, Readiness, Sessions, SettingsView};
use phoebis_bus::Bus;
use phoebis_keepalive::{
    keepalive_router, spawn_observer, Engine, EngineParts, KeepaliveTiming, PERSIST_INTERVAL,
};
use phoebis_ledger::{
    metrics_router, usage_router, LedgerConfig, LedgerDb, Metrics, Pricing, QuotaPoll,
};
use phoebis_oauth::{Clock, CredentialManager, PairingSessions, UsageService};
use phoebis_relay::{
    Accounting, CatalogService, Limits, ListenLimits, Relay, UpstreamGzip, REFRESH_TICK,
};
use phoebis_store::{Secret, StorageKey, Store};
use phoebis_transport::control::{ControlClient, ResolveFuture, Resolver};
use phoebis_transport::pool::Pool;
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};
use tokio::task::JoinHandle;

use crate::config::{Config, RoutingConfig, UpstreamConfig};
use crate::{listen, tls};

/// The whole shutdown's budget: every bounded phase takes what remains of it in order, so a phase that overran
/// leaves the next less and the process ends inside the unit's stop grace whatever stalled. The session memory's
/// flush and the store close run whatever the clock reads.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(35);

/// The listeners' drain of in-flight requests, capped below the budget so a stream that outlives it still leaves the
/// phases after it the rest: the ledger's queued records and the router task's acknowledged writes land rather than
/// end with the runtime.
const LISTENER_DRAIN: Duration = Duration::from_secs(30);

/// A first TLS handshake on a cold host runs well under this; a stalled one costs at most twice it before the 502.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The relay's own ceiling on a silent upstream, above any client's wait: Claude Code 2.1.296 aborts a stream
/// after 180 s without a byte and gives the head 180 s plus 1 s per 32 KiB of body under its own 600 s cap, so,
/// with its watchdog on, the client leaves no later than these two guards, which catch a dead upstream behind a
/// client that already left.
const HEAD_TIMEOUT: Duration = Duration::from_secs(600);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

// A completion is published on every path, the handler's drop guard included, so an in-flight entry silent this
// long belongs to a stream the relay itself ended at its idle timeout: the assembler's TTL must outwait it, or a
// still-open stream would be aged out from under a request that has not finished.
const _: () = assert!(phoebis_ledger::IN_FLIGHT_TTL_MS as u128 > IDLE_TIMEOUT.as_millis());

/// A stream that has not reached its first committing event by then is forwarded as it stands, because the
/// caller is waiting and a longer hold buys no better verdict.
const HOLD_TIMEOUT: Duration = Duration::from_secs(30);

const HTTPS_PORT: u16 = 443;

/// A connection that has not sent a request head by then is holding a slot for nothing; the caps keep a
/// flood of such connections from taking the listener's descriptors with it.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const PROXY_MAX_CONNECTIONS: usize = 4_096;
const ADMIN_MAX_CONNECTIONS: usize = 256;
const METRICS_MAX_CONNECTIONS: usize = 64;

/// An upstream connection idle for longer than this is likely to have been closed at the other end already,
/// and reusing it costs a failed write and a retry.
const POOL_IDLE_TTL: Duration = Duration::from_secs(60);
const POOL_MAX_IDLE: usize = 8;

/// A models list is one small request; a host that has not answered by then is not going to.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// An unreachable resolver must not hold a request for longer than this per lookup of an upstream or catalog
/// host.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// A pairing session lives ten minutes, so a sweep this often frees an abandoned exchange within a minute
/// of the deadline it can no longer meet.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// An unattended browser must not keep an admin session usable, and no session outlives the day it was
/// opened on. A login that asks to stay lives by its signed token's 30 days instead.
const SESSION_ABSOLUTE: Duration = Duration::from_secs(43_200);
const SESSION_IDLE: Duration = Duration::from_secs(900);

/// `run_until` for a caller whose runtime outlives the run, as an in-process test's does.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    run_until(config, shutdown, &ShutdownDeadline::default()).await
}

/// Builds every component from the configuration, serves the proxy, the admin, and the metrics listeners
/// until `shutdown` resolves, then drains, the drain's deadline written to `deadline`.
pub async fn run_until(
    config: Config,
    shutdown: impl Future<Output = ()>,
    deadline: &ShutdownDeadline,
) -> anyhow::Result<()> {
    init_tracing();
    let profile = phoebis_profile::claude_code_2_1_269_linux_x64();

    let key = StorageKey::from_file(&config.storage.encryption_key_file).with_context(|| {
        format!(
            "reading storage.encryption_key_file {}",
            config.storage.encryption_key_file.display()
        )
    })?;
    let store = Arc::new(
        Store::open(&config.storage.path, &key)
            .with_context(|| format!("opening the store at {}", config.storage.path.display()))?,
    );
    tracing::info!(database = %config.storage.path.display(), "store opened");
    // Ahead of the recovery below, so the refresh it settles and the quarantine it lands reach the ledger as rows.
    let mirror_baseline = phoebis_ledger::MirrorBaseline::read(&store);

    let extra_ca = match &config.upstream.extra_ca_pem {
        None => None,
        Some(path) => Some((
            path,
            std::fs::read_to_string(path)
                .with_context(|| format!("reading upstream.extra_ca_pem {}", path.display()))?,
        )),
    };
    let mut inference_builder = connector_builder(profile, Plane::Inference)?;
    let mut control_builder = connector_builder(profile, Plane::Control)?;
    if let Some((path, pem)) = &extra_ca {
        trust_extra_ca(&mut inference_builder, pem)
            .with_context(|| format!("upstream.extra_ca_pem {}", path.display()))?;
        trust_extra_ca(&mut control_builder, pem)
            .with_context(|| format!("upstream.extra_ca_pem {}", path.display()))?;
    }

    let clock: Clock = Arc::new(|| jiff::Timestamp::now().as_millisecond());
    let resolver = resolver_for(&config.upstream);
    let control = Arc::new(ControlClient::new(
        profile,
        control_builder.build(),
        resolver.clone(),
    ));
    let manager = Arc::new(CredentialManager::new(
        store.clone(),
        control.clone(),
        profile,
        clock.clone(),
    ));
    for account_id in manager.recover_on_start()? {
        tracing::warn!(
            account = %account_id,
            "a token refresh was in flight when this process last stopped, so the account needs pairing again"
        );
    }
    let pool = Arc::new(Pool::new(
        inference_builder.build(),
        POOL_MAX_IDLE,
        POOL_IDLE_TTL,
    ));
    let usage = Arc::new(UsageService::new(
        manager.clone(),
        control.clone(),
        profile,
        config.usage.refresh_concurrency,
        clock.clone(),
    ));
    // Loaded ahead of the routing task, which prices every burn through it from its first renewal on.
    let pricing = Arc::new(
        Pricing::load(config.stats.api_prices_path.as_deref()).with_context(|| {
            format!(
                "loading the price table from {}",
                config
                    .stats
                    .api_prices_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the built-in table".to_string())
            )
        })?,
    );
    // One copy of the subscription cost model for every reader: the router's burn rates, band and share pricer, the
    // keepalive's settlements and the admin's session burn and settings echo.
    let tables = Arc::new(
        phoebis_ledger::PlanTables::load(config.stats.subscription_costs_path.as_deref())
            .with_context(|| {
                format!(
                    "loading the subscription cost model from {}",
                    config
                        .stats
                        .subscription_costs_path
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "the built-in model".to_string())
                )
            })?,
    );
    let revisions = pricing.revisions();
    let started = clock();
    let prices = revisions.in_force(started);
    let router_task = phoebis_routing::spawn(
        store.clone(),
        clock.clone(),
        router_config(prices, tables.clone(), &config.routing, started),
    )?;
    let bus = Bus::new();
    let ledger_db = Arc::new(LedgerDb::open(&config.stats.ledger_path).with_context(|| {
        format!(
            "opening the ledger at {}",
            config.stats.ledger_path.display()
        )
    })?);
    log_ledger_opened(&config.stats.ledger_path, &revisions, started);
    // Rows from before the rollup key carried a generation are stamped with each live key's own, once; the
    // ledger task must not write before this, or a fresh row could collide with a stamped one. An id that had
    // a holder before this one starts at its own creation, so the earlier holder's rows stay unstamped.
    let load = store
        .load_registry()
        .context("reading the keys for the ledger's generation stamp")?;
    let reused: std::collections::HashSet<&str> =
        load.reused_key_ids.iter().map(String::as_str).collect();
    let live_keys: Vec<phoebis_ledger::LiveKeyLifetime> = load
        .keys
        .iter()
        .map(|k| phoebis_ledger::LiveKeyLifetime {
            id: k.id.clone(),
            created_at: k.created_at,
            floor: reused.contains(k.id.as_str()).then_some(k.created_at),
        })
        .collect();
    let stamped = phoebis_ledger::reconcile_key_generations(&ledger_db, &live_keys)
        .context("stamping key generations onto the ledger's earlier rows")?;
    if stamped > 0 {
        tracing::info!(
            rows = stamped,
            "stamped key generations onto rollup rows written before the column existed"
        );
    }
    let refolded = phoebis_ledger::refold_bands(
        &ledger_db,
        &pricing.revisions(),
        config.stats.retention_days,
        clock(),
    )
    .context("re-folding the rollup rows whose prompt-length band moved")?;
    if refolded > 0 {
        tracing::info!(
            requests = refolded,
            "re-folded the requests whose prompt-length band moved"
        );
    }
    phoebis_ledger::refold_refusals(
        &ledger_db,
        &pricing.revisions(),
        config.stats.retention_days,
        clock(),
    )
    .context("moving the stored refusals out of their hours' counts")?;
    let metrics = Arc::new(Metrics::new());
    let ledger = phoebis_ledger::spawn(
        ledger_db.clone(),
        pricing.clone(),
        tables.clone(),
        store.clone(),
        &bus,
        router_task.clone(),
        clock.clone(),
        metrics.clone(),
        LedgerConfig {
            retention_days: config.stats.retention_days,
            rollup_retention_days: config.stats.rollup_retention_days,
            purge_interval: Duration::from_secs(3_600),
            event_poll: Duration::from_secs(5),
            expire_interval: Duration::from_secs(60),
            start_version: Some(env!("CARGO_PKG_VERSION").into()),
            mirror_baseline: Some(mirror_baseline),
        },
    );
    // The first purge announces the downtime's clock resets and wind-downs: held until here, its rows reach the
    // mirror and land behind the start row, whose gap they would otherwise date.
    router_task.release_purge().await;
    // Moved into the admin state below rather than kept here: a local holding its `Bus` clone to the end of
    // `run` would keep the subscribers' queues open past the shutdown's `drop(bus)`.
    let readiness = Arc::new(Readiness {
        bus: bus.clone(),
        ledger: ledger.clone(),
        router: router_task.clone(),
        store: store.clone(),
    });
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_task.clone(),
        clock.clone(),
        DISCOVERY_TIMEOUT,
        Duration::from_secs(config.routing.startup_fetch_seconds),
    ));
    catalog.set_on_change(Arc::new({
        let ledger = ledger.clone();
        move |change| ledger.record(phoebis_ledger::catalog_changed(&change))
    }));
    let relay = Arc::new(
        Relay::new(
            store.clone(),
            manager.clone(),
            pool.clone(),
            router_task.clone(),
            resolver.clone(),
            clock.clone(),
            Limits {
                body_budget_bytes: config.proxy.body_budget_bytes,
                body_capture_max_hours: config.stats.body_capture_max_hours,
                body_read_timeout: Duration::from_secs(config.proxy.body_read_seconds),
                connect_timeout: CONNECT_TIMEOUT,
                cooldown_fallback: Duration::from_secs(config.routing.cooldown_fallback_seconds),
                head_timeout: HEAD_TIMEOUT,
                hold_timeout: HOLD_TIMEOUT,
                idle_timeout: IDLE_TIMEOUT,
                key_body_reads: config.proxy.key_body_reads,
                max_request_bytes: config.proxy.max_request_bytes,
                model_rejection: Duration::from_secs(config.routing.model_rejection_seconds),
                // The load refused every other spelling, so anything past the two named is `auto`.
                upstream_gzip: match config.proxy.upstream_gzip.as_str() {
                    "off" => UpstreamGzip::Off,
                    "on" => UpstreamGzip::On,
                    _ => UpstreamGzip::Auto,
                },
            },
            catalog.clone(),
        )
        .with_bus(bus.clone())
        .with_readiness({
            let readiness = readiness.clone();
            Arc::new(move || readiness.dead())
        })
        .with_accounting({
            let ledger = ledger.clone();
            Arc::new(move |counts: bool| Accounting {
                behind: if counts {
                    ledger.admission().behind
                } else {
                    ledger.behind()
                },
            })
        })
        .with_signed_placeholder(config.proxy.sign_cch_placeholder)
        .with_profile_name(profile.name)
        .with_session_memory(
            config.sessions.memory_cap,
            Duration::from_secs(config.sessions.memory_idle_seconds),
        )
        .with_memory_store(
            store.clone(),
            Duration::from_secs(u64::from(config.sessions.memory_retention_days) * 86_400),
        ),
    );
    let memory_flush = relay
        .clone()
        .spawn_memory_flush(phoebis_relay::MEMORY_FLUSH_INTERVAL);
    // Read at the start beside the keepalive's timing, so a test's variable has to outlive the start alone.
    let budget_total = shutdown_budget();
    let keepalive = Engine::new(EngineParts {
        bus: bus.clone(),
        clock: clock.clone(),
        cooldown_fallback: Duration::from_secs(config.routing.cooldown_fallback_seconds),
        ledger: ledger.clone(),
        limits: config.keepalive.clone(),
        max_request_bytes: config.proxy.max_request_bytes,
        metrics: metrics.clone(),
        pricing: pricing.clone(),
        relay: relay.clone(),
        router: router_task.clone(),
        session_expiry: Duration::from_secs(config.sessions.expire_after_seconds),
        store: store.clone(),
        tables: tables.clone(),
        timing: KeepaliveTiming::for_start(),
    });
    let keepalive_flush = keepalive.clone().spawn_persist_flush(PERSIST_INTERVAL);
    // The sessions, recipes and counters the last process saved, back before the scheduler's first tick.
    keepalive.load_persisted().await;
    // Empty here because the attention reader and the admin state take it before the pairs load below. The load fills
    // it before any listener binds, so nothing reads it empty.
    let tls_live = tls::LiveSet::new();
    let (attention, mut attention_handles) =
        phoebis_attention::spawn(phoebis_attention::AttentionParts {
            bus: bus.clone(),
            clock: clock.clone(),
            ledger: Some(ledger.clone()),
            router: router_task.clone(),
            store: store.clone(),
            tls: Some(tls_live.clone()),
            usage: Some(usage.clone()),
        });
    let observer = spawn_observer(keepalive.clone());
    let scheduler_tick = keepalive.spawn_scheduler();
    // Captured here so the metrics listener reads the engine's agent counts on every scrape without depending
    // on the keepalive crate.
    let keepalive_gauge_source = keepalive.gauge_source();
    let vault = Arc::new(PairingVault::new());
    let pairing = Arc::new(PairingSessions::new());
    let sessions = Arc::new(
        Sessions::new(
            SESSION_IDLE,
            SESSION_ABSOLUTE,
            Secret::new(config.admin.secret.expose()),
            store.clone(),
        )
        .context("reading the admin session epoch from the store")?,
    );
    // The admin state takes the manager; the shutdown drains its refreshes after the admin listener is gone.
    let refreshes = manager.clone();
    let admin = Arc::new(AdminState {
        store,
        manager,
        usage,
        pairing: pairing.clone(),
        control,
        profile,
        sessions: sessions.clone(),
        secrets: AdminSecrets {
            admin: Secret::new(config.admin.secret.expose()),
        },
        clock: clock.clone(),
        tls: config.admin.tls.is_some(),
        tls_reload: Some(tls_live.clone()),
        vault: vault.clone(),
        started_at: (clock)(),
        build: crate::BUILD_COMMIT,
        router: router_task.clone(),
        catalog: catalog.clone(),
        body_capture_max_hours: config.stats.body_capture_max_hours,
        session_expiry: Duration::from_secs(config.sessions.expire_after_seconds),
        bus: bus.clone(),
        ledger: ledger.clone(),
        metrics: metrics.clone(),
        pricing: pricing.clone(),
        tables: tables.clone(),
        settings: Arc::new(settings_view(&config, profile)),
        // Cloned rather than moved: the local binding still serves the companion router and the shutdown
        // sequence below.
        keepalive: keepalive.clone(),
        attention: attention.clone(),
        readiness,
    });

    // Before the proxy listener binds, so the first request finds the dated ids and the samples in place; each
    // half bounds itself by the same clock, and what is still running at the bound finishes in the background.
    // The store and the usage service live in the admin state by now.
    let startup = Duration::from_secs(config.routing.startup_fetch_seconds);
    let (prefetched, sampled) = tokio::join!(
        catalog.prefetch(),
        QuotaPoll::sample_all(
            admin.store.clone(),
            admin.usage.clone(),
            ledger.clone(),
            router_task.clone(),
            startup,
        ),
    );
    tracing::info!(
        catalogs_landed = prefetched.landed,
        catalogs_failed = prefetched.failed,
        catalogs_pending = prefetched.pending,
        samples_reported = sampled.reported,
        samples_failed = sampled.failed,
        samples_pending = sampled.pending,
        bound_seconds = config.routing.startup_fetch_seconds,
        "startup fetch finished"
    );
    // Started only now: the startup step has just read every account, so the scheduler's first slots lie one
    // interval plus their phase from here. The ledger aborts it before its channel closes.
    ledger.track(QuotaPoll::spawn(
        admin.store.clone(),
        admin.usage.clone(),
        ledger.clone(),
        router_task.clone(),
    ));
    let refresh = catalog.spawn_refresh(
        Duration::from_secs(config.routing.catalog_refresh_seconds),
        REFRESH_TICK,
    );
    // Read before any bind, so a pair the start cannot serve refuses with its line and no port is taken.
    let proxy_tls = tls::load("proxy.tls", &config.proxy.tls)?;
    let admin_tls = tls::load("admin.tls", &config.admin.tls)?;
    for (listener, key, serving) in [
        ("proxy", "proxy.tls", &proxy_tls),
        ("admin", "admin.tls", &admin_tls),
    ] {
        if let tls::Serving::Tls(loaded) = serving {
            tracing::info!(listener, names = ?loaded.leaf.names, not_after = %loaded.leaf.not_after, "serving TLS");
        }
        for warning in tls::warnings(key, serving) {
            tracing::warn!("{warning}");
        }
    }
    let loaded_at = clock();
    let proxy_live = live_pair("proxy.tls", proxy_tls, loaded_at);
    let admin_live = live_pair("admin.tls", admin_tls, loaded_at);
    let proxy_acceptor = acceptor(&proxy_live);
    let admin_acceptor = acceptor(&admin_live);
    tls_live.fill(vec![("proxy", proxy_live), ("admin", admin_live)]);
    let proxy_listener = tokio::net::TcpListener::bind(config.proxy.listen)
        .await
        .with_context(|| format!("binding proxy.listen {}", config.proxy.listen))?;
    let admin_listener = tokio::net::TcpListener::bind(config.admin.listen)
        .await
        .with_context(|| format!("binding admin.listen {}", config.admin.listen))?;
    let metrics_listener = tokio::net::TcpListener::bind(config.stats.metrics_listen)
        .await
        .with_context(|| {
            format!(
                "binding stats.metrics_listen {}",
                config.stats.metrics_listen
            )
        })?;
    tracing::info!(
        proxy = %format_args!("{}://{}", tls::scheme(&config.proxy.tls), config.proxy.listen),
        admin = %format_args!("{}://{}", tls::scheme(&config.admin.tls), config.admin.listen),
        metrics = %format_args!("http://{}", config.stats.metrics_listen),
        profile = profile.name,
        "listening"
    );
    if phoebis_ui::is_placeholder() {
        tracing::warn!(
            "this build carries no web UI, so the admin listener serves a placeholder page"
        );
    }

    // The admin routes carry their full `/admin/...` paths, so they are merged rather than nested.
    let admin_app = Router::new()
        .merge(phoebis_admin::router(admin))
        .merge(phoebis_ui::router());
    // An exchange nobody confirms holds a credential bundle in memory, and only this sweep frees one whose
    // pairing session has run out; an ended pairing and an expired admin session would otherwise stay in their
    // maps until the process ends.
    let sweeper = {
        let clock = clock.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                let now = clock();
                let exchanges = vault.sweep(now);
                let pairings = pairing.sweep(now);
                let admin_sessions = sessions.sweep(now);
                if exchanges + pairings + admin_sessions > 0 {
                    tracing::debug!(
                        exchanges,
                        pairings,
                        admin_sessions,
                        "ended pairings and expired admin sessions dropped"
                    );
                }
            }
        })
    };
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let proxy_stop = stop_rx.clone();
    // The relayed surface is served through the relay's own accept loop so every connection can be wrapped
    // to preserve the caller's original header spelling; the admin and metrics listeners share a plainer
    // loop with the same header read timeout and connection cap.
    let mut proxy = tokio::spawn(phoebis_relay::serve(
        proxy_listener,
        relay.clone(),
        keepalive_router(keepalive.clone()).merge(usage_router(
            ledger_db.clone(),
            router_task.clone(),
            clock.clone(),
            pricing.clone(),
            tables.clone(),
        )),
        proxy_stop,
        ListenLimits {
            drain_timeout: LISTENER_DRAIN,
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: PROXY_MAX_CONNECTIONS,
        },
        proxy_acceptor,
    ));
    let mut admin_server = tokio::spawn(listen::serve(
        admin_listener,
        admin_app,
        stop_rx.clone(),
        ListenLimits {
            drain_timeout: LISTENER_DRAIN,
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: ADMIN_MAX_CONNECTIONS,
        },
        admin_acceptor,
    ));
    let mut metrics_server = tokio::spawn(listen::serve(
        metrics_listener,
        metrics_router(
            metrics.clone(),
            router_task.clone(),
            pricing.clone(),
            bus.clone(),
            clock.clone(),
            Duration::from_secs(config.routing.cooldown_fallback_seconds),
            Some(keepalive_gauge_source),
            Some(attention.gauge_source()),
            Some(ledger.clone()),
        ),
        stop_rx,
        ListenLimits {
            drain_timeout: LISTENER_DRAIN,
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: METRICS_MAX_CONNECTIONS,
        },
        None,
    ));

    // A listener that ends on its own is read here, once: a `JoinHandle` polled again after it completed
    // panics, so its result is kept in a slot and the join below skips that handle.
    let mut ended: [Option<Listened>; 3] = [None, None, None];
    let ended_early = first_to_end(
        shutdown,
        &mut proxy,
        &mut admin_server,
        &mut metrics_server,
        &mut ended,
    )
    .await;
    if let Some(listener) = ended_early {
        tracing::error!(
            listener,
            "a listener ended on its own, so the process shuts down"
        );
    }
    let early_error = ended_early.map(|listener| {
        let index = match listener {
            "proxy" => 0,
            "admin" => 1,
            _ => 2,
        };
        let result = ended[index]
            .as_ref()
            .expect("first_to_end filled the ended listener's slot");
        anyhow!("the {listener} listener ended on its own: {result:?}")
    });
    tracing::info!("shutting down");
    let budget = deadline.start(budget_total);
    let _ = stop.send(true);
    // The attention arm's state dies with the process, so nothing the drain completes is worth reading into it. The
    // stop is synchronous and sits outside the timed block, so it has run whatever the block does.
    attention_handles.stop();
    let listeners = async {
        // The scheduler stops and its probes end before the listeners drain: a probe runs through the relay,
        // which the drain is about to drop. The engine quiesces first, so no update lands after the state it saves,
        // then drains its probes, saves, and clears last, so the clear takes nothing unsaved. The flusher is joined
        // rather than aborted: the quiesce ended its loop, and a flush it had in flight ran to its end under the
        // flush lock ahead of the shutdown's own.
        keepalive.shutdown().await;
        if let Err(error) = keepalive_flush.await {
            tracing::warn!(%error, "the keepalive's flush task did not end cleanly");
        }
        tracing::info!("the keepalive's last flush ran");
        join_remaining(
            &mut ended,
            [&mut proxy, &mut admin_server, &mut metrics_server],
        )
        .await;
    };
    let served = match budget
        .bound_within("listeners", LISTENER_DRAIN, listeners)
        .await
    {
        Some(()) => drained(ended[0].take().expect("every slot is filled by now"))
            .and(drained(
                ended[1].take().expect("every slot is filled by now"),
            ))
            .and(drained(
                ended[2].take().expect("every slot is filled by now"),
            )),
        None => {
            tracing::warn!("requests were still in flight when the drain window closed");
            proxy.abort();
            admin_server.abort();
            metrics_server.abort();
            attention_handles.abort();
            Ok(())
        }
    };
    let served = match early_error {
        Some(error) => Err(error),
        None => served,
    };
    // The stop lets the arm finish the event in hand; whatever is left of it ends before the bus closes.
    attention_handles.abort();
    // The bus closes further down, so the observer is still subscribed on both paths and this ends it.
    observer.abort();
    scheduler_tick.abort();
    sweeper.abort();
    // The engine holds an Arc<Relay>, so dropping the relay alone would not release the pool's last holder.
    drop(keepalive);
    // The listeners are done, while a relayed body runs on its own task. A cut connection drops its body channel's
    // receiver, so the body task ends at its next send and publishes its completion, which the ledger and the
    // memory below are still alive to take. A body task blocked on an upstream read waits for the relay's idle
    // timeout, and what the listeners left of the budget is what a shutdown gives it.
    budget.bound("bodies", relay.drain_bodies()).await;
    // The periodic flusher is aborted and joined, and a flush it had in flight runs to its end on the
    // blocking pool under the memory's flush lock, so the last flush queues behind it and an older batch
    // never follows a newer one to the store. The last flush lands after the listeners drained, so every
    // request that committed reaches the store.
    memory_flush.abort();
    if let Err(error) = memory_flush.await {
        // The abort above is the expected end; anything else is a flusher that died on its own, which means
        // the flushes between its last one and this shutdown never ran.
        if !error.is_cancelled() {
            tracing::warn!(%error, "the session memory's flush task did not end cleanly");
        }
    }
    relay.flush_memory().await;
    tracing::info!("the session memory's last flush ran");
    // Idle upstream connections live in the pool, so nothing closes them until the last holder lets go.
    drop(relay);
    // Stopped ahead of the tracked fills and retries below, so no tick still due starts a fill after they join.
    refresh.abort();
    // A retry sleeping its backoff would otherwise hold the store past this function's return.
    budget.bound("catalog", catalog.shutdown()).await;
    drop(catalog);
    // A refresh the usage service or the catalog started dies with the runtime otherwise, and the next start
    // quarantines the account for a lost answer that was only unjoined. The drain closes the manager before it
    // takes its set, so a refresh asked for later, as the ledger's quota poll may until the ledger stops below, is
    // refused rather than started outside the set. Its own bound counts inside what the budget has left.
    refreshes.drain_within(budget.remaining()).await;
    // The last Bus handle on the normal path: the relay's, the admin state's, and the metrics router's
    // clones went with their tasks, so the subscribers' queues close here. The ledger then drains off its
    // stop watch before the router task, the last writer of the registry, shuts down.
    drop(bus);
    // The ledger's event mirror reads routing state, so it stops before the task that answers it.
    match budget.bound("ledger", ledger.shutdown()).await {
        Some(Err(error)) => tracing::warn!(%error, "the ledger task had already stopped"),
        None => tracing::warn!(
            "the ledger's last write was still running when the shutdown budget ran out, so the process ends without it"
        ),
        Some(Ok(())) => {}
    }
    drop(ledger_db);
    // The router task is the last writer: a pin or a cooldown the relay acknowledged reaches the store while
    // this waits, rather than dying with the process.
    match budget.bound("router", router_task.shutdown()).await {
        Some(Err(error)) => tracing::warn!(%error, "the router task had already stopped"),
        None => tracing::warn!(
            "the routing state's last registry write was still running when the shutdown budget ran out, so the \
             process ends without it"
        ),
        Some(Ok(())) => {}
    }
    drop(pool);
    served
}

/// The routing task's burn prices and peak band: the model's 5-hour share per token on its anchor tier per regime and
/// its band, and for dollars `prices`, the revision of the table `Pricing::load` chose that is in force at the start
/// `now`, the share of an account's window a completed request consumed, and the share per second a pin's burn
/// consumes, both priced through the model on the account's tier, and the Max 20x equivalent of a window a pin's
/// rewrite spends, priced on the anchor tier whatever the account's. Every closure prices under the regime of the
/// entrypoint it is handed, the base one for `None`. The `BurnRates` tables and the measured windows are read at `now`
/// and fixed for the life of the process, so a pending revision reaches them at the next start, while the two shares
/// and the equivalent price at each call's instant under the revision in force then.
fn router_config(
    prices: &phoebis_ledger::PriceTable,
    tables: Arc<phoebis_ledger::PlanTables>,
    routing: &RoutingConfig,
    now: i64,
) -> phoebis_routing::RouterConfig {
    let band = tables.clone();
    let measured = tables.clone();
    let rate_tables = tables.clone();
    let equivalent_tables = tables.clone();
    let segment_tables = tables.clone();
    phoebis_routing::RouterConfig {
        band: phoebis_routing::BandWeight::new(move |window, entrypoint, at| {
            band.band_factor(window, entrypoint, at)
        }),
        rates: phoebis_routing::BurnRates {
            dollars: prices.rate_table(),
            five_hour: tables.regime_rates(phoebis_routing::FIVE_HOUR, now),
            fast: prices.fast_rate_table(),
        },
        share: phoebis_routing::SharePricer::new(
            move |window, entrypoint, tier, model, usage, at| {
                tables.share_at(window, entrypoint, tier, model, usage, at)
            },
        )
        .with_measured(move |window| measured.measured(window, now))
        .with_rate(move |window, entrypoint, tier, model, burn, at| {
            rate_tables.share_rate_at(window, entrypoint, tier, model, burn, at)
        })
        .with_equivalent(move |window, entrypoint, model, usage, at| {
            equivalent_tables.equivalent_at(window, entrypoint, model, usage, at)
        })
        .with_band_segments(move |window, entrypoint, from, to| {
            segment_tables.band_segments(window, entrypoint, from, to)
        }),
        funding_retry: Duration::from_secs(300.min(routing.model_rejection_seconds)),
        // Released once the ledger's mirror subscribed and the start row is recorded, below.
        hold_first_purge: true,
        ..phoebis_routing::RouterConfig::default()
    }
}

/// The start's `ledger opened` line. It names the date of the first entry dated after the one in force at the start,
/// and no date when none is. The tables built at the start take that entry only at the next one, so an operator reads
/// in the log what a restart changes.
fn log_ledger_opened(
    ledger: &std::path::Path,
    revisions: &phoebis_ledger::PriceRevisions,
    started: i64,
) {
    tracing::info!(
        ledger = %ledger.display(),
        pricing = %revisions.in_force(started).effective,
        pricing_revisions = revisions.len(),
        // An `Option` records nothing at `None`, so the line names no next date while nothing is held.
        pricing_next = revisions
            .pending(started)
            .map(|next| tracing::field::display(&next.effective)),
        "ledger opened"
    );
}

fn settings_view(config: &Config, profile: &phoebis_profile::Profile) -> SettingsView {
    SettingsView {
        api_prices_path: config
            .stats
            .api_prices_path
            .as_ref()
            .map(|p| p.display().to_string()),
        body_capture_max_hours: config.stats.body_capture_max_hours,
        cooldown_fallback_seconds: config.routing.cooldown_fallback_seconds,
        keepalive: config.keepalive.clone(),
        ledger_path: config.stats.ledger_path.display().to_string(),
        metrics_listen: config.stats.metrics_listen.to_string(),
        profile: profile.name.to_string(),
        proxy_listen: config.proxy.listen.to_string(),
        proxy_tls: config.proxy.tls.is_some(),
        retention_days: config.stats.retention_days,
        rollup_retention_days: config.stats.rollup_retention_days,
        session_expire_after_seconds: config.sessions.expire_after_seconds,
        storage_path: config.storage.path.display().to_string(),
        subscription_costs_path: config
            .stats
            .subscription_costs_path
            .as_ref()
            .map(|p| p.display().to_string()),
    }
}

/// The listener's pair as the router serves it, for what `tls::load` answered: `None` is plain HTTP.
fn live_pair(key: &'static str, serving: tls::Serving, now: i64) -> Option<tls::Live> {
    match serving {
        tls::Serving::Plain => None,
        tls::Serving::Tls(loaded) => Some(tls::Live::new(key, loaded, now)),
    }
}

/// The accept loop's acceptor: the pair's own, so a reload reaches the loop.
fn acceptor(live: &Option<tls::Live>) -> tls::Acceptor {
    live.as_ref().map(tls::Live::acceptor)
}

/// One listener's outcome: the task either panicked or the serve future itself failed.
type Listened = Result<std::io::Result<()>, tokio::task::JoinError>;

fn drained(result: Listened) -> anyhow::Result<()> {
    Ok(result??)
}

/// Waits for the shutdown signal or the first listener to end; an ended listener's result goes into its slot.
async fn first_to_end(
    shutdown: impl Future<Output = ()>,
    proxy: &mut JoinHandle<std::io::Result<()>>,
    admin: &mut JoinHandle<std::io::Result<()>>,
    metrics: &mut JoinHandle<std::io::Result<()>>,
    ended: &mut [Option<Listened>; 3],
) -> Option<&'static str> {
    tokio::pin!(shutdown);
    tokio::select! {
        _ = &mut shutdown => None,
        result = proxy => { ended[0] = Some(result); Some("proxy") }
        result = admin => { ended[1] = Some(result); Some("admin") }
        result = metrics => { ended[2] = Some(result); Some("metrics") }
    }
}

/// Joins the listeners the early-exit select did not consume; the one it did is read from its slot.
async fn join_remaining(
    ended: &mut [Option<Listened>; 3],
    handles: [&mut JoinHandle<std::io::Result<()>>; 3],
) {
    for (slot, handle) in ended.iter_mut().zip(handles) {
        if slot.is_none() {
            *slot = Some(handle.await);
        }
    }
}

/// The shutdown's one deadline, which every bounded phase reads from the same start.
struct ShutdownBudget {
    deadline: tokio::time::Instant,
}

impl ShutdownBudget {
    fn start(total: Duration) -> ShutdownBudget {
        ShutdownBudget {
            deadline: tokio::time::Instant::now() + total,
        }
    }

    /// What is left, zero once the deadline passed.
    fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// `phase` bounded by the deadline: `None` when it ran out, with one warn line naming the phase and the seconds
    /// it was given. The future is polled once before the deadline is read, so a phase that is done at its first poll
    /// still answers once nothing remains, and a stop it starts there is at least asked for.
    async fn bound<F: Future>(&self, phase: &'static str, future: F) -> Option<F::Output> {
        self.bound_within(phase, self.remaining(), future).await
    }

    /// `bound`, with the phase given at most `within` of what remains, so a phase that can run long leaves the phases
    /// after it the rest of the budget.
    async fn bound_within<F: Future>(
        &self,
        phase: &'static str,
        within: Duration,
        future: F,
    ) -> Option<F::Output> {
        let now = tokio::time::Instant::now();
        let deadline = self.deadline.min(now + within);
        let given = deadline.saturating_duration_since(now);
        match tokio::time::timeout_at(deadline, future).await {
            Ok(output) => Some(output),
            Err(_) => {
                tracing::warn!(
                    phase,
                    seconds = given.as_secs(),
                    "the shutdown budget ran out before this phase ended, so it was cut"
                );
                None
            }
        }
    }
}

/// The shutdown budget's end on the process clock, set when the budget starts. A phase the budget cut leaves its
/// writer running on a blocking thread, and a runtime dropped as it is waits for that thread without a bound, so
/// `main` stops the runtime within what is left of the budget.
#[derive(Clone, Debug, Default)]
pub struct ShutdownDeadline(Arc<std::sync::OnceLock<std::time::Instant>>);

impl ShutdownDeadline {
    /// What the runtime's teardown may wait at `now`: zero once the deadline passed, and the whole budget when no
    /// shutdown started, as for a start that failed.
    pub fn remaining_at(&self, now: std::time::Instant) -> Duration {
        match self.0.get() {
            Some(deadline) => deadline.saturating_duration_since(now),
            None => shutdown_budget(),
        }
    }

    fn start(&self, total: Duration) -> ShutdownBudget {
        let budget = ShutdownBudget::start(total);
        let _ = self.0.set(budget.deadline.into_std());
        budget
    }
}

/// The budget a shutdown starts with. Under the `debug-hooks` feature, which only the router's own test build turns
/// on, `PHOEBIS_DEBUG_SHUTDOWN_BUDGET_SECONDS` replaces it, so an end-to-end case sees a stalled stop end in seconds.
fn shutdown_budget() -> Duration {
    #[cfg(feature = "debug-hooks")]
    if let Some(seconds) = std::env::var("PHOEBIS_DEBUG_SHUTDOWN_BUDGET_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_secs(seconds);
    }
    SHUTDOWN_BUDGET
}

fn resolver_for(upstream: &UpstreamConfig) -> Resolver {
    if let Some(addr) = upstream.resolve_override {
        // Every host goes to the one address, which is how a lab upstream stands in for the real one.
        return Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    }
    // The control plane and the relay ask for a name once per new connection rather than per request. The
    // port travels with the authority the caller passes in; a bare host defaults to 443.
    Arc::new(move |authority: &str| {
        let (host, port) = match split_authority(authority) {
            Some(split) => split,
            // A bracketed v6 literal carries its brackets here too, and no resolver takes them.
            None => (
                authority.trim_start_matches('[').trim_end_matches(']'),
                HTTPS_PORT,
            ),
        };
        let (host, port) = (host.to_string(), port);
        Box::pin(async move {
            // Bounded: an unreachable resolver must not hold a request for the resolver's own timeout.
            tokio::time::timeout(
                RESOLVE_TIMEOUT,
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            .ok()?
            .ok()?
            .next()
        })
    })
}

/// `host:port` or `[v6]:port`; `None` when there is no port.
fn split_authority(authority: &str) -> Option<(&str, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    if host.contains(':') && !host.starts_with('[') {
        return None;
    }
    Some((
        host.trim_start_matches('[').trim_end_matches(']'),
        port.parse().ok()?,
    ))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Two runs in one process, as the tests do it, must not take the process down over the second one.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monday 2026-09-21 at 14:00 UTC: the start the tests build the router's tables at.
    const START: i64 = 1_789_999_200_000;

    #[test]
    fn the_router_config_prices_a_windows_share_through_the_shipped_plan_tables() {
        let tables = phoebis_ledger::PlanTables::shipped();
        let config = router_config(
            phoebis_ledger::PriceRevisions::shipped().in_force(START),
            Arc::new(tables.clone()),
            &RoutingConfig::default(),
            START,
        );
        let usage = phoebis_routing::PinUsage {
            input: 1_000,
            cache_read: 10_000,
            write_5m: 100,
            write_1h: 200,
            output: 500,
        };
        let tier = "default_claude_max_20x";
        // Monday 2026-09-21 at 14:00 UTC, inside the peak band.
        let at = 1_789_999_200_000;
        assert_eq!(
            config.share.price(
                phoebis_routing::FIVE_HOUR,
                None,
                tier,
                "claude-opus-5-5",
                usage,
                at
            ),
            tables.share_at(
                phoebis_routing::FIVE_HOUR,
                None,
                tier,
                "claude-opus-5-5",
                usage,
                at
            ),
            "the 5-hour share is weighed by the band at the completion"
        );
        for window in [phoebis_routing::SEVEN_DAY, phoebis_routing::SEVEN_DAY_FABLE] {
            assert_eq!(
                config
                    .share
                    .price(window, None, tier, "claude-opus-5-5", usage, at),
                tables.share_at(window, None, tier, "claude-opus-5-5", usage, at),
                "{window}"
            );
            assert!(config.share.measured(window), "{window}");
        }
        assert!(
            !config.share.measured("seven_day_opus"),
            "the router reads the tables' own answer on a model week they do not measure"
        );
        let burn = phoebis_store::PinBurn {
            cache_read: 100.0,
            input: 10.0,
            output: 5.0,
            write_1h: 2.0,
            write_5m: 1.0,
        };
        for window in [
            phoebis_routing::FIVE_HOUR,
            phoebis_routing::SEVEN_DAY,
            phoebis_routing::SEVEN_DAY_FABLE,
        ] {
            let rate = config
                .share
                .rate(window, None, tier, "claude-opus-5-5", &burn, at);
            assert_eq!(
                rate,
                tables.share_rate_at(window, None, tier, "claude-opus-5-5", &burn, at),
                "a pin's burn on {window} is priced through the same tables"
            );
            assert!(rate.is_some_and(|rate| rate > 0.0), "{window}");
        }
        assert!(config
            .share
            .price(
                phoebis_routing::FIVE_HOUR,
                None,
                tier,
                "claude-opus-5-5",
                usage,
                at
            )
            .is_some_and(|share| share > 0.0));
        assert_eq!(
            config.share.price(
                phoebis_routing::FIVE_HOUR,
                None,
                "no_such_tier",
                "claude-opus-5-5",
                usage,
                at
            ),
            None,
            "a tier the model does not size prices at no share"
        );
        assert_eq!(
            config.share.price(
                phoebis_routing::FIVE_HOUR,
                None,
                tier,
                "claude-mystery-2",
                usage,
                at
            ),
            None,
            "the shipped model places Mystery 2 in no tier, so nothing weighs it"
        );
    }

    /// The equivalent prices on the anchor tier through the model week's own capacity factor, so on tables whose Fable
    /// week holds a quarter of the 7-day window's capacity, a write's Fable-week figure reads four times its 7-day one.
    /// The test model's regime `desk` holds twice the base 5-hour capacity on Max 20x, so outside the band a write
    /// under it reads half the base figure.
    #[test]
    fn the_router_configs_equivalent_closure_prices_a_write_through_the_tables() {
        let quarter_week = phoebis_ledger::test_tables::first_entry_costs_text().replacen(
            "seven_day_fable: {of: seven_day, capacity_factor: 0.5}",
            "seven_day_fable: {of: seven_day, capacity_factor: 0.25}",
            1,
        );
        let tables = phoebis_ledger::PlanTables {
            revisions: phoebis_ledger::CostRevisions::from_yaml(&quarter_week)
                .expect("the test cost model parses"),
            path: None,
            source: "test".into(),
        };
        let config = router_config(
            phoebis_ledger::PriceRevisions::shipped().in_force(START),
            Arc::new(tables),
            &RoutingConfig::default(),
            START,
        );
        let write = phoebis_routing::PinUsage {
            write_5m: 1_000,
            write_1h: 500,
            ..Default::default()
        };
        let model = "claude-fable-5-1";
        let at = START;
        let week = config
            .share
            .equivalent(phoebis_routing::SEVEN_DAY, None, model, write, at);
        let fable_week =
            config
                .share
                .equivalent(phoebis_routing::SEVEN_DAY_FABLE, None, model, write, at);
        assert!(
            fable_week
                .zip(week)
                .is_some_and(|(fable_week, week)| week > 0.0
                    && (fable_week - 4.0 * week).abs() < 1e-12),
            "the Fable week's factor of a quarter reads four times the 7-day figure: {fable_week:?} against {week:?}"
        );
        // Monday 2026-09-21 at 20:00 UTC, outside the test model's band.
        let out_of_band = 1_790_020_800_000;
        let five_hour = |entrypoint: Option<&str>| {
            config.share.equivalent(
                phoebis_routing::FIVE_HOUR,
                entrypoint,
                model,
                write,
                out_of_band,
            )
        };
        assert!(
            five_hour(Some("desk"))
                .zip(five_hour(None))
                .is_some_and(|(desk, base)| base > 0.0 && (desk / base - 0.5).abs() < 1e-12),
            "the equivalent prices under the regime its entrypoint names: {:?} against {:?}",
            five_hour(Some("desk")),
            five_hour(None)
        );
        let shipped = phoebis_ledger::PlanTables::shipped();
        let config = router_config(
            phoebis_ledger::PriceRevisions::shipped().in_force(START),
            Arc::new(shipped.clone()),
            &RoutingConfig::default(),
            START,
        );
        for window in [
            phoebis_routing::FIVE_HOUR,
            phoebis_routing::SEVEN_DAY,
            phoebis_routing::SEVEN_DAY_FABLE,
        ] {
            assert_eq!(
                config.share.equivalent(window, None, model, write, at),
                shipped.equivalent_at(window, None, model, write, at),
                "the closure answers the tables' equivalent on {window}"
            );
        }
    }

    /// The test model's regime `desk` holds twice the base 5-hour capacity on Max 20x, and the base's band halves the
    /// base capacity while the regime keeps its own, so outside the band every figure under the regime is half the base
    /// one, and inside it the band weighs the base alone.
    #[test]
    fn the_router_config_passes_the_entrypoint_through_every_closure_and_builds_the_regime_tables()
    {
        let tables = phoebis_ledger::PlanTables {
            revisions: phoebis_ledger::CostRevisions::from_yaml(
                &phoebis_ledger::test_tables::first_entry_costs_text(),
            )
            .expect("the test model parses"),
            path: None,
            source: "test".to_string(),
        };
        let config = router_config(
            phoebis_ledger::PriceRevisions::shipped().in_force(START),
            Arc::new(tables),
            &RoutingConfig::default(),
            START,
        );
        let usage = phoebis_routing::PinUsage {
            input: 1_000,
            ..Default::default()
        };
        let burn = phoebis_store::PinBurn {
            input: 10.0,
            ..Default::default()
        };
        let (tier, model) = ("default_claude_max_20x", "claude-opus-5-5");
        // Monday 2026-09-21 at 14:00 and at 20:00 UTC, inside and outside the test model's band.
        let (in_band, out_of_band) = (1_789_999_200_000, 1_790_020_800_000);
        let half = |under: Option<f64>, base: Option<f64>| {
            under
                .zip(base)
                .is_some_and(|(under, base)| base > 0.0 && (under / base - 0.5).abs() < 1e-12)
        };
        let five_hour = phoebis_routing::FIVE_HOUR;
        assert!(
            half(
                config
                    .share
                    .price(five_hour, Some("desk"), tier, model, usage, out_of_band),
                config
                    .share
                    .price(five_hour, None, tier, model, usage, out_of_band)
            ),
            "a request's charge prices under its regime"
        );
        assert!(
            half(
                config
                    .share
                    .rate(five_hour, Some("desk"), tier, model, &burn, out_of_band),
                config
                    .share
                    .rate(five_hour, None, tier, model, &burn, out_of_band)
            ),
            "a pin's burn prices under its regime"
        );
        let input =
            |entrypoint| config.rates.five_hour.for_entrypoint(entrypoint).rows[model].input;
        assert!(
            half(input(Some("desk")), input(None)),
            "the burn table of the regime ranks its pins at half the base rate"
        );
        assert_eq!(
            config.rates.five_hour.for_entrypoint(Some("pane")),
            config.rates.five_hour.for_entrypoint(Some("desk")),
            "every entrypoint the regime lists reads its table"
        );
        assert_eq!(
            config.band.at(five_hour, Some("desk"), in_band),
            1.0,
            "the regime carries no band"
        );
        assert_eq!(
            config.band.at(five_hour, None, in_band),
            2.0,
            "the base band halves the base capacity"
        );
        // Monday 2026-09-21 at 12:00 UTC, the test model's band opening.
        let (opens, hour) = (1_789_992_000_000, 3_600_000);
        let segments = |entrypoint| {
            config
                .share
                .band_segments(five_hour, entrypoint, opens - hour, opens + hour)
                .iter()
                .map(|segment| (segment.from, segment.to, segment.weight))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            segments(None),
            [(opens - hour, opens, 1.0), (opens, opens + hour, 2.0)],
            "the base band cuts a span across its opening"
        );
        assert_eq!(
            segments(Some("desk")),
            [(opens - hour, opens + hour, 1.0)],
            "the regime's span reads one segment"
        );
    }

    #[test]
    fn the_router_config_retries_a_funding_mark_after_the_shorter_of_five_minutes_and_the_model_rejection_interval(
    ) {
        let config = |routing: &RoutingConfig| {
            router_config(
                phoebis_ledger::PriceRevisions::shipped().in_force(START),
                Arc::new(phoebis_ledger::PlanTables::shipped()),
                routing,
                START,
            )
            .funding_retry
        };
        assert_eq!(
            config(&RoutingConfig {
                model_rejection_seconds: 120,
                ..RoutingConfig::default()
            }),
            Duration::from_secs(120)
        );
        assert_eq!(
            config(&RoutingConfig::default()),
            Duration::from_secs(300),
            "the default model rejection interval is longer than five minutes"
        );
    }

    /// The week's band factor is 1 in the shipped model; a file that bands the week reaches the router's 7-day
    /// share through the completion the router hands the model, with no change to the router.
    #[test]
    fn the_router_config_weighs_a_banded_week_by_the_band_at_the_completion() {
        let banded_week = phoebis_ledger::test_tables::first_entry_costs_text().replacen(
            "capacity_factor: {five_hour: 0.5, seven_day: 1}",
            "capacity_factor: {five_hour: 0.5, seven_day: 0.5}",
            1,
        );
        let tables = phoebis_ledger::PlanTables {
            revisions: phoebis_ledger::CostRevisions::from_yaml(&banded_week)
                .expect("the banded week parses"),
            path: None,
            source: "test".to_string(),
        };
        let config = router_config(
            phoebis_ledger::PriceRevisions::shipped().in_force(START),
            Arc::new(tables.clone()),
            &RoutingConfig::default(),
            START,
        );
        let usage = phoebis_routing::PinUsage {
            input: 1_000,
            ..Default::default()
        };
        let tier = "default_claude_max_20x";
        // Monday 2026-09-21 at 14:00 and at 20:00 UTC.
        let (in_band, out_of_band) = (1_789_999_200_000, 1_790_020_800_000);
        let week = |at| {
            config.share.price(
                phoebis_routing::SEVEN_DAY,
                None,
                tier,
                "claude-opus-5-5",
                usage,
                at,
            )
        };
        assert_eq!(
            week(in_band),
            tables.share_at(
                phoebis_routing::SEVEN_DAY,
                None,
                tier,
                "claude-opus-5-5",
                usage,
                in_band
            ),
            "the router prices the week as the model it loaded does at the completion"
        );
        assert!(
            week(in_band)
                .zip(week(out_of_band))
                .is_some_and(|(inside, outside)| inside > outside),
            "a week charge inside the band weighs more than one outside it: {:?} against {:?}",
            week(in_band),
            week(out_of_band)
        );
    }

    #[test]
    fn the_router_config_prices_the_burn_through_the_cost_models_five_hour_rates_and_the_price_table(
    ) {
        let revisions = phoebis_ledger::PriceRevisions::shipped();
        let prices = revisions.in_force(START);
        let tables = phoebis_ledger::PlanTables::shipped();
        let config = router_config(
            prices,
            Arc::new(tables.clone()),
            &RoutingConfig::default(),
            START,
        );
        let usage = phoebis_routing::PinUsage {
            input: 1_000,
            cache_read: 10_000,
            write_5m: 100,
            write_1h: 200,
            output: 500,
        };
        let prompt = usage.input + usage.cache_read + usage.write_5m + usage.write_1h;
        // Monday 2026-09-21 at 14:00 and at 20:00 UTC.
        let (in_band, out_of_band) = (1_789_999_200_000, 1_790_020_800_000);
        let share = config
            .rates
            .five_hour
            .base
            .price("claude-opus-5-5", usage, prompt)
            .expect("a priced model");
        let expected = tables
            .share_at(
                phoebis_routing::FIVE_HOUR,
                None,
                "default_claude_max_20x",
                "claude-opus-5-5",
                usage,
                out_of_band,
            )
            .unwrap();
        assert!(
            (share - expected).abs() < 1e-15,
            "the 5-hour table is the anchor tier's share per token, unweighed by the band: {share} against {expected}"
        );
        assert_ne!(
            share,
            phoebis_routing::RateTable::unit()
                .price("claude-opus-5-5", usage, prompt)
                .expect("a priced model"),
            "the share table, not the unit table's token count"
        );
        let estimated = config
            .rates
            .five_hour
            .base
            .price("claude-opus-4-8", usage, prompt)
            .expect("a model of a weighted model tier");
        let expected = tables
            .share_at(
                phoebis_routing::FIVE_HOUR,
                None,
                "default_claude_max_20x",
                "claude-opus-4-8",
                usage,
                out_of_band,
            )
            .unwrap();
        assert!(
            (estimated - expected).abs() < 1e-15,
            "a model the cost model has no weight for prices at its model tier's estimate, as the meter charges it: \
             {estimated} against {expected}"
        );
        assert_eq!(
            config.rates.five_hour.base.price("gpt-5", usage, prompt),
            None,
            "the share table prices no model outside every weighted model tier"
        );
        assert!(
            config
                .rates
                .dollars
                .price("gpt-5", usage, prompt)
                .is_some_and(|usd| usd > 0.0),
            "the dollar table keeps its dearest-price fallback"
        );
        let (usd, _) = prices
            .estimate(
                "claude-sonnet-5",
                phoebis_ledger::Speed::Standard,
                1_000,
                10_000,
                100,
                200,
                500,
            )
            .unwrap();
        assert!(
            (config
                .rates
                .dollars
                .price("claude-sonnet-5", usage, prompt)
                .expect("a priced model")
                - usd)
                .abs()
                < 1e-12,
            "the dollar table prices a usage as the price table estimates it"
        );
        let (fast_usd, _) = prices
            .estimate(
                "claude-opus-5-5",
                phoebis_ledger::Speed::Fast,
                1_000,
                10_000,
                100,
                200,
                500,
            )
            .unwrap();
        assert!(
            (config
                .rates
                .fast
                .price("claude-opus-5-5", usage, prompt)
                .expect("a priced model")
                - fast_usd)
                .abs()
                < 1e-12,
            "the fast table prices a usage as the price table estimates a fast answer"
        );
        let banded = config.band.at(phoebis_routing::FIVE_HOUR, None, in_band);
        assert_eq!(
            banded,
            tables.band_factor(phoebis_routing::FIVE_HOUR, None, in_band),
            "the router weighs the band as the model it loaded does"
        );
        assert_eq!(
            config
                .band
                .at(phoebis_routing::FIVE_HOUR, None, out_of_band),
            1.0
        );
        assert!(
            banded > 1.0,
            "a charge inside the band weighs more than one outside it: {banded}"
        );
    }

    /// The routing task's 5-hour burn rates are built once at the start, so a cost revision dated after the start is
    /// held for the next one: the rates read the cost entry in force at the start and never the held entry's doubled
    /// weights.
    #[test]
    fn the_router_config_reads_the_cost_revision_in_force_at_the_start_and_not_a_pending_one() {
        let held = phoebis_ledger::test_tables::SUBSCRIPTION_COSTS
            .replace("effective: 2026-02-01", "effective: 2099-01-01");
        let tables = phoebis_ledger::PlanTables {
            revisions: phoebis_ledger::CostRevisions::from_yaml(&held)
                .expect("the list with a held entry parses"),
            path: None,
            source: "test".to_string(),
        };
        let config = router_config(
            phoebis_ledger::PriceRevisions::shipped().in_force(START),
            Arc::new(tables.clone()),
            &RoutingConfig::default(),
            START,
        );
        let in_force = tables.revisions.in_force(START);
        let pending = tables
            .revisions
            .pending(START)
            .expect("the 2099 entry is held at the start");
        assert_eq!(
            (in_force.effective.as_str(), pending.effective.as_str()),
            ("2026-01-01", "2099-01-01")
        );
        let opus = |table: &phoebis_routing::RateTable| table.rates_for("claude-opus-5-5", 0);
        let burn = opus(&config.rates.five_hour.base);
        assert_eq!(
            burn,
            opus(&in_force.rate_table(phoebis_routing::FIVE_HOUR, None)),
            "the burn rates are the entry in force at the start"
        );
        assert_ne!(
            burn,
            opus(&pending.rate_table(phoebis_routing::FIVE_HOUR, None)),
            "the held entry waits for the next start"
        );
    }

    /// The formatter's output kept in memory, so a case reads back the lines a function logged.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The start log names the price list's entry in force and its revision count, and a held entry's date only while
    /// one is held, so an operator reads in the log what a restart changes. A lone entry dated after the start is
    /// already in force, so nothing is held.
    #[test]
    fn the_start_log_names_the_price_revisions_and_a_held_entrys_date_only_while_one_is_held() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(captured.clone())
            .finish();
        let held = phoebis_ledger::PriceRevisions::from_yaml(
            &phoebis_ledger::test_tables::API_PRICES
                .replace("effective: 2026-02-01", "effective: 2099-01-01"),
            "test",
        )
        .expect("the list with a held entry parses");
        let alone = phoebis_ledger::PriceRevisions::new(vec![held.in_force(START).clone()], "test")
            .expect("one entry");
        let ahead = phoebis_ledger::PriceRevisions::new(vec![held.latest().clone()], "test")
            .expect("one entry");
        tracing::subscriber::with_default(subscriber, || {
            log_ledger_opened(std::path::Path::new("ledger.sqlite"), &held, START);
            log_ledger_opened(std::path::Path::new("ledger.sqlite"), &alone, START);
            log_ledger_opened(std::path::Path::new("ledger.sqlite"), &ahead, START);
        });
        let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text.lines().map(str::trim).collect::<Vec<_>>(),
            [
                "INFO ledger opened ledger=ledger.sqlite pricing=2026-01-01 pricing_revisions=2 \
                 pricing_next=2099-01-01",
                "INFO ledger opened ledger=ledger.sqlite pricing=2026-01-01 pricing_revisions=1",
                "INFO ledger opened ledger=ledger.sqlite pricing=2099-01-01 pricing_revisions=1",
            ]
        );
    }

    /// One row with a `fast` block and one without, so the fast table's rule shows on a table of the test's own.
    const FAST_AND_PLAIN: &str = "\
revisions:
  - effective: 2026-01-01
    models:
      claude-fast-1:
        {input: 2, cache_write_5m: 3, cache_write_1h: 4, cache_read: 1, output: 10,
         fast: {input: 12, cache_write_5m: 15, cache_write_1h: 24, cache_read: 6, output: 60}}
      claude-plain-1: {input: 3, cache_write_5m: 3.75, cache_write_1h: 6, cache_read: 0.3, output: 15}
";

    #[test]
    fn the_router_configs_fast_table_reads_a_rows_fast_cells_and_a_row_without_them_its_standard_cells(
    ) {
        let prices = phoebis_ledger::PriceRevisions::from_yaml(FAST_AND_PLAIN, "test").unwrap();
        let config = router_config(
            prices.in_force(START),
            Arc::new(phoebis_ledger::PlanTables::shipped()),
            &RoutingConfig::default(),
            START,
        );
        let per_token =
            |[input, write_5m, write_1h, read, output]: [f64; 5]| phoebis_routing::Rates {
                input: Some(input / 1_000_000.0),
                cache_read: Some(read / 1_000_000.0),
                write_5m: Some(write_5m / 1_000_000.0),
                write_1h: Some(write_1h / 1_000_000.0),
                output: Some(output / 1_000_000.0),
            };
        assert_eq!(
            config.rates.fast.rates_for("claude-fast-1", 0),
            per_token([12.0, 15.0, 24.0, 6.0, 60.0]),
            "a row with a fast block burns fast at its fast cells"
        );
        assert_eq!(
            config.rates.dollars.rates_for("claude-fast-1", 0),
            per_token([2.0, 3.0, 4.0, 1.0, 10.0]),
            "the dollar table keeps the same row's standard cells"
        );
        assert_eq!(
            config.rates.fast.rates_for("claude-plain-1", 0),
            per_token([3.0, 3.75, 6.0, 0.3, 15.0]),
            "a row without a fast block burns fast at its standard cells"
        );
    }

    #[test]
    fn the_settings_view_says_the_proxy_listener_terminates_tls_when_the_file_names_its_pair() {
        let config = |proxy_lines: &str| -> Config {
            serde_norway::from_str(&format!(
                "admin:\n  listen: 127.0.0.1:8081\n  secret_env: TEST_SETTINGS_ADMIN\n\
                 proxy:\n  listen: 0.0.0.0:8317\n{proxy_lines}\
                 storage:\n  encryption_key_file: storage.key\n  path: router.sqlite\n"
            ))
            .unwrap()
        };
        let profile = phoebis_profile::claude_code_2_1_269_linux_x64();
        let plain = settings_view(&config(""), profile);
        let paired = settings_view(
            &config("  tls:\n    cert_file: cert.pem\n    key_file: key.pem\n"),
            profile,
        );
        assert_eq!((plain.proxy_tls, paired.proxy_tls), (false, true));
    }

    #[test]
    fn the_settings_view_mirrors_the_stats_sections_two_retention_keys() {
        let config: Config = serde_norway::from_str(
            "admin:\n  listen: 127.0.0.1:8081\n  secret_env: TEST_SETTINGS_ADMIN\n\
             proxy:\n  listen: 0.0.0.0:8317\n\
             stats:\n  retention_days: 30\n  rollup_retention_days: 0\n\
             storage:\n  encryption_key_file: storage.key\n  path: router.sqlite\n",
        )
        .unwrap();
        let view = settings_view(&config, phoebis_profile::claude_code_2_1_269_linux_x64());
        assert_eq!((view.retention_days, view.rollup_retention_days), (30, 0));
    }

    /// The anchor tier carries a capacity on both windows, so every model the cost model names has a burn rate on
    /// each, at its own weight or, as an estimate, at its model tier's latest weight: a pin on a model without a
    /// weight ranks by its estimated burn rather than last.
    #[test]
    fn the_burn_rates_carry_a_row_per_model_the_fallback_prices_so_an_unweighted_pin_ranks_by_its_estimate(
    ) {
        let tables = phoebis_ledger::PlanTables::shipped();
        let costs = tables.revisions.in_force(START);
        for window in [phoebis_routing::FIVE_HOUR, phoebis_routing::SEVEN_DAY] {
            let rates = tables.rate_table(window, None, START);
            let mut rows: Vec<&str> = rates.rows.keys().map(String::as_str).collect();
            rows.sort_unstable();
            let mut priced: Vec<&str> = costs
                .model_weight
                .keys()
                .chain(costs.model_tier.keys())
                .map(String::as_str)
                .filter(|model| costs.weight_or_fallback(model, window).is_some())
                .collect();
            priced.sort_unstable();
            priced.dedup();
            assert!(
                !rows.is_empty(),
                "{window}: the shipped model weighs a model"
            );
            assert_eq!(rows, priced, "{window}");
        }
    }

    #[tokio::test]
    async fn the_production_resolver_honours_an_explicit_port_and_defaults_to_443() {
        let resolve = resolver_for(&UpstreamConfig::default());
        assert_eq!(
            resolve("127.0.0.1:8317").await.map(|a| a.port()),
            Some(8317)
        );
        assert_eq!(resolve("127.0.0.1").await.map(|a| a.port()), Some(443));
        assert_eq!(resolve("[::1]:8443").await.map(|a| a.port()), Some(8443));
        assert_eq!(resolve("[::1]").await.map(|a| a.port()), Some(443));
        let answer = tokio::time::timeout(
            RESOLVE_TIMEOUT + Duration::from_secs(1),
            resolve("no-such-host.invalid"),
        )
        .await
        .expect("the resolver's own bound ends the lookup within RESOLVE_TIMEOUT");
        assert!(answer.is_none());
    }

    /// The early-exit select must not poll a `JoinHandle` it already consumed: `join_remaining` reads that
    /// listener's result from its slot instead.
    #[tokio::test]
    async fn a_listener_the_early_select_consumed_is_not_polled_again() {
        let (tx_admin, rx_admin) = tokio::sync::oneshot::channel::<()>();
        let (tx_metrics, rx_metrics) = tokio::sync::oneshot::channel::<()>();
        let mut proxy: JoinHandle<std::io::Result<()>> =
            tokio::spawn(async { Err(std::io::Error::other("the proxy listener failed")) });
        let mut admin: JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            let _ = rx_admin.await;
            Ok(())
        });
        let mut metrics: JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            let _ = rx_metrics.await;
            Ok(())
        });
        let mut ended: [Option<Listened>; 3] = [None, None, None];
        let ended_early = first_to_end(
            std::future::pending(),
            &mut proxy,
            &mut admin,
            &mut metrics,
            &mut ended,
        )
        .await;
        assert_eq!(ended_early, Some("proxy"));
        assert!(ended[0].is_some());
        assert!(ended[1].is_none());
        assert!(ended[2].is_none());
        let _ = tx_admin.send(());
        let _ = tx_metrics.send(());
        join_remaining(&mut ended, [&mut proxy, &mut admin, &mut metrics]).await;
        assert!(ended.iter().all(Option::is_some), "{ended:?}");
    }

    #[test]
    fn the_runtime_teardown_waits_what_the_budget_left_and_the_whole_budget_before_any_shutdown() {
        let deadline = ShutdownDeadline::default();
        let now = std::time::Instant::now();
        assert_eq!(
            deadline.remaining_at(now),
            shutdown_budget(),
            "a start that failed returns before the budget starts"
        );
        deadline.0.set(now + Duration::from_secs(10)).unwrap();
        assert_eq!(
            deadline.remaining_at(now + Duration::from_secs(3)),
            Duration::from_secs(7)
        );
        assert_eq!(
            deadline.remaining_at(now + Duration::from_secs(12)),
            Duration::ZERO,
            "a blocking write past the deadline gets no wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn each_shutdown_phase_takes_what_remains_of_one_budget_from_its_start() {
        let start = tokio::time::Instant::now();
        let budget = ShutdownBudget::start(Duration::from_secs(10));
        let phase = |seconds| tokio::time::sleep(Duration::from_secs(seconds));
        assert_eq!(budget.bound("listeners", phase(3)).await, Some(()));
        assert_eq!(budget.remaining(), Duration::from_secs(7));
        assert_eq!(budget.bound("bodies", phase(3)).await, Some(()));
        assert_eq!(budget.remaining(), Duration::from_secs(4));
        assert_eq!(
            budget.bound("catalog", phase(5)).await,
            None,
            "the third phase gets the four seconds the first two left, not a five of its own"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert_eq!(budget.remaining(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_shutdown_phase_that_overruns_leaves_the_next_less_down_to_zero() {
        let start = tokio::time::Instant::now();
        let budget = ShutdownBudget::start(Duration::from_secs(10));
        let phase = |seconds| tokio::time::sleep(Duration::from_secs(seconds));
        assert_eq!(budget.bound("listeners", phase(7)).await, Some(()));
        assert_eq!(
            budget.bound("bodies", phase(5)).await,
            None,
            "the overrunning phase is cut where the budget ends"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert_eq!(budget.remaining(), Duration::ZERO);
        assert_eq!(
            budget.bound("catalog", std::future::ready(7)).await,
            Some(7),
            "a phase done at its first poll still answers once nothing remains"
        );
        assert_eq!(
            budget.bound("ledger", phase(1)).await,
            None,
            "a phase still pending gets nothing once the budget is spent"
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(10),
            "a spent budget waits for nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_capped_phase_ends_at_its_cap_and_leaves_the_phases_after_it_the_rest_of_the_budget()
    {
        let start = tokio::time::Instant::now();
        let budget = ShutdownBudget::start(Duration::from_secs(35));
        let phase = |seconds| tokio::time::sleep(Duration::from_secs(seconds));
        assert_eq!(
            budget
                .bound_within("listeners", Duration::from_secs(30), phase(60))
                .await,
            None,
            "a stream that outlives the drain is cut at the cap"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(30));
        assert_eq!(budget.remaining(), Duration::from_secs(5));
        assert_eq!(
            budget.bound("ledger", phase(4)).await,
            Some(()),
            "the phases after it share what the cap left"
        );
        assert_eq!(
            budget
                .bound_within("router", Duration::from_secs(30), phase(2))
                .await,
            None,
            "a cap past the deadline is cut to it"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(35));
    }
}
