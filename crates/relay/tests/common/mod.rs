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
use phoebis_bus::Bus;
use phoebis_oauth::Clock;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{router, CatalogService, Limits, OriginalHead, Relay, RelayOutcome};
use phoebis_routing::{spawn, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, Identity, NewBundle, OveragePolicy, PoolRow,
    Secret, StorageKey, Store, Strategy, SubStrategy,
};
use phoebis_transport::request::parse_head;
use fake_upstream::{FakeUpstream, Respond};
// The polls live with the fake so every integration suite has them; the call sites here name them as before.
#[allow(unused_imports)]
pub use fake_upstream::{stays_for, wait_for};

pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";
pub const KEY: &str = "phoebis-testkey";
pub const NOW: i64 = 1_000_000;

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
    pub allowed_models: Option<Vec<&'static str>>,
    pub cache_diagnostics: Option<bool>,
    pub cache_ttl: &'static str,
    pub migrate_above_utilization: Option<f64>,
    pub overage_policy: OveragePolicy,
    pub strategy: Strategy,
}

impl Default for PoolSpec {
    fn default() -> Self {
        PoolSpec {
            affinity_ttl_seconds: 3600,
            allowed_models: None,
            cache_diagnostics: None,
            cache_ttl: "{}",
            migrate_above_utilization: None,
            overage_policy: OveragePolicy::PreferPlan,
            strategy: Strategy::PriorityRandom,
        }
    }
}

pub struct Harness {
    pub app: axum::Router,
    pub catalog: Arc<CatalogService>,
    pub clock: Arc<AtomicI64>,
    /// The relay behind `app`, for the suites that serve it on a real socket instead of calling it in process.
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    pub store: Arc<Store>,
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
        &["claude-sonnet-4-5", "claude-opus-5"],
        limits,
    )
    .await
    .app
}

pub async fn harness(up: &FakeUpstream, accounts: &[TestAccount], pool: PoolSpec) -> Harness {
    harness_with(
        up,
        accounts,
        pool,
        &["claude-sonnet-4-5", "claude-opus-5"],
        default_limits(),
    )
    .await
}

/// Every account serves `models`; the key is `KEY` in pool `main`; the clock starts at `NOW` and is shared
/// with the router task so a test can move time.
pub async fn harness_with(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    models: &[&str],
    limits: Limits,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    harness_on(store, up, accounts, pool, models, limits).await
}

/// The same builder over a store the caller already opened, so a test can reopen an on-disk store and see its
/// rows survive. Seeding accounts, the pool, and the key is skipped once the store already has accounts, and
/// the model seed is skipped when the caller wants a bare catalog for its own sync to fill in.
pub async fn harness_on(
    store: Arc<Store>,
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    models: &[&str],
    limits: Limits,
) -> Harness {
    build(store, up, accounts, pool, models, limits, None, false, None).await
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
        &["claude-sonnet-4-5", "claude-opus-5"],
        default_limits(),
        None,
        false,
        Some(cap),
    )
    .await
}

/// The default harness with the bootstrap-level `cache_diagnostics` option set, for the cases that check how it
/// interacts with a pool's own override.
pub async fn harness_with_cache_diagnostics_default(
    up: &FakeUpstream,
    accounts: &[TestAccount],
    pool: PoolSpec,
    cache_diagnostics: bool,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    build(
        store,
        up,
        accounts,
        pool,
        &["claude-sonnet-4-5", "claude-opus-5"],
        default_limits(),
        None,
        cache_diagnostics,
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
        &["claude-sonnet-4-5", "claude-opus-5"],
        limits,
        Some(bus),
        false,
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
    models: &[&str],
    limits: Limits,
    bus: Option<Bus>,
    cache_diagnostics: bool,
    memory_cap: Option<usize>,
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
                    default_allowed_models: pool
                        .allowed_models
                        .map(|l| l.into_iter().map(String::from).collect()),
                    keepalive_json: "{}".into(),
                    body_capture_hours: 0,
                    prefer_subscription: false,
                    api_key_sub_strategy: SubStrategy::PriorityRandom,
                    cache_ttl_json: pool.cache_ttl.into(),
                    cache_diagnostics: pool.cache_diagnostics,
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
                    .map(|a| (a.id.to_string(), a.priority))
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
                    allowed_models: None,
                    keepalive_opt_out: false,
                    body_capture_hours: None,
                    secret_digest: store.api_key_digest(KEY).to_vec(),
                    prefix: phoebis_store::key_prefix(KEY),
                    created_at: NOW,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                },
                "test",
            )
            .unwrap();
    }
    if !models.is_empty() {
        for account in accounts {
            store
                .apply_catalog_sync(
                    account.id,
                    &models
                        .iter()
                        .map(|m| (m.to_string(), false))
                        .collect::<Vec<_>>(),
                    None,
                    NOW,
                    "test",
                )
                .unwrap();
        }
    }

    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let mut b = phoebis_transport::connector_builder(p, phoebis_transport::Plane::Inference).unwrap();
    trust_fake_ca(&mut b, up);
    let addr = up.addr();
    // An authority with an explicit port names another loopback fake on that port; everything else is `up`.
    let resolver: phoebis_transport::control::Resolver = Arc::new(move |authority: &str| {
        authority
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .map(|p| std::net::SocketAddr::from(([127, 0, 0, 1], p)))
            .or(Some(addr))
    });
    let mut cb = phoebis_transport::connector_builder(p, phoebis_transport::Plane::Control).unwrap();
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
    let router_handle = spawn(store.clone(), None, clock.clone(), RouterConfig::default()).unwrap();
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_handle.clone(),
        clock.clone(),
        limits.head_timeout,
    ));
    let relay = Relay::new(
        store.clone(),
        manager,
        pool,
        router_handle.clone(),
        resolver,
        clock,
        limits,
    );
    let relay = relay
        .with_cache_diagnostics(cache_diagnostics);
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
    let relay = Arc::new(relay);
    Harness {
        app: router(relay.clone()),
        catalog,
        clock: clock_cell,
        relay,
        router: router_handle,
        store,
    }
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
        // The configuration's own default, so a test that wants the cap to bite asks for it.
        body_capture_max_hours: 72,
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        hold_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(600),
        max_attempts: 3,
        max_request_bytes: 32 << 20,
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

pub fn json_200() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: b"{\"ok\":true}".to_vec(),
    }
}

pub fn outcome(resp: &axum::http::Response<Body>) -> RelayOutcome {
    resp.extensions()
        .get::<RelayOutcome>()
        .expect("a relayed response carries its outcome")
        .clone()
}

pub fn credentials_seen(up: &FakeUpstream) -> Vec<String> {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/messages"))
        .filter_map(|r| r.credential().map(String::from))
        .collect()
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
