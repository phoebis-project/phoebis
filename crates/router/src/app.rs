//! The running process: the store and the credential manager the three listeners share, and the drain that
//! ends them.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use axum::Router;
use phoebis_admin::{AdminSecrets, AdminState, PairingVault, Sessions, SettingsView};
use phoebis_bus::Bus;
use phoebis_keepalive::{companion_router, spawn_observer, Engine, EngineParts};
use phoebis_ledger::{
    metrics_router, usage_router, LedgerConfig, LedgerDb, Metrics, Pricing, QuotaPoll,
};
use phoebis_live::{LiveConfig, LiveFeed};
use phoebis_oauth::{Clock, CredentialManager, PairingSessions, UsageService};
use phoebis_relay::{CatalogService, Limits, ListenLimits, Relay, REFRESH_TICK};
use phoebis_store::{Secret, StorageKey, Store};
use phoebis_transport::control::{ControlClient, ResolveFuture, Resolver};
use phoebis_transport::pool::Pool;
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};
use tokio::task::JoinHandle;

use crate::config::{Config, UpstreamConfig};
use crate::listen;

/// Whether an alert rule may aim its webhook at an address only this host can reach. Off in the binary; the
/// routes refuse such a destination when a rule is written and the deliverer drops it out of every answer a
/// name resolves to, so both read it from here.
const ALLOW_LOCAL_WEBHOOKS: bool = false;

/// How long a shutdown waits for relays that are still running. A stream that has not ended by then is not
/// going to end sooner because the process keeps waiting.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The native client gives a request ten minutes (`X-Stainless-Timeout: 600`), so neither the wait for the
/// response head nor the wait between stream events gives up before the caller itself would.
const HEAD_TIMEOUT: Duration = Duration::from_secs(600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

// A completion is never dropped now, so an in-flight entry silent this long belongs to a stream the relay
// itself ended at its idle timeout: the assembler's TTL must outwait it, or a still-open stream would be aged
// out from under a request that has not finished.
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

/// Well past the largest measured request; a body beyond it is refused rather than buffered.
const MAX_REQUEST_BYTES: usize = 32 << 20;

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
/// opened on.
const SESSION_ABSOLUTE: Duration = Duration::from_secs(43_200);
const SESSION_IDLE: Duration = Duration::from_secs(900);

/// Builds every component from the configuration, serves the proxy, the admin, and the metrics listeners
/// until `shutdown` resolves, then drains.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    init_tracing();
    let profile = phoebis_profile::by_name(&config.compatibility.profile).ok_or_else(|| {
        anyhow!(
            "compatibility.profile {} is not a profile this build carries",
            config.compatibility.profile
        )
    })?;

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

    let extra_ca = match &config.upstream.extra_ca_pem {
        None => None,
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading upstream.extra_ca_pem {}", path.display()))?,
        ),
    };
    let mut inference_builder = connector_builder(profile, Plane::Inference)?;
    let mut control_builder = connector_builder(profile, Plane::Control)?;
    if let Some(pem) = &extra_ca {
        trust_extra_ca(&mut inference_builder, pem)?;
        trust_extra_ca(&mut control_builder, pem)?;
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
        Duration::from_secs(config.usage.cache_ttl_seconds),
        config.usage.refresh_concurrency,
        clock.clone(),
    ));
    let router_task = phoebis_routing::spawn(
        store.clone(),
        Some(usage.clone()),
        clock.clone(),
        phoebis_routing::RouterConfig::default(),
    )?;
    let bus = Bus::new();
    let ledger_db = Arc::new(LedgerDb::open(&config.stats.ledger_path).with_context(|| {
        format!(
            "opening the ledger at {}",
            config.stats.ledger_path.display()
        )
    })?);
    let pricing = Arc::new(
        Pricing::load(&store, config.stats.pricing_path.as_deref()).with_context(|| {
            format!(
                "loading the price table from {}",
                config
                    .stats
                    .pricing_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the built-in table".to_string())
            )
        })?,
    );
    tracing::info!(ledger = %config.stats.ledger_path.display(), pricing = %pricing.current().version, "ledger opened");
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
    let metrics = Arc::new(Metrics::new());
    let ledger = phoebis_ledger::spawn(
        ledger_db.clone(),
        pricing.clone(),
        store.clone(),
        Some(usage.clone()),
        &bus,
        router_task.clone(),
        clock.clone(),
        metrics.clone(),
        LedgerConfig {
            retention_days: config.stats.retention_days,
            rollup_retention_days: config.stats.rollup_retention_days,
            sample_retention_days: config.quota.sample_retention_days,
            purge_interval: Duration::from_secs(3_600),
            quota_poll_interval: Duration::from_secs(config.quota.poll_interval_seconds),
            event_poll: Duration::from_secs(5),
        },
    );
    let live = LiveFeed::spawn(&bus, pricing.clone(), clock.clone(), LiveConfig::default());
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_task.clone(),
        clock.clone(),
        DISCOVERY_TIMEOUT,
        Duration::from_secs(config.routing.catalog_cache_seconds),
        Duration::from_secs(config.routing.startup_fetch_seconds),
    ));
    let relay = Arc::new(
        Relay::new(
            store.clone(),
            manager.clone(),
            pool.clone(),
            router_task.clone(),
            resolver.clone(),
            clock.clone(),
            Limits {
                body_capture_max_hours: config.stats.body_capture_max_hours,
                cooldown_fallback: Duration::from_secs(config.routing.cooldown_fallback_seconds),
                head_timeout: HEAD_TIMEOUT,
                hold_timeout: HOLD_TIMEOUT,
                idle_timeout: IDLE_TIMEOUT,
                max_attempts: config.routing.max_attempts,
                max_request_bytes: MAX_REQUEST_BYTES,
                model_rejection: Duration::from_secs(config.routing.model_rejection_seconds),
            },
            catalog.clone(),
        )
        .with_bus(bus.clone())
        .with_cache_diagnostics(config.compatibility.cache_diagnostics)
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
    // The companion's reports reach the alerts off the request bus, which drops under pressure while a
    // session's start and end must not.
    let (lifecycle_tx, lifecycle_rx) = phoebis_bus::lifecycle_channel();
    let keepalive = Engine::new(EngineParts {
        bus: bus.clone(),
        clock: clock.clone(),
        cooldown_fallback: Duration::from_secs(config.routing.cooldown_fallback_seconds),
        ledger: ledger.clone(),
        lifecycle: Some(lifecycle_tx),
        limits: config.keepalive.clone(),
        metrics: metrics.clone(),
        pricing: pricing.clone(),
        relay: relay.clone(),
        router: router_task.clone(),
        session_expiry: Duration::from_secs(config.sessions.expire_after_seconds),
    });
    // The day's spend comes from the rollups so a restart does not reset a pool's cap.
    keepalive.load_spent_today().await;
    let alerts = phoebis_alerts::Alerts::new(phoebis_alerts::AlertParts {
        clock: clock.clone(),
        ledger: ledger.clone(),
        metrics: metrics.clone(),
        // The body names this router so a receiver fed by several can tell them apart.
        router_name: config
            .admin
            .origin
            .clone()
            .unwrap_or_else(|| config.admin.listen.to_string()),
        store: store.clone(),
        webhook: Arc::new(phoebis_alerts::ReqwestWebhook::new(ALLOW_LOCAL_WEBHOOKS)),
    });
    let mut alert_inputs = phoebis_alerts::spawn_inputs(
        alerts.clone(),
        phoebis_alerts::AlertInputs {
            body_capture_max_hours: config.stats.body_capture_max_hours,
            bus: bus.clone(),
            lifecycle: Some(lifecycle_rx),
            router: router_task.clone(),
            session_expiry: Duration::from_secs(config.sessions.expire_after_seconds),
            spent_today: Arc::new({
                let engine = keepalive.clone();
                move || engine.spent_today()
            }),
            tick: Duration::from_secs(1),
        },
    );
    let observer = spawn_observer(keepalive.clone());
    let scheduler_tick = keepalive.spawn_scheduler();
    // Captured here so the metrics listener reads the engine's agent counts on every scrape without depending
    // on the keepalive crate.
    let keepalive_gauge_source = keepalive.gauge_source();
    let vault = Arc::new(PairingVault::new());
    let admin = Arc::new(AdminState {
        store,
        manager,
        usage,
        pairing: Arc::new(PairingSessions::new()),
        control,
        profile,
        sessions: Arc::new(Sessions::new(SESSION_IDLE, SESSION_ABSOLUTE)),
        secrets: AdminSecrets {
            admin: Secret::new(config.admin.secret.expose()),
            viewer: config
                .admin
                .viewer_secret
                .as_ref()
                .map(|secret| Secret::new(secret.expose())),
        },
        clock: clock.clone(),
        plain_http: config.admin.plain_http,
        origin: config.admin.origin.clone(),
        vault: vault.clone(),
        started_at: (clock)(),
        router: router_task.clone(),
        catalog: catalog.clone(),
        body_capture_max_hours: config.stats.body_capture_max_hours,
        session_expiry: Duration::from_secs(config.sessions.expire_after_seconds),
        bus: bus.clone(),
        ledger: ledger.clone(),
        live: live.clone(),
        metrics: metrics.clone(),
        pricing: pricing.clone(),
        settings: Arc::new(SettingsView {
            admin_listen: config.admin.listen.to_string(),
            admin_origin: config.admin.origin.clone(),
            body_capture_max_hours: config.stats.body_capture_max_hours,
            cache_diagnostics: config.compatibility.cache_diagnostics,
            catalog_cache_seconds: config.routing.catalog_cache_seconds,
            catalog_refresh_seconds: config.routing.catalog_refresh_seconds,
            cooldown_fallback_seconds: config.routing.cooldown_fallback_seconds,
            keepalive: config.keepalive.clone(),
            ledger_path: config.stats.ledger_path.display().to_string(),
            max_attempts: config.routing.max_attempts,
            metrics_listen: config.stats.metrics_listen.to_string(),
            model_rejection_seconds: config.routing.model_rejection_seconds,
            pricing_path: config
                .stats
                .pricing_path
                .as_ref()
                .map(|p| p.display().to_string()),
            profile: config.compatibility.profile.clone(),
            proxy_listen: config.proxy.listen.to_string(),
            quota_poll_interval_seconds: config.quota.poll_interval_seconds,
            quota_sample_retention_days: config.quota.sample_retention_days,
            retention_days: config.stats.retention_days,
            rollup_retention_days: config.stats.rollup_retention_days,
            session_expire_after_seconds: config.sessions.expire_after_seconds,
            startup_fetch_seconds: config.routing.startup_fetch_seconds,
            storage_path: config.storage.path.display().to_string(),
            usage_cache_ttl_seconds: config.usage.cache_ttl_seconds,
            usage_entrypoint: config.compatibility.usage_entrypoint.clone(),
            usage_refresh_concurrency: config.usage.refresh_concurrency,
            viewer_configured: config.admin.viewer_secret.is_some(),
        }),
        // Cloned rather than moved: the local binding still serves the companion router and the shutdown
        // sequence below.
        keepalive: keepalive.clone(),
        alerts: alerts.clone(),
        allow_local_webhooks: ALLOW_LOCAL_WEBHOOKS,
        pending_imports: Mutex::new(HashMap::new()),
        confirming: tokio::sync::Mutex::new(()),
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
    let refresh = catalog.spawn_refresh(
        Duration::from_secs(config.routing.catalog_refresh_seconds),
        REFRESH_TICK,
    );
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
        proxy = %config.proxy.listen,
        admin = %config.admin.listen,
        metrics = %config.stats.metrics_listen,
        profile = profile.name,
        "listening"
    );
    if phoebis_ui::is_placeholder() {
        tracing::warn!(
            "this build carries no web UI; the admin listener serves a placeholder page"
        );
    }

    // The admin routes carry their full `/admin/...` paths, so they are merged rather than nested.
    let admin_app = Router::new()
        .merge(phoebis_admin::router(admin))
        .merge(phoebis_ui::router());
    // An exchange nobody confirms holds a credential bundle in memory, and only this sweep frees one whose
    // pairing session has run out.
    let sweeper = {
        let clock = clock.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                let swept = vault.sweep(clock());
                if swept > 0 {
                    tracing::debug!(swept, "abandoned pairing exchanges dropped");
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
        companion_router(keepalive.clone()).merge(usage_router(
            ledger_db.clone(),
            router_task.clone(),
            clock.clone(),
        )),
        proxy_stop,
        ListenLimits {
            drain_timeout: DRAIN_TIMEOUT,
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: PROXY_MAX_CONNECTIONS,
        },
    ));
    let mut admin_server = tokio::spawn(listen::serve(
        admin_listener,
        admin_app,
        stop_rx.clone(),
        ListenLimits {
            drain_timeout: DRAIN_TIMEOUT,
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: ADMIN_MAX_CONNECTIONS,
        },
        true,
    ));
    let mut metrics_server = tokio::spawn(listen::serve(
        metrics_listener,
        metrics_router(
            metrics.clone(),
            router_task.clone(),
            pricing.clone(),
            bus.clone(),
            clock.clone(),
            Some(keepalive_gauge_source),
        ),
        stop_rx,
        ListenLimits {
            drain_timeout: DRAIN_TIMEOUT,
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: METRICS_MAX_CONNECTIONS,
        },
        false,
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
        tracing::error!(listener, "a listener ended on its own; shutting down");
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
    let _ = stop.send(true);
    // Stop, close, drain, listeners: the inputs stop posting starts and the poller ends before the keepalive
    // close, so no request observed during the drain is posted after the end of its session; the close
    // enqueues a session_ended per live session; the lifecycle consumer posts those, and the drain then ends
    // whatever sessions are left, before the listeners are joined. The stop is synchronous and sits outside
    // the timed block, so it has run whatever the block does.
    alert_inputs.stop();
    let served = match tokio::time::timeout(DRAIN_TIMEOUT, async {
        // The scheduler stops and its probes end before the listeners drain: a probe runs through the relay,
        // which the drain is about to drop.
        keepalive.shutdown().await;
        alert_inputs.drain().await;
        join_remaining(
            &mut ended,
            [&mut proxy, &mut admin_server, &mut metrics_server],
        )
        .await;
    })
    .await
    {
        Ok(()) => drained(ended[0].take().expect("every slot is filled by now"))
            .and(drained(
                ended[1].take().expect("every slot is filled by now"),
            ))
            .and(drained(
                ended[2].take().expect("every slot is filled by now"),
            )),
        Err(_) => {
            tracing::warn!(
                seconds = DRAIN_TIMEOUT.as_secs(),
                "requests were still in flight when the drain window closed"
            );
            proxy.abort();
            admin_server.abort();
            metrics_server.abort();
            alert_inputs.abort();
            Ok(())
        }
    };
    let served = match early_error {
        Some(error) => Err(error),
        None => served,
    };
    // Every delivery queued before the stop lands before the ledger and the bus close.
    alerts.shutdown().await;
    // The bus arm read through the whole drain so a request completing in it still reached the conditions it
    // carries; with the listeners joined and the engine closed there is nothing left for it to post.
    alert_inputs.abort();
    // The bus closes further down, so the observer is still subscribed on both paths and this ends it.
    observer.abort();
    scheduler_tick.abort();
    sweeper.abort();
    // The engine holds an Arc<Relay>, so dropping the relay alone would not release the pool's last holder.
    drop(keepalive);
    // The listeners are done; a relayed body runs on its own task and a live socket on its own. A cut
    // connection drops its body channel's receiver, so the body task ends at its next send and publishes its
    // completion, which the ledger and the memory below are still alive to take. A body task blocked on an
    // upstream read waits for the relay's idle timeout, and the margin is what a shutdown gives it.
    let margin = DRAIN_TIMEOUT + Duration::from_secs(5);
    if tokio::time::timeout(margin, async {
        live.shutdown().await;
        relay.drain_bodies().await;
    })
    .await
    .is_err()
    {
        tracing::warn!("body tasks or live sockets were still running past the drain margin");
    }
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
    // Idle upstream connections live in the pool, so nothing closes them until the last holder lets go.
    drop(relay);
    // Stopped ahead of the tracked fills and retries below, so no tick still due starts a fill after they join.
    refresh.abort();
    // A retry sleeping its backoff would otherwise hold the store past this function's return.
    catalog.shutdown().await;
    drop(catalog);
    // The last Bus handle on the normal path: the relay's, the admin state's, and the metrics router's
    // clones went with their tasks, so the subscribers' queues close here. The ledger then drains off its
    // stop watch before the router task, the last writer of the registry, shuts down.
    drop(bus);
    // The ledger's event mirror reads routing state, so it stops before the task that answers it.
    if let Err(error) = ledger.shutdown().await {
        tracing::warn!(%error, "the ledger task had already stopped");
    }
    drop(ledger_db);
    // The router task is the last writer: a pin or a cooldown the relay acknowledged reaches the store while
    // this waits, rather than dying with the process.
    if let Err(error) = router_task.shutdown().await {
        tracing::warn!(%error, "the router task had already stopped");
    }
    drop(pool);
    served
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
}
