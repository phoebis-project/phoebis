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
use phoebis_store::{AccountRow, AccountState, Secret, StorageKey, Store};
use phoebis_transport::control::{ControlClient, Resolver};
use phoebis_transport::pool::Pool;
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};

use crate::config::{AccountKind, Config};

/// How long a shutdown waits for relays that are still running. A stream that has not ended by then is not
/// going to end sooner because the process keeps waiting.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an account is cooled when the upstream rejected it without saying for how long.
const COOLDOWN_FALLBACK: Duration = Duration::from_secs(60);

/// The native client gives a request ten minutes (`X-Stainless-Timeout: 600`), so neither the wait for the
/// response head nor the wait between stream events gives up before the caller itself would.
const HEAD_TIMEOUT: Duration = Duration::from_secs(600);
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// A stream that has not reached its first committing event by then is forwarded as it stands, because the
/// caller is waiting and a longer hold buys no better verdict.
const HOLD_TIMEOUT: Duration = Duration::from_secs(30);

/// Upstream submissions one caller request may cost across every account it tries.
const MAX_ATTEMPTS: u8 = 3;

const HTTPS_PORT: u16 = 443;

/// Well past the largest measured request; a body beyond it is refused rather than buffered.
const MAX_REQUEST_BYTES: usize = 32 << 20;

/// An upstream connection idle for longer than this is likely to have been closed at the other end already,
/// and reusing it costs a failed write and a retry.
const POOL_IDLE_TTL: Duration = Duration::from_secs(60);
const POOL_MAX_IDLE: usize = 8;

/// The ceiling on a key's body-capture window, so no capture can be left recording for weeks.
const BODY_CAPTURE_MAX_HOURS: u32 = 72;

/// How long a session pin stays listed on `GET /admin/sessions` after its last use.
const SESSION_EXPIRY: Duration = Duration::from_secs(86_400);

/// A models list is one small request; a host that has not answered by then is not going to.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

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
        profile,
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_task.clone(),
        clock.clone(),
        DISCOVERY_TIMEOUT,
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
                cooldown_fallback: COOLDOWN_FALLBACK,
                head_timeout: HEAD_TIMEOUT,
                hold_timeout: HOLD_TIMEOUT,
                idle_timeout: IDLE_TIMEOUT,
                max_attempts: MAX_ATTEMPTS,
                max_request_bytes: MAX_REQUEST_BYTES,
            },
        )
    );
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
        vault: Arc::new(PairingVault::new()),
        started_at: (clock)(),
        router: router_task,
        catalog,
        body_capture_max_hours: BODY_CAPTURE_MAX_HOURS,
        session_expiry: SESSION_EXPIRY,
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

/// Brings the configured accounts into the registry: the file owns an account's kind, label,
/// and API-key secret, and the registry owns everything pairing writes, so identity, credentials, state, and
/// the lifecycle generation survive every reload. A slot the file no longer names keeps its row, since
/// deleting it would take a paired account's credentials with it.
fn sync_accounts(store: &Store, config: &Config) -> anyhow::Result<()> {
    for account in &config.accounts {
        match store.account(&account.id)? {
            Some(_) => store.update_account_config(
                &account.id,
                account.kind.store_kind(),
                &account.label,
            )?,
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
            // A slot the file has just moved from OAuth to an API key has everything it needs the moment its
            // key is stored, but it still carries the unpaired state the OAuth row was created with, which
            // the relay refuses and which no pairing will ever clear for an API-key account.
            let unpaired = store
                .account(&account.id)?
                .is_some_and(|row| matches!(row.state, AccountState::Unpaired));
            if matches!(account.kind, AccountKind::ApiKey) && unpaired {
                store.set_state(&account.id, AccountState::Ready)?;
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::StorageKey;
    use std::path::Path;

    fn configured_as(dir: &Path, label: &str) -> Config {
        let yaml = format!(
            r#"accounts:
  - id: console
    kind: api_key
    label: {label}
    secret_env: TEST_SYNC_CONSOLE_KEY
admin:
  listen: 127.0.0.1:0
  secret_env: TEST_SYNC_ADMIN_SECRET
  dev_loopback_http: true
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:0
routes:
  - key_id: desktop
    secret_env: TEST_SYNC_DESKTOP_KEY
    account_id: console
storage:
  encryption_key_file: {key}
  path: {db}
upstream:
usage:
  cache_ttl_seconds: 60
  refresh_concurrency: 2
"#,
            key = dir.join("storage.key").display(),
            db = dir.join("router.sqlite").display()
        );
        let path = dir.join(format!("{label}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        crate::config::load(&path).unwrap()
    }

    #[test]
    fn a_renamed_account_keeps_its_row_and_its_stored_key() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_SYNC_CONSOLE_KEY", "sk-ant-api03-PLACEHOLDER");
        std::env::set_var("TEST_SYNC_ADMIN_SECRET", "admin-secret");
        std::env::set_var("TEST_SYNC_DESKTOP_KEY", "phoebis-testkey");
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        sync_accounts(&store, &configured_as(dir.path(), "Console")).unwrap();
        sync_accounts(&store, &configured_as(dir.path(), "Renamed")).unwrap();
        let rows = store.accounts().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Renamed");
        assert_eq!(
            store.api_key_secret("console").unwrap().unwrap().expose(),
            "sk-ant-api03-PLACEHOLDER"
        );
    }

    #[test]
    fn an_unpaired_slot_reconfigured_as_an_api_key_is_ready_once_its_key_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_SYNC_CONSOLE_KEY", "sk-ant-api03-PLACEHOLDER");
        std::env::set_var("TEST_SYNC_ADMIN_SECRET", "admin-secret");
        std::env::set_var("TEST_SYNC_DESKTOP_KEY", "phoebis-testkey");
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        store
            .insert_account(&AccountRow {
                id: "console".into(),
                kind: phoebis_store::AccountKind::Oauth,
                label: "Console".into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                state: AccountState::Unpaired,
            })
            .unwrap();
        sync_accounts(&store, &configured_as(dir.path(), "Console")).unwrap();
        let row = store.account("console").unwrap().unwrap();
        assert_eq!(row.kind, phoebis_store::AccountKind::ApiKey);
        assert_eq!(
            row.state,
            AccountState::Ready,
            "nothing else would ever move an api_key slot out of unpaired"
        );
    }
}
