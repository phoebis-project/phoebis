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
use phoebis_oauth::Clock;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{router, Limits, Relay, RelayOutcome};
use phoebis_routing::{spawn, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, Identity, NewBundle, OveragePolicy, PoolRow,
    Secret, StorageKey, Store, Strategy,
};
use phoebis_transport::request::parse_head;
use fake_upstream::{FakeUpstream, Respond};

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

pub struct PoolSpec {
    pub affinity_ttl_seconds: u64,
    pub allowed_models: Option<Vec<&'static str>>,
    pub migrate_above_utilization: Option<f64>,
    pub overage_policy: OveragePolicy,
    pub strategy: Strategy,
}

impl Default for PoolSpec {
    fn default() -> Self {
        PoolSpec {
            affinity_ttl_seconds: 3600,
            allowed_models: None,
            migrate_above_utilization: None,
            overage_policy: OveragePolicy::PreferPlan,
            strategy: Strategy::PriorityRandom,
        }
    }
}

pub struct Harness {
    pub app: axum::Router,
    pub clock: Arc<AtomicI64>,
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
    let p = claude_code_2_1_269_linux_x64();
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
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
        store
            .apply_catalog_sync(
                account.id,
                &models
                    .iter()
                    .map(|m| (m.to_string(), false))
                    .collect::<Vec<_>>(),
                NOW,
                "test",
            )
            .unwrap();
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
                default_allowed_models: pool
                    .allowed_models
                    .map(|l| l.into_iter().map(String::from).collect()),
                keepalive_json: "{}".into(),
                body_capture_hours: 0,
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
            },
            "test",
        )
        .unwrap();

    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let mut b = phoebis_transport::connector_builder(p, phoebis_transport::Plane::Inference).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
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
    phoebis_transport::trust_extra_ca(&mut cb, up.ca_pem()).unwrap();
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
    let relay = Arc::new(Relay::new(
        p,
        store.clone(),
        manager,
        pool,
        router_handle.clone(),
        resolver,
        clock,
        limits,
    ));
    Harness {
        app: router(relay),
        clock: clock_cell,
        router: router_handle,
        store,
    }
}

/// Bounds no test is meant to reach unless it asks for them.
pub fn default_limits() -> Limits {
    Limits {
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        hold_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(600),
        max_attempts: 3,
        max_request_bytes: 32 << 20,
    }
}

pub fn request_from_fixture(head: &[u8], body: &[u8], key: &str) -> Request<Body> {
    let (line, headers) = parse_head(head).unwrap();
    let target = line.split(' ').nth(1).unwrap();
    let mut req = Request::builder().method("POST").uri(target);
    for (n, v) in headers {
        // The caller presents the router's key, not the upstream credential; Host and Content-Length are transport-owned.
        match n.to_ascii_lowercase().as_str() {
            "authorization" => {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
            "x-api-key" => {
                req = req.header("x-api-key", key);
            }
            "host" | "content-length" => {}
            _ => {
                req = req.header(n, v);
            }
        }
    }
    req.body(Body::from(body.to_vec())).unwrap()
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
    req.headers_mut()
        .insert("x-claude-code-session-id", session.parse().unwrap());
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
    req.headers_mut()
        .insert("x-claude-code-session-id", session.parse().unwrap());
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

/// Waits for something a background task of the router or the fake produces. The deadline is generous because
/// it is only ever reached when the thing never happens, and the poll is short so a passing test does not pay
/// for it.
pub async fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let give_up = std::time::Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the expected state never arrived within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
