//! Builders shared by the relay's fixture, streaming, and failover tests.
//!
//! Each integration test binary compiles this module separately, so a helper only one binary uses looks dead to
//! the other.
#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::Bus;
use phoebis_oauth::Clock;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{
    router, AccountingSource, CatalogService, Limits, OriginalHead, Relay, RelayOutcome,
    UpstreamGzip,
};
use phoebis_routing::{spawn, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Affinity, ApiKeyRow, Failover, Identity,
    MemberWrite, ModelRule, NewBundle, OveragePolicy, PoolRow, RulesMode, Secret, Shedding,
    StorageKey, Store, Strategy,
};
use phoebis_transport::request::parse_head;
use tower::ServiceExt;
// The polls live with the fake so every integration suite has them; the call sites here name them as before.
#[allow(unused_imports)]
pub use fake_upstream::{stays_for, wait_for, wait_for_async};

pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";
/// The Claude Code CLI's user agent, so a test whose point is not the client class needs no shape of its own.
pub const CLAUDE_CLI_USER_AGENT: &str = "claude-cli/2.1.269 (external, sdk-cli)";
pub const KEY: &str = "phoebis-testkey";
/// A raw Messages caller, not Claude Code.
pub const SDK_USER_AGENT: &str = "anthropic-sdk-python/0.40.0";

pub const NOW: i64 = 1_000_000;
/// The startup bound every harness passes to `CatalogService::new`, short enough for a test to wait past it.
pub const STARTUP_BOUND: Duration = Duration::from_millis(500);

pub fn user(text: &str) -> serde_json::Value {
    serde_json::json!({"role": "user", "content": text})
}

pub fn assistant(text: &str) -> serde_json::Value {
    serde_json::json!({"role": "assistant", "content": text})
}

#[derive(Clone)]
pub struct TestAccount {
    pub id: &'static str,
    pub kind: AccountKind,
    pub secret: &'static str,
    pub priority: u32,
    pub uuid: Option<&'static str>,
}

pub fn oauth(id: &'static str, secret: &'static str, priority: u32) -> TestAccount {
    TestAccount {
        id,
        kind: AccountKind::Oauth,
        secret,
        priority,
        uuid: None,
    }
}

pub fn api_key(id: &'static str, secret: &'static str, priority: u32) -> TestAccount {
    TestAccount {
        id,
        kind: AccountKind::ApiKey,
        secret,
        priority,
        uuid: None,
    }
}

/// The row `build` inserts for a `TestAccount`, so a test can add a member to a running harness through the
/// store directly instead of restarting it with a wider account list.
pub fn test_account_row(account: &TestAccount) -> AccountRow {
    let identity = matches!(account.kind, AccountKind::Oauth).then(|| Identity {
        account_uuid: account
            .uuid
            .map(String::from)
            .unwrap_or_else(|| "00000000-0000-4000-8000-000000000199".into()),
        email: format!("{}@example.invalid", account.id),
        organization_uuid: None,
    });
    AccountRow {
        id: account.id.into(),
        kind: account.kind,
        label: account.id.into(),
        enabled: true,
        identity,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Ready,
    }
}

pub struct PoolSpec {
    pub affinity_ttl_seconds: u64,
    pub auto_mode_server: bool,
    pub cache_diagnostics: bool,
    pub cache_ttl: &'static str,
    pub failover: Failover,
    /// The harness key's own override of `auto_mode_server`; `None` inherits the pool's.
    pub key_auto_mode_server: Option<bool>,
    pub migrate_above_utilization: Option<f64>,
    pub model_rules: Vec<ModelRule>,
    pub overage_policy: OveragePolicy,
    pub strategy: Strategy,
}

impl Default for PoolSpec {
    fn default() -> Self {
        PoolSpec {
            affinity_ttl_seconds: 3600,
            auto_mode_server: true,
            cache_diagnostics: false,
            cache_ttl: "{}",
            failover: Failover::default(),
            key_auto_mode_server: None,
            migrate_above_utilization: None,
            model_rules: Vec::new(),
            overage_policy: OveragePolicy::PreferPlan,
            strategy: Strategy::Priority,
        }
    }
}

/// A member of a pool at `priority`, no budget: the shorthand every `set_pool_members` call in these tests uses.
pub fn member(account_id: &str, priority: u32) -> MemberWrite {
    MemberWrite {
        account_id: account_id.into(),
        priority,
        budget_daily_usd: None,
        budget_monthly_usd: None,
    }
}

/// `"NOT x"` builds a negated entry, anything else a plain one: the shorthand the pages use for a list.
pub fn rules(entries: &[&str]) -> Vec<ModelRule> {
    entries
        .iter()
        .map(|entry| match entry.strip_prefix("NOT ") {
            Some(pattern) => ModelRule {
                pattern: pattern.into(),
                not: true,
            },
            None => ModelRule {
                pattern: (*entry).into(),
                not: false,
            },
        })
        .collect()
}

pub struct Harness {
    pub app: axum::Router,
    pub catalog: Arc<CatalogService>,
    /// The fake the listing fetches go to, apart from `up` so a background fill never lands in a test's
    /// `up.requests()`. It has its own connection pool and trusted CA: `PoolKey` is the account and the host
    /// with no address, so a shared pool would hand either fake the other's connection.
    pub catalog_up: FakeUpstream,
    pub clock: Arc<AtomicI64>,
    /// The manager the relay was built with, so a test reads `backoff_until` before it steps the clock.
    pub manager: Arc<phoebis_oauth::CredentialManager>,
    /// The relay behind `app`, for the suites that serve it on a real socket instead of calling it in process.
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    /// Aborts the periodic refresh loop on drop, ahead of `_stop` so no tick still due starts a fill after the
    /// fills below are cancelled.
    _refresh: RefreshGuard,
    /// Cancels `catalog`'s fills and retries on drop, so a retry sleeping its backoff never outlives the test.
    _stop: CatalogGuard,
    pub store: Arc<Store>,
}

struct CatalogGuard(Arc<CatalogService>);

impl Drop for CatalogGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct RefreshGuard(tokio::task::JoinHandle<()>);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The default catalog fake: `/v1/models?limit=1000` answers two rows (`claude-sonnet-4-5-20250929` at
/// 1,000,000 input tokens and `claude-opus-5` at 1,000,000), the listing alone.
pub fn catalog_rules() -> Vec<Rule> {
    let sonnet = serde_json::json!({
        "id": "claude-sonnet-4-5-20250929",
        "display_name": "Claude Sonnet 4.5",
        "created_at": "2025-09-29",
        "max_input_tokens": 1_000_000,
        "max_tokens": 128_000,
    });
    vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::json!({
                "data": [
                    sonnet,
                    {
                        "id": "claude-opus-5",
                        "display_name": "Claude Opus 5",
                        "created_at": "2025-11-01",
                        "max_input_tokens": 1_000_000,
                        "max_tokens": 64_000,
                    },
                ],
            })
            .to_string()
            .into_bytes(),
        },
    }]
}

pub async fn relay_with(kind: AccountKind, up: &FakeUpstream) -> axum::Router {
    relay_with_limits(kind, up, default_limits()).await
}

/// The same relay with the request limit set by the caller, for the tests that have to send past it.
pub async fn relay_with_limit(
    kind: AccountKind,
    up: &FakeUpstream,
    max_request_bytes: usize,
) -> axum::Router {
    relay_with_limits(
        kind,
        up,
        Limits {
            max_request_bytes,
            ..default_limits()
        },
    )
    .await
}

/// The same relay with a head timeout a test can actually reach, for the cases about a stalled upstream.
pub async fn relay_with_head_timeout(
    kind: AccountKind,
    up: &FakeUpstream,
    head_timeout: Duration,
) -> axum::Router {
    relay_with_limits(
        kind,
        up,
        Limits {
            head_timeout,
            ..default_limits()
        },
    )
    .await
}

pub async fn relay_with_limits(
    kind: AccountKind,
    up: &FakeUpstream,
    limits: Limits,
) -> axum::Router {
    let secret = match kind {
        AccountKind::Oauth => "sk-ant-oat01-PLACEHOLDER",
        AccountKind::ApiKey => "sk-ant-api03-PLACEHOLDER",
    };
    // The one-account wrapper keeps `ACCOUNT_UUID`: the wire-equality oracles splice that exact uuid.
    harness_with(
        up,
        &[TestAccount {
            id: "acct",
            kind,
            secret,
            priority: 0,
            uuid: Some(ACCOUNT_UUID),
        }],
        PoolSpec::default(),
        limits,
    )
    .await
    .app
}

pub async fn harness(up: &FakeUpstream, accounts: &[TestAccount], pool: PoolSpec) -> Harness {
    harness_with(up, accounts, pool, default_limits()).await
}

/// The default harness with the router task on `config`, for a test that needs a share pricer or another router
/// setting of its own.
pub async fn harness_with_router_config(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    config: RouterConfig,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        None,
        false,
        None,
        None,
        config,
        None,
    )
    .await
}

/// A harness whose pool is restricted to one model, for the tests that check a routing refusal against a
/// specific model without a second account or model to route around. The pattern is a glob on the spelling: the
/// prefetch names the harness's default model by its dated snapshot before any request, so an exact entry for
/// the undated spelling would never match.
pub async fn harness_restricted_to(up: &FakeUpstream, model: &'static str) -> Harness {
    let glob = format!("{model}*");
    harness(
        up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec {
            model_rules: rules(&[&glob, "NOT *"]),
            ..PoolSpec::default()
        },
    )
    .await
}

/// The key is `KEY` in pool `main`; the clock starts at `NOW` and is shared with the router task so a test
/// can move time. The catalog cache reads its own fake, started from `catalog_rules()`.
pub async fn harness_with(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    limits: Limits,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    harness_on(store, up, accounts, pool, limits).await
}

/// `harness_with`, named for the case that only needs to override `Limits`; kept as its own name since every
/// caller already reads that way.
pub async fn harness_with_limits(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    limits: Limits,
) -> Harness {
    harness_with(up, accounts, pool, limits).await
}

/// The same builder over a store the caller already opened, so a test can reopen an on-disk store and see its
/// rows survive. Seeding accounts, the pool, and the key is skipped once the store already has accounts.
pub async fn harness_on(
    store: Arc<Store>,
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    limits: Limits,
) -> Harness {
    build(
        store,
        up,
        accounts,
        pool,
        limits,
        None,
        false,
        None,
        None,
        RouterConfig::default(),
        None,
    )
    .await
}

/// `harness_on` with a bus attached, for a reopened store whose requests a test reads off the bus.
pub async fn harness_on_with_bus(
    store: Arc<Store>,
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    bus: Bus,
) -> Harness {
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        Some(bus),
        false,
        None,
        None,
        RouterConfig::default(),
        None,
    )
    .await
}

/// The same builder with the session memory held to `cap` entries, for the cases that need an eviction to
/// land at a point of their choosing rather than after sixty-five thousand sessions.
pub async fn harness_with_memory_cap(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    cap: usize,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        None,
        false,
        Some(cap),
        None,
        RouterConfig::default(),
        None,
    )
    .await
}

/// The default harness with `proxy.sign_cch_placeholder` on, for the cases about the restored placeholder's
/// signature.
pub async fn harness_with_signed_placeholder(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        None,
        true,
        None,
        None,
        RouterConfig::default(),
        None,
    )
    .await
}

/// The default harness with a bus attached, for the suites that read what the relay publishes.
pub async fn harness_with_bus(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    bus: Bus,
) -> Harness {
    harness_with_bus_limits(up, accounts, pool, bus, default_limits()).await
}

/// The same with the relay's limits set by the caller, for the cases that have to reach one.
pub async fn harness_with_bus_limits(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    bus: Bus,
    limits: Limits,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        limits,
        Some(bus),
        false,
        None,
        None,
        RouterConfig::default(),
        None,
    )
    .await
}

/// The default harness with the ledger's standing read through `source`, for the admission refusals.
pub async fn harness_with_accounting(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    source: AccountingSource,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        PoolSpec::default(),
        default_limits(),
        None,
        false,
        None,
        None,
        RouterConfig::default(),
        Some(source),
    )
    .await
}

/// `harness_with_bus_limits` with the ledger's standing read through `source`, for a case that reads what the
/// admission refusals publish.
pub async fn harness_with_bus_and_accounting(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    bus: Bus,
    limits: Limits,
    source: AccountingSource,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        PoolSpec::default(),
        limits,
        Some(bus),
        false,
        None,
        None,
        RouterConfig::default(),
        Some(source),
    )
    .await
}

/// The default harness whose catalog fake is the caller's own, for the tests that script it beyond the
/// default `catalog_rules()` answers.
pub async fn harness_with_catalog(
    up: &FakeUpstream,
    catalog_up: FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        None,
        false,
        None,
        Some(catalog_up),
        RouterConfig::default(),
        None,
    )
    .await
}

/// `harness_with_bus`, with the caller's own catalog fake in place of `catalog_rules()`'s: for a suite (like
/// keepalive's) built around a session's pin and account continuity, where a spelling resolving to a dated id
/// mid-test is not what the suite is about.
pub async fn harness_with_bus_and_catalog(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    bus: Bus,
    catalog_up: FakeUpstream,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        Some(bus),
        false,
        None,
        Some(catalog_up),
        RouterConfig::default(),
        None,
    )
    .await
}

/// `build` over a store the caller already opened and seeded, for the restart and roll-back tests that check a
/// resolution carried across the store the harness is handed rather than the one it would have opened itself.
pub async fn harness_with_store_and_catalog(
    store: Arc<Store>,
    up: &FakeUpstream,
    catalog_up: FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
) -> Harness {
    build(
        store,
        up,
        accounts,
        pool,
        default_limits(),
        None,
        false,
        None,
        Some(catalog_up),
        RouterConfig::default(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn build(
    store: Arc<Store>,
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    limits: Limits,
    bus: Option<Bus>,
    sign_placeholder: bool,
    memory_cap: Option<usize>,
    catalog_up: Option<FakeUpstream>,
    router_config: RouterConfig,
    accounting: Option<AccountingSource>,
) -> Harness {
    let p = claude_code_2_1_269_linux_x64();
    if store.accounts().unwrap().is_empty() {
        for (n, account) in accounts.iter().enumerate() {
            // Every extra account gets its own uuid; the one-account wrapper is the only caller that names one.
            let uuid = account
                .uuid
                .map(String::from)
                .unwrap_or_else(|| format!("00000000-0000-4000-8000-0000000001{n:02}"));
            let identity = matches!(account.kind, AccountKind::Oauth).then(|| Identity {
                account_uuid: uuid,
                email: format!("{}@example.invalid", account.id),
                organization_uuid: None,
            });
            store
                .insert_account(&AccountRow {
                    id: account.id.into(),
                    kind: account.kind,
                    label: account.id.into(),
                    enabled: true,
                    identity,
                    lifecycle_generation: 1,
                    incarnation: 1,
                    revision: 1,
                    state: AccountState::Ready,
                })
                .unwrap();
            match account.kind {
                // The refresh token is seeded because the 401 replay tests drive a real refresh through an account.
                AccountKind::Oauth => {
                    store
                        .commit_credentials(
                            account.id,
                            1,
                            1,
                            None,
                            None,
                            &NewBundle {
                                access_token: Secret::new(account.secret),
                                refresh_token: Some(Secret::new(&format!(
                                    "{}-refresh",
                                    account.secret
                                ))),
                                issued_at: NOW,
                                expires_at: i64::MAX / 2,
                                refresh_expires_at: None,
                                scopes: "user:inference".into(),
                            },
                        )
                        .unwrap();
                }
                AccountKind::ApiKey => {
                    store
                        .set_api_key_secret(account.id, &Secret::new(account.secret))
                        .unwrap();
                }
            }
        }
        store
            .insert_pool(
                &PoolRow {
                    id: "main".into(),
                    label: "main".into(),
                    enabled: true,
                    strategy: pool.strategy,
                    affinity_ttl_seconds: pool.affinity_ttl_seconds,
                    overage_policy: pool.overage_policy,
                    migrate_above_utilization: pool.migrate_above_utilization,
                    select_below_utilization: None,
                    model_rules: pool.model_rules,
                    keepalive_json: "{}".into(),
                    body_capture_hours: 0,
                    prefer_subscription: false,
                    shedding: Shedding::default(),
                    failover: pool.failover,
                    cache_ttl_json: pool.cache_ttl.into(),
                    cache_diagnostics: pool.cache_diagnostics,
                    auto_mode_server: pool.auto_mode_server,
                    affinity: Affinity::default(),
                    created_at: NOW,
                    updated_at: NOW,
                },
                "test",
            )
            .unwrap();
        store
            .set_pool_members(
                "main",
                &accounts
                    .iter()
                    .map(|a| MemberWrite {
                        account_id: a.id.to_string(),
                        priority: a.priority,
                        budget_daily_usd: None,
                        budget_monthly_usd: None,
                    })
                    .collect::<Vec<_>>(),
                "test",
            )
            .unwrap();
        store
            .insert_api_key(
                &ApiKeyRow {
                    id: "desktop".into(),
                    label: "desktop".into(),
                    notes: None,
                    pool_id: "main".into(),
                    enabled: true,
                    revoked_at: None,
                    expires_at: None,
                    model_rules: Vec::new(),
                    model_rules_mode: RulesMode::Extend,
                    long_context_twins: true,
                    keepalive_opt_out: false,
                    body_capture_hours: None,
                    secret_digest: store.api_key_digest(KEY).to_vec(),
                    prefix: phoebis_store::key_prefix(KEY),
                    created_at: NOW,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                    auto_mode_server: pool.key_auto_mode_server,
                },
                "test",
            )
            .unwrap();
    }

    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let mut b =
        phoebis_transport::connector_builder(p, phoebis_transport::Plane::Inference).unwrap();
    trust_fake_ca(&mut b, up);
    let addr = up.addr();
    // An authority with an explicit port names another loopback fake on that port; everything else is `up`.
    let resolver: phoebis_transport::control::Resolver = Arc::new(move |authority: &str| {
        let resolved = authority
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .map(|p| std::net::SocketAddr::from(([127, 0, 0, 1], p)))
            .or(Some(addr));
        Box::pin(async move { resolved }) as phoebis_transport::control::ResolveFuture
    });
    let mut cb =
        phoebis_transport::connector_builder(p, phoebis_transport::Plane::Control).unwrap();
    trust_fake_ca(&mut cb, up);
    let control = Arc::new(phoebis_transport::control::ControlClient::new(
        p,
        cb.build(),
        resolver.clone(),
    ));
    let manager = Arc::new(phoebis_oauth::CredentialManager::new(
        store.clone(),
        control,
        p,
        clock.clone(),
    ));
    let pool = Arc::new(phoebis_transport::pool::Pool::new(
        b.build(),
        4,
        Duration::from_secs(60),
    ));
    let router_handle = spawn(store.clone(), clock.clone(), router_config).unwrap();
    // A separate connector, pool, and resolver from the relay's own: `PoolKey` is the account and the host
    // with no address, so a shared pool would hand a listing fetch an inference connection into `up`, or an
    // inference request a listing fetch's connection into `catalog_up`.
    let catalog_up = match catalog_up {
        Some(catalog_up) => catalog_up,
        None => FakeUpstream::start(catalog_rules()).await,
    };
    let mut catalog_cb =
        phoebis_transport::connector_builder(p, phoebis_transport::Plane::Inference).unwrap();
    trust_fake_ca(&mut catalog_cb, &catalog_up);
    let catalog_addr = catalog_up.addr();
    let catalog_resolver: phoebis_transport::control::Resolver = Arc::new(move |_: &str| {
        Box::pin(async move { Some(catalog_addr) }) as phoebis_transport::control::ResolveFuture
    });
    let catalog_pool = Arc::new(phoebis_transport::pool::Pool::new(
        catalog_cb.build(),
        4,
        Duration::from_secs(60),
    ));
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        catalog_pool,
        catalog_resolver,
        router_handle.clone(),
        clock.clone(),
        limits.head_timeout,
        STARTUP_BOUND,
    ));
    let relay = Relay::new(
        store.clone(),
        manager.clone(),
        pool,
        router_handle.clone(),
        resolver,
        clock,
        limits,
        catalog.clone(),
    );
    // So a harness answers like the router does: a spelling the prefetch resolved is named by its dated id
    // from the harness's first request, with no per-test fill to wait on.
    catalog.prefetch().await;
    // A tick fast enough for a test to step the clock and wait on it in real time, rather than the production
    // default's six-hour interval.
    let refresh = catalog.spawn_refresh(Duration::from_secs(21_600), Duration::from_millis(20));
    let relay = relay.with_signed_placeholder(sign_placeholder);
    // Before the store is attached, which rebuilds the memory around whatever cap it finds.
    let relay = match memory_cap {
        // The idle bound the relay's own default carries, so the cap is the only thing that differs.
        Some(cap) => relay.with_session_memory(cap, Duration::from_secs(24 * 3_600)),
        None => relay,
    };
    let relay = relay.with_memory_store(store.clone(), Duration::from_secs(30 * 86_400));
    let relay = match bus {
        Some(bus) => relay.with_bus(bus),
        None => relay,
    };
    let relay = match accounting {
        Some(source) => relay.with_accounting(source),
        None => relay,
    };
    let relay = Arc::new(relay);
    Harness {
        app: router(relay.clone()),
        catalog: catalog.clone(),
        catalog_up,
        clock: clock_cell,
        manager,
        relay,
        router: router_handle,
        _refresh: RefreshGuard(refresh),
        _stop: CatalogGuard(catalog),
        store,
    }
}

/// Moves the shared clock to the backoff the manager armed on `account` after a failed refresh, so a request held
/// on the account through that backoff goes on. The fake records a token `POST` before it answers and the manager
/// arms the backoff after the answer, so the step waits for the armed instant itself: a step taken as soon as the
/// `POST` shows could land before the arming and leave the hold waiting on a clock nothing moves.
pub async fn step_to_backoff(h: &Harness, up: &FakeUpstream, account: &str) {
    wait_for(Duration::from_secs(5), || {
        up.requests()
            .iter()
            .any(|r| r.line.starts_with("POST /v1/oauth/token"))
            .then_some(())
    })
    .await;
    let until = wait_for(Duration::from_secs(5), || {
        h.manager
            .backoff_until(account)
            .filter(|until| *until > h.clock.load(Ordering::SeqCst))
    })
    .await;
    h.clock.store(until, Ordering::SeqCst);
}

/// A fake serving plain HTTP has no certificate to trust, and `X509::from_pem` refuses the empty string.
fn trust_fake_ca(builder: &mut boring::ssl::SslConnectorBuilder, up: &FakeUpstream) {
    if !up.ca_pem().is_empty() {
        phoebis_transport::trust_extra_ca(builder, up.ca_pem()).unwrap();
    }
}

/// Bounds no test is meant to reach unless it asks for them.
pub fn default_limits() -> Limits {
    Limits {
        body_budget_bytes: 512 << 20,
        // The configuration's own default, so a test that wants the cap to bite asks for it.
        body_capture_max_hours: 72,
        body_read_timeout: Duration::from_secs(60),
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        hold_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(600),
        key_body_reads: 128,
        max_request_bytes: 32 << 20,
        model_rejection: Duration::from_secs(3_600),
        upstream_gzip: UpstreamGzip::Auto,
    }
}

/// A fixture request as it would have arrived on the proxy listener: the caller presents the router's key
/// rather than the upstream credential, and the head the recorder would have taken from the socket rides
/// along, because that record is what the outbound builder walks for order and spelling.
pub fn request_from_fixture(head: &[u8], body: &[u8], key: &str) -> Request<Body> {
    let (line, _) = parse_head(head).unwrap();
    let target = line.split(' ').nth(1).unwrap();
    let (mut original, _) = OriginalHead::parse(head).expect("a measured fixture head parses");
    for (name, value) in &mut original.headers {
        if name.eq_ignore_ascii_case("authorization") {
            *value = format!("Bearer {key}");
        } else if name.eq_ignore_ascii_case("x-api-key") {
            *value = key.to_string();
        }
    }
    let mut req = Request::builder().method("POST").uri(target);
    for (name, value) in &original.headers {
        // Host and Content-Length belong to the transport an in-process request never has; the recorded head
        // keeps both, which is what a socket would have carried.
        if !name.eq_ignore_ascii_case("host") && !name.eq_ignore_ascii_case("content-length") {
            req = req.header(name, value);
        }
    }
    let mut req = req.body(Body::from(body.to_vec())).unwrap();
    req.extensions_mut().insert(original);
    req
}

/// `body` with a per-turn control entry appended to `messages`, as client 2.1.284 sends one: a `system` entry
/// whose content is one text block and whose `output_config.effort` is `effort`.
pub fn with_control_entry(body: &[u8], effort: &str) -> Vec<u8> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body).expect("the body is valid JSON");
    value["messages"]
        .as_array_mut()
        .expect("the body carries a messages array")
        .push(serde_json::json!({
            "role": "system",
            "content": [{"type": "text", "text": "hook context"}],
            "output_config": {"effort": effort},
        }));
    serde_json::to_vec(&value).expect("a Value always re-encodes")
}

/// Sets a header on a fixture request the way a caller that sent it would have: on the map the relay reads
/// and on the recorded head the outbound builder walks, so the two never disagree about what arrived.
pub fn set_header(req: &mut Request<Body>, name: &str, value: &str) {
    req.headers_mut().insert(
        axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
        value.parse().unwrap(),
    );
    let Some(head) = req.extensions_mut().get_mut::<OriginalHead>() else {
        return;
    };
    match head
        .headers
        .iter_mut()
        .find(|(held, _)| held.eq_ignore_ascii_case(name))
    {
        Some((_, held)) => *held = value.into(),
        None => head.headers.push((name.into(), value.into())),
    }
}

/// The mirror of `set_header`, for a caller that sent none of that name: off the map and off the recorded
/// head, however many lines the fixture wrote.
pub fn remove_header(req: &mut Request<Body>, name: &str) {
    req.headers_mut().remove(name);
    if let Some(head) = req.extensions_mut().get_mut::<OriginalHead>() {
        head.headers
            .retain(|(held, _)| !held.eq_ignore_ascii_case(name));
    }
}

/// The OAuth non-first-party fixture with its model rewritten; that fixture carries no billing hash, so the
/// rewrite changes nothing the relay would have to restamp.
pub fn request_for(key: &str, model: &str, session: &str) -> Request<Body> {
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(
            r#""model":"claude-sonnet-4-5""#,
            &format!(r#""model":"{model}""#),
            1,
        );
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        key,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// A billing line in the client's placeholder spelling, which the drift check exempts from the hash it verifies.
pub const PLACEHOLDER_LINE: &str =
    "x-anthropic-billing-header: cc_version=2.1.269.abc; cc_entrypoint=sdk-py; cch=00000;";
/// A billing line stamping a hash this build never computed over the body.
pub const UNCHECKED_LINE: &str =
    "x-anthropic-billing-header: cc_version=2.1.269.abc; cc_entrypoint=sdk-py; cch=abcde;";

/// A JSON `POST` of `body` to `path` under the router key and no user agent, answered with its status, its
/// adapter error code (empty for none) and its body text.
pub async fn send(h: &Harness, path: &str, body: &str) -> (u16, String, String) {
    let request = Request::post(path)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let code = response
        .headers()
        .get("x-phoebis-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, code, String::from_utf8_lossy(&bytes).into_owned())
}

/// A `GET` on the proxy listener under the router key, for the `/v1/models` routes.
pub fn models_request(key: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("Authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap()
}

/// The profile body with `"speed":"fast"` spliced at the top level, under the session header.
pub fn fast_request(session: &str) -> Request<Body> {
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(r#"{"model""#, r#"{"speed":"fast","model""#, 1);
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        KEY,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// The same request carrying a top-level `container`, which binds it to the account that holds the container.
pub fn non_portable_request(key: &str, session: &str) -> Request<Body> {
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(r#"{"model""#, r#"{"container":"cont_1","model""#, 1);
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        native.as_bytes(),
        key,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

pub fn json_429_unified(reset_secs: i64, claim: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![
            (
                "anthropic-ratelimit-unified-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".into(),
                reset_secs.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-reset".into(),
                reset_secs.to_string(),
            ),
            (
                "anthropic-ratelimit-unified-representative-claim".into(),
                claim.into(),
            ),
        ],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#.to_vec(),
    }
}

/// A fast request's 429 for the fast lane's own limit: `retry-after` when given, and the six `anthropic-fast-*`
/// headers when the resets are given, the input family's reset first.
pub fn json_429_fast_rate_limit(
    retry_after: Option<u64>,
    fast_resets: Option<(&str, &str)>,
) -> Respond {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(seconds) = retry_after {
        headers.push(("retry-after".into(), seconds.to_string()));
    }
    if let Some((input, output)) = fast_resets {
        for (family, limit, reset) in [("input", "400000", input), ("output", "80000", output)] {
            let name = |part: &str| format!("anthropic-fast-{family}-tokens-{part}");
            headers.push((name("limit"), limit.into()));
            headers.push((name("remaining"), "0".into()));
            headers.push((name("reset"), reset.into()));
        }
    }
    Respond::Json {
        status: 429,
        headers,
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"fast limit"}}"#
            .to_vec(),
    }
}

/// A fast request's 429 for usage credits it cannot draw: the overage-disabled reason, the overage reset when
/// given, and both shared windows `allowed`, so no plan window reads as the one that rejected.
pub fn json_429_fast_credits(reason: &str, overage_reset: Option<i64>) -> Respond {
    let mut headers: Vec<(String, String)> = vec![
        (
            "anthropic-ratelimit-unified-5h-status".into(),
            "allowed".into(),
        ),
        (
            "anthropic-ratelimit-unified-7d-status".into(),
            "allowed".into(),
        ),
        (
            "anthropic-ratelimit-unified-overage-disabled-reason".into(),
            reason.into(),
        ),
    ];
    if let Some(reset) = overage_reset {
        headers.push((
            "anthropic-ratelimit-unified-overage-reset".into(),
            reset.to_string(),
        ));
    }
    Respond::Json {
        status: 429,
        headers,
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"no credits"}}"#
            .to_vec(),
    }
}

pub fn json_200() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: b"{\"ok\":true}".to_vec(),
    }
}

/// A JSON answer at `status` carrying the given headers, for a test that scripts the rate-limit bucket, the
/// unified windows, or the upstream's own identity and cookie headers on a response the relay forwards.
pub fn json_with_headers(status: u16, headers: &[(&str, &str)]) -> Respond {
    Respond::Json {
        status,
        headers: headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        body: b"{\"ok\":true}".to_vec(),
    }
}

/// A 200 JSON answer carrying the given headers, for a test that scripts the rate-limit bucket or the unified
/// windows on a response the relay still commits.
pub fn json_200_with_headers(headers: &[(&str, &str)]) -> Respond {
    json_with_headers(200, headers)
}

/// The headers a real Anthropic JSON error carries, so the pass-through test has something of the upstream's
/// own to check survived the forward.
fn error_headers() -> Vec<(String, String)> {
    vec![
        ("content-type".into(), "application/json".into()),
        ("request-id".into(), "req_fake".into()),
    ]
}

/// The answer `api.anthropic.com` gives for an id it does not serve.
pub fn json_404_model(model: &str) -> Respond {
    Respond::Json {
        status: 404,
        headers: error_headers(),
        body: serde_json::json!({
            "type": "error",
            "error": { "type": "not_found_error", "message": format!("model: {model}") },
            "request_id": "req_fake",
        })
        .to_string()
        .into_bytes(),
    }
}

/// A 404 that names no model, forwarded as it came.
pub fn json_404_plain() -> Respond {
    Respond::Json {
        status: 404,
        headers: error_headers(),
        body: serde_json::json!({
            "type": "error",
            "error": { "type": "not_found_error", "message": "Not Found" },
            "request_id": "req_fake",
        })
        .to_string()
        .into_bytes(),
    }
}

/// Waits for the bus to publish a completion and hands back every event seen up to and including it.
pub async fn drain_until_complete(sub: &mut phoebis_bus::Subscriber) -> Vec<phoebis_bus::Event> {
    let give_up = std::time::Instant::now() + Duration::from_secs(2);
    let mut all = Vec::new();
    loop {
        while let Ok(e) = sub.events.try_recv() {
            all.push(e);
        }
        if all
            .iter()
            .any(|e| matches!(e, phoebis_bus::Event::RequestComplete(_)))
        {
            return all;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "no completion was published"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn outcome(resp: &axum::http::Response<Body>) -> RelayOutcome {
    resp.extensions()
        .get::<RelayOutcome>()
        .expect("a relayed response carries its outcome")
        .clone()
}

/// Every listing request line the fake recorded, for the catalog cache's one fill path.
pub fn listings_for(up: &FakeUpstream) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with("GET /v1/models?limit=1000"))
        .count()
}

pub fn credentials_seen(up: &FakeUpstream) -> Vec<String> {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/messages"))
        .filter_map(|r| r.credential().map(String::from))
        .collect()
}

/// The `anthropic-beta` list a recorded request carried, in its order.
pub fn beta_list(r: &fake_upstream::RecordedRequest) -> Vec<String> {
    r.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.split(',').map(str::trim).map(String::from).collect())
        .unwrap_or_default()
}

pub fn wire_of(r: &fake_upstream::RecordedRequest) -> Vec<u8> {
    let mut w = r.line.clone().into_bytes();
    w.extend_from_slice(b"\r\n");
    for (n, v) in &r.headers {
        w.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
    }
    w.extend_from_slice(b"\r\n");
    w.extend_from_slice(&r.body);
    w
}

/// A streaming request as the native client sends it: the OAuth non-first-party fixture, whose body already
/// carries `"stream":true`.
pub fn stream_request(key: &str) -> Request<Body> {
    request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        key,
    )
}
