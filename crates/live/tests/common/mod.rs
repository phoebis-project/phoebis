//! The relay harness with the bus and the live feed attached, plus a loopback WebSocket server over the feed.
#![allow(dead_code)]

use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::Request;
use axum::routing::get;
use axum::Router;
use fake_upstream::FakeUpstream;
use phoebis_bus::Bus;
use phoebis_ledger::{PriceTable, Pricing};
use phoebis_live::{LiveConfig, LiveEvent, LiveFeed, SessionCheck};
use phoebis_oauth::Clock;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{router, CatalogService, Limits, Relay};
use phoebis_routing::{spawn as spawn_router, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Affinity, AffinitySource, ApiKeyRow,
    Failover, Identity, MemberWrite, NewBundle, OveragePolicy, PoolRow, QuotaHeaders, RulesMode,
    Secret, Shedding, StorageKey, Store, Strategy,
};
use phoebis_transport::request::parse_head;
// The polls live with the fake so every integration suite has them; only some of them are used here.
#[allow(unused_imports)]
pub use fake_upstream::{stays_for, wait_for};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

pub const KEY: &str = "phoebis-testkey";
pub const NOW: i64 = 1_000_000;
pub const A: &str = "sk-ant-oat01-AAAA";

pub struct Harness {
    /// The flag the ws route's session check reads; a test flips it to simulate the session ending. `true` by
    /// default, so a test that never touches it gets an ordinary connection.
    pub alive: Arc<AtomicBool>,
    pub app: Router,
    pub bus: Bus,
    pub clock: Arc<AtomicI64>,
    pub feed: LiveFeed,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    pub ws_addr: std::net::SocketAddr,
}

#[derive(serde::Deserialize)]
struct Preview {
    preview: Option<bool>,
}

/// One OAuth account `a` serving `claude-sonnet-4-5` in pool `main` with `capture_hours`; the key is `KEY`.
pub async fn harness(up: &FakeUpstream, capture_hours: u32) -> Harness {
    let p = claude_code_2_1_269_linux_x64();
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: Some(Identity {
                account_uuid: "00000000-0000-4000-8000-000000000100".into(),
                email: "a@example.invalid".into(),
                organization_uuid: None,
            }),
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new(A),
                refresh_token: Some(Secret::new("r")),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    store
        .insert_pool(
            &PoolRow {
                id: "main".into(),
                label: "main".into(),
                enabled: true,
                strategy: Strategy::Priority,
                affinity_ttl_seconds: 3600,
                overage_policy: OveragePolicy::PreferPlan,
                migrate_above_utilization: None,
                select_below_utilization: None,
                model_rules: Vec::new(),
                keepalive_json: "{}".into(),
                body_capture_hours: capture_hours,
                prefer_subscription: false,
                shedding: Shedding::default(),
                failover: Failover::default(),
                cache_ttl_json: "{}".into(),
                cache_diagnostics: None,
                auto_mode_server: true,
                affinity: Affinity::default(),
                quota_headers: QuotaHeaders::Pass,
                created_at: NOW,
                updated_at: NOW,
            },
            "test",
        )
        .unwrap();
    store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
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
                in_flight_cap: None,
                affinity_source: AffinitySource::Auto,
                auto_mode_server: None,
            },
            "test",
        )
        .unwrap();
    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let mut b =
        phoebis_transport::connector_builder(p, phoebis_transport::Plane::Inference).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: phoebis_transport::control::Resolver = Arc::new(move |_: &str| {
        Box::pin(async move { Some(addr) }) as phoebis_transport::control::ResolveFuture
    });
    let mut cb =
        phoebis_transport::connector_builder(p, phoebis_transport::Plane::Control).unwrap();
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
        Duration::from_secs(60),
        Duration::from_secs(5),
    ));
    let bus = Bus::new();
    // The shipped table prices `claude-sonnet-5`, not the fixture's `claude-sonnet-4-5`; the tests assert a cost.
    let mut table = PriceTable::shipped();
    table.models.insert(
        "claude-sonnet-4-5".into(),
        table.models["claude-sonnet-5"].clone(),
    );
    let feed = LiveFeed::spawn(
        &bus,
        Arc::new(Pricing::new(table)),
        clock.clone(),
        LiveConfig {
            tick: Duration::from_millis(200),
        },
    );
    let limits = Limits {
        body_capture_max_hours: 72,
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        hold_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(600),
        max_request_bytes: 32 << 20,
        model_rejection: Duration::from_secs(3_600),
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
        .with_bus(bus.clone()),
    );
    let ws_feed = feed.clone();
    let alive = Arc::new(AtomicBool::new(true));
    let route_alive = alive.clone();
    let ws_app = Router::new()
        .route(
            "/live",
            get(
                move |State(feed): State<LiveFeed>,
                      Query(q): Query<Preview>,
                      ws: WebSocketUpgrade| {
                    let alive = route_alive.clone();
                    async move {
                        let slot = feed.slot().expect("room");
                        let check: SessionCheck = Box::new(move || alive.load(Ordering::SeqCst));
                        ws.on_upgrade(move |socket| async move {
                            feed.attach(socket, q.preview.unwrap_or(true), check, slot)
                                .await
                        })
                    }
                },
            ),
        )
        .with_state(ws_feed);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, ws_app).into_future());
    Harness {
        alive,
        app: router(relay),
        bus,
        clock: clock_cell,
        feed,
        router: router_handle,
        store,
        ws_addr,
    }
}

pub fn request_for(key: &str, model: &str, session: &str) -> Request<Body> {
    let native = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen(
            r#""model":"claude-sonnet-4-5""#,
            &format!(r#""model":"{model}""#),
            1,
        );
    let (line, headers) = parse_head(inference_head(FixtureKind::OauthNonFirstParty)).unwrap();
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

pub type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn connect(h: &Harness, preview: bool) -> Client {
    let (client, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/live?preview={preview}", h.ws_addr))
            .await
            .unwrap();
    client
}

/// The next JSON message, decoded as the wire envelope.
pub async fn next_event(client: &mut Client) -> serde_json::Value {
    let message = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("a message within 5 s")
        .expect("the socket is open")
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected a text frame, got {message:?}")
    };
    serde_json::from_str(&text).unwrap()
}

/// Deltas until one of `kind` for `id` arrives, returned with every delta before it. A `tick` carries no id
/// and matches on its type alone; the whole wait is under one deadline, so a stream of ticks cannot keep it
/// going for ever.
pub async fn until_delta(client: &mut Client, id: &str, kind: &str) -> Vec<serde_json::Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut seen = Vec::new();
        loop {
            let event = next_event(client).await;
            assert_eq!(event["kind"], "delta", "{event}");
            let delta = event["delta"].clone();
            let done = delta["type"] == kind
                && (kind == "tick" || delta["id"] == id || delta["request"]["id"] == id);
            seen.push(delta);
            if done {
                return seen;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} delta for {id:?} within 10 s"))
}

pub async fn send_close(client: &mut Client) {
    let _ = client.send(Message::Close(None)).await;
}

pub fn request_id(resp: &axum::http::Response<Body>) -> String {
    resp.extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .unwrap()
        .request_id
        .clone()
}

pub fn assert_live_event_shape(event: &serde_json::Value) {
    let _: LiveEvent = serde_json::from_value(event.clone()).expect("the wire shape round-trips");
}
