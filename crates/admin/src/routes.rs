//! The `/admin` surface: one gate in front of every route, then the account, pairing, usage, and system
//! handlers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{MatchedPath, Path, Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE,
    TRANSFER_ENCODING, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Extension, Json, Router};
use phoebis_ledger::{lead_over_pace, QuotaPoll};
use phoebis_oauth::{
    authorization_url, control_request, ephemeral_port, exchange_body, loopback_redirect_uri,
    parse_profile, parse_token_response, CallbackInput, ControlKind, LoginKind, OauthError,
    PairingError, PairingSession, PairingStatus, PendingIdentity, PollOutcome, PollStatus,
    ProfileResponse, Started, UsageSnapshot, UsageWindow, FABLE_WINDOW_KEY,
};
use phoebis_relay::DiscoveryError;
use phoebis_routing::{
    account_window_id, is_plan_window, AccountProjection, MemberSpend, OverageEvidence,
    ReloadError, SpendResetSource, SpendWindow, UtilizationSample, WindowReading, FIVE_HOUR,
    SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{
    answers_stamp_token, stamp_token, AccountKind, AccountRow, AccountState, CooldownRow, Identity,
    NewBundle, PoolMember, Settled, StateCause, StoreError,
};
use phoebis_transport::response::ResponseHead;
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::openapi::{ErrorBody, RegistryUnavailable, StorageFailure};
use crate::session::{Authenticated, PERSISTENT_LIFE};
use crate::{
    accounts, analytics, attention, dashboard, guardrails, keepalive, keys, models, openapi, pools,
    pricing, requests, sessions, settings, tls, AdminState, LoginError, NewAccount,
};

/// Covers a slow control-plane answer without letting one stalled exchange hold an admin request open
/// for minutes.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

const COOKIE_NAME: &str = "phoebis_admin";

const LOGIN_PATH: &str = "/admin/session";

/// The route table and the document its registrations build, with no state bound, so the path count reads the
/// document without an `AdminState`.
pub(crate) fn parts() -> (Router<Arc<AdminState>>, utoipa::openapi::OpenApi) {
    let (router, mut api) = OpenApiRouter::with_openapi(openapi::Document::openapi())
        .routes(routes!(list_accounts, accounts::create))
        .routes(routes!(show_account, accounts::patch, accounts::remove))
        .routes(routes!(accounts::clear_funding))
        .routes(routes!(accounts::clear_cooldown))
        .routes(routes!(disconnect))
        .routes(routes!(pause))
        .routes(routes!(accounts::catalog))
        .routes(routes!(accounts::quota))
        .routes(routes!(resume))
        .routes(routes!(usage_latest))
        .routes(routes!(usage_refresh))
        .routes(routes!(verify))
        .routes(routes!(analytics::accounts))
        .routes(routes!(analytics::breakdown))
        .routes(routes!(analytics::budgets))
        .routes(routes!(analytics::burn))
        .routes(routes!(analytics::guardrails))
        .routes(routes!(analytics::placements))
        .routes(routes!(analytics::reconciliations))
        .routes(routes!(analytics::refusal_categories))
        .routes(routes!(analytics::rewrites))
        .routes(routes!(analytics::series))
        .routes(routes!(analytics::unused_allowance))
        .routes(routes!(attention::show))
        .routes(routes!(dashboard::show))
        .routes(routes!(guardrails::list, guardrails::create))
        .routes(routes!(guardrails::presets))
        .routes(routes!(guardrails::test))
        .routes(routes!(
            guardrails::show,
            guardrails::patch,
            guardrails::remove
        ))
        .routes(routes!(keepalive::overview))
        .routes(routes!(keepalive::recipes))
        .routes(routes!(keys::list, keys::create))
        .routes(routes!(keys::patch, keys::remove))
        .routes(routes!(models::key_models))
        .routes(routes!(keys::revoke))
        .routes(routes!(keys::rotate))
        .routes(routes!(crate::ledger::replay_parked))
        .routes(routes!(pairing_start))
        .routes(routes!(pairing_show, pairing_cancel))
        .routes(routes!(pairing_callback))
        .routes(routes!(pairing_confirm))
        .routes(routes!(pools::list, pools::create))
        .routes(routes!(pools::patch, pools::remove))
        .routes(routes!(pools::set_members))
        .routes(routes!(pools::traffic))
        .routes(routes!(pricing::show))
        .routes(routes!(requests::list))
        .routes(routes!(requests::show))
        .routes(routes!(requests::bodies))
        .routes(routes!(crate::routing_log::record))
        .routes(routes!(crate::routing_log::list))
        .routes(routes!(whoami, login, logout))
        .routes(routes!(sessions::list))
        .routes(routes!(sessions::release))
        .routes(routes!(sessions::costs))
        .routes(routes!(settings::show))
        .routes(routes!(system))
        .routes(routes!(tls::reload))
        .route("/admin", any(unknown_admin_route))
        .route("/admin/{*rest}", any(unknown_admin_route))
        .split_for_parts();
    openapi::Examples.modify(&mut api);
    (router, api)
}

pub fn router(state: Arc<AdminState>) -> Router {
    let (router, api) = parts();
    let document = Bytes::from(api.to_json().expect("the admin document serializes"));
    let build = state.build;
    router
        .layer(from_fn_with_state(state.clone(), gate))
        .route(
            openapi::DOCUMENT_PATH,
            get(move || {
                let document = document.clone();
                async move { openapi::document(document, build) }
            }),
        )
        .with_state(state)
}

/// How many paths the served document holds, read off the document itself, so the merged-app test asserts the
/// router serves the admin crate's document rather than a figure written twice.
pub fn path_count() -> usize {
    parts().1.paths.paths.len()
}

/// Who is calling, as the gate resolved them: the handle that names this admin session in pairing
/// ownership and in the mutation log, and the credential that admitted it. The cookie or bearer that produced it
/// never leaves the gate.
#[derive(Clone)]
pub(crate) struct Caller {
    pub session_id: String,
    /// `"bearer"` or `"session"`.
    pub credential: &'static str,
    /// A kept login's token the gate verified. False for a map session and the bearer, so only a verified token
    /// moves the session epoch at a logout.
    pub persistent: bool,
}

/// The session id every bearer caller carries, so a pairing one scripted call starts the next confirms.
pub(crate) const BEARER_SESSION_ID: &str = "bearer";

/// The id a create handler made, which its path does not carry; read by the mutation log.
#[derive(Clone)]
pub(crate) struct CreatedId(pub String);

/// The entity and id a handler names to the mutation log itself, ahead of what the route's path says: the confirm
/// of a pairing started with `new` names the account it created, not the session its path carries.
#[derive(Clone)]
pub(crate) struct LoggedEntity(pub &'static str, pub String);

/// Authentication, the origin check, `Cache-Control`, `X-Content-Type-Options`, `x-phoebis-build` and the mutation
/// log for every route at once, so no handler can be added that quietly skips one of them.
async fn gate(State(state): State<Arc<AdminState>>, mut request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let mutation = is_mutation(&method);
    // The matched pattern names the route in the line; the request's own path supplies the id's segment.
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || request.uri().path().to_string(),
        |m| m.as_str().to_string(),
    );
    let path = request.uri().path().to_string();
    let mut response = match admit(&state, &mut request) {
        None => {
            let caller = request.extensions().get::<Caller>().cloned();
            // Taken out before the handler runs, so the renewed token never reaches a handler.
            let renewal = request.extensions_mut().remove::<Renewal>();
            let mut response = next.run(request).await;
            if let Some(Renewal(cookie)) = renewal {
                response.headers_mut().append(SET_COOKIE, cookie);
            }
            if mutation {
                log_mutation(&method, &route, &path, caller.as_ref(), &response);
            }
            response
        }
        Some(refusal) => {
            if mutation {
                log_mutation(&method, &route, &path, None, &refusal);
            }
            refusal
        }
    };
    // An admin answer describes credentials and usage; a cache between the browser and this process has
    // no business holding any of it, and a browser has no business reading a body as anything but its type.
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // On refusals too, so a console that compares it to its own bundle's commit reads it before any login.
    headers.insert(
        HeaderName::from_static(openapi::BUILD_HEADER),
        openapi::build_value(state.build),
    );
    response
}

fn is_mutation(method: &Method) -> bool {
    matches!(
        *method,
        Method::DELETE | Method::PATCH | Method::POST | Method::PUT
    )
}

/// The trail of what the operator changed lives in the host's journal and nowhere else, so the line carries what
/// an operator reading it needs and nothing a log must not hold: no body, no query string, no secret, no cookie.
fn log_mutation(
    method: &Method,
    route: &str,
    path: &str,
    caller: Option<&Caller>,
    response: &Response,
) {
    let (entity, id) = match response.extensions().get::<LoggedEntity>() {
        Some(LoggedEntity(entity, id)) => (*entity, Some(id.clone())),
        None => entity_of(route, path, response.extensions().get::<CreatedId>()),
    };
    let status = response.status().as_u16();
    let id = id.as_deref().unwrap_or("-");
    match caller {
        Some(caller) => tracing::info!(
            %method, %route, status, %entity, %id, credential = %caller.credential,
            session = %caller.session_id, "admin mutation"
        ),
        None => tracing::info!(
            %method, %route, status, %entity, %id, credential = %"none", "admin mutation"
        ),
    }
}

/// The entity a mutation touched, read off the route's own kind segment, and its id: the path's segment where
/// the pattern names one, else the id a create handler left on its answer.
fn entity_of(
    route: &str,
    path: &str,
    created: Option<&CreatedId>,
) -> (&'static str, Option<String>) {
    let pattern: Vec<&str> = route.split('/').collect();
    let actual: Vec<&str> = path.split('/').collect();
    let segment = |name: &str| {
        pattern
            .iter()
            .position(|s| *s == name)
            .and_then(|i| actual.get(i))
            .map(|s| s.to_string())
    };
    let created = created.map(|c| c.0.clone());
    match pattern.get(2).copied() {
        Some("accounts") => ("account", segment("{id}").or(created)),
        Some("guardrails") => ("guardrail", segment("{id}").or(created)),
        Some("keys") => ("key", segment("{id}").or(created)),
        Some("ledger") => ("ledger", None),
        Some("pools") => ("pool", segment("{id}").or(created)),
        Some("oauth") => ("pairing", segment("{id}").or(created)),
        Some("sessions") => (
            "session",
            segment("{key}")
                .zip(segment("{session}"))
                .map(|(k, s)| format!("{k}/{s}")),
        ),
        Some("session") => ("admin_session", None),
        _ => ("none", None),
    }
}

/// `None` admits the request; anything else is the refusal to answer with.
fn admit(state: &AdminState, request: &mut Request) -> Option<Response> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let mutation = is_mutation(&method);

    // A bearer is the operator's own hand: a browser cannot attach `Authorization` to a cross-site request without a
    // preflight the admin API never answers, so the Origin check is the cookie's and not the bearer's. The login
    // route stays the cookie's door and ignores a bearer on it.
    let bearer = (!(method == Method::POST && path == LOGIN_PATH))
        .then(|| presented_bearer(request.headers()))
        .flatten();
    // A browser attaches `Origin` to every cross-site write. A presented `Origin` must name the request's own `Host`,
    // since another authority is another page on this host driving the console, while a request with no `Origin`
    // (curl, a subcommand, the health probe) is the operator's own hand. Where this listener terminates TLS, or a
    // front says the browser's leg does, the `Origin` must be `https://` too: a page on this host over plain HTTP
    // could otherwise drive a bodiless write with a cookie some browsers send across schemes. A front that
    // terminates TLS passes `Host` through beside `X-Forwarded-Proto`, so both headers name the front, and a forged
    // header on a plain listener only costs the forger the `http://` origin it would need. The login is checked
    // too: a forced login is the one mutation an unauthenticated caller can reach.
    if bearer.is_none() && mutation {
        if let Some(origin) = request.headers().get(ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = request
                .headers()
                .get(HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let https_only = state.tls || forwarded_https(request.headers());
            if !same_authority(origin, host, https_only) {
                return Some(failure(StatusCode::FORBIDDEN, "origin is not allowed"));
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

    if let Some(bearer) = bearer {
        return match state
            .sessions
            .bearer(&bearer, &state.secrets, (state.clock)())
        {
            Ok(()) => {
                request.extensions_mut().insert(Caller {
                    session_id: BEARER_SESSION_ID.to_string(),
                    credential: "bearer",
                    persistent: false,
                });
                None
            }
            Err(LoginError::Throttled) => Some(failure(
                StatusCode::TOO_MANY_REQUESTS,
                "too many failed logins, try again in a minute",
            )),
            Err(LoginError::Invalid) => {
                Some(failure(StatusCode::UNAUTHORIZED, "secret does not match"))
            }
        };
    }

    let session = presented_cookie(request.headers())
        .and_then(|cookie| state.sessions.authenticate(&cookie, (state.clock)()));
    match session {
        Some(Authenticated::Map { id }) => {
            request.extensions_mut().insert(Caller {
                session_id: id,
                credential: "session",
                persistent: false,
            });
            None
        }
        Some(Authenticated::Persistent { id, renewed_cookie }) => {
            // The logout answers its clear alone: a renewal beside it would leave the browser a live token.
            if !(method == Method::DELETE && path == LOGIN_PATH) {
                let secure = state.tls || forwarded_https(request.headers());
                // A base64url token and fixed attributes always make a header value. Were one ever refused, the
                // answer would go without its renewal and the presented token would stay valid to its own end.
                if let Ok(cookie) = HeaderValue::from_str(&kept_cookie(&renewed_cookie, secure)) {
                    request.extensions_mut().insert(Renewal(cookie));
                }
            }
            request.extensions_mut().insert(Caller {
                session_id: id,
                credential: "session",
                persistent: true,
            });
            None
        }
        // A tab holding a dead cookie still needs the browser's copy cleared, so the logout answers it rather
        // than the gate refusing it.
        None if method == Method::DELETE && path == LOGIN_PATH => None,
        None => Some(failure(StatusCode::UNAUTHORIZED, "no admin session")),
    }
}

/// `origin` names the request's own authority: its scheme stripped, the rest equals `host` ignoring case. Under
/// `https_only` the scheme must be `https`, the rule when the listener terminates TLS or a front reports that the
/// browser's leg does, since a page on this host over plain HTTP could otherwise drive a bodiless write with a
/// cookie some browsers send across schemes; a plain listener behind no front admits either scheme.
pub(crate) fn same_authority(origin: &str, host: &str, https_only: bool) -> bool {
    let http = (!https_only)
        .then(|| origin.get(..7))
        .flatten()
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &origin[7..]);
    let authority = http.or_else(|| {
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

/// Whether a front's word says the browser's leg is TLS: the first comma-separated value of `X-Forwarded-Proto`,
/// case-insensitively `https`. A plain-HTTP forger earns a cookie its browser never returns over `http://`.
pub(crate) fn forwarded_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|first| first.trim().eq_ignore_ascii_case("https"))
}

/// The value of `Authorization: Bearer ...` when the header carries that scheme (case-insensitive), trimmed.
pub(crate) fn presented_bearer(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = header.trim_start().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| value.trim().to_string())
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

/// The `Set-Cookie` a kept login's answer carries, the gate's handoff from `admit` to the answer.
#[derive(Clone)]
struct Renewal(HeaderValue);

/// The login's attributes with the token's 30 days as `Max-Age`, so the browser keeps the cookie as long as the
/// router would verify it.
fn kept_cookie(token: &str, secure: bool) -> String {
    let mut attributes = format!(
        "{COOKIE_NAME}={token}; Path=/admin; HttpOnly; SameSite=Strict; Max-Age={}",
        PERSISTENT_LIFE.as_secs()
    );
    if secure {
        attributes.push_str("; Secure");
    }
    attributes
}

/// The login's body.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct LoginInput {
    /// The admin secret.
    pub secret: String,
    /// Keep the login for 30 days from its last read, across a restart, until a secret rotation or a logout of a kept
    /// login. False or absent, the session lives 15 minutes idle and 12 hours in all.
    #[serde(default)]
    #[schema(default = false)]
    pub stay_logged_in: bool,
}

/// Whether the cookie the console holds still names a session, by the id pairing ownership uses, never by the
/// cookie value.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionView {
    /// True for a login that asked to stay, false for a plain login and for a bearer caller.
    pub persistent: bool,
    /// The session's id as pairing ownership and the mutation log name it, the same across a kept login's renewals.
    /// `bearer` for a bearer caller.
    #[schema(example = "bearer")]
    pub session_id: String,
}

/// Log in with the admin secret.
///
/// No session required: this is the login itself. The answer sets the `phoebis_admin` cookie, with `Secure` when
/// the admin listener terminates TLS or the request carries `X-Forwarded-Proto: https`, and carries no body. See
/// `GET /admin/session` for what the session answers afterwards. A bearer on this operation is ignored: the login
/// stays the cookie's door.
#[utoipa::path(
    post,
    path = "/admin/session",
    operation_id = "session_login",
    tag = "Session",
    request_body(content = LoginInput, description = "The admin secret, and `stay_logged_in` for a login that stays", content_type = "application/json"),
    responses(
        (status = 204, description = "Logged in. The `phoebis_admin` cookie is set and the answer carries no body.",
            headers(("Set-Cookie" = String, description = "`phoebis_admin=<token>; Path=/admin; HttpOnly; SameSite=Strict`, with `; Secure` where the admin listener terminates TLS or the request carries `X-Forwarded-Proto: https`. The session lives 15 minutes idle and 12 hours in all. With `stay_logged_in` true the token is signed and the cookie carries `; Max-Age=2592000` before any `; Secure`: the session lives 30 days from its last read, across a restart, every answer to it but its logout's sets a renewed cookie of the same shape and life, and a secret rotation or a logout of a kept login ends it."))),
        (status = 400, description = "`body must carry a secret`", body = ErrorBody, example = json!({"error": "body must carry a secret"})),
        (status = 401, description = "`secret does not match`", body = ErrorBody, example = json!({"error": "secret does not match"})),
        (status = 429, description = "`too many failed logins, try again in a minute`", body = ErrorBody, example = json!({"error": "too many failed logins, try again in a minute"})),
    ),
    security(),
)]
async fn login(State(state): State<Arc<AdminState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(input) = serde_json::from_slice::<LoginInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry a secret");
    };
    let now = (state.clock)();
    let secure = state.tls || forwarded_https(&headers);
    let login = if input.stay_logged_in {
        state
            .sessions
            .login_persistent(&input.secret, now)
            .map(|token| kept_cookie(&token, secure))
    } else {
        state
            .sessions
            .login(&input.secret, &state.secrets, now)
            .map(|cookie| {
                let mut attributes =
                    format!("{COOKIE_NAME}={cookie}; Path=/admin; HttpOnly; SameSite=Strict");
                if secure {
                    attributes.push_str("; Secure");
                }
                attributes
            })
    };
    match login {
        Ok(attributes) => set_cookie(StatusCode::NO_CONTENT, &attributes),
        Err(LoginError::Throttled) => failure(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed logins, try again in a minute",
        ),
        Err(LoginError::Invalid) => failure(StatusCode::UNAUTHORIZED, "secret does not match"),
    }
}

/// End the caller's own session.
///
/// The gate admits this operation without a live session: a cookie that names no session, an expired one included,
/// answers the same `204` and the same clear, so a tab holding a dead cookie can still drop it. The logout of a kept
/// login ends every kept login of the install, on this process and after a restart.
#[utoipa::path(
    delete,
    path = "/admin/session",
    operation_id = "session_logout",
    tag = "Session",
    responses(
        (status = 204, description = "The session is ended and the browser's cookie cleared. A kept login's logout ends every kept login, each browser's included, and the answer carries the clear alone.",
            headers(("Set-Cookie" = String, description = "`phoebis_admin=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0`"))),
        (status = 500, response = StorageFailure),
    ),
    security(),
)]
async fn logout(
    State(state): State<Arc<AdminState>>,
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
) -> Response {
    // The gate's verdict alone moves the epoch: it admits this route with no live session, so a cookie that only
    // looks like a token must end nothing.
    if caller.is_some_and(|Extension(caller)| caller.persistent) {
        // A refused bump leaves the kept logins valid, so the answer says the logout failed.
        if let Err(error) = state.sessions.logout_persistent() {
            return store_failure(error);
        }
    }
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

/// Whether the cookie the console holds still names a session.
///
/// Answers `session_id`, the id pairing ownership uses, never the cookie value, and `persistent`, true for a login
/// that asked to stay. A bearer caller answers `session_id` `bearer` and `persistent` false.
#[utoipa::path(
    get,
    path = "/admin/session",
    operation_id = "session_show",
    tag = "Session",
    responses((status = 200, description = "The session's id", body = SessionView)),
)]
async fn whoami(Extension(caller): Extension<Caller>) -> Response {
    Json(SessionView {
        persistent: caller.persistent,
        session_id: caller.session_id,
    })
    .into_response()
}

/// List the accounts.
#[utoipa::path(
    get,
    path = "/admin/accounts",
    operation_id = "accounts_list",
    tag = "Accounts",
    responses(
        (status = 200, description = "The accounts.", body = AccountList),
        (status = 500, response = StorageFailure),
    ),
)]
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
    Json(AccountList { accounts }).into_response()
}

/// Show an account.
#[utoipa::path(
    get,
    path = "/admin/accounts/{id}",
    operation_id = "account_show",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 200, description = "The same keys as a row of `GET /admin/accounts`, at the top level rather than nested under `accounts`.", body = Account),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
    ),
)]
async fn show_account(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    match account(&state, &id) {
        Ok(row) => Json(account_json(&state, &row).await).into_response(),
        Err(refusal) => refusal,
    }
}

/// Pause the account.
#[utoipa::path(
    post,
    path = "/admin/accounts/{id}/pause",
    operation_id = "account_pause",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 204, description = "The account is paused."),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
async fn pause(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    set_enabled(&state, &id, false).await
}

/// Resume the account.
#[utoipa::path(
    post,
    path = "/admin/accounts/{id}/resume",
    operation_id = "account_resume",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 204, description = "The account is resumed."),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
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

/// Disconnect the account's credential.
#[utoipa::path(
    delete,
    path = "/admin/accounts/{id}/credentials",
    operation_id = "account_credentials_delete",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 204, description = "The credential goes, request history stays, the slot lands on `disconnected`, and an `api_key` account moves on again to `unpaired`, since it has no pairing dialog to leave `disconnected` through. A re-pair stores a fresh secret in one `PATCH /admin/accounts/{id}`."),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
// Disconnects the slot: the lifecycle bump deletes both credential kinds, and everything still holding
// the old identity's state goes with them.
async fn disconnect(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    match state.store.bump_lifecycle(&id, StateCause::Operator) {
        Ok(_) => {}
        Err(StoreError::AccountNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "account not found")
        }
        Err(error) => return store_failure(error),
    }
    // The bump deleted the bundle outside the manager, so the copy it keeps goes with it.
    state.manager.invalidate(&id);
    state.catalog.evict(&id);
    // The bump leaves the slot `disconnected`, which an OAuth slot leaves again by pairing. An API-key slot
    // has no pairing to run, so it rests where a slot that never had a key rests.
    if row.kind == AccountKind::ApiKey {
        if let Err(error) = state
            .store
            .set_state(&id, AccountState::Unpaired, StateCause::Operator)
        {
            return store_failure(error);
        }
    }
    state.usage.invalidate(&id);
    let now = (state.clock)();
    for (session_id, owner) in state.vault.discard_for_account(&id, now) {
        let _ = state.pairing.cancel(&session_id, &owner, now);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The poll's standing on an account and the sentence that answers while nothing composes a snapshot. The poll never
/// asks an account without a credential, so no read is coming: the card says what the operator can do, and a
/// standing left from before the credential went would describe nothing current.
fn poll_standing(state: &AdminState, row: &AccountRow) -> (Option<PollStatus>, &'static str) {
    if matches!(
        row.state,
        AccountState::Disconnected | AccountState::Unpaired
    ) {
        (
            None,
            "no credential is stored for this account, so pair it to load quota",
        )
    } else {
        (
            state.usage.poll_status(&row.id),
            "the poll has not read this account yet",
        )
    }
}

/// Answer the poll's latest snapshot and the poll's standing without reading the endpoint.
///
/// When the snapshot holds no windows, the windows the router holds answer in their place (persisted from the last
/// run, or read since), so `error` is set only when no windows exist at all. The windows the router holds are the last
/// reading carried forward by the router's own requests since it, so their utilization can read above that reading's
/// while `fetched_at` names the reading's instant. Before the poll first reads an account that holds none, `error`
/// reads `the poll has not read this account yet` and `fetched_at` is null. A disconnected or unpaired account, which
/// the poll never reads, answers `no credential is stored for this account, so pair it to load quota` with `poll` null
/// while the router holds no windows for it, and otherwise the windows of its last reading with `error` and `poll`
/// null. `GET /admin/accounts` carries the same `error`, `fetched_at`, `held_until` and `poll` on each `oauth` row, as
/// its `usage` block. While the account waits out a 429, `error` is the latest snapshot's (while no windows exist,
/// `usage request rate-limited` after a usage 429 and `no usage fetched yet` after a profile 429 before any reading),
/// `poll.last_error` and the `error` a refresh answers inside the hold name the endpoint and no instant, and the hold's
/// instant rides `held_until` on the usage block and `next_attempt_at` on the poll.
#[utoipa::path(
    get,
    path = "/admin/accounts/{id}/usage",
    operation_id = "account_usage",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 200, description = "The poll's latest snapshot and standing.", body = Usage),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
    ),
)]
// `GET .../usage`: the poll's latest snapshot and standing, with no read of the endpoint. A snapshot with no
// windows gives way to the windows the router holds, persisted from the last run or read since, so a card
// never reads as an error while windows exist, and the error answers only when none do.
async fn usage_latest(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let (poll, unread) = poll_standing(&state, &row);
    let latest = state.usage.latest_for(&row);
    let held = match &latest {
        Some(snapshot) if has_windows(snapshot) => None,
        _ => state.router.samples().await.ok().and_then(|samples| {
            samples
                .into_iter()
                .find_map(|(account_id, sample)| (account_id == id).then_some(sample))
        }),
    };
    // The usage answer carries no lead, so it reads no term for one.
    let answer = compose_usage(&id, latest, held, None, (state.clock)());
    Json(usage_json(
        &id,
        answer.as_ref().ok_or(unread),
        poll.as_ref(),
    ))
    .into_response()
}

/// Read the usage endpoint now.
///
/// Reads the endpoint now unless the account is backing off, through the same path as the scheduled poll: a clean
/// reading is recorded to the ledger and handed to the router, so the account's windows change on it, and the Quota
/// view's timeline when the reading's five-minute bucket holds no sample yet. The hold, the concurrency cap and the
/// coalescing bound the read, so a held-down refresh makes one request per account per fetch.
///
/// While the account's `plan_ended_at` is set, the profile is read first: a profile with a plan clears the flag and
/// the usage endpoint is read as above, and with no plan no usage request goes out and the answer is the `GET`'s,
/// `poll.last_error` reading `plan ended`.
#[utoipa::path(
    post,
    path = "/admin/accounts/{id}/usage/refresh",
    operation_id = "account_usage_refresh",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 200, description = "The same shape as `GET /admin/accounts/{id}/usage`, from the reading the poll made, its `error` set when the read failed or the account is backing off.", body = Usage),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, description = "The upstream error text, once the usage service itself fails.", body = ErrorBody, example = json!({"error": "<the upstream error text>"})),
    ),
)]
// `POST .../usage/refresh`: the one manual read of the endpoint, through the poll's own path, so the reading is
// recorded to the ledger and handed to the router as a scheduled read is, and the answer is that reading, its
// error set when the read failed or the account is backing off. The hold, the concurrency cap and the coalescing
// bound it, so a held-down refresh button cannot turn into a stream of requests against a per-account rate limit.
async fn usage_refresh(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let row = match account(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    // An account whose plan ended sends no usage request until a profile read finds a plan, so the button reads the
    // profile first and, with no plan back, answers the standing as the `GET` does.
    if QuotaPoll::plan_paused(&state.store, &id) {
        QuotaPoll::refresh_profile(
            &state.store,
            &state.usage,
            &state.tables,
            &state.ledger,
            &state.router,
            &row,
        )
        .await;
        if QuotaPoll::plan_paused(&state.store, &id) {
            return usage_latest(State(state), Path(id)).await;
        }
    }
    match QuotaPoll::poll_account(
        &state.store,
        &state.usage,
        &state.tables,
        &state.ledger,
        &state.router,
        &row,
    )
    .await
    {
        Ok(snapshot) => {
            let poll = state.usage.poll_status(&id);
            Json(usage_json(&id, Ok(&snapshot), poll.as_ref())).into_response()
        }
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

/// Verify an API key.
#[utoipa::path(
    post,
    path = "/admin/accounts/{id}/verify",
    operation_id = "account_verify",
    tag = "Accounts",
    params(("id" = String, Path, description = "The account's id")),
    responses(
        (status = 200, description = "The key check's result.", body = Verify),
        (status = 400, description = "`only an API-key account is verified this way`", body = ErrorBody, example = json!({"error": "only an API-key account is verified this way"})),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 409, description = "`account holds no API key`", body = ErrorBody, example = json!({"error": "account holds no API key"})),
        (status = 500, response = StorageFailure),
        (status = 502, description = "`upstream: {error}`", body = ErrorBody, example = json!({"error": "upstream: {error}"})),
    ),
)]
// Answers whether an API key still works, by asking the cheapest authenticated endpoint there is: the
// catalog discovery, which goes to the account's own host over the pool that host belongs to.
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
    Json(Verify { ok, status }).into_response()
}

/// Exactly one of `account_id` or `new`, and the login kind.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct PairingStartInput {
    /// An existing OAuth account: its first pairing, or a re-pair after Disconnect or a credential failure.
    #[serde(default)]
    pub account_id: Option<String>,
    /// `claude_ai`, the default, or `console`.
    #[serde(default)]
    #[schema(example = "claude_ai")]
    pub login: Option<String>,
    /// The router reserves the id for the session and writes the row at the confirm, so a session that is
    /// canceled, expires or fails leaves no account.
    #[serde(default, rename = "new")]
    pub new_account: Option<NewAccountInput>,
}

/// An account that does not exist yet.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct NewAccountInput {
    /// `[a-z0-9][a-z0-9-]{0,63}`.
    pub id: String,
    /// At most 256 bytes.
    pub label: String,
}

/// Either `url` alone, or `code` and `state` together.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct PairingCallbackInput {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// The whole callback URL the browser landed on.
    #[serde(default)]
    pub url: Option<String>,
}

/// The pairing session the browser is sent to.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairingStarted {
    pub id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub expires_at: i64,
}

/// A pairing session as its owner reads it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairingSessionView {
    /// The reserved id for a session started with `new`.
    pub account_id: String,
    pub expires_at: i64,
    pub id: String,
    /// One of eight values: `canceled`, `code_received`, `confirmed`, `confirming`, `exchanged`, `expired`,
    /// `failed`, `pending`.
    #[schema(example = "canceled")]
    pub status: String,
}

/// The exchange's result, once the token exchange and profile lookup succeed.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exchanged {
    /// `exchanged`.
    #[schema(example = "exchanged")]
    pub status: String,
    pub pending: PendingIdentityView,
}

/// The identity the exchange answered, which the confirm commits.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingIdentityView {
    pub account_uuid: String,
    pub email: String,
    pub organization_name: Option<String>,
    pub organization_uuid: Option<String>,
}

/// The committed pairing.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Confirmed {
    pub account_id: String,
    /// `confirmed`.
    #[schema(example = "confirmed")]
    pub status: String,
    /// The first usage poll runs in the background, so the account's windows land a few seconds after this
    /// answer.
    pub first_poll_in_flight: bool,
}

/// Start a pairing session.
#[utoipa::path(
    post,
    path = "/admin/oauth/sessions",
    operation_id = "pairing_start",
    tag = "Pairing",
    request_body(content = PairingStartInput, content_type = "application/json"),
    responses(
        (status = 201, description = "The started session.", body = PairingStarted),
        (status = 400, description = "- `a new account needs a label`\n- `body must carry an account_id or a new account`\n- `id must match [a-z0-9][a-z0-9-]{0,63}`\n- `label is at most 256 bytes`\n- `login must be claude_ai or console`\n- `only an OAuth account is paired this way`", body = ErrorBody, example = json!({"error": "a new account needs a label"})),
        (status = 404, description = "`account not found`", body = ErrorBody, example = json!({"error": "account not found"})),
        (status = 409, description = "- `a pairing session is already in progress for this account`: another admin session's pairing stands, or the account's pairing is being confirmed. A start by the admin session that owns the standing pairing replaces it, and the old link stops working.\n- `an account with this id exists`", body = ErrorBody, example = json!({"error": "a pairing session is already in progress for this account"})),
        (status = 500, response = StorageFailure),
    ),
)]
async fn pairing_start(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    body: Bytes,
) -> Response {
    let malformed = || {
        failure(
            StatusCode::BAD_REQUEST,
            "body must carry an account_id or a new account",
        )
    };
    let Ok(input) = serde_json::from_slice::<PairingStartInput>(&body) else {
        return malformed();
    };
    let (account_id, new_account) = match (input.account_id, input.new_account) {
        (Some(account_id), None) => (account_id, None),
        (None, Some(new_account)) => (new_account.id.clone(), Some(new_account)),
        _ => return malformed(),
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
    // The lifecycle the pairing starts from, and the row a start with `new` leaves for the confirm to write.
    let (lifecycle_generation, new_account) = match new_account {
        None => {
            let row = match account(&state, &account_id) {
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
            (row.lifecycle_generation, None)
        }
        Some(new_account) => {
            if !accounts::valid_slug(&new_account.id) {
                return failure(
                    StatusCode::BAD_REQUEST,
                    "id must match [a-z0-9][a-z0-9-]{0,63}",
                );
            }
            if let Err(refusal) = accounts::within_text_bound("label", Some(&new_account.label)) {
                return refusal;
            }
            let label = new_account.label.trim().to_string();
            if label.is_empty() {
                return failure(StatusCode::BAD_REQUEST, "a new account needs a label");
            }
            // No row is written until the confirm, so a held id is refused here. A create that lands in between
            // meets the store's constraint at the confirm instead.
            match state.store.account(&new_account.id) {
                Ok(None) => {}
                Ok(Some(_)) => return failure(StatusCode::CONFLICT, accounts::DUPLICATE_ID),
                Err(error) => return store_failure(error),
            }
            // A row born at the confirm starts its lifecycle at 1, as a created row does.
            (
                1,
                Some(NewAccount {
                    id: new_account.id,
                    label,
                }),
            )
        }
    };
    // The provider knows two redirects for this client: a loopback URL on an OS-assigned port, and a page on
    // platform.claude.com that shows the code. The loopback one keeps the browser off the provider's page: the
    // navigation to localhost fails to load and the operator pastes the URL from the address bar, or the code.
    let redirect_uri = loopback_redirect_uri(ephemeral_port());
    let now = (state.clock)();
    let Started {
        session,
        superseded,
    } = match state
        .pairing
        .start(&account_id, &caller.session_id, &redirect_uri, now)
    {
        Ok(started) => started,
        Err(error) => return pairing_failure(error),
    };
    // The replaced pairing can never be confirmed, so its bundle leaves memory now rather than at the sweep.
    if let Some(id) = superseded {
        state.vault.discard(&id, now);
    }
    state.vault.open(
        &session.id,
        &account_id,
        &caller.session_id,
        lifecycle_generation,
        session.deadline,
        now,
        new_account,
    );
    let url = authorization_url(
        state.profile,
        kind,
        &redirect_uri,
        state.profile.control.default_login_scopes,
        &session.pkce.challenge,
        &session.state,
    );
    let mut response = (
        StatusCode::CREATED,
        Json(PairingStarted {
            id: session.id.clone(),
            authorization_url: url,
            redirect_uri,
            expires_at: session.deadline,
        }),
    )
        .into_response();
    response
        .extensions_mut()
        .insert(CreatedId(session.id.clone()));
    response
}

/// Read a pairing session.
#[utoipa::path(
    get,
    path = "/admin/oauth/sessions/{id}",
    operation_id = "pairing_show",
    tag = "Pairing",
    params(("id" = String, Path, description = "The pairing session's id")),
    responses(
        (status = 200, description = "The session and its status.", body = PairingSessionView),
        (status = 404, description = "`pairing session not found`, including for a session id that exists but belongs to a different caller's session: an unguessable handle answers the same as one that never existed.", body = ErrorBody, example = json!({"error": "pairing session not found"})),
    ),
)]
async fn pairing_show(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    let Some(session) = state.pairing.get(&id, &caller.session_id, (state.clock)()) else {
        return failure(StatusCode::NOT_FOUND, "pairing session not found");
    };
    let status = reported_status(&session).to_string();
    Json(PairingSessionView {
        account_id: session.account_id,
        expires_at: session.deadline,
        id: session.id,
        status,
    })
    .into_response()
}

/// Cancel a pairing session.
#[utoipa::path(
    delete,
    path = "/admin/oauth/sessions/{id}",
    operation_id = "pairing_cancel",
    tag = "Pairing",
    params(("id" = String, Path, description = "The pairing session's id")),
    responses(
        (status = 204, description = "The session is canceled."),
        (status = 404, description = "- `pairing session not found`\n- `admin session does not own this pairing session`", body = ErrorBody, example = json!({"error": "pairing session not found"})),
        (status = 409, description = "- `pairing session expired`\n- `pairing session already consumed`", body = ErrorBody, example = json!({"error": "pairing session expired"})),
    ),
)]
async fn pairing_cancel(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    let now = (state.clock)();
    match state.pairing.cancel(&id, &caller.session_id, now) {
        Ok(()) => {
            state.vault.discard(&id, now);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => pairing_failure(error),
    }
}

/// Deliver the upstream's callback.
#[utoipa::path(
    post,
    path = "/admin/oauth/sessions/{id}/callback",
    operation_id = "pairing_callback",
    tag = "Pairing",
    params(("id" = String, Path, description = "The pairing session's id")),
    request_body(content = PairingCallbackInput, content_type = "application/json"),
    responses(
        (status = 200, description = "The identity the upstream answered.", body = Exchanged),
        (status = 400, description = "- `body must carry the callback url, or a code and a state`\n- `callback does not match the registered redirect`\n- `invalid callback: <detail>`\n- `state does not match the pairing session`", body = ErrorBody, example = json!({"error": "body must carry the callback url, or a code and a state"})),
        (status = 404, description = "- `pairing session not found`\n- `admin session does not own this pairing session`", body = ErrorBody, example = json!({"error": "pairing session not found"})),
        (status = 409, description = "- `pairing session expired`\n- `pairing session already consumed`\n- `pairing session did not accept the callback`\n- `the pairing session was closed while the code was being exchanged`", body = ErrorBody, example = json!({"error": "pairing session expired"})),
        (status = 502, description = "The upstream's detail, never the token itself, when the exchange is rejected.", body = ErrorBody, example = json!({"error": "<the upstream's detail>"})),
    ),
)]
async fn pairing_callback(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let now = (state.clock)();
    let malformed = || {
        failure(
            StatusCode::BAD_REQUEST,
            "body must carry the callback url, or a code and a state",
        )
    };
    let Ok(input) = serde_json::from_slice::<PairingCallbackInput>(&body) else {
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
            state.vault.discard(&id, now);
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
        state.vault.discard(&id, now);
        return pairing_failure(error);
    }
    if !state
        .vault
        .record_exchange(&id, identity.clone(), bundle, profile, now)
    {
        return failure(
            StatusCode::CONFLICT,
            "the pairing session was closed while the code was being exchanged",
        );
    }
    Json(Exchanged {
        status: "exchanged".into(),
        pending: PendingIdentityView {
            account_uuid: identity.account_uuid,
            email: identity.email,
            organization_name: identity.organization_name,
            organization_uuid: identity.organization_uuid,
        },
    })
    .into_response()
}

/// Confirm the pairing into a credential.
///
/// The answer comes once the account's credential is committed, its state set to `ready`, its usage cache
/// invalidated, and its cached catalog evicted, fetched again on the next read. The answer does not wait for the usage
/// endpoint: the confirm reads it right after answering, records the first sample and hands it to the router, so the
/// account's windows show within seconds. For a session started with `new`, the account row is created `ready` with
/// its identity, its credential and its profile in one write, and the mutation log names the created account.
#[utoipa::path(
    post,
    path = "/admin/oauth/sessions/{id}/confirm",
    operation_id = "pairing_confirm",
    tag = "Pairing",
    params(("id" = String, Path, description = "The pairing session's id")),
    responses(
        (status = 200, description = "The account, the confirmed status, and `first_poll_in_flight`, true when the confirm started the account's first usage poll.", body = Confirmed),
        (status = 404, description = "- `pairing session not found`\n- `admin session does not own this pairing session`\n- `account not found`", body = ErrorBody, example = json!({"error": "pairing session not found"})),
        // The header goes ahead of `example`: utoipa-gen 6 takes the comma after an example as its own, so an
        // attribute after it fails to parse.
        (status = 409, description = "- `pairing session expired`\n- `pairing session already consumed`\n- `pairing session has no exchanged credential`\n- `the account was disconnected while pairing`\n- `the credential changed while pairing, so start the pairing again`\n- `{label} holds a credential sealed under another storage key, so disconnect it before pairing again`, which carries the `sealed_under_another_key` code\n- `this slot is already paired to a different upstream account`\n- `that upstream account is already paired to another slot`\n- `an account with this id exists`",
            headers(("x-phoebis-error-code" = String, description = "`sealed_under_another_key`: the slot's stored credential does not open under this router's storage key")),
            body = ErrorBody, example = json!({"error": "pairing session expired"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
// Commits a pairing. An existing row passes four checks that all have to hold at once. A row the start
// reserved with `new` is written by the one create, which the store refuses when another slot holds the
// identity or a create took the id meanwhile. Every one of them refuses rather than repairs, and every
// refusal drops the pending bundle, so no path through here leaves a credential behind that a later call
// could commit.
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
            state.vault.discard(&id, now);
        }
        return pairing_failure(error);
    }
    let conflict = |state: &AdminState, message: &'static str| {
        let _ = state.pairing.fail(&id, &caller.session_id, message, now);
        state.vault.discard(&id, now);
        failure(StatusCode::CONFLICT, message)
    };
    let unstored = |state: &AdminState, error: StoreError| {
        let _ = state
            .pairing
            .fail(&id, &caller.session_id, "storage failure", now);
        state.vault.discard(&id, now);
        store_failure(error)
    };
    // AES-GCM cannot tell a blob sealed under another storage key from a damaged one, so both answer this, and the
    // operator's way out of either is Disconnect, which deletes the blob.
    let sealed = |state: &AdminState, label: &str| {
        let message = format!(
            "{label} holds a credential sealed under another storage key, so disconnect it before pairing again"
        );
        let _ = state.pairing.fail(&id, &caller.session_id, &message, now);
        state.vault.discard(&id, now);
        failure_with_code(StatusCode::CONFLICT, "sealed_under_another_key", &message)
    };

    let Some(pending) = state.vault.take_exchange(&id, now) else {
        return conflict(&state, "pairing session has no exchanged credential");
    };
    let identity = Identity {
        account_uuid: pending.identity.account_uuid.clone(),
        email: pending.identity.email.clone(),
        organization_uuid: pending.identity.organization_uuid.clone(),
    };
    let (row, created) = match pending.new_account {
        Some(new_account) => {
            let row = AccountRow {
                id: new_account.id,
                kind: AccountKind::Oauth,
                label: new_account.label,
                enabled: true,
                identity: Some(identity),
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
                state_cause: StateCause::Paired,
            };
            // One transaction for the row, its identity, its credential and its profile: a confirm that cannot
            // write the row stores no credential, and one that cannot write the credential leaves no row.
            if let Err(error) = state.store.create_paired_account(
                &row,
                &pending.bundle,
                &pending.profile.raw.to_string(),
                &pending.profile.subscription,
                now,
                accounts::ACTOR,
            ) {
                return match error {
                    StoreError::IdentityTaken => conflict(
                        &state,
                        "that upstream account is already paired to another slot",
                    ),
                    // The start's read found no row and a create landed since, so the row standing under the id
                    // belongs to that create, untouched and without a credential.
                    StoreError::IdTaken(_) => conflict(&state, accounts::DUPLICATE_ID),
                    other => unstored(&state, other),
                };
            }
            (row, true)
        }
        None => {
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
                // The store's unique index over the bound account uuid is what refuses the second slot. A scan
                // here would have to read every row and could still lose to a confirmation landing between the
                // read and the write.
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
                Err(StoreError::Crypto) => return sealed(&state, &row.label),
                Err(error) => return unstored(&state, error),
            };
            // The lifecycle carried from the exchange, not `row`'s own read: the store's fence is the one that
            // counts, and it has to be checked against what the pairing started from, not against a read this
            // same request just took, or a disconnect that landed before this call would pass its own fresh
            // state back to itself.
            if let Err(error) = state.store.commit_pairing(
                &row.id,
                row.incarnation,
                pending.lifecycle_generation,
                expected,
                &pending.bundle,
            ) {
                return match error {
                    StoreError::Disconnected => {
                        conflict(&state, "the account was disconnected while pairing")
                    }
                    StoreError::Stale => conflict(
                        &state,
                        "the credential changed while pairing, so start the pairing again",
                    ),
                    // A bundle without a refresh token opens the row it replaces to carry that token forward.
                    StoreError::Crypto => sealed(&state, &row.label),
                    other => unstored(&state, other),
                };
            }
            (row, false)
        }
    };
    // Committed outside the manager, which still keeps the replaced bundle: the usage service and the catalog
    // expect no generation, so they would be served it until it neared its expiry.
    state.manager.invalidate(&row.id);
    // A row born paired took its profile and state in the create's transaction.
    if !created {
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
        if let Err(error) = state
            .store
            .set_state(&row.id, AccountState::Ready, StateCause::Paired)
        {
            return unstored(&state, error);
        }
    }
    // The confirmed pairing names the plan its profile holds, so a flag an ended plan left goes with it, published by
    // the reload below.
    if !created {
        match state.store.clear_plan_ended(&row.id) {
            Ok(true) => state.ledger.record(phoebis_ledger::plan_restored(
                &row.id,
                now,
                "pairing",
                &pending.profile.raw,
            )),
            Ok(false) => {}
            Err(error) => return unstored(&state, error),
        }
    }
    state.usage.invalidate(&row.id);
    state.vault.mark_confirmed(&id, now);
    // The credential just committed replaces whatever a slot under this id answered before, so the cached
    // list (if any) is stale from this instant; the next read fetches fresh with the new one.
    state.catalog.evict(&row.id);
    // The pairing is over, so the account's one-at-a-time slot is handed back. The credential is already
    // committed at this point, so a session that refuses to close costs the account nothing beyond the
    // wait for its own deadline.
    if let Err(error) = state.pairing.confirm(&id, &caller.session_id, now) {
        tracing::warn!(%error, "a confirmed pairing session could not be closed");
    }
    let mut response = match reload_or_refuse(&state).await {
        Err(refusal) => refusal,
        Ok(()) => {
            // The first reading is taken now rather than left to the scheduler's next pass, so the account's
            // windows show as soon as the endpoint answers. The answer does not wait on it: the endpoint can be
            // slow and the pairing is committed either way, so a failed read records nothing and the next pass
            // reads the account. Tracked so a shutdown aborts a read still in flight rather than letting it
            // record into the closing channel.
            let polled = Arc::clone(&state);
            let account_id = row.id.clone();
            let handle = tokio::spawn(async move {
                match polled.store.account(&account_id) {
                    Ok(Some(committed)) => {
                        let _ = QuotaPoll::poll_account(
                            &polled.store,
                            &polled.usage,
                            &polled.tables,
                            &polled.ledger,
                            &polled.router,
                            &committed,
                        )
                        .await;
                    }
                    Ok(None) => {
                        tracing::debug!(account = %account_id, "the account was deleted before its first poll")
                    }
                    Err(error) => {
                        tracing::debug!(account = %account_id, %error, "the account could not be re-read for its first poll")
                    }
                }
            });
            state.ledger.track(handle);
            Json(Confirmed {
                account_id: row.id.clone(),
                status: "confirmed".into(),
                first_poll_in_flight: true,
            })
            .into_response()
        }
    };
    if created {
        // The row exists from here on, so every answer, a reload refusal included, names it to the mutation log.
        response
            .extensions_mut()
            .insert(LoggedEntity("account", row.id));
    }
    response
}

/// The process's standing: the version, the readiness, the ledger's and the router's counters, the bus, the
/// pricing sources, the recent gaps and the keepalive engine.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct System {
    pub version: String,
    /// The full commit the router was built from, or `unknown` for a build that had none, as every answer carries it
    /// in `x-phoebis-build`.
    pub build: String,
    pub profile: String,
    pub uptime_seconds: i64,
    pub storage_ok: bool,
    pub ready: bool,
    /// The components that died: `ledger`, `router`, `store`.
    pub dead: Vec<String>,
    pub ledger: LedgerStats,
    pub router: RouterStats,
    /// Per subscriber.
    pub bus: Vec<BusSubscriber>,
    /// The requests whose usage the price table could not price, counted once per request since start.
    pub pricing_unknown_models: u64,
    pub pricing: PricingSources,
    /// Recent `ledger_gap` events.
    pub gaps: Vec<crate::requests::Event>,
    pub keepalive: KeepaliveStats,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerStats {
    pub schema_version: Option<u32>,
    pub written: u64,
    pub dropped: u64,
    pub write_failures: u64,
    /// The cache-map writes whose decision row the ledger never stored, counted and never retried.
    pub cache_map_orphans: u64,
    /// The router events the ledger's mirror missed because its receiver lagged, each a decision or an event row
    /// never written.
    pub mirror_lagged: u64,
    pub gaps: u64,
    pub last_gap_at: i64,
    pub channel_depth: usize,
    /// The records waiting in the writer channel now.
    pub queued: usize,
    /// Whether admission refuses with `ledger_behind` (from 2,048 queued).
    pub behind: bool,
    /// The requests answered `ledger_behind`.
    pub behind_refusals: u64,
    /// Whether a batch failed to land and the spend has not been read again since.
    pub degraded: bool,
    /// 1 while a batch is under retry.
    pub retrying: u64,
    /// The priced records parked in the ledger file.
    pub parked_priced: u64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouterStats {
    /// The queue's size.
    pub command_depth: usize,
    /// The commands waiting in it now.
    pub queued: usize,
    /// The reports a closed queue refused at shutdown, per kind (`account_state`, `accounting`, `affinity`,
    /// `cooldown`, `credits`, `ended`, `member_spend`, `overage`, `pin_invalidated`, `pin_released`,
    /// `pin_renewed`, `sample`, `usage`).
    #[schema(value_type = BTreeMap<String, u64>)]
    pub dropped_reports: indexmap::IndexMap<String, u64>,
    /// The refusal records a full or closed router queue refused, each a decision row never written.
    pub dropped_refusals: u64,
    /// The routing state writer.
    pub writer: WriterStats,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriterStats {
    /// The batches waiting in its channel.
    pub depth: usize,
    /// The writes held in memory past its bound of 1,024 batches, one per row.
    pub deferred: usize,
    /// Whether writes are held in memory.
    pub degraded: bool,
    /// The batches the registry file as a whole refused since start.
    pub failed_batches: u64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BusSubscriber {
    pub subscriber: String,
    pub dropped: u64,
    pub panics: u64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PricingSources {
    pub api_prices: PricingSource,
    pub subscription_costs: PricingSource,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PricingSource {
    /// The configured path or null when the key is unset.
    pub path: Option<String>,
    /// `shipped`, or `file` when that path was read.
    #[schema(example = "shipped")]
    pub source: String,
    /// The date of the revision in force.
    pub effective: String,
    /// How many revisions the file lists.
    pub revisions: usize,
    /// The date of the first revision dated after the one in force, which takes effect at that date's midnight UTC,
    /// null when none is.
    pub next: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeepaliveStats {
    pub agents: usize,
    pub bus_gaps: u64,
    pub retained_bytes: usize,
    pub running_probes: usize,
    pub sessions: usize,
}

/// Read the process's standing.
#[utoipa::path(
    get,
    path = "/admin/system",
    operation_id = "system_show",
    tag = "System",
    responses(
        (status = 200, description = "The standing.", body = System),
    ),
)]
async fn system(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let uptime = (now - state.started_at).max(0) / 1_000;
    let stats = state.ledger.stats();
    let load = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    let read = state
        .ledger
        .db()
        .read(move |c| phoebis_ledger::recent_gaps(c, now - 86_400_000, 50))
        .await;
    let gaps = match read {
        Ok(gaps) => gaps,
        // The gap counter in the same response is whatever the writer reached, so an empty list from a read
        // that failed would tell the operator the counter has nothing behind it.
        Err(error) => {
            tracing::warn!(%error, "the ledger's gap events could not be read");
            Vec::new()
        }
    };
    let parked = match state.ledger.db().read(phoebis_ledger::parked_count).await {
        Ok(parked) => parked,
        Err(error) => {
            tracing::warn!(%error, "the ledger's parked records could not be counted");
            0
        }
    };
    let keepalive_stats = state.keepalive.stats();
    let writer = state.router.writer_status();
    let dead = state.readiness.dead();
    let dropped_reports = state
        .router
        .dropped_reports()
        .into_iter()
        .map(|(kind, n)| (kind.to_string(), n))
        .collect::<indexmap::IndexMap<String, u64>>();
    let revisions = state.pricing.revisions();
    let prices = revisions.in_force(now);
    Json(System {
        version: env!("CARGO_PKG_VERSION").into(),
        build: state.build.into(),
        profile: state.profile.name.to_string(),
        uptime_seconds: uptime,
        // From the one store read `dead` made, so a transient failure cannot show both a working and a dead store.
        storage_ok: !dead.contains(&"store"),
        ready: dead.is_empty(),
        dead: dead.iter().map(|d| d.to_string()).collect(),
        ledger: LedgerStats {
            schema_version: state.ledger.db().schema_version().await.ok(),
            written: load(&stats.written),
            dropped: load(&stats.dropped),
            write_failures: load(&stats.write_failures),
            cache_map_orphans: load(&stats.cache_map_orphans),
            mirror_lagged: load(&stats.mirror_lagged),
            gaps: load(&stats.gaps),
            last_gap_at: stats.last_gap_at.load(std::sync::atomic::Ordering::Relaxed),
            channel_depth: phoebis_ledger::CHANNEL_DEPTH,
            queued: state.ledger.queued(),
            behind: state.ledger.behind(),
            behind_refusals: load(&stats.behind_refusals),
            degraded: state.ledger.degraded(),
            retrying: load(&stats.retrying),
            parked_priced: parked,
        },
        router: RouterStats {
            command_depth: phoebis_routing::COMMANDS,
            queued: state.router.queued(),
            dropped_reports,
            dropped_refusals: state.router.dropped_refusals(),
            writer: WriterStats {
                depth: writer.depth,
                deferred: writer.deferred,
                degraded: writer.degraded,
                failed_batches: writer.failed_batches,
            },
        },
        bus: state
            .bus
            .stats()
            .iter()
            .map(|(name, s)| BusSubscriber {
                subscriber: name.to_string(),
                dropped: load(&s.dropped),
                panics: load(&s.panics),
            })
            .collect(),
        pricing_unknown_models: state.pricing.unknown_models(),
        pricing: PricingSources {
            api_prices: PricingSource {
                path: state.settings.api_prices_path.clone(),
                source: revisions.source.clone(),
                effective: prices.effective.clone(),
                revisions: revisions.len(),
                next: revisions.pending(now).map(|t| t.effective.clone()),
            },
            subscription_costs: PricingSource {
                path: state.settings.subscription_costs_path.clone(),
                source: state.tables.source.clone(),
                effective: state.tables.revisions.in_force(now).effective.clone(),
                revisions: state.tables.revisions.len(),
                next: state
                    .tables
                    .revisions
                    .pending(now)
                    .map(|t| t.effective.clone()),
            },
        },
        gaps: gaps.iter().map(crate::requests::event_json).collect(),
        keepalive: KeepaliveStats {
            agents: keepalive_stats.agents,
            bus_gaps: keepalive_stats.bus_gaps,
            retained_bytes: keepalive_stats.retained_bytes,
            running_probes: keepalive_stats.running_probes,
            sessions: keepalive_stats.sessions,
        },
    })
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
    let bundle = token.into_bundle(now, state.profile.control.default_login_scopes);
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
        .send(request, host, CONTROL_TIMEOUT)
        .await
        .map_err(|error| format!("control request failed: {error}"))
}

// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
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
    /// The newest polled extra-usage row per account, the one that carries the amounts a later head row lacks.
    pub extra_polled: Vec<phoebis_ledger::ExtraUsageSampleRecord>,
    /// The newest extra-usage row per account that carries a share, which a later head naming the reset alone lacks.
    pub extra_share: Vec<phoebis_ledger::ExtraUsageSampleRecord>,
    /// Each account's profile, subscription and term, read once. A row created since reads its own.
    pub extras: BTreeMap<String, phoebis_store::AccountExtras>,
    /// The router's retry instant per `funding_required` account.
    pub funding_retries: BTreeMap<String, i64>,
    /// The router's windows per account, predicted at its clock, as the usage `GET` reads them.
    pub held: BTreeMap<String, UtilizationSample>,
    pub lifetime: BTreeMap<String, phoebis_ledger::Totals>,
    pub members: Vec<PoolMember>,
    pub overages: BTreeMap<String, OverageEvidence>,
    /// The router's projections per account under its pool's settings, absent for an account in no pool.
    pub projections: BTreeMap<String, AccountProjection>,
    pub quota: Vec<phoebis_ledger::QuotaSampleRecord>,
    /// The router's samples per account as the poll or a head reported them, no charge added and no reset rolled:
    /// the readings an account's windows draw.
    pub reported: BTreeMap<String, UtilizationSample>,
    /// The router's spend window per usage-based account, at its clock.
    pub spend_windows: BTreeMap<String, SpendWindow>,
    pub week: BTreeMap<String, phoebis_ledger::Totals>,
}

/// The shared windows `account`'s set lacks as the router holds it now, which a route strips from an own-window share
/// member it answers for that one account. Empty for an account the router holds no sample of and for one the
/// registry no longer lists, whose row keeps its shares as priced.
pub(crate) fn windows_lacked(
    reported: &BTreeMap<String, UtilizationSample>,
    account: &str,
) -> Vec<&'static str> {
    reported.get(account).map_or_else(Vec::new, |sample| {
        [FIVE_HOUR, SEVEN_DAY]
            .into_iter()
            .filter(|window| !sample.has_window(window))
            .collect()
    })
}

/// The router's samples per account as the poll or a head reported them, the sets `windows_lacked` reads. A router
/// that answers none reads as holding no sample, so every account keeps both shared windows.
pub(crate) async fn reported_samples(state: &AdminState) -> BTreeMap<String, UtilizationSample> {
    state
        .router
        .reported_samples()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// One `projections` ask per pool with members, each account keeping its own pool's answer: the registry holds an
/// account in one pool, and every figure reads that pool's settings.
async fn projections_by_account(
    state: &AdminState,
    members: &[PoolMember],
) -> BTreeMap<String, AccountProjection> {
    let pool_of: BTreeMap<&str, &str> = members
        .iter()
        .map(|m| (m.account_id.as_str(), m.pool_id.as_str()))
        .collect();
    let pools: std::collections::BTreeSet<&str> = pool_of.values().copied().collect();
    let mut out = BTreeMap::new();
    for pool_id in pools {
        for projection in state.router.projections(pool_id).await.unwrap_or_default() {
            if pool_of.get(projection.account_id.as_str()) == Some(&pool_id) {
                out.insert(projection.account_id.clone(), projection);
            }
        }
    }
    out
}

/// The pins' demand on an account window as the router reads it, null where `window_projection` answers none.
fn window_demand(ctx: &AccountContext, account_id: &str, id: &str) -> Option<f64> {
    let window = if id == FABLE_WINDOW_KEY {
        SEVEN_DAY_FABLE
    } else {
        id
    };
    ctx.projections
        .get(account_id)
        .and_then(|p| p.windows.get(window))
        .map(|w| w.demand)
}

/// The account's slow-down entry in force as its pool's `projections` ask answered it.
fn slow_down_view(ctx: &AccountContext, account_id: &str) -> Option<AccountSlowDownView> {
    let entry = ctx.projections.get(account_id)?.slow_down?;
    Some(AccountSlowDownView {
        factor: entry.factor,
        speed: entry.speed,
        since: entry.since,
        stretch_ms: entry.stretch_ms,
        threshold_ms: entry.threshold_ms,
        hold_ms: entry.hold_ms,
    })
}

/// The projection of an account window as the router reads it, its id mapped back to the routing name: `(projection,
/// horizon, burn)`, all null without one.
fn window_projection(
    ctx: &AccountContext,
    account_id: &str,
    id: &str,
) -> (Option<f64>, Option<i64>, Option<f64>) {
    let window = if id == FABLE_WINDOW_KEY {
        SEVEN_DAY_FABLE
    } else {
        id
    };
    ctx.projections
        .get(account_id)
        .and_then(|p| p.windows.get(window))
        .map_or((None, None, None), |w| {
            (Some(w.projection), Some(w.horizon), Some(w.burn))
        })
}

pub(crate) async fn account_context(state: &AdminState) -> AccountContext {
    let now = (state.clock)();
    let hour = phoebis_ledger::hour_of(now);
    let to = crate::analytics::hour_end(hour);
    let week_start = crate::lifetime::week_start(hour);
    let day_start = hour - 24 * 3_600;
    let prices = state.pricing.revisions();
    let tables = state.tables.clone();
    let ledger = async {
        let old = state
            .old_totals
            .accounts
            .get((week_start, BTreeMap::new()), || {
                let (prices, tables) = (prices.clone(), tables.clone());
                state.ledger.db().read(move |c| {
                    phoebis_ledger::request_totals(
                        c,
                        phoebis_ledger::TotalsBy::Account,
                        i64::MIN,
                        week_start,
                        &prices,
                        &tables,
                        now,
                    )
                })
            })
            .await?;
        let read = state
            .ledger
            .db()
            .read(move |c| {
                Ok((
                    phoebis_ledger::latest_quota(c, now)?,
                    phoebis_ledger::latest_extra_usage(c, now)?,
                    phoebis_ledger::latest_extra_usage_polled(c, now)?,
                    phoebis_ledger::latest_extra_usage_share(c, now)?,
                    phoebis_ledger::request_totals(
                        c,
                        phoebis_ledger::TotalsBy::Account,
                        day_start,
                        to,
                        &prices,
                        &tables,
                        now,
                    )?,
                    phoebis_ledger::request_totals(
                        c,
                        phoebis_ledger::TotalsBy::Account,
                        week_start,
                        day_start,
                        &prices,
                        &tables,
                        now,
                    )?,
                ))
            })
            .await?;
        Ok::<_, phoebis_ledger::LedgerError>((old, read))
    }
    .await;
    let (old, (quota, extra, extra_polled, extra_share, day, six_days)) = match ledger {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(%error, "the account view could not read the ledger, so totals and windows are empty");
            Default::default()
        }
    };
    // The read takes the six days before the day, so the day's rows are read once, and the week joins the two here:
    // only the request counts reach the answer, and integer sums add exactly in any split.
    let week = crate::lifetime::lifetime_of(six_days, &day);
    let lifetime = crate::lifetime::lifetime_of(old, &week);
    let members = state.store.all_pool_members().unwrap_or_default();
    AccountContext {
        cooldowns: state.router.cooldowns().await.unwrap_or_default(),
        day,
        extra,
        extra_polled,
        extra_share,
        extras: state.store.account_extras().unwrap_or_default(),
        funding_retries: state
            .router
            .funding_retries()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        held: state
            .router
            .samples()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        lifetime,
        projections: projections_by_account(state, &members).await,
        members,
        overages: state
            .router
            .overages()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        quota,
        reported: state
            .router
            .reported_samples()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        spend_windows: state
            .router
            .spend_windows()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        week,
    }
}

/// The accounts, one row each.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountList {
    /// Each row carries the account's fields, the same shape `GET /admin/accounts/{id}` answers at the top level.
    pub accounts: Vec<Account>,
}

/// An account as the registry, the router and the ledger see it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Account {
    pub id: String,
    /// `api_key` or `oauth`.
    #[schema(example = "api_key")]
    pub kind: String,
    pub label: String,
    pub enabled: bool,
    /// Set only on an `oauth` account. An `api_key` account always reads false.
    pub canceled: bool,
    /// Epoch milliseconds: when a profile read found the account's organization holding no plan, null while a plan
    /// stands. While it is set the usage poll sends the account no usage request and reads its profile every third
    /// poll, a quarter hour apart, the routing reads the account as past its end, the row answers the document the read
    /// found (`subscription.plan` `free`, and its `profile.organization_type`, `profile.billing_type` and
    /// `subscription.tier`), and a profile read that finds a plan, or a pairing confirm, clears it.
    pub plan_ended_at: Option<i64>,
    /// `disconnected`, `funding_required`, `reauth_required`, `ready` or `unpaired`.
    #[schema(example = "disconnected")]
    pub state: String,
    /// Epoch milliseconds or null: the instant a `funding_required` account is next tried, null in any other state,
    /// and null after a restart until the first retry claims the account and stamps that request's instant.
    pub funding_retry_at: Option<i64>,
    pub identity: Option<AccountIdentity>,
    /// Never a token.
    pub credential: Option<AccountCredential>,
    pub lifecycle_generation: u64,
    /// The row's edit token, which `PATCH /admin/accounts/{id}` takes back as `expected_revision`.
    pub revision: u64,
    pub cooldowns: Vec<Cooldown>,
    pub overage: Option<Overage>,
    pub profile: Option<AccountProfile>,
    /// The whole object null for an `api_key` account or an `oauth` account with no profile document yet and not
    /// canceled.
    pub subscription: Option<AccountSubscription>,
    /// The account's quota windows: the router's reported sample first and the ledger's samples of the last day for
    /// the rest, the ledger's alone for an `api_key` account. A ledger sample of a shared window the account's window
    /// set lacks is left out. Empty for an account with no reading, and for an `oauth` account in the `disconnected` or
    /// `reauth_required` state or flagged `plan_ended_at`, whose last reading the view no longer answers.
    pub windows: Vec<AccountWindow>,
    /// The account's slow-down entry in force as the router reads it, null on an api_key account, an untiered account
    /// or one with no 5-hour reading, an account in no pool, a pool with the slow-down off, and an account no pinned
    /// selection has read since the last purge or restart.
    pub slow_down: Option<AccountSlowDownView>,
    /// The whole object null until an answer to a fast request carried the fast headers.
    pub fast_lane: Option<FastLane>,
    pub extra_usage: Option<ExtraUsage>,
    pub pool: Option<PoolMembership>,
    pub requests: RequestCounts,
    /// Null for an `api_key` account. For an `oauth` account `error`, `fetched_at`, `held_until` and `poll`, as
    /// `GET /admin/accounts/{id}/usage` answers them, so one list read carries every account's standing.
    pub usage: Option<UsageStanding>,
    /// True for an `oauth` account whose stored profile names a usage-based Enterprise seat (the rate-limit tier
    /// `default_claude_zero` or the seat tier `enterprise_usage_based`), false on every other row, an `api_key`
    /// account included. Such an account never reads as in overage, so `overage` answers null.
    pub usage_based: bool,
    /// The spend window of a usage-based account, null on every account the class does not hold.
    pub spend: Option<Spend>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountIdentity {
    pub email: String,
    pub account_uuid: String,
    pub organization_uuid: Option<String>,
}

/// The credential's standing.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountCredential {
    pub access_expires_at: i64,
    pub refresh_expires_at: Option<i64>,
    pub generation: u64,
    pub last_refresh_at: Option<i64>,
    /// The reason of the last failed or uncertain refresh, or of the last credential failure that opened no
    /// refresh (a backoff or a quarantine), whichever is newer.
    pub last_error: Option<String>,
    pub quarantined: bool,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cooldown {
    pub scope: String,
    pub until: i64,
    pub reason: String,
    pub window: String,
    pub set_at: i64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Overage {
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
}

/// Seven values read off the stored profile document as they are.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountProfile {
    /// Usually a string, null when the document lacks it.
    pub billing_type: Value,
    /// Usually a string, null when the document lacks it.
    pub display_name: Value,
    /// Usually a boolean, read from the document's `organization` as the tier is, null when the document lacks it
    /// there.
    pub has_extra_usage_enabled: Value,
    /// Usually a string, null when the document lacks it.
    pub organization_name: Value,
    /// Usually a string, null when the document lacks it.
    pub organization_type: Value,
    /// Usually a string (`Ultra` on the `default_claude_max_20x` tier), read from the document's `organization`, null
    /// when the document lacks it there.
    pub plan_display_name: Value,
    /// Usually a string, null when the document lacks it.
    pub rate_limit_tier: Value,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountSubscription {
    /// The plan the stored profile resolves to: `free`, `pro`, `max_5x`, `max_20x`, `max` (a Max organization whose
    /// tier names neither size), `team`, `enterprise` or `usage_based`, read from the organization type first and the
    /// rate-limit tier second, null while no stored document and no tier the table names stand. The console labels
    /// `max_20x` Ultra, `max_5x` Max and `max` Max (tier unknown).
    pub plan: Option<String>,
    /// Null when the profile lacks it.
    pub tier: Option<String>,
    /// Epoch milliseconds, null when the profile lacks it.
    pub since: Option<i64>,
    /// Epoch milliseconds, null when the profile lacks it.
    pub trial_ends_at: Option<i64>,
    /// The end the operator saved, standing only while `canceled` is set.
    pub ends_at: Option<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountWindow {
    pub id: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
    /// The instant of the reading: the router's reported sample's, or the ledger row's own, the bucket's 300-second
    /// floor only for a row from before the samples carried one.
    pub observed_at: i64,
    /// The router's projection of the window at its horizon, null on an api_key account, a window the router holds no
    /// reading of and an account in no pool.
    pub projection: Option<f64>,
    /// When the projection is read: the earlier of the reset and the account's end on the 5-hour window, the
    /// earliest of five hours on, the reset and the end on a weekly one. Null where `projection` is.
    pub horizon: Option<i64>,
    /// The pins' burn on the window, in share per hour. Null where `projection` is.
    pub burn: Option<f64>,
    /// The pins' unpaced demand on the window, in share per hour. Null where `projection` is.
    pub demand: Option<f64>,
    /// The window's lead over pace at the answer's instant: the utilization less the share of the window elapsed
    /// toward the earlier of its reset and the account's end, the figure the engine sheds and cools by. Null while
    /// the utilization is unknown.
    pub lead: Option<f64>,
}

/// The slow-down entry the account's last pinned selection wrote, the figures its holds apply.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountSlowDownView {
    /// The burn the 5-hour window can still carry to its reset over the demand, from 0 to 1: under 1 the account is
    /// slowed.
    pub factor: f64,
    /// The speed the holds realized, which the demand divides the pins' burn by.
    pub speed: f64,
    /// Epoch milliseconds: when the factor first read under 1 since it last read 1, null outside a span.
    pub since: Option<i64>,
    /// How long before the reset the sessions' unpaced demand would pass the window's ceiling, 0 when it never does.
    pub stretch_ms: i64,
    /// The pool's `slow_down_stretch_seconds` at the read, in milliseconds.
    pub threshold_ms: i64,
    /// The pool's `slow_down_hold_seconds`, the longest one request waits, in milliseconds.
    pub hold_ms: i64,
}

/// The fast lane's input and output buckets as the router last read them off an answer's head.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FastLane {
    pub input: Option<LaneReading>,
    pub output: Option<LaneReading>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaneReading {
    pub utilization: f64,
    pub resets_at: Option<i64>,
    pub observed_at: i64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtraUsage {
    pub currency: Option<String>,
    pub disabled_reason: Option<String>,
    pub in_use: Option<bool>,
    pub is_enabled: bool,
    pub monthly_limit: Option<i64>,
    pub used_credits: Option<i64>,
    pub utilization: Option<f64>,
    pub observed_at: i64,
}

/// A usage-based seat's spend window: the poll's amounts, the newest share and the window's reset.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spend {
    /// Null while no polled row of the last day carries the amounts.
    pub amounts: Option<SpendAmounts>,
    /// The share as a fraction: the router's wherever it holds a reading of the seat, else the stored `utilization` of
    /// the newest row of the last day, of any source, that carries one. Null where the share it reads is none and once
    /// the reset the share was read under has passed, the amounts drawn with no meter then.
    pub utilization: Option<f64>,
    /// Epoch milliseconds: the window's reset as the router reads it, the estimate where the router answers none.
    pub resets_at: i64,
    /// `head` for the newest head's reset while it is ahead of now, else `estimated` for 00:00 UTC on the first of
    /// the next calendar month.
    #[schema(example = "estimated")]
    pub reset_source: String,
    /// The newest row's instant, null without a row.
    pub observed_at: Option<i64>,
}

/// The polled row's `spend` figures, or its `extra_usage` amounts where the body carried no `spend` object.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpendAmounts {
    pub currency: String,
    /// The currency's minor-unit exponent.
    pub exponent: u8,
    /// Null where the body sets no limit: the spend alone, no bar.
    pub limit_minor: Option<i64>,
    pub used_minor: i64,
    /// Epoch milliseconds: the polled row's instant.
    pub polled_at: i64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolMembership {
    pub pool_id: String,
    pub priority: u32,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestCounts {
    pub last_24h: u64,
    pub last_7d: u64,
    pub lifetime: u64,
}

/// The poll's standing on an `oauth` account, as `GET /admin/accounts/{id}/usage` answers it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageStanding {
    pub error: Option<String>,
    pub fetched_at: Option<i64>,
    /// The instant the next attempt may run while the account is held, Unix milliseconds, null otherwise.
    pub held_until: Option<i64>,
    pub poll: Option<PollStatusView>,
}

/// The poll's standing. The instants are Unix milliseconds, `backoff_since` null outside a run of 429s and
/// `next_attempt_at` null once the account's next slot may ask it.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PollStatusView {
    pub backoff_since: Option<i64>,
    pub last_attempt_at: i64,
    pub last_clean_at: Option<i64>,
    /// The message of the last attempt that was not clean, null after a clean one.
    pub last_error: Option<String>,
    /// `clean`, `failed`, or `rate_limited`.
    #[schema(example = "clean")]
    pub last_outcome: String,
    pub next_attempt_at: Option<i64>,
    pub rate_limited_in_a_row: u32,
}

/// The poll's latest snapshot and standing, or the error that stands in for a snapshot when there is none, with
/// every other member null or empty.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Usage {
    pub account_id: String,
    pub error: Option<String>,
    pub extra_usage: Option<UsageExtra>,
    pub fable_window: Option<UsageWindowView>,
    pub fetched_at: Option<i64>,
    /// The instant the next attempt may run while the account is held, Unix milliseconds, null otherwise, the
    /// instant the poll's `next_attempt_at` names.
    pub held_until: Option<i64>,
    /// Zero is the unstamped generation, as the ledger stamps a row it has no identity for.
    pub identity_generation: u64,
    /// Null until the poll first asks.
    pub poll: Option<PollStatusView>,
    pub raw_keys: Vec<String>,
    pub windows: Vec<UsageWindowView>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageWindowView {
    pub key: String,
    pub resets_at: Option<i64>,
    pub utilization: Option<f64>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageExtra {
    pub currency: Option<String>,
    pub disabled_reason: Option<String>,
    pub is_enabled: bool,
    pub monthly_limit: Option<i64>,
    pub used_credits: Option<i64>,
    pub utilization: Option<f64>,
}

/// Whether an API key still works, for an `api_key` account only, asked over the cheapest authenticated endpoint
/// there is: the catalog discovery, against the account's own host.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verify {
    pub ok: bool,
    /// The upstream HTTP status.
    pub status: u16,
}

/// Never a token, in any branch: an account view is the one place a credential could plausibly be shown
/// and must not be.
pub(crate) async fn account_json(state: &AdminState, row: &AccountRow) -> Account {
    let ctx = account_context(state).await;
    account_view(state, row, &ctx).await
}

/// The view over a context the caller already holds, so listing every account asks the router and the
/// ledger once.
async fn account_view(state: &AdminState, row: &AccountRow, ctx: &AccountContext) -> Account {
    let overage = ctx.overages.get(&row.id);
    let credential = state.store.credential_metadata(&row.id).ok().flatten();
    let attempt = state
        .store
        .last_refresh_attempt(&row.id, row.incarnation)
        .ok()
        .flatten();
    // Past the refresh manager's floor a retry opens beside an `uncertain` attempt, and a retry that fails leaves
    // that attempt open, so the flag reads every open attempt of this holder rather than the newest alone: any
    // one of them is what a restart quarantines the account for.
    let quarantined = state
        .store
        .has_pending_refresh_attempt(&row.id, row.incarnation)
        .unwrap_or(false);
    // A failure that opened no attempt (a backoff or a quarantine) still updates the credential row, so
    // whichever of the two landed last is the one that answers `credential.last_error`.
    let attempt_error = attempt
        .as_ref()
        .filter(|attempt| {
            matches!(
                attempt.settled(),
                Some(Settled::Failed | Settled::Superseded | Settled::Uncertain)
            )
        })
        .and_then(|attempt| attempt.resolved_at.map(|at| (at, attempt.detail.clone())));
    let extras = ctx.extras.get(&row.id);
    let profile = match extras {
        Some(extras) => extras.profile_json.clone(),
        None => state.store.profile_json(&row.id).ok().flatten(),
    }
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .map(|p| AccountProfile {
        billing_type: p["organization"]["billing_type"].clone(),
        display_name: p["account"]["display_name"].clone(),
        has_extra_usage_enabled: p["organization"]["has_extra_usage_enabled"].clone(),
        organization_name: p["organization"]["name"].clone(),
        organization_type: p["organization"]["organization_type"].clone(),
        plan_display_name: p["organization"]["plan_display_name"].clone(),
        rate_limit_tier: p["organization"]["rate_limit_tier"].clone(),
    });
    // The stored flag and end read through this alone, with no profile join, so a canceled account answers
    // them even before it ever pairs.
    let term = match extras {
        Some(extras) => extras.term.clone(),
        None => state
            .store
            .subscription_term(&row.id)
            .ok()
            .flatten()
            .unwrap_or_default(),
    };
    let now = (state.clock)();
    let end = phoebis_routing::term::end_of(&term, now);
    // Null until the account has paired and a profile document exists, unless the operator has canceled it:
    // a canceled, unpaired OAuth row still has an end to report, so the flag joins `profile.is_some()` here.
    let subscription = (row.kind == AccountKind::Oauth && (profile.is_some() || term.canceled))
        .then(|| match extras {
            Some(extras) => extras.subscription.clone(),
            None => match state.store.subscription(&row.id) {
                Ok(subscription) => subscription.unwrap_or_default(),
                Err(error) => {
                    tracing::warn!(
                        account = %row.id,
                        %error,
                        "the account's tier could not be read, so the view answers as if no subscription were stored"
                    );
                    Default::default()
                }
            },
        })
        .map(|s| AccountSubscription {
            plan: term.plan.clone(),
            tier: s.tier,
            since: s.since,
            trial_ends_at: s.trial_ends_at,
            ends_at: term.ends_at,
        });
    // The token manager hands out no token for an `oauth` account in either state, and a flagged account's poll is
    // paused and its routing reads it as past its end, so neither the poll nor a request reads the account until it
    // pairs again or a profile read finds a plan: what the router and the ledger still hold is its last reading, which
    // both paths below would draw as live.
    let windows_withheld = row.kind == AccountKind::Oauth
        && (matches!(
            row.state,
            AccountState::Disconnected | AccountState::ReauthRequired
        ) || term.plan_ended_at.is_some());
    // The router holds a reading the moment the poll or a head reports it, while the ledger's row waits on the writer,
    // so a refresh's reading shows as soon as the refresh answers. The ledger's rows fill each plan window the router
    // holds no reading of, and every window of an api_key account.
    let reported = ctx
        .reported
        .get(&row.id)
        .filter(|_| row.kind == AccountKind::Oauth && !windows_withheld);
    let mut windows: Vec<AccountWindow> = reported
        .map(|sample| held_windows(sample, end, now))
        .unwrap_or_default()
        .into_iter()
        .map(|(key, reading, lead)| {
            let (projection, horizon, burn) = window_projection(ctx, &row.id, &key);
            let demand = window_demand(ctx, &row.id, &key);
            AccountWindow {
                id: account_window_id(&key).to_string(),
                utilization: Some(reading.utilization),
                resets_at: reading.reset,
                observed_at: reading.observed_at,
                projection,
                horizon,
                burn,
                demand,
                lead: Some(lead),
            }
        })
        .collect();
    windows.extend(
        ctx.quota
            .iter()
            .filter(|q| !windows_withheld && q.account_id == row.id)
            .filter(|q| {
                !reported
                    .is_some_and(|s| is_plan_window(&q.window) && s.reading(&q.window).is_some())
            })
            // A row the ledger kept from before the poll that dropped its window is no window of the account.
            .filter(|q| reported.is_none_or(|s| s.has_window(&q.window)))
            .map(|q| {
                let id = account_window_id(&q.window);
                let (projection, horizon, burn) = window_projection(ctx, &row.id, id);
                let demand = window_demand(ctx, &row.id, id);
                AccountWindow {
                    id: id.to_string(),
                    utilization: q.utilization,
                    resets_at: q.reset_at,
                    observed_at: q.observed_at,
                    projection,
                    horizon,
                    burn,
                    demand,
                    lead: q
                        .utilization
                        .map(|u| lead_over_pace(&q.window, u, q.reset_at, end, now)),
                }
            }),
    );
    // The Fable week reaches the list under one id from up to three sources, so one row stands per id: the newest
    // reading, the router's on a tie.
    let mut one_per_id: Vec<AccountWindow> = Vec::with_capacity(windows.len());
    for window in windows {
        match one_per_id.iter_mut().find(|kept| kept.id == window.id) {
            Some(kept) if kept.observed_at < window.observed_at => *kept = window,
            Some(_) => {}
            None => one_per_id.push(window),
        }
    }
    let windows = one_per_id;
    let fast_lane = ctx.held.get(&row.id).and_then(fast_lane_json);
    // The router may hold an entry for a mark whose write landed nowhere, so the registry's state decides.
    let funding_retry_at = (row.state == AccountState::FundingRequired)
        .then(|| ctx.funding_retries.get(&row.id).copied())
        .flatten();
    // Composed as the usage `GET` composes its answer, over the router's windows the list already read once, so one
    // list read carries every account's standing. The poll never reads an API-key account.
    let usage = (row.kind == AccountKind::Oauth).then(|| {
        let (poll, unread) = poll_standing(state, row);
        let latest = state.usage.latest_for(row);
        let held = match &latest {
            Some(snapshot) if has_windows(snapshot) => None,
            _ => ctx.held.get(&row.id).cloned(),
        };
        let answer = compose_usage(&row.id, latest, held, end, now);
        UsageStanding {
            error: answer
                .as_ref()
                .map_or(Some(unread), |s| s.error.as_deref())
                .map(str::to_string),
            fetched_at: answer.as_ref().map(|s| s.fetched_at),
            held_until: held_until(poll.as_ref()),
            poll: poll.as_ref().map(poll_json),
        }
    });
    Account {
        id: row.id.clone(),
        kind: row.kind.as_str().to_string(),
        label: row.label.clone(),
        enabled: row.enabled,
        canceled: term.canceled,
        plan_ended_at: term.plan_ended_at,
        state: state_name(row.state).to_string(),
        funding_retry_at,
        identity: row.identity.as_ref().map(|identity| AccountIdentity {
            email: identity.email.clone(),
            account_uuid: identity.account_uuid.clone(),
            organization_uuid: identity.organization_uuid.clone(),
        }),
        credential: credential.map(|credential| AccountCredential {
            access_expires_at: credential.expires_at,
            refresh_expires_at: credential.refresh_expires_at,
            generation: credential.generation,
            last_refresh_at: attempt.as_ref().and_then(|attempt| attempt.resolved_at),
            last_error: match (&attempt_error, credential.last_error.as_ref()) {
                (Some((at, detail)), Some(text)) => {
                    // A tie favors the credential error: a backoff recorded right after the attempt that
                    // armed it lands in the same millisecond on the store's clock as often as not.
                    if credential.last_error_at >= *at {
                        Some(text.clone())
                    } else {
                        detail.clone()
                    }
                }
                (Some((_, detail)), None) => detail.clone(),
                (None, Some(text)) => Some(text.clone()),
                (None, None) => None,
            },
            quarantined,
        }),
        lifecycle_generation: row.lifecycle_generation,
        revision: row.edit_token(),
        cooldowns: ctx
            .cooldowns
            .iter()
            .filter(|cooldown| cooldown.account_id == row.id)
            .map(|cooldown| Cooldown {
                scope: cooldown.scope.to_string(),
                until: cooldown.until,
                reason: cooldown.reason.clone(),
                window: cooldown.window.clone(),
                set_at: cooldown.set_at,
            })
            .collect(),
        // A usage-based seat's overage family is its spend window, so no evidence the router or a head row from before
        // the class holds reads as overage on it.
        overage: overage
            .filter(|_| !term.usage_based)
            .map(|overage| Overage {
                in_use: overage.in_use,
                since: overage.since,
                reset: overage.reset,
                observed_at: overage.observed_at,
            }),
        profile,
        subscription,
        windows,
        slow_down: slow_down_view(ctx, &row.id),
        fast_lane,
        // The amounts come from the newest polled row, since a head row carries none, the share from the newest row
        // that carries one, and the rest from the newest row of any source.
        extra_usage: ctx.extra.iter().find(|e| e.account_id == row.id).map(|e| {
            let polled = ctx.extra_polled.iter().find(|p| p.account_id == row.id);
            let shared = ctx.extra_share.iter().find(|p| p.account_id == row.id);
            ExtraUsage {
                currency: polled.and_then(|p| p.currency.clone()),
                disabled_reason: e.disabled_reason.clone(),
                in_use: e.in_use.filter(|_| !term.usage_based),
                is_enabled: e.is_enabled,
                monthly_limit: polled.and_then(|p| p.monthly_limit),
                used_credits: polled.and_then(|p| p.used_credits),
                utilization: shared.and_then(|p| p.utilization),
                observed_at: e.bucket * 1_000,
            }
        }),
        pool: ctx
            .members
            .iter()
            .find(|m| m.account_id == row.id)
            .map(|m| PoolMembership {
                pool_id: m.pool_id.clone(),
                priority: m.priority,
            }),
        requests: RequestCounts {
            last_24h: ctx.day.get(&row.id).map_or(0, |t| t.requests),
            last_7d: ctx.week.get(&row.id).map_or(0, |t| t.requests),
            lifetime: ctx.lifetime.get(&row.id).map_or(0, |t| t.requests),
        },
        usage,
        usage_based: term.usage_based,
        spend: term.usage_based.then(|| spend_view(ctx, &row.id, now)),
    }
}

/// A usage-based seat's spend window: the amounts from the newest polled row, the instant from the newest row of any
/// source, and the reset from the router's window. The share and its lapse are the router's wherever it holds a reading
/// of the seat, so the meter lifts where the hold does, and otherwise the share of the newest row that carries one,
/// lapsing where `now` reaches the reset that row's reading was taken under.
fn spend_view(ctx: &AccountContext, account_id: &str, now: i64) -> Spend {
    let polled = ctx.extra_polled.iter().find(|e| e.account_id == account_id);
    let newest = ctx.extra.iter().find(|e| e.account_id == account_id);
    let shared = ctx.extra_share.iter().find(|e| e.account_id == account_id);
    let amounts = polled.and_then(|p| {
        let polled_at = p.bucket * 1_000;
        match p.spend_used_minor {
            Some(used_minor) => Some(SpendAmounts {
                currency: p.spend_currency.clone()?,
                exponent: p.spend_exponent?,
                limit_minor: p.spend_limit_minor,
                used_minor,
                polled_at,
            }),
            // A row the body's `spend` object did not fill stores no exponent, and the endpoint's currencies carry two
            // decimals.
            None => Some(SpendAmounts {
                currency: p.currency.clone()?,
                exponent: 2,
                limit_minor: p.monthly_limit,
                used_minor: p.used_credits?,
                polled_at,
            }),
        }
    });
    // The router lapses its share at the reset it read the share under, a held head's included, which the ledger's row
    // does not keep. The row serves a router that answers no window, stopped or on a snapshot that predates the class,
    // and one that holds no reading of the seat.
    let utilization = match ctx
        .spend_windows
        .get(account_id)
        .filter(|window| window.observed_at.is_some())
    {
        Some(window) => window.utilization,
        None => shared.and_then(|e| {
            let reset = phoebis_routing::spend_reading_reset(e.bucket * 1_000, e.overage_reset);
            e.utilization.filter(|_| now < reset)
        }),
    };
    // A router that answers no window for the seat, a stopped one or one whose snapshot predates the class, leaves the
    // estimate.
    let (resets_at, reset_source) = ctx.spend_windows.get(account_id).map_or_else(
        || {
            (
                phoebis_routing::spend_reset_estimate(now),
                SpendResetSource::Estimated,
            )
        },
        |window| (window.reset, window.reset_source),
    );
    Spend {
        amounts,
        utilization,
        resets_at,
        reset_source: reset_source.as_str().to_string(),
        observed_at: newest.map(|e| e.bucket * 1_000),
    }
}

fn has_windows(snapshot: &UsageSnapshot) -> bool {
    !snapshot.windows.is_empty() || snapshot.fable_window.is_some()
}

/// The router's plan windows under the keys the usage endpoint gives them, each with its lead over pace at `now`
/// toward the earlier of its reset and the account's `end`: what both the usage `GET` and the account view show when
/// they read the router's windows.
fn held_windows(
    sample: &UtilizationSample,
    end: Option<i64>,
    now: i64,
) -> Vec<(String, WindowReading, f64)> {
    // The router names every plan window as the endpoint does, except the Fable week it reads from `limits`.
    sample
        .windows
        .iter()
        .filter(|(name, _)| is_plan_window(name))
        .map(|(name, reading)| {
            // Paced off the router's own name, which says the Fable week is a week, before it takes the endpoint's key.
            let lead = reading.utilization
                - phoebis_routing::elapsed_share_at(name, reading.reset, end, now);
            let key = if name == SEVEN_DAY_FABLE {
                FABLE_WINDOW_KEY
            } else {
                name.as_str()
            };
            (key.to_string(), reading.clone(), lead)
        })
        .collect()
}

/// The fast lane's input and output buckets as the router last read them off an answer's head; `None` while it
/// holds neither, since only an answer to a fast request carries them.
fn fast_lane_json(sample: &UtilizationSample) -> Option<FastLane> {
    let reading = |window: &str| {
        sample.reading(window).map(|r| LaneReading {
            utilization: r.utilization,
            resets_at: r.reset,
            observed_at: r.observed_at,
        })
    };
    let (input, output) = (
        reading(phoebis_relay::FAST_INPUT),
        reading(phoebis_relay::FAST_OUTPUT),
    );
    (input.is_some() || output.is_some()).then_some(FastLane { input, output })
}

/// What the `GET` answers from the poll's `latest` snapshot and the router's `held` sample of the account, whose end
/// is `end` at `now`.
fn compose_usage(
    id: &str,
    latest: Option<UsageSnapshot>,
    held: Option<UtilizationSample>,
    end: Option<i64>,
    now: i64,
) -> Option<UsageSnapshot> {
    if let Some(snapshot) = latest.as_ref().filter(|s| has_windows(s)) {
        return Some(UsageSnapshot {
            error: None,
            ..snapshot.clone()
        });
    }
    // The router reads a utilization as a fraction; the endpoint's windows carry a percentage.
    let window = |(key, reading, _): &(String, WindowReading, f64)| UsageWindow {
        key: key.clone(),
        utilization: Some(reading.utilization * 100.0),
        resets_at: reading.reset,
    };
    let held = held.and_then(|sample| {
        let readings = held_windows(&sample, end, now);
        let observed_at = readings
            .iter()
            .map(|(_, reading, _)| reading.observed_at)
            .max()?;
        let windows: Vec<UsageWindow> = readings
            .iter()
            .filter(|(key, _, _)| *key != FABLE_WINDOW_KEY)
            .map(window)
            .collect();
        let fable = readings
            .iter()
            .find(|(key, _, _)| *key == FABLE_WINDOW_KEY)
            .map(window);
        Some((windows, fable, observed_at))
    });
    let Some((windows, fable, observed_at)) = held else {
        return latest;
    };
    Some(UsageSnapshot {
        account_id: id.to_string(),
        identity_generation: latest.as_ref().map_or(0, |s| s.identity_generation),
        fetched_at: observed_at,
        windows,
        fable_window: fable,
        extra_usage: latest.as_ref().and_then(|s| s.extra_usage.clone()),
        spend: latest.as_ref().and_then(|s| s.spend.clone()),
        raw_keys: latest.map(|s| s.raw_keys).unwrap_or_default(),
        error: None,
    })
}

/// The poll's standing as the usage `GET` and every account row answer it.
fn poll_json(status: &PollStatus) -> PollStatusView {
    PollStatusView {
        backoff_since: status.backoff_since,
        last_attempt_at: status.last_attempt_at,
        last_clean_at: status.last_clean_at,
        last_error: status.last_error.clone(),
        // The words `PollOutcome`'s snake_case serialization writes.
        last_outcome: match status.last_outcome {
            PollOutcome::Clean => "clean",
            PollOutcome::Failed => "failed",
            PollOutcome::RateLimited => "rate_limited",
        }
        .to_string(),
        next_attempt_at: status.next_attempt_at,
        rate_limited_in_a_row: status.rate_limited_in_a_row,
    }
}

/// The hold's deadline as the console prints it: the poll's next attempt while the account waits out a 429, null
/// otherwise. Read off the poll status, where the service keeps the hold, so the usage GET and the account list agree.
fn held_until(poll: Option<&PollStatus>) -> Option<i64> {
    poll.and_then(|p| p.next_attempt_at)
}

/// `snapshot` is the answer, or the error that stands in for it when there is none.
fn usage_json(
    id: &str,
    snapshot: Result<&UsageSnapshot, &str>,
    poll: Option<&PollStatus>,
) -> Usage {
    let held_until = held_until(poll);
    let poll = poll.map(poll_json);
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        // Zero is the unstamped generation, as the ledger stamps a row it has no identity for.
        Err(error) => {
            return Usage {
                account_id: id.to_string(),
                error: Some(error.to_string()),
                extra_usage: None,
                fable_window: None,
                fetched_at: None,
                held_until,
                identity_generation: 0,
                poll,
                raw_keys: vec![],
                windows: vec![],
            };
        }
    };
    let window = |window: &UsageWindow| UsageWindowView {
        key: window.key.clone(),
        resets_at: window.resets_at,
        utilization: window.utilization,
    };
    Usage {
        account_id: snapshot.account_id.clone(),
        error: snapshot.error.clone(),
        extra_usage: snapshot.extra_usage.as_ref().map(|extra| UsageExtra {
            currency: extra.currency.clone(),
            disabled_reason: extra.disabled_reason.clone(),
            is_enabled: extra.is_enabled,
            monthly_limit: extra.monthly_limit,
            used_credits: extra.used_credits,
            utilization: extra.utilization,
        }),
        fable_window: snapshot.fable_window.as_ref().map(window),
        fetched_at: Some(snapshot.fetched_at),
        held_until,
        identity_generation: snapshot.identity_generation,
        poll,
        raw_keys: snapshot.raw_keys.clone(),
        windows: snapshot.windows.iter().map(window).collect(),
    }
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
        PairingStatus::Canceled => "canceled",
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
        OauthError::Json(_) | OauthError::UsageJson(_) => {
            format!("{what} response was not the JSON this endpoint returns")
        }
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
        "the registry was written but could not be reloaded, and the next successful reload publishes it",
    )
}

pub(crate) fn store_failure(error: StoreError) -> Response {
    tracing::warn!(%error, "admin request failed in the store");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "storage failure")
}

pub(crate) fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// A JSON array deserializes into a struct whose members all default, so a route would answer `id is required` to
/// `[]` in place of the sentence every route gives a body that is no object. The shape is checked on the parsed value
/// before any member check, and the struct is read from the bytes, where the derive refuses a repeated member that a
/// value would keep at its last occurrence.
// The error is a rendered HTTP response, returned once per request and dropped, so boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn parse_object<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "body must be a JSON object"))?;
    if !value.is_object() {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "body must be a JSON object",
        ));
    }
    serde_json::from_slice(body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "body must be a JSON object"))
}

/// The reports of one member, any period: what `phoebis_routing::budget_deadline` reads, so the routes answer the
/// selector's own verdict rather than a copy of its rule.
pub(crate) fn spend_rows<'a>(
    spends: &'a [MemberSpend],
    pool_id: &str,
    account_id: &str,
) -> Vec<&'a MemberSpend> {
    spends
        .iter()
        .filter(|s| s.pool_id == pool_id && s.account_id == account_id)
        .collect()
}

/// Tells an absent field from an explicit `null`, so a `PATCH` can clear a nullable column.
pub(crate) fn double_option<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// A field that is there, whatever it holds: `Option` alone folds a present `null` into an absent field.
pub(crate) fn present<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}

/// The stamp a pool or key edit's body names through the row's `revision`: the row's own when the body sent none,
/// refused when it is not a non-negative integer, and answered `409 stale_edit` with `stale` when it names another
/// incarnation than the row's. A token of the row's incarnation answers the stamp it carries, which the route
/// compares with its own read.
// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn fence_token(
    sent: Option<&serde_json::Value>,
    incarnation: u64,
    updated_at: i64,
    stale: &str,
) -> Result<i64, Response> {
    let Some(value) = sent else {
        return Ok(updated_at);
    };
    let Some(token) = value.as_u64() else {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "expected_revision is the revision the row was read at",
        ));
    };
    // What the token carries above the incarnation's floor is its stamp only when its incarnation is the row's, which
    // `answers_stamp_token` confirms for the two parts together.
    match token
        .checked_sub(stamp_token(incarnation, 0))
        .and_then(|stamp| i64::try_from(stamp).ok())
    {
        Some(stamp) if answers_stamp_token(incarnation, stamp, token) => Ok(stamp),
        _ => Err(failure_with_code(StatusCode::CONFLICT, "stale_edit", stale)),
    }
}

/// The edit token an account edit's body names, `None` when it sent none, refused when it is not a non-negative integer.
/// The route answers 409 when it names another incarnation or revision than its own read, so the store fences on the
/// read alone.
// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn fence_revision(sent: Option<&serde_json::Value>) -> Result<Option<u64>, Response> {
    let Some(value) = sent else {
        return Ok(None);
    };
    value.as_u64().map(Some).ok_or_else(|| {
        failure(
            StatusCode::BAD_REQUEST,
            "expected_revision is the revision the row was read at",
        )
    })
}

/// A refusal an operator console can branch on without reading the prose, in the same header slot the relay
/// puts its own codes.
pub(crate) fn failure_with_code(status: StatusCode, code: &'static str, message: &str) -> Response {
    let mut response = failure(status, message);
    response.headers_mut().insert(
        HeaderName::from_static(phoebis_relay::ERROR_CODE_HEADER),
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
    use super::{parse_object, same_authority, spend_rows};
    use axum::body::Bytes;
    use axum::http::StatusCode;
    use phoebis_routing::MemberSpend;
    use phoebis_store::BudgetPeriod;
    use serde::Deserialize;
    use serde_json::{json, Value};

    #[test]
    fn same_authority_names_the_requests_own_host_and_requires_https_when_asked() {
        assert!(same_authority(
            "http://localhost:8081",
            "localhost:8081",
            false
        ));
        assert!(same_authority(
            "HTTP://LocalHost:8081",
            "localhost:8081",
            false
        ));
        assert!(!same_authority(
            "http://localhost:5173",
            "localhost:8081",
            false
        ));
        assert!(!same_authority("null", "localhost:8081", false));
        assert!(!same_authority(
            "https://localhost:8081",
            "localhost",
            false
        ));
        assert!(!same_authority(
            "ftp://localhost:8081",
            "localhost:8081",
            false
        ));
        assert!(same_authority(
            "https://localhost:8081",
            "localhost:8081",
            true
        ));
        assert!(same_authority(
            "HTTPS://LocalHost:8081",
            "localhost:8081",
            true
        ));
        assert!(!same_authority(
            "http://localhost:8081",
            "localhost:8081",
            true
        ));
        assert!(!same_authority(
            "https://localhost:5173",
            "localhost:8081",
            true
        ));
    }

    #[test]
    fn spend_rows_answers_every_period_of_one_member_and_no_other() {
        let row = |account_id: &str, period| MemberSpend {
            pool_id: "main".into(),
            account_id: account_id.into(),
            period,
            period_start: 0,
            period_end: 2_678_400_000,
            spent_usd: 12.5,
            observed_at: 0,
        };
        let spends = vec![
            row("a", BudgetPeriod::Day),
            row("a", BudgetPeriod::Month),
            row("b", BudgetPeriod::Month),
        ];
        assert_eq!(
            spend_rows(&spends, "main", "a")
                .iter()
                .map(|s| s.period)
                .collect::<Vec<_>>(),
            vec![BudgetPeriod::Day, BudgetPeriod::Month],
            "both of the member's reports, whatever their period"
        );
        assert!(
            spend_rows(&spends, "other", "a").is_empty(),
            "the same account under another pool reads none"
        );
    }

    #[tokio::test]
    async fn parse_object_reads_an_object_and_refuses_an_array_a_string_and_malformed_bytes() {
        #[derive(Debug, Default, Deserialize, PartialEq)]
        #[serde(default)]
        struct Pair {
            a: u32,
            b: u32,
        }
        assert_eq!(
            parse_object::<Pair>(&Bytes::from_static(br#"{"a": 1}"#)).ok(),
            Some(Pair { a: 1, b: 0 })
        );
        for body in [&b"[]"[..], br#""x""#, b"{"] {
            let Err(response) = parse_object::<Pair>(&Bytes::copy_from_slice(body)) else {
                panic!("{} read as an object", String::from_utf8_lossy(body));
            };
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                (status, serde_json::from_slice::<Value>(&bytes).unwrap()),
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "body must be a JSON object"})
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[tokio::test]
    async fn parse_object_refuses_a_body_that_repeats_a_member() {
        #[derive(Debug, Default, Deserialize, PartialEq)]
        #[serde(default)]
        struct Pair {
            a: u32,
            b: u32,
        }
        let body = br#"{"a": 1, "a": 2}"#;
        let Err(response) = parse_object::<Pair>(&Bytes::from_static(body)) else {
            panic!("a repeated member read as an object");
        };
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            (status, serde_json::from_slice::<Value>(&bytes).unwrap()),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "body must be a JSON object"})
            )
        );
    }
}
