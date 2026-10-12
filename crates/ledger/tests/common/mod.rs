//! The relay harness with the observation bus and the ledger attached, over an in-memory store.
#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fake_upstream::FakeUpstream;
use phoebis_bus::Bus;
use phoebis_ledger::{
    spawn, Ledger, LedgerConfig, LedgerDb, Metrics, PlanTables, PriceRevisions, Pricing,
};
use phoebis_oauth::Clock;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{router, Accounting, CatalogService, Limits, Relay, UpstreamGzip};
use phoebis_routing::{spawn as spawn_router, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, ApiKeyRow, Identity,
    MemberWrite, NewBundle, PoolRow, RulesMode, Secret, StateCause, StorageKey, Store, StoreError,
    Strategy,
};
use phoebis_transport::request::parse_head;
// The polls live with the fake so every integration suite has them; only some of them are used here.
#[allow(unused_imports)]
pub use fake_upstream::{capture_tracing, stays_for, stays_for_async, wait_for, wait_for_async};

pub const KEY: &str = "phoebis-testkey";
pub const NOW: i64 = 1_000_000;
pub const A: &str = "sk-ant-oat01-AAAA";
pub const B: &str = "sk-ant-oat01-BBBB";
pub const C: &str = "sk-ant-api03-CCCC";

pub struct Harness {
    pub app: axum::Router,
    pub bus: Bus,
    pub clock: Arc<AtomicI64>,
    pub db: Arc<LedgerDb>,
    pub ledger: Ledger,
    pub metrics: Arc<Metrics>,
    /// The table the writer prices with, which the admin's confirmation replaces in place.
    pub pricing: Arc<Pricing>,
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    pub usage: Option<Arc<phoebis_oauth::UsageService>>,
}

pub struct Account {
    pub id: &'static str,
    pub kind: AccountKind,
    pub secret: &'static str,
    pub priority: u32,
}

pub fn oauth(id: &'static str, secret: &'static str, priority: u32) -> Account {
    Account {
        id,
        kind: AccountKind::Oauth,
        secret,
        priority,
    }
}

pub fn api_key(id: &'static str, secret: &'static str, priority: u32) -> Account {
    Account {
        id,
        kind: AccountKind::ApiKey,
        secret,
        priority,
    }
}

/// `capture` is (pool hours, key override); `models` every account serves; the key is `KEY` in pool `main`.
pub async fn harness(
    up: &FakeUpstream,
    accounts: &[Account],
    capture: (u32, Option<u32>),
    config: LedgerConfig,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    harness_on(store, db, up, accounts, capture, config, false).await
}

/// `harness` with a `UsageService` built over the same manager and control connector, so a test can run the
/// startup step or the poll's scheduler over it.
pub async fn harness_with_poll(
    up: &FakeUpstream,
    accounts: &[Account],
    capture: (u32, Option<u32>),
    config: LedgerConfig,
) -> Harness {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    harness_on(store, db, up, accounts, capture, config, true).await
}

/// `with_usage` builds the harness's `UsageService`, and `false` builds none.
pub async fn harness_on(
    store: Arc<Store>,
    db: Arc<LedgerDb>,
    up: &FakeUpstream,
    accounts: &[Account],
    capture: (u32, Option<u32>),
    config: LedgerConfig,
    with_usage: bool,
) -> Harness {
    let p = claude_code_2_1_269_linux_x64();
    if store.accounts().unwrap().is_empty() {
        for (n, account) in accounts.iter().enumerate() {
            let identity = matches!(account.kind, AccountKind::Oauth).then(|| Identity {
                account_uuid: format!("00000000-0000-4000-8000-0000000001{n:02}"),
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
                    state_cause: StateCause::Operator,
                })
                .unwrap();
            match account.kind {
                AccountKind::Oauth => store
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
                    .map(|_| ())
                    .unwrap(),
                AccountKind::ApiKey => store
                    .set_api_key_secret(account.id, &Secret::new(account.secret))
                    .unwrap(),
            }
        }
        store
            .insert_pool(
                &PoolRow {
                    strategy: Strategy::Priority,
                    body_capture_hours: capture.0,
                    prefer_subscription: false,
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
                    keepalive_opt_out: false,
                    body_capture_hours: capture.1,
                    secret_digest: store.api_key_digest(KEY).to_vec(),
                    prefix: phoebis_store::key_prefix(KEY),
                    incarnation: 1,
                    created_at: NOW,
                    updated_at: 0,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                    auto_mode_server: None,
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
    if !up.ca_pem().is_empty() {
        phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    }
    let addr = up.addr();
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
    if !up.ca_pem().is_empty() {
        phoebis_transport::trust_extra_ca(&mut cb, up.ca_pem()).unwrap();
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
    // The quota poll's own `UsageService`, over the same manager and control connector, so a fetch it makes
    // reaches the same fake upstream every other control call in this harness does.
    let usage = with_usage.then(|| {
        Arc::new(phoebis_oauth::UsageService::new(
            manager.clone(),
            control,
            p,
            4,
            clock.clone(),
        ))
    });
    let pool = Arc::new(phoebis_transport::pool::Pool::new(
        b.build(),
        4,
        Duration::from_secs(60),
    ));
    let router_handle =
        spawn_router(store.clone(), clock.clone(), RouterConfig::default()).unwrap();
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_handle.clone(),
        clock.clone(),
        Duration::from_secs(10),
        Duration::from_secs(5),
    ));
    let bus = Bus::new();
    let pricing = Arc::new(Pricing::new(PriceRevisions::shipped()));
    let metrics = Arc::new(Metrics::new());
    let ledger = spawn(
        db.clone(),
        pricing.clone(),
        Arc::new(PlanTables::shipped()),
        store.clone(),
        &bus,
        router_handle.clone(),
        clock.clone(),
        metrics.clone(),
        config,
    );
    let limits = Limits {
        body_budget_bytes: 512 << 20,
        body_capture_max_hours: 72,
        body_read_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        hold_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(600),
        key_body_reads: 128,
        max_request_bytes: 32 << 20,
        model_rejection: Duration::from_secs(3_600),
        upstream_gzip: UpstreamGzip::Auto,
    };
    let relay = Arc::new(
        Relay::new(
            store.clone(),
            manager,
            pool,
            router_handle.clone(),
            resolver,
            clock,
            limits,
            catalog,
        )
        .with_bus(bus.clone())
        .with_accounting({
            let ledger = ledger.clone();
            Arc::new(move |counts: bool| Accounting {
                behind: if counts {
                    ledger.admission().behind
                } else {
                    ledger.behind()
                },
            })
        }),
    );
    Harness {
        app: router(relay.clone()),
        bus,
        clock: clock_cell,
        db,
        ledger,
        metrics,
        pricing,
        relay,
        router: router_handle,
        store,
        usage,
    }
}

/// A `Clock` reading the same cell `harness_on` seeded, built the same way it builds its own: the harness
/// stores only the raw `AtomicI64` cell, so a caller needing the closure form wraps it here.
pub fn clock_of(h: &Harness) -> Clock {
    let c = h.clock.clone();
    Arc::new(move || c.load(Ordering::SeqCst))
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

/// The head fixture captured together with a body fixture: a first-party body checked against a non-first-
/// party head's declared betas trips client-drift detection.
fn head_of(fixture: &BodyFixture) -> FixtureKind {
    match fixture {
        BodyFixture::ApiKey => FixtureKind::ApiKey,
        BodyFixture::CountApiKey => FixtureKind::CountApiKey,
        BodyFixture::CountOauth => FixtureKind::CountOauthNonFirstParty,
        BodyFixture::CountOauthFirstParty => FixtureKind::CountOauthFirstParty,
        BodyFixture::FirstParty9b244 | BodyFixture::FirstPartyE142e => FixtureKind::OauthFirstParty,
        BodyFixture::Oauth => FixtureKind::OauthNonFirstParty,
    }
}

pub fn request_for(key: &str, model: &str, session: &str) -> Request<Body> {
    request_for_fixture(BodyFixture::Oauth, key, model, session)
}

/// `request_for` fixed to the OAuth fixture; this is the same request over any other body fixture, model
/// substituted the same way.
pub fn request_for_fixture(
    fixture: BodyFixture,
    key: &str,
    model: &str,
    session: &str,
) -> Request<Body> {
    let head = head_of(&fixture);
    let native = String::from_utf8(body(fixture).to_vec()).unwrap().replacen(
        r#""model":"claude-sonnet-4-5""#,
        &format!(r#""model":"{model}""#),
        1,
    );
    let (line, headers) = parse_head(inference_head(head)).unwrap();
    let target = line.split(' ').nth(1).unwrap();
    let mut req = Request::builder().method("POST").uri(target);
    for (n, v) in headers {
        match n.to_ascii_lowercase().as_str() {
            "authorization" => req = req.header("Authorization", format!("Bearer {key}")),
            "host" | "content-length" | "x-claude-code-session-id" => {}
            _ => req = req.header(n, v),
        }
    }
    req.header("x-claude-code-session-id", session)
        .body(Body::from(native.into_bytes()))
        .unwrap()
}

/// The ledger row for a request, once the writer has landed it.
pub async fn ledgered(
    h: &Harness,
    id: &str,
) -> (
    phoebis_ledger::RequestRecord,
    Vec<phoebis_ledger::AttemptRecord>,
) {
    wait_for_async(Duration::from_secs(5), || async {
        h.db.request(id).await.unwrap()
    })
    .await
}

pub fn json_200() -> fake_upstream::Respond {
    fake_upstream::Respond::Json {
        status: 200,
        headers: vec![],
        body: br#"{"id":"msg_1","type":"message","model":"claude-sonnet-4-5","stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":4,"output_tokens_details":{"thinking_tokens":1},"server_tool_use":{"web_search_requests":1,"web_fetch_requests":0},"speed":"standard","inference_geo":"us"}}"#
            .to_vec(),
    }
}
