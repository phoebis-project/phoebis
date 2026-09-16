//! The running process: the store and the credential manager the two listeners share, and the drain that
//! ends them.

use std::future::{Future, IntoFuture};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use phoebis_admin::{AdminSecrets, AdminState, PairingVault, Sessions};
use phoebis_oauth::{Clock, CredentialManager, PairingSessions, UsageService};
use phoebis_relay::{CatalogService, Limits, Relay};
use phoebis_store::{Secret, StorageKey, Store};
use phoebis_transport::control::{ControlClient, Resolver};
use phoebis_transport::pool::Pool;
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};

use crate::config::{Config, UpstreamConfig};

/// How long a shutdown waits for relays that are still running. A stream that has not ended by then is not
/// going to end sooner because the process keeps waiting.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The native client gives a request ten minutes (`X-Stainless-Timeout: 600`), so neither the wait for the
/// response head nor the wait between stream events gives up before the caller itself would.
const HEAD_TIMEOUT: Duration = Duration::from_secs(600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// A stream that has not reached its first committing event by then is forwarded as it stands, because the
/// caller is waiting and a longer hold buys no better verdict.
const HOLD_TIMEOUT: Duration = Duration::from_secs(30);

const HTTPS_PORT: u16 = 443;

/// Well past the largest measured request; a body beyond it is refused rather than buffered.
const MAX_REQUEST_BYTES: usize = 32 << 20;

/// An upstream connection idle for longer than this is likely to have been closed at the other end already,
/// and reusing it costs a failed write and a retry.
const POOL_IDLE_TTL: Duration = Duration::from_secs(60);
const POOL_MAX_IDLE: usize = 8;

/// A models list is one small request; a host that has not answered by then is not going to.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// A pairing session lives ten minutes, so a sweep this often frees an abandoned exchange within a minute
/// of the deadline it can no longer meet.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// An unattended browser must not keep an admin session usable, and no session outlives the day it was
/// opened on.
const SESSION_ABSOLUTE: Duration = Duration::from_secs(43_200);
const SESSION_IDLE: Duration = Duration::from_secs(900);

/// Builds every component from the configuration, serves the proxy and the admin listener until `shutdown`
/// resolves, then drains.
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
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_task.clone(),
        clock.clone(),
        DISCOVERY_TIMEOUT,
    ));
    let scheduler = catalog.clone().spawn_scheduler(Duration::from_secs(
        config.routing.catalog_sync_interval_seconds,
    ));
    let relay = Arc::new(
        Relay::new(
            profile,
            store.clone(),
            manager.clone(),
            pool.clone(),
            router_task.clone(),
            resolver,
            clock.clone(),
            Limits {
                cooldown_fallback: Duration::from_secs(config.routing.cooldown_fallback_seconds),
                head_timeout: HEAD_TIMEOUT,
                hold_timeout: HOLD_TIMEOUT,
                idle_timeout: IDLE_TIMEOUT,
                max_attempts: config.routing.max_attempts,
                max_request_bytes: MAX_REQUEST_BYTES,
            },
        )
    );
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
        dev_loopback_http: config.admin.dev_loopback_http,
        origin: config.admin.origin.clone(),
        vault: vault.clone(),
        started_at: (clock)(),
        router: router_task.clone(),
        catalog: catalog.clone(),
        body_capture_max_hours: config.stats.body_capture_max_hours,
        session_expiry: Duration::from_secs(config.sessions.expire_after_seconds),
    });

    let proxy_listener = tokio::net::TcpListener::bind(config.proxy.listen)
        .await
        .with_context(|| format!("binding proxy.listen {}", config.proxy.listen))?;
    let admin_listener = tokio::net::TcpListener::bind(config.admin.listen)
        .await
        .with_context(|| format!("binding admin.listen {}", config.admin.listen))?;
    tracing::info!(
        proxy = %config.proxy.listen,
        admin = %config.admin.listen,
        profile = profile.name,
        "listening"
    );

    // The admin routes carry their full `/admin/...` paths, so they are merged rather than nested. The admin
    // router has no fallback of its own, so an unknown path would otherwise reach axum's bare 404, outside
    // the gate that answers for every admin route.
    let admin_app = Router::new()
        .merge(phoebis_admin::router(admin))
        .fallback(unknown_admin_route);
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
    let mut proxy = tokio::spawn(
        axum::serve(proxy_listener, phoebis_relay::router(relay.clone()))
            .with_graceful_shutdown(stopped(proxy_stop))
            .into_future(),
    );
    let mut admin_server = tokio::spawn(
        axum::serve(admin_listener, admin_app)
            .with_graceful_shutdown(stopped(stop_rx))
            .into_future(),
    );

    shutdown.await;
    tracing::info!("shutting down");
    let _ = stop.send(true);
    // Held rather than returned: a listener that failed still leaves the routing writer to drain and two
    // tasks to abort, so the teardown below runs either way and the error is the last thing that happens.
    let served = match tokio::time::timeout(DRAIN_TIMEOUT, async {
        tokio::join!(&mut proxy, &mut admin_server)
    })
    .await
    {
        Ok((proxy_result, admin_result)) => drained(proxy_result).and(drained(admin_result)),
        Err(_) => {
            tracing::warn!(
                seconds = DRAIN_TIMEOUT.as_secs(),
                "requests were still in flight when the drain window closed"
            );
            proxy.abort();
            admin_server.abort();
            Ok(())
        }
    };
    scheduler.abort();
    sweeper.abort();
    // Idle upstream connections live in the pool, so nothing closes them until the last holder lets go.
    drop(relay);
    drop(catalog);
    // The router task is the last writer: a pin or a cooldown the relay acknowledged reaches the store while
    // this waits, rather than dying with the process.
    if let Err(error) = router_task.shutdown().await {
        tracing::warn!(%error, "the router task had already stopped");
    }
    drop(pool);
    served
}

/// One listener's outcome: the task either panicked or the serve future itself failed.
fn drained(result: Result<std::io::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
    Ok(result??)
}

fn resolver_for(upstream: &UpstreamConfig) -> Resolver {
    if let Some(addr) = upstream.resolve_override {
        // Every host goes to the one address, which is how a lab upstream stands in for the real one.
        return Arc::new(move |_| Some(addr));
    }
    // Blocking name resolution: the contract is a synchronous closure, and the control plane and the relay
    // ask for a name once per new connection rather than per request. The port travels with the authority
    // the caller passes in; a bare host defaults to 443.
    Arc::new(|authority: &str| {
        let (host, port) = match split_authority(authority) {
            Some(split) => split,
            // A bracketed v6 literal carries its brackets here too, and no resolver takes them.
            None => (
                authority.trim_start_matches('[').trim_end_matches(']'),
                HTTPS_PORT,
            ),
        };
        (host, port).to_socket_addrs().ok()?.next()
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

/// Unauthenticated callers learn nothing about the admin surface from a path that does not exist, and no
/// cache keeps the answer.
async fn unknown_admin_route() -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        [(CONTENT_TYPE, "application/json")],
        r#"{"error":"no admin session"}"#,
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn stopped(mut stop: tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
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

    #[test]
    fn the_production_resolver_honours_an_explicit_port_and_defaults_to_443() {
        let resolve = resolver_for(&UpstreamConfig::default());
        assert_eq!(resolve("127.0.0.1:8317").map(|a| a.port()), Some(8317));
        assert_eq!(resolve("127.0.0.1").map(|a| a.port()), Some(443));
        assert_eq!(resolve("[::1]:8443").map(|a| a.port()), Some(8443));
        assert_eq!(resolve("[::1]").map(|a| a.port()), Some(443));
        assert!(resolve("no-such-host.invalid").is_none());
    }
}
