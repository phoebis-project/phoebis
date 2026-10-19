mod common;

#[tokio::test]
async fn login_sets_a_cookie_that_reaches_every_route_until_logout() {
    use common::{cookie_of, delete, get, post};
    let (app, _up) = common::admin_app().await;
    let bad = post(&app, "/admin/session", r#"{"secret":"nope"}"#, None).await;
    assert_eq!(bad.status(), 401);
    let ok = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    assert_eq!(ok.status(), 204);
    let cookie = cookie_of(&ok);
    assert!(ok
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("HttpOnly"));
    assert_eq!(
        get(&app, "/admin/accounts", Some(&cookie)).await.status(),
        200
    );
    assert_eq!(
        post(&app, "/admin/accounts/business/pause", "", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        delete(&app, "/admin/session", Some(&cookie)).await.status(),
        204
    );
    assert_eq!(
        get(&app, "/admin/accounts", Some(&cookie)).await.status(),
        401,
        "the logged-out cookie no longer resolves"
    );
    assert_eq!(get(&app, "/admin/accounts", None).await.status(), 401);
}

#[tokio::test]
async fn a_dead_cookie_on_logout_answers_204_with_the_clear() {
    use common::{delete, login};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    assert_eq!(
        delete(&app, "/admin/session", Some(&cookie)).await.status(),
        204
    );
    for (presented, what) in [
        (Some(cookie.as_str()), "a cookie already logged out"),
        (Some("never-issued"), "a cookie no session ever had"),
        (None, "no cookie at all"),
    ] {
        let again = delete(&app, "/admin/session", presented).await;
        assert_eq!(again.status(), 204, "{what}");
        let clear = again
            .headers()
            .get("set-cookie")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(
            clear.starts_with("phoebis_admin=;") && clear.contains("Max-Age=0"),
            "{what} still clears the browser's copy: {clear:?}"
        );
    }
}

#[tokio::test]
async fn the_cookie_carries_secure_under_tls_or_a_fronts_https_and_not_otherwise() {
    use common::{post, post_forwarded};
    const BODY: &str = r#"{"secret":"admin-secret"}"#;
    let (plain, _up) = common::admin_app().await;
    let bare = post(&plain, "/admin/session", BODY, None).await;
    assert_eq!(bare.status(), 204);
    assert!(!cookie_attributes(&bare).contains("Secure"));

    let (tls, _up) = common::admin_app_tls().await;
    let terminated = post(&tls, "/admin/session", BODY, None).await;
    assert_eq!(terminated.status(), 204);
    // The whole attribute, not a substring of one: `contains` would also pass on a cookie value that happened
    // to carry the word.
    assert!(cookie_attributes(&terminated).ends_with("; Secure"));

    let fronted = post_forwarded(&plain, "/admin/session", BODY, None, "https").await;
    assert_eq!(fronted.status(), 204);
    assert!(cookie_attributes(&fronted).ends_with("; Secure"));

    let chained = post_forwarded(&plain, "/admin/session", BODY, None, "http, https").await;
    assert_eq!(chained.status(), 204);
    assert!(
        !cookie_attributes(&chained).contains("Secure"),
        "the first value is the browser's leg"
    );
}

fn cookie_attributes(response: &axum::http::Response<axum::body::Body>) -> String {
    response
        .headers()
        .get("set-cookie")
        .expect("a login response carries the session cookie")
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_foreign_origin_is_refused_and_a_missing_one_is_admitted() {
    use common::{json, login, post, post_from, send_with_host};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let pause = "/admin/accounts/business/pause";
    let foreign = post_from(
        &parts.app,
        pause,
        "",
        Some(&cookie),
        "http://localhost:5173",
    )
    .await;
    assert_eq!(foreign.status(), 403);
    assert_eq!(json(foreign).await["error"], "origin is not allowed");
    // The same-host check compares against what the request says its Host is.
    let (origin, host) = ("http://localhost:8081", "localhost:8081");
    let same = send_with_host(&parts.app, "POST", pause, "", Some(&cookie), origin, host).await;
    assert_ne!(
        same.status(),
        403,
        "an origin that names the request's own host is admitted"
    );
    let none = post(&parts.app, pause, "", Some(&cookie)).await;
    assert_ne!(none.status(), 403, "curl sends no Origin");
}

/// A page on this host over plain HTTP could drive a bodiless write with a cookie some browsers send across
/// schemes, so a listener that terminates TLS admits only its own `https://` origin.
#[tokio::test]
async fn an_origin_must_be_https_when_the_admin_listener_terminates_tls() {
    use common::{cookie_of, json, post, send_with_host};
    const HOST: &str = "phoebis.example";
    let (tls, _up) = common::admin_app_tls().await;
    let forced = send_with_host(
        &tls,
        "POST",
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
        "http://phoebis.example",
        HOST,
    )
    .await;
    assert_eq!(forced.status(), 403);
    assert!(
        forced.headers().get("set-cookie").is_none(),
        "a refused login sets no cookie"
    );
    assert_eq!(json(forced).await["error"], "origin is not allowed");
    let login = post(&tls, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    assert_eq!(login.status(), 204);
    let cookie = cookie_of(&login);
    let pause = "/admin/accounts/business/pause";
    let plain = send_with_host(
        &tls,
        "POST",
        pause,
        "",
        Some(&cookie),
        "http://phoebis.example",
        HOST,
    )
    .await;
    assert_eq!(plain.status(), 403);
    assert_eq!(json(plain).await["error"], "origin is not allowed");
    let secure = send_with_host(
        &tls,
        "POST",
        pause,
        "",
        Some(&cookie),
        "https://phoebis.example",
        HOST,
    )
    .await;
    assert_ne!(
        secure.status(),
        403,
        "the listener's own https origin is admitted"
    );
}

/// A front that terminates TLS reports the browser's leg in `X-Forwarded-Proto`, and an https leg needs an https
/// origin as a listener that terminates TLS does.
#[tokio::test]
async fn a_fronts_https_makes_the_origin_https_only_on_a_plain_listener() {
    use common::{json, login, send_fronted};
    const HOST: &str = "phoebis.example";
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let pause = "/admin/accounts/business/pause";
    let plain = send_fronted(
        &app,
        "POST",
        pause,
        "",
        Some(&cookie),
        "http://phoebis.example",
        HOST,
        "https",
    )
    .await;
    assert_eq!(plain.status(), 403);
    assert_eq!(json(plain).await["error"], "origin is not allowed");
    let secure = send_fronted(
        &app,
        "POST",
        pause,
        "",
        Some(&cookie),
        "https://phoebis.example",
        HOST,
        "https",
    )
    .await;
    assert_ne!(secure.status(), 403, "the front's https origin is admitted");
    let chained = send_fronted(
        &app,
        "POST",
        pause,
        "",
        Some(&cookie),
        "http://phoebis.example",
        HOST,
        "http, https",
    )
    .await;
    assert_ne!(
        chained.status(),
        403,
        "the first value is the browser's leg"
    );
}

#[tokio::test]
async fn a_plain_listener_behind_no_front_admits_an_origin_of_either_scheme() {
    use common::{login, send_with_host};
    const HOST: &str = "phoebis.example";
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let pause = "/admin/accounts/business/pause";
    for origin in ["http://phoebis.example", "https://phoebis.example"] {
        let response = send_with_host(&app, "POST", pause, "", Some(&cookie), origin, HOST).await;
        assert_ne!(
            response.status(),
            403,
            "{origin} names the request's own host"
        );
    }
}

/// The scheme is required on a listener that terminates TLS whatever a front says, so a downgrade word relaxes
/// nothing.
#[tokio::test]
async fn a_forwarded_header_on_a_listener_that_terminates_tls_changes_nothing() {
    use common::{json, login, send_fronted};
    const HOST: &str = "phoebis.example";
    let (tls, _up) = common::admin_app_tls().await;
    let cookie = login(&tls, "admin-secret").await;
    let pause = "/admin/accounts/business/pause";
    let downgraded = send_fronted(
        &tls,
        "POST",
        pause,
        "",
        Some(&cookie),
        "http://phoebis.example",
        HOST,
        "http",
    )
    .await;
    assert_eq!(downgraded.status(), 403, "a downgrade word relaxes nothing");
    assert_eq!(json(downgraded).await["error"], "origin is not allowed");
    let secure = send_fronted(
        &tls,
        "POST",
        pause,
        "",
        Some(&cookie),
        "https://phoebis.example",
        HOST,
        "http",
    )
    .await;
    assert_ne!(
        secure.status(),
        403,
        "the listener's own https origin is admitted"
    );
    let plain = send_fronted(
        &tls,
        "POST",
        pause,
        "",
        Some(&cookie),
        "http://phoebis.example",
        HOST,
        "https",
    )
    .await;
    assert_eq!(plain.status(), 403);
    assert_eq!(json(plain).await["error"], "origin is not allowed");
}

#[tokio::test]
async fn a_body_carrying_mutation_needs_the_json_content_type() {
    use common::{json, login, send_typed};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let pause = "/admin/accounts/business/pause";
    let key = r#"{"id":"k","label":"K","pool_id":"main"}"#;
    let plain = send_typed(
        &parts.app,
        "POST",
        "/admin/keys",
        key,
        Some(&cookie),
        Some("text/plain"),
    )
    .await;
    assert_eq!(plain.status(), 415);
    assert_eq!(
        json(plain).await["error"],
        "content type must be application/json"
    );
    let untyped = send_typed(&parts.app, "POST", "/admin/keys", key, Some(&cookie), None).await;
    assert_eq!(untyped.status(), 415);
    let login_body = r#"{"secret":"admin-secret"}"#;
    let typed = Some("application/json; charset=utf-8");
    let charset = send_typed(
        &parts.app,
        "POST",
        "/admin/session",
        login_body,
        None,
        typed,
    )
    .await;
    assert_eq!(
        charset.status(),
        204,
        "a parameter after the media type is fine"
    );
    let bodiless = send_typed(&parts.app, "POST", pause, "", Some(&cookie), None).await;
    assert_ne!(
        bodiless.status(),
        415,
        "a mutation with no body carries no content type"
    );
}

/// A scripted call: `Authorization: Bearer <bearer>`, no cookie, and the `Origin` and body given, a body typed
/// `application/json`.
async fn with_bearer(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: &str,
    origin: Option<&str>,
    body: &str,
) -> axum::http::Response<axum::body::Body> {
    let content_type = (!body.is_empty()).then_some("application/json");
    let authorization = format!("Bearer {bearer}");
    authorized(
        app,
        method,
        path,
        &authorization,
        origin,
        content_type,
        body,
    )
    .await
}

/// A call carrying the `Authorization` value given whole, no cookie, and the `Origin` and `Content-Type` given.
async fn authorized(
    app: &axum::Router,
    method: &str,
    path: &str,
    authorization: &str,
    origin: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> axum::http::Response<axum::body::Body> {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", authorization);
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    if !body.is_empty() {
        request = request.header("content-length", body.len().to_string());
    }
    app.clone()
        .oneshot(
            request
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_bearer_admits_a_read_and_a_mutation_without_a_cookie_or_an_origin() {
    use common::json;
    let (app, _up) = common::admin_app().await;
    let system = with_bearer(&app, "GET", "/admin/system", "admin-secret", None, "").await;
    assert_eq!(system.status(), 200);
    let pause = "/admin/accounts/business/pause";
    let paused = with_bearer(
        &app,
        "POST",
        pause,
        "admin-secret",
        Some("http://elsewhere"),
        "",
    )
    .await;
    assert_eq!(paused.status(), 204, "the bearer skips the Origin check");
    let whoami = with_bearer(&app, "GET", "/admin/session", "admin-secret", None, "").await;
    assert_eq!(whoami.status(), 200);
    assert_eq!(
        json(whoami).await,
        serde_json::json!({"session_id": "bearer"})
    );
    // The scheme word is matched ignoring case, and the JSON rule still holds under a bearer.
    let lower = authorized(
        &app,
        "GET",
        "/admin/system",
        "bearer admin-secret",
        None,
        None,
        "",
    )
    .await;
    assert_eq!(lower.status(), 200);
    let untyped = authorized(
        &app,
        "POST",
        "/admin/keys",
        "Bearer admin-secret",
        None,
        Some("text/plain"),
        r#"{"id":"k","label":"K","pool_id":"main"}"#,
    )
    .await;
    assert_eq!(untyped.status(), 415);
    assert_eq!(
        json(untyped).await["error"],
        "content type must be application/json"
    );
}

#[tokio::test]
async fn a_wrong_bearer_is_refused_and_counts_toward_the_throttle() {
    use common::{json, post};
    let (app, _up) = common::admin_app().await;
    for attempt in 0..5 {
        let refused = with_bearer(&app, "GET", "/admin/system", "nope", None, "").await;
        assert_eq!(refused.status(), 401, "attempt {attempt}");
        assert_eq!(json(refused).await["error"], "secret does not match");
    }
    let sixth = with_bearer(&app, "GET", "/admin/system", "nope", None, "").await;
    assert_eq!(sixth.status(), 429);
    assert_eq!(
        json(sixth).await["error"],
        "too many failed logins, try again in a minute"
    );
    let login = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    assert_eq!(
        login.status(),
        429,
        "the bearers' failures throttle the login"
    );
    let correct = with_bearer(&app, "GET", "/admin/system", "admin-secret", None, "").await;
    assert_eq!(
        correct.status(),
        429,
        "a throttled process refuses a bearer too"
    );
}

#[tokio::test]
async fn a_bearer_on_the_login_route_is_ignored() {
    use common::json;
    let (app, _up) = common::admin_app().await;
    let bodiless = with_bearer(&app, "POST", "/admin/session", "admin-secret", None, "").await;
    assert_eq!(bodiless.status(), 400);
    assert_eq!(json(bodiless).await["error"], "body must carry a secret");
    // The login stays the cookie's door: its Origin check holds with a bearer on it, and a wrong bearer neither
    // refuses a correct login nor counts against the throttle.
    let login = r#"{"secret":"admin-secret"}"#;
    let foreign = with_bearer(
        &app,
        "POST",
        "/admin/session",
        "admin-secret",
        Some("http://elsewhere"),
        login,
    )
    .await;
    assert_eq!(foreign.status(), 403);
    assert_eq!(json(foreign).await["error"], "origin is not allowed");
    for attempt in 0..6 {
        let wrong = with_bearer(&app, "POST", "/admin/session", "nope", None, login).await;
        assert_eq!(wrong.status(), 204, "attempt {attempt}");
        assert!(wrong.headers().contains_key("set-cookie"));
    }
}

#[tokio::test]
async fn pairing_runs_start_callback_confirm_and_never_exposes_tokens() {
    use common::{get, json, login, post};
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let started = post(
        &app,
        "/admin/oauth/sessions",
        r#"{"account_id":"business","login":"claude_ai"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), 201);
    let v = json(started).await;
    let id = v["id"].as_str().unwrap().to_string();
    let url = v["authorization_url"].as_str().unwrap();
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize?code=true&client_id="));
    let redirect = v["redirect_uri"].as_str().unwrap().to_string();
    assert!(
        redirect.starts_with("http://localhost:") && redirect.ends_with("/callback"),
        "the redirect is the client's loopback form: {redirect}"
    );
    assert!(
        url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A"),
        "the authorization URL registers the loopback redirect: {url}"
    );
    let state = url.split("state=").nth(1).unwrap().to_string();
    let cb = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(cb.status(), 200);
    let v = json(cb).await;
    assert_eq!(v["status"], "exchanged");
    assert_eq!(v["pending"]["email"], "test@example.invalid");
    let body = serde_json::to_string(&v).unwrap();
    assert!(
        !body.contains("sk-ant-oat01") && !body.contains("sk-ant-ort01"),
        "no token leaves the backend"
    );
    // Bound to a local: indexing into the temporary Vec that requests() returns would not outlive the statement.
    let recorded = up.requests();
    let token_req = recorded
        .iter()
        .find(|r| r.line.starts_with("POST /v1/oauth/token"))
        .unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&token_req.body).unwrap();
    assert_eq!(sent["grant_type"], "authorization_code");
    assert_eq!(sent["code"], "CODE123");
    let keys: Vec<&String> = sent.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "grant_type",
            "code",
            "redirect_uri",
            "client_id",
            "code_verifier",
            "state"
        ]
    );
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 200);
    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(shown["status"], "confirmed");
    let acct = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(acct["state"], "ready");
    assert_eq!(
        acct["identity"]["account_uuid"],
        "00000000-0000-4000-8000-000000000001"
    );
    assert!(acct["credential"]["access_expires_at"].is_number());
    assert_eq!(acct["credential"]["quarantined"], false);
    let recorded = up.requests();
    let profile_calls = recorded
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
        .count();
    let roles_calls = recorded
        .iter()
        .filter(|r| r.line.starts_with("GET /api/oauth/claude_cli/roles"))
        .count();
    assert!(
        profile_calls >= 1 && roles_calls == 1,
        "the native post-login lookups were issued"
    );
}

/// Starts, calls back and confirms a pairing of `business`, answering the confirm's response.
async fn pair_business(app: &axum::Router, cookie: &str) -> axum::response::Response {
    use common::{json, post};
    let started = post(
        app,
        "/admin/oauth/sessions",
        r#"{"account_id":"business","login":"claude_ai"}"#,
        Some(cookie),
    )
    .await;
    assert_eq!(started.status(), 201);
    let started = json(started).await;
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
    assert_eq!(callback.status(), 200);
    post(
        app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(cookie),
    )
    .await
}

/// The router's sample of the account once it carries the reading taken at `observed_at`, within five seconds:
/// the poll hands the router its report after it records the ledger's rows, so a read that found the rows can be
/// one report ahead of the router.
async fn routed_sample(
    router: &phoebis_routing::RouterHandle,
    account_id: &str,
    observed_at: i64,
) -> phoebis_routing::UtilizationSample {
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let found = router
            .samples()
            .await
            .unwrap()
            .into_iter()
            .find_map(|(id, sample)| {
                (id == account_id && sample.observed_at == observed_at).then_some(sample)
            });
        if let Some(sample) = found {
            return sample;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "the router never held a reading of {account_id} taken at {observed_at}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Two settled failures, the run `refresh_failed` holds on, with nothing left open for a commit to supersede.
fn seed_failed_refreshes(store: &phoebis_store::Store) {
    for detail in ["first", "second"] {
        let attempt = store.open_refresh_attempt("business", 1, 1, 1).unwrap();
        store
            .resolve_refresh_attempt(
                attempt,
                1,
                phoebis_store::AttemptOutcome::Failed(detail.into()),
            )
            .unwrap();
    }
}

async fn refresh_failed_holds(app: &axum::Router, cookie: &str) -> bool {
    let body = common::json(common::get(app, "/admin/attention", Some(cookie)).await).await;
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["kind"] == "refresh_failed" && item["subject"]["id"] == "business")
}

#[tokio::test]
async fn a_pairing_over_an_uncertain_refresh_row_leaves_the_commit_as_the_newest_resolved_attempt()
{
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = common::login(&app, "admin-secret").await;
    let attempt = store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Uncertain("lost".into()),
        )
        .unwrap();
    assert_eq!(pair_business(&app, &cookie).await.status(), 200);
    let newest: Vec<(String, Option<String>)> = store
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap()
        .into_iter()
        .map(|a| (a.outcome, a.detail))
        .collect();
    assert_eq!(
        newest,
        [
            ("committed".to_string(), Some("pairing".to_string())),
            ("failed".to_string(), Some("superseded".to_string())),
        ]
    );
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
}

#[tokio::test]
async fn a_pairing_ends_a_run_of_failed_refreshes_with_its_own_committed_attempt() {
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = common::login(&app, "admin-secret").await;
    seed_failed_refreshes(&store);
    assert!(refresh_failed_holds(&app, &cookie).await);
    assert_eq!(pair_business(&app, &cookie).await.status(), 200);
    let newest = store
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap();
    assert_eq!(
        (newest[0].outcome.as_str(), newest[0].detail.as_deref()),
        ("committed", Some("pairing"))
    );
    assert!(
        !refresh_failed_holds(&app, &cookie).await,
        "the pairing's own attempt ends the run"
    );
}

#[tokio::test]
async fn a_pairing_whose_credential_commit_fails_records_no_committed_attempt() {
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = common::login(&app, "admin-secret").await;
    seed_failed_refreshes(&store);
    store.debug_fail_next_credential_commit();
    assert_eq!(pair_business(&app, &cookie).await.status(), 500);
    assert!(
        store
            .newest_resolved_refresh_attempts("business", 1, 16)
            .unwrap()
            .iter()
            .all(|a| a.detail.as_deref() != Some("pairing")),
        "no success is recorded for a credential that was never stored"
    );
    assert!(refresh_failed_holds(&app, &cookie).await);
}

#[tokio::test]
async fn a_pairing_commit_replaces_the_bundle_the_manager_had_cached() {
    use common::{json, login, post};
    let (app, _up, _store, manager) = common::paired_parts().await;
    let cached = manager.acquire("business").await.unwrap();
    assert_eq!(cached.generation, 1);
    let cookie = login(&app, "admin-secret").await;
    let started = post(
        &app,
        "/admin/oauth/sessions",
        r#"{"account_id":"business","login":"claude_ai"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), 201);
    let started = json(started).await;
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
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(callback.status(), 200);
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 200);
    // The cached bundle is far from its expiry, so only the confirm's own drop of it brings the new one.
    let paired = manager.acquire("business").await.unwrap();
    assert_eq!(
        paired.generation, 2,
        "the confirmed bundle, not the cached one"
    );
}

#[tokio::test]
async fn a_confirmed_pairing_stores_the_plan_and_the_start_at_once() {
    use common::{get, json, login, post};
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let mut profile: serde_json::Value =
        serde_json::from_slice(phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Profile,
        ))
        .unwrap();
    profile["account"]["has_claude_max"] = serde_json::json!(true);
    profile["organization"]["subscription_created_at"] = serde_json::json!("2026-03-14T10:00:00Z");
    profile["organization"]["claude_code_trial_ends_at"] =
        serde_json::json!("2026-10-02T00:00:00+00:00");
    up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&profile).unwrap(),
        },
    );
    let started = post(
        &app,
        "/admin/oauth/sessions",
        r#"{"account_id":"business","login":"claude_ai"}"#,
        Some(&cookie),
    )
    .await;
    let v = json(started).await;
    let id = v["id"].as_str().unwrap().to_string();
    let redirect = v["redirect_uri"].as_str().unwrap().to_string();
    let state = v["authorization_url"]
        .as_str()
        .unwrap()
        .split("state=")
        .nth(1)
        .unwrap()
        .to_string();
    let cb = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(cb.status(), 200);
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 200);
    let acct = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        acct["subscription"],
        serde_json::json!({
            "plan": "max",
            "tier": "default_claude_max_5x",
            "since": 1_773_482_400_000_i64,
            "trial_ends_at": 1_790_899_200_000_i64,
            "ends_at": null,
        }),
        "the confirm stored the plan and the start with no poll having run"
    );
}

#[tokio::test]
async fn a_confirmed_pairing_polls_the_quota_at_once_and_a_refresh_records_its_reading() {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    let h = common::admin_parts_full().await;
    let cookie = login(&h.app, "admin-secret").await;
    let asked = || {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count()
    };
    // The endpoint records the poll's request and holds its answer, so the confirm's answer and the reads right
    // after it land while nothing is recorded yet.
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Gated(Box::new(fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Usage,
            )
            .to_vec(),
        })),
    );
    let tracked = h.ledger.tracked();
    let confirmed = pair_business(&h.app, &cookie).await;
    assert_eq!(
        confirmed.status(),
        200,
        "the confirm answers before the held endpoint does"
    );
    assert_eq!(
        h.ledger.tracked(),
        tracked + 1,
        "the confirm's poll is tracked, so a shutdown aborts it before the channel closes"
    );
    let bucket = phoebis_ledger::bucket_of(NOW);
    assert!(
        h.db.quota_samples("business", bucket, bucket + 300)
            .await
            .unwrap()
            .is_empty(),
        "nothing is recorded before the endpoint answers"
    );
    let acct = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(acct["windows"].as_array().map(Vec::len), Some(0), "{acct}");
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while asked() < 1 {
        assert!(
            tokio::time::Instant::now() < give_up,
            "the confirm never polled the endpoint"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(asked(), 1, "the confirm's poll is one request");
    h.up.release();
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let rows = loop {
        let rows =
            h.db.quota_samples("business", bucket, bucket + 300)
                .await
                .unwrap();
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "the confirm's poll never recorded a sample"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(
        rows.iter().map(|r| r.window.as_str()).collect::<Vec<_>>(),
        ["five_hour", "seven_day"],
        "one row per window the endpoint reports: {rows:?}"
    );
    assert!(rows.iter().all(|r| r.source != "head"), "{rows:?}");
    let sample = routed_sample(&h.router, "business", NOW).await;
    assert!(
        sample.reading("five_hour").is_some(),
        "the confirm's poll handed the router the windows: {sample:?}"
    );
    let acct = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert!(
        acct["windows"].as_array().is_some_and(|w| !w.is_empty()),
        "{acct}"
    );
    let usage = json(get(&h.app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert_eq!(
        (usage["fetched_at"].as_i64(), usage["error"].is_null()),
        (Some(NOW), true),
        "{usage}"
    );

    // The next bucket, so the refresh's reading is a row of its own.
    h.clock_cell.store(NOW + 300_000, Ordering::SeqCst);
    let refreshed = post(
        &h.app,
        "/admin/accounts/business/usage/refresh",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(refreshed.status(), 200);
    let refreshed = json(refreshed).await;
    assert!(refreshed["error"].is_null(), "{refreshed}");
    assert_eq!(refreshed["fetched_at"], NOW + 300_000);
    assert_eq!(asked(), 2, "one endpoint request per refresh");
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let rows = loop {
        let rows =
            h.db.quota_samples("business", bucket + 300, bucket + 600)
                .await
                .unwrap();
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "the refresh never recorded a sample"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert!(rows.iter().any(|r| r.window == "five_hour"), "{rows:?}");
    let sample = routed_sample(&h.router, "business", NOW + 300_000).await;
    assert!(
        sample.reading("five_hour").is_some(),
        "the refresh handed the router its reading: {sample:?}"
    );
}

#[tokio::test]
async fn a_confirm_says_its_first_poll_is_in_flight_and_the_list_row_lists_the_windows_it_lands() {
    use common::{get, json, login, NOW};
    let h = common::admin_parts_full().await;
    let cookie = login(&h.app, "admin-secret").await;
    let asked = || {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count()
    };
    // The endpoint holds its answer, so the confirm answers while the poll it started is still waiting on it.
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Gated(Box::new(fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Usage,
            )
            .to_vec(),
        })),
    );
    let confirmed = pair_business(&h.app, &cookie).await;
    assert_eq!(confirmed.status(), 200);
    let confirmed = json(confirmed).await;
    assert_eq!(
        (
            confirmed["account_id"].as_str(),
            confirmed["status"].as_str(),
            confirmed["first_poll_in_flight"].as_bool(),
        ),
        (Some("business"), Some("confirmed"), Some(true)),
        "{confirmed}"
    );
    let windows_listed = |listed: serde_json::Value| {
        listed["accounts"]
            .as_array()
            .and_then(|rows| rows.iter().find(|a| a["id"] == "business"))
            .and_then(|row| row["windows"].as_array().map(Vec::len))
    };
    let listed = json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await;
    assert_eq!(
        windows_listed(listed),
        Some(0),
        "the row lists no window while the poll is in flight"
    );
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while asked() < 1 {
        assert!(
            tokio::time::Instant::now() < give_up,
            "the confirm never polled the endpoint"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    h.up.release();
    routed_sample(&h.router, "business", NOW).await;
    let listed = json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await;
    assert!(
        windows_listed(listed).is_some_and(|n| n > 0),
        "the row lists the first reading's windows once the poll landed"
    );
}

#[tokio::test]
async fn the_usage_get_answers_the_polls_latest_snapshot_and_standing_without_a_fetch() {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    // A refresh reads the endpoint through the poll's path whatever the cache holds and answers the reading that read
    // made, so the GET's own reads show beside it.
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let path = "/admin/accounts/business/usage";
    let refresh = "/admin/accounts/business/usage/refresh";
    let asked = || {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count()
    };
    let before = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(
        (
            before["error"].as_str(),
            before["fetched_at"].is_null(),
            before["poll"].is_null(),
            before["identity_generation"].as_u64(),
            before["windows"].as_array().map(Vec::len),
        ),
        (
            Some("the poll has not read this account yet"),
            true,
            true,
            Some(0),
            Some(0)
        ),
        "{before}"
    );
    assert_eq!(asked(), 0, "the GET never reads the endpoint itself");

    h.usage.poll("business").await.unwrap();
    let first = json(get(&h.app, path, Some(&cookie)).await).await;
    let second = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(asked(), 1, "two GETs after the poll add no request");
    assert_eq!(first, second);
    assert_eq!(first["windows"][0]["key"], "five_hour");
    assert!(first["error"].is_null(), "{first}");
    assert_eq!(
        first["poll"],
        serde_json::json!({
            "backoff_since": null,
            "last_attempt_at": NOW,
            "last_clean_at": NOW,
            "last_error": null,
            "last_outcome": "clean",
            "next_attempt_at": null,
            "rate_limited_in_a_row": 0,
        })
    );
    assert!(!first.to_string().contains("sk-ant-"), "{first}");

    // A threshold of 0 applies the switch at the rule's next request, whatever it has counted so far.
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    h.clock_cell.store(NOW + 10_000, Ordering::SeqCst);
    let refused = post(&h.app, refresh, "", Some(&cookie)).await;
    assert_eq!(refused.status(), 200);
    let refused = json(refused).await;
    assert_eq!(asked(), 2);
    assert_eq!(refused["error"], "usage request rate-limited");
    assert_eq!(
        refused["poll"],
        serde_json::json!({
            "backoff_since": NOW + 10_000,
            "last_attempt_at": NOW + 10_000,
            "last_clean_at": NOW,
            "last_error": "the usage endpoint rate-limited the account",
            "last_outcome": "rate_limited",
            "next_attempt_at": NOW + 10_000 + 120_000,
            "rate_limited_in_a_row": 1,
        }),
        "the first 429 holds the account for the ladder's first step, two minutes"
    );
    assert_eq!(
        refused["held_until"],
        NOW + 10_000 + 120_000,
        "the hold's deadline is the poll's next attempt, answered as a member of its own"
    );
    let held = json(get(&h.app, path, Some(&cookie)).await).await;
    assert!(
        held["error"].is_null(),
        "an account with windows never reads as an error: {held}"
    );
    assert_eq!(held["windows"], first["windows"]);
    assert_eq!(held["fetched_at"], NOW, "the windows are the clean read's");
    assert_eq!(held["poll"], refused["poll"]);
    assert_eq!(
        held["held_until"], refused["held_until"],
        "the GET inside the hold carries the deadline beside the windows"
    );

    h.clock_cell.store(NOW + 40_000, Ordering::SeqCst);
    let again = json(post(&h.app, refresh, "", Some(&cookie)).await).await;
    assert_eq!(asked(), 2, "a refresh inside the hold reads nothing");
    assert_eq!(again["poll"], refused["poll"]);
    assert!(
        again["error"]
            .as_str()
            .is_some_and(|e| e.contains("rate-limited the account")),
        "{again}"
    );

    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Usage,
            )
            .to_vec(),
        },
    );
    h.clock_cell.store(NOW + 130_000, Ordering::SeqCst);
    let restored = json(post(&h.app, refresh, "", Some(&cookie)).await).await;
    assert_eq!(asked(), 3, "past the hold the refresh reads the endpoint");
    assert!(restored["error"].is_null(), "{restored}");
    assert_eq!(
        restored["poll"],
        serde_json::json!({
            "backoff_since": null,
            "last_attempt_at": NOW + 130_000,
            "last_clean_at": NOW + 130_000,
            "last_error": null,
            "last_outcome": "clean",
            "next_attempt_at": null,
            "rate_limited_in_a_row": 0,
        })
    );
}

#[tokio::test]
async fn a_refresh_whose_read_ends_after_the_clock_moved_answers_the_one_reading_it_recorded() {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let asked = || {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count()
    };
    let bucket = phoebis_ledger::bucket_of(NOW);
    // The endpoint records the read's request and holds its answer, so the clock moves on while the read is out.
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Gated(Box::new(fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Usage,
            )
            .to_vec(),
        })),
    );
    let app = h.app.clone();
    let c = cookie.clone();
    let refreshing = tokio::spawn(async move {
        json(post(&app, "/admin/accounts/business/usage/refresh", "", Some(&c)).await).await
    });
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while asked() < 1 {
        assert!(
            tokio::time::Instant::now() < give_up,
            "the refresh never reached the endpoint"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Two seconds past the send instant, which is what stamps the reading.
    h.clock_cell.store(NOW + 2_000, Ordering::SeqCst);
    h.up.release();
    let refreshed = refreshing.await.unwrap();
    assert_eq!(asked(), 1, "one endpoint request per refresh");
    assert!(refreshed["error"].is_null(), "{refreshed}");
    assert_eq!(refreshed["fetched_at"], NOW);
    let windows = refreshed["windows"].as_array().unwrap();
    assert!(
        windows.iter().any(|w| w["key"] == "five_hour"),
        "{refreshed}"
    );
    let read: Vec<(String, f64)> = windows
        .iter()
        .filter(|w| !w["utilization"].is_null())
        .map(|w| {
            (
                w["key"].as_str().unwrap().to_string(),
                w["utilization"].as_f64().unwrap(),
            )
        })
        .collect();
    assert!(!read.is_empty(), "{refreshed}");

    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let rows = loop {
        let rows =
            h.db.quota_samples("business", bucket, bucket + 300)
                .await
                .unwrap();
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "the refresh never recorded its reading"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(rows.len(), read.len(), "one row per window read: {rows:?}");
    for (key, percent) in &read {
        let row = rows
            .iter()
            .find(|r| &r.window == key)
            .unwrap_or_else(|| panic!("no row under {key}: {rows:?}"));
        assert_eq!(
            (row.source.as_str(), row.observed_at, row.utilization),
            ("poll", Some(NOW), Some(percent / 100.0)),
            "{row:?}"
        );
    }
    let sample = routed_sample(&h.router, "business", NOW).await;
    for (key, _) in &read {
        assert!(
            sample.reading(key).is_some(),
            "{key} is missing from the router's reading: {sample:?}"
        );
    }
    let acct = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let routed = acct["windows"].as_array().unwrap();
    assert_eq!(routed.len(), read.len(), "{acct}");
    for (key, percent) in &read {
        let entry = routed
            .iter()
            .find(|w| w["id"] == key.as_str())
            .unwrap_or_else(|| panic!("no window {key} on the account: {acct}"));
        assert_eq!(
            entry["utilization"],
            serde_json::json!(percent / 100.0),
            "{acct}"
        );
    }
    let latest = json(get(&h.app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert_eq!(
        (&latest["windows"], &latest["fetched_at"]),
        (&refreshed["windows"], &refreshed["fetched_at"]),
        "{latest}"
    );
    assert_eq!(asked(), 1, "the GET reads nothing");
}

/// A refresh hands the router its reading as soon as it answers, while the ledger's writer may still hold the row:
/// the card draws the reading the refresh took, at the instant it was taken, over the ledger's older row.
#[tokio::test]
async fn a_refresh_and_the_list_read_after_it_carry_its_reading_at_its_own_instant_while_the_ledger_holds_an_older_row(
) {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    const HOUR: i64 = 3_600_000;
    let h = common::admin_parts_full().await;
    let cookie = login(&h.app, "admin-secret").await;
    assert_eq!(pair_business(&h.app, &cookie).await.status(), 200);
    let first = phoebis_ledger::bucket_of(NOW);
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while h
        .db
        .quota_samples("business", first, first + 300)
        .await
        .unwrap()
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < give_up,
            "the confirm's poll never recorded a sample"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    routed_sample(&h.router, "business", NOW).await;

    common::hold_the_writer(
        &h,
        phoebis_ledger::RequestRecord {
            id: "held".into(),
            received_at: NOW,
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            ..Default::default()
        },
        vec![],
    )
    .await;
    // Figures of their own, so the card's reading tells the refresh's from the confirm's.
    let rfc3339 = |at: i64| jiff::Timestamp::from_millisecond(at).unwrap().to_string();
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: format!(
                r#"{{"five_hour":{{"utilization":37.5,"resets_at":"{}"}},"seven_day":{{"utilization":61.0,"resets_at":"{}"}}}}"#,
                rfc3339(NOW + 3 * HOUR),
                rfc3339(NOW + 90 * HOUR)
            )
            .into_bytes(),
        },
    );
    // The next bucket, off its 300-second floor, so the reading's own instant tells from the bucket's.
    let taken = NOW + 307_654;
    let bucket = phoebis_ledger::bucket_of(taken);
    assert!(bucket > first && bucket * 1_000 != taken);
    h.clock_cell.store(taken, Ordering::SeqCst);
    let refreshed = json(
        post(
            &h.app,
            "/admin/accounts/business/usage/refresh",
            "",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(refreshed["error"].is_null(), "{refreshed}");
    assert_eq!(refreshed["fetched_at"], taken);
    let by_id = |mut windows: Vec<serde_json::Value>| {
        windows.sort_by_key(|w| w["id"].to_string());
        windows
    };
    let reading = by_id(
        refreshed["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w["key"],
                    "utilization": w["utilization"].as_f64().unwrap() / 100.0,
                    "resets_at": w["resets_at"],
                    "observed_at": taken,
                })
            })
            .collect(),
    );
    assert_eq!(reading.len(), 2, "{refreshed}");
    assert!(
        h.db.quota_samples("business", bucket, bucket + 300)
            .await
            .unwrap()
            .is_empty(),
        "the held writer has not landed the refresh's row"
    );
    let drawn = |account: &serde_json::Value| {
        by_id(
            account["windows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "id": w["id"],
                        "utilization": w["utilization"],
                        "resets_at": w["resets_at"],
                        "observed_at": w["observed_at"],
                    })
                })
                .collect(),
        )
    };
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(drawn(&account), reading, "{account}");
    let listed = json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await;
    let business = listed["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap();
    assert_eq!(drawn(business), reading, "{business}");

    common::free_the_writer(&h).await;
    // The writer frees on the held request batch, which may land ahead of the refresh's quota row.
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while h
        .db
        .quota_samples("business", bucket, bucket + 300)
        .await
        .unwrap()
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < give_up,
            "the freed writer never landed the refresh's row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        drawn(&account),
        reading,
        "the landed row changes nothing: {account}"
    );
}

/// The ledger's rows stand where the router holds no plan window of the account: an api_key account's always, and an
/// OAuth account's while the router holds its fast lane alone. Each window reads its row's own instant, and the
/// bucket's floor only for a row written before the samples carried one.
#[tokio::test]
async fn an_account_the_router_holds_no_plan_window_of_lists_the_ledger_rows_at_their_own_instants()
{
    use common::{get, json, login, wait_json, NOW};
    use phoebis_ledger::QuotaSampleRecord;
    use phoebis_routing::{Report, UtilizationSample, WindowReading, WindowSource};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    h.router
        .report(Report::Sample {
            account_id: "business".into(),
            sample: UtilizationSample::default().with(
                phoebis_relay::FAST_INPUT,
                WindowReading {
                    utilization: 0.3,
                    reset: Some(NOW + 60_000),
                    observed_at: NOW - 30_000,
                    source: WindowSource::Head,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    wait_json(&h.app, "/admin/accounts/business", &cookie, |v| {
        !v["fast_lane"].is_null()
    })
    .await;
    // Off the 300-second floor its bucket files it under.
    let read_at = NOW - 42_123;
    let bucket = phoebis_ledger::bucket_of(read_at);
    assert_ne!(bucket * 1_000, read_at);
    let row = |account: &str, window: &str, utilization: f64, observed_at| QuotaSampleRecord {
        account_id: account.into(),
        bucket,
        window: window.into(),
        utilization: Some(utilization),
        reset_at: None,
        source: "head".into(),
        observed_at,
    };
    h.db.insert_samples(
        &[
            row("business", "five_hour", 0.4, Some(read_at)),
            row("business", "seven_day", 0.2, None),
            row("direct", "five_hour", 0.1, Some(read_at)),
            row("direct", "seven_day", 0.05, None),
        ],
        None,
    )
    .unwrap();
    for (id, five_hour, seven_day) in [("business", 0.4, 0.2), ("direct", 0.1, 0.05)] {
        let account =
            json(get(&h.app, &format!("/admin/accounts/{id}"), Some(&cookie)).await).await;
        let drawn: Vec<(&str, f64, i64)> = account["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["id"].as_str().unwrap(),
                    w["utilization"].as_f64().unwrap(),
                    w["observed_at"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            drawn,
            [
                ("five_hour", five_hour, read_at),
                ("seven_day", seven_day, bucket * 1_000)
            ],
            "{account}"
        );
    }
}

#[tokio::test]
async fn the_usage_get_says_no_credential_is_stored_for_a_disconnected_or_unpaired_account() {
    use common::{delete, get, json, login};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let path = "/admin/accounts/business/usage";
    let answer = |v: &serde_json::Value| {
        (
            v["error"].as_str().map(str::to_owned),
            v["poll"].is_null(),
            v["windows"].as_array().map(Vec::len),
        )
    };
    let unread = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(
        answer(&unread),
        (
            Some("the poll has not read this account yet".into()),
            true,
            Some(0)
        ),
        "a paired account the poll has not read yet"
    );
    h.usage.poll("business").await.unwrap();
    let removed = delete(
        &h.app,
        "/admin/accounts/business/credentials",
        Some(&cookie),
    )
    .await;
    assert_eq!(removed.status(), 204);
    let no_credential = (
        Some("no credential is stored for this account, so pair it to load quota".into()),
        true,
        Some(0),
    );
    let disconnected = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(answer(&disconnected), no_credential, "{disconnected}");
    h.store
        .set_state(
            "business",
            phoebis_store::AccountState::Unpaired,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    let unpaired = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(answer(&unpaired), no_credential, "{unpaired}");
}

#[tokio::test]
async fn the_accounts_list_carries_each_oauth_rows_poll_standing_and_null_for_an_api_key_account() {
    use common::{get, json, login, post, NOW};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let created = post(
        &h.app,
        "/admin/accounts",
        r#"{"id":"spare","kind":"oauth","label":"Spare"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let created = json(created).await;
    let no_credential = serde_json::json!({
        "error": "no credential is stored for this account, so pair it to load quota",
        "fetched_at": null,
        "held_until": null,
        "poll": null,
    });
    assert_eq!(
        created["usage"], no_credential,
        "a write answers the row with its block"
    );
    h.usage.poll("business").await.unwrap();
    let asked = || {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count()
    };
    let polled = asked();

    let listed = json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await;
    let row = |id: &str| {
        listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()["usage"]
            .clone()
    };
    let business = row("business");
    assert_eq!(business["poll"]["last_outcome"], "clean", "{business}");
    assert_eq!(business["fetched_at"], NOW, "{business}");
    assert!(business["error"].is_null(), "{business}");
    // The usage route's own answer is the reference the block is composed to match.
    let usage = json(get(&h.app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert_eq!(
        business,
        serde_json::json!({ "error": usage["error"], "fetched_at": usage["fetched_at"], "held_until": usage["held_until"], "poll": usage["poll"] }),
    );
    assert_eq!(row("spare"), no_credential);
    assert!(
        row("direct").is_null(),
        "the poll never reads an API-key account"
    );
    assert_eq!(
        json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await["usage"],
        business,
        "the single read carries the block the list does"
    );
    assert_eq!(
        asked(),
        polled,
        "the list reads the poll's standing, never the endpoint"
    );
}

#[tokio::test]
async fn an_account_held_by_a_429_lists_its_hold_deadline() {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    // The refresh below reads the endpoint through the poll's path and takes its 429.
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let usage_of = |listed: &serde_json::Value| {
        listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "business")
            .unwrap()["usage"]
            .clone()
    };
    // The clean read comes first: a 429 over windows keeps them, and the list drops the error while they stand, so
    // the deadline is the one thing the hold adds to the block.
    h.usage.poll("business").await.unwrap();
    let clean = usage_of(&json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await);
    assert!(clean["error"].is_null(), "{clean}");
    assert!(
        clean["held_until"].is_null(),
        "no hold, no deadline: {clean}"
    );

    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    h.clock_cell.store(NOW + 10_000, Ordering::SeqCst);
    let refused = json(
        post(
            &h.app,
            "/admin/accounts/business/usage/refresh",
            "",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(refused["poll"]["last_outcome"], "rate_limited", "{refused}");
    let held = usage_of(&json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await);
    assert_eq!(
        held["held_until"],
        NOW + 10_000 + 120_000,
        "the list carries the hold's deadline, the poll's next attempt: {held}"
    );
    assert_eq!(held["held_until"], held["poll"]["next_attempt_at"]);
    assert!(
        held["error"].is_null(),
        "the windows stand, so the block reads no error beside the deadline: {held}"
    );

    h.clock_cell.store(NOW + 10_000 + 120_000, Ordering::SeqCst);
    let past = usage_of(&json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await);
    assert!(
        past["held_until"].is_null(),
        "past the hold the deadline leaves the block: {past}"
    );
    assert_eq!(
        past["poll"]["next_attempt_at"],
        serde_json::Value::Null,
        "{past}"
    );
}

#[tokio::test]
async fn a_profile_429_before_any_reading_answers_no_usage_fetched_yet_beside_the_hold_deadline() {
    use common::{get, json, login, NOW};
    use std::sync::atomic::Ordering;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    // The poll reads the usage first and the profile after it, so a profile 429 ahead of any usage reading comes
    // through the profile read alone, the first thing the account's entry then records.
    assert!(
        h.usage.profile("business").await.is_err(),
        "the first read meets the profile 429"
    );
    let held = json(get(&h.app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert_eq!(held["error"], "no usage fetched yet", "{held}");
    let until = held["held_until"]
        .as_i64()
        .unwrap_or_else(|| panic!("the hold's deadline rides the block: {held}"));
    assert!(until > NOW, "{held}");
    assert_eq!(held["held_until"], held["poll"]["next_attempt_at"]);
    assert!(
        held["windows"].as_array().is_some_and(Vec::is_empty),
        "no reading landed: {held}"
    );

    h.clock_cell.store(until, Ordering::SeqCst);
    let past = json(get(&h.app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert!(
        past["held_until"].is_null(),
        "past the hold the deadline leaves the block: {past}"
    );
    assert_eq!(
        past["poll"]["next_attempt_at"],
        serde_json::Value::Null,
        "{past}"
    );
}

#[tokio::test]
async fn an_account_re_created_inside_a_hold_answers_the_unread_reason_beside_the_deadline() {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    h.usage.poll("business").await.unwrap();
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    h.clock_cell.store(NOW + 10_000, Ordering::SeqCst);
    let refused = json(
        post(
            &h.app,
            "/admin/accounts/business/usage/refresh",
            "",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(refused["poll"]["last_outcome"], "rate_limited", "{refused}");
    // The id is deleted and inserted again behind the routes, so nothing clears the poll's hold on it, and the
    // snapshot the earlier holder left is filtered out.
    let earlier = h.store.account("business").unwrap().unwrap();
    h.store.delete_account("business", "test").unwrap();
    h.store
        .insert_account(&phoebis_store::AccountRow {
            lifecycle_generation: earlier.lifecycle_generation + 1,
            ..earlier
        })
        .unwrap();
    let held = json(get(&h.app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert_eq!(
        (
            &held["error"],
            &held["held_until"],
            &held["fetched_at"],
            &held["identity_generation"],
        ),
        (
            &serde_json::json!("the poll has not read this account yet"),
            &serde_json::json!(NOW + 10_000 + 120_000),
            &serde_json::Value::Null,
            &serde_json::json!(0),
        ),
        "{held}"
    );
    assert_eq!(held["held_until"], held["poll"]["next_attempt_at"]);
}

#[tokio::test]
async fn an_account_whose_snapshot_holds_no_windows_answers_the_windows_the_router_holds() {
    use common::{get, json, login, wait_json, NOW};
    use phoebis_routing::{
        Report, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
    };
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let path = "/admin/accounts/business/usage";
    // What a restart loads from the persisted windows, before the poll has read the account.
    h.router
        .report(Report::Sample {
            account_id: "business".into(),
            sample: UtilizationSample::default()
                .with(
                    FIVE_HOUR,
                    WindowReading {
                        utilization: 0.25,
                        reset: Some(NOW + 3_600_000),
                        observed_at: NOW - 120_000,
                        source: WindowSource::Stored,
                    },
                )
                .with(
                    SEVEN_DAY,
                    WindowReading {
                        utilization: 0.5,
                        reset: Some(NOW + 86_400_000),
                        observed_at: NOW - 60_000,
                        source: WindowSource::Stored,
                    },
                ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    let held = wait_json(&h.app, path, &cookie, |v| v["error"].is_null()).await;
    assert_eq!(
        held["windows"],
        serde_json::json!([
            { "key": "five_hour", "resets_at": NOW + 3_600_000, "utilization": 25.0 },
            { "key": "seven_day", "resets_at": NOW + 86_400_000, "utilization": 50.0 },
        ])
    );
    assert_eq!(
        held["fetched_at"],
        NOW - 60_000,
        "the newest of the readings shown"
    );
    assert!(held["poll"].is_null(), "not yet polled: {held}");

    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let refused = h.usage.poll("business").await.unwrap();
    assert!(refused.windows.is_empty() && refused.error.is_some());
    let polled = json(get(&h.app, path, Some(&cookie)).await).await;
    assert!(
        polled["error"].is_null(),
        "the router's windows still answer: {polled}"
    );
    assert_eq!(polled["windows"], held["windows"]);
    assert_eq!(polled["poll"]["last_outcome"], "rate_limited");
}

/// Windows the router still routes on after the ledger's last day has no sample of them, persisted from the last
/// run and read more than a day ago, are the windows the account view lists, each reading as the router stored it.
#[tokio::test]
async fn an_account_the_ledger_holds_no_sample_of_today_lists_the_windows_the_router_holds() {
    use common::{get, json, login, wait_json, NOW};
    use phoebis_routing::{
        Report, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
        SEVEN_DAY_FABLE,
    };
    const HOUR: i64 = 3_600_000;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    // A pool member, so the account reads its projections under that pool's settings.
    assert_eq!(
        common::post(
            &h.app,
            "/admin/pools",
            r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#,
            Some(&cookie),
        )
        .await
        .status(),
        201
    );
    let read_at = NOW - 30 * HOUR;
    let reading = |utilization, reset| WindowReading {
        utilization,
        reset,
        observed_at: read_at,
        source: WindowSource::Stored,
    };
    h.router
        .report(Report::Sample {
            account_id: "business".into(),
            sample: UtilizationSample::default()
                .with(FIVE_HOUR, reading(0.9, Some(NOW - 25 * HOUR)))
                .with(SEVEN_DAY, reading(0.5, Some(NOW + 72 * HOUR)))
                .with(SEVEN_DAY_FABLE, reading(0.2, Some(NOW + 72 * HOUR))),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    wait_json(&h.app, "/admin/accounts/business/usage", &cookie, |v| {
        v["error"].is_null()
    })
    .await;
    // Each window reads as the router stored it, the 5-hour window's passed reset with it, while the projection reads
    // the router's prediction, which empties that window until the next reading names a reset. No pin burns, so each
    // window projects its prediction at its horizon: five hours on, the weekly windows' resets lying further out. The
    // Fable week reads under the endpoint's key, mapped from the routing name, and its lead paces it as the week the
    // routing name says it is.
    let lead = |window: &str, utilization: f64, reset: Option<i64>| {
        utilization - phoebis_routing::elapsed_share_at(window, reset, None, NOW)
    };
    let held = serde_json::json!([
        { "id": "five_hour", "observed_at": read_at, "resets_at": NOW - 25 * HOUR, "utilization": 0.9,
          "projection": 0.0, "horizon": NOW + 5 * HOUR, "burn": 0.0, "demand": 0.0,
          "lead": lead(FIVE_HOUR, 0.9, Some(NOW - 25 * HOUR)) },
        { "id": "seven_day", "observed_at": read_at, "resets_at": NOW + 72 * HOUR, "utilization": 0.5,
          "projection": 0.5, "horizon": NOW + 5 * HOUR, "burn": 0.0, "demand": 0.0,
          "lead": lead(SEVEN_DAY, 0.5, Some(NOW + 72 * HOUR)) },
        { "id": "fable", "observed_at": read_at, "resets_at": NOW + 72 * HOUR, "utilization": 0.2,
          "projection": 0.2, "horizon": NOW + 5 * HOUR, "burn": 0.0, "demand": 0.0,
          "lead": lead(SEVEN_DAY_FABLE, 0.2, Some(NOW + 72 * HOUR)) },
    ]);
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(account["windows"], held, "{account}");
    let listed = json(get(&h.app, "/admin/accounts", Some(&cookie)).await).await;
    let business = listed["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap();
    assert_eq!(business["windows"], held, "{business}");
}

#[tokio::test]
async fn re_pairing_to_a_different_upstream_account_is_rejected() {
    use common::{delete, get, json, login, post};
    // The identity survives the disconnect, so a later login as somebody else cannot take the slot over.
    let (app, _up) = common::admin_app_paired_as("00000000-0000-4000-8000-0000000000ff").await;
    let cookie = login(&app, "admin-secret").await;
    assert_eq!(
        delete(&app, "/admin/accounts/business/credentials", Some(&cookie))
            .await
            .status(),
        204
    );
    let started = json(
        post(
            &app,
            "/admin/oauth/sessions",
            r#"{"account_id":"business","login":"claude_ai"}"#,
            Some(&cookie),
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
    let cb = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(cb.status(), 200);
    assert_eq!(
        post(
            &app,
            &format!("/admin/oauth/sessions/{id}/confirm"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        409
    );
    let acct = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        acct["identity"]["account_uuid"], "00000000-0000-4000-8000-000000000001",
        "the bound identity is untouched"
    );
    assert!(
        acct["credential"].is_null(),
        "the foreign bundle is discarded"
    );
}

#[tokio::test]
async fn disconnect_bumps_the_lifecycle_and_a_late_confirm_is_rejected() {
    use common::{delete, get, json, login, post};
    let (app, _up) = common::admin_app_paired().await;
    let cookie = login(&app, "admin-secret").await;
    let started = json(
        post(
            &app,
            "/admin/oauth/sessions",
            r#"{"account_id":"business","login":"claude_ai"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    let id = started["id"].as_str().unwrap().to_string();
    assert_eq!(
        delete(&app, "/admin/accounts/business/credentials", Some(&cookie))
            .await
            .status(),
        204
    );
    let acct = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(acct["state"], "disconnected");
    assert!(acct["credential"].is_null());
    assert_eq!(
        post(
            &app,
            &format!("/admin/oauth/sessions/{id}/confirm"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        409,
        "the pairing session was invalidated by the disconnect"
    );
}

/// A disconnect that discards the vault's record while the code is being exchanged leaves the pairing session itself
/// in flight, so the exchange runs and its bundle reaches a record no confirm may take.
#[tokio::test]
async fn an_exchange_landing_on_a_record_a_disconnect_discarded_answers_409_and_keeps_no_bundle() {
    use common::{json, login, post};
    let (app, _up, _store, _router, vault) = common::admin_parts_with_vault().await;
    let cookie = login(&app, "admin-secret").await;
    let started = json(
        post(
            &app,
            "/admin/oauth/sessions",
            r#"{"account_id":"business","login":"claude_ai"}"#,
            Some(&cookie),
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
    assert_eq!(
        vault.discard_for_account("business", common::NOW).len(),
        1,
        "the pairing in flight is the one the disconnect cancels"
    );
    let callback = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(callback.status(), 409);
    assert_eq!(
        json(callback).await["error"],
        "the pairing session was closed while the code was being exchanged"
    );
    assert!(
        vault.take_exchange(&id, common::NOW).is_none(),
        "the discarded record took no bundle"
    );
}

#[tokio::test]
async fn verifying_an_api_key_account_reports_the_upstream_status() {
    use common::{delete, json, login, post, API_KEY};
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let verified = post(&app, "/admin/accounts/direct/verify", "", Some(&cookie)).await;
    assert_eq!(verified.status(), 200);
    assert_eq!(json(verified).await["status"], 200);
    let recorded = up.requests();
    let probe = recorded
        .iter()
        .find(|r| r.line.starts_with("GET /v1/models?limit=1000"))
        .expect("the probe reached the upstream");
    assert!(
        probe
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-api-key") && value == API_KEY),
        "the stored key is what the probe presents"
    );
    let oauth = post(&app, "/admin/accounts/business/verify", "", Some(&cookie)).await;
    assert_eq!(oauth.status(), 400, "an OAuth account has no key to verify");
    assert_eq!(
        json(oauth).await["error"],
        "only an API-key account is verified this way"
    );
    assert_eq!(
        delete(&app, "/admin/accounts/direct/credentials", Some(&cookie))
            .await
            .status(),
        204
    );
    let keyless = post(&app, "/admin/accounts/direct/verify", "", Some(&cookie)).await;
    assert_eq!(keyless.status(), 409, "the slot's key is gone");
    assert_eq!(json(keyless).await["error"], "account holds no API key");
}

#[tokio::test]
async fn a_refused_origin_answers_no_store_and_nosniff_and_a_login_needs_no_origin() {
    use common::{cookie_of, get, json, post, send_with_host};
    const HOST: &str = "phoebis.example";
    let (app, _up) = common::admin_app().await;
    // A forced login is the one mutation an unauthenticated caller can reach, so the gate checks it too.
    let forced = send_with_host(
        &app,
        "POST",
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
        "https://evil.example",
        HOST,
    )
    .await;
    assert_eq!(forced.status(), 403);
    assert!(
        forced.headers().get("set-cookie").is_none(),
        "a refused login sets no cookie"
    );
    assert_eq!(json(forced).await["error"], "origin is not allowed");
    let login = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    assert_eq!(
        login.status(),
        204,
        "a login with no Origin is the operator's own hand"
    );
    let cookie = cookie_of(&login);
    let pause = "/admin/accounts/business/pause";
    let forged = send_with_host(
        &app,
        "POST",
        pause,
        "",
        Some(&cookie),
        "https://evil.example",
        HOST,
    )
    .await;
    assert_eq!(forged.status(), 403);
    assert_eq!(
        forged.headers().get("cache-control").unwrap(),
        "no-store",
        "a refusal is no more cacheable than an answer"
    );
    assert_eq!(
        forged.headers().get("x-content-type-options").unwrap(),
        "nosniff",
        "and no more sniffable"
    );
    assert_eq!(
        send_with_host(
            &app,
            "POST",
            pause,
            "",
            Some(&cookie),
            "https://phoebis.example",
            HOST
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        get(&app, "/admin/accounts", Some(&cookie)).await.status(),
        200,
        "a read carries no Origin and needs none"
    );
}

#[tokio::test]
async fn an_uncertain_refresh_shows_as_quarantined_with_its_detail() {
    use common::{get, json, login};
    let (app, _up) =
        common::admin_app_quarantined("refresh response lost: connection closed").await;
    let cookie = login(&app, "admin-secret").await;
    let acct = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(acct["credential"]["quarantined"], true);
    assert_eq!(
        acct["credential"]["last_error"], "refresh response lost: connection closed",
        "an uncertain refresh says why, not just that it happened"
    );
}

#[tokio::test]
async fn a_failed_retry_beside_an_uncertain_refresh_still_shows_as_quarantined() {
    use common::{get, json, login};
    let (app, _up, store, _manager) =
        common::quarantined_parts("refresh response lost: connection closed").await;
    // Past the manager's floor one retry opens beside the uncertain attempt, and a retry that fails settles
    // nothing: the uncertain attempt is still the one a restart quarantines the account for.
    let retry = store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    store
        .resolve_refresh_attempt(
            retry,
            1,
            phoebis_store::AttemptOutcome::Failed("refresh rejected with status 503".into()),
        )
        .unwrap();
    let cookie = login(&app, "admin-secret").await;
    let acct = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(acct["credential"]["quarantined"], true);
}

#[tokio::test]
async fn a_confirmed_pairing_supersedes_the_attempt_that_quarantined_the_slot() {
    use common::{get, json, login, post, NOW};
    // No disconnect anywhere in this test: the operator pairs the quarantined slot again, which is the one
    // way out that used to leave the uncertain attempt blocking every later refresh.
    let (app, up, store, manager) =
        common::quarantined_parts("refresh response lost: connection closed").await;
    let cookie = login(&app, "admin-secret").await;
    let started = post(
        &app,
        "/admin/oauth/sessions",
        r#"{"account_id":"business","login":"claude_ai"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), 201);
    let started = json(started).await;
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
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(callback.status(), 200);
    assert_eq!(
        post(
            &app,
            &format!("/admin/oauth/sessions/{id}/confirm"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        200
    );

    let account = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(account["state"], "ready");
    assert_eq!(account["credential"]["quarantined"], false);

    // Inside its margin, so the acquire below can only answer by running a refresh.
    let generation = store.credentials("business").unwrap().unwrap().generation;
    store
        .commit_credentials(
            "business",
            1,
            1,
            Some(generation),
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new("expiring"),
                refresh_token: Some(phoebis_store::Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: NOW - 3_600_000,
                expires_at: NOW + 1_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    // The confirm's own poll acquired the paired bundle and the manager cached it. The commit above went around
    // the manager, so the cache is dropped as a route's commit drops it, and the acquire reads the expiring bundle.
    manager.invalidate("business");
    let refreshes = || {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("POST /v1/oauth/token"))
            .count()
    };
    let before = refreshes();
    let token = manager
        .acquire("business")
        .await
        .expect("the confirmed grant cleared the attempt that was blocking refreshes");
    assert_eq!(token.secret.expose(), "sk-ant-oat01-PLACEHOLDER");
    assert_eq!(
        refreshes(),
        before + 1,
        "the acquire ran a refresh rather than answering from a cache"
    );
    assert!(
        manager.recover_on_start().unwrap().is_empty(),
        "a restart has nothing left to quarantine"
    );
}

#[tokio::test]
async fn a_rejected_token_exchange_ends_the_session_and_frees_the_account() {
    use common::{get, json, login, post};
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    up.set_rule_after(
        0,
        "/v1/oauth/token",
        fake_upstream::Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant"}"#.to_vec(),
        },
    );
    let start = r#"{"account_id":"business","login":"claude_ai"}"#;
    let started = json(post(&app, "/admin/oauth/sessions", start, Some(&cookie)).await).await;
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
        &app,
        &format!("/admin/oauth/sessions/{id}/callback"),
        &format!(r#"{{"url":"{redirect}?code=CODE123&state={state}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(callback.status(), 502);
    let refusal = json(callback).await;
    let refusal = refusal["error"].as_str().unwrap().to_string();
    assert!(
        refusal.contains("400"),
        "the operator is told what the endpoint answered: {refusal}"
    );

    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(shown["status"], "failed");
    // The owner's own start would replace a pairing that still held the slot, so only another admin
    // session's start shows the slot was handed back.
    let other = login(&app, "admin-secret").await;
    assert_eq!(
        post(&app, "/admin/oauth/sessions", start, Some(&other))
            .await
            .status(),
        201,
        "the spent code does not hold the account for another ten minutes"
    );
}

#[tokio::test]
async fn a_second_confirmation_is_refused_without_undoing_the_first() {
    use common::{get, json, login, post};
    // What a double-clicked confirm button sends. The loser must not empty the vault or reopen the session
    // under the winner.
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for(&app, &cookie, "business").await;
    let confirm = format!("/admin/oauth/sessions/{id}/confirm");

    assert_eq!(post(&app, &confirm, "", Some(&cookie)).await.status(), 200);
    assert_eq!(post(&app, &confirm, "", Some(&cookie)).await.status(), 409);

    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(
        shown["status"], "confirmed",
        "the refused confirmation left the session it lost to alone"
    );
    let account = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(account["state"], "ready");
    assert_eq!(account["credential"]["quarantined"], false);
}

#[tokio::test]
async fn a_confirmation_from_another_admin_session_changes_nothing() {
    use common::{get, json, login, post};
    let (app, _up) = common::admin_app().await;
    let owner = login(&app, "admin-secret").await;
    let other = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for(&app, &owner, "business").await;

    let refused = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&other),
    )
    .await;
    assert_eq!(
        refused.status(),
        404,
        "a pairing session id is an unguessable handle, so a non-owner is not told it exists"
    );

    assert_eq!(
        post(
            &app,
            &format!("/admin/oauth/sessions/{id}/confirm"),
            "",
            Some(&owner)
        )
        .await
        .status(),
        200,
        "the owner's confirmation still has its session and its credential"
    );
    let account = json(get(&app, "/admin/accounts/business", Some(&owner)).await).await;
    assert_eq!(account["state"], "ready");
}

#[tokio::test]
async fn an_abandoned_exchange_is_swept_at_its_deadline() {
    use common::login;
    let (app, _up, _store, _router, vault) = common::admin_parts_with_vault().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for(&app, &cookie, "business").await;
    assert_eq!(
        vault.sweep(common::NOW + 600_000),
        0,
        "the deadline has not passed"
    );
    assert_eq!(vault.sweep(common::NOW + 600_001), 1);
    assert!(
        vault.take_exchange(&id, common::NOW + 600_001).is_none(),
        "the bundle is gone"
    );
}

#[tokio::test]
async fn a_confirmed_pairing_leaves_the_vault_ten_minutes_after_its_confirm() {
    use common::{get, login, post};
    use std::sync::atomic::Ordering;
    let parts = common::admin_parts_sweepable().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let id = common::exchanged_session_for(&parts.app, &cookie, "business").await;
    assert_eq!(
        post(
            &parts.app,
            &format!("/admin/oauth/sessions/{id}/confirm"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    let path = format!("/admin/oauth/sessions/{id}");
    let inside = common::NOW + 600_000 - 1;
    parts.clock_cell.store(inside, Ordering::SeqCst);
    assert_eq!(
        (parts.vault.sweep(inside), parts.pairing.sweep(inside)),
        (0, 0),
        "inside its ten minutes the ended pairing stays readable"
    );
    assert_eq!(get(&parts.app, &path, Some(&cookie)).await.status(), 200);
    let at = common::NOW + 600_000;
    parts.clock_cell.store(at, Ordering::SeqCst);
    assert_eq!(
        parts.vault.sweep(at),
        1,
        "the confirmed record leaves the vault at its ten minutes exactly"
    );
    assert_eq!(
        parts.pairing.sweep(at),
        1,
        "and the ended session leaves the pairings"
    );
    assert_eq!(get(&parts.app, &path, Some(&cookie)).await.status(), 404);
    let after = common::NOW + 11 * 60_000;
    assert_eq!(
        (parts.vault.sweep(after), parts.pairing.sweep(after)),
        (0, 0)
    );
}

/// An ended pairing's lifetime runs from its first end: a repeated owner `DELETE` answers `409` and never extends
/// how long the session and its vault record are kept.
#[tokio::test]
async fn a_repeated_delete_of_an_ended_pairing_keeps_its_first_end() {
    use common::{delete, json, login};
    use std::sync::atomic::Ordering;
    let parts = common::admin_parts_sweepable().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let id = common::exchanged_session_for(&parts.app, &cookie, "business").await;
    let path = format!("/admin/oauth/sessions/{id}");
    assert_eq!(delete(&parts.app, &path, Some(&cookie)).await.status(), 204);
    parts
        .clock_cell
        .store(common::NOW + 300_000, Ordering::SeqCst);
    let again = delete(&parts.app, &path, Some(&cookie)).await;
    assert_eq!(again.status(), 409);
    assert_eq!(
        json(again).await["error"],
        "pairing session already consumed"
    );
    let at = common::NOW + 600_000;
    assert_eq!(
        (parts.vault.sweep(at), parts.pairing.sweep(at)),
        (1, 1),
        "both leave ten minutes after the first DELETE"
    );
}

#[test]
fn a_record_discarded_twice_leaves_one_lifetime_after_its_first_discard() {
    let vault = phoebis_admin::PairingVault::new();
    vault.open(
        "pairing",
        "business",
        "owner",
        0,
        common::NOW + 600_000,
        common::NOW,
        None,
    );
    vault.discard("pairing", common::NOW);
    // A callback whose exchange returns after the owner cancelled discards the record a second time.
    vault.discard("pairing", common::NOW + 300_000);
    assert_eq!(vault.sweep(common::NOW + 600_000 - 1), 0);
    assert_eq!(
        vault.sweep(common::NOW + 600_000),
        1,
        "the record leaves ten minutes after its first discard"
    );
}

#[tokio::test]
async fn a_second_start_by_the_owning_session_replaces_its_standing_pairing() {
    use common::{get, json, login, post};
    // What a reload or a closed tab leaves behind: the same cookie starts again while its first pairing,
    // already holding an exchanged credential, still stands.
    let (app, _up, _store, _router, vault) = common::admin_parts_with_vault().await;
    let cookie = login(&app, "admin-secret").await;
    let first = common::exchanged_session_for(&app, &cookie, "business").await;

    let start = r#"{"account_id":"business","login":"claude_ai"}"#;
    let replaced = post(&app, "/admin/oauth/sessions", start, Some(&cookie)).await;
    assert_eq!(replaced.status(), 201);
    let second = json(replaced).await["id"].as_str().unwrap().to_string();
    assert_ne!(second, first);

    let shown = json(
        get(
            &app,
            &format!("/admin/oauth/sessions/{first}"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(shown["status"], "cancelled");
    let shown = json(
        get(
            &app,
            &format!("/admin/oauth/sessions/{second}"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(shown["status"], "pending");
    assert!(
        vault.take_exchange(&first, common::NOW).is_none(),
        "the replaced pairing's credential is dropped at once, not left for the sweep"
    );
}

#[tokio::test]
async fn a_start_by_another_admin_session_answers_409() {
    use common::{get, json, login, post};
    let (app, _up) = common::admin_app().await;
    let owner = login(&app, "admin-secret").await;
    let other = login(&app, "admin-secret").await;
    let start = r#"{"account_id":"business","login":"claude_ai"}"#;
    let started = json(post(&app, "/admin/oauth/sessions", start, Some(&owner)).await).await;
    let id = started["id"].as_str().unwrap().to_string();

    let refused = post(&app, "/admin/oauth/sessions", start, Some(&other)).await;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        json(refused).await["error"],
        "a pairing session is already in progress for this account"
    );
    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&owner)).await).await;
    assert_eq!(
        shown["status"], "pending",
        "the refused start left the owner's pairing alone"
    );
}

#[tokio::test]
async fn a_start_with_new_confirms_into_a_ready_row_with_its_identity_plan_and_start() {
    use common::{get, json, login, post};
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let mut profile: serde_json::Value =
        serde_json::from_slice(phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Profile,
        ))
        .unwrap();
    profile["account"]["has_claude_max"] = serde_json::json!(true);
    profile["organization"]["subscription_created_at"] = serde_json::json!("2026-03-14T10:00:00Z");
    profile["organization"]["claude_code_trial_ends_at"] =
        serde_json::json!("2026-10-02T00:00:00+00:00");
    up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&profile).unwrap(),
        },
    );
    let id = common::exchanged_session_for_new(&app, &cookie, "fresh", "Fresh").await;
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 200);
    assert_eq!(
        json(confirmed).await,
        serde_json::json!({ "account_id": "fresh", "status": "confirmed", "first_poll_in_flight": true })
    );
    let acct = json(get(&app, "/admin/accounts/fresh", Some(&cookie)).await).await;
    assert_eq!(
        (
            acct["state"].as_str(),
            acct["kind"].as_str(),
            acct["label"].as_str()
        ),
        (Some("ready"), Some("oauth"), Some("Fresh"))
    );
    assert_eq!(acct["identity"]["account_uuid"], common::ACCOUNT_UUID);
    assert!(acct["credential"]["access_expires_at"].is_number());
    assert_eq!(
        acct["subscription"],
        serde_json::json!({
            "plan": "max",
            "tier": "default_claude_max_5x",
            "since": 1_773_482_400_000_i64,
            "trial_ends_at": 1_790_899_200_000_i64,
            "ends_at": null,
        }),
        "the one write stored the plan and the start with the row"
    );
    let listed = json(get(&app, "/admin/accounts", Some(&cookie)).await).await;
    let fresh = listed["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["id"] == "fresh")
        .count();
    assert_eq!(fresh, 1, "the list carries the row once");
}

#[tokio::test]
async fn a_cancel_after_a_start_with_new_leaves_no_row() {
    use common::{delete, get, json, login, post};
    let (app, _up) = common::admin_app().await;
    let owner = login(&app, "admin-secret").await;
    let start = r#"{"new":{"id":"fresh","label":"Fresh"},"login":"claude_ai"}"#;
    let started = post(&app, "/admin/oauth/sessions", start, Some(&owner)).await;
    assert_eq!(started.status(), 201);
    let id = json(started).await["id"].as_str().unwrap().to_string();
    assert_eq!(
        delete(&app, &format!("/admin/oauth/sessions/{id}"), Some(&owner))
            .await
            .status(),
        204
    );
    assert_eq!(
        get(&app, "/admin/accounts/fresh", Some(&owner))
            .await
            .status(),
        404
    );
    // The owner's own start would supersede a standing session whatever the cancel did. Another admin session's
    // start is refused while one stands, so its 201 is what proves the id was freed.
    let other = login(&app, "admin-secret").await;
    assert_eq!(
        post(&app, "/admin/oauth/sessions", start, Some(&other))
            .await
            .status(),
        201
    );
}

#[tokio::test]
async fn an_expiry_after_a_start_with_new_leaves_no_row() {
    use common::{get, json, login, post};
    use std::sync::atomic::Ordering;
    let parts = common::admin_parts_sweepable().await;
    let owner = login(&parts.app, "admin-secret").await;
    let start = r#"{"new":{"id":"fresh","label":"Fresh"},"login":"claude_ai"}"#;
    let started = post(&parts.app, "/admin/oauth/sessions", start, Some(&owner)).await;
    assert_eq!(started.status(), 201);
    let deadline = json(started).await["expires_at"].as_i64().unwrap();
    let at = deadline + 1;
    parts.clock_cell.store(at, Ordering::SeqCst);
    assert_eq!(
        parts.vault.sweep(at),
        1,
        "the vault record left at the deadline"
    );
    parts.pairing.sweep(at);
    assert_eq!(
        get(&parts.app, "/admin/accounts/fresh", Some(&owner))
            .await
            .status(),
        404
    );
    let other = login(&parts.app, "admin-secret").await;
    assert_eq!(
        post(&parts.app, "/admin/oauth/sessions", start, Some(&other))
            .await
            .status(),
        201,
        "the expired pairing holds the id no longer"
    );
}

#[tokio::test]
async fn a_confirm_whose_id_a_create_took_meanwhile_answers_409_and_leaves_the_row_untouched() {
    use common::{get, json, login, post};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for_new(&app, &cookie, "fresh", "Fresh").await;
    let created = post(
        &app,
        "/admin/accounts",
        r#"{"id":"fresh","kind":"oauth","label":"Taken"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 409);
    assert_eq!(
        json(confirmed).await["error"],
        "an account with this id exists"
    );
    let acct = json(get(&app, "/admin/accounts/fresh", Some(&cookie)).await).await;
    assert_eq!(
        (acct["label"].as_str(), acct["state"].as_str()),
        (Some("Taken"), Some("unpaired")),
        "the row belongs to the create that won"
    );
    assert!(
        acct["credential"]["access_expires_at"].is_null(),
        "no credential reached the winner's row"
    );
    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(shown["status"], "failed");
}

#[tokio::test]
async fn a_start_with_new_for_an_existing_id_answers_409() {
    use common::{json, login, post};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let refused = post(
        &app,
        "/admin/oauth/sessions",
        r#"{"new":{"id":"business","label":"Business"}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        json(refused).await["error"],
        "an account with this id exists"
    );
}

#[tokio::test]
async fn a_start_with_both_members_or_neither_answers_400() {
    use common::{json, login, post};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    for (body, error) in [
        (
            r#"{"account_id":"business","new":{"id":"fresh","label":"Fresh"}}"#,
            "body must carry an account_id or a new account",
        ),
        (
            r#"{"login":"claude_ai"}"#,
            "body must carry an account_id or a new account",
        ),
        (
            r#"{"new":{"id":"Bad Id","label":"x"}}"#,
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        ),
        (
            r#"{"new":{"id":"ok","label":""}}"#,
            "a new account needs a label",
        ),
    ] {
        let refused = post(&app, "/admin/oauth/sessions", body, Some(&cookie)).await;
        assert_eq!(refused.status(), 400, "{body}");
        assert_eq!(json(refused).await["error"], error, "{body}");
    }
}

#[tokio::test]
async fn a_start_with_new_whose_identity_another_slot_holds_leaves_no_row() {
    use common::{get, json, login, post};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    assert_eq!(pair_business(&app, &cookie).await.status(), 200);
    let id = common::exchanged_session_for_new(&app, &cookie, "second", "Second").await;
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 409);
    assert_eq!(
        json(confirmed).await["error"],
        "that upstream account is already paired to another slot"
    );
    assert_eq!(
        get(&app, "/admin/accounts/second", Some(&cookie))
            .await
            .status(),
        404,
        "the refused confirm left no row"
    );
    let business = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(business["state"], "ready");
}

#[tokio::test]
async fn a_confirm_whose_create_the_store_refuses_answers_500_and_leaves_no_row() {
    use common::{get, json, login, post};
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for_new(&app, &cookie, "fresh", "Fresh").await;
    store.debug_fail_next_credential_commit();
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 500);
    assert_eq!(json(confirmed).await["error"], "storage failure");
    assert_eq!(
        get(&app, "/admin/accounts/fresh", Some(&cookie))
            .await
            .status(),
        404,
        "the refused create left no row"
    );
    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(shown["status"], "failed");
}

#[tokio::test]
async fn the_session_route_answers_the_session_id_and_settings_say_the_admin_secret_is_configured()
{
    use common::{cookie_of, delete, get, json, post};
    let (app, _up) = common::admin_app().await;
    let anonymous = get(&app, "/admin/session", None).await;
    assert_eq!(anonymous.status(), 401);
    assert_eq!(anonymous.headers()["cache-control"], "no-store");
    let admin = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    let cookie = cookie_of(&admin);
    let me = json(get(&app, "/admin/session", Some(&cookie)).await).await;
    assert_eq!(
        me.as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["session_id".to_string()]),
        "the session answer names the session and nothing else: {me}"
    );
    assert!(
        me["session_id"].as_str().is_some_and(|id| id != cookie),
        "the cookie value stays in the store"
    );
    let settings = json(get(&app, "/admin/settings", Some(&cookie)).await).await;
    assert_eq!(
        settings["access"],
        serde_json::json!({ "admin_configured": true })
    );
    assert_eq!(
        settings["about"]["profile"].as_str(),
        Some("claude-code-2.1.269-linux-x64")
    );
    assert!(settings["compatibility"].is_null());
    assert!(
        !settings.to_string().contains("secret"),
        "no secret and no variable name in the settings answer"
    );
    assert_eq!(
        delete(&app, "/admin/session", Some(&cookie)).await.status(),
        204
    );
    assert_eq!(
        get(&app, "/admin/session", Some(&cookie)).await.status(),
        401
    );
}

/// A pin burning `plain` Opus input tokens a second, and `banded` in the copy weighed by the peak band.
fn burning_opus_pin(
    session: &str,
    account: &str,
    key_generation: i64,
    now: i64,
    plain: f64,
    banded: f64,
) -> phoebis_store::PinRow {
    let input = |input: f64| phoebis_store::PinBurn {
        input,
        ..phoebis_store::PinBurn::default()
    };
    phoebis_store::PinRow {
        key_id: "desktop".into(),
        session_id: session.into(),
        agent_id: "main".into(),
        model: "claude-opus-5".into(),
        account_id: account.into(),
        epoch: 1,
        created_at: now - 10,
        last_used_at: now - 5,
        key_generation,
        incarnation: 1,
        burn: input(plain),
        // At the rig's fixed clock, so a read decays nothing and the figures below are the fold's own.
        burn_at: Some(now),
        burn_banded: input(banded),
        burn_fast: phoebis_store::PinBurn::default(),
        burn_unmetered: phoebis_store::PinBurn::default(),
        last_input_tokens: 1_000,
        last_classifier: false,
        last_refresh_at: now - 5,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        shed_reason: None,
        static_hash: None,
        tail_hash: None,
        lane: phoebis_routing::lane_key("claude-opus-5", "standard", "none", "-"),
        entrypoint: String::new(),
        marked_at: None,
        marked_on: None,
        marked_window: None,
    }
}

/// Stores a profile naming `tier` for `account`, as the profile fetch would.
fn name_the_tier(store: &phoebis_store::Store, account: &str, tier: &str) {
    store
        .set_profile(
            account,
            "{}",
            &phoebis_store::SubscriptionRow {
                plan: None,
                tier: Some(tier.into()),
                since: None,
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// The pool `main` over `business` (subscription, Max 20x) and `direct` (api_key) behind key `desktop`, seeded
/// through one harness with `pins` and handed to a second one: the router task loads its pins at spawn.
async fn sessions_harness_with(pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>) -> common::Full {
    sessions_harness_priced(phoebis_ledger::PriceRevisions::shipped(), pins).await
}

/// `sessions_harness_with` pricing dollars through `prices` rather than the shipped table.
async fn sessions_harness_priced(
    prices: phoebis_ledger::PriceRevisions,
    pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>,
) -> common::Full {
    use common::{generation, login, post};
    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&seeding)).await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    name_the_tier(&seed.store, "business", "default_claude_max_20x");
    for pin in pins(generation(&seed.store, "desktop")) {
        seed.store.upsert_pin(&pin).unwrap();
    }
    common::admin_parts_full_on_priced(seed.store.clone(), prices).await
}

fn session_row(sessions: &serde_json::Value, session: &str) -> serde_json::Value {
    sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == session)
        .unwrap_or_else(|| panic!("no row for {session}: {sessions}"))
        .clone()
}

/// A row's `(burn, burn_unit)`.
fn burn_of(sessions: &serde_json::Value, session: &str) -> (Option<f64>, Option<String>) {
    let row = session_row(sessions, session);
    (
        row["burn"].as_f64(),
        row["burn_unit"].as_str().map(String::from),
    )
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-12)
}

#[tokio::test]
async fn a_session_row_carries_the_burn_in_the_unit_the_accounts_funding_selects() {
    use common::{get, json, login, wait_json};
    let now = common::NOW;
    // One hundred input tokens a second plain and 145 banded: the share reads the banded copy, dollars the
    // plain one. The subscription pin burns Opus 5.5, which the cost model weighs; the api_key pin Opus 5.
    let parts = sessions_harness_with(|generation| {
        vec![
            phoebis_store::PinRow {
                model: "claude-opus-5-5".into(),
                ..burning_opus_pin("s1", "business", generation, now, 100.0, 145.0)
            },
            burning_opus_pin("s2", "direct", generation, now, 100.0, 145.0),
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = |sessions: &serde_json::Value, session: &str| session_row(sessions, session);
    // The banded copy priced at the fixed clock, which the peak band never covers, so the shipped model's plain
    // Opus 5.5 share on Max 20x; the plain copy at the shipped Opus 5 input price.
    let share_per_hour = phoebis_ledger::PlanTables::shipped()
        .share_at(
            phoebis_routing::FIVE_HOUR,
            None,
            "default_claude_max_20x",
            "claude-opus-5-5",
            phoebis_routing::PinUsage {
                input: 145,
                ..phoebis_routing::PinUsage::default()
            },
            now,
        )
        .unwrap()
        * 3_600.0;
    let opus_5_input = phoebis_ledger::PriceRevisions::shipped()
        .in_force(now)
        .price("claude-opus-5")
        .unwrap()
        .input;
    let usd_per_hour = 100.0 * opus_5_input / 1_000_000.0 * 3_600.0;
    let (burn, unit) = burn_of(&sessions, "s1");
    assert!(
        close(burn, share_per_hour) && unit.as_deref() == Some("window_share"),
        "a subscription account on its plan reads the banded copy as its share per hour: {:?}",
        (burn, unit)
    );
    let (burn, unit) = burn_of(&sessions, "s2");
    assert!(
        close(burn, usd_per_hour) && unit.as_deref() == Some("usd"),
        "an api_key pin always reads the plain copy in dollars per hour: {:?}",
        (burn, unit)
    );
    // The shipped table lists Opus 5.5, so its dollar reading is that row's own rate, read as the route reads it.
    let s1_usd_per_hour = phoebis_ledger::PriceRevisions::shipped()
        .in_force(now)
        .rate_table()
        .price_burn(
            "claude-opus-5-5",
            &phoebis_store::PinBurn {
                input: 100.0,
                ..phoebis_store::PinBurn::default()
            },
            1_000,
        )
        .unwrap()
        * 3_600.0;
    assert_eq!(
        row(&sessions, "s1")["burn_at"].as_i64(),
        Some(now),
        "the instant the served burn is decayed to, the read's own"
    );
    let equivalent_per_hour = phoebis_ledger::PlanTables::shipped()
        .equivalent_at(
            phoebis_routing::FIVE_HOUR,
            None,
            "claude-opus-5-5",
            phoebis_routing::PinUsage {
                input: 145,
                ..phoebis_routing::PinUsage::default()
            },
            now,
        )
        .unwrap()
        * 3_600.0;
    let (s1, s2) = (row(&sessions, "s1"), row(&sessions, "s2"));
    assert!(
        close(s1["burn_equivalent"].as_f64(), equivalent_per_hour) && s1["estimated"] == false,
        "the plan pin's banded copy as its 20x 5-hour equivalent per hour, on a weighted model: {s1}"
    );
    assert_eq!(
        (s2.get("burn_equivalent"), &s2["estimated"]),
        (Some(&serde_json::Value::Null), &serde_json::json!(false)),
        "a pin billed in dollars has no window equivalent to mark, whatever its model: {s2}"
    );
    // Pushed into overage with a reset ahead of now: the same pin reads the dollar average instead.
    parts
        .router
        .report(phoebis_routing::Report::Overage {
            account_id: "business".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: Some(now - 3_600_000),
                reset: Some(now + 3_600_000),
                observed_at: now,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    let overage_sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["burn_unit"] == "usd")
    })
    .await;
    assert!(
        close(row(&overage_sessions, "s1")["burn"].as_f64(), s1_usd_per_hour),
        "overage evidence whose reset is ahead moves a subscription account's reading to dollars: {}",
        row(&overage_sessions, "s1")
    );
    assert_eq!(
        row(&overage_sessions, "s1").get("burn_equivalent"),
        Some(&serde_json::Value::Null),
        "and its window equivalent goes with the plan reading"
    );
    // A reset that has already passed no longer counts as overage: a raw `in_use` read would still answer
    // `usd` here, since the flag itself never cleared. A later `observed_at` than the ahead-of-now report
    // above, or the router's own newest-wins rule drops this one on the floor.
    parts
        .router
        .report(phoebis_routing::Report::Overage {
            account_id: "business".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: Some(now - 3_600_000),
                reset: Some(now - 1),
                observed_at: now + 1,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    let recovered_sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["burn_unit"] == "window_share")
    })
    .await;
    assert!(
        close(
            row(&recovered_sessions, "s1")["burn"].as_f64(),
            share_per_hour
        ),
        "a passed reset reads as not in overage, so the reading returns to the share: {}",
        row(&recovered_sessions, "s1")
    );
}

#[tokio::test]
async fn a_session_row_prices_the_burn_at_the_set_the_pins_last_prompt_selects() {
    use common::{get, json, login, patch, wait_json};
    use phoebis_ledger::Speed;
    let now = common::NOW;
    // One Haiku 5.5 row with both blocks in round numbers: the standard cells at 1, the fast cells at 2 and the
    // long-context cells at 5 over 1,000 prompt tokens.
    let revisions = phoebis_ledger::PriceRevisions::from_yaml(
        "revisions:\n  - effective: 2026-01-01\n    models:\n      claude-haiku-5-5: {input: 1, cache_write_5m: 1, \
         cache_write_1h: 1, cache_read: 1, output: 1, \
         fast: {input: 2, cache_write_5m: 2, cache_write_1h: 2, cache_read: 2, output: 2}, \
         long_context: {over: 1000, input: 5, cache_write_5m: 5, cache_write_1h: 5, cache_read: 5, output: 5}}\n",
        "test",
    )
    .unwrap();
    let input = |speed, prompt| {
        revisions
            .in_force(now)
            .price_at("claude-haiku-5-5", speed, prompt)
            .unwrap()
            .input
    };
    // Three api_key pins burning 100 input tokens a second: `s-at` with its last prompt at the threshold,
    // `s-over` one token over it, and `s-fast` over it, which then answers a fast turn and a standard turn below
    // the threshold through the router. Its copies were never folded, so nothing decays at those turns.
    let parts = sessions_harness_priced(revisions.clone(), |generation| {
        let haiku = |session: &str, last_input_tokens: u64| phoebis_store::PinRow {
            model: "claude-haiku-5-5".into(),
            last_input_tokens,
            ..burning_opus_pin(session, "direct", generation, now, 100.0, 100.0)
        };
        vec![
            haiku("s-at", 1_000),
            haiku("s-over", 1_001),
            phoebis_store::PinRow {
                burn_at: None,
                ..haiku("s-fast", 1_001)
            },
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let averaged = patch(
        &parts.app,
        "/admin/pools/main",
        r#"{"burn_average_seconds":600}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(averaged.status(), 200);
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let usd_per_hour =
        |tokens: f64, speed, prompt| tokens * input(speed, prompt) / 1_000_000.0 * 3_600.0;
    for (session, prompt) in [("s-at", 1_000), ("s-over", 1_001)] {
        let (burn, unit) = burn_of(&sessions, session);
        assert!(
            close(burn, usd_per_hour(100.0, Speed::Standard, prompt))
                && unit.as_deref() == Some("usd"),
            "{session} reads its burn per hour at the cells its last prompt selects: {:?} against {}",
            (burn, unit),
            usd_per_hour(100.0, Speed::Standard, prompt)
        );
        let row = session_row(&sessions, session);
        assert_eq!(
            (row.get("burn_equivalent"), &row["estimated"]),
            (Some(&serde_json::Value::Null), &serde_json::json!(false)),
            "{session}: a pin billed in dollars reads no window equivalent: {row}"
        );
    }
    // 60,000 fast input tokens over the pool's 600 s burn average are 100 a second at the fast cells, whatever
    // the prompt. The standard turn's 600 tokens are one a second beside the seeded hundred, and its prompt of
    // 600 moves the pin's ordinary copy to the standard cells.
    let renewal = |fast: bool, input: u64| phoebis_routing::Report::PinRenewed {
        pin: phoebis_routing::PinRef {
            key_id: "desktop".into(),
            session_id: "s-fast".into(),
            agent_id: "main".into(),
            model: "claude-haiku-5-5".into(),
            epoch: 1,
            lane: phoebis_routing::lane_key("claude-opus-5", "standard", "none", "-"),
            entrypoint: String::new(),
        },
        now,
        usage: Some(phoebis_routing::PinUsage {
            input,
            ..phoebis_routing::PinUsage::default()
        }),
        origin: "client",
        cache_ttl_ms: 300_000,
        fast,
        classifier: false,
    };
    parts.router.report(renewal(true, 60_000)).await;
    parts.router.report(renewal(false, 600)).await;
    let expected =
        usd_per_hour(101.0, Speed::Standard, 600) + usd_per_hour(100.0, Speed::Fast, 600);
    let renewed = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        close(session_row(v, "s-fast")["burn"].as_f64(), expected)
    })
    .await;
    assert_eq!(
        burn_of(&renewed, "s-fast").1.as_deref(),
        Some("usd"),
        "the fast tokens read at the fast cells and the ordinary ones at the set the last prompt selects, summed \
         in dollars"
    );
}

#[tokio::test]
async fn a_pinned_sessions_burn_reads_its_share_rate_on_the_account_it_sits_on() {
    use common::{login, patch, wait_json};
    let now = common::NOW;
    let parts = sessions_harness_with(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5-5".into(),
            ..burning_opus_pin("s1", "business", generation, now, 0.0, 0.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let averaged = patch(
        &parts.app,
        "/admin/pools/main",
        r#"{"burn_average_seconds":600}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(averaged.status(), 200);
    // A million Opus 5.5 input tokens at the fixed clock, which the peak band never covers, folded over the
    // pool's 600-second burn average.
    parts
        .router
        .report(phoebis_routing::Report::PinRenewed {
            pin: phoebis_routing::PinRef {
                key_id: "desktop".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: "claude-opus-5-5".into(),
                epoch: 1,
                lane: phoebis_routing::lane_key("claude-opus-5", "standard", "none", "-"),
                entrypoint: String::new(),
            },
            now,
            usage: Some(phoebis_routing::PinUsage {
                input: 1_000_000,
                ..phoebis_routing::PinUsage::default()
            }),
            origin: "client",
            cache_ttl_ms: 300_000,
            fast: false,
            classifier: false,
        })
        .await;
    let window = 600.0;
    let tables = phoebis_ledger::PlanTables::shipped();
    let million = phoebis_routing::PinUsage {
        input: 1_000_000,
        ..phoebis_routing::PinUsage::default()
    };
    let on_max_20x = tables
        .share_at(
            phoebis_routing::FIVE_HOUR,
            None,
            "default_claude_max_20x",
            "claude-opus-5-5",
            million,
            now,
        )
        .unwrap()
        * 3_600.0
        / window;
    let renewed = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        session_row(v, "s1")["burn"]
            .as_f64()
            .is_some_and(|b| b > 0.0)
    })
    .await;
    let (burn, unit) = burn_of(&renewed, "s1");
    assert!(
        close(burn, on_max_20x) && unit.as_deref() == Some("window_share"),
        "a million Opus 5.5 input tokens over the window, as a share of Max 20x's 5-hour window per hour: {:?} \
         against {on_max_20x}",
        (burn, unit)
    );
    // The 20x equivalent reads no account tier, so the same figure on every tier the account moves to below.
    let equivalent = tables
        .equivalent_at(
            phoebis_routing::FIVE_HOUR,
            None,
            "claude-opus-5-5",
            million,
            now,
        )
        .unwrap()
        * 3_600.0
        / window;
    let equivalent_of = |sessions: &serde_json::Value| {
        let row = session_row(sessions, "s1");
        (row["burn_equivalent"].as_f64(), row["estimated"].clone())
    };
    let (on_20x, estimated) = equivalent_of(&renewed);
    assert!(
        close(on_20x, equivalent) && estimated == false,
        "the burn as the 20x 5-hour equivalent per hour: {on_20x:?} against {equivalent}"
    );
    name_the_tier(&parts.store, "business", "default_claude_ai");
    let on_pro = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        !close(session_row(v, "s1")["burn"].as_f64(), on_max_20x)
    })
    .await;
    let pro = tables
        .share_at(
            phoebis_routing::FIVE_HOUR,
            None,
            "default_claude_ai",
            "claude-opus-5-5",
            million,
            now,
        )
        .unwrap()
        * 3_600.0
        / window;
    assert!(
        close(burn_of(&on_pro, "s1").0, pro),
        "the same tokens read on Pro are their share of Pro's own 5-hour window: {:?} against {pro}",
        burn_of(&on_pro, "s1")
    );
    assert!(
        close(equivalent_of(&on_pro).0, equivalent),
        "the equivalent takes no tier division on Pro: {:?} against {equivalent}",
        equivalent_of(&on_pro)
    );
    name_the_tier(&parts.store, "business", "default_claude_team");
    let on_team = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        session_row(v, "s1")["burn"].is_null()
    })
    .await;
    assert_eq!(
        burn_of(&on_team, "s1"),
        (None, Some("window_share".into())),
        "a tier the cost model does not size sizes no window, so the figure is null rather than a guess"
    );
    assert!(
        close(equivalent_of(&on_team).0, equivalent),
        "while the 20x equivalent still reads on a tier the cost model does not size: {:?} against {equivalent}",
        equivalent_of(&on_team)
    );
}

/// The case's own rates: a pin burning a thousand input tokens a second burns five points of the 5-hour window an hour
/// and two tenths of a point of the 7-day window, whatever the account tier, model or instant.
fn round_rates(
    window: &str,
    _entrypoint: Option<&str>,
    _tier: &str,
    _model: &str,
    burn: &phoebis_store::PinBurn,
    _at: i64,
) -> Option<f64> {
    let per_hour = match window {
        phoebis_routing::FIVE_HOUR => 0.05,
        phoebis_routing::SEVEN_DAY => 0.002,
        _ => return None,
    };
    Some(burn.input / 1_000.0 * per_hour / 3_600.0)
}

/// `sessions_harness_with` with the router pricing the pins' burn through `round_rates`, the pool's ceiling at 0.95
/// and its burn average at 1,800 seconds.
async fn sessions_harness_rated(pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>) -> common::Full {
    use common::{generation, login, post};
    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","window_ceiling":0.95,"burn_average_seconds":1800,"members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&seeding)).await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    name_the_tier(&seed.store, "business", "default_claude_max_20x");
    for pin in pins(generation(&seed.store, "desktop")) {
        seed.store.upsert_pin(&pin).unwrap();
    }
    common::admin_parts_full_on_rated(seed.store.clone(), round_rates).await
}

/// Reports `readings` of `account` as an answer's head would land them, read a minute before the rig's clock.
async fn read_windows(parts: &common::Full, account: &str, readings: &[(&str, f64, Option<i64>)]) {
    let sample = readings.iter().fold(
        phoebis_routing::UtilizationSample::default(),
        |sample, (window, utilization, reset)| {
            sample.with(
                window,
                phoebis_routing::WindowReading {
                    utilization: *utilization,
                    reset: *reset,
                    observed_at: common::NOW - 60_000,
                    source: phoebis_routing::WindowSource::Head,
                },
            )
        },
    );
    parts
        .router
        .report(phoebis_routing::Report::Sample {
            account_id: account.into(),
            sample,
            incarnation: 1,
            source_request_id: None,
        })
        .await;
}

fn within(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

#[tokio::test]
async fn a_pinned_sessions_runway_reads_its_accounts_own_projection_with_its_burn_counted_once() {
    use common::{login, wait_json, NOW};
    const HOUR: i64 = 3_600_000;
    // One pin on `business` burning five points of its 5-hour window an hour, the window at 0.80 and resetting in four
    // hours, under the pool's ceiling of 0.95: the account reaches the ceiling in (0.95 - 0.80) / 0.05 = 3 hours.
    // `fit_of`'s arithmetic for a destination would add the pin's burn a second time and its cold rewrite, which the
    // rig prices at none, so it would read (0.15 - 0) / 0.10 = 1.5 hours.
    let parts = sessions_harness_rated(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5-5".into(),
            ..burning_opus_pin("s1", "business", generation, NOW, 1_000.0, 1_000.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    read_windows(
        &parts,
        "business",
        &[
            (phoebis_routing::FIVE_HOUR, 0.80, Some(NOW + 4 * HOUR)),
            (phoebis_routing::SEVEN_DAY, 0.30, Some(NOW + 50 * HOUR)),
        ],
    )
    .await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        !session_row(v, "s1")["runway_ms"].is_null()
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (&row["runway_ms"], &row["runway_bounded"]),
        (&serde_json::json!(10_800_000), &serde_json::json!(true)),
        "the runway on the 5-hour window, the shorter of the two the model draws: {row}"
    );
}

#[tokio::test]
async fn a_pinned_sessions_runway_reads_null_without_a_projection_unbounded_without_a_burn_and_its_burn_decays_to_the_read(
) {
    use common::{get, json, login, wait_json, NOW};
    const HOUR: i64 = 3_600_000;
    // The default pricer prices no burn, so `business` projects its reading alone and its pins' runway is unbounded,
    // and `direct` is an api_key account with no plan window to project. `s-old` burns as `s1` does but folded ten
    // minutes before the read, so it reads its burn decayed over the pool's 1,800-second average.
    let parts = sessions_harness_with(|generation| {
        vec![
            phoebis_store::PinRow {
                model: "claude-opus-5-5".into(),
                ..burning_opus_pin("s1", "business", generation, NOW, 100.0, 100.0)
            },
            phoebis_store::PinRow {
                model: "claude-opus-5-5".into(),
                burn_at: Some(NOW - 600_000),
                ..burning_opus_pin("s-old", "business", generation, NOW, 100.0, 100.0)
            },
            burning_opus_pin("s2", "direct", generation, NOW, 100.0, 100.0),
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let before = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let s1 = session_row(&before, "s1");
    assert_eq!(
        (&s1["runway_ms"], &s1["runway_bounded"]),
        (&serde_json::Value::Null, &serde_json::Value::Null),
        "no reading of a window the model draws, no projection: {s1}"
    );
    read_windows(
        &parts,
        "business",
        &[(phoebis_routing::FIVE_HOUR, 0.80, Some(NOW + 4 * HOUR))],
    )
    .await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        !session_row(v, "s1")["runway_bounded"].is_null()
    })
    .await;
    let (s1, s_old, s2) = (
        session_row(&sessions, "s1"),
        session_row(&sessions, "s-old"),
        session_row(&sessions, "s2"),
    );
    assert_eq!(
        (&s1["runway_ms"], &s1["runway_bounded"]),
        (&serde_json::Value::Null, &serde_json::json!(false)),
        "a projection with no burn never reaches the ceiling: {s1}"
    );
    assert_eq!(
        (&s2["runway_ms"], &s2["runway_bounded"]),
        (&serde_json::Value::Null, &serde_json::Value::Null),
        "an api_key account has no projection: {s2}"
    );
    let decay = (-600_000.0_f64 / 1_800_000.0).exp();
    assert!(
        within(
            s_old["burn"].as_f64().map(|b| b / s1["burn"].as_f64().unwrap()),
            decay
        ),
        "the older fold reads its burn decayed to the read by the pool's average: {s_old} against {s1}"
    );
    assert_eq!(
        (s_old["burn_at"].as_i64(), s1["burn_at"].as_i64()),
        (Some(NOW), Some(NOW)),
        "both burns are served at the read's instant"
    );
}

#[tokio::test]
async fn an_account_window_carries_its_projection_horizon_and_burn_and_null_where_nothing_is_projected(
) {
    use common::{get, json, login, post, wait_json, NOW};
    use phoebis_ledger::QuotaSampleRecord;
    const HOUR: i64 = 3_600_000;
    let parts = sessions_harness_rated(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5-5".into(),
            ..burning_opus_pin("s1", "business", generation, NOW, 1_000.0, 1_000.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts",
            r#"{"id":"outside","kind":"oauth","label":"Outside"}"#,
            Some(&cookie),
        )
        .await
        .status(),
        201
    );
    for account in ["business", "outside"] {
        read_windows(
            &parts,
            account,
            &[
                (phoebis_routing::FIVE_HOUR, 0.80, Some(NOW + 4 * HOUR)),
                (phoebis_routing::SEVEN_DAY, 0.30, Some(NOW + 50 * HOUR)),
            ],
        )
        .await;
    }
    // The ledger's rows: `business`'s two plan windows and a model week the router keeps no reading of, the same
    // 5-hour row on `outside`, which no pool holds, and one on the api_key account.
    let bucket = phoebis_ledger::bucket_of(NOW - 60_000);
    let row = |account: &str, window: &str, utilization: f64| QuotaSampleRecord {
        account_id: account.into(),
        bucket,
        window: window.into(),
        utilization: Some(utilization),
        reset_at: None,
        source: "head".into(),
        observed_at: None,
    };
    parts
        .db
        .insert_samples(
            &[
                row("business", "five_hour", 0.80),
                row("business", "seven_day", 0.30),
                row("business", "seven_day_opus", 0.03),
                row("outside", "five_hour", 0.80),
                row("direct", "five_hour", 0.10),
            ],
            None,
        )
        .unwrap();
    let account = wait_json(&parts.app, "/admin/accounts/business", &cookie, |v| {
        v["windows"].as_array().is_some_and(|w| w.len() == 3)
    })
    .await;
    let window = |account: &serde_json::Value, id: &str| {
        account["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == id)
            .unwrap_or_else(|| panic!("no {id} window: {account}"))
            .clone()
    };
    // Five points an hour over the four hours to the reset, and two tenths of a point an hour over the five hours a
    // weekly horizon reaches.
    let five_hour = window(&account, "five_hour");
    assert!(
        within(five_hour["projection"].as_f64(), 0.80 + 0.05 * 4.0)
            && five_hour["horizon"] == NOW + 4 * HOUR
            && within(five_hour["burn"].as_f64(), 0.05),
        "{five_hour}"
    );
    let seven_day = window(&account, "seven_day");
    assert!(
        within(seven_day["projection"].as_f64(), 0.30 + 0.002 * 5.0)
            && seven_day["horizon"] == NOW + 5 * HOUR
            && within(seven_day["burn"].as_f64(), 0.002),
        "{seven_day}"
    );
    let unread = window(&account, "seven_day_opus");
    assert_eq!(
        (&unread["projection"], &unread["horizon"], &unread["burn"]),
        (
            &serde_json::Value::Null,
            &serde_json::Value::Null,
            &serde_json::Value::Null
        ),
        "a window the router holds no reading of projects nothing: {unread}"
    );
    let listed = json(get(&parts.app, "/admin/accounts", Some(&cookie)).await).await;
    let listed_row = |id: &str| {
        listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("no {id} row: {listed}"))
            .clone()
    };
    assert_eq!(
        window(&listed_row("business"), "five_hour"),
        five_hour,
        "the list reads the same projection as the account's own route"
    );
    for (id, why) in [
        ("direct", "an api_key account projects no plan window"),
        (
            "outside",
            "an account in no pool has no pool's settings to project with",
        ),
    ] {
        let five_hour = window(&listed_row(id), "five_hour");
        assert_eq!(
            (
                &five_hour["projection"],
                &five_hour["horizon"],
                &five_hour["burn"]
            ),
            (
                &serde_json::Value::Null,
                &serde_json::Value::Null,
                &serde_json::Value::Null
            ),
            "{why}: {five_hour}"
        );
    }
}

/// The pool `main` over `business` and `direct` with its slow-down switch at `slow_down`, its threshold at 600 seconds
/// and its hold cap at 60, the router pricing through `round_rates`: `s1` on the tiered `business` burns five points
/// of its 5-hour window an hour. `business` is paired and ready, so a turn of `s1` stays on it.
async fn slow_down_harness(slow_down: bool) -> common::Full {
    use common::{generation, login, post};
    let seed = common::admin_parts_full().await;
    seed.store
        .commit_credentials(
            "business",
            1,
            1,
            None,
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new("sk-ant-oat01-PLACEHOLDER"),
                refresh_token: Some(phoebis_store::Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: common::NOW,
                expires_at: common::NOW + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    seed.store
        .set_state(
            "business",
            phoebis_store::AccountState::Ready,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    let seeding = login(&seed.app, "admin-secret").await;
    let pool = format!(
        r#"{{"id":"main","label":"Main","strategy":"priority","window_ceiling":0.95,"burn_average_seconds":1800,"slow_down":{slow_down},"slow_down_stretch_seconds":600,"slow_down_hold_seconds":60,"members":[{{"account_id":"business","priority":0}},{{"account_id":"direct","priority":1}}]}}"#
    );
    assert_eq!(
        post(&seed.app, "/admin/pools", &pool, Some(&seeding))
            .await
            .status(),
        201
    );
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    name_the_tier(&seed.store, "business", "default_claude_max_20x");
    seed.store
        .upsert_pin(&phoebis_store::PinRow {
            model: "claude-sonnet-5".into(),
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            ..burning_opus_pin(
                "s1",
                "business",
                generation(&seed.store, "desktop"),
                common::NOW,
                1_000.0,
                1_000.0,
            )
        })
        .unwrap();
    common::admin_parts_full_on_rated(seed.store.clone(), round_rates).await
}

/// A main turn of `session` on the desktop key at `at`, as the relay asks for its first wire attempt.
async fn main_turn(
    parts: &common::Full,
    session: &str,
    request_id: &str,
    at: i64,
) -> phoebis_routing::SelectResult {
    let snapshot = parts.router.snapshot();
    let key = snapshot
        .key("desktop")
        .expect("the key is in the snapshot")
        .clone();
    let candidates = phoebis_routing::resolve(
        &snapshot,
        &key,
        "claude-sonnet-5",
        "claude-sonnet-5",
    )
    .expect("the pool allows the model");
    parts
        .router
        .select(phoebis_routing::SelectRequest {
            request_id: request_id.into(),
            attempt: 1,
            seq: 1,
            key_id: "desktop".into(),
            key_generation: common::generation(&parts.store, "desktop"),
            agent_id: "main".into(),
            session_id: Some(session.into()),
            canonical_model: "claude-sonnet-5".into(),
            folded_model: "claude-sonnet-5".into(),
            candidates,
            portable: true,
            attempted: Vec::new(),
            now: at,
            received_at: at,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: Some("main".into()),
            context_compacted: false,
            static_hash: None,
            boundaries: std::sync::Arc::from([]),
            client: phoebis_routing::ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: std::sync::Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
        })
        .await
        .unwrap()
        .result
}

/// The member `account` of the pool `main` as the pools list answers it.
fn pool_member(pools: &serde_json::Value, account: &str) -> serde_json::Value {
    pools["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main")
        .and_then(|p| {
            p["members"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["account_id"] == account)
                .cloned()
        })
        .unwrap_or_else(|| panic!("no member {account} of main: {pools}"))
}

#[tokio::test]
async fn the_account_routes_answer_slow_down_null_on_an_api_key_account_and_under_a_pool_with_the_slow_down_off(
) {
    use common::{get, json, login, NOW};
    const HOUR: i64 = 3_600_000;
    let parts = slow_down_harness(false).await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A reading the slow-down would act on, two points under the ceiling with four hours left.
    read_windows(
        &parts,
        "business",
        &[(phoebis_routing::FIVE_HOUR, 0.93, Some(NOW + 4 * HOUR))],
    )
    .await;
    assert!(matches!(
        main_turn(&parts, "s1", "r-1", NOW).await,
        phoebis_routing::SelectResult::Selected(_)
    ));
    let null = serde_json::Value::Null;
    let business = json(get(&parts.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        business.get("slow_down"),
        Some(&null),
        "a pool with the slow-down off keeps no entry in force: {business}"
    );
    let five_hour = business["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == "five_hour")
        .unwrap_or_else(|| panic!("no five_hour window: {business}"))
        .clone();
    assert!(
        within(five_hour["burn"].as_f64(), 0.05) && five_hour["demand"] == five_hour["burn"],
        "with no speed under 1 the demand is the achieved burn, in share per hour: {five_hour}"
    );
    let direct = json(get(&parts.app, "/admin/accounts/direct", Some(&cookie)).await).await;
    assert_eq!(
        direct.get("slow_down"),
        Some(&null),
        "an api_key account has no 5-hour window to slow: {direct}"
    );
    let pools = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await;
    assert_eq!(
        (
            pool_member(&pools, "business").get("speed"),
            pool_member(&pools, "direct").get("speed")
        ),
        (Some(&null), Some(&null)),
        "a member's speed is null where its slow_down is"
    );
}

#[tokio::test]
async fn a_slowed_account_answers_its_slow_down_on_the_account_route_and_its_factor_as_its_members_and_sessions_speed(
) {
    use common::{get, json, login, NOW};
    const HOUR: i64 = 3_600_000;
    let parts = slow_down_harness(true).await;
    let cookie = login(&parts.app, "admin-secret").await;
    // Two points under the ceiling with four hours left at five points an hour: the window reaches the ceiling 24
    // minutes in, so the sessions are slowed to a tenth.
    read_windows(
        &parts,
        "business",
        &[(phoebis_routing::FIVE_HOUR, 0.93, Some(NOW + 4 * HOUR))],
    )
    .await;
    assert!(matches!(
        main_turn(&parts, "s1", "r-1", NOW).await,
        phoebis_routing::SelectResult::Selected(_)
    ));
    let business = json(get(&parts.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let slow_down = &business["slow_down"];
    let factor = slow_down["factor"]
        .as_f64()
        .unwrap_or_else(|| panic!("the pinned read's entry is in force: {business}"));
    assert!(factor > 0.0 && factor < 1.0, "{slow_down}");
    assert_eq!(
        (
            &slow_down["speed"],
            &slow_down["since"],
            &slow_down["threshold_ms"],
            &slow_down["hold_ms"]
        ),
        (
            &serde_json::json!(1.0),
            &serde_json::json!(NOW),
            &serde_json::json!(600_000),
            &serde_json::json!(60_000)
        ),
        "no pin has a cycle yet, so none is held, and the span opened at the read: {slow_down}"
    );
    assert!(
        slow_down["stretch_ms"]
            .as_i64()
            .is_some_and(|stretch| stretch > 600_000),
        "{slow_down}"
    );
    let direct = json(get(&parts.app, "/admin/accounts/direct", Some(&cookie)).await).await;
    assert_eq!(
        direct.get("slow_down"),
        Some(&serde_json::Value::Null),
        "an api_key account has no 5-hour window to slow, whatever its pool's switch reads: {direct}"
    );
    let pools = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await;
    assert_eq!(
        (
            pool_member(&pools, "business")["speed"].as_f64(),
            pool_member(&pools, "direct").get("speed")
        ),
        (Some(factor), Some(&serde_json::Value::Null)),
        "the member's speed is its account's factor, and null where no entry is in force"
    );
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let s1 = session_row(&sessions, "s1");
    assert_eq!(
        (s1["speed"].as_f64(), s1.get("cycle_ms")),
        (Some(factor), Some(&serde_json::Value::Null)),
        "the pin reads its account's factor, and no cycle before a second request of the pin: {s1}"
    );
}

#[tokio::test]
async fn a_session_row_with_no_pin_reads_no_slow_down_speed_and_no_cycle() {
    use common::{login, wait_json, NOW};
    let parts = slow_down_harness(true).await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.record(phoebis_ledger::Record::Request {
        request: phoebis_ledger::RequestRecord {
            id: "r-unpinned".into(),
            received_at: NOW - 1_500,
            finished_at: Some(NOW - 1_000),
            key_id: Some("desktop".into()),
            key_generation: common::generation(&parts.store, "desktop"),
            session_id: Some("s-unpinned".into()),
            agent_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    });
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s-unpinned")
    })
    .await;
    let row = session_row(&sessions, "s-unpinned");
    let null = serde_json::Value::Null;
    assert_eq!(
        (
            row["pinned"].as_bool(),
            row.get("speed"),
            row.get("cycle_ms")
        ),
        (Some(false), Some(&null), Some(&null)),
        "a row with no pin has no account's factor and no cycle: {row}"
    );
}

#[tokio::test]
async fn a_pool_member_answers_the_engines_pace_score_whatever_its_kind() {
    use common::{login, wait_json, NOW};
    const HOUR: i64 = 3_600_000;
    let parts = sessions_harness_rated(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5-5".into(),
            ..burning_opus_pin("s1", "business", generation, NOW, 1_000.0, 1_000.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    // Half the week left over the fifty hours to its reset, less the pin's two tenths of a point an hour.
    read_windows(
        &parts,
        "business",
        &[(phoebis_routing::SEVEN_DAY, 0.5, Some(NOW + 50 * HOUR))],
    )
    .await;
    // The api_key account's worst live bucket stands at 0.7.
    read_windows(
        &parts,
        "direct",
        &[("itpm:claude-opus-5-5", 0.7, Some(NOW + 30_000))],
    )
    .await;
    let member = |pools: &serde_json::Value, id: &str| {
        pools["pools"][0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["account_id"] == id)
            .unwrap_or_else(|| panic!("no member {id}: {pools}"))
            .clone()
    };
    let pools = wait_json(&parts.app, "/admin/pools", &cookie, |v| {
        member(v, "direct")["pace_score"]
            .as_f64()
            .is_some_and(|score| score < 1.0)
            && member(v, "business")["burn"].as_f64().is_some()
    })
    .await;
    let (business, direct) = (member(&pools, "business"), member(&pools, "direct"));
    assert!(
        within(business["pace_score"].as_f64(), 0.5 / 50.0 - 0.002)
            && within(business["burn"].as_f64(), 0.002),
        "an OAuth member scores its spare rate on the 7-day window and burns its pins' share of it: {business}"
    );
    assert!(
        within(direct["pace_score"].as_f64(), 1.0 - 0.7),
        "an api_key member scores the room its worst bucket leaves: {direct}"
    );
    assert_eq!(
        direct["burn"],
        serde_json::Value::Null,
        "an api_key member has no plan window to burn: {direct}"
    );
}

#[tokio::test]
async fn a_pin_on_a_model_without_a_weight_reads_an_estimated_burn() {
    // The pin burns Opus 5, which the shipped cost model has no weight for: the share is estimated at the weight of
    // its model tier's latest weighted model, and the same pin in overage prices in dollars, since the price table
    // lists the model.
    use common::{get, json, login, wait_json};
    let now = common::NOW;
    let parts = sessions_harness_with(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5".into(),
            ..burning_opus_pin("s1", "business", generation, now, 100.0, 100.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let tables = phoebis_ledger::PlanTables::shipped();
    let donor = tables
        .tier_of("claude-opus-5", now)
        .and_then(|tier| tables.latest_weighted(tier, now))
        .expect("the shipped model weighs a model of Opus 5's model tier");
    // The banded copy at the fixed clock, which the peak band never covers, priced at the donor's weight on Max 20x.
    let share_per_hour = tables
        .share_at(
            phoebis_routing::FIVE_HOUR,
            None,
            "default_claude_max_20x",
            donor,
            phoebis_routing::PinUsage {
                input: 100,
                ..phoebis_routing::PinUsage::default()
            },
            now,
        )
        .unwrap()
        * 3_600.0;
    let (burn, unit) = burn_of(&sessions, "s1");
    assert!(
        close(burn, share_per_hour) && unit.as_deref() == Some("window_share"),
        "a model the cost model has no weight for burns at its model tier's latest weight: {:?} against \
         {share_per_hour}",
        (burn, unit)
    );
    let equivalent_per_hour = tables
        .equivalent_at(
            phoebis_routing::FIVE_HOUR,
            None,
            donor,
            phoebis_routing::PinUsage {
                input: 100,
                ..phoebis_routing::PinUsage::default()
            },
            now,
        )
        .unwrap()
        * 3_600.0;
    let row = session_row(&sessions, "s1");
    assert!(
        close(row["burn_equivalent"].as_f64(), equivalent_per_hour) && row["estimated"] == true,
        "the 20x equivalent at the donor's weight, marked estimated: {row}"
    );
    parts
        .router
        .report(phoebis_routing::Report::Overage {
            account_id: "business".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: Some(now - 3_600_000),
                reset: Some(now + 3_600_000),
                observed_at: now,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    let in_overage = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        session_row(v, "s1")["burn_unit"] == "usd"
    })
    .await;
    assert!(
        burn_of(&in_overage, "s1").0.is_some_and(|b| b > 0.0),
        "dollars price the same pin, since the price table lists the model"
    );
    let row = session_row(&in_overage, "s1");
    assert_eq!(
        (row.get("burn_equivalent"), &row["estimated"]),
        (Some(&serde_json::Value::Null), &serde_json::json!(false)),
        "the dollars are the price table's own, so nothing is estimated: {row}"
    );
}

#[tokio::test]
async fn every_session_row_answers_its_fork_members_and_a_forked_session_names_its_base() {
    use common::{generation, login, post, wait_json};
    use phoebis_ledger::{Record, RequestRecord};
    use std::sync::atomic::Ordering;

    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(
        &seed.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#,
        Some(&seeding),
    )
    .await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    let key_generation = generation(&seed.store, "desktop");
    // Advanced well past the epoch, so the activity cutoff (`now` minus the harness's 24-hour session
    // expiry) lands at a positive instant: only then can a request be planted both before and after it.
    let now = common::NOW + 200_000_000;
    let cutoff = now - 86_400_000;
    seed.store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "desktop".into(),
            session_id: "pinned-old".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            account_id: "business".into(),
            epoch: 1,
            created_at: now - 90_000_000,
            last_used_at: now - 1_000,
            key_generation,
            incarnation: 1,
            burn: phoebis_store::PinBurn {
                input: 1.0,
                ..phoebis_store::PinBurn::default()
            },
            burn_at: Some(now - 1_000),
            burn_banded: phoebis_store::PinBurn {
                input: 1.0,
                ..phoebis_store::PinBurn::default()
            },
            burn_fast: phoebis_store::PinBurn::default(),
            burn_unmetered: phoebis_store::PinBurn::default(),
            last_input_tokens: 100,
            last_classifier: false,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            shed_reason: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: String::new(),
            marked_at: None,
            marked_on: None,
            marked_window: None,
        })
        .unwrap();
    // A live pin the ledger has never recorded a request for at all, not merely one older than the cutoff:
    // `session_identities` reads no time bound, so this row's identity is null because the ledger genuinely
    // holds nothing, not because of the window.
    seed.store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "desktop".into(),
            session_id: "pin-no-request".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            account_id: "business".into(),
            epoch: 1,
            created_at: now - 1_000,
            last_used_at: now - 1_000,
            key_generation,
            incarnation: 1,
            burn: phoebis_store::PinBurn {
                input: 1.0,
                ..phoebis_store::PinBurn::default()
            },
            burn_at: Some(now - 1_000),
            burn_banded: phoebis_store::PinBurn {
                input: 1.0,
                ..phoebis_store::PinBurn::default()
            },
            burn_fast: phoebis_store::PinBurn::default(),
            burn_unmetered: phoebis_store::PinBurn::default(),
            last_input_tokens: 100,
            last_classifier: false,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            shed_reason: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: String::new(),
            marked_at: None,
            marked_on: None,
            marked_window: None,
        })
        .unwrap();
    // The router task loads its pins at spawn, so the pins above need the store written before this call.
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    parts.clock_cell.store(now, Ordering::SeqCst);
    let cookie = login(&parts.app, "admin-secret").await;

    let request = |id: &str, session: &str, at: i64| RequestRecord {
        id: id.into(),
        received_at: at,
        key_id: Some("desktop".into()),
        key_generation,
        session_id: Some(session.into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        ..Default::default()
    };
    parts.ledger.record(Record::Request {
        request: request("r-plain", "sess-plain", now - 1_000),
        attempts: vec![],
        bodies: None,
    });
    parts.ledger.record(Record::Request {
        request: request("r-pseudo", "e31a9f0011", now - 2_000),
        attempts: vec![],
        bodies: None,
    });
    // Older than `cutoff`: absent from the ledger's own activity window, so only the live pin surfaces it.
    assert!(now - 90_000_000 < cutoff);
    parts.ledger.record(Record::Request {
        request: request("r-pinned-old", "pinned-old", now - 90_000_000),
        attempts: vec![],
        bodies: None,
    });
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            forked_from: Some("s1".into()),
            fork_depth: Some(3),
            ..request("r-forked", "sess-forked", now - 3_000)
        },
        attempts: vec![],
        bodies: None,
    });

    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"].as_array().unwrap().len() >= 5
    })
    .await;
    let row = |session: &str| {
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["session_id"] == session)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            row("sess-forked")["forked_from"].as_str(),
            row("sess-forked")["fork_depth"].as_i64()
        ),
        (Some("s1"), Some(3)),
        "a session whose first request forked from another names it and the matched depth"
    );
    for session in ["sess-plain", "e31a9f0011", "pinned-old"] {
        assert_eq!(
            (
                row(session)["forked_from"].as_str(),
                row(session)["fork_depth"].as_i64()
            ),
            (None, None),
            "{session}: no fork, so both read null"
        );
    }
    assert_eq!(
        (
            row("pin-no-request")["identified_by"].as_str(),
            row("pin-no-request")["identity_root"].as_str(),
            row("pin-no-request")["forked_from"].as_str(),
            row("pin-no-request")["fork_depth"].as_i64(),
        ),
        (None, None, None, None),
        "a pin the ledger holds no client request for at all reads null on every identity field"
    );
}

#[tokio::test]
async fn a_request_groups_under_its_own_session_with_that_sessions_pin_and_releasing_it_frees_the_pin(
) {
    use common::{generation, login, post, wait_json};
    use phoebis_ledger::{EventRecord, Record, RequestRecord};
    use std::sync::atomic::Ordering;

    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(
        &seed.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#,
        Some(&seeding),
    )
    .await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    let key_generation = generation(&seed.store, "desktop");
    let now = common::NOW + 200_000_000;
    // A request groups under its own session id, the one its pin is held under.
    seed.store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "desktop".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            account_id: "business".into(),
            epoch: 2,
            created_at: now - 2_000,
            last_used_at: now - 1_000,
            key_generation,
            incarnation: 1,
            burn: phoebis_store::PinBurn {
                input: 1.0,
                ..phoebis_store::PinBurn::default()
            },
            burn_at: Some(now - 1_000),
            burn_banded: phoebis_store::PinBurn {
                input: 1.0,
                ..phoebis_store::PinBurn::default()
            },
            burn_fast: phoebis_store::PinBurn::default(),
            burn_unmetered: phoebis_store::PinBurn::default(),
            last_input_tokens: 100,
            last_classifier: false,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            shed_reason: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: String::new(),
            marked_at: None,
            marked_on: None,
            marked_window: None,
        })
        .unwrap();
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    parts.clock_cell.store(now, Ordering::SeqCst);
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-plain".into(),
            received_at: now - 1_500,
            finished_at: Some(now - 1_000),
            key_id: Some("desktop".into()),
            key_generation,
            session_id: Some("s1".into()),
            agent_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    });
    parts.ledger.record(Record::Event(EventRecord {
        at: now - 1_200,
        kind: "pin_migrated".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(
            r#"{"agent_id":"main","epoch":2,"from_account":"direct","reason":"cooldown"}"#.into(),
        ),
        pool_id: None,
        agent_id: Some("main".into()),
        request_id: None,
    }));

    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["requests"].as_u64() == Some(1))
    })
    .await;
    let rows = sessions["sessions"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the pin and the activity are one row: {rows:?}"
    );
    assert_eq!(
        (
            rows[0]["session_id"].as_str(),
            rows[0]["pinned"].as_bool(),
            rows[0]["requests"].as_u64(),
        ),
        (Some("s1"), Some(true), Some(1)),
        "the row is the session's own"
    );

    let detail = wait_json(&parts.app, "/admin/requests/r-plain", &cookie, |v| {
        v["events"].as_array().is_some_and(|e| !e.is_empty())
    })
    .await;
    assert!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "pin_migrated"),
        "the request's events carry the move of the pin it was routed by: {detail}"
    );

    let released = post(
        &parts.app,
        "/admin/sessions/desktop/s1/release",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(released.status(), 204);
    assert!(
        parts.router.pins().await.unwrap().is_empty(),
        "releasing the session frees its pin"
    );
}

#[tokio::test]
async fn a_request_row_answers_its_fork_fields() {
    use common::{generation, get, json, login, post, wait_json};
    use phoebis_ledger::{Record, RequestRecord};

    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let key_generation = generation(&parts.store, "desktop");
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-forked".into(),
            received_at: common::NOW,
            key_id: Some("desktop".into()),
            key_generation,
            session_id: Some("s2".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            forked_from: Some("s1".into()),
            fork_depth: Some(3),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    });
    let requests = wait_json(&parts.app, "/admin/requests", &cookie, |v| {
        !v["requests"].as_array().unwrap().is_empty()
    })
    .await;
    let row = &requests["requests"][0];
    assert_eq!(
        (row["forked_from"].as_str(), row["fork_depth"].as_i64()),
        (Some("s1"), Some(3)),
        "a request row answers its two fork fields"
    );
    let detail = json(get(&parts.app, "/admin/requests/r-forked", Some(&cookie)).await).await;
    assert_eq!(
        (
            detail["request"]["forked_from"].as_str(),
            detail["request"]["fork_depth"].as_i64(),
        ),
        (Some("s1"), Some(3)),
        "the detail route answers the same fields"
    );
}

/// A finished request of `session` on the `desktop` key, from `received_at` to `finished_at`.
fn session_request(
    id: &str,
    session: &str,
    received_at: i64,
    finished_at: i64,
) -> phoebis_ledger::Record {
    phoebis_ledger::Record::Request {
        request: phoebis_ledger::RequestRecord {
            id: id.into(),
            received_at,
            finished_at: Some(finished_at),
            key_id: Some("desktop".into()),
            session_id: Some(session.into()),
            agent_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "incomplete".into(),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    }
}

/// The `stream_interrupted` row the ledger writes for `request_id`'s cut on `attempt`.
fn stream_interrupted(
    request_id: &str,
    session: &str,
    at: i64,
    code: &str,
    text: &str,
    attempt: u32,
) -> phoebis_ledger::Record {
    phoebis_ledger::Record::Event(phoebis_ledger::EventRecord {
        at,
        kind: "stream_interrupted".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some(session.into()),
        model: Some("claude-sonnet-5".into()),
        detail: Some(
            serde_json::json!({
                "code": code,
                "text": text,
                "bytes_relayed": 42,
                "upstream_request_id": format!("req_up_{attempt}"),
                "attempt": attempt,
            })
            .to_string(),
        ),
        request_id: Some(request_id.into()),
        ..Default::default()
    })
}

#[tokio::test]
async fn a_request_detail_answers_its_newest_stream_interrupted_row_as_stream_cut() {
    use common::{login, wait_json, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .ledger
        .record(session_request("r-cut", "s1", NOW - 2_000, NOW - 1_000));
    parts.ledger.record(stream_interrupted(
        "r-cut",
        "s1",
        NOW - 1_500,
        "stream_stalled",
        "no chunk for 20 s",
        1,
    ));
    parts.ledger.record(stream_interrupted(
        "r-cut",
        "s1",
        NOW - 1_000,
        "stream_cut",
        "malformed response: connection closed inside chunked body",
        2,
    ));
    let detail = wait_json(&parts.app, "/admin/requests/r-cut", &cookie, |v| {
        v["events"].as_array().is_some_and(|e| e.len() == 2)
    })
    .await;
    assert_eq!(
        detail["stream_cut"],
        serde_json::json!({
            "at": NOW - 1_000,
            "code": "stream_cut",
            "text": "malformed response: connection closed inside chunked body",
            "bytes_relayed": 42,
            "upstream_request_id": "req_up_2",
            "attempt": 2,
        }),
        "the newest of the request's two rows, its six members"
    );
}

#[tokio::test]
async fn a_request_detail_without_a_stream_interrupted_row_answers_stream_cut_null() {
    use common::{login, wait_json, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .ledger
        .record(session_request("r-plain", "s1", NOW - 2_000, NOW - 1_000));
    let detail = wait_json(&parts.app, "/admin/requests/r-plain", &cookie, |v| {
        v["request"]["id"] == "r-plain"
    })
    .await;
    assert_eq!(
        detail.get("stream_cut"),
        Some(&serde_json::Value::Null),
        "the member is answered, as null: {detail}"
    );
}

#[tokio::test]
async fn a_cut_of_another_request_of_the_session_inside_the_window_is_not_the_requests_stream_cut()
{
    use common::{login, wait_json, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A long main turn, and a subagent's call of the same session that was cut inside the turn's window.
    parts
        .ledger
        .record(session_request("r-main", "s1", NOW - 10_000, NOW - 1_000));
    parts
        .ledger
        .record(session_request("r-sub", "s1", NOW - 6_000, NOW - 5_000));
    parts.ledger.record(stream_interrupted(
        "r-sub",
        "s1",
        NOW - 5_000,
        "stream_cut",
        "malformed response: connection closed inside chunked body",
        1,
    ));
    let main = wait_json(&parts.app, "/admin/requests/r-main", &cookie, |v| {
        v["events"]
            .as_array()
            .is_some_and(|e| e.iter().any(|e| e["kind"] == "stream_interrupted"))
    })
    .await;
    assert_eq!(
        main["stream_cut"],
        serde_json::Value::Null,
        "the session's row lists among the turn's events, and the cut is the subagent's: {main}"
    );
    let sub = wait_json(&parts.app, "/admin/requests/r-sub", &cookie, |v| {
        v["request"]["id"] == "r-sub"
    })
    .await;
    assert_eq!(sub["stream_cut"]["code"], "stream_cut");
}

#[tokio::test]
async fn deleting_either_fast_cooldown_clears_its_own_row_and_leaves_the_others() {
    use common::{delete, get, json, login, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let cooled = |scope: &str, reason: &str, window: &str| phoebis_routing::Report::Cooldown {
        account_id: "business".into(),
        row: phoebis_store::CooldownRow {
            account_id: "business".into(),
            scope: scope.into(),
            until: NOW + 600_000,
            set_at: 0,
            reason: reason.into(),
            window: window.into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    };
    parts
        .router
        .report(cooled(
            "fast",
            phoebis_routing::FAST_RATE_LIMIT_REASON,
            "fast",
        ))
        .await;
    parts
        .router
        .report(cooled(
            "fast_credits",
            phoebis_routing::FAST_CREDITS_REASON,
            "out_of_credits",
        ))
        .await;
    parts
        .router
        .report(cooled("account", "rate_limit", "five_hour"))
        .await;
    // A query queues behind the reports, so the rows it answers are the ones the route will read.
    assert_eq!(parts.router.cooldowns().await.unwrap().len(), 3);
    let shown = json(get(&parts.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let mut scopes: Vec<&str> = shown["cooldowns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["scope"].as_str().unwrap())
        .collect();
    scopes.sort_unstable();
    assert_eq!(scopes, ["account", "fast", "fast_credits"]);
    let standing = || async {
        parts
            .router
            .cooldowns()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.scope)
            .collect::<Vec<_>>()
    };
    let path = "/admin/accounts/business/cooldowns/fast";
    assert_eq!(delete(&parts.app, path, Some(&cookie)).await.status(), 204);
    assert_eq!(
        standing().await,
        ["account", "fast_credits"],
        "the rate limit's row alone leaves"
    );
    assert_eq!(delete(&parts.app, path, Some(&cookie)).await.status(), 404);
    let path = "/admin/accounts/business/cooldowns/fast_credits";
    assert_eq!(delete(&parts.app, path, Some(&cookie)).await.status(), 204);
    assert_eq!(
        standing().await,
        ["account"],
        "the account's own row stands"
    );
    let refused = delete(
        &parts.app,
        "/admin/accounts/business/cooldowns/turbo",
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert_eq!(
        json(refused).await["error"],
        "scope is account, fast, fast_credits or model:<canonical>"
    );
}

#[tokio::test]
async fn the_tls_reload_route_answers_401_without_a_session_and_an_empty_list_where_no_listener_is_known(
) {
    use common::{json, login, post};
    let (app, _up) = common::admin_app().await;
    assert_eq!(
        post(&app, "/admin/tls/reload", "", None).await.status(),
        401
    );
    let cookie = login(&app, "admin-secret").await;
    let reloaded = post(&app, "/admin/tls/reload", "", Some(&cookie)).await;
    assert_eq!(reloaded.status(), 200);
    assert_eq!(json(reloaded).await, serde_json::json!({"listeners": []}));
}
