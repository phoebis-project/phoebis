//! Builders shared by the admin API tests.
//!
//! Each integration test binary compiles this module separately, so a helper only one binary uses looks dead to
//! the other.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use phoebis_admin::{router, AdminSecrets, AdminState, PairingVault, Sessions};
use phoebis_oauth::{Clock, CredentialManager, PairingSessions, UsageService};
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{control_response, ControlFixture},
};
use phoebis_relay::CatalogService;
use phoebis_routing::{spawn, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AttemptOutcome, Identity, NewBundle, Secret, StorageKey,
    Store,
};
use phoebis_transport::{
    connector_builder,
    control::{ControlClient, Resolver},
    pool::Pool,
    trust_extra_ca, Plane,
};
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// The account uuid every control fixture names.
pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";

/// The key stored on the API-key account, which the verify test expects to see on the wire.
pub const API_KEY: &str = "sk-ant-api03-PLACEHOLDER";

/// Unix milliseconds every clock in these tests reports, so no deadline moves under a test.
pub const NOW: i64 = 1_000_000;

const BODY_CAPTURE_MAX_HOURS: u32 = 72;

/// The bound the relay harness gives discovery too, so a fake that never answers cannot hang the suite.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything `app_with` builds. A test reaching past the HTTP surface takes what it needs rather than
/// every helper growing another tuple slot.
struct Parts {
    app: Router,
    manager: Arc<CredentialManager>,
    router: RouterHandle,
    store: Arc<Store>,
    up: FakeUpstream,
    vault: Arc<PairingVault>,
}

pub async fn admin_app() -> (Router, FakeUpstream) {
    let parts = app_with(None, ACCOUNT_UUID, None, None).await;
    (parts.app, parts.up)
}

pub async fn admin_app_paired() -> (Router, FakeUpstream) {
    let parts = app_with(Some(fixture_identity()), ACCOUNT_UUID, None, None).await;
    (parts.app, parts.up)
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
    let attempt = parts.store.open_refresh_attempt("business", 1, 1).unwrap();
    parts
        .store
        .resolve_refresh_attempt(attempt, 1, AttemptOutcome::Uncertain(detail.to_string()))
        .unwrap();
    (parts.app, parts.up, parts.store, parts.manager)
}

/// An app that pins its admin origin, as every configuration outside the loopback development mode must.
pub async fn admin_app_with_origin(origin: &str) -> (Router, FakeUpstream) {
    let parts = app_with(None, ACCOUNT_UUID, Some(origin.to_string()), None).await;
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

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
) -> Response<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", format!("phoebis_admin={cookie}"));
    }
    if let Some(origin) = origin {
        request = request.header("origin", origin);
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
        authority
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .or(Some(addr))
    });
    let mut control_builder = connector_builder(p, Plane::Control).unwrap();
    trust_extra_ca(&mut control_builder, up.ca_pem()).unwrap();
    let control = Arc::new(ControlClient::new(
        p,
        control_builder.build(),
        resolver.clone(),
    ));
    let clock: Clock = Arc::new(|| NOW);
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
        Duration::from_secs(60),
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
    let router_handle = spawn(store.clone(), None, clock.clone(), RouterConfig::default()).unwrap();
    let catalog = Arc::new(CatalogService::new(
        store.clone(),
        manager.clone(),
        pool,
        resolver,
        router_handle.clone(),
        clock.clone(),
        DISCOVERY_TIMEOUT,
    ));
    let vault = Arc::new(PairingVault::new());
    let state = Arc::new(AdminState {
        store: store.clone(),
        manager: manager.clone(),
        usage,
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
        // The two go together: an origin is pinned exactly where the admin API is not on loopback.
        dev_loopback_http: origin.is_none(),
        origin,
        vault: vault.clone(),
        started_at: NOW,
        router: router_handle.clone(),
        catalog,
        body_capture_max_hours: BODY_CAPTURE_MAX_HOURS,
        session_expiry: Duration::from_secs(86_400),
    });
    Parts {
        app: router(state),
        manager,
        router: router_handle,
        store,
        up,
        vault,
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
            br#"{"data":[{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec(),
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
