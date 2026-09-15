//! The running process: the store and the credential manager the two listeners share, and the drain that
//! ends them.

use std::future::{Future, IntoFuture};
use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use phoebis_admin::{AdminSecrets, AdminState, PairingVault, Sessions};
use phoebis_oauth::{Clock, CredentialManager, PairingSessions, UsageService};
use phoebis_relay::{KeyTable, Limits, Relay, Route};
use phoebis_store::{AccountRow, AccountState, Secret, StorageKey, Store};
use phoebis_transport::control::{ControlClient, Resolver};
use phoebis_transport::pool::Pool;
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};

use crate::config::{AccountConfig, AccountKind, Config};

/// How long a shutdown waits for relays that are still running. A stream that has not ended by then is not
/// going to end sooner because the process keeps waiting.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The native client gives a request ten minutes (`X-Stainless-Timeout: 600`), so neither the wait for the
/// response head nor the wait between stream events gives up before the caller itself would.
const HEAD_TIMEOUT: Duration = Duration::from_secs(600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

const HTTPS_PORT: u16 = 443;

/// Well past the largest measured request; a body beyond it is refused rather than buffered.
const MAX_REQUEST_BYTES: usize = 32 << 20;

/// An upstream connection idle for longer than this is likely to have been closed at the other end already,
/// and reusing it costs a failed write and a retry.
const POOL_IDLE_TTL: Duration = Duration::from_secs(60);
const POOL_MAX_IDLE: usize = 8;

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
    // The store names the database file inside the directory it is given, so the configured path
    // contributes its parent.
    let dir = config.storage.path.parent().unwrap_or(Path::new("."));
    let store = Arc::new(
        Store::open(dir, &key)
            .with_context(|| format!("opening the store under {}", dir.display()))?,
    );
    sync_accounts(&store, &config)?;

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
    let resolver = resolver_for(&config);
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
    // The key table refuses two routes that share a secret on its own. `load` refuses the same file
    // earlier, and deliberately so: neither check is allowed to depend on the other existing.
    let keys = KeyTable::from_routes(&routes(&config))?;
    let relay = Arc::new(Relay::new(
        profile,
        store.clone(),
        manager.clone(),
        pool.clone(),
        keys,
        resolver,
        Limits {
            max_request_bytes: MAX_REQUEST_BYTES,
            head_timeout: HEAD_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
        },
    ));
    let usage = Arc::new(UsageService::new(
        manager.clone(),
        control.clone(),
        profile,
        Duration::from_secs(config.usage.cache_ttl_seconds),
        config.usage.refresh_concurrency,
        clock.clone(),
    ));
    // The provider sends the operator's browser back here after an authorization; behind TLS that is the
    // configured origin, and in loopback development mode the admin listener itself.
    let redirect_base = config
        .admin
        .origin
        .clone()
        .unwrap_or_else(|| format!("http://{}", config.admin.listen));
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
        redirect_base,
        vault: Arc::new(PairingVault::new()),
        started_at: (clock)(),
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

    // The admin routes carry their full `/admin/...` paths, so they are merged rather than nested; the
    // fallback below replaces the one that does not survive a merge.
    let admin_app = Router::new()
        .merge(phoebis_admin::router(admin))
        .fallback(unknown_admin_route);
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
    match tokio::time::timeout(DRAIN_TIMEOUT, async {
        tokio::join!(&mut proxy, &mut admin_server)
    })
    .await
    {
        Ok((proxy_result, admin_result)) => {
            proxy_result??;
            admin_result??;
        }
        Err(_) => {
            tracing::warn!(
                seconds = DRAIN_TIMEOUT.as_secs(),
                "requests were still in flight when the drain window closed"
            );
            proxy.abort();
            admin_server.abort();
        }
    }
    // Idle upstream connections live in the pool, so nothing closes them until the last holder lets go.
    drop(relay);
    drop(pool);
    Ok(())
}

/// Brings the configured accounts into the registry. An account the registry already holds keeps the row it
/// has: pairing, credentials, and lifecycle generation live there, and no configuration reload may reset
/// them. The API-key secret is the one thing configuration owns outright, so it is written every time.
fn sync_accounts(store: &Store, config: &Config) -> anyhow::Result<()> {
    for account in &config.accounts {
        match store.account(&account.id)? {
            Some(stored) => report_drift(&stored, account),
            None => store.insert_account(&AccountRow {
                id: account.id.clone(),
                kind: account.kind.store_kind(),
                label: account.label.clone(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                // An API key is usable as soon as it is stored; an OAuth slot has to be paired first.
                state: match account.kind {
                    AccountKind::ApiKey => AccountState::Ready,
                    AccountKind::Oauth => AccountState::Unpaired,
                },
            })?,
        }
        if let Some(secret) = &account.secret {
            store.set_api_key_secret(&account.id, secret)?;
        }
    }
    Ok(())
}

/// Says out loud which value is in force when the file and the registry disagree about an account, since
/// the registry's row is the one every request is served under.
fn report_drift(stored: &AccountRow, configured: &AccountConfig) {
    if stored.kind != configured.kind.store_kind() {
        tracing::warn!(
            account = %stored.id,
            stored = ?stored.kind,
            "the configured account kind differs from the registry, which keeps its own"
        );
    }
    if stored.label != configured.label {
        tracing::warn!(
            account = %stored.id,
            stored = %stored.label,
            "the configured label differs from the registry, which keeps its own"
        );
    }
}

fn routes(config: &Config) -> Vec<Route> {
    config
        .routes
        .iter()
        .map(|route| Route {
            key_id: route.key_id.clone(),
            secret: Secret::new(route.secret.expose()),
            account_id: route.account_id.clone(),
        })
        .collect()
}

fn resolver_for(config: &Config) -> Resolver {
    if let Some(addr) = config.upstream.resolve_override {
        // Every host goes to the one address, which is how a lab upstream stands in for the real one.
        return Arc::new(move |_| Some(addr));
    }
    // Blocking name resolution: the contract is a synchronous closure, and the control plane and the relay
    // ask for a name once per new connection rather than per request.
    Arc::new(|host: &str| (host, HTTPS_PORT).to_socket_addrs().ok()?.next())
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
