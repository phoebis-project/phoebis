//! The `/admin` surface: one gate in front of every route, then the account, pairing, usage, and system
//! handlers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE,
    TRANSFER_ENCODING, UPGRADE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, patch, post, put};
use axum::{Extension, Json, Router};
use phoebis_oauth::{
    authorization_url, control_request, ephemeral_port, exchange_body, loopback_redirect_uri,
    parse_profile, parse_token_response, CallbackInput, ControlKind, LoginKind, OauthError,
    PairingError, PairingSession, PairingStatus, PendingIdentity, ProfileResponse, UsageSnapshot,
};
use phoebis_relay::DiscoveryError;
use phoebis_routing::{OverageEvidence, ReloadError};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, CooldownRow, Identity, NewBundle, PoolMember, StoreError,
};
use phoebis_transport::response::ResponseHead;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    accounts, alerts, analytics, dashboard, keepalive, keys, live, models, pools, pricing,
    requests, sessions, settings, AdminState, LoginError, Role,
};

/// Covers a slow control-plane answer without letting one stalled exchange hold an admin request open
/// for minutes.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

const COOKIE_NAME: &str = "phoebis_admin";

const LOGIN_PATH: &str = "/admin/session";

pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/admin/accounts", get(list_accounts).post(accounts::create))
        .route(
            "/admin/accounts/{id}",
            get(show_account)
                .patch(accounts::patch)
                .delete(accounts::remove),
        )
        .route(
            "/admin/accounts/{id}/clear-funding",
            post(accounts::clear_funding),
        )
        .route(
            "/admin/accounts/{id}/cooldowns/{scope}",
            delete(accounts::clear_cooldown),
        )
        .route("/admin/accounts/{id}/credentials", delete(disconnect))
        .route("/admin/accounts/{id}/pause", post(pause))
        .route("/admin/accounts/{id}/catalog", get(accounts::catalog))
        .route("/admin/accounts/{id}/quota", get(accounts::quota))
        .route("/admin/accounts/{id}/resume", post(resume))
        .route("/admin/accounts/{id}/usage", get(usage))
        .route("/admin/accounts/{id}/usage/refresh", post(usage))
        .route("/admin/accounts/{id}/verify", post(verify))
        .route("/admin/alerts/deliveries", get(alerts::list_lifecycle))
        .route("/admin/alerts/events", get(alerts::list_events))
        .route(
            "/admin/alerts/events/{id}/deliveries",
            get(alerts::list_deliveries),
        )
        .route(
            "/admin/alerts/events/{id}/resolve",
            post(alerts::resolve_event),
        )
        .route(
            "/admin/alerts/rules",
            get(alerts::list_rules).post(alerts::create_rule),
        )
        .route(
            "/admin/alerts/rules/{id}",
            patch(alerts::patch_rule).delete(alerts::delete_rule),
        )
        .route("/admin/alerts/rules/{id}/test", post(alerts::test_rule))
        .route("/admin/analytics/accounts", get(analytics::accounts))
        .route("/admin/analytics/breakdown", get(analytics::breakdown))
        .route("/admin/analytics/budgets", get(analytics::budgets))
        .route("/admin/analytics/burn", get(analytics::burn))
        .route("/admin/analytics/fits", get(analytics::fits))
        .route("/admin/analytics/placements", get(analytics::placements))
        .route("/admin/analytics/rewrites", get(analytics::rewrites))
        .route("/admin/analytics/series", get(analytics::series))
        .route("/admin/dashboard", get(dashboard::show))
        .route("/admin/keepalive/overview", get(keepalive::overview))
        .route("/admin/keepalive/recipes", get(keepalive::recipes))
        .route("/admin/keys", get(keys::list).post(keys::create))
        .route("/admin/keys/{id}", patch(keys::patch).delete(keys::remove))
        .route("/admin/keys/{id}/models", get(models::key_models))
        .route("/admin/keys/{id}/revoke", post(keys::revoke))
        .route("/admin/keys/{id}/rotate", post(keys::rotate))
        .route("/admin/live", get(live::upgrade))
        .route("/admin/live/snapshot", get(live::past))
        .route("/admin/models/state", get(models::state))
        .route("/admin/oauth/sessions", post(pairing_start))
        .route(
            "/admin/oauth/sessions/{id}",
            get(pairing_show).delete(pairing_cancel),
        )
        .route(
            "/admin/oauth/sessions/{id}/callback",
            post(pairing_callback),
        )
        .route("/admin/oauth/sessions/{id}/confirm", post(pairing_confirm))
        .route("/admin/pools", get(pools::list).post(pools::create))
        .route(
            "/admin/pools/{id}",
            patch(pools::patch).delete(pools::remove),
        )
        .route("/admin/pools/{id}/members", put(pools::set_members))
        .route("/admin/pools/{id}/traffic", get(pools::traffic))
        .route("/admin/pricing", get(pricing::show))
        .route("/admin/pricing/import", post(pricing::import))
        .route("/admin/pricing/import/confirm", post(pricing::confirm))
        .route("/admin/requests", get(requests::list))
        .route("/admin/requests/{id}", get(requests::show))
        .route("/admin/requests/{id}/bodies", get(requests::bodies))
        .route("/admin/session", get(whoami).post(login).delete(logout))
        .route("/admin/sessions", get(sessions::list))
        .route(
            "/admin/sessions/{key}/{session}/release",
            post(sessions::release),
        )
        .route("/admin/sessions/costs", get(sessions::costs))
        .route("/admin/settings", get(settings::show))
        .route("/admin/system", get(system))
        .route("/admin", any(unknown_admin_route))
        .route("/admin/{*rest}", any(unknown_admin_route))
        .layer(from_fn_with_state(state.clone(), gate))
        .with_state(state)
}

/// Who is calling, as the gate resolved them: the handle that names this admin session in pairing
/// ownership. The cookie that produced it never leaves the session store.
#[derive(Clone)]
pub(crate) struct Caller {
    pub session_id: String,
    pub role: Role,
}

/// Authentication, the role gate, the origin check, and `Cache-Control` for every route at once, so no
/// handler can be added that quietly skips one of them.
async fn gate(State(state): State<Arc<AdminState>>, mut request: Request, next: Next) -> Response {
    let mut response = match admit(&state, &mut request) {
        None => next.run(request).await,
        Some(refusal) => refusal,
    };
    // An admin answer describes credentials and usage; a cache between the browser and this process has
    // no business holding any of it.
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `None` admits the request; anything else is the refusal to answer with.
fn admit(state: &AdminState, request: &mut Request) -> Option<Response> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let mutation = matches!(
        method,
        Method::DELETE | Method::PATCH | Method::POST | Method::PUT
    );

    let websocket = request
        .headers()
        .get(UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    // A browser attaches `Origin` to every cross-site write and to every WebSocket handshake; a value that is
    // not the configured admin origin is a forged request rather than a mistake. The login is checked too: a
    // forced login is a real attack, and it is the one mutation an unauthenticated caller can reach.
    if mutation || websocket {
        let presented = request.headers().get(ORIGIN).and_then(|v| v.to_str().ok());
        match &state.origin {
            Some(expected) => {
                if presented != Some(expected.as_str()) {
                    return Some(failure(StatusCode::FORBIDDEN, "origin is not allowed"));
                }
            }
            // No configured origin means plain HTTP on a host the operator trusts. A browser still sends
            // `Origin` on every cross-site write and every handshake, and a value naming another authority
            // than the request's own `Host` is another page on this host driving the console; a request
            // with no `Origin` (curl, the health probe) is admitted.
            None => {
                if let Some(origin) = presented {
                    let host = request
                        .headers()
                        .get(HOST)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if !same_authority(origin, host) {
                        return Some(failure(StatusCode::FORBIDDEN, "origin is not allowed"));
                    }
                }
            }
        }
    }
    // A cross-site `fetch` may carry `text/plain` without a preflight; requiring the JSON media type on a
    // body-carrying mutation makes every such write a preflighted one, which the browser refuses without
    // CORS headers. The login is a mutation too.
    let with_body = matches!(method, Method::PATCH | Method::POST | Method::PUT);
    if with_body && carries_body(request.headers()) {
        let json = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            })
            .unwrap_or(false);
        if !json {
            return Some(failure(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content type must be application/json",
            ));
        }
    }

    if method == Method::POST && path == LOGIN_PATH {
        return None;
    }

    let Some(cookie) = presented_cookie(request.headers()) else {
        return Some(failure(StatusCode::UNAUTHORIZED, "no admin session"));
    };
    let Some((session_id, role)) = state.sessions.authenticate(&cookie, (state.clock)()) else {
        return Some(failure(StatusCode::UNAUTHORIZED, "no admin session"));
    };
    if mutation && role != Role::Admin && !viewer_may_mutate(&method, &path) {
        return Some(failure(
            StatusCode::FORBIDDEN,
            "the viewer role cannot change anything",
        ));
    }
    request.extensions_mut().insert(Caller { session_id, role });
    None
}

/// The two mutations a viewer may issue: ending its own session, which reaches nothing but itself, and a
/// usage refresh, which is served from the usage cache and so changes nothing upstream either.
fn viewer_may_mutate(method: &Method, path: &str) -> bool {
    (method == Method::DELETE && path == LOGIN_PATH)
        || (path.starts_with("/admin/accounts/") && path.ends_with("/usage/refresh"))
}

/// `origin` names the request's own authority: its scheme stripped, the rest equals `host` ignoring case.
pub(crate) fn same_authority(origin: &str, host: &str) -> bool {
    let authority = origin
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &origin[7..])
        .or_else(|| {
            origin
                .get(..8)
                .filter(|p| p.eq_ignore_ascii_case("https://"))
                .map(|_| &origin[8..])
        });
    authority.is_some_and(|a| !a.is_empty() && a.eq_ignore_ascii_case(host))
}

/// Whether the request carries a body a browser would attach a real `Content-Type` to: a `Content-Length` past
/// zero, or any `Transfer-Encoding` (chunked bodies never announce a length).
fn carries_body(headers: &HeaderMap) -> bool {
    let has_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > 0);
    has_length || headers.contains_key(TRANSFER_ENCODING)
}

fn presented_cookie(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        pair.trim()
            .strip_prefix(COOKIE_NAME)
            .and_then(|rest| rest.strip_prefix('='))
            .map(str::to_string)
    })
}

async fn login(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    #[derive(serde::Deserialize)]
    struct Input {
        secret: String,
    }
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry a secret");
    };
    match state
        .sessions
        .login(&input.secret, &state.secrets, (state.clock)())
    {
        Ok((cookie, _)) => {
            let mut attributes =
                format!("{COOKIE_NAME}={cookie}; Path=/admin; HttpOnly; SameSite=Strict");
            if !state.plain_http {
                attributes.push_str("; Secure");
            }
            set_cookie(StatusCode::NO_CONTENT, &attributes)
        }
        Err(LoginError::Throttled) => failure(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed logins, try again in a minute",
        ),
        Err(LoginError::Invalid) => failure(StatusCode::UNAUTHORIZED, "secret does not match"),
    }
}

async fn logout(State(state): State<Arc<AdminState>>, headers: HeaderMap) -> Response {
    if let Some(cookie) = presented_cookie(&headers) {
        state.sessions.logout(&cookie);
    }
    // Clearing the browser's copy as well keeps a logged-out tab from presenting a cookie this process
    // would only reject.
    set_cookie(
        StatusCode::NO_CONTENT,
        &format!("{COOKIE_NAME}=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0"),
    )
}

/// The console's first call: whether the cookie it holds still names a session, and which role that session
/// carries. The body names the session by the id pairing ownership uses, never by the cookie value.
async fn whoami(Extension(caller): Extension<Caller>) -> Response {
    Json(json!({
        "role": match caller.role { Role::Admin => "admin", Role::Viewer => "viewer" },
        "session_id": caller.session_id,
    }))
    .into_response()
}

async fn list_accounts(State(state): State<Arc<AdminState>>) -> Response {
    let rows = match state.store.accounts() {
        Ok(rows) => rows,
        Err(error) => return store_failure(error),
    };
    // One ask for what every row shares: the router and the ledger each answer once for the whole list.
    let ctx = account_context(&state).await;
    let mut accounts = Vec::with_capacity(rows.len());
    for row in &rows {
        accounts.push(account_view(&state, row, &ctx).await);
    }
    Json(json!({ "accounts": accounts })).into_response()
}

async fn show_account(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    match account(&state, &id) {
        Ok(row) => Json(account_json(&state, &row).await).into_response(),
        Err(refusal) => refusal,
    }
}

async fn pause(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    set_enabled(&state, &id, false).await
}

async fn resume(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    set_enabled(&state, &id, true).await
}

/// Selection reads `enabled` off the snapshot, so a pause that is only in the store still takes traffic.
async fn set_enabled(state: &AdminState, id: &str, enabled: bool) -> Response {
    if let Err(refusal) = account(state, id) {
        return refusal;
    }
    if let Err(error) = state.store.set_enabled(id, enabled) {
        return store_failure(error);
    }
    if let Err(refusal) = reload_or_refuse(state).await {
        return refusal;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Disconnects the slot: the lifecycle bump deletes both credential kinds, and everything still holding
/// the old identity's state goes with them.
async fn disconnect(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    match state.store.bump_lifecycle(&id) {
        Ok(_) => {}
        Err(StoreError::AccountNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "account not found")
        }
        Err(error) => return store_failure(error),
    }
    state.catalog.evict(&id);
    // The bump leaves the slot `disconnected`, which an OAuth slot leaves again by pairing. An API-key slot
    // has no pairing to run, so it rests where a slot that never had a key rests.
    if row.kind == AccountKind::ApiKey {
        if let Err(error) = state.store.set_state(&id, AccountState::Unpaired) {
            return store_failure(error);
        }
    }
    state.usage.invalidate(&id);
    let now = (state.clock)();
    for (session_id, owner) in state.vault.discard_for_account(&id) {
        let _ = state.pairing.cancel(&session_id, &owner, now);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Serves `GET .../usage` and `POST .../usage/refresh` alike: the usage service has no cache bypass, so
/// a held-down refresh button cannot turn into a stream of requests against a per-account rate limit.
async fn usage(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    if let Err(refusal) = account(&state, &id) {
        return refusal;
    }
    match state.usage.snapshot(&id).await {
        Ok(snapshot) => Json(usage_json(&snapshot)).into_response(),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

/// Answers whether an API key still works, by asking the cheapest authenticated endpoint there is: the
/// catalog discovery, which goes to the account's own host over the pool that host belongs to.
async fn verify(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    if row.kind != AccountKind::ApiKey {
        return failure(
            StatusCode::BAD_REQUEST,
            "only an API-key account is verified this way",
        );
    }
    let (ok, status) = match state.catalog.discover(&row).await {
        Ok(_) => (true, 200),
        Err(DiscoveryError::Unauthorized) => (false, 401),
        Err(DiscoveryError::Status(status)) => (false, status),
        Err(DiscoveryError::NoCredential) => {
            return failure(StatusCode::CONFLICT, "account holds no API key")
        }
        Err(error) => return failure(StatusCode::BAD_GATEWAY, &format!("upstream: {error}")),
    };
    Json(json!({ "ok": ok, "status": status })).into_response()
}

async fn pairing_start(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Response {
    #[derive(serde::Deserialize)]
    struct Input {
        account_id: String,
        login: Option<String>,
    }
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry an account_id");
    };
    let kind = match input.login.as_deref() {
        None | Some("claude_ai") => LoginKind::ClaudeAi,
        Some("console") => LoginKind::Console,
        Some(_) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "login must be claude_ai or console",
            )
        }
    };
    let row = match account(&state, &input.account_id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    // An API-key slot has no login to run: committing an OAuth bundle onto it would leave a credential
    // the relay never reaches for, since it picks the credential by the account's kind.
    if row.kind != AccountKind::Oauth {
        return failure(
            StatusCode::BAD_REQUEST,
            "only an OAuth account is paired this way",
        );
    }
    // The provider knows two redirects for this client: a loopback URL on an OS-assigned port, and a page on
    // platform.claude.com that shows the code. The loopback one keeps the browser off the provider's page: the
    // navigation to localhost fails to load and the operator pastes the URL from the address bar, or the code.
    let redirect_uri = loopback_redirect_uri(ephemeral_port());
    let session =
        match state
            .pairing
            .start(&row.id, &caller.session_id, &redirect_uri, (state.clock)())
        {
            Ok(session) => session,
            Err(error) => return pairing_failure(error),
        };
    state.vault.open(
        &session.id,
        &row.id,
        &caller.session_id,
        row.lifecycle_generation,
        session.deadline,
    );
    let url = authorization_url(
        state.profile,
        kind,
        &redirect_uri,
        state.profile.control.default_login_scopes,
        &session.pkce.challenge,
        &session.state,
    );
    (
        StatusCode::CREATED,
        Json(json!({
            "id": session.id,
            "authorization_url": url,
            "redirect_uri": redirect_uri,
            "expires_at": session.deadline,
        })),
    )
        .into_response()
}

async fn pairing_show(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    let Some(session) = state.pairing.get(&id, &caller.session_id, (state.clock)()) else {
        return failure(StatusCode::NOT_FOUND, "pairing session not found");
    };
    Json(json!({
        "account_id": session.account_id,
        "expires_at": session.deadline,
        "id": session.id,
        "status": reported_status(&session),
    }))
    .into_response()
}

async fn pairing_cancel(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    match state
        .pairing
        .cancel(&id, &caller.session_id, (state.clock)())
    {
        Ok(()) => {
            state.vault.discard(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => pairing_failure(error),
    }
}

async fn pairing_callback(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    #[derive(serde::Deserialize)]
    struct Input {
        code: Option<String>,
        state: Option<String>,
        url: Option<String>,
    }
    let now = (state.clock)();
    let malformed = || {
        failure(
            StatusCode::BAD_REQUEST,
            "body must carry the callback url, or a code and a state",
        )
    };
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        return malformed();
    };
    let callback = match (input.url, input.code, input.state) {
        (Some(url), _, _) => CallbackInput::Url(url),
        (None, Some(code), Some(state)) => CallbackInput::Code { code, state },
        _ => return malformed(),
    };
    let session = match state
        .pairing
        .accept_callback(&id, &caller.session_id, callback, now)
    {
        Ok(session) => session,
        Err(error) => return pairing_failure(error),
    };
    let PairingStatus::CodeReceived { code } = &session.status else {
        return failure(
            StatusCode::CONFLICT,
            "pairing session did not accept the callback",
        );
    };
    let (identity, bundle, profile) = match exchange(&state, &session, code).await {
        Ok(exchanged) => exchanged,
        // The authorization code is spent whether or not the exchange worked, so a rejected one ends the
        // session rather than leaving it in flight: the account would otherwise stay busy for the rest of
        // the ten-minute deadline with nothing able to finish it. The operator is told the same thing the
        // session now records.
        Err(detail) => {
            state.vault.discard(&id);
            let _ = state.pairing.fail(&id, &caller.session_id, &detail, now);
            return failure(StatusCode::BAD_GATEWAY, &detail);
        }
    };
    // The pairing session moves first: it is what the confirmation is checked against, so a bundle must
    // never sit in the vault for a session that says it is anything but exchanged.
    if let Err(error) = state
        .pairing
        .mark_exchanged(&id, &caller.session_id, identity.clone(), now)
    {
        state.vault.discard(&id);
        return pairing_failure(error);
    }
    if !state
        .vault
        .record_exchange(&id, identity.clone(), bundle, profile)
    {
        return failure(
            StatusCode::CONFLICT,
            "the pairing session was closed while the code was being exchanged",
        );
    }
    Json(json!({
        "status": "exchanged",
        "pending": {
            "account_uuid": identity.account_uuid,
            "email": identity.email,
            "organization_name": identity.organization_name,
            "organization_uuid": identity.organization_uuid,
        },
    }))
    .into_response()
}

/// Commits a pairing, after four checks that all have to hold at once. Every one of them refuses rather
/// than repairs, and every refusal drops the pending bundle, so no path through here leaves a credential
/// behind that a later call could commit.
async fn pairing_confirm(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    let now = (state.clock)();
    // Claiming the session is the first thing this does, and it is what makes everything below safe: the
    // status moves out of `Exchanged` under the sessions lock, so a cancellation arriving while the
    // credential is being installed is refused rather than leaving the account paired behind it. An expired
    // session, one that never reached an exchange, and one somebody else owns are all refused here too.
    if let Err(error) = state.pairing.claim_confirm(&id, &caller.session_id, now) {
        // A claim that does not land leaves the session and the vault exactly as they were. The refusal may
        // well be about a session another confirmation is holding at this moment, and emptying the vault from
        // here would fail that one too; the rest name a session this caller does not own, or one that is
        // still usable. A session that ran out of time is the exception: it can never be confirmed, and its
        // authorization code is spent, so the credential waiting on it is dropped now rather than held until
        // the process restarts.
        if matches!(error, PairingError::Expired) {
            state.vault.discard(&id);
        }
        return pairing_failure(error);
    }
    let conflict = |state: &AdminState, message: &'static str| {
        let _ = state.pairing.fail(&id, &caller.session_id, message, now);
        state.vault.discard(&id);
        failure(StatusCode::CONFLICT, message)
    };
    let unstored = |state: &AdminState, error: StoreError| {
        let _ = state
            .pairing
            .fail(&id, &caller.session_id, "storage failure", now);
        state.vault.discard(&id);
        store_failure(error)
    };

    let Some(pending) = state.vault.take_exchange(&id) else {
        return conflict(&state, "pairing session has no exchanged credential");
    };
    let row = match account(&state, &pending.account_id) {
        Ok(row) => row,
        Err(refusal) => {
            let _ = state
                .pairing
                .fail(&id, &caller.session_id, "account unavailable", now);
            return refusal;
        }
    };
    // An identity is permanent: `bump_lifecycle` clears credentials but never the binding, so a slot
    // stays with the upstream account it was first paired to. Moving a slot elsewhere means deleting it.
    if row
        .identity
        .as_ref()
        .is_some_and(|bound| bound.account_uuid != pending.identity.account_uuid)
    {
        return conflict(
            &state,
            "this slot is already paired to a different upstream account",
        );
    }
    if row.identity.is_none() {
        let identity = Identity {
            account_uuid: pending.identity.account_uuid.clone(),
            email: pending.identity.email.clone(),
            organization_uuid: pending.identity.organization_uuid.clone(),
        };
        // The store's unique index over the bound account uuid is what refuses the second slot. A scan here
        // would have to read every row and could still lose to a confirmation landing between the read and
        // the write.
        match state.store.set_identity(&row.id, &identity) {
            Ok(()) => {}
            Err(StoreError::IdentityTaken) => {
                return conflict(
                    &state,
                    "that upstream account is already paired to another slot",
                )
            }
            Err(error) => return unstored(&state, error),
        }
    }
    let expected = match state.store.credentials(&row.id) {
        Ok(current) => current.map(|bundle| bundle.generation),
        Err(error) => return unstored(&state, error),
    };
    // The lifecycle carried from the exchange, not `row`'s own read: the store's fence is the one that
    // counts, and it has to be checked against what the pairing started from, not against a read this same
    // request just took, or a disconnect that landed before this call would pass its own fresh state back
    // to itself.
    if let Err(error) = state.store.commit_credentials(
        &row.id,
        row.incarnation,
        pending.lifecycle_generation,
        expected,
        None,
        &pending.bundle,
    ) {
        return match error {
            StoreError::Disconnected => {
                conflict(&state, "the account was disconnected while pairing")
            }
            StoreError::Stale => conflict(
                &state,
                "the credential changed while pairing; start the pairing again",
            ),
            other => unstored(&state, other),
        };
    }
    // Stored now rather than left to the poll, so a freshly paired account shows its plan and start at once
    // instead of waiting out the next interval; a failure here is not the confirm's to fail, since the poll
    // rewrites the row regardless.
    if let Err(error) = state.store.set_profile(
        &row.id,
        &pending.profile.raw.to_string(),
        &pending.profile.subscription,
    ) {
        tracing::warn!(account = %row.id, %error, "the profile could not be stored after pairing");
    }
    if let Err(error) = state.store.set_state(&row.id, AccountState::Ready) {
        return unstored(&state, error);
    }
    state.usage.invalidate(&row.id);
    state.vault.mark_confirmed(&id);
    // The credential just committed replaces whatever a slot under this id answered before, so the cached
    // list (if any) is stale from this instant; the next read fetches fresh with the new one.
    state.catalog.evict(&row.id);
    // The pairing is over, so the account's one-at-a-time slot is handed back. The credential is already
    // committed at this point, so a session that refuses to close costs the account nothing beyond the
    // wait for its own deadline.
    if let Err(error) = state.pairing.confirm(&id, &caller.session_id, now) {
        tracing::warn!(%error, "a confirmed pairing session could not be closed");
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    Json(json!({ "account_id": row.id, "status": "confirmed" })).into_response()
}

async fn system(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let uptime = (now - state.started_at).max(0) / 1_000;
    let stats = state.ledger.stats();
    let load = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    let read = state.ledger.db().read(move |c| Ok(c.prepare("SELECT at, kind, account_id, key_id, session_id, model, actor, detail FROM events WHERE kind = 'ledger_gap' AND at >= ?1 ORDER BY at DESC LIMIT 50")?
        .query_map(rusqlite::params![now - 86_400_000], |r| Ok(phoebis_ledger::EventRecord { at: r.get(0)?, kind: r.get(1)?, account_id: r.get(2)?, key_id: r.get(3)?, session_id: r.get(4)?, model: r.get(5)?, actor: r.get(6)?, detail: r.get(7)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?)).await;
    let gaps = match read {
        Ok(gaps) => gaps,
        // The gap counter in the same response is whatever the writer reached, so an empty list from a read
        // that failed would tell the operator the counter has nothing behind it.
        Err(error) => {
            tracing::warn!(%error, "the ledger's gap events could not be read");
            Vec::new()
        }
    };
    let keepalive_stats = state.keepalive.stats();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "profile": state.profile.name,
        "uptime_seconds": uptime,
        "storage_ok": state.store.accounts().is_ok(),
        "ledger": {
            "schema_version": state.ledger.db().schema_version().await.ok(),
            "written": load(&stats.written), "dropped": load(&stats.dropped), "write_failures": load(&stats.write_failures),
            "gaps": load(&stats.gaps), "last_gap_at": stats.last_gap_at.load(std::sync::atomic::Ordering::Relaxed),
            "channel_depth": phoebis_ledger::CHANNEL_DEPTH,
        },
        "bus": state.bus.stats().iter().map(|(name, s)| json!({ "subscriber": name, "dropped": load(&s.dropped), "panics": load(&s.panics) })).collect::<Vec<_>>(),
        "live": { "clients": state.live.clients(), "in_flight": state.live.in_flight() },
        "pricing_unknown_models": state.pricing.unknown_models(),
        "gaps": gaps.iter().map(crate::requests::event_json).collect::<Vec<_>>(),
        "keepalive": {
            "agents": keepalive_stats.agents,
            "bus_gaps": keepalive_stats.bus_gaps,
            "retained_bytes": keepalive_stats.retained_bytes,
            "running_probes": keepalive_stats.running_probes,
            "sessions": keepalive_stats.sessions,
        },
        "alerts": state.alerts.stats(),
    }))
    .into_response()
}

/// An admin path nothing serves. Under the gate, so without a session it is the gate's 401 and nothing about
/// the surface leaks; with one it is a plain 404 the console can tell from the shell.
async fn unknown_admin_route() -> Response {
    failure(StatusCode::NOT_FOUND, "not found")
}

/// Runs the native post-login sequence against the control plane and turns its answers into the pending
/// identity an operator confirms, plus the bundle that confirmation would commit.
///
/// A failure is a detail rather than a response because the caller both answers with it and records it on the
/// session. Every way this fails is the upstream's fault, so the caller's status is always a bad gateway. No
/// detail here names a token: an upstream body is described, never quoted.
async fn exchange(
    state: &AdminState,
    session: &PairingSession,
    code: &str,
) -> Result<(PendingIdentity, NewBundle, ProfileResponse), String> {
    let body = exchange_body(
        state.profile,
        code,
        &session.redirect_uri,
        &session.pkce.verifier,
        &session.state,
    );
    let (head, raw) = control_send(state, ControlKind::Token, None, Some(body)).await?;
    if head.status != 200 {
        return Err(format!(
            "token exchange rejected with status {}",
            head.status
        ));
    }
    let token = parse_token_response(&raw).map_err(|error| oauth_detail("token", error))?;

    let answered = {
        let bearer = token.access_token.expose();
        // The measured login issues the profile lookup twice and the roles lookup once. Both are part of
        // what a login looks like on the wire, and the roles answer itself is not used here.
        let mut answered: Option<Bytes> = None;
        for _ in 0..2 {
            if let Ok((head, raw)) =
                control_send(state, ControlKind::Profile, Some(bearer), None).await
            {
                if head.status == 200 {
                    answered = Some(raw);
                }
            }
        }
        let _ = control_send(state, ControlKind::Roles, Some(bearer), None).await;
        answered
    };
    let Some(raw) = answered else {
        return Err("the profile lookup did not answer".to_string());
    };
    let profile = parse_profile(&raw).map_err(|error| oauth_detail("profile", error))?;

    // Cloned rather than moved: the confirm route stores the whole document once the credential commits, so
    // `profile` has to survive this function intact.
    let identity = PendingIdentity {
        account_uuid: profile.account_uuid.clone(),
        email: profile.email.clone(),
        organization_name: token
            .organization
            .as_ref()
            .and_then(|organization| organization.name.clone()),
        organization_uuid: profile.organization_uuid.clone(),
    };
    let now = (state.clock)();
    let bundle = NewBundle {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        issued_at: now,
        expires_at: now.saturating_add(token.expires_in.saturating_mul(1_000)),
        refresh_expires_at: token
            .refresh_token_expires_in
            .map(|seconds| now.saturating_add(seconds.saturating_mul(1_000))),
        scopes: token
            .scope
            .unwrap_or_else(|| state.profile.control.default_login_scopes.to_string()),
    };
    Ok((identity, bundle, profile))
}

async fn control_send(
    state: &AdminState,
    kind: ControlKind,
    bearer: Option<&str>,
    body: Option<Vec<u8>>,
) -> Result<(ResponseHead, Bytes), String> {
    let request = control_request(state.profile, kind, bearer, body);
    let host = kind.host(state.profile);
    state
        .control
        .send(request, &host, CONTROL_TIMEOUT)
        .await
        .map_err(|error| format!("control request failed: {error}"))
}

// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn account(state: &AdminState, id: &str) -> Result<AccountRow, Response> {
    match state.store.account(id) {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(failure(StatusCode::NOT_FOUND, "account not found")),
        Err(error) => Err(store_failure(error)),
    }
}

/// What every account row reads from the ledger and the registry, fetched once per list call.
pub(crate) struct AccountContext {
    pub cooldowns: Vec<CooldownRow>,
    pub day: BTreeMap<String, phoebis_ledger::Totals>,
    pub extra: Vec<phoebis_ledger::ExtraUsageSampleRecord>,
    pub lifetime: BTreeMap<String, phoebis_ledger::Totals>,
    pub members: Vec<PoolMember>,
    pub overages: BTreeMap<String, OverageEvidence>,
    pub quota: Vec<phoebis_ledger::QuotaSampleRecord>,
    pub week: BTreeMap<String, phoebis_ledger::Totals>,
}

pub(crate) async fn account_context(state: &AdminState) -> AccountContext {
    let now = (state.clock)();
    let hour = phoebis_ledger::hour_of(now);
    let ledger = state
        .ledger
        .db()
        .read(move |c| {
            Ok((
                phoebis_ledger::latest_quota(c, now)?,
                phoebis_ledger::latest_extra_usage(c, now)?,
                phoebis_ledger::request_totals(c, "account_id", hour - 24 * 3_600)?,
                phoebis_ledger::request_totals(c, "account_id", hour - 7 * 24 * 3_600)?,
                // Lifetime: `i64::MIN` sits at or below the earliest hour the rollups can hold, so every row clears it.
                phoebis_ledger::request_totals(c, "account_id", i64::MIN)?,
            ))
        })
        .await;
    let (quota, extra, day, week, lifetime) = match ledger {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(%error, "the account view could not read the ledger; totals and windows are empty");
            Default::default()
        }
    };
    let members = state
        .store
        .load_registry()
        .map(|l| l.members)
        .unwrap_or_default();
    AccountContext {
        cooldowns: state.router.cooldowns().await.unwrap_or_default(),
        day,
        extra,
        lifetime,
        members,
        overages: state
            .router
            .overages()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        quota,
        week,
    }
}

/// Never a token, in any branch: an account view is the one place a credential could plausibly be shown
/// and must not be.
pub(crate) async fn account_json(state: &AdminState, row: &AccountRow) -> Value {
    let ctx = account_context(state).await;
    account_view(state, row, &ctx).await
}

/// The view over a context the caller already holds, so listing every account asks the router and the
/// ledger once.
async fn account_view(state: &AdminState, row: &AccountRow, ctx: &AccountContext) -> Value {
    let overage = ctx.overages.get(&row.id);
    let credential = state.store.credential_metadata(&row.id).ok().flatten();
    let attempt = state
        .store
        .last_refresh_attempt(&row.id, row.incarnation)
        .ok()
        .flatten();
    // The store refuses to open a second attempt while one is unresolved or uncertain, so the newest
    // attempt is the only one either state can be hiding in.
    let quarantined = attempt.as_ref().is_some_and(|attempt| {
        attempt.resolved_at.is_none() || attempt.outcome.as_deref() == Some("uncertain")
    });
    // A failure that opened no attempt (a backoff or a quarantine) still updates the credential row, so
    // whichever of the two landed last is the one that answers `credential.last_error`.
    let attempt_error = attempt
        .as_ref()
        .filter(|attempt| {
            matches!(
                attempt.outcome.as_deref(),
                Some("failed") | Some("uncertain")
            )
        })
        .and_then(|attempt| attempt.resolved_at.map(|at| (at, attempt.detail.clone())));
    // Built ahead of the `json!` call below: the profile block pushes the macro past its expansion depth,
    // as `attempt_json` found for the request log.
    let profile = state
        .store
        .profile_json(&row.id)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|p| {
            json!({
                "billing_type": p["organization"]["billing_type"],
                "display_name": p["account"]["display_name"],
                "has_extra_usage_enabled": p["account"]["has_extra_usage_enabled"],
                "organization_name": p["organization"]["name"],
                "organization_type": p["organization"]["organization_type"],
                "rate_limit_tier": p["organization"]["rate_limit_tier"],
            })
        });
    // The stored flag and end read through this alone, with no profile join, so a cancelled account answers
    // them even before it ever pairs.
    let term = state
        .store
        .subscription_term(&row.id)
        .ok()
        .flatten()
        .unwrap_or_default();
    // Null until the account has paired and a profile document exists, unless the operator has cancelled it:
    // a cancelled, unpaired OAuth row still has an end to report, so the flag joins `profile.is_some()` here.
    let subscription = (row.kind == AccountKind::Oauth && (profile.is_some() || term.cancelled))
        .then(|| {
            state
                .store
                .subscription(&row.id)
                .ok()
                .flatten()
                .unwrap_or_default()
        })
        .map(|s| {
            json!({
                "plan": s.plan,
                "tier": s.tier,
                "since": s.since,
                "trial_ends_at": s.trial_ends_at,
                "ends_at": term.ends_at,
                "derived_ends_at": phoebis_routing::term::derived_end(&term, (state.clock)()),
            })
        });
    json!({
        "id": row.id,
        "kind": match row.kind { AccountKind::ApiKey => "api_key", AccountKind::Oauth => "oauth" },
        "label": row.label,
        "enabled": row.enabled,
        "cancelled": term.cancelled,
        "state": state_name(row.state),
        "identity": row.identity.as_ref().map(|identity| json!({
            "email": identity.email,
            "account_uuid": identity.account_uuid,
            "organization_uuid": identity.organization_uuid,
        })),
        "credential": credential.map(|credential| json!({
            "access_expires_at": credential.expires_at,
            "refresh_expires_at": credential.refresh_expires_at,
            "generation": credential.generation,
            "last_refresh_at": attempt.as_ref().and_then(|attempt| attempt.resolved_at),
            "last_error": match (&attempt_error, credential.last_error.as_ref()) {
                (Some((at, detail)), Some(text)) => {
                    // A tie favors the credential error: a backoff recorded right after the attempt that
                    // armed it lands in the same millisecond on the store's clock as often as not.
                    if credential.last_error_at >= *at { Some(text.clone()) } else { detail.clone() }
                }
                (Some((_, detail)), None) => detail.clone(),
                (None, Some(text)) => Some(text.clone()),
                (None, None) => None,
            },
            "quarantined": quarantined,
        })),
        "lifecycle_generation": row.lifecycle_generation,
        "cooldowns": ctx.cooldowns.iter().filter(|cooldown| cooldown.account_id == row.id).map(|cooldown| json!({
            "scope": cooldown.scope,
            "until": cooldown.until,
            "reason": cooldown.reason,
            "window": cooldown.window,
            "set_at": cooldown.set_at,
        })).collect::<Vec<_>>(),
        "overage": overage.map(|overage| json!({
            "in_use": overage.in_use,
            "since": overage.since,
            "reset": overage.reset,
            "observed_at": overage.observed_at,
        })),
        "profile": profile,
        "subscription": subscription,
        "windows": ctx.quota.iter().filter(|q| q.account_id == row.id).map(|q| json!({ "id": q.window, "utilization": q.utilization, "resets_at": q.reset_at, "observed_at": q.bucket * 1_000 })).collect::<Vec<_>>(),
        "extra_usage": ctx.extra.iter().find(|e| e.account_id == row.id).map(|e| json!({ "currency": e.currency, "disabled_reason": e.disabled_reason, "in_use": e.in_use, "is_enabled": e.is_enabled, "monthly_limit": e.monthly_limit, "used_credits": e.used_credits, "utilization": e.utilization, "observed_at": e.bucket * 1_000 })),
        "pools": ctx.members.iter().filter(|m| m.account_id == row.id).map(|m| json!({ "pool_id": m.pool_id, "priority": m.priority })).collect::<Vec<_>>(),
        "requests": { "last_24h": ctx.day.get(&row.id).map_or(0, |t| t.requests), "last_7d": ctx.week.get(&row.id).map_or(0, |t| t.requests), "lifetime": ctx.lifetime.get(&row.id).map_or(0, |t| t.requests) },
    })
}

fn usage_json(snapshot: &UsageSnapshot) -> Value {
    let window = |window: &phoebis_oauth::UsageWindow| {
        json!({
            "key": window.key,
            "resets_at": window.resets_at,
            "utilization": window.utilization,
        })
    };
    json!({
        "account_id": snapshot.account_id,
        "error": snapshot.error,
        "extra_usage": snapshot.extra_usage.as_ref().map(|extra| json!({
            "currency": extra.currency,
            "disabled_reason": extra.disabled_reason,
            "is_enabled": extra.is_enabled,
            "monthly_limit": extra.monthly_limit,
            "used_credits": extra.used_credits,
            "utilization": extra.utilization,
        })),
        "fable_window": snapshot.fable_window.as_ref().map(&window),
        "fetched_at": snapshot.fetched_at,
        "identity_generation": snapshot.identity_generation,
        "raw_keys": snapshot.raw_keys,
        "windows": snapshot.windows.iter().map(&window).collect::<Vec<_>>(),
    })
}

fn state_name(state: AccountState) -> &'static str {
    match state {
        AccountState::Disconnected => "disconnected",
        AccountState::FundingRequired => "funding_required",
        AccountState::ReauthRequired => "reauth_required",
        AccountState::Ready => "ready",
        AccountState::Unpaired => "unpaired",
    }
}

fn reported_status(session: &PairingSession) -> &'static str {
    match session.status {
        PairingStatus::Cancelled => "cancelled",
        PairingStatus::CodeReceived { .. } => "code_received",
        PairingStatus::Confirmed => "confirmed",
        PairingStatus::Confirming { .. } => "confirming",
        PairingStatus::Exchanged { .. } => "exchanged",
        PairingStatus::Expired => "expired",
        PairingStatus::Failed(_) => "failed",
        PairingStatus::Pending => "pending",
    }
}

/// A `serde_json` failure quotes the value it tripped on, and a token response is made of credentials,
/// so only the shape complaints this crate writes itself are passed back to the caller.
fn oauth_detail(what: &str, error: OauthError) -> String {
    match error {
        OauthError::Malformed(detail) => format!("{what} response: {detail}"),
        OauthError::Json(_) => format!("{what} response was not the JSON this endpoint returns"),
    }
}

fn pairing_failure(error: PairingError) -> Response {
    let status = match error {
        PairingError::AlreadyConsumed | PairingError::Busy | PairingError::Expired => {
            StatusCode::CONFLICT
        }
        // A pairing session id is an unguessable handle; answering a non-owner with anything but "not
        // found" would tell them the handle they hold is real.
        PairingError::Forbidden | PairingError::NotFound => StatusCode::NOT_FOUND,
        PairingError::Invalid(_) | PairingError::RedirectMismatch | PairingError::StateMismatch => {
            StatusCode::BAD_REQUEST
        }
    };
    failure(status, &error.to_string())
}

/// The row is already written; a snapshot that could not be rebuilt is reported rather than hidden, because
/// until the next successful reload the router still authenticates and routes by the old registry.
pub(crate) async fn reload_or_refuse(state: &AdminState) -> Result<(), Response> {
    match state.router.reload().await {
        Ok(()) => Ok(()),
        Err(ReloadError::Load(error)) => {
            tracing::error!(%error, "a registry write could not be published");
            Err(registry_unavailable())
        }
        Err(ReloadError::Gone(_)) => Err(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        )),
    }
}

/// The answer to a write that landed but that routing has not been told about. Shared, so a route that
/// publishes through something other than `reload_or_refuse` gives a console the same thing to branch on.
pub(crate) fn registry_unavailable() -> Response {
    failure_with_code(
        StatusCode::SERVICE_UNAVAILABLE,
        "registry_unavailable",
        "the registry was written but could not be reloaded; the next successful reload publishes it",
    )
}

pub(crate) fn store_failure(error: StoreError) -> Response {
    tracing::warn!(%error, "admin request failed in the store");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "storage failure")
}

pub(crate) fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Tells an absent field from an explicit `null`, so a `PATCH` can clear a nullable column.
pub(crate) fn double_option<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// A refusal an operator console can branch on without reading the prose, in the same header slot the relay
/// puts its own codes.
pub(crate) fn failure_with_code(status: StatusCode, code: &'static str, message: &str) -> Response {
    let mut response = failure(status, message);
    response.headers_mut().insert(
        HeaderName::from_static("x-phoebis-error-code"),
        HeaderValue::from_static(code),
    );
    response
}

fn set_cookie(status: StatusCode, attributes: &str) -> Response {
    let mut response = status.into_response();
    match HeaderValue::from_str(attributes) {
        Ok(value) => {
            response.headers_mut().insert(SET_COOKIE, value);
            response
        }
        // The value is built from a base64url token and fixed attributes, so this is unreachable short
        // of a bug; refusing the login beats answering with no cookie at all.
        Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR, "cookie could not be set"),
    }
}

#[cfg(test)]
mod tests {
    use super::same_authority;

    #[test]
    fn same_authority_names_the_requests_own_host_case_insensitively() {
        assert!(same_authority("http://localhost:8081", "localhost:8081"));
        assert!(same_authority("HTTP://LocalHost:8081", "localhost:8081"));
        assert!(!same_authority("http://localhost:5173", "localhost:8081"));
        assert!(!same_authority("null", "localhost:8081"));
        assert!(!same_authority("https://localhost:8081", "localhost"));
        assert!(!same_authority("ftp://localhost:8081", "localhost:8081"));
    }
}
