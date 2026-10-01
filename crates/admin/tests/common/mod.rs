//! Builders shared by the admin API tests.
//!
//! Each integration test binary compiles this module separately, so a helper only one binary uses looks dead to
//! the other.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use fake_upstream::{FakeUpstream, Respond, Rule};
use futures::StreamExt;
use http_body_util::BodyExt;
use phoebis_admin::{router, AdminSecrets, AdminState, PairingVault, Sessions, SettingsView};
use phoebis_alerts::{AlertParts, Alerts, Webhook};
use phoebis_bus::Bus;
use phoebis_keepalive::{Engine, EngineParts, KeepaliveLimits};
use phoebis_ledger::{
    spawn as spawn_ledger, Ledger, LedgerConfig, LedgerDb, Metrics, PlanTables, PriceTable, Pricing,
};
use phoebis_live::{LiveConfig, LiveFeed};
use phoebis_oauth::{Clock, CredentialManager, PairingSessions, UsageService};
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{control_response, ControlFixture},
};
use phoebis_relay::{CatalogService, Limits, Relay};
use phoebis_routing::{spawn, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AttemptOutcome, Identity, NewBundle, Secret, StorageKey,
    Store,
};
use phoebis_transport::{
    connector_builder,
    control::{ControlClient, ResolveFuture, Resolver},
    pool::Pool,
    trust_extra_ca, Plane,
};
use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

/// The account uuid every control fixture names.
pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";

/// Runs on every read of the alerts engine's own clock, for a test that has to land a store mutation inside
/// `Alerts::resolve`'s otherwise-synchronous read-then-write window: nothing else in that path yields.
pub type ClockHook = Arc<dyn Fn() + Send + Sync>;

/// The key stored on the API-key account, which the verify test expects to see on the wire.
pub const API_KEY: &str = "sk-ant-api03-PLACEHOLDER";

/// Unix milliseconds every clock in these tests reports, so no deadline moves under a test.
pub const NOW: i64 = 1_000_000;

const BODY_CAPTURE_MAX_HOURS: u32 = 72;

/// The usage cache TTL every builder but `admin_parts_polled` runs with, the settings echo's figure.
const USAGE_TTL: Duration = Duration::from_secs(60);

/// The bound the relay harness gives discovery too, so a fake that never answers cannot hang the suite.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a socket read may wait before the test fails rather than hangs.
const WS_DEADLINE: Duration = Duration::from_secs(10);

/// One post the alert deliverer made. The alerts crate has a fake of its own, which no integration test module
/// in another crate can reach, so the admin harness carries one too.
#[derive(Clone, Debug)]
pub struct Posted {
    pub body: serde_json::Value,
    pub signature: Option<String>,
    pub url: String,
}

/// Answers the alert deliverer whatever the test queued, and keeps every post for it to read back. An empty
/// queue answers 200, so a test that does not care about a delivery need not script one.
pub struct FakeWebhook {
    answers: Mutex<VecDeque<Result<u16, String>>>,
    posts: Mutex<Vec<Posted>>,
}

impl FakeWebhook {
    pub fn new() -> Arc<FakeWebhook> {
        Arc::new(FakeWebhook {
            answers: Mutex::new(VecDeque::new()),
            posts: Mutex::new(Vec::new()),
        })
    }

    pub fn answer(&self, a: Result<u16, String>) {
        self.answers.lock().unwrap().push_back(a);
    }

    pub fn posts(&self) -> Vec<Posted> {
        self.posts.lock().unwrap().clone()
    }
}

impl Webhook for FakeWebhook {
    fn post(
        &self,
        url: String,
        body: Vec<u8>,
        signature: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send>> {
        self.posts.lock().unwrap().push(Posted {
            body: serde_json::from_slice(&body).unwrap(),
            signature,
            url,
        });
        let answer = self.answers.lock().unwrap().pop_front().unwrap_or(Ok(200));
        Box::pin(async move { answer })
    }
}

/// Everything `app_with` builds. A test reaching past the HTTP surface takes what it needs rather than
/// every helper growing another tuple slot.
struct Parts {
    alerts: Arc<Alerts>,
    app: Router,
    bus: Bus,
    clock_cell: Arc<AtomicI64>,
    db: Arc<LedgerDb>,
    keepalive: Arc<Engine>,
    ledger: Ledger,
    live: LiveFeed,
    manager: Arc<CredentialManager>,
    pricing: Arc<Pricing>,
    relay: Arc<Relay>,
    resolver: Resolver,
    router: RouterHandle,
    store: Arc<Store>,
    up: FakeUpstream,
    usage: Arc<UsageService>,
    vault: Arc<PairingVault>,
    webhook: Arc<FakeWebhook>,
}

/// The app with the observation stack behind it, for the tests that write rows into the ledger and then read
/// them back through the analytics, request-log, and dashboard routes.
pub struct Full {
    pub alerts: Arc<Alerts>,
    pub app: Router,
    pub bus: Bus,
    pub clock_cell: Arc<AtomicI64>,
    pub db: Arc<LedgerDb>,
    pub keepalive: Arc<Engine>,
    pub ledger: Ledger,
    pub live: LiveFeed,
    pub pricing: Arc<Pricing>,
    /// The relay backing the harness's own keepalive engine, exposed so a test can drive a real request
    /// through the proxy listener's own authenticate-and-touch path rather than reaching around it.
    pub relay: Arc<Relay>,
    pub resolver: Resolver,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    pub up: FakeUpstream,
    /// No scheduler runs in the harness, so a test that needs a polled snapshot reads it through `usage.poll`.
    pub usage: Arc<UsageService>,
    pub webhook: Arc<FakeWebhook>,
}

pub async fn admin_parts_full() -> Full {
    full_from(app_with(None, ACCOUNT_UUID, None, None).await)
}

/// The full harness with the admin origin pinned, which is what every configuration outside the loopback
/// development mode runs, and the only shape in which the origin check on a handshake is exercised.
pub async fn admin_parts_full_with_origin(origin: &str) -> Full {
    full_from(app_with(None, ACCOUNT_UUID, Some(origin.to_string()), None).await)
}

/// The full harness with the alert webhook destination check on, as every running router has it. The other
/// builders leave it off so a rule may name the fake receiver, which is on loopback.
pub async fn admin_parts_full_refusing_local_webhooks() -> Full {
    full_from(
        app_with_webhook_policy(None, ACCOUNT_UUID, None, true, None, false, None, USAGE_TTL).await,
    )
}

/// The full harness with a hook on the alerts engine's own clock, for a test that must interleave a store
/// mutation with `Alerts::resolve`'s internal read-then-write; every other builder's alerts engine reads the
/// plain clock.
pub async fn admin_parts_full_with_alert_clock_hook(hook: ClockHook) -> Full {
    full_from(
        app_with_webhook_policy(
            None,
            ACCOUNT_UUID,
            None,
            true,
            None,
            true,
            Some(hook),
            USAGE_TTL,
        )
        .await,
    )
}

/// The full harness over a store the caller already holds. The router task reads the routing state once, at
/// spawn, so a test that needs a pin in the list has to write it before this call.
pub async fn admin_parts_full_on(store: Arc<Store>) -> Full {
    full_from(app_with(None, ACCOUNT_UUID, None, Some(store)).await)
}

fn full_from(parts: Parts) -> Full {
    Full {
        alerts: parts.alerts,
        app: parts.app,
        bus: parts.bus,
        clock_cell: parts.clock_cell,
        db: parts.db,
        keepalive: parts.keepalive,
        ledger: parts.ledger,
        live: parts.live,
        pricing: parts.pricing,
        relay: parts.relay,
        resolver: parts.resolver,
        router: parts.router,
        store: parts.store,
        up: parts.up,
        usage: parts.usage,
        webhook: parts.webhook,
    }
}

/// The generation the store stamped for `key_id`. A companion session belongs to the key and the generation
/// that registered it, so every engine call names the pair.
pub fn generation(store: &Store, key_id: &str) -> i64 {
    store.api_key(key_id).unwrap().unwrap().created_at
}

pub async fn admin_app() -> (Router, FakeUpstream) {
    let parts = app_with(None, ACCOUNT_UUID, None, None).await;
    (parts.app, parts.up)
}

pub async fn admin_app_paired() -> (Router, FakeUpstream) {
    let parts = app_with(Some(fixture_identity()), ACCOUNT_UUID, None, None).await;
    (parts.app, parts.up)
}

/// A paired slot with its usage service and the router behind it, the service's cache TTL the test's.
pub struct Polled {
    pub app: Router,
    pub clock_cell: Arc<AtomicI64>,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    pub up: FakeUpstream,
    pub usage: Arc<UsageService>,
}

/// No scheduler runs here, so the test reads the endpoint through `usage.poll` where a running router's
/// schedule would; a TTL of zero lets every manual refresh past the backoff reach the endpoint.
pub async fn admin_parts_polled(usage_ttl: Duration) -> Polled {
    let parts = app_with_webhook_policy(
        Some(fixture_identity()),
        ACCOUNT_UUID,
        None,
        true,
        None,
        true,
        None,
        usage_ttl,
    )
    .await;
    Polled {
        app: parts.app,
        clock_cell: parts.clock_cell,
        router: parts.router,
        store: parts.store,
        up: parts.up,
        usage: parts.usage,
    }
}

/// A paired slot whose upstream now answers as a different account, which is how the re-pairing test
/// presents somebody else's login to a slot that is already bound.
pub async fn admin_app_paired_as(uuid: &str) -> (Router, FakeUpstream) {
    let parts = app_with(Some(fixture_identity()), uuid, None, None).await;
    (parts.app, parts.up)
}

/// A paired slot whose last refresh resolved uncertain, which is the state an operator has to be shown.
pub async fn admin_app_quarantined(detail: &str) -> (Router, FakeUpstream) {
    let (router, up, _, _) = quarantined_parts(detail).await;
    (router, up)
}

/// The unpaired app with the store and the router behind it, for the tests that have to look past the HTTP
/// surface at what a write did to the registry or to the published snapshot.
pub async fn admin_parts() -> (Router, FakeUpstream, Arc<Store>, RouterHandle) {
    let parts = app_with(None, ACCOUNT_UUID, None, None).await;
    (parts.app, parts.up, parts.store, parts.router)
}

/// The unpaired app with the vault behind it, for the tests that have to look at what the vault still
/// holds after a pairing was left unfinished.
pub async fn admin_parts_with_vault() -> (
    Router,
    FakeUpstream,
    Arc<Store>,
    RouterHandle,
    Arc<PairingVault>,
) {
    let parts = app_with(None, ACCOUNT_UUID, None, None).await;
    (parts.app, parts.up, parts.store, parts.router, parts.vault)
}

/// A second app over a store another `Parts` already opened, so its router task loads whatever the store
/// holds at this moment rather than what it held when the first app started.
pub async fn admin_parts_on(store: Arc<Store>) -> (Router, FakeUpstream, Arc<Store>, RouterHandle) {
    let parts = app_with(None, ACCOUNT_UUID, None, Some(store)).await;
    (parts.app, parts.up, parts.store, parts.router)
}

/// The same slot with the store and the manager behind it, for the tests that have to look past the HTTP
/// surface at what a confirmation did to the account's bookkeeping.
pub async fn quarantined_parts(
    detail: &str,
) -> (Router, FakeUpstream, Arc<Store>, Arc<CredentialManager>) {
    let parts = app_with(Some(fixture_identity()), ACCOUNT_UUID, None, None).await;
    let attempt = parts
        .store
        .open_refresh_attempt("business", 1, 1, 1)
        .unwrap();
    parts
        .store
        .resolve_refresh_attempt(attempt, 1, AttemptOutcome::Uncertain(detail.to_string()))
        .unwrap();
    (parts.app, parts.up, parts.store, parts.manager)
}

/// A paired slot with an already-expired token, backing off after the token endpoint rejected the last
/// refresh: the state an operator has to be shown even though no refresh attempt is left open.
pub async fn backing_off_parts() -> (
    Router,
    FakeUpstream,
    Arc<Store>,
    Arc<CredentialManager>,
    Arc<AtomicI64>,
) {
    let parts = app_with(Some(fixture_identity()), ACCOUNT_UUID, None, None).await;
    parts
        .store
        .commit_credentials(
            "business",
            1,
            1,
            Some(1),
            None,
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: NOW - 3_600_000,
                expires_at: NOW - 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    parts.up.set_rule_after(
        0,
        "/v1/oauth/token",
        Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let _ = parts.manager.acquire("business").await;
    let _ = parts.manager.acquire("business").await;
    (
        parts.app,
        parts.up,
        parts.store,
        parts.manager,
        parts.clock_cell,
    )
}

/// Switches the token endpoint back to a successful, identity-matched response, for a test that drives a
/// refresh past a rejection it set up earlier.
pub fn succeed_token_requests(up: &FakeUpstream) {
    up.set_rule_after(
        0,
        "/v1/oauth/token",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: as_account(control_response(ControlFixture::Token), ACCOUNT_UUID),
        },
    );
}

/// An app that pins its admin origin, as every configuration outside plain HTTP must.
pub async fn admin_app_with_origin(origin: &str) -> (Router, FakeUpstream) {
    let parts = app_with(None, ACCOUNT_UUID, Some(origin.to_string()), None).await;
    (parts.app, parts.up)
}

/// The origin-pinned app with `plain_http` on as well: the one shape that still enforces an origin while the
/// cookie travels without `Secure`, which no other builder produces since they tie the two together.
pub async fn admin_app_plain_with_origin(origin: &str) -> (Router, FakeUpstream) {
    let parts = app_with_webhook_policy(
        None,
        ACCOUNT_UUID,
        Some(origin.to_string()),
        true,
        None,
        true,
        None,
        USAGE_TTL,
    )
    .await;
    (parts.app, parts.up)
}

pub async fn get(app: &Router, path: &str, cookie: Option<&str>) -> Response<Body> {
    send(app, "GET", path, "", cookie, None).await
}

pub async fn post(app: &Router, path: &str, body: &str, cookie: Option<&str>) -> Response<Body> {
    send(app, "POST", path, body, cookie, None).await
}

/// A POST carrying the `Origin` header a browser attaches to every cross-site write.
pub async fn post_from(
    app: &Router,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    origin: &str,
) -> Response<Body> {
    send(app, "POST", path, body, cookie, Some(origin)).await
}

pub async fn delete(app: &Router, path: &str, cookie: Option<&str>) -> Response<Body> {
    send(app, "DELETE", path, "", cookie, None).await
}

pub async fn patch(app: &Router, path: &str, body: &str, cookie: Option<&str>) -> Response<Body> {
    send(app, "PATCH", path, body, cookie, None).await
}

pub async fn put(app: &Router, path: &str, body: &str, cookie: Option<&str>) -> Response<Body> {
    send(app, "PUT", path, body, cookie, None).await
}

pub async fn json(response: Response<Body>) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Polls a GET until `ready` accepts its JSON, within five seconds: the ledger task lands a write a batch
/// after the record was offered, so a view read straight after a `record` may be one batch behind.
pub async fn wait_json(
    app: &Router,
    path: &str,
    cookie: &str,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let give_up = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let value = json(get(app, path, Some(cookie)).await).await;
        if ready(&value) {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "{path} never became ready: {value}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn cookie_of(response: &Response<Body>) -> String {
    response
        .headers()
        .get("set-cookie")
        .expect("a login response carries the session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("phoebis_admin=")
        .expect("the cookie is named phoebis_admin")
        .to_string()
}

pub async fn login(app: &Router, secret: &str) -> String {
    let response = post(
        app,
        "/admin/session",
        &format!(r#"{{"secret":"{secret}"}}"#),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    cookie_of(&response)
}

/// A login carrying the `Origin` a browser would attach, which an app with a pinned origin requires on the
/// one mutation an unauthenticated caller reaches.
pub async fn login_from(app: &Router, secret: &str, origin: &str) -> String {
    let response = post_from(
        app,
        "/admin/session",
        &format!(r#"{{"secret":"{secret}"}}"#),
        None,
        origin,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    cookie_of(&response)
}

pub type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The whole app behind a loopback listener, for the tests that need a real handshake rather than a
/// `oneshot` call.
pub async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// The next text message on the socket, as JSON.
pub async fn next_text(ws: &mut Client) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + WS_DEADLINE;
    loop {
        let message = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("a message arrives before the deadline")
            .expect("the socket is still open")
            .expect("the message is not an error");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

/// Reads until a delta of `kind` arrives, returning every delta read on the way, all under one deadline.
pub async fn deltas_until(ws: &mut Client, kind: &str) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + WS_DEADLINE;
    let mut seen = Vec::new();
    loop {
        let message = tokio::time::timeout_at(deadline, ws.next())
            .await
            .unwrap_or_else(|_| panic!("no {kind} delta arrived; saw {seen:?}"))
            .expect("the socket is still open")
            .expect("the message is not an error");
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let Some(delta) = value.get("delta") else {
            continue;
        };
        let done = delta["type"] == kind;
        seen.push(delta.clone());
        if done {
            return seen;
        }
    }
}

/// Every delta that arrives within `window`. A test asserting something is never sent has to wait out a
/// window rather than read until it sees it.
pub async fn deltas_for(ws: &mut Client, window: Duration) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + window;
    let mut seen = Vec::new();
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, ws.next()).await {
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if let Some(delta) = value.get("delta") {
                seen.push(delta.clone());
            }
        }
    }
    seen
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
) -> Response<Body> {
    send_ex(
        app,
        method,
        path,
        body,
        cookie,
        origin,
        None,
        Some("application/json"),
    )
    .await
}

/// A request naming its own `Origin` and `Host`, for the same-authority check that compares one against the
/// other rather than against a configured value.
pub async fn send_with_host(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    origin: &str,
    host: &str,
) -> Response<Body> {
    send_ex(
        app,
        method,
        path,
        body,
        cookie,
        Some(origin),
        Some(host),
        Some("application/json"),
    )
    .await
}

/// A request whose `Content-Type` is the given value, or none at all.
pub async fn send_typed(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    content_type: Option<&str>,
) -> Response<Body> {
    send_ex(app, method, path, body, cookie, None, None, content_type).await
}

// A real request's `Content-Length` reflects what a client actually sent; a non-empty body here carries the
// same header so the gate's `carries_body` check sees what a browser's would.
#[allow(clippy::too_many_arguments)]
async fn send_ex(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    host: Option<&str>,
    content_type: Option<&str>,
) -> Response<Body> {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    if !body.is_empty() {
        request = request.header("content-length", body.len().to_string());
    }
    if let Some(cookie) = cookie {
        request = request.header("cookie", format!("phoebis_admin={cookie}"));
    }
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    if let Some(host) = host {
        request = request.header("host", host);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

fn fixture_identity() -> Identity {
    Identity {
        account_uuid: ACCOUNT_UUID.into(),
        email: "test@example.invalid".into(),
        organization_uuid: Some("00000000-0000-4000-8000-000000000002".into()),
    }
}

async fn app_with(
    identity: Option<Identity>,
    upstream_uuid: &str,
    origin: Option<String>,
    store: Option<Arc<Store>>,
) -> Parts {
    // Every builder but the plain-with-origin one ties this to whether an origin is pinned.
    let plain_http = origin.is_none();
    app_with_webhook_policy(
        identity,
        upstream_uuid,
        origin,
        plain_http,
        store,
        true,
        None,
        USAGE_TTL,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn app_with_webhook_policy(
    identity: Option<Identity>,
    upstream_uuid: &str,
    origin: Option<String>,
    plain_http: bool,
    store: Option<Arc<Store>>,
    allow_local_webhooks: bool,
    alert_clock_hook: Option<ClockHook>,
    usage_ttl: Duration,
) -> Parts {
    let up = fake_control_plane(upstream_uuid).await;
    let p = claude_code_2_1_269_linux_x64();
    let store =
        store.unwrap_or_else(|| Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap()));
    let paired = identity.is_some();
    // A store handed in by the caller already carries its own accounts; seeding it again would collide.
    if store.accounts().unwrap().is_empty() {
        store
            .insert_account(&AccountRow {
                id: "business".into(),
                kind: AccountKind::Oauth,
                label: "Business".into(),
                enabled: true,
                identity,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: if paired {
                    AccountState::Ready
                } else {
                    AccountState::Unpaired
                },
            })
            .unwrap();
        if paired {
            // Far from expiry, so reading usage never triggers a refresh of its own against the fake.
            store
                .commit_credentials(
                    "business",
                    1,
                    1,
                    None,
                    None,
                    &NewBundle {
                        access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                        refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                        issued_at: NOW,
                        expires_at: NOW + 3_600_000,
                        refresh_expires_at: None,
                        scopes: "user:profile user:inference".into(),
                    },
                )
                .unwrap();
        }

        // Every app carries an API-key slot as well, since verification is the one route that only an
        // API-key account reaches.
        store
            .insert_account(&AccountRow {
                id: "direct".into(),
                kind: AccountKind::ApiKey,
                label: "Direct".into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
        store
            .set_api_key_secret("direct", &Secret::new(API_KEY))
            .unwrap();
    }

    let addr = up.addr();
    // An authority with an explicit port names another loopback fake on that port; everything else is `up`.
    let resolver: Resolver = Arc::new(move |authority: &str| {
        let resolved = authority
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .or(Some(addr));
        Box::pin(async move { resolved }) as ResolveFuture
    });
    let mut control_builder = connector_builder(p, Plane::Control).unwrap();
    trust_extra_ca(&mut control_builder, up.ca_pem()).unwrap();
    let control = Arc::new(ControlClient::new(
        p,
        control_builder.build(),
        resolver.clone(),
    ));
    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let manager = Arc::new(CredentialManager::new(
        store.clone(),
        control.clone(),
        p,
        clock.clone(),
    ));
    let usage = Arc::new(UsageService::new(
        manager.clone(),
        control.clone(),
        p,
        usage_ttl,
        4,
        clock.clone(),
    ));
    let mut inference_builder = connector_builder(p, Plane::Inference).unwrap();
    trust_extra_ca(&mut inference_builder, up.ca_pem()).unwrap();
    let pool = Arc::new(Pool::new(
        inference_builder.build(),
        4,
        Duration::from_secs(60),
    ));
    let router_handle = spawn(store.clone(), clock.clone(), RouterConfig::default()).unwrap();
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool.clone(),
        resolver.clone(),
        router_handle.clone(),
        clock.clone(),
        DISCOVERY_TIMEOUT,
        Duration::from_secs(60),
        Duration::from_secs(5),
    ));
    let bus = Bus::new();
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let pricing = Arc::new(Pricing::new(PriceTable::shipped()));
    let metrics = Arc::new(Metrics::new());
    let ledger = spawn_ledger(
        db.clone(),
        pricing.clone(),
        store.clone(),
        &bus,
        router_handle.clone(),
        clock.clone(),
        metrics.clone(),
        LedgerConfig::default(),
    );
    let live = LiveFeed::spawn(&bus, pricing.clone(), clock.clone(), LiveConfig::default());
    // The admin harness had no relay before this task; keepalive needs one to probe through, even though
    // nothing here drives its scheduler.
    let limits = Limits {
        body_capture_max_hours: BODY_CAPTURE_MAX_HOURS,
        cooldown_fallback: Duration::from_secs(60),
        head_timeout: Duration::from_secs(10),
        hold_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(600),
        max_attempts: 3,
        max_request_bytes: 32 << 20,
        model_rejection: Duration::from_secs(3_600),
    };
    let relay = Arc::new(
        Relay::new(
            store.clone(),
            manager.clone(),
            pool,
            router_handle.clone(),
            resolver.clone(),
            clock.clone(),
            limits,
            catalog.clone(),
        )
        .with_bus(bus.clone()),
    );
    let tables = Arc::new(PlanTables::shipped());
    let keepalive = Engine::new(EngineParts {
        bus: bus.clone(),
        clock: clock.clone(),
        cooldown_fallback: Duration::from_secs(60),
        ledger: ledger.clone(),
        lifecycle: None,
        limits: KeepaliveLimits::default(),
        metrics: metrics.clone(),
        pricing: pricing.clone(),
        relay: relay.clone(),
        router: router_handle.clone(),
        session_expiry: Duration::from_secs(86_400),
        store: store.clone(),
        tables: tables.clone(),
    });
    let webhook = FakeWebhook::new();
    // A hooked clock for the alerts engine alone: sharing the hook with `clock` would fire it on every other
    // service's reads too, which is far more than the one test that needs it wants to reason about.
    let alert_clock: Clock = match alert_clock_hook {
        Some(hook) => {
            let c = clock_cell.clone();
            Arc::new(move || {
                hook();
                c.load(Ordering::SeqCst)
            })
        }
        None => clock.clone(),
    };
    // The instant sleeper: a delivery walks its three attempts in milliseconds rather than the real backoff.
    let alerts = Alerts::with_sleeper(
        AlertParts {
            clock: alert_clock,
            ledger: ledger.clone(),
            metrics: metrics.clone(),
            router_name: "phoebis-test".into(),
            store: store.clone(),
            webhook: webhook.clone(),
        },
        Box::new(|_| Box::pin(async {})),
    );
    let settings = Arc::new(SettingsView {
        admin_listen: "test".into(),
        admin_origin: origin.clone(),
        body_capture_max_hours: BODY_CAPTURE_MAX_HOURS,
        cache_diagnostics: false,
        catalog_cache_seconds: 60,
        catalog_refresh_seconds: 21_600,
        cooldown_fallback_seconds: 60,
        keepalive: KeepaliveLimits::default(),
        ledger_path: "test".into(),
        max_attempts: 3,
        metrics_listen: "127.0.0.1:9464".into(),
        model_rejection_seconds: 3_600,
        pricing_path: None,
        profile: p.name.into(),
        proxy_listen: "test".into(),
        quota_sample_retention_days: 30,
        retention_days: 90,
        rollup_retention_days: 400,
        session_expire_after_seconds: 86_400,
        startup_fetch_seconds: 5,
        storage_path: "test".into(),
        usage_cache_ttl_seconds: 60,
        usage_refresh_concurrency: 4,
        viewer_configured: true,
    });
    let vault = Arc::new(PairingVault::new());
    let state = Arc::new(AdminState {
        store: store.clone(),
        manager: manager.clone(),
        usage: usage.clone(),
        pairing: Arc::new(PairingSessions::new()),
        control,
        profile: p,
        sessions: Arc::new(Sessions::new(
            Duration::from_secs(900),
            Duration::from_secs(43_200),
        )),
        secrets: AdminSecrets {
            admin: Secret::new("admin-secret"),
            viewer: Some(Secret::new("viewer-secret")),
        },
        clock,
        plain_http,
        origin,
        vault: vault.clone(),
        started_at: NOW,
        router: router_handle.clone(),
        catalog,
        body_capture_max_hours: BODY_CAPTURE_MAX_HOURS,
        session_expiry: Duration::from_secs(86_400),
        bus: bus.clone(),
        ledger: ledger.clone(),
        live: live.clone(),
        metrics: metrics.clone(),
        pricing: pricing.clone(),
        tables,
        settings,
        keepalive: keepalive.clone(),
        alerts: alerts.clone(),
        allow_local_webhooks,
    });
    Parts {
        alerts,
        app: router(state),
        bus,
        clock_cell,
        db,
        keepalive,
        ledger,
        live,
        manager,
        pricing,
        relay,
        resolver,
        router: router_handle,
        store,
        up,
        usage,
        vault,
        webhook,
    }
}

/// Drives an unpaired slot to an exchanged pairing session and returns its id.
pub async fn exchanged_session_for(app: &Router, cookie: &str, account_id: &str) -> String {
    let started = json(
        post(
            app,
            "/admin/oauth/sessions",
            &format!(r#"{{"account_id":"{account_id}","login":"claude_ai"}}"#),
            Some(cookie),
        )
        .await,
    )
    .await;
    let id = started["id"].as_str().unwrap().to_string();
    let redirect = started["redirect_uri"].as_str().unwrap().to_string();
    let state = started["authorization_url"]
        .as_str()
        .unwrap()
        .split("state=")
        .nth(1)
        .unwrap()
        .to_string();
    let callback = post(
        app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(cookie),
    )
    .await;
    assert_eq!(callback.status(), StatusCode::OK);
    id
}

async fn fake_control_plane(upstream_uuid: &str) -> FakeUpstream {
    FakeUpstream::start(vec![
        json_rule(
            "/api/oauth/claude_cli/roles",
            control_response(ControlFixture::Roles).to_vec(),
        ),
        json_rule(
            "/api/oauth/profile",
            as_account(control_response(ControlFixture::Profile), upstream_uuid),
        ),
        json_rule(
            "/api/oauth/usage",
            control_response(ControlFixture::Usage).to_vec(),
        ),
        json_rule(
            "/v1/models",
            br#"{"data":[{"id":"claude-sonnet-4-5","type":"model","capabilities":{"effort":{"high":{"supported":true},"low":{"supported":true},"medium":{"supported":true},"supported":true}}}]}"#.to_vec(),
        ),
        json_rule(
            "/v1/oauth/token",
            as_account(control_response(ControlFixture::Token), upstream_uuid),
        ),
    ])
    .await
}

fn json_rule(path_prefix: &str, body: Vec<u8>) -> Rule {
    Rule {
        path_prefix: path_prefix.into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body,
        },
    }
}

/// The fixtures name one upstream account; the account uuid is what the confirm checks compare, so
/// rewriting it is enough to make the fake answer as somebody else.
fn as_account(body: &[u8], uuid: &str) -> Vec<u8> {
    String::from_utf8(body.to_vec())
        .unwrap()
        .replace(ACCOUNT_UUID, uuid)
        .into_bytes()
}
