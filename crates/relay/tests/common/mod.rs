//! Builders shared by the relay's fixture, streaming, and failover tests.
//!
//! Each integration test binary compiles this module separately, so a helper only one binary uses looks dead to
//! the other.
#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::Bus;
use phoebis_oauth::Clock;
use phoebis_profile::{
    claude_code_2_1_310_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{
    router, AccountingSource, CatalogService, Limits, OriginalHead, Relay, RelayOutcome,
    ShutdownMark, UpstreamGzip,
};
use phoebis_routing::{
    lane_key, resolve, spawn, ClientClass, DecisionRecord, DecisionSummary, RefusalFacts, Report,
    RouterConfig, RouterEvent, RouterHandle, SelectRequest, SelectResult,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountUpdate, ApiKeyRow, CompanionPermissions, Failover, Identity,
    KeyFeatureAccounts, MemberWrite, ModelRule, NewBundle, OveragePolicy, PoolFeatureAccounts,
    PoolRow, RulesMode, Secret, StorageKey, Store, StoreError, Strategy,
};
use phoebis_transport::request::parse_head;
use tokio::sync::broadcast;
use tower::ServiceExt;
// The polls live with the fake so every integration suite has them; the call sites here name them as before.
#[allow(unused_imports)]
pub use fake_upstream::{stays_for, wait_for, wait_for_async};

pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";
/// The Claude Code CLI's user agent, so a test whose point is not the client class needs no shape of its own.
pub const CLAUDE_CLI_USER_AGENT: &str = "claude-cli/2.1.269 (external, sdk-cli)";
/// The host of the connectors' sessions, which the harness of `harness_with_connector_host` sends to its second fake.
pub const CONNECTOR_HOST: &str = "mcp-proxy.anthropic.com";
/// The session id the first-party fixture's head carries.
pub const FIRST_PARTY_SESSION: &str = "11111111-1111-4111-8111-111111111111";
pub const KEY: &str = "phoebis-testkey";
/// A raw Messages caller, which the gate refuses before the body is read.
pub const SDK_USER_AGENT: &str = "anthropic-sdk-python/0.40.0";

pub const NOW: i64 = 1_000_000;
/// The startup bound every harness passes to `CatalogService::new`, short enough for a test to wait past it.
pub const STARTUP_BOUND: Duration = Duration::from_millis(500);

pub fn user(text: &str) -> serde_json::Value {
    serde_json::json!({"role": "user", "content": text})
}

/// A user message whose one text block carries a `cache_control` marker, so the body holds a marker of the
/// client's own.
pub fn user_with_marker(text: &str) -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": [{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}],
    })
}

pub fn assistant(text: &str) -> serde_json::Value {
    serde_json::json!({"role": "assistant", "content": text})
}

/// The `metadata.user_id` string Claude Code sends behind the router: a device id, an empty `account_uuid` (the
/// client holds no OAuth account behind a router key) and the session, escaped as the client escapes it.
pub fn user_id_for(session: &str) -> String {
    serde_json::json!({"device_id": "test-device", "account_uuid": "", "session_id": session})
        .to_string()
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

/// A session id no other call in this process answered: `test-session-<n>` from a process-wide counter, so two
/// cases of one suite never share a pin.
pub fn fresh_session() -> String {
    format!(
        "test-session-{}",
        NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
    )
}

/// `body` with `BARE_PLACEHOLDER_LINE` as `system[0]` where `system` is an array whose first block carries no
/// attribution line, and `body` as it is otherwise (no `system`, a string `system`, or a line already in front):
/// the contract requires the line of every inference body whose `system` is an array. The line names the agent's
/// version bare, so the suffix check has no suffix to judge, and its placeholder stamp is the one the hash check
/// exempts. A case whose subject is the hash of its `system` array computes the hash over this value, which is
/// the body `claude_code_request` sends.
pub fn with_attribution_line(body: &serde_json::Value) -> serde_json::Value {
    let mut value = body.clone();
    if let Some(blocks) = value.get_mut("system").and_then(|s| s.as_array_mut()) {
        let carries_line = blocks
            .first()
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.starts_with("x-anthropic-billing-header: "));
        if !carries_line {
            blocks.insert(
                0,
                serde_json::json!({"type": "text", "text": BARE_PLACEHOLDER_LINE}),
            );
        }
    }
    value
}

/// A Claude Code request on the API-key fixture head carrying `body` under `key` and `session`: the head's
/// `claude-cli/` user agent and the class header `main` that `request_from_fixture` adds, the session header set
/// to `session`, `metadata.user_id` from `user_id_for` where the body carries no `metadata.user_id` of its own,
/// and the attribution line in front of a `system` array through `with_attribution_line`. A case whose subject
/// is not the client's class meets the contract as the client does.
pub fn claude_code_request(key: &str, session: &str, body: &serde_json::Value) -> Request<Body> {
    let mut value = with_attribution_line(body);
    if value.pointer("/metadata/user_id").is_none() {
        value["metadata"]["user_id"] = serde_json::Value::from(user_id_for(session));
    }
    let bytes = serde_json::to_vec(&value).expect("a Value always encodes");
    let mut req = request_from_fixture(inference_head(FixtureKind::ApiKey), &bytes, key);
    set_header(&mut req, "x-claude-code-session-id", session);
    req
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
        identity,
        ..AccountRow::for_tests(account.id, account.kind)
    }
}

pub struct PoolSpec {
    pub affinity_ttl_seconds: u64,
    pub auto_mode_server: bool,
    pub cache_diagnostics: bool,
    pub cache_ttl: &'static str,
    pub failover: Failover,
    /// The pool's account for each claude.ai feature, none for both by default.
    pub feature_accounts: PoolFeatureAccounts,
    /// The harness key's own override of `auto_mode_server`; `None` inherits the pool's.
    pub key_auto_mode_server: Option<bool>,
    /// The harness key's own value for each feature, inherit for both by default.
    pub key_feature_accounts: KeyFeatureAccounts,
    pub model_rules: Vec<ModelRule>,
    pub overage_policy: OveragePolicy,
    pub strategy: Strategy,
    pub thinking_diagnostics: bool,
}

impl Default for PoolSpec {
    fn default() -> Self {
        PoolSpec {
            affinity_ttl_seconds: 3600,
            auto_mode_server: true,
            cache_diagnostics: false,
            cache_ttl: "{}",
            failover: Failover::default(),
            feature_accounts: PoolFeatureAccounts::default(),
            key_auto_mode_server: None,
            key_feature_accounts: KeyFeatureAccounts::default(),
            model_rules: Vec::new(),
            overage_policy: OveragePolicy::PreferPlan,
            strategy: Strategy::Priority,
            thinking_diagnostics: false,
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
    /// The client the feature listener's control-plane calls leave through.
    pub control: Arc<phoebis_transport::control::ControlClient>,
    /// The manager the relay was built with, so a test reads `backoff_until` before it steps the clock.
    pub manager: Arc<phoebis_oauth::CredentialManager>,
    /// The relay behind `app`, for the suites that serve it on a real socket instead of calling it in process.
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    /// Aborts the periodic refresh loop on drop, ahead of `_stop` so no tick still due starts a fill after the
    /// fills below are canceled.
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
    one_account_harness(kind, up, limits).await.app
}

/// The one-account relay `relay_with_limits` answers, with its whole harness, for a case that reads the relay's counts.
pub async fn one_account_harness(kind: AccountKind, up: &FakeUpstream, limits: Limits) -> Harness {
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
        None,
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

/// A fake that plays `CONNECTOR_HOST`, its leaf naming that host, answering as `rules` say.
pub async fn connector_host_fake(rules: Vec<Rule>) -> FakeUpstream {
    FakeUpstream::start_tls_for(rules, &[CONNECTOR_HOST]).await
}

/// `harness_with` with a second fake, one of `connector_host_fake`: the resolver sends `CONNECTOR_HOST` to it, and
/// both connectors trust its CA beside `up`'s.
pub async fn harness_with_connector_host(
    up: &FakeUpstream,
    connector_host: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    limits: Limits,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
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
        None,
        Some(connector_host),
    )
    .await
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
        None,
    )
    .await
}

/// `harness_with_bus_limits` with the router's shutdown mark attached, for the cases that set it.
pub async fn harness_with_shutdown_mark(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    bus: Bus,
    limits: Limits,
    mark: ShutdownMark,
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
        Some(mark),
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
        None,
        None,
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
        None,
        None,
    )
    .await
}

/// `harness_with_accounting` over `pool` and `limits`, for the feature listener's calls while the ledger reads behind.
pub async fn harness_with_pool_and_accounting(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    limits: Limits,
    source: AccountingSource,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
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
        Some(source),
        None,
        None,
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
    harness_with_catalog_and_head_timeout(
        up,
        catalog_up,
        accounts,
        pool,
        default_limits().head_timeout,
    )
    .await
}

/// `harness_with_catalog` under the caller's head timeout, which the catalog's discovery takes as its own
/// bound: a case that waits out a stalled models endpoint ends in that time rather than in the default's ten
/// seconds.
pub async fn harness_with_catalog_and_head_timeout(
    up: &FakeUpstream,
    catalog_up: FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    head_timeout: Duration,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        Limits {
            head_timeout,
            ..default_limits()
        },
        None,
        false,
        None,
        Some(catalog_up),
        RouterConfig::default(),
        None,
        None,
        None,
    )
    .await
}

/// `harness_on_with_bus`, with the caller's own catalog fake in place of `catalog_rules()`'s: for a suite (like
/// keepalive's) built around a session's pin and account continuity, where a spelling resolving to a dated id
/// mid-test is not what the suite is about. `accounting`, when named, is the ledger's standing admission reads, and
/// the router task runs on `router_config`.
#[allow(clippy::too_many_arguments)]
pub async fn harness_on_with_bus_and_catalog(
    store: Arc<Store>,
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    bus: Bus,
    catalog_up: FakeUpstream,
    accounting: Option<AccountingSource>,
    router_config: RouterConfig,
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
        Some(catalog_up),
        router_config,
        accounting,
        None,
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
        None,
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
    shutdown_mark: Option<ShutdownMark>,
    connector_host: Option<&FakeUpstream>,
) -> Harness {
    let p = claude_code_2_1_310_linux_x64();
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
                    identity,
                    ..AccountRow::for_tests(account.id, account.kind)
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
                    strategy: pool.strategy,
                    affinity_ttl_seconds: pool.affinity_ttl_seconds,
                    overage_policy: pool.overage_policy,
                    model_rules: pool.model_rules,
                    prefer_subscription: false,
                    failover: pool.failover,
                    cache_ttl_json: pool.cache_ttl.into(),
                    cache_diagnostics: pool.cache_diagnostics,
                    thinking_diagnostics: pool.thinking_diagnostics,
                    auto_mode_server: pool.auto_mode_server,
                    feature_accounts: pool.feature_accounts,
                    created_at: NOW,
                    updated_at: NOW,
                    ..PoolRow::for_tests("main")
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
                    companion_permissions: CompanionPermissions::default(),
                    body_capture_hours: None,
                    secret_digest: store.api_key_digest(KEY).to_vec(),
                    prefix: phoebis_store::key_prefix(KEY),
                    incarnation: 1,
                    created_at: NOW,
                    updated_at: 0,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                    auto_mode_server: pool.key_auto_mode_server,
                    feature_accounts: pool.key_feature_accounts,
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
    let connector_addr = connector_host.map(FakeUpstream::addr);
    // An authority with an explicit port names another loopback fake on that port, the connectors' host names
    // `connector_host` where there is one, and everything else is `up`.
    let resolver: phoebis_transport::control::Resolver = Arc::new(move |authority: &str| {
        let resolved = authority
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .map(|p| std::net::SocketAddr::from(([127, 0, 0, 1], p)))
            .or(connector_addr.filter(|_| authority == CONNECTOR_HOST))
            .or(Some(addr));
        Box::pin(async move { resolved }) as phoebis_transport::control::ResolveFuture
    });
    let mut cb =
        phoebis_transport::connector_builder(p, phoebis_transport::Plane::Control).unwrap();
    trust_fake_ca(&mut cb, up);
    // Both connectors, so a forward that sent the connectors' host on the wrong plane still reaches its fake.
    if let Some(second) = connector_host {
        trust_fake_ca(&mut b, second);
        trust_fake_ca(&mut cb, second);
    }
    let control = Arc::new(phoebis_transport::control::ControlClient::new(
        p,
        cb.build(),
        resolver.clone(),
    ));
    let manager = Arc::new(phoebis_oauth::CredentialManager::new(
        store.clone(),
        control.clone(),
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
    let relay = match shutdown_mark {
        Some(mark) => relay.with_shutdown_mark(mark),
        None => relay,
    };
    let relay = Arc::new(relay);
    Harness {
        app: router(relay.clone()),
        catalog: catalog.clone(),
        catalog_up,
        clock: clock_cell,
        control,
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

/// A label edit through the one account write, fenced on the revision the caller read. The incarnation and the
/// generation are the row's own at call time, so only the revision half of the admin route's fence can refuse here.
pub fn rename(
    h: &Harness,
    id: &str,
    revision: u64,
    label: &str,
    actor: &str,
) -> Result<(), StoreError> {
    let (incarnation, generation) = h
        .store
        .account(id)?
        .map_or((0, 0), |row| (row.incarnation, row.lifecycle_generation));
    h.store.commit_account_update(&AccountUpdate {
        id,
        expected_revision: revision,
        incarnation,
        lifecycle_generation: generation,
        label,
        secret: None,
        state: None,
        at: h.clock.load(Ordering::SeqCst),
        actor,
        enabled: None,
        cancellation: None,
    })
}

/// A fake serving plain HTTP has no certificate to trust, and `X509::from_pem` refuses the empty string.
fn trust_fake_ca(builder: &mut boring::ssl::SslConnectorBuilder, up: &FakeUpstream) {
    if !up.ca_pem().is_empty() {
        phoebis_transport::trust_extra_ca(builder, up.ca_pem()).unwrap();
    }
}

/// The feature module on a loopback port, served through `serve_features` under a pair of the test's own making
/// for both feature hosts.
pub struct FeatureListener {
    pub addr: std::net::SocketAddr,
    pair: fake_upstream::GeneratedPair,
    /// Dropped with the listener, which ends its accept loop.
    _stop: tokio::sync::watch::Sender<bool>,
}

pub async fn feature_listener(h: &Harness) -> FeatureListener {
    let pair = fake_upstream::generate_pair(&["api.anthropic.com", "mcp-proxy.anthropic.com"]);
    let acceptor = phoebis_relay::LiveAcceptor::new(fake_upstream::acceptor_for(&pair));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_relay::serve_features(
        listener,
        h.relay.clone(),
        h.control.clone(),
        &claude_code_2_1_310_linux_x64().features,
        stop_rx,
        phoebis_relay::ListenLimits {
            drain_timeout: Duration::from_secs(30),
            header_read_timeout: Duration::from_secs(30),
            max_connections: 64,
        },
        Arc::new(acceptor),
    ));
    FeatureListener {
        addr,
        pair,
        _stop: stop,
    }
}

impl FeatureListener {
    /// Sends `request`, the bytes of one HTTP/1.1 request, on a new TLS connection that names `host`, and answers
    /// the status, the head's lines as sent and the body. The answer is read to the connection's end, so `request`
    /// carries `Connection: close`, and a chunked body comes back in its framing.
    pub async fn send(&self, host: &str, request: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        self.hold(host, request).await.answer().await
    }

    /// Writes `request` on a new TLS connection that names `host` and hands the connection back open, so a head that
    /// declares more body than `request` carries keeps the router reading for as long as the test holds it.
    pub async fn hold(&self, host: &str, request: &[u8]) -> HeldRequest {
        use tokio::io::AsyncWriteExt;

        let socket = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let mut tls =
            tokio_boring::connect(fake_upstream::connector_trusting(&self.pair), host, socket)
                .await
                .unwrap();
        tls.write_all(request).await.unwrap();
        HeldRequest(tls)
    }

    /// Sends `request` as `send` does and answers once the head arrived: the status, the head's lines as sent, and
    /// the body still on the connection.
    pub async fn open(&self, host: &str, request: &[u8]) -> (u16, Vec<(String, String)>, OpenBody) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let socket = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let mut tls =
            tokio_boring::connect(fake_upstream::connector_trusting(&self.pair), host, socket)
                .await
                .unwrap();
        tls.write_all(request).await.unwrap();
        let mut read = Vec::new();
        let split = loop {
            if let Some(split) = read.windows(4).position(|window| window == b"\r\n\r\n") {
                break split;
            }
            let mut piece = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(30), tls.read(&mut piece))
                .await
                .expect("the answer's head stalled")
                .unwrap_or(0);
            assert!(
                n > 0,
                "the connection ended before the answer's head: {:?}",
                String::from_utf8_lossy(&read)
            );
            read.extend_from_slice(&piece[..n]);
        };
        let head = String::from_utf8_lossy(&read[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {head:?}"));
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
            .collect();
        let body = OpenBody {
            tls,
            left: read[split + 4..].to_vec(),
        };
        (status, headers, body)
    }
}

/// A connection the test wrote a request on and keeps open. Dropping it closes the connection.
pub struct HeldRequest(tokio_boring::SslStream<tokio::net::TcpStream>);

impl HeldRequest {
    /// The status, the head's lines as sent and the body of the answer, read to the connection's end.
    pub async fn answer(mut self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        use tokio::io::AsyncReadExt;

        let mut answer = Vec::new();
        // A peer that closes without a TLS close_notify ends the read with an error after the whole answer.
        let _ = tokio::time::timeout(Duration::from_secs(30), self.0.read_to_end(&mut answer))
            .await
            .expect("the answer stalled before the connection ended");
        let split = answer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no answer head in {:?}", String::from_utf8_lossy(&answer)));
        let head = String::from_utf8_lossy(&answer[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {head:?}"));
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
            .collect();
        (status, headers, answer[split + 4..].to_vec())
    }
}

/// The body of an answer whose head was read. Dropping it closes the caller's connection.
pub struct OpenBody {
    tls: tokio_boring::SslStream<tokio::net::TcpStream>,
    /// What the head's read took of the body, handed out first.
    left: Vec<u8>,
}

impl OpenBody {
    /// The next piece of the body as it arrives, in its framing, `None` at its end.
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;

        if !self.left.is_empty() {
            return Some(std::mem::take(&mut self.left));
        }
        let mut piece = [0u8; 4096];
        // A peer that closes without a TLS close_notify ends the read with an error, which is the body's end too.
        let n = tokio::time::timeout(Duration::from_secs(30), self.tls.read(&mut piece))
            .await
            .expect("the next piece of the body stalled")
            .unwrap_or(0);
        (n > 0).then(|| piece[..n].to_vec())
    }
}

/// Bounds no test is meant to reach unless it asks for them.
pub fn default_limits() -> Limits {
    Limits {
        body_budget_bytes: 512 << 20,
        // The configuration's own default, so a test that wants the cap to bite asks for it.
        body_capture_max_hours: 72,
        body_read_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        cooldown_fallback: Duration::from_secs(60),
        domain_check_timeout: Duration::from_secs(30),
        domain_checks_per_source: 4,
        domain_checks_total: 16,
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
    // The client sends the class header on every inference request, so the harness adds it to a recorded head
    // that lacks it.
    let claude_code = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|agent| agent.starts_with("claude-cli/"));
    if claude_code
        && target.split('?').next() == Some("/v1/messages")
        && !req.headers().contains_key("x-claude-code-request-class")
    {
        set_header(&mut req, "x-claude-code-request-class", "main");
    }
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

/// Every `cache_control` member removed, so a policy sees a body with no marker anywhere.
pub fn without_markers(native: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(native).unwrap();
    let strip = |v: &mut serde_json::Value| {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("cache_control");
        }
    };
    for field in ["system", "tools"] {
        if let Some(items) = value.get_mut(field).and_then(|v| v.as_array_mut()) {
            for item in items {
                strip(item);
            }
        }
    }
    if let Some(messages) = value.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for message in messages {
            if let Some(content) = message.get_mut("content").and_then(|v| v.as_array_mut()) {
                for block in content {
                    strip(block);
                }
            }
        }
    }
    serde_json::to_vec(&value).unwrap()
}

/// The `tools` member removed, or set to `[]` when `empty`: a body that sends no tools list either way.
pub fn without_tools(native: &[u8], empty: bool) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(native).unwrap();
    let object = value.as_object_mut().expect("the body is a JSON object");
    if empty {
        object.insert("tools".into(), serde_json::json!([]));
    } else {
        object.remove("tools");
    }
    serde_json::to_vec(&value).unwrap()
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

static NEXT_PIN: AtomicU64 = AtomicU64::new(0);

/// Places the pin of `session`'s `agent` on `model` under the harness key through the router's own selection, asked
/// as the relay asks it for a request's first wire attempt, then ends the selection as the relay ends a request that
/// used nothing, so no booking stands. Nothing goes upstream and no bus event leaves, so a case that sends a keepalive
/// probe holds the pin the probe renews without a real request's counts. The account the selection placed it on.
pub async fn pin_session(h: &Harness, session: &str, agent: &str, model: &str) -> String {
    let snapshot = h.router.snapshot();
    let key = snapshot
        .key("desktop")
        .expect("the harness key is in the snapshot")
        .clone();
    let folded = phoebis_store::canonical_id(model);
    let canonical = h.catalog.canonical_for(&folded).await;
    let now = h.clock.load(Ordering::SeqCst);
    let request_id = format!("pin-{}", NEXT_PIN.fetch_add(1, Ordering::Relaxed));
    let answer = h
        .router
        .select(SelectRequest {
            request_id: request_id.clone(),
            attempt: 1,
            seq: 1,
            key_id: key.id.clone(),
            key_generation: key.created_at,
            agent_id: agent.into(),
            session_id: session.into(),
            candidates: resolve(&snapshot, &key, &canonical, &folded)
                .expect("the pool's model rules allow the model"),
            canonical_model: canonical.clone(),
            folded_model: folded,
            portable: true,
            credit_account: None,
            attempted: Vec::new(),
            now,
            received_at: now,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: Arc::from([]),
            marker: true,
            parent_agent_id: None,
            failed_over: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: lane_key(&canonical, "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
        })
        .await
        .expect("the router answers the selection");
    let SelectResult::Selected(selection) = answer.result else {
        panic!("the session's selection placed no pin: {:?}", answer.result)
    };
    h.router
        .report(Report::Ended {
            request_id,
            attempt: 1,
            at: now,
            cause: "canceled",
        })
        .await;
    selection.account.id
}

/// The OAuth non-first-party fixture with its `messages` replaced, so a scenario sends a real conversation turn
/// whose chains the relay reads. The fixture's two system markers and its 26 tools stay, so the diagnostics
/// member's gate holds, and it carries no billing hash and no `diagnostics` member of its own.
pub fn conversation_request(
    key: &str,
    model: &str,
    session: &str,
    messages: &[serde_json::Value],
) -> Request<Body> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body(BodyFixture::Oauth)).expect("the fixture body is valid JSON");
    value["model"] = serde_json::Value::from(model);
    value["messages"] = serde_json::Value::Array(messages.to_vec());
    let native = serde_json::to_vec(&value).expect("a Value always re-encodes");
    let mut req = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        &native,
        key,
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// A billing line in the client's placeholder spelling, which the drift check exempts from the hash it verifies.
pub const PLACEHOLDER_LINE: &str =
    "x-anthropic-billing-header: cc_version=2.1.269.abc; cc_entrypoint=sdk-py; cch=00000;";
/// The placeholder line under the version and the entrypoint `CLAUDE_CLI_USER_AGENT` names, the version spelled
/// bare: a bare version carries no suffix for the suffix check to judge, the entrypoint agrees with the agent's,
/// and the placeholder stamp is the one the hash check exempts, so a Claude Code request carrying it is admitted.
pub const BARE_PLACEHOLDER_LINE: &str =
    "x-anthropic-billing-header: cc_version=2.1.269; cc_entrypoint=sdk-cli; cch=00000;";
/// A billing line stamping a hash this build never computed over the body.
pub const UNCHECKED_LINE: &str =
    "x-anthropic-billing-header: cc_version=2.1.269.abc; cc_entrypoint=sdk-py; cch=abcde;";

/// A JSON `POST` of `body` to `path` under the router key as Claude Code sends it: the `claude-cli/` user agent
/// `CLAUDE_CLI_USER_AGENT`, a fresh session header, and the class header `main` on `/v1/messages`. The body
/// travels as given, so a case whose subject is the body's own shape writes that shape itself, answered with the
/// status, the adapter error code (empty for none) and the body text.
pub async fn send(h: &Harness, path: &str, body: &str) -> (u16, String, String) {
    let mut request = Request::post(path)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("user-agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", fresh_session());
    if path == "/v1/messages" {
        request = request.header("x-claude-code-request-class", "main");
    }
    let request = request.body(Body::from(body.to_string())).unwrap();
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

/// A non-streaming commit whose message carries `id`, so the id lands in memory synchronously within the request.
pub fn ok_with_id(id: &str) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: format!(
            r#"{{"id":"{id}","type":"message","content":[],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        )
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

/// The router's events up to and including the first one `last` accepts, under a five-second bound.
pub async fn router_events_until(
    events: &mut broadcast::Receiver<RouterEvent>,
    last: impl Fn(&RouterEvent) -> bool,
) -> Vec<RouterEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the router's event arrives")
            .expect("not lagged");
        let done = last(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

/// The decisions among `events` for `request_id`, in the order the router made them.
pub fn decisions_of(
    events: &[RouterEvent],
    request_id: &str,
) -> Vec<(DecisionSummary, DecisionRecord)> {
    events
        .iter()
        .filter_map(|event| match event {
            RouterEvent::Decision {
                summary, record, ..
            } if record.request.request_id == request_id => {
                Some(((**summary).clone(), (**record).clone()))
            }
            _ => None,
        })
        .collect()
}

/// The refusal the router recorded as the request's decision, under a five-second bound.
pub async fn recorded_refusal(
    events: &mut broadcast::Receiver<RouterEvent>,
    request_id: &str,
) -> Option<RefusalFacts> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("the request's decision arrives")
            .expect("not lagged");
        if let RouterEvent::Decision {
            summary, record, ..
        } = event
        {
            if record.request.request_id == request_id {
                return summary.refusal.clone();
            }
        }
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

pub fn token_posts(up: &FakeUpstream) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/oauth/token"))
        .count()
}

/// The token endpoint's answer to a refresh that rotates: the profile's fixture, naming `ACCOUNT_UUID`.
pub fn token_answer() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Token,
        )
        .to_vec(),
    }
}

pub fn expired() -> Respond {
    Respond::Json {
        status: 401,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#
            .to_vec(),
    }
}

/// An OAuth account the token fixture's identity matches, so a refresh that reaches the endpoint commits.
pub fn paired(id: &'static str, secret: &'static str) -> TestAccount {
    TestAccount {
        uuid: Some(ACCOUNT_UUID),
        ..oauth(id, secret, 0)
    }
}

/// The token endpoint's conclusive refusal of a refresh, which lands the reauth mark.
pub fn invalid_grant() -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
    }
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
