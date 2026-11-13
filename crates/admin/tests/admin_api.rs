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

const KEPT_LOGIN: &str = r#"{"secret":"admin-secret","stay_logged_in":true}"#;

/// The cookie a kept login's answer sets, after checking it is one line of the login's shape and 30-day life. The
/// messages name the attributes alone, since the value is the session's credential.
fn renewal_of(response: &axum::http::Response<axum::body::Body>, what: &str) -> String {
    let lines: Vec<&str> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "{what}: the answer renews the kept login's cookie"
    );
    let (_, attributes) = lines[0].split_once(';').unwrap();
    assert_eq!(
        attributes, " Path=/admin; HttpOnly; SameSite=Strict; Max-Age=2592000",
        "{what}: the renewal keeps the login's attributes and its 30 days"
    );
    common::cookie_of(response)
}

/// One `Set-Cookie` line, the logout's clear, checked without printing a line that might carry a token.
fn assert_clear_alone(response: &axum::http::Response<axum::body::Body>, what: &str) {
    let lines: Vec<&str> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{what}: one Set-Cookie line");
    assert!(
        lines[0] == "phoebis_admin=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0",
        "{what}: the line is the clear"
    );
}

#[tokio::test]
async fn a_kept_login_sets_a_thirty_day_cookie_that_a_restart_on_the_same_store_verifies() {
    use common::{cookie_of, get, json, post};
    let (app, _up, store, _router) = common::admin_parts().await;
    let login = post(&app, "/admin/session", KEPT_LOGIN, None).await;
    assert_eq!(login.status(), 204);
    let attributes = cookie_attributes(&login);
    let (_, attributes) = attributes.split_once(';').unwrap();
    assert_eq!(
        attributes, " Path=/admin; HttpOnly; SameSite=Strict; Max-Age=2592000",
        "a kept login's cookie lives 30 days beside the login's attributes"
    );
    let cookie = cookie_of(&login);
    // The same store under the same secret, as the next start of the router reads them.
    let (restarted, _up, _, _) = common::admin_parts_on(store).await;
    let shown = get(&restarted, "/admin/session", Some(&cookie)).await;
    assert_eq!(
        shown.status(),
        200,
        "the restarted sessions verify the token"
    );
    assert_eq!(json(shown).await["persistent"], true);
}

#[tokio::test]
async fn every_answer_to_a_kept_login_renews_its_cookie_under_the_same_session_id() {
    use common::{cookie_of, get, json, post};
    let (app, _up) = common::admin_app().await;
    let first = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    let mut cookie = first.clone();
    let mut ids = Vec::new();
    for (method, path) in [
        ("GET", "/admin/session"),
        ("GET", "/admin/accounts"),
        ("POST", "/admin/accounts/business/pause"),
        ("GET", "/admin/no-such-route"),
        ("GET", "/admin/session"),
    ] {
        let answer = match method {
            "GET" => get(&app, path, Some(&cookie)).await,
            _ => post(&app, path, "", Some(&cookie)).await,
        };
        let renewed = renewal_of(&answer, &format!("{method} {path}"));
        assert!(
            renewed != cookie,
            "{method} {path}: a fresh issue instant and nonce"
        );
        if path == "/admin/session" {
            ids.push(json(answer).await["session_id"].clone());
        }
        cookie = renewed;
    }
    let last = json(get(&app, "/admin/session", Some(&cookie)).await).await;
    ids.push(last["session_id"].clone());
    assert!(ids[0].is_string());
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "one session id under every renewed cookie"
    );
    assert_eq!(
        get(&app, "/admin/session", Some(&first)).await.status(),
        200,
        "a renewal leaves the earlier token valid to its own expiry"
    );
}

#[tokio::test]
async fn a_kept_logins_renewal_carries_secure_under_tls_or_a_fronts_https_and_not_otherwise() {
    use common::{cookie_of, get, post, post_forwarded};
    let (tls, _up) = common::admin_app_tls().await;
    let cookie = cookie_of(&post(&tls, "/admin/session", KEPT_LOGIN, None).await);
    let terminated = get(&tls, "/admin/session", Some(&cookie)).await;
    assert_eq!(terminated.status(), 200);
    assert!(
        cookie_attributes(&terminated).ends_with("; Secure"),
        "a read under TLS renews the cookie with the whole attribute"
    );

    let (plain, _up) = common::admin_app().await;
    let login = post_forwarded(&plain, "/admin/session", KEPT_LOGIN, None, "https").await;
    let cookie = cookie_of(&login);
    let fronted = post_forwarded(
        &plain,
        "/admin/accounts/business/pause",
        "",
        Some(&cookie),
        "https",
    )
    .await;
    assert_eq!(fronted.status(), 204);
    assert!(
        cookie_attributes(&fronted).ends_with("; Secure"),
        "an answer behind a front's https renews the cookie with the attribute"
    );
    let bare = get(&plain, "/admin/session", Some(&cookie)).await;
    assert_eq!(bare.status(), 200);
    assert!(
        !cookie_attributes(&bare).contains("Secure"),
        "a plain read's renewal carries none"
    );
}

#[tokio::test]
async fn the_session_answer_reads_persistent_for_a_kept_login_alone() {
    use common::{cookie_of, get, json, login, post};
    let (app, _up) = common::admin_app().await;
    let kept = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    let plain = login(&app, "admin-secret").await;
    assert_eq!(
        json(get(&app, "/admin/session", Some(&kept)).await).await["persistent"],
        true
    );
    let answer = get(&app, "/admin/session", Some(&plain)).await;
    assert!(
        answer.headers().get("set-cookie").is_none(),
        "a login without the flag keeps today's cookie, renewed by nothing"
    );
    assert_eq!(json(answer).await["persistent"], false);
}

#[tokio::test]
async fn a_pairing_started_under_a_kept_login_completes_under_each_renewed_cookie() {
    use common::{cookie_of, delete, get, json, post};
    let (app, _up) = common::admin_app().await;
    let cookie = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    let started = post(
        &app,
        "/admin/oauth/sessions",
        r#"{"account_id":"business","login":"claude_ai"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), 201);
    let cookie = renewal_of(&started, "the pairing's start");
    let id = json(started).await["id"].as_str().unwrap().to_string();
    let shown = get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await;
    assert_eq!(
        shown.status(),
        200,
        "the renewed cookie names the session that started the pairing"
    );
    let cookie = renewal_of(&shown, "the pairing's show");
    assert_eq!(json(shown).await["status"], "pending");
    let canceled = delete(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await;
    assert_eq!(canceled.status(), 204);
    renewal_of(&canceled, "the pairing's cancel");
}

#[tokio::test]
async fn a_kept_logout_answers_its_clear_alone_and_ends_every_kept_token_across_a_restart() {
    use common::{cookie_of, delete, get, post};
    let (app, _up, store, _router) = common::admin_parts().await;
    let first = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    let renewed = renewal_of(&get(&app, "/admin/session", Some(&first)).await, "a read");
    let other = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    let out = delete(&app, "/admin/session", Some(&renewed)).await;
    assert_eq!(out.status(), 204);
    assert_clear_alone(&out, "the kept logout");
    let (restarted, _up, _, _) = common::admin_parts_on(store).await;
    for (cookie, what) in [
        (&first, "the first token"),
        (&renewed, "the renewed token"),
        (&other, "another kept login's token"),
    ] {
        assert_eq!(
            get(&app, "/admin/session", Some(cookie)).await.status(),
            401,
            "{what} on this process"
        );
        assert_eq!(
            get(&restarted, "/admin/session", Some(cookie))
                .await
                .status(),
            401,
            "{what} after a restart"
        );
    }
    let fresh = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    assert_eq!(
        get(&app, "/admin/session", Some(&fresh)).await.status(),
        200
    );
    assert_eq!(
        get(&restarted, "/admin/session", Some(&fresh))
            .await
            .status(),
        200,
        "a kept login after the logout verifies after a restart"
    );
}

#[tokio::test]
async fn a_logout_under_a_token_shaped_cookie_that_fails_verification_leaves_the_epoch_unmoved() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use common::{cookie_of, delete, get, json, post};
    let (app, _up, store, _router) = common::admin_parts().await;
    let kept = cookie_of(&post(&app, "/admin/session", KEPT_LOGIN, None).await);
    let mut forged = URL_SAFE_NO_PAD.decode(&kept).unwrap();
    *forged.last_mut().unwrap() ^= 1;
    let forged = URL_SAFE_NO_PAD.encode(forged);
    let out = delete(&app, "/admin/session", Some(&forged)).await;
    assert_eq!(out.status(), 204);
    assert_clear_alone(&out, "a logout under a forged token");
    assert_eq!(store.admin_session_epoch().unwrap(), 0);
    let shown = get(&app, "/admin/session", Some(&kept)).await;
    assert_eq!(shown.status(), 200, "the kept login was not ended");
    assert_eq!(json(shown).await["persistent"], true);
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
        serde_json::json!({"persistent": false, "session_id": "bearer"})
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
                phoebis_store::AttemptOutcome::Failed {
                    cause: detail.into(),
                    detail: detail.into(),
                },
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
            phoebis_store::AttemptOutcome::Uncertain {
                cause: "lost".into(),
                detail: "lost".into(),
            },
        )
        .unwrap();
    assert_eq!(pair_business(&app, &cookie).await.status(), 200);
    // An attempt left open reads as quarantined, and a newest attempt that settled failed or uncertain answers its
    // detail as the last error. Both read clear only when the commit superseded the row and stands newest.
    let account =
        common::json(common::get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        (
            &account["credential"]["quarantined"],
            &account["credential"]["last_error"]
        ),
        (&serde_json::json!(false), &serde_json::Value::Null),
        "{account}"
    );
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
            "plan": "max_5x",
            "tier": "default_claude_max_5x",
            "since": 1_773_482_400_000_i64,
            "trial_ends_at": 1_790_899_200_000_i64,
            "ends_at": null,
        }),
        "the confirm stored the plan and the start with no poll having run"
    );
}

#[tokio::test]
async fn a_pairing_confirm_clears_the_plan_ended_flag_and_writes_plan_restored_from_the_pairing() {
    use common::{json, login, post};
    let h = common::admin_parts_full().await;
    let cookie = login(&h.app, "admin-secret").await;
    assert!(h
        .store
        .set_plan_ended("business", common::NOW - 60_000)
        .unwrap());
    let id = common::exchanged_session_for(&h.app, &cookie, "business").await;
    let confirmed = post(
        &h.app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    let status = confirmed.status();
    let confirmed = json(confirmed).await;
    assert_eq!(status, 200, "{confirmed}");
    assert_eq!(
        h.store
            .subscription_term("business")
            .unwrap()
            .unwrap()
            .plan_ended_at,
        None,
        "the confirm's write clears the flag"
    );
    assert_eq!(
        confirmed["first_poll_in_flight"],
        serde_json::json!(true),
        "the confirm that cleared the flag starts the first poll: {confirmed}"
    );
    let restored = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "plan_restored")
    })
    .await;
    let detail: serde_json::Value =
        serde_json::from_str(restored.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        (
            restored.account_id.as_deref(),
            detail["source"].as_str(),
            detail["organization_type"].as_str()
        ),
        (Some("business"), Some("pairing"), Some("claude_max")),
        "the row names the pairing and the profile it confirmed"
    );
}

/// A re-pair whose profile still reports no plan keeps the flag at its instant, records no `plan_restored` and starts
/// no first poll, since the poll's schedule reads no flagged account.
#[tokio::test]
async fn a_pairing_confirm_on_a_profile_with_no_plan_keeps_the_plan_ended_flag_and_starts_no_poll()
{
    use common::{json, login, post};
    let h = common::admin_parts_full().await;
    let cookie = login(&h.app, "admin-secret").await;
    let flagged = common::NOW - 60_000;
    assert!(h.store.set_plan_ended("business", flagged).unwrap());
    let mut profile: serde_json::Value =
        serde_json::from_slice(phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Profile,
        ))
        .unwrap();
    profile["organization"]["organization_type"] = serde_json::json!("claude_free");
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&profile).unwrap(),
        },
    );
    let id = common::exchanged_session_for(&h.app, &cookie, "business").await;
    let confirmed = post(
        &h.app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 200);
    let confirmed = json(confirmed).await;
    // The writer lands its channel in order, so once a row recorded behind the confirm shows, a `plan_restored` the
    // confirm recorded shows too.
    h.ledger
        .record(phoebis_ledger::Record::Event(phoebis_ledger::EventRecord {
            at: common::NOW,
            kind: "settled".into(),
            ..Default::default()
        }));
    let events = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        let events = h.db.events(0, i64::MAX, 100).await.unwrap();
        events.iter().any(|e| e.kind == "settled").then_some(events)
    })
    .await;
    assert_eq!(
        (
            h.store
                .subscription_term("business")
                .unwrap()
                .unwrap()
                .plan_ended_at,
            events.iter().filter(|e| e.kind == "plan_restored").count(),
            &confirmed["first_poll_in_flight"],
        ),
        (Some(flagged), 0, &serde_json::json!(false)),
        "the flag stands, no restore is recorded and no first poll starts: {confirmed}"
    );
}

#[tokio::test]
async fn the_account_row_answers_the_plan_ended_flag_as_an_instant_or_null() {
    use common::{get, json, login};
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let flag = |answer: &serde_json::Value| answer["plan_ended_at"].clone();
    let listed = |list: &serde_json::Value| {
        list["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "business")
            .cloned()
            .unwrap()
    };
    let list = json(get(&app, "/admin/accounts", Some(&cookie)).await).await;
    assert_eq!(
        flag(&listed(&list)),
        serde_json::Value::Null,
        "a plan stands"
    );
    assert!(store.set_plan_ended("business", 1_234_567).unwrap());
    let list = json(get(&app, "/admin/accounts", Some(&cookie)).await).await;
    let one = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        (flag(&listed(&list)), flag(&one)),
        (serde_json::json!(1_234_567), serde_json::json!(1_234_567)),
        "the list row and the single account answer the flag's instant"
    );
}

#[tokio::test]
async fn the_refresh_reads_the_profile_first_while_the_plan_ended_flag_stands() {
    use common::{json, login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let refresh = "/admin/accounts/business/usage/refresh";
    let asked = |path: &str| {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with(&format!("GET {path}")))
            .count()
    };
    let flag = || {
        h.store
            .subscription_term("business")
            .unwrap()
            .unwrap()
            .plan_ended_at
    };
    assert!(h
        .store
        .set_plan_ended("business", common::NOW - 60_000)
        .unwrap());
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid","has_claude_max":false,"has_claude_pro":false},"organization":{"uuid":"o","organization_type":"claude_free","rate_limit_tier":"default_claude_ai","subscription_status":"canceled","billing_type":"none"}}"#
                .to_vec(),
        },
    );
    let paused = post(&h.app, refresh, "", Some(&cookie)).await;
    assert_eq!(paused.status(), 200);
    let paused = json(paused).await;
    assert_eq!(
        (
            asked("/api/oauth/profile"),
            asked("/api/oauth/usage"),
            paused["poll"]["last_error"].as_str(),
            flag()
        ),
        (1, 0, Some("plan ended"), Some(common::NOW - 60_000)),
        "a profile with no plan sends no usage request and the answer names the pause: {paused}"
    );
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: phoebis_profile::fixtures::control_response(
                phoebis_profile::fixtures::ControlFixture::Profile,
            )
            .to_vec(),
        },
    );
    let resumed = json(post(&h.app, refresh, "", Some(&cookie)).await).await;
    assert_eq!(
        (
            asked("/api/oauth/profile"),
            asked("/api/oauth/usage "),
            resumed["error"].as_str(),
            flag()
        ),
        (2, 1, None, None),
        "a profile with a plan clears the flag and the usage poll answers its reading: {resumed}"
    );
    let restored = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "plan_restored")
    })
    .await;
    assert!(
        restored
            .detail
            .as_deref()
            .is_some_and(|d| d.contains(r#""source":"profile""#)),
        "{restored:?}"
    );
}

/// A grant in the upstream's field names: one reset of one, usable now, that gives the Fable week back, from
/// 2026-10-01 to 2026-12-01.
fn reset_grant(id: &str) -> serde_json::Value {
    serde_json::json!({
        "clears": ["seven_day_overage_included"],
        "ends_at": "2026-12-01T00:00:00Z",
        "gives_back_weekly": true,
        "id": id,
        "label": "Launch reset",
        "resets_left": 1,
        "resets_total": 1,
        "starts_at": "2026-10-01T00:00:00Z",
        "usable_now": true,
    })
}

/// Stores `block` on `business` as a block read at the rig's clock does: parsed from a usage body and written in its
/// typed form under the row's holder and the epoch 0 the rig's pairing leaves.
fn store_limit_resets(store: &phoebis_store::Store, block: serde_json::Value) {
    let row = store.account("business").unwrap().unwrap();
    let body = serde_json::json!({ "cedar_ember": block }).to_string();
    let parsed = phoebis_oauth::parse_limit_resets(body.as_bytes())
        .unwrap()
        .expect("the body carries a block");
    let stored = serde_json::to_string(&parsed).unwrap();
    assert!(
        store
            .set_limit_resets(
                "business",
                row.incarnation,
                row.lifecycle_generation,
                0,
                Some(&stored),
                common::NOW
            )
            .unwrap(),
        "the block lands on the rig's row"
    );
}

/// The fake's answer to a usage read in the client's shape that carries `block`.
fn limit_resets_answer(block: serde_json::Value) -> fake_upstream::Respond {
    fake_upstream::Respond::Json {
        status: 200,
        headers: vec![],
        body: serde_json::json!({ "cedar_ember": block })
            .to_string()
            .into_bytes(),
    }
}

#[tokio::test]
async fn the_account_row_hides_limit_resets_the_poll_no_longer_refreshes() {
    use common::{delete, get, json, login};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    let polled = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let member = &polled["limit_resets"];
    assert_eq!(
        (
            member["grants"][0]["id"].as_str(),
            member["grants"][0]["state"].as_str(),
            member["left"].as_u64()
        ),
        (Some("rg-b-1"), Some("usable"), Some(1)),
        "a polled account answers its stored block: {polled}"
    );
    assert_eq!(
        delete(
            &h.app,
            "/admin/accounts/business/credentials",
            Some(&cookie)
        )
        .await
        .status(),
        204
    );
    let disconnected = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        (
            disconnected["state"].as_str(),
            &disconnected["limit_resets"]
        ),
        (Some("disconnected"), &serde_json::Value::Null),
        "nothing refreshes a disconnected account's block, so the row answers none: {disconnected}"
    );
}

#[tokio::test]
async fn a_block_with_no_grant_is_not_an_unread_block() {
    use common::{get, json, login};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let unread = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        unread["limit_resets"],
        serde_json::Value::Null,
        "an account whose block was never read answers none: {unread}"
    );
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": false, "grants": [], "ineligible_reason": "seat"}),
    );
    let read = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        read["limit_resets"],
        serde_json::json!({
            "cooldown_until": null,
            "eligible": false,
            "fable_week_share": null,
            "grants": [],
            "ineligible_reason": "seat",
            "left": 0,
            "read_at": common::NOW,
            // The row's first incarnation, 1 << 32, above the epoch 0 of a block no fold or pairing moved.
            "revision": 4_294_967_296_u64,
        }),
        "a block that lists no grant answers the member with an empty list: {read}"
    );
}

#[tokio::test]
async fn the_account_row_hides_limit_resets_once_the_plan_ended() {
    use common::{get, json, login};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    let asked = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        asked["limit_resets"]["left"].as_u64(),
        Some(1),
        "an account the poll asks answers its stored block: {asked}"
    );
    assert!(h.store.set_plan_ended("business", common::NOW).unwrap());
    let ended = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        ended["limit_resets"],
        serde_json::Value::Null,
        "the poll no longer asks an account whose plan ended, so the row answers none: {ended}"
    );
}

#[tokio::test]
async fn the_account_row_drops_a_cooldown_that_passed() {
    use common::{get, json, login};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let cooled = |until: &str| {
        serde_json::json!({
            "cooldown_until": until,
            "eligible": true,
            "grants": [reset_grant("rg-b-1")],
        })
    };
    // The rig's clock reads 2026-10-11T12:16:40Z.
    store_limit_resets(&h.store, cooled("2026-10-11T12:16:41Z"));
    let ahead = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        ahead["limit_resets"]["cooldown_until"].as_i64(),
        Some(1_791_721_001_000),
        "a cooldown one second after the clock reads as stored: {ahead}"
    );
    store_limit_resets(&h.store, cooled("2026-10-11T12:16:40Z"));
    let passed = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        passed["limit_resets"]["cooldown_until"],
        serde_json::Value::Null,
        "a cooldown at the clock's own instant has passed: {passed}"
    );
}

#[tokio::test]
async fn the_account_row_answers_the_fable_weeks_share() {
    use common::{get, json, login};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.usage.poll("business").await.unwrap();
    let unshared = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        unshared["limit_resets"]["fable_week_share"],
        serde_json::Value::Null,
        "the rig's usage body carries no Fable week: {unshared}"
    );
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"limits":[{"kind":"weekly_scoped","percent":30,"scope":{"model":{"display_name":"Fable"}}}],"weekly_scoped_shares":[{"limit_index":0,"used_percent_of_weekly":50}]}"#
                .to_vec(),
        },
    );
    h.usage.poll("business").await.unwrap();
    let shared = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        shared["limit_resets"]["fable_week_share"].as_f64(),
        Some(0.5),
        "a reading whose Fable week used half of the 7-day window answers the share: {shared}"
    );
}

#[tokio::test]
async fn the_read_route_answers_the_stored_block_where_the_read_fails() {
    use common::{json, login, post, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let read = "/admin/accounts/business/limit-resets/read";
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(
        0,
        LIMIT_RESETS_PATH,
        fake_upstream::Respond::Json {
            status: 500,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#
                .to_vec(),
        },
    );
    let failed = post(&h.app, read, "", Some(&cookie)).await;
    assert_eq!(failed.status(), 200);
    let failed = json(failed).await;
    assert_eq!(
        (
            failed["limit_resets"]["grants"][0]["id"].as_str(),
            failed["error"].is_string()
        ),
        (Some("rg-b-1"), true),
        "a failed read answers the stored block beside its error: {failed}"
    );
    h.up.set_rule_after(
        0,
        LIMIT_RESETS_PATH,
        limit_resets_answer(
            serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-2")]}),
        ),
    );
    let landed = post(&h.app, read, "", Some(&cookie)).await;
    assert_eq!(landed.status(), 200);
    let landed = json(landed).await;
    assert_eq!(
        (
            landed["limit_resets"]["grants"][0]["id"].as_str(),
            &landed["error"]
        ),
        (Some("rg-b-2"), &serde_json::Value::Null),
        "a read that lands answers the block it read: {landed}"
    );
}

#[tokio::test]
async fn the_read_route_sends_nothing_to_a_plan_that_ended() {
    use common::{login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    assert!(h.store.set_plan_ended("business", common::NOW).unwrap());
    let refused = post(
        &h.app,
        "/admin/accounts/business/limit-resets/read",
        "",
        Some(&cookie),
    )
    .await;
    let code = refused
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    let sent =
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage?cedar_ember=1"))
            .count();
    assert_eq!(
        (refused.status().as_u16(), code.as_deref(), sent),
        (409, Some("limit_reset_not_polled"), 0),
        "the poll no longer asks an account whose plan ended, so the route sends it no read"
    );
}

/// Commits a bundle for `business` that sits inside its refresh margin at the rig's clock and answers every refresh
/// with the token endpoint's `invalid_grant`, so the next acquisition refreshes and lands the re-pair mark.
fn revoke_on_refresh(h: &common::Polled) {
    let stored = h.store.credentials("business").unwrap().unwrap();
    h.store
        .commit_credentials(
            "business",
            1,
            1,
            Some(stored.generation),
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new("sk-ant-oat01-SHORT"),
                refresh_token: Some(phoebis_store::Secret::new("sk-ant-ort01-SHORT")),
                issued_at: common::NOW - 3_600_000,
                expires_at: common::NOW + 60_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    h.up.set_rule_after(
        0,
        "/v1/oauth/token",
        fake_upstream::Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        },
    );
}

/// Waits for the router's snapshot to read `business` as `reauth_required`. The manager's mark is a store write alone,
/// which starts no registry reload, so only a report reaches the snapshot.
async fn routing_reads_reauth_required(router: &phoebis_routing::RouterHandle) {
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        router
            .snapshot()
            .account("business")
            .filter(|row| row.state == phoebis_store::AccountState::ReauthRequired)
            .map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn the_read_route_reports_a_reauth_mark_its_read_landed() {
    use common::{json, login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    revoke_on_refresh(&h);
    let read = post(
        &h.app,
        "/admin/accounts/business/limit-resets/read",
        "",
        Some(&cookie),
    )
    .await;
    let status = read.status().as_u16();
    let read = json(read).await;
    assert_eq!(
        (status, read["error"].is_string()),
        (200, true),
        "the read's refresh found the refresh token revoked, and the dialog still opens: {read}"
    );
    assert_eq!(
        h.store.account("business").unwrap().unwrap().state,
        phoebis_store::AccountState::ReauthRequired,
        "the refresh's invalid_grant landed the mark"
    );
    routing_reads_reauth_required(&h.router).await;
}

#[tokio::test]
async fn the_cards_refresh_reads_the_limit_resets() {
    use common::{get, json, login, post, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let mut needs_limit = reset_grant("rg-b-1");
    needs_limit["usable_now"] = serde_json::json!(false);
    needs_limit["unavailable_reason"] = serde_json::json!("needs_limit");
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [needs_limit]}),
    );
    let state = |account: &serde_json::Value| {
        account["limit_resets"]["grants"][0]["state"]
            .as_str()
            .map(str::to_string)
    };
    let stored = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        state(&stored).as_deref(),
        Some("blocked"),
        "the stored block lists the grant as blocked: {stored}"
    );
    h.up.set_rule_after(
        0,
        LIMIT_RESETS_PATH,
        limit_resets_answer(
            serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
        ),
    );
    let refreshed = post(
        &h.app,
        "/admin/accounts/business/usage/refresh",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(refreshed.status(), 200);
    let after = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        state(&after).as_deref(),
        Some("usable"),
        "the refresh read the block that lists the grant as usable: {after}"
    );
}

#[tokio::test]
async fn a_refresh_sends_no_limit_resets_read_to_a_plan_that_just_ended() {
    use common::{json, login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 403,
            headers: vec![("x-should-retry".into(), "false".into())],
            body: br#"{"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization.","details":{"error_code":"oauth_not_allowed_for_organization","error_visibility":"user_facing"}}}"#
                .to_vec(),
        },
    );
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid","has_claude_max":false,"has_claude_pro":false},"organization":{"uuid":"o","organization_type":"claude_free","rate_limit_tier":"default_claude_ai","subscription_status":"canceled","billing_type":"none"}}"#
                .to_vec(),
        },
    );
    let refreshed = post(
        &h.app,
        "/admin/accounts/business/usage/refresh",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(refreshed.status(), 200);
    let refreshed = json(refreshed).await;
    let sent = |line: &str| {
        h.up.requests()
            .iter()
            .filter(|r| r.line.starts_with(line))
            .count()
    };
    let ended = h
        .store
        .subscription_term("business")
        .unwrap()
        .unwrap()
        .plan_ended_at;
    assert_eq!(
        (
            sent("GET /api/oauth/usage "),
            ended.is_some(),
            sent("GET /api/oauth/usage?cedar_ember=1")
        ),
        (1, true, 0),
        "the refresh's poll found the plan ended, so no limit resets read follows it: {refreshed}"
    );
}

/// The use of `business`'s grant `rg-b-1`.
const USE_RESET: &str = "/admin/accounts/business/limit-resets/rg-b-1/use";

/// The fake's 200 to a limit reset claim with `body`.
fn claim_answer(body: &str) -> fake_upstream::Respond {
    fake_upstream::Respond::Json {
        status: 200,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

/// The fake's 500 with the upstream's error body.
fn upstream_failure() -> fake_upstream::Respond {
    fake_upstream::Respond::Json {
        status: 500,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#
            .to_vec(),
    }
}

/// How many requests the fake read whose line starts with `line`.
fn sent(up: &fake_upstream::FakeUpstream, line: &str) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with(line))
        .count()
}

/// The routing log's `limit_reset_claimed` rows once `ready` takes them: the ledger's writer lands an event a batch
/// after it was handed over.
async fn claimed_rows(
    app: &axum::Router,
    cookie: &str,
    ready: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let rows = |page: &serde_json::Value| page["rows"].as_array().cloned().unwrap_or_default();
    let page = common::wait_json(
        app,
        "/admin/routing-log?kinds=limit_reset_claimed",
        cookie,
        |page| ready(&rows(page)),
    )
    .await;
    rows(&page)
}

/// A claim the upstream answered with `result` at `at`, as the service answers one, for the event's builder.
fn claimed(result: &str, outcome: phoebis_oauth::ClaimOutcome, at: i64) -> phoebis_oauth::Claim {
    phoebis_oauth::Claim {
        at,
        label: "Launch reset".into(),
        outcome,
        reply: phoebis_oauth::ClaimReply::Answered {
            answer: Some(phoebis_oauth::ClaimAnswer {
                cleared: vec![],
                cooldown_until: None,
                reason: None,
                resets_left: Some(0),
                result: result.into(),
                weekly_resets_at: None,
            }),
            status: 200,
        },
        request_id: "00000000-0000-4000-8000-0000000000a1".into(),
    }
}

/// `reset_grant` with no reset left, as a block reads one that was used.
fn spent_grant(id: &str) -> serde_json::Value {
    let mut grant = reset_grant(id);
    grant["resets_left"] = serde_json::json!(0);
    grant["usable_now"] = serde_json::json!(false);
    grant
}

#[tokio::test]
async fn a_claim_outlives_the_caller_that_went_away() {
    use common::{login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(
        0,
        CLAIM_PATH,
        fake_upstream::Respond::Gated(Box::new(claim_answer(r#"{"result":"reset"}"#))),
    );
    let caller = tokio::spawn({
        let (app, cookie) = (h.app.clone(), cookie.clone());
        async move { post(&app, USE_RESET, "", Some(&cookie)).await }
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (sent(&h.up, "POST /api/organizations/") == 1).then_some(())
    })
    .await;
    caller.abort();
    assert!(
        caller.await.unwrap_err().is_cancelled(),
        "the caller went away while the fake held the claim"
    );
    h.up.release();
    let rows = claimed_rows(&h.app, &cookie, |rows| !rows.is_empty()).await;
    let read: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row["detail"]["result"].as_str(),
                row["detail"]["outcome"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        read,
        vec![(Some("reset"), Some("used"))],
        "the claim the caller left still lands its answer's row: {rows:?}"
    );
}

#[tokio::test]
async fn a_stop_waits_for_a_claims_record() {
    use common::{login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(
        0,
        CLAIM_PATH,
        fake_upstream::Respond::Gated(Box::new(claim_answer(r#"{"result":"reset"}"#))),
    );
    tokio::spawn({
        let (app, cookie) = (h.app.clone(), cookie.clone());
        async move { post(&app, USE_RESET, "", Some(&cookie)).await }
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (sent(&h.up, "POST /api/organizations/") == 1).then_some(())
    })
    .await;
    let waits = tokio::select! {
        biased;
        () = h.claims.drain() => false,
        () = std::future::ready(()) => true,
    };
    assert!(waits, "a stop waits while the fake holds the claim");
    h.up.release();
    tokio::time::timeout(std::time::Duration::from_secs(5), h.claims.drain())
        .await
        .expect("the drain ends once the claim's event is handed to the ledger");
    // The stop goes on to the ledger, whose shutdown aborts the claim's task and lands what its writer was handed.
    h.ledger.shutdown().await.unwrap();
    let uses = h.db.read(phoebis_ledger::limit_reset_uses).await.unwrap();
    let block = h
        .store
        .account_extras_of("business")
        .unwrap()
        .and_then(|extras| extras.limit_resets);
    assert_eq!(
        (
            uses.iter()
                .map(|used| (used.account_id.as_str(), used.grant_id.as_str()))
                .collect::<Vec<_>>(),
            block.and_then(|block| block["grants"][0]["resets_left"].as_u64()),
        ),
        (vec![("business", "rg-b-1")], Some(0)),
        "the claim the stop waited for left its event and its fold"
    );
}

#[tokio::test]
async fn the_reading_after_a_claim_is_one_sent_after_it() {
    use common::{get, json, login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    let five_hour = |utilization: u32| fake_upstream::Respond::Json {
        status: 200,
        headers: vec![],
        body: format!(
            r#"{{"five_hour":{{"utilization":{utilization},"resets_at":"2026-10-11T15:00:00Z"}}}}"#
        )
        .into_bytes(),
    };
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Gated(Box::new(five_hour(100))),
    );
    let held = tokio::spawn({
        let usage = h.usage.clone();
        async move { usage.poll("business").await }
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (sent(&h.up, "GET /api/oauth/usage ") == 1).then_some(())
    })
    .await;
    h.up.set_rule_after(1, "/api/oauth/usage", five_hour(0));
    h.clock_cell
        .fetch_add(1_000, std::sync::atomic::Ordering::SeqCst);
    let used = tokio::spawn({
        let (app, cookie) = (h.app.clone(), cookie.clone());
        async move { post(&app, USE_RESET, "", Some(&cookie)).await }
    });
    // The claim's task writes its row before its poll, which then joins the held read.
    fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "limit_reset_claimed")
    })
    .await;
    h.up.release();
    let used = used.await.unwrap();
    assert_eq!(used.status(), 200);
    let used = json(used).await;
    held.await.unwrap().unwrap();
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let five_hour_now = account["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["id"] == "five_hour")
        .and_then(|window| window["utilization"].as_f64());
    assert_eq!(
        (
            used["outcome"].as_str(),
            sent(&h.up, "GET /api/oauth/usage "),
            five_hour_now
        ),
        (Some("used"), 2, Some(0.0)),
        "the reading the claim joined was sent before its answer, so a second one is the router's: {account}"
    );
}

#[tokio::test]
async fn a_used_reset_stays_used_when_every_read_after_it_fails() {
    use common::{get, json, login, post, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let mut two = reset_grant("rg-b-1");
    two["resets_left"] = serde_json::json!(2);
    two["resets_total"] = serde_json::json!(2);
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [two]}),
    );
    h.up.set_rule_after(0, "/api/oauth/usage", upstream_failure());
    h.up.set_rule_after(0, LIMIT_RESETS_PATH, upstream_failure());
    let used = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let status = used.status().as_u16();
    let used = json(used).await;
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        (
            status,
            used["limit_resets"]["grants"][0]["resets_left"].as_u64(),
            account["limit_resets"]["grants"][0]["resets_left"].as_u64()
        ),
        (200, Some(1), Some(1)),
        "a reset answer with no count takes one of two off, whatever the reads after it do: {used}"
    );
}

#[tokio::test]
async fn a_failed_read_of_an_open_outcome_is_due_in_the_next_slot() {
    use common::{login, post, CLAIM_PATH, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(0, CLAIM_PATH, fake_upstream::Respond::CloseAfterRequest);
    h.up.set_rule_after(0, LIMIT_RESETS_PATH, upstream_failure());
    let open = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let code = open
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    assert_eq!(
        (
            open.status().as_u16(),
            code.as_deref(),
            sent(&h.up, "GET /api/oauth/usage?cedar_ember=1")
        ),
        (502, Some("limit_reset_unanswered"), 1),
        "the open outcome's read went out and failed"
    );
    // `business` is the one polled account, so its first slot falls 300 s after the scheduler's start and reads no
    // profile. The hour has not passed since the failed read, and the slot's reading stands at the set that read
    // remembered, so only the due mark sends the block read.
    tokio::time::pause();
    let scheduler = phoebis_ledger::QuotaPoll {
        ledger: h.ledger.clone(),
        router: h.router.clone(),
        store: h.store.clone(),
        tables: std::sync::Arc::new(phoebis_ledger::PlanTables::shipped()),
        usage: h.usage.clone(),
    }
    .spawn();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    h.clock_cell
        .fetch_add(301_000, std::sync::atomic::Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::time::resume();
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (sent(&h.up, "GET /api/oauth/usage?cedar_ember=1") == 2).then_some(())
    })
    .await;
    scheduler.abort();
}

#[tokio::test]
async fn an_open_outcome_is_read_again_after_a_read_from_before_it() {
    use common::{get, json, login, post, CLAIM_PATH, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let usable = serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]});
    store_limit_resets(&h.store, usable.clone());
    h.up.set_rule_after(
        0,
        LIMIT_RESETS_PATH,
        fake_upstream::Respond::Gated(Box::new(limit_resets_answer(usable))),
    );
    let early = tokio::spawn({
        let usage = h.usage.clone();
        async move { usage.limit_resets("business").await }
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (sent(&h.up, "GET /api/oauth/usage?cedar_ember=1") == 1).then_some(())
    })
    .await;
    h.up.set_rule_after(
        1,
        LIMIT_RESETS_PATH,
        limit_resets_answer(
            serde_json::json!({"eligible": true, "grants": [spent_grant("rg-b-1")]}),
        ),
    );
    h.clock_cell
        .fetch_add(1_000, std::sync::atomic::Ordering::SeqCst);
    h.up.set_rule_after(0, CLAIM_PATH, fake_upstream::Respond::CloseAfterRequest);
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"five_hour":{"utilization":42.0,"resets_at":"2026-10-11T15:00:00Z"}}"#
                .to_vec(),
        },
    );
    let open = tokio::spawn({
        let (app, cookie) = (h.app.clone(), cookie.clone());
        async move { post(&app, USE_RESET, "", Some(&cookie)).await }
    });
    // The claim's task reports its poll's reading and runs on to the block read, which joins the held one.
    common::wait_json(&h.app, "/admin/accounts/business", &cookie, |account| {
        account["windows"].as_array().is_some_and(|windows| {
            windows
                .iter()
                .any(|w| w["id"] == "five_hour" && w["utilization"].as_f64() == Some(0.42))
        })
    })
    .await;
    h.up.release();
    let open = open.await.unwrap();
    let code = open
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    let status = open.status().as_u16();
    early.await.unwrap().unwrap();
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let grant = &account["limit_resets"]["grants"][0];
    assert_eq!(
        (
            status,
            code.as_deref(),
            grant["state"].as_str(),
            grant["resets_left"].as_u64()
        ),
        (502, Some("limit_reset_unanswered"), Some("used"), Some(0)),
        "the block the joined read sent before the claim is read again: {account}"
    );
}

#[tokio::test]
async fn a_failed_reading_after_a_claim_is_not_asked_again() {
    use common::{json, login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    // The rig polls nothing on its own, so the clean reading from before the claim is this poll's.
    h.usage.poll("business").await.unwrap();
    h.clock_cell
        .fetch_add(1_000, std::sync::atomic::Ordering::SeqCst);
    h.up.set_rule_after(0, "/api/oauth/usage", upstream_failure());
    let before = sent(&h.up, "GET /api/oauth/usage ");
    let used = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let status = used.status().as_u16();
    let used = json(used).await;
    assert_eq!(
        (
            status,
            used["outcome"].as_str(),
            sent(&h.up, "GET /api/oauth/usage ") - before
        ),
        (200, Some("used"), 1),
        "a failed reading keeps the instant of the one before the claim and is not asked again: {used}"
    );
}

#[tokio::test]
async fn the_use_route_sends_no_claim_to_a_plan_that_ended() {
    use common::{login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    assert!(h.store.set_plan_ended("business", common::NOW).unwrap());
    let refused = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let code = refused
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    assert_eq!(
        (
            refused.status().as_u16(),
            code.as_deref(),
            sent(&h.up, "POST /api/organizations/")
        ),
        (409, Some("limit_reset_not_polled"), 0),
        "the poll no longer asks an account whose plan ended, so the route sends it no claim"
    );
}

#[tokio::test]
async fn an_unsettled_use_sends_no_limit_resets_read_to_a_plan_that_just_ended() {
    use common::{login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(0, CLAIM_PATH, fake_upstream::Respond::CloseAfterRequest);
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 403,
            headers: vec![("x-should-retry".into(), "false".into())],
            body: br#"{"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization.","details":{"error_code":"oauth_not_allowed_for_organization","error_visibility":"user_facing"}}}"#
                .to_vec(),
        },
    );
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid","has_claude_max":false,"has_claude_pro":false},"organization":{"uuid":"o","organization_type":"claude_free","rate_limit_tier":"default_claude_ai","subscription_status":"canceled","billing_type":"none"}}"#
                .to_vec(),
        },
    );
    let open = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let code = open
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    let ended = h
        .store
        .subscription_term("business")
        .unwrap()
        .unwrap()
        .plan_ended_at;
    assert_eq!(
        (
            open.status().as_u16(),
            code.as_deref(),
            ended.is_some(),
            sent(&h.up, "GET /api/oauth/usage?cedar_ember=1")
        ),
        (502, Some("limit_reset_unanswered"), true, 0),
        "the claim's poll found the plan ended, so no limit resets read follows it"
    );
}

#[tokio::test]
async fn an_upstream_status_answers_by_the_claims_class() {
    use common::{login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    let answered = |status: u16| fake_upstream::Respond::Json {
        status,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"api_error","message":"Request failed"}}"#
            .to_vec(),
    };
    let code = |response: &axum::http::Response<axum::body::Body>| {
        response
            .headers()
            .get("x-phoebis-error-code")
            .map(|code| code.to_str().unwrap().to_string())
    };
    h.up.set_rule_after(0, CLAIM_PATH, answered(403));
    let refused = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    // A refused claim folds nothing, so the grant stays usable for the second claim.
    h.up.set_rule_after(0, CLAIM_PATH, answered(500));
    let open = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    assert_eq!(
        (
            refused.status().as_u16(),
            code(&refused).as_deref(),
            open.status().as_u16(),
            code(&open).as_deref()
        ),
        (
            502,
            Some("limit_reset_refused"),
            502,
            Some("limit_reset_unanswered")
        ),
        "a 403 refuses the claim and a 500 leaves it open, so each answers its class's code"
    );
}

#[tokio::test]
async fn a_use_sends_no_usage_read_to_a_plan_that_ended_under_the_claim() {
    use common::{json, login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(
        0,
        CLAIM_PATH,
        fake_upstream::Respond::Gated(Box::new(claim_answer(r#"{"result":"reset"}"#))),
    );
    let used = tokio::spawn({
        let (app, cookie) = (h.app.clone(), cookie.clone());
        async move { post(&app, USE_RESET, "", Some(&cookie)).await }
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (sent(&h.up, "POST /api/organizations/") == 1).then_some(())
    })
    .await;
    // A slot or a refresh that found the plan ended while the claim was out.
    assert!(h.store.set_plan_ended("business", common::NOW).unwrap());
    let before = sent(&h.up, "GET /api/oauth/usage");
    h.up.release();
    let used = used.await.unwrap();
    let status = used.status().as_u16();
    let used = json(used).await;
    assert_eq!(
        (
            before,
            status,
            used["outcome"].as_str(),
            sent(&h.up, "GET /api/oauth/usage")
        ),
        (0, 200, Some("used"), 0),
        "the claim's task sends no usage request to an account whose plan ended under the claim: {used}"
    );
}

#[tokio::test]
async fn a_refused_claim_leaves_its_row() {
    use common::{json, login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(0, CLAIM_PATH, claim_answer(r#"{"result":"cooldown"}"#));
    let refused = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let status = refused.status().as_u16();
    let refused = json(refused).await;
    assert_eq!(
        (status, refused["outcome"].as_str()),
        (200, Some("refused")),
        "{refused}"
    );
    let rows = claimed_rows(&h.app, &cookie, |rows| !rows.is_empty()).await;
    let read: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row["detail"]["result"].as_str(),
                row["detail"]["outcome"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        read,
        vec![(Some("cooldown"), Some("refused"))],
        "a refused claim reached the upstream, so it leaves its row: {rows:?}"
    );
}

#[tokio::test]
async fn an_unanswered_claim_leaves_its_row() {
    use common::{login, post, CLAIM_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    h.up.set_rule_after(0, CLAIM_PATH, fake_upstream::Respond::CloseAfterRequest);
    let open = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let code = open
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    assert_eq!(
        (open.status().as_u16(), code.as_deref()),
        (502, Some("limit_reset_unanswered"))
    );
    let request =
        h.up.requests()
            .into_iter()
            .find(|r| r.line.starts_with("POST /api/organizations/"))
            .expect("the claim reached the fake");
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    let rows = claimed_rows(&h.app, &cookie, |rows| !rows.is_empty()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let detail = &rows[0]["detail"];
    assert_eq!(
        (
            &detail["request_id"],
            &detail["result"],
            detail["outcome"].as_str(),
            &detail["status"],
            detail["error"].is_string(),
            &detail["cleared"]
        ),
        (
            &body["request_id"],
            &serde_json::Value::Null,
            Some("unsettled"),
            &serde_json::Value::Null,
            true,
            &serde_json::json!([])
        ),
        "a claim the fake never answered leaves its row with the id it sent: {detail}"
    );
}

#[tokio::test]
async fn the_use_route_reports_a_reauth_mark_its_claim_landed() {
    use common::{login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    revoke_on_refresh(&h);
    let refused = post(&h.app, USE_RESET, "", Some(&cookie)).await;
    let code = refused
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    assert_eq!(
        (
            refused.status().as_u16(),
            code.as_deref(),
            sent(&h.up, "POST /api/organizations/")
        ),
        (502, Some("limit_reset_not_sent"), 0),
        "the claim's refresh found the refresh token revoked, so nothing left"
    );
    routing_reads_reauth_required(&h.router).await;
}

/// The `revision` of `business`'s `limit_resets` member in `answer`, a read's or the account's own.
fn resets_revision(answer: &serde_json::Value) -> u64 {
    answer["limit_resets"]["revision"]
        .as_u64()
        .unwrap_or_else(|| panic!("the member carries its revision: {answer}"))
}

/// The use of `business`'s grant `rg-b-1` with `revision` as its `expected_revision`, answered by its status, its
/// code and the count of claims the fake has read since the rig started.
async fn use_naming(
    h: &common::Polled,
    cookie: &str,
    revision: u64,
) -> (u16, Option<String>, usize) {
    let body = format!(r#"{{"expected_revision":{revision}}}"#);
    let used = common::post(&h.app, USE_RESET, &body, Some(cookie)).await;
    let code = used
        .headers()
        .get("x-phoebis-error-code")
        .map(|code| code.to_str().unwrap().to_string());
    (
        used.status().as_u16(),
        code,
        sent(&h.up, "POST /api/organizations/"),
    )
}

#[tokio::test]
async fn a_use_that_names_an_earlier_accounts_revision_sends_nothing() {
    use common::{delete, get, json, login, post, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let usable = serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]});
    h.up.set_rule_after(0, LIMIT_RESETS_PATH, limit_resets_answer(usable.clone()));
    let read = "/admin/accounts/business/limit-resets/read";
    let earlier = resets_revision(&json(post(&h.app, read, "", Some(&cookie)).await).await);
    // The routes drop the caches a running router holds for the deleted account. The create route writes an unpaired
    // slot whose pairing would move the epoch, so the new account takes its identity, its bundle and its state from
    // the store, its block at the epoch 0 the first account's read left.
    assert_eq!(
        delete(&h.app, "/admin/accounts/business", Some(&cookie))
            .await
            .status(),
        204
    );
    let created = post(
        &h.app,
        "/admin/accounts",
        r#"{"id":"business","kind":"oauth","label":"Business"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    h.store
        .set_identity(
            "business",
            &phoebis_store::Identity {
                account_uuid: common::ACCOUNT_UUID.into(),
                email: "test@example.invalid".into(),
                organization_uuid: Some("00000000-0000-4000-8000-000000000002".into()),
            },
        )
        .unwrap();
    let incarnation = h.store.account("business").unwrap().unwrap().incarnation;
    h.store
        .commit_credentials(
            "business",
            incarnation,
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
    h.store
        .set_state(
            "business",
            phoebis_store::AccountState::Ready,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    store_limit_resets(&h.store, usable);
    let stale = use_naming(&h, &cookie, earlier).await;
    assert_eq!(
        stale,
        (409, Some("limit_reset_account_changed".to_string()), 0),
        "the click names the deleted account's member, so the new account's reset is not spent"
    );
    let current =
        resets_revision(&json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await);
    let used = use_naming(&h, &cookie, current).await;
    assert_eq!(
        (used.0, used.2),
        (200, 1),
        "a use that names the new account's member leaves"
    );
}

#[tokio::test]
async fn a_use_that_names_a_revision_from_before_a_pairing_sends_nothing() {
    use common::{json, login, post, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    h.up.set_rule_after(
        0,
        LIMIT_RESETS_PATH,
        limit_resets_answer(
            serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
        ),
    );
    let read = "/admin/accounts/business/limit-resets/read";
    let earlier = resets_revision(&json(post(&h.app, read, "", Some(&cookie)).await).await);
    let stored = h.store.credentials("business").unwrap().unwrap();
    h.store
        .commit_pairing(
            "business",
            1,
            1,
            Some(stored.generation),
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
    // The pairing cleared the block, and the next read stores the grant as usable again under the pairing's epoch.
    let again = json(post(&h.app, read, "", Some(&cookie)).await).await;
    assert_eq!(
        again["limit_resets"]["grants"][0]["state"], "usable",
        "{again}"
    );
    let stale = use_naming(&h, &cookie, earlier).await;
    assert_eq!(
        stale,
        (409, Some("limit_reset_account_changed".to_string()), 0),
        "the click names the member from before the pairing, so nothing leaves"
    );
}

#[tokio::test]
async fn a_use_whose_expected_revision_is_null_sends_nothing() {
    use common::{json, login, post, LIMIT_RESETS_PATH};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [reset_grant("rg-b-1")]}),
    );
    let refused = post(
        &h.app,
        USE_RESET,
        r#"{"expected_revision": null}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        (
            refused.status().as_u16(),
            sent(&h.up, "POST /api/organizations/")
        ),
        (400, 0),
        "a member the route cannot read names no account, so no claim leaves bound to the route's own read"
    );
    // The block read fails, so the read route answers the stored block, which a claim's fold would have moved.
    h.up.set_rule_after(0, LIMIT_RESETS_PATH, upstream_failure());
    let read = json(
        post(
            &h.app,
            "/admin/accounts/business/limit-resets/read",
            "",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let grant = &read["limit_resets"]["grants"][0];
    assert_eq!(
        (grant["state"].as_str(), grant["resets_left"].as_u64()),
        (Some("usable"), Some(1)),
        "the grant keeps its one reset: {read}"
    );
}

#[tokio::test]
async fn a_later_answer_never_moves_the_use() {
    use common::{get, json, login};
    use phoebis_oauth::ClaimOutcome;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [spent_grant("rg-b-1")]}),
    );
    for claim in [
        claimed("reset", ClaimOutcome::Used, common::NOW + 1_000),
        claimed("already_used", ClaimOutcome::Refused, common::NOW + 2_000),
    ] {
        h.ledger.record(phoebis_ledger::limit_reset_claimed(
            "business", "admin", "rg-b-1", &claim,
        ));
    }
    // The routing log reads up to the clock, which then stands at the later event.
    h.clock_cell
        .fetch_add(2_000, std::sync::atomic::Ordering::SeqCst);
    claimed_rows(&h.app, &cookie, |rows| rows.len() == 2).await;
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        account["limit_resets"]["grants"][0]["used_at"].as_i64(),
        Some(1_791_721_001_000),
        "an already used answer after the use leaves the use's instant: {account}"
    );
}

#[tokio::test]
async fn a_use_from_before_the_row_was_created_is_not_read() {
    use common::{get, json, login};
    use phoebis_oauth::ClaimOutcome;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    store_limit_resets(
        &h.store,
        serde_json::json!({"eligible": true, "grants": [spent_grant("rg-b-1"), spent_grant("rg-b-2")]}),
    );
    // `business` was created at the rig's clock.
    for (grant, at) in [("rg-b-1", common::NOW - 1), ("rg-b-2", common::NOW)] {
        h.ledger.record(phoebis_ledger::limit_reset_claimed(
            "business",
            "admin",
            grant,
            &claimed("reset", ClaimOutcome::Used, at),
        ));
    }
    claimed_rows(&h.app, &cookie, |rows| rows.len() == 2).await;
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    let grants = &account["limit_resets"]["grants"];
    assert_eq!(
        (&grants[0]["used_at"], &grants[1]["used_at"]),
        (
            &serde_json::Value::Null,
            &serde_json::json!(1_791_721_000_000_i64)
        ),
        "a use dated before the row was created is an earlier holder's: {account}"
    );
}

#[tokio::test]
async fn a_use_of_the_same_grant_on_another_account_is_not_read() {
    use common::{json, login, post, LIMIT_RESETS_PATH};
    use phoebis_oauth::ClaimOutcome;
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let spent = serde_json::json!({"eligible": true, "grants": [spent_grant("rg-b-1")]});
    store_limit_resets(&h.store, spent.clone());
    // The route reads the block before it answers, so the fake answers the stored one and the grant stays used.
    h.up.set_rule_after(0, LIMIT_RESETS_PATH, limit_resets_answer(spent));
    // The routing log reads up to the clock, which then stands at the later event.
    h.clock_cell
        .fetch_add(2_000, std::sync::atomic::Ordering::SeqCst);
    let read = "/admin/accounts/business/limit-resets/read";
    let used_at =
        |answer: &serde_json::Value| answer["limit_resets"]["grants"][0]["used_at"].clone();
    // A promotion gives its grant the same id on every account.
    h.ledger.record(phoebis_ledger::limit_reset_claimed(
        "other",
        "admin",
        "rg-b-1",
        &claimed("reset", ClaimOutcome::Used, common::NOW + 2_000),
    ));
    claimed_rows(&h.app, &cookie, |rows| rows.len() == 1).await;
    let elsewhere = json(post(&h.app, read, "", Some(&cookie)).await).await;
    h.ledger.record(phoebis_ledger::limit_reset_claimed(
        "business",
        "admin",
        "rg-b-1",
        &claimed("reset", ClaimOutcome::Used, common::NOW + 1_000),
    ));
    claimed_rows(&h.app, &cookie, |rows| rows.len() == 2).await;
    let own = json(post(&h.app, read, "", Some(&cookie)).await).await;
    assert_eq!(
        (used_at(&elsewhere), used_at(&own)),
        (
            serde_json::Value::Null,
            serde_json::json!(1_791_721_001_000_i64)
        ),
        "another account's use of the grant's id is not this account's: {elsewhere} {own}"
    );
}

/// A profile read that finds no plan stores the document it found, so a flagged account answers the free plan and
/// the found tier beside the flag's instant, where the last plan's document stood before the read.
#[tokio::test]
async fn a_plan_ended_account_answers_the_free_plan_and_the_found_tier_beside_its_flag() {
    use common::{get, json, login, post};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    h.store
        .set_profile(
            "business",
            r#"{"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_max_20x".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(h
        .store
        .set_plan_ended("business", common::NOW - 60_000)
        .unwrap());
    h.up.set_rule_after(
        0,
        "/api/oauth/profile",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"account":{"uuid":"00000000-0000-4000-8000-000000000100","email":"a@example.invalid","has_claude_max":false,"has_claude_pro":false},"organization":{"uuid":"o","organization_type":"claude_free","rate_limit_tier":"default_claude_ai","subscription_status":"canceled","billing_type":"none"}}"#
                .to_vec(),
        },
    );
    let refreshed = post(
        &h.app,
        "/admin/accounts/business/usage/refresh",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(refreshed.status(), 200);
    let account = json(get(&h.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        (
            &account["subscription"]["plan"],
            &account["subscription"]["tier"],
            &account["plan_ended_at"]
        ),
        (
            &serde_json::json!("free"),
            &serde_json::json!("default_claude_ai"),
            &serde_json::json!(common::NOW - 60_000)
        ),
        "the found document's plan and tier beside the flag: {account}"
    );
}

#[tokio::test]
async fn the_account_view_reads_the_extra_usage_flag_from_the_organization_alone() {
    use common::{get, json, login};
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    // A document can carry the flag under `account` as well, and the client reads the organization's, so the view
    // ignores the account's copy.
    store
        .set_profile(
            "business",
            r#"{"account":{"has_extra_usage_enabled":true},"organization":{"has_extra_usage_enabled":false}}"#,
            &phoebis_store::SubscriptionRow::default(),
        )
        .unwrap();
    let both = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    store
        .set_profile(
            "business",
            r#"{"account":{"has_extra_usage_enabled":true},"organization":{}}"#,
            &phoebis_store::SubscriptionRow::default(),
        )
        .unwrap();
    let account_only = json(get(&app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        (
            both["profile"].get("has_extra_usage_enabled"),
            account_only["profile"].get("has_extra_usage_enabled"),
        ),
        (
            Some(&serde_json::json!(false)),
            Some(&serde_json::Value::Null)
        ),
        "the organization's flag answers over the account's, and a flag under the account alone answers null"
    );
}

#[tokio::test]
async fn the_account_view_answers_the_plan_the_profile_resolves_to() {
    use common::{get, json, login};
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    for (id, label) in [("seat", "Seat"), ("unpaired", "Unpaired")] {
        store
            .insert_account(&phoebis_store::AccountRow {
                label: label.into(),
                state_cause: phoebis_store::StateCause::Paired,
                ..phoebis_store::AccountRow::for_tests(id, phoebis_store::AccountKind::Oauth)
            })
            .unwrap();
    }
    // A free organization reports the Pro tier and neither plan flag, so the type alone names it.
    store
        .set_profile(
            "business",
            r#"{"account":{"has_claude_max":false,"has_claude_pro":false},"organization":{"organization_type":"claude_free","rate_limit_tier":"default_claude_ai"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_ai".into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    store
        .set_profile(
            "seat",
            r#"{"organization":{"organization_type":"claude_enterprise","rate_limit_tier":"default_claude_zero"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_zero".into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    // Canceled before it ever paired, so the view answers a subscription with no profile behind it.
    store
        .set_canceled(
            "unpaired",
            phoebis_store::Cancellation::On {
                ends_at: Some(common::NOW + 86_400_000),
            },
        )
        .unwrap();
    let mut plans = Vec::new();
    for id in ["business", "seat", "unpaired"] {
        let view = json(get(&app, &format!("/admin/accounts/{id}"), Some(&cookie)).await).await;
        assert!(view["subscription"].is_object(), "{id}: {view}");
        plans.push(view["subscription"]["plan"].clone());
    }
    assert_eq!(
        plans,
        [
            serde_json::json!("free"),
            serde_json::json!("usage_based"),
            serde_json::Value::Null,
        ],
        "the free organization reads free, the usage-based seat its class, and an account with no profile null"
    );
}

#[tokio::test]
async fn a_usage_based_account_answers_its_class_and_no_overage_whatever_a_head_or_an_older_row_says(
) {
    use common::{get, json, login};
    use phoebis_ledger::ExtraUsageSampleRecord;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    parts
        .store
        .insert_account(&phoebis_store::AccountRow {
            label: "Seat".into(),
            state_cause: phoebis_store::StateCause::Paired,
            ..phoebis_store::AccountRow::for_tests("seat", phoebis_store::AccountKind::Oauth)
        })
        .unwrap();
    parts
        .store
        .set_profile(
            "seat",
            r#"{"organization":{"rate_limit_tier":"default_claude_zero","seat_tier":"enterprise_usage_based"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_zero".into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    parts.router.reload().await.unwrap();
    // The overage family of a head as a leg reports it for a plan seat, on the seat and on the plan account alike,
    // and a head row from before the class whose flag reads in use.
    let bucket = phoebis_ledger::bucket_of(now - 60_000);
    for account in ["seat", "business"] {
        parts
            .router
            .report(phoebis_routing::Report::Overage {
                account_id: account.into(),
                evidence: phoebis_routing::OverageEvidence {
                    in_use: true,
                    since: Some(now - 60_000),
                    reset: Some(now + 3_600_000),
                    observed_at: now - 60_000,
                    source: phoebis_routing::EvidenceSource::Head,
                    incarnation: 1,
                },
                incarnation: 1,
            })
            .await;
        parts
            .db
            .insert_samples(
                &[],
                Some(&ExtraUsageSampleRecord {
                    account_id: account.into(),
                    bucket,
                    is_enabled: true,
                    in_use: Some(true),
                    monthly_limit: None,
                    used_credits: None,
                    utilization: Some(0.4),
                    currency: None,
                    disabled_reason: None,
                    overage_reset: Some(now + 3_600_000),
                    source: phoebis_ledger::SampleSource::Head,
                    spend_limit_minor: None,
                    spend_used_minor: None,
                    spend_percent: None,
                    spend_currency: None,
                    spend_exponent: None,
                }),
            )
            .unwrap();
    }
    // A query queues behind the reports, so the views below read the folded state.
    parts.router.overages().await.unwrap();
    let listed = json(get(&parts.app, "/admin/accounts", Some(&cookie)).await).await;
    let row = |id: &str| {
        listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} row: {listed}"))
    };
    let seat = row("seat");
    assert_eq!(
        (
            &seat["usage_based"],
            &seat["overage"],
            &seat["extra_usage"]["in_use"]
        ),
        (
            &serde_json::json!(true),
            &serde_json::Value::Null,
            &serde_json::Value::Null
        ),
        "the seat is classed and never reads as in overage: {seat}"
    );
    assert_eq!(
        seat["extra_usage"]["utilization"],
        serde_json::json!(0.4),
        "the row's share still answers: {seat}"
    );
    let plan = row("business");
    assert_eq!(
        (
            &plan["usage_based"],
            &plan["overage"]["in_use"],
            &plan["extra_usage"]["in_use"]
        ),
        (
            &serde_json::json!(false),
            &serde_json::json!(true),
            &serde_json::json!(true)
        ),
        "a plan account keeps its evidence and its row's flag: {plan}"
    );
    assert_eq!(
        row("direct")["usage_based"],
        serde_json::json!(false),
        "an api_key account is never classed"
    );
    let shown = json(get(&parts.app, "/admin/accounts/seat", Some(&cookie)).await).await;
    assert_eq!(
        (&shown["usage_based"], &shown["overage"]),
        (&serde_json::json!(true), &serde_json::Value::Null),
        "the single account answers as its row does: {shown}"
    );
}

/// Inserts the paired OAuth account `id`, a usage-based seat by its stored profile where `seat` holds. The router
/// reads the class at its next reload.
fn insert_oauth(parts: &common::Full, id: &str, seat: bool) {
    parts
        .store
        .insert_account(&phoebis_store::AccountRow {
            state_cause: phoebis_store::StateCause::Paired,
            ..phoebis_store::AccountRow::for_tests(id, phoebis_store::AccountKind::Oauth)
        })
        .unwrap();
    if seat {
        parts
            .store
            .set_profile(
                id,
                r#"{"organization":{"rate_limit_tier":"default_claude_zero","seat_tier":"enterprise_usage_based"}}"#,
                &phoebis_store::SubscriptionRow {
                    tier: Some("default_claude_zero".into()),
                    ..phoebis_store::SubscriptionRow::default()
                },
            )
            .unwrap();
    }
}

/// A poll's extra-usage row of `account` in the bucket of `at`: the `extra_usage` amounts and share as the poll
/// stores them, and the body's `spend` object as `(limit_minor, used_minor, percent)` where it carried one.
fn polled_row(
    account: &str,
    at: i64,
    amounts: (i64, i64),
    utilization: f64,
    spend: Option<(Option<i64>, i64, Option<f64>)>,
) -> phoebis_ledger::ExtraUsageSampleRecord {
    phoebis_ledger::ExtraUsageSampleRecord {
        account_id: account.into(),
        bucket: phoebis_ledger::bucket_of(at),
        is_enabled: true,
        in_use: None,
        monthly_limit: Some(amounts.0),
        used_credits: Some(amounts.1),
        utilization: Some(utilization),
        currency: Some("USD".into()),
        disabled_reason: None,
        overage_reset: None,
        source: phoebis_ledger::SampleSource::Poll,
        spend_limit_minor: spend.and_then(|(limit, _, _)| limit),
        spend_used_minor: spend.map(|(_, used, _)| used),
        spend_percent: spend.and_then(|(_, _, percent)| percent),
        spend_currency: spend.map(|_| "USD".into()),
        spend_exponent: spend.map(|_| 2),
    }
}

/// A head's extra-usage row of `account` in the bucket of `at`, its flag as the head named it.
fn head_row(
    account: &str,
    at: i64,
    in_use: Option<bool>,
    utilization: f64,
    reset: i64,
) -> phoebis_ledger::ExtraUsageSampleRecord {
    phoebis_ledger::ExtraUsageSampleRecord {
        account_id: account.into(),
        bucket: phoebis_ledger::bucket_of(at),
        is_enabled: true,
        in_use,
        utilization: Some(utilization),
        overage_reset: Some(reset),
        source: phoebis_ledger::SampleSource::Head,
        ..Default::default()
    }
}

/// `id` as `GET /admin/accounts/{id}` answers it.
async fn account_of(app: &axum::Router, cookie: &str, id: &str) -> serde_json::Value {
    common::json(common::get(app, &format!("/admin/accounts/{id}"), Some(cookie)).await).await
}

/// `id`'s row of the account list.
async fn listed_row(app: &axum::Router, cookie: &str, id: &str) -> serde_json::Value {
    let listed = common::json(common::get(app, "/admin/accounts", Some(cookie)).await).await;
    listed["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no {id} row: {listed}"))
}

#[tokio::test]
async fn a_usage_based_account_answers_its_spend_window_with_the_polled_figures_under_a_newer_heads_share_and_reset(
) {
    use common::{get, json, login, month_end, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    insert_oauth(&parts, "seat", true);
    parts.router.reload().await.unwrap();
    let (polled_at, headed_at, reset) = (NOW - 600_000, NOW - 60_000, NOW + 3_600_000);
    // The amounts beside the object are a minor unit apart from it, so each amount names the column it came from.
    let rows = [
        polled_row(
            "seat",
            polled_at,
            (250_001, 102_501),
            0.41,
            Some((Some(250_000), 102_500, Some(41.0))),
        ),
        head_row("seat", headed_at, None, 0.42, reset),
    ];
    for row in &rows {
        parts.db.insert_samples(&[], Some(row)).unwrap();
    }
    parts
        .router
        .report(phoebis_routing::Report::SpendHead {
            account_id: "seat".into(),
            utilization: Some(0.42),
            reset: Some(reset),
            observed_at: headed_at,
            incarnation: 1,
        })
        .await;
    parts.router.spend_windows().await.unwrap();
    let polled_bucket = phoebis_ledger::bucket_of(polled_at) * 1_000;
    let headed_bucket = phoebis_ledger::bucket_of(headed_at) * 1_000;
    let amounts = serde_json::json!({
        "currency": "USD", "exponent": 2, "limit_minor": 250_000, "used_minor": 102_500, "polled_at": polled_bucket
    });
    let seat = json(get(&parts.app, "/admin/accounts/seat", Some(&cookie)).await).await;
    assert_eq!(
        seat["spend"],
        serde_json::json!({
            "amounts": amounts, "utilization": 0.42, "resets_at": reset, "reset_source": "head",
            "observed_at": headed_bucket
        }),
        "the polled row's spend figures under the newer head's share and reset: {seat}"
    );
    assert_eq!(
        seat["extra_usage"],
        serde_json::json!({
            "currency": "USD", "disabled_reason": null, "in_use": null, "is_enabled": true,
            "monthly_limit": 250_001, "used_credits": 102_501, "utilization": 0.42, "observed_at": headed_bucket
        }),
        "the amounts come from the polled row and the rest from the newest: {seat}"
    );
    assert_eq!(
        listed_row(&parts.app, &cookie, "seat").await["spend"],
        seat["spend"],
        "the list answers the seat as its own read does"
    );

    // The writer lands the head's credits row after the read above returns, so the rebuild waits for it.
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        parts
            .store
            .load_routing()
            .unwrap()
            .credits
            .into_iter()
            .find(|row| row.account_id == "seat" && row.observed_at == headed_at)
    })
    .await;
    // Built again over the same store, with the same rows in its ledger: the router's memory of the head's reset is
    // gone, so the reset reads the estimate. Past the head's reset the ledger's head row has lapsed, so the share that
    // stands is the one the rebuilt router loaded from the credits row, which lapses at the estimate over its instant.
    let again = common::admin_parts_full_on(parts.store.clone()).await;
    for row in &rows {
        again.db.insert_samples(&[], Some(row)).unwrap();
    }
    common::step(
        &again,
        std::time::Duration::from_millis((reset - NOW + 1) as u64),
    );
    let cookie_again = login(&again.app, "admin-secret").await;
    let restarted = json(get(&again.app, "/admin/accounts/seat", Some(&cookie_again)).await).await;
    assert_eq!(
        restarted["spend"],
        serde_json::json!({
            "amounts": amounts, "utilization": 0.42, "resets_at": month_end(), "reset_source": "estimated",
            "observed_at": headed_bucket
        }),
        "after a restart and a step past the head's reset the amounts and the loaded share stand, the reset the \
         estimate: {restarted}"
    );
    assert_eq!(
        again
            .router
            .spend_windows()
            .await
            .unwrap()
            .into_iter()
            .map(|(id, window)| (id, window.reset_source))
            .collect::<Vec<_>>(),
        [(
            "seat".to_string(),
            phoebis_routing::SpendResetSource::Estimated
        )],
        "the estimate is the restarted router's own window, not the view's fallback"
    );

    common::step(
        &parts,
        std::time::Duration::from_millis((reset - NOW + 1) as u64),
    );
    // The step passes the session's idle deadline, so the case logs in again.
    let cookie = login(&parts.app, "admin-secret").await;
    let lapsed = json(get(&parts.app, "/admin/accounts/seat", Some(&cookie)).await).await;
    assert_eq!(
        lapsed["spend"],
        serde_json::json!({
            "amounts": amounts, "utilization": null, "resets_at": month_end(), "reset_source": "estimated",
            "observed_at": headed_bucket
        }),
        "past the head's reset with no newer row the share lapses and the amounts stay: {lapsed}"
    );
}

/// Writes the ledger rows of the seat `id` and reports its router readings, answering the reset the head names: a head
/// twenty minutes before `NOW` naming the reset an hour after it, then a poll ten minutes before `NOW` carrying the
/// share 0.41. The poll's row lands in a later bucket than the head's, so the ledger's share read takes the poll's row,
/// which names no reset and so lapses at the estimate over its bucket start.
async fn poll_after_a_held_head(parts: &common::Full, id: &str) -> i64 {
    let (headed_at, polled_at, reset) = (
        common::NOW - 1_200_000,
        common::NOW - 600_000,
        common::NOW + 3_600_000,
    );
    for row in [
        head_row(id, headed_at, None, 0.42, reset),
        polled_row(id, polled_at, (250_000, 102_500), 0.41, None),
    ] {
        parts.db.insert_samples(&[], Some(&row)).unwrap();
    }
    parts
        .router
        .report(phoebis_routing::Report::SpendHead {
            account_id: id.into(),
            utilization: Some(0.42),
            reset: Some(reset),
            observed_at: headed_at,
            incarnation: 1,
        })
        .await;
    // Each read folds the readings pending before it, so the head lands before the poll whatever their keys' order.
    parts.router.spend_windows().await.unwrap();
    parts
        .router
        .report(phoebis_routing::Report::Credits {
            account_id: id.into(),
            state: phoebis_routing::CreditsState {
                enabled: true,
                disabled_reason: None,
                utilization: Some(0.41),
                observed_at: polled_at,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    parts.router.spend_windows().await.unwrap();
    reset
}

/// The share and the reset of `account`'s spend window.
fn spend_share_and_reset(account: &serde_json::Value) -> serde_json::Value {
    serde_json::json!([
        account["spend"]["utilization"],
        account["spend"]["resets_at"],
        account["spend"]["reset_source"]
    ])
}

#[tokio::test]
async fn a_usage_based_accounts_spend_share_lapses_at_the_held_head_reset_the_router_read_a_later_poll_under(
) {
    use common::{login, month_end, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    insert_oauth(&parts, "seat", true);
    parts.router.reload().await.unwrap();
    let reset = poll_after_a_held_head(&parts, "seat").await;
    let held = account_of(&parts.app, &cookie, "seat").await;
    assert_eq!(
        spend_share_and_reset(&held),
        serde_json::json!([0.41, reset, "head"]),
        "the poll's share under the head's reset while that is ahead: {held}"
    );
    common::step(
        &parts,
        std::time::Duration::from_millis((reset - NOW + 1) as u64),
    );
    // The step passes the session's idle deadline, so the case logs in again.
    let cookie = login(&parts.app, "admin-secret").await;
    let lapsed = account_of(&parts.app, &cookie, "seat").await;
    assert_eq!(
        spend_share_and_reset(&lapsed),
        serde_json::json!([null, month_end(), "estimated"]),
        "the router read the poll under the head's reset, so the share lapses there, where the ledger's poll row \
         would hold it to the estimate: {lapsed}"
    );
}

#[tokio::test]
async fn a_usage_based_accounts_spend_reads_the_ledger_rows_share_where_the_router_answers_no_window_or_holds_no_reading_of_the_seat(
) {
    use common::{login, month_end, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for seat in ["held", "unread"] {
        insert_oauth(&parts, seat, true);
    }
    parts.router.reload().await.unwrap();
    let reset = poll_after_a_held_head(&parts, "held").await;
    parts
        .db
        .insert_samples(
            &[],
            Some(&polled_row(
                "unread",
                NOW - 600_000,
                (300_000, 75_000),
                0.25,
                None,
            )),
        )
        .unwrap();
    let unread = account_of(&parts.app, &cookie, "unread").await;
    assert_eq!(
        spend_share_and_reset(&unread),
        serde_json::json!([0.25, month_end(), "estimated"]),
        "the running router holds no reading of the seat, so the share is the ledger row's: {unread}"
    );
    common::step(
        &parts,
        std::time::Duration::from_millis((reset - NOW + 1) as u64),
    );
    parts.router.shutdown().await.unwrap();
    let cookie = login(&parts.app, "admin-secret").await;
    let stopped = account_of(&parts.app, &cookie, "held").await;
    assert_eq!(
        spend_share_and_reset(&stopped),
        serde_json::json!([0.41, month_end(), "estimated"]),
        "a stopped router answers no window, so the share is the ledger's poll row's, lapsing at the estimate over \
         its bucket start: {stopped}"
    );
}

#[tokio::test]
async fn a_usage_based_accounts_spend_falls_back_to_the_polls_amounts_reads_no_limit_the_body_set_none_and_none_without_a_poll(
) {
    use common::{login, month_end, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for seat in ["amounts", "object", "open", "unpolled"] {
        insert_oauth(&parts, seat, true);
    }
    parts.router.reload().await.unwrap();
    let at = NOW - 120_000;
    let bucket = phoebis_ledger::bucket_of(at) * 1_000;
    for row in [
        polled_row("amounts", at, (250_000, 102_500), 0.41, None),
        polled_row(
            "object",
            at,
            (250_001, 103_125),
            0.4125,
            Some((Some(250_000), 102_500, Some(41.0))),
        ),
        polled_row(
            "open",
            at,
            (300_000, 5_000),
            0.0167,
            Some((None, 5_000, None)),
        ),
        head_row("unpolled", at, None, 0.3, NOW + 3_600_000),
    ] {
        parts.db.insert_samples(&[], Some(&row)).unwrap();
    }
    let shown = |id: &'static str| account_of(&parts.app, &cookie, id);
    assert_eq!(
        shown("amounts").await["spend"],
        serde_json::json!({
            "amounts": {"currency": "USD", "exponent": 2, "limit_minor": 250_000, "used_minor": 102_500,
                        "polled_at": bucket},
            "utilization": 0.41, "resets_at": month_end(), "reset_source": "estimated", "observed_at": bucket
        }),
        "a poll with no spend object answers its extra-usage amounts and share, the reset estimated before any head"
    );
    let object = shown("object").await;
    assert_eq!(
        (
            &object["spend"]["amounts"]["used_minor"],
            &object["spend"]["amounts"]["limit_minor"],
            &object["spend"]["utilization"]
        ),
        (
            &serde_json::json!(102_500),
            &serde_json::json!(250_000),
            &serde_json::json!(0.4125)
        ),
        "the object's amounts win over the extra-usage ones, and the share is the row's stored one, which the \
         router's hold reads too, never the object's rounded percent: {object}"
    );
    let open = shown("open").await;
    assert_eq!(
        (
            &open["spend"]["amounts"]["limit_minor"],
            &open["spend"]["amounts"]["used_minor"]
        ),
        (&serde_json::Value::Null, &serde_json::json!(5_000)),
        "a body that sets no limit answers the spend alone: {open}"
    );
    assert_eq!(
        open["extra_usage"]["monthly_limit"],
        serde_json::json!(300_000),
        "whatever limit the extra-usage row carries"
    );
    let unpolled = shown("unpolled").await;
    assert_eq!(
        (
            &unpolled["spend"]["amounts"],
            &unpolled["spend"]["utilization"]
        ),
        (&serde_json::Value::Null, &serde_json::json!(0.3)),
        "with no polled row of the day the head's share stands and no amounts answer: {unpolled}"
    );
    assert_eq!(
        (
            &unpolled["extra_usage"]["monthly_limit"],
            &unpolled["extra_usage"]["used_credits"],
            &unpolled["extra_usage"]["currency"]
        ),
        (
            &serde_json::Value::Null,
            &serde_json::Value::Null,
            &serde_json::Value::Null
        ),
        "nor does the extra usage: {unpolled}"
    );
}

/// A head row that names the overage reset alone, its share unread.
fn reset_only_head_row(
    account: &str,
    at: i64,
    reset: i64,
) -> phoebis_ledger::ExtraUsageSampleRecord {
    phoebis_ledger::ExtraUsageSampleRecord {
        utilization: None,
        ..head_row(account, at, None, 0.0, reset)
    }
}

#[tokio::test]
async fn a_head_row_that_names_the_reset_alone_hides_no_earlier_share_from_the_spend_window_or_the_extra_usage(
) {
    use common::{login, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for seat in ["capped", "headed"] {
        insert_oauth(&parts, seat, true);
    }
    parts.router.reload().await.unwrap();
    let reset = NOW + 3_600_000;
    for row in [
        polled_row("capped", NOW - 600_000, (300_000, 300_000), 1.0, None),
        reset_only_head_row("capped", NOW - 60_000, reset),
        polled_row("headed", NOW - 1_200_000, (300_000, 150_000), 0.5, None),
        head_row("headed", NOW - 600_000, None, 0.9, reset),
        reset_only_head_row("headed", NOW - 60_000, reset),
    ] {
        parts.db.insert_samples(&[], Some(&row)).unwrap();
    }
    for (seat, share, which) in [
        ("capped", 1.0, "the poll's share at one"),
        ("headed", 0.9, "the head's share, newer than the poll's"),
    ] {
        let account = account_of(&parts.app, &cookie, seat).await;
        assert_eq!(
            (
                &account["spend"]["utilization"],
                &account["extra_usage"]["utilization"],
                &account["spend"]["observed_at"]
            ),
            (
                &serde_json::json!(share),
                &serde_json::json!(share),
                &serde_json::json!(phoebis_ledger::bucket_of(NOW - 60_000) * 1_000)
            ),
            "{seat}: {which} stands under the newer reset-only head, which still dates the window: {account}"
        );
    }
}

#[tokio::test]
async fn a_head_row_naming_the_reset_alone_keeps_the_share_of_a_head_at_the_cap_in_its_bucket() {
    use common::{login, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    insert_oauth(&parts, "seat", true);
    parts.router.reload().await.unwrap();
    let reset = NOW + 3_600_000;
    // The head at the cap and the reset-only head after it share one bucket, the poll an earlier one.
    let bucket_start = phoebis_ledger::bucket_of(NOW - 60_000) * 1_000;
    for row in [
        polled_row("seat", NOW - 600_000, (300_000, 75_000), 0.25, None),
        head_row("seat", bucket_start, None, 1.0, reset),
        reset_only_head_row("seat", NOW - 60_000, reset),
    ] {
        parts.db.insert_samples(&[], Some(&row)).unwrap();
    }
    let account = account_of(&parts.app, &cookie, "seat").await;
    assert_eq!(
        (
            &account["spend"]["utilization"],
            &account["extra_usage"]["utilization"]
        ),
        (&serde_json::json!(1.0), &serde_json::json!(1.0)),
        "the head's share at one stands under the reset-only head of its bucket: {account}"
    );
}

#[tokio::test]
async fn a_usage_based_accounts_spend_reads_the_estimate_where_the_router_answers_no_window() {
    use common::{login, month_end, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    insert_oauth(&parts, "seat", true);
    parts.router.reload().await.unwrap();
    let reset = NOW + 3_600_000;
    parts
        .db
        .insert_samples(&[], Some(&head_row("seat", NOW - 60_000, None, 0.4, reset)))
        .unwrap();
    parts
        .router
        .report(phoebis_routing::Report::SpendHead {
            account_id: "seat".into(),
            utilization: Some(0.4),
            reset: Some(reset),
            observed_at: NOW - 60_000,
            incarnation: 1,
        })
        .await;
    parts.router.spend_windows().await.unwrap();
    let reset_of = |account: &serde_json::Value| {
        (
            account["spend"]["resets_at"].clone(),
            account["spend"]["reset_source"].clone(),
        )
    };
    let held = account_of(&parts.app, &cookie, "seat").await;
    assert_eq!(
        reset_of(&held),
        (serde_json::json!(reset), serde_json::json!("head")),
        "the running router answers the head's reset: {held}"
    );
    parts.router.shutdown().await.unwrap();
    let stopped = account_of(&parts.app, &cookie, "seat").await;
    assert_eq!(
        reset_of(&stopped),
        (serde_json::json!(month_end()), serde_json::json!("estimated")),
        "a stopped router answers no window, so the view reads the estimate over its own clock: {stopped}"
    );
    assert_eq!(
        stopped["spend"]["utilization"],
        serde_json::json!(0.4),
        "a stopped router answers no window, so the share is the ledger row's"
    );
}

#[tokio::test]
async fn a_plan_accounts_spend_is_null_and_its_extra_usage_keeps_the_polls_amounts_under_a_newer_head_row(
) {
    use common::{get, json, login, NOW};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let headed_at = NOW - 60_000;
    for row in [
        polled_row("business", NOW - 600_000, (5_000, 1_234), 0.25, None),
        head_row("business", headed_at, Some(true), 0.4, NOW + 3_600_000),
    ] {
        parts.db.insert_samples(&[], Some(&row)).unwrap();
    }
    let business = json(get(&parts.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        business["extra_usage"],
        serde_json::json!({
            "currency": "USD", "disabled_reason": null, "in_use": true, "is_enabled": true,
            "monthly_limit": 5_000, "used_credits": 1_234, "utilization": 0.4,
            "observed_at": phoebis_ledger::bucket_of(headed_at) * 1_000
        }),
        "the head row answers the flag and the share while the poll's amounts stand: {business}"
    );
    assert_eq!(
        business["spend"],
        serde_json::Value::Null,
        "a plan account has no spend window"
    );
    let direct = json(get(&parts.app, "/admin/accounts/direct", Some(&cookie)).await).await;
    assert_eq!(direct["spend"], serde_json::Value::Null, "{direct}");
}

#[tokio::test]
async fn the_account_view_answers_the_fable_week_under_one_id_from_a_head_row_a_poll_row_and_a_held_reading_the_newest_winning(
) {
    use common::{login, NOW};
    use phoebis_ledger::QuotaSampleRecord;
    use phoebis_routing::{
        Report, UtilizationSample, WindowReading, WindowSource, SEVEN_DAY_FABLE,
    };
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for id in ["held", "headed", "polled"] {
        insert_oauth(&parts, id, false);
    }
    parts.router.reload().await.unwrap();
    let quota = |account: &str, window: &str, utilization: f64, at: i64| QuotaSampleRecord {
        account_id: account.into(),
        bucket: phoebis_ledger::bucket_of(at),
        window: window.into(),
        utilization: Some(utilization),
        reset_at: Some(NOW + 86_400_000),
        source: if window == "seven_day_fable" {
            phoebis_ledger::SampleSource::Poll
        } else {
            phoebis_ledger::SampleSource::Head
        },
        observed_at: at,
    };
    let held = |account: &str, utilization: f64, at: i64| Report::Sample {
        account_id: account.into(),
        sample: UtilizationSample::default().with(
            SEVEN_DAY_FABLE,
            WindowReading {
                utilization,
                reset: Some(NOW + 86_400_000),
                observed_at: at,
                source: WindowSource::Poll,
                plan: None,
                tier: None,
            },
        ),
        incarnation: 1,
        source_request_id: None,
    };
    parts
        .db
        .insert_samples(
            &[
                quota("held", "seven_day_overage_included", 0.31, NOW - 1_200_000),
                quota("headed", "seven_day_overage_included", 0.42, NOW - 60_000),
                quota(
                    "polled",
                    "seven_day_overage_included",
                    0.51,
                    NOW - 1_200_000,
                ),
                quota("polled", "seven_day_fable", 0.53, NOW - 60_000),
            ],
            None,
        )
        .unwrap();
    parts.router.report(held("held", 0.33, NOW - 30_000)).await;
    parts
        .router
        .report(held("headed", 0.44, NOW - 1_800_000))
        .await;
    parts.router.samples().await.unwrap();
    for (id, utilization, newest) in [
        ("held", 0.33, "the router's held reading"),
        ("headed", 0.42, "the head row's overage-included week"),
        ("polled", 0.53, "the poll row's Fable week"),
    ] {
        let account = account_of(&parts.app, &cookie, id).await;
        let fable: Vec<(&str, f64)> = account["windows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["id"] != "five_hour" && w["id"] != "seven_day")
            .map(|w| {
                (
                    w["id"].as_str().unwrap(),
                    w["utilization"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            fable,
            [("seven_day_fable", utilization)],
            "{id}: one Fable-week row, {newest} the newest: {account}"
        );
    }
}

/// An attempt answers the class the ledger stored for it, true on a usage-based seat's attempt and false on a plan
/// account's, on the request list and on the request's own answer alike.
#[tokio::test]
async fn an_attempt_on_a_usage_based_seat_answers_its_class_and_one_on_a_plan_account_answers_false(
) {
    use common::{get, json, login, wait_json};
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let served = |id: &str, account: &str, usage_based: bool| Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: now - 1_000,
            finished_at: Some(now - 500),
            pool_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: phoebis_store::AccountKind::Oauth,
            started_at: now - 1_000,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            usage_based,
            ..Default::default()
        }],
        bodies: None,
    };
    parts.ledger.record(served("r-seat", "seat", true));
    parts.ledger.record(served("r-plan", "business", false));
    let listed = wait_json(&parts.app, "/admin/requests", &cookie, |v| {
        v["requests"].as_array().is_some_and(|rows| rows.len() >= 2)
    })
    .await;
    let class_of = |id: &str| {
        listed["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .map(|r| r["attempts"][0]["usage_based"].clone())
            .unwrap_or_else(|| panic!("no {id} row: {listed}"))
    };
    assert_eq!(
        (class_of("r-seat"), class_of("r-plan")),
        (serde_json::json!(true), serde_json::json!(false)),
        "each attempt answers the class its row stored: {listed}"
    );
    let shown = json(get(&parts.app, "/admin/requests/r-seat", Some(&cookie)).await).await;
    assert_eq!(
        shown["request"]["attempts"][0]["usage_based"],
        serde_json::json!(true),
        "the request's own answer carries the member as the list does: {shown}"
    );
}

/// The request log narrows on the serving attempt's stop reason and refusal category, and a kept row answers both
/// words off its attempt's response, the row itself carrying neither. The log also narrows on whether the row
/// carries a fallback credit's digest.
#[tokio::test]
async fn the_request_list_filters_on_the_serving_attempts_stop_reason_and_refusal_category() {
    use common::{get, json, login, wait_json};
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let served = |id: &str, at: i64, answer: Option<(&str, Option<&str>)>| Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            pool_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(if answer.is_some() { 200 } else { 503 }),
            outcome: if answer.is_some() {
                "succeeded"
            } else {
                "exhausted"
            }
            .into(),
            attempts: answer.is_some() as u32,
            ..Default::default()
        },
        attempts: answer
            .map(|(stop_reason, category)| AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "business".into(),
                account_kind: phoebis_store::AccountKind::Oauth,
                started_at: at,
                outcome: "succeeded".into(),
                stop_reason: Some(stop_reason.into()),
                refusal_category: category.map(String::from),
                ..Default::default()
            })
            .into_iter()
            .collect(),
        bodies: None,
    };
    parts.ledger.record(served(
        "r-harmful",
        now - 4_000,
        Some(("refusal", Some("harmful_request"))),
    ));
    parts
        .ledger
        .record(served("r-bare", now - 3_000, Some(("refusal", None))));
    // The answer after the refusals presented a fallback credit, a made-up digest in the column's form.
    let Record::Request {
        request,
        attempts,
        bodies,
    } = served("r-ended", now - 2_000, Some(("end_turn", None)))
    else {
        unreachable!("served writes a request")
    };
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            fallback_digest: Some(
                "3b7e1f9a5c2d8e4b6a0f7c3d9e1b5a8f2c6d0e4a7b9f1c3e5d8a2b6f0c4e7a9d".into(),
            ),
            ..request
        },
        attempts,
        bodies,
    });
    parts.ledger.record(served("r-unserved", now - 1_000, None));
    wait_json(&parts.app, "/admin/requests", &cookie, |v| {
        v["requests"].as_array().is_some_and(|rows| rows.len() == 4)
    })
    .await;
    let listed = |query: &'static str| {
        let app = parts.app.clone();
        let cookie = cookie.clone();
        async move {
            let page =
                json(get(&app, &format!("/admin/requests?{query}"), Some(&cookie)).await).await;
            page["requests"]
                .as_array()
                .unwrap_or_else(|| panic!("a page of rows: {page}"))
                .clone()
        }
    };
    let ids = |rows: &[serde_json::Value]| -> Vec<String> {
        rows.iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let refused = listed("stop_reasons=refusal").await;
    assert_eq!(
        ids(&refused),
        ["r-bare", "r-harmful"],
        "the refusal stop keeps the two refusals"
    );
    for row in &refused {
        let response = &row["attempts"][0]["response"];
        assert_eq!(response["stop_reason"], "refusal", "{row}");
        assert!(
            row.get("stop_reason").is_none() && row.get("refusal_category").is_none(),
            "the row carries the words on its attempt alone: {row}"
        );
    }
    assert_eq!(
        (
            refused[0]["attempts"][0]["response"]["refusal_category"].clone(),
            refused[1]["attempts"][0]["response"]["refusal_category"].clone(),
        ),
        (
            serde_json::Value::Null,
            serde_json::json!("harmful_request")
        ),
        "each kept row's serving attempt answers the category it stored"
    );
    assert_eq!(
        ids(&listed("refusal_categories=-").await),
        ["r-unserved", "r-ended", "r-bare"],
        "`-` keeps every request whose serving attempt names no category, and the one no attempt served"
    );
    assert_eq!(
        ids(&listed("stop_reasons=refusal&refusal_categories=-").await),
        ["r-bare"],
        "the two filters conjoin, so `-` beside the refusal stop keeps the refusal that named no category"
    );
    assert_eq!(
        ids(&listed("fallback=true").await),
        ["r-ended"],
        "`true` keeps the request that presented a fallback credit"
    );
    assert_eq!(
        ids(&listed("fallback=false").await),
        ["r-unserved", "r-bare", "r-harmful"],
        "`false` keeps every request that presented none"
    );
}

/// Every session's main agent is `main`, so the session narrows the agent's requests to one conversation.
#[tokio::test]
async fn the_request_list_keeps_one_agents_requests_under_agent_id_beside_the_session() {
    use common::{get, json, login, wait_json};
    use phoebis_ledger::{Record, RequestRecord};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    for (id, at, session, agent) in [
        ("r-main-s1", now - 4_000, "s1", Some("main")),
        ("r-agent", now - 3_000, "s1", Some("agent-7")),
        ("r-main-s2", now - 2_000, "s2", Some("main")),
        ("r-none", now - 1_000, "s1", None),
    ] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 500),
                pool_id: Some("main".into()),
                session_id: Some(session.into()),
                agent_id: agent.map(String::from),
                model: "claude-sonnet-5".into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                status: Some(503),
                outcome: "exhausted".into(),
                ..Default::default()
            },
            attempts: Vec::new(),
            bodies: None,
        });
    }
    wait_json(&parts.app, "/admin/requests", &cookie, |v| {
        v["requests"].as_array().is_some_and(|rows| rows.len() == 4)
    })
    .await;
    let ids = |query: &'static str| {
        let app = parts.app.clone();
        let cookie = cookie.clone();
        async move {
            let page =
                json(get(&app, &format!("/admin/requests?{query}"), Some(&cookie)).await).await;
            page["requests"]
                .as_array()
                .unwrap_or_else(|| panic!("a page of rows: {page}"))
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        ids("agent_id=agent-7").await,
        ["r-agent"],
        "the agent's request, the main agent's and the one that names no agent left out"
    );
    assert_eq!(
        ids("agent_id=main").await,
        ["r-main-s2", "r-main-s1"],
        "the main agent's requests of both sessions, newest first"
    );
    assert_eq!(
        ids("agent_id=main&session=s1").await,
        ["r-main-s1"],
        "the session narrows the agent to one conversation"
    );
}

/// The writer stamps a row from the rig's clock, so the clock set past `to` before the write lands a row received
/// inside the range after the range's end.
#[tokio::test]
async fn the_request_list_answers_a_fixed_end_watermark_past_to_and_a_live_one_at_to() {
    use common::{get, json, login, wait_json};
    use phoebis_ledger::{Record, RequestRecord};
    use std::sync::atomic::Ordering;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let (from, to) = (common::NOW - 5_000, common::NOW);
    let written_at = common::NOW + 5_000;
    parts.clock_cell.store(written_at, Ordering::SeqCst);
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-late".into(),
            received_at: to - 1_000,
            finished_at: Some(to - 500),
            pool_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    });
    let range = format!("/admin/requests?from={from}&to={to}");
    let fixed = wait_json(&parts.app, &format!("{range}&fixed_end=1"), &cookie, |v| {
        v["requests"].as_array().is_some_and(|rows| rows.len() == 1)
    })
    .await;
    assert_eq!(
        fixed["landed_through"].as_i64(),
        Some(written_at),
        "the fixed-end read answers the late row's stamp, past to {to}: {fixed}"
    );
    let spelled = json(
        get(
            &parts.app,
            &format!("{range}&fixed_end=true"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        spelled["landed_through"].as_i64(),
        Some(written_at),
        "the console's fixed_end=true spelling reads the same stamp: {spelled}"
    );
    let live = json(get(&parts.app, &range, Some(&cookie)).await).await;
    assert_eq!(
        (
            live["requests"].as_array().map(Vec::len),
            live["landed_through"].as_i64()
        ),
        (Some(1), Some(to)),
        "the same range read live keeps its watermark at to: {live}"
    );
}

#[tokio::test]
async fn a_confirm_on_a_slot_sealed_under_another_key_answers_409_naming_the_account() {
    use common::{get, json, login, post};
    let (app, _up, store, _manager) = common::paired_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for(&app, &cookie, "business").await;
    // AES-GCM cannot tell a blob sealed under another storage key from a damaged one, so a blob this key cannot
    // open stands for the slot sealed elsewhere, and the refusal answers both alike.
    store.debug_corrupt_credentials("business").unwrap();
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 409);
    assert_eq!(
        confirmed.headers()["x-phoebis-error-code"],
        "sealed_under_another_key"
    );
    assert_eq!(
        json(confirmed).await["error"],
        "Business holds a credential sealed under another storage key, so disconnect it before pairing again"
    );
    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(shown["status"], "failed");
}

/// The commit's own refusal: a token answer without a refresh token makes the commit open the credential it replaces
/// to carry that token forward, so a credential sealed under another key after the confirm's read refuses the commit
/// as the read's refusal does. The read parks on another thread, so the runtime has two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirm_whose_commit_opens_a_credential_sealed_under_another_key_answers_409_naming_the_account(
) {
    use common::{get, json, login, post};
    let (app, up, store, _manager) = common::paired_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let mut token: serde_json::Value =
        serde_json::from_slice(phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Token,
        ))
        .unwrap();
    token.as_object_mut().unwrap().remove("refresh_token");
    up.set_rule_after(
        0,
        "/v1/oauth/token",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&token).unwrap(),
        },
    );
    let id = common::exchanged_session_for(&app, &cookie, "business").await;
    let hold = store.debug_hold_after_next_credential_read();
    let confirm = tokio::spawn({
        let (app, cookie, path) = (
            app.clone(),
            cookie.clone(),
            format!("/admin/oauth/sessions/{id}/confirm"),
        );
        async move { post(&app, &path, "", Some(&cookie)).await }
    });
    let give_up = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !hold.is_parked() {
        assert!(
            tokio::time::Instant::now() < give_up,
            "the confirm's credential read never parked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The slot's credential keeps its generation, so the commit's fence passes and its open is what refuses it.
    store.debug_corrupt_credentials("business").unwrap();
    hold.release();
    let confirmed = confirm.await.unwrap();
    assert_eq!(confirmed.status(), 409);
    assert_eq!(
        confirmed.headers()["x-phoebis-error-code"],
        "sealed_under_another_key"
    );
    assert_eq!(
        json(confirmed).await["error"],
        "Business holds a credential sealed under another storage key, so disconnect it before pairing again"
    );
    let shown = json(get(&app, &format!("/admin/oauth/sessions/{id}"), Some(&cookie)).await).await;
    assert_eq!(shown["status"], "failed");
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
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
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
    assert!(
        rows.iter()
            .all(|r| r.source != phoebis_ledger::SampleSource::Head),
        "{rows:?}"
    );
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
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
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
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
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
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
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
    assert_eq!(
        read,
        [
            ("five_hour".to_string(), 12.5),
            ("seven_day".to_string(), 40.0)
        ],
        "the Usage fixture's two windows in percent: {refreshed}"
    );
    // The same two figures as shares of one.
    let shares = [("five_hour", 0.125), ("seven_day", 0.4)];

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
    for (key, share) in shares {
        let row = rows
            .iter()
            .find(|r| r.window == key)
            .unwrap_or_else(|| panic!("no row under {key}: {rows:?}"));
        assert_eq!(
            (row.source.as_str(), row.observed_at, row.utilization),
            ("poll", NOW, Some(share)),
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
    for (key, share) in shares {
        let entry = routed
            .iter()
            .find(|w| w["id"] == key)
            .unwrap_or_else(|| panic!("no window {key} on the account: {acct}"));
        assert_eq!(entry["utilization"], serde_json::json!(share), "{acct}");
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
    assert_eq!(
        refreshed["windows"].as_array().map(Vec::len),
        Some(2),
        "{refreshed}"
    );
    // The upstream's 37.5 and 61.0 percent as shares of one, at the resets the body named and the refresh's instant.
    let reading = vec![
        serde_json::json!({
            "id": "five_hour", "utilization": 0.375, "resets_at": NOW + 3 * HOUR, "observed_at": taken,
        }),
        serde_json::json!({
            "id": "seven_day", "utilization": 0.61, "resets_at": NOW + 90 * HOUR, "observed_at": taken,
        }),
    ];
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
/// OAuth account's while the router holds its fast lane alone. Each window reads its row's own instant.
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
                    plan: None,
                    tier: None,
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
        source: phoebis_ledger::SampleSource::Head,
        observed_at,
    };
    h.db.insert_samples(
        &[
            row("business", "five_hour", 0.4, read_at),
            row("direct", "five_hour", 0.1, read_at),
        ],
        None,
    )
    .unwrap();
    for (id, five_hour) in [("business", 0.4), ("direct", 0.1)] {
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
        assert_eq!(drawn, [("five_hour", five_hour, read_at)], "{account}");
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
            .filter(|r| r.line.starts_with("GET /api/oauth/usage "))
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
                        plan: None,
                        tier: None,
                    },
                )
                .with(
                    SEVEN_DAY,
                    WindowReading {
                        utilization: 0.5,
                        reset: Some(NOW + 86_400_000),
                        observed_at: NOW - 60_000,
                        source: WindowSource::Stored,
                        plan: None,
                        tier: None,
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
        plan: None,
        tier: None,
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
    let usage = wait_json(&h.app, "/admin/accounts/business/usage", &cookie, |v| {
        v["error"].is_null()
    })
    .await;
    assert_eq!(
        (
            &usage["fable_window"]["key"],
            usage["windows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["key"] == "seven_day_fable")
        ),
        (&serde_json::json!("fable"), false),
        "the usage answer keeps the Fable week under its own key, apart from its windows: {usage}"
    );
    // Each window reads as the router stored it, the 5-hour window's passed reset with it, while the projection reads
    // the router's prediction, which empties that window until the next reading names a reset. No pin burns, so each
    // window projects its prediction at its horizon: five hours on, the weekly windows' resets lying further out. The
    // Fable week reads under the one id `seven_day_fable`, and its lead paces it as the week the routing name says it
    // is. Each lead is the utilization less the share of the window run: the whole 5-hour window, whose reset passed a
    // day ago, and 96 of each week's 168 hours, four sevenths, with its reset 72 hours out.
    let held = serde_json::json!([
        { "id": "five_hour", "observed_at": read_at, "resets_at": NOW - 25 * HOUR, "utilization": 0.9,
          "projection": 0.0, "horizon": NOW + 5 * HOUR, "burn": 0.0, "demand": 0.0,
          "lead": 0.9 - 1.0 },
        { "id": "seven_day", "observed_at": read_at, "resets_at": NOW + 72 * HOUR, "utilization": 0.5,
          "projection": 0.5, "horizon": NOW + 5 * HOUR, "burn": 0.0, "demand": 0.0,
          "lead": 0.5 - 4.0 / 7.0 },
        { "id": "seven_day_fable", "observed_at": read_at, "resets_at": NOW + 72 * HOUR, "utilization": 0.2,
          "projection": 0.2, "horizon": NOW + 5 * HOUR, "burn": 0.0, "demand": 0.0,
          "lead": 0.2 - 4.0 / 7.0 },
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

/// `business` as `GET /admin/accounts/{id}` and the list answer it, in that order.
async fn business_as_read_and_listed(h: &common::Polled, cookie: &str) -> [serde_json::Value; 2] {
    use common::{get, json};
    let read = json(get(&h.app, "/admin/accounts/business", Some(cookie)).await).await;
    let listed = json(get(&h.app, "/admin/accounts", Some(cookie)).await).await;
    let row = listed["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap()
        .clone();
    [read, row]
}

/// `business` with a reading on each of the view's two windows paths, the router's reported 5-hour window beside a
/// fast-lane reading and the ledger's rows of today for both plan windows, so the router's reading answers the 5-hour
/// window and the ledger's row the week. The endpoint refuses one poll, so the row carries a poll standing that a
/// moved state can drop. Answers the account as its own `GET` reads it before any state moves.
async fn business_with_both_windows_paths() -> (common::Polled, String, serde_json::Value) {
    use common::{login, wait_json, NOW};
    use phoebis_ledger::QuotaSampleRecord;
    use phoebis_routing::{Report, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR};
    let h = common::admin_parts_polled().await;
    let cookie = login(&h.app, "admin-secret").await;
    let reading = |utilization, reset, observed_at| WindowReading {
        utilization,
        reset: Some(reset),
        observed_at,
        source: WindowSource::Head,
        plan: None,
        tier: None,
    };
    h.router
        .report(Report::Sample {
            account_id: "business".into(),
            sample: UtilizationSample::default()
                .with(FIVE_HOUR, reading(0.25, NOW + 3_600_000, NOW - 60_000))
                .with(
                    phoebis_relay::FAST_INPUT,
                    reading(0.3, NOW + 60_000, NOW - 30_000),
                ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    let read_at = NOW - 42_000;
    let row = |window: &str, utilization| QuotaSampleRecord {
        account_id: "business".into(),
        bucket: phoebis_ledger::bucket_of(read_at),
        window: window.into(),
        utilization: Some(utilization),
        reset_at: None,
        source: phoebis_ledger::SampleSource::Head,
        observed_at: read_at,
    };
    h.db.insert_samples(&[row("five_hour", 0.4), row("seven_day", 0.5)], None)
        .unwrap();
    h.up.set_rule_after(
        0,
        "/api/oauth/usage",
        fake_upstream::Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    h.usage.poll("business").await.unwrap();
    let before = wait_json(&h.app, "/admin/accounts/business", &cookie, |v| {
        !v["fast_lane"].is_null()
    })
    .await;
    let drawn: Vec<(&str, f64)> = before["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["id"].as_str().unwrap(),
                w["utilization"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(drawn, [("five_hour", 0.25), ("seven_day", 0.5)], "{before}");
    assert!(!before["usage"]["poll"].is_null(), "{before}");
    (h, cookie, before)
}

/// The poll asks no disconnected account, so the moved row's poll standing reads as `poll_standing` answers it for one,
/// while the router's held windows still compose its `error` and `fetched_at`.
#[tokio::test]
async fn a_disconnected_oauth_account_answers_no_window_whatever_the_router_and_the_ledger_hold() {
    let (h, cookie, before) = business_with_both_windows_paths().await;
    h.store
        .set_state(
            "business",
            phoebis_store::AccountState::Disconnected,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    let poll_less = serde_json::json!({
        "error": before["usage"]["error"],
        "fetched_at": before["usage"]["fetched_at"],
        "held_until": null,
        "poll": null,
    });
    for after in business_as_read_and_listed(&h, &cookie).await {
        assert_eq!(after["state"], "disconnected", "{after}");
        assert_eq!(after["fast_lane"], before["fast_lane"], "{after}");
        assert_eq!(after["usage"], poll_less, "{after}");
        assert_eq!(after["windows"], serde_json::json!([]), "{after}");
    }
}

#[tokio::test]
async fn a_reauth_required_oauth_account_answers_no_window_whatever_the_router_and_the_ledger_hold()
{
    let (h, cookie, before) = business_with_both_windows_paths().await;
    h.store
        .set_state(
            "business",
            phoebis_store::AccountState::ReauthRequired,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    for after in business_as_read_and_listed(&h, &cookie).await {
        assert_eq!(after["state"], "reauth_required", "{after}");
        assert_eq!(after["fast_lane"], before["fast_lane"], "{after}");
        assert_eq!(after["usage"], before["usage"], "{after}");
        assert_eq!(after["windows"], serde_json::json!([]), "{after}");
    }
}

/// A flagged account stays `ready`, so its poll standing reads whole, `held_until` included, and the flag alone
/// withholds the windows.
#[tokio::test]
async fn a_plan_ended_oauth_account_answers_no_window_while_the_flag_stands_and_its_windows_again_once_it_clears(
) {
    let (h, cookie, before) = business_with_both_windows_paths().await;
    let flagged_at = common::NOW - 60_000;
    assert!(h.store.set_plan_ended("business", flagged_at).unwrap());
    for after in business_as_read_and_listed(&h, &cookie).await {
        assert_eq!(after["windows"], serde_json::json!([]), "{after}");
        assert_eq!(after["state"], "ready", "{after}");
        assert_eq!(after["plan_ended_at"], flagged_at, "{after}");
        assert_eq!(after["fast_lane"], before["fast_lane"], "{after}");
        assert_eq!(after["usage"], before["usage"], "{after}");
    }
    assert!(h.store.clear_plan_ended("business").unwrap());
    for after in business_as_read_and_listed(&h, &cookie).await {
        assert_eq!(after["windows"], before["windows"], "{after}");
    }
}

#[tokio::test]
async fn a_funding_required_oauth_account_keeps_the_windows_the_router_and_the_ledger_hold() {
    let (h, cookie, before) = business_with_both_windows_paths().await;
    h.store
        .set_state(
            "business",
            phoebis_store::AccountState::FundingRequired,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    for after in business_as_read_and_listed(&h, &cookie).await {
        assert_eq!(after["state"], "funding_required", "{after}");
        assert_eq!(after["windows"], before["windows"], "{after}");
    }
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
async fn every_answer_and_every_refusal_of_the_gate_names_the_build_the_binary_was_built_from() {
    use common::{cookie_of, delete, get, post, post_from, FIXTURE_BUILD_COMMIT};
    let (app, _up) = common::admin_app().await;
    let anonymous = get(&app, "/admin/accounts", None).await;
    assert_eq!(anonymous.status(), 401);
    assert_eq!(
        anonymous.headers()["x-phoebis-build"],
        FIXTURE_BUILD_COMMIT,
        "the gate's refusal names the build too"
    );
    let refused = post(&app, "/admin/session", r#"{"secret":"nope"}"#, None).await;
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()["x-phoebis-build"], FIXTURE_BUILD_COMMIT);
    let login = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    assert_eq!(login.status(), 204);
    assert_eq!(login.headers()["x-phoebis-build"], FIXTURE_BUILD_COMMIT);
    let cookie = cookie_of(&login);
    let forged = post_from(
        &app,
        "/admin/accounts/business/pause",
        "",
        Some(&cookie),
        "https://evil.example",
    )
    .await;
    assert_eq!(forged.status(), 403);
    assert_eq!(forged.headers()["x-phoebis-build"], FIXTURE_BUILD_COMMIT);
    for path in ["/admin/accounts", "/admin/system", "/admin/no-such-route"] {
        let answer = get(&app, path, Some(&cookie)).await;
        assert_eq!(
            answer.headers()["x-phoebis-build"],
            FIXTURE_BUILD_COMMIT,
            "{path} answered {}",
            answer.status()
        );
    }
    let logout = delete(&app, "/admin/session", Some(&cookie)).await;
    assert_eq!(logout.status(), 204);
    assert_eq!(logout.headers()["x-phoebis-build"], FIXTURE_BUILD_COMMIT);
}

#[tokio::test]
async fn the_system_and_settings_answers_carry_the_build_beside_the_version() {
    use common::{get, json, login, FIXTURE_BUILD_COMMIT};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let system = json(get(&app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(system["build"], FIXTURE_BUILD_COMMIT, "{system}");
    assert_eq!(system["version"], env!("CARGO_PKG_VERSION"), "{system}");
    let settings = json(get(&app, "/admin/settings", Some(&cookie)).await).await;
    assert_eq!(
        settings["about"]["build"], FIXTURE_BUILD_COMMIT,
        "{settings}"
    );
    assert_eq!(settings["about"]["version"], env!("CARGO_PKG_VERSION"));
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
            phoebis_store::AttemptOutcome::Failed {
                cause: "503".into(),
                detail: "refresh rejected with status 503".into(),
            },
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
        None,
    );
    vault.discard("pairing", common::NOW);
    // A callback whose exchange returns after the owner canceled discards the record a second time.
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
    assert_eq!(shown["status"], "canceled");
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
            "plan": "max_5x",
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
        Some(vec!["persistent".to_string(), "session_id".to_string()]),
        "the session answer names the session and its kind and nothing else: {me}"
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
        Some("claude-code-2.1.310-linux-x64")
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
        burn_unmetered_model: None,
        last_input_tokens: 1_000,
        last_refresh_at: now - 5,
        cache_ttl_ms: 300_000,
        shed: None,
        static_hash: None,
        tail_hash: None,
        lane: phoebis_routing::lane_key("claude-opus-5", "standard", "none", "-"),
        entrypoint: None,
        mark: None,
        auxiliary_only: false,
        effort: None,
        ended: false,
    }
}

/// Stores a profile naming `tier` for `account`, as the profile fetch would.
fn name_the_tier(store: &phoebis_store::Store, account: &str, tier: &str) {
    store
        .set_profile(
            account,
            "{}",
            &phoebis_store::SubscriptionRow {
                tier: Some(tier.into()),
                since: None,
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// The pool `main` over `business` (subscription, Ultra) and `direct` (api_key) behind key `desktop`, seeded
/// through one harness with `pins` and handed to a second one: the router task loads its pins at spawn.
async fn sessions_harness_with(pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>) -> common::Full {
    sessions_harness_priced(phoebis_ledger::PriceRevisions::shipped(), pins).await
}

/// `sessions_harness_with` pricing dollars through `prices` rather than the shipped table.
async fn sessions_harness_priced(
    prices: phoebis_ledger::PriceRevisions,
    pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>,
) -> common::Full {
    let seed = sessions_seed(pins).await;
    common::admin_parts_full_on_priced(seed.store.clone(), prices).await
}

/// `sessions_harness_with` pricing dollars and shares through the ledger's test tables rather than the shipped files.
async fn sessions_harness_on_test_tables(
    pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>,
) -> common::Full {
    let seed = sessions_seed(pins).await;
    common::admin_parts_full_test_tables(Some(seed.store.clone())).await
}

/// `sessions_harness_with` with the sessions route bounded by `session_expiry` and `non_companion_idle` rather than
/// the harness's 24 hours and one hour.
async fn sessions_harness_bounded(
    session_expiry: std::time::Duration,
    non_companion_idle: std::time::Duration,
    pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>,
) -> common::Full {
    let seed = sessions_seed(pins).await;
    common::admin_parts_full_on_bounded(seed.store.clone(), session_expiry, non_companion_idle)
        .await
}

/// The harness `sessions_harness_with` writes its store through: the pool, the key, the business account's tier and
/// `pins`.
async fn sessions_seed(pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>) -> common::Full {
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
    seed
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
    let parts = sessions_harness_on_test_tables(|generation| {
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
    // The test tables' second entries, in force at the fixed clock, which the peak band never covers: 145 Opus 5.5
    // input tokens a second at the weight 4 are 580 units, over Ultra's 100 on the 5-hour window and times 3,600
    // seconds 20,880 an hour. The plain copy's 100 tokens a second are 400 units, over the Pro tier's 50 on the week
    // and times 3,600 seconds 28,800 an hour. The plain copy at the Opus 5 input price of $10 a million tokens is $3.60
    // an hour, and at Opus 5.5's own $20 it is $7.20.
    let (share_per_hour, equivalent_per_hour) = (20_880.0, 28_800.0);
    let usd_per_hour = 3.6;
    // The file's absolute bound of 1e-12 lies under one ulp of a figure in the thousands, so these compare to a
    // relative one.
    let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() <= b.abs() * 1e-12);
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
    // The test list prices Opus 5.5, so its dollar reading is that row's own rate.
    let s1_usd_per_hour = 7.2;
    assert_eq!(
        row(&sessions, "s1")["burn_at"].as_i64(),
        Some(now),
        "the instant the served burn is decayed to, the read's own"
    );
    let (s1, s2) = (row(&sessions, "s1"), row(&sessions, "s2"));
    assert!(
        close(s1["burn_equivalent"].as_f64(), equivalent_per_hour) && s1.get("estimated").is_none(),
        "the plan pin's plain copy as its Pro-week figure per hour, on a weighted model: {s1}"
    );
    assert_eq!(
        (
            s2.get("burn_equivalent"),
            s2.get("burn_window"),
            s2.get("estimated")
        ),
        (
            Some(&serde_json::Value::Null),
            Some(&serde_json::Value::Null),
            None
        ),
        "a pin billed in dollars has no window equivalent and names no window, whatever its model: {s2}"
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
    // Evidence that names no reset stands for the cooldown fallback past its observation, so read at the instant it
    // was observed it moves the reading to dollars as a reset ahead does. The clock steps first: the evidence lands
    // only over the newer report above, and it is observed at the router's clock rather than ahead of it, since an
    // observation ahead of the clock stands under any fallback, zero included.
    common::step(&parts, std::time::Duration::from_secs(10));
    parts
        .router
        .report(phoebis_routing::Report::Overage {
            account_id: "business".into(),
            evidence: phoebis_routing::OverageEvidence {
                in_use: true,
                since: Some(now - 3_600_000),
                reset: None,
                observed_at: now + 10_000,
                source: phoebis_routing::EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
    wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["burn_unit"] == "usd")
    })
    .await;
}

#[tokio::test]
async fn a_session_row_prices_the_burn_at_the_set_the_pins_last_prompt_selects() {
    use common::{get, json, login, patch, wait_json};
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
    // Three api_key pins burning 100 input tokens a second: `s-at` with its last prompt at the threshold,
    // `s-over` one token over it, and `s-fast` over it, which then answers a fast turn and a standard turn below
    // the threshold through the router. Its copies were never folded, so nothing decays at those turns.
    let parts = sessions_harness_priced(revisions, |generation| {
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
    // 100 input tokens a second for an hour are 360,000 tokens: $0.36 at the standard cells' $1 a million, the prompt
    // at the threshold, and $1.80 at the long-context cells' $5, the prompt one token over it.
    for (session, usd_per_hour) in [("s-at", 0.36), ("s-over", 1.8)] {
        let (burn, unit) = burn_of(&sessions, session);
        assert!(
            close(burn, usd_per_hour) && unit.as_deref() == Some("usd"),
            "{session} reads its burn per hour at the cells its last prompt selects: {:?} against {usd_per_hour}",
            (burn, unit)
        );
        let row = session_row(&sessions, session);
        assert_eq!(
            (row.get("burn_equivalent"), row.get("estimated")),
            (Some(&serde_json::Value::Null), None),
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
            entrypoint: None,
            effort: None,
        },
        now,
        usage: Some(phoebis_routing::PinUsage {
            input,
            ..phoebis_routing::PinUsage::default()
        }),
        keepalive: false,
        cache_ttl_ms: 300_000,
        fast,
        turn_end: phoebis_routing::RenewalEnd::Turn,
        thinking_tokens: None,
        input_transformations: None,
    };
    parts.router.report(renewal(true, 60_000)).await;
    parts.router.report(renewal(false, 600)).await;
    // An hour of 101 standard tokens a second at $1 a million and 100 fast ones at $2: 0.3636 and 0.72 dollars.
    let expected = 1.0836;
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
async fn a_plan_pins_dollars_per_hour_price_its_plan_copy_at_the_listed_rows_and_a_billed_or_unlisted_pin_reads_none(
) {
    use common::{get, json, login};
    let now = common::NOW;
    // One Haiku 5.5 row at $1 a million tokens in every cell, so the dearest cells the dollar table's fallback prices an
    // unlisted model at are $1 too.
    let revisions = phoebis_ledger::PriceRevisions::from_yaml(
        "revisions:\n  - effective: 2026-01-01\n    models:\n      claude-haiku-5-5: {input: 1, cache_write_5m: 1, \
         cache_write_1h: 1, cache_read: 1, output: 1}\n",
        "test",
    )
    .unwrap();
    // Each pin burns 100 input tokens a second in its plan copy. The plan pin on `business` carries 50 more a second of
    // the client's classifier calls, the billed pin sits on the API-key account `direct`, and the unlisted pin burns
    // Opus 5, which the case's table does not list.
    let parts = sessions_harness_priced(revisions, |generation| {
        let pin = |session: &str, account: &str, model: &str| phoebis_store::PinRow {
            model: model.into(),
            ..burning_opus_pin(session, account, generation, now, 100.0, 100.0)
        };
        vec![
            phoebis_store::PinRow {
                burn_unmetered: phoebis_store::PinBurn {
                    input: 50.0,
                    ..phoebis_store::PinBurn::default()
                },
                ..pin("s-plan", "business", "claude-haiku-5-5")
            },
            pin("s-billed", "direct", "claude-haiku-5-5"),
            pin("s-unlisted", "business", "claude-opus-5"),
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let usd = |session: &str| session_row(&sessions, session)["burn_usd"].as_f64();
    // 100 plan tokens a second for an hour are 360,000 tokens, $0.36 at $1 a million.
    assert!(
        close(usd("s-plan"), 0.36),
        "the plan pin reads its plan copy alone at the listed row's rate: {}",
        session_row(&sessions, "s-plan")
    );
    assert_eq!(
        (usd("s-billed"), usd("s-unlisted")),
        (None, None),
        "a pin billed in dollars reads no plan dollars, and neither does a plan pin on a model the price list lacks: \
         {sessions}"
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
    read_windows_at(parts, account, readings, common::NOW - 60_000).await;
}

/// `read_windows` observed at `observed_at`, since a reading lands only over an older one.
async fn read_windows_at(
    parts: &common::Full,
    account: &str,
    readings: &[(&str, f64, Option<i64>)],
    observed_at: i64,
) {
    let sample = readings.iter().fold(
        phoebis_routing::UtilizationSample::default(),
        |sample, (window, utilization, reset)| {
            sample.with(
                window,
                phoebis_routing::WindowReading {
                    utilization: *utilization,
                    reset: *reset,
                    observed_at,
                    source: phoebis_routing::WindowSource::Head,
                    plan: None,
                    tier: None,
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
async fn a_pinned_sessions_runway_reads_its_accounts_own_projection_with_its_burn_counted_once_and_unbounded_past_the_reset(
) {
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
    // Read first with the 5-hour window resetting in two hours, before its three hours to the ceiling, and the week's
    // (0.95 - 0.30) / 0.002 = 325 hours past its reset 50 hours out: each window resets before the ceiling is reached.
    read_windows_at(
        &parts,
        "business",
        &[
            (phoebis_routing::FIVE_HOUR, 0.80, Some(NOW + 2 * HOUR)),
            (phoebis_routing::SEVEN_DAY, 0.30, Some(NOW + 50 * HOUR)),
        ],
        NOW - 120_000,
    )
    .await;
    let past = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        !session_row(v, "s1")["runway_bounded"].is_null()
    })
    .await;
    let row = session_row(&past, "s1");
    assert_eq!(
        (&row["runway_ms"], &row["runway_bounded"]),
        (&serde_json::Value::Null, &serde_json::json!(false)),
        "a runway past its window's reset reads unbounded: {row}"
    );
    // The newer reading moves the 5-hour reset out to four hours, past the three hours to the ceiling.
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
        source: phoebis_ledger::SampleSource::Head,
        observed_at: bucket * 1_000,
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
    agent_turn(parts, session, "main", "claude-sonnet-5", request_id, at).await
}

/// `main_turn` for `agent` asking for `model`, a subagent's turn naming main its parent.
async fn agent_turn(
    parts: &common::Full,
    session: &str,
    agent: &str,
    model: &str,
    request_id: &str,
    at: i64,
) -> phoebis_routing::SelectResult {
    let snapshot = parts.router.snapshot();
    let key = snapshot
        .key("desktop")
        .expect("the key is in the snapshot")
        .clone();
    let candidates =
        phoebis_routing::resolve(&snapshot, &key, model, model).expect("the pool allows the model");
    let main = agent == "main";
    parts
        .router
        .select(phoebis_routing::SelectRequest {
            request_id: request_id.into(),
            attempt: 1,
            seq: 1,
            key_id: "desktop".into(),
            key_generation: common::generation(&parts.store, "desktop"),
            agent_id: agent.into(),
            session_id: session.into(),
            canonical_model: model.into(),
            folded_model: model.into(),
            candidates,
            portable: true,
            credit_account: None,
            attempted: Vec::new(),
            now: at,
            received_at: at,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: Some(if main { "main" } else { "subagent" }.into()),
            context_compacted: false,
            boundaries: std::sync::Arc::from([]),
            client: phoebis_routing::ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: std::sync::Arc::from([]),
            marker: false,
            parent_agent_id: (!main).then(|| "main".into()),
            failed_over: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: phoebis_routing::lane_key(model, "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
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
async fn a_slowed_account_answers_its_slow_down_on_the_account_route_and_its_factor_and_speed_on_its_member_and_session(
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
            &slow_down["ramp"],
            &slow_down["threshold_ms"],
            &slow_down["hold_ms"]
        ),
        (
            &serde_json::json!(1.0),
            &serde_json::json!(NOW),
            &serde_json::json!(0.0),
            &serde_json::json!(600_000),
            &serde_json::json!(60_000)
        ),
        "no pin has a cycle yet, so none is held, and the span opened at the read with no time for its ramp: {slow_down}"
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
    let null = serde_json::Value::Null;
    assert_eq!(
        (
            pool_member(&pools, "business")["factor"].as_f64(),
            pool_member(&pools, "business")["speed"].as_f64(),
            pool_member(&pools, "direct").get("factor"),
            pool_member(&pools, "direct").get("speed")
        ),
        (Some(factor), Some(1.0), Some(&null), Some(&null)),
        "the member names its account's factor and realized speed, and null where no entry is in force"
    );
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let s1 = session_row(&sessions, "s1");
    assert_eq!(
        (s1["factor"].as_f64(), s1["speed"].as_f64(), s1.get("cycle_ms")),
        (Some(factor), Some(1.0), Some(&null)),
        "the pin reads its account's factor and realized speed, and no cycle before a second request of the pin: {s1}"
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
async fn a_pool_member_the_router_has_not_loaded_reads_unknown_while_a_paused_one_reads_unavailable(
) {
    use common::{get, json, login, post};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts/direct/pause",
            "",
            Some(&cookie)
        )
        .await
        .status(),
        204,
        "the pause's reload brings the obstacle to the router"
    );
    // Through the store alone and after the pause, since any reload before the read gives the member a projection.
    let member = |account_id: &str, priority| phoebis_store::MemberWrite {
        account_id: account_id.into(),
        priority,
        budget_daily_usd: None,
        budget_monthly_usd: None,
    };
    parts
        .store
        .set_pool_members(
            "main",
            &[member("direct", 0), member("business", 1)],
            "test",
        )
        .unwrap();
    let pools = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await;
    assert_eq!(
        (
            pool_member(&pools, "direct")["eligible"].as_str(),
            pool_member(&pools, "business")["eligible"].as_str()
        ),
        (Some("unavailable"), Some("unknown")),
        "a member the router passes over keeps its obstacle's word, and one it has not loaded reads unknown: {pools}"
    );
}

/// A cost model whose `default_claude_max_20x` tier holds 20 times Pro's 5-hour window and 40 times Pro's week, so a
/// burn read on one window through the other window's tier factor reads apart from the right figure.
const UNEQUAL_FACTORS_COSTS: &str = "\
unit: {tier: default_claude_ai, window: seven_day}
revisions:
  - effective: 2026-01-01
    capacity:
      default_claude_ai:      {five_hour: 5000000, seven_day: 50000000}
      default_claude_max_20x: {five_hour: 100000000, seven_day: 2000000000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-opus-5-5: 2
    model_windows: {}
    overrides: {}
    tier_order: [opus]
    model_tier:
      claude-opus-5-5: opus
";

/// `sessions_harness_with` reading the subscription cost model `costs` rather than the shipped one.
async fn sessions_harness_costed(
    costs: phoebis_ledger::PlanTables,
    pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>,
) -> common::Full {
    let seed = sessions_seed(pins).await;
    common::admin_parts_full_on_costed(seed.store.clone(), costs).await
}

#[tokio::test]
async fn a_pin_whose_account_lacks_the_five_hour_window_reads_its_burn_on_the_week() {
    use common::{get, json, login};
    use phoebis_routing::{PinUsage, UtilizationSample, WindowReading, WindowSource};
    use phoebis_routing::{FIVE_HOUR, SEVEN_DAY};
    let now = common::NOW;
    let tables = phoebis_ledger::PlanTables {
        revisions: phoebis_ledger::CostRevisions::from_yaml(UNEQUAL_FACTORS_COSTS)
            .expect("the cost model with unequal factors parses"),
        path: None,
        source: "test".to_string(),
    };
    // One hundred input tokens a second plain and 145 banded: the 5-hour window reads the banded copy, the week the
    // plain one.
    let parts = sessions_harness_costed(tables.clone(), |generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5-5".into(),
            ..burning_opus_pin("s1", "business", generation, now, 100.0, 145.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let input = |input: u64| PinUsage {
        input,
        ..PinUsage::default()
    };
    let share_per_hour = |window: &str, usage: PinUsage| {
        tables
            .share_at(
                window,
                None,
                "default_claude_max_20x",
                "claude-opus-5-5",
                usage,
                now,
            )
            .unwrap()
            * 3_600.0
    };
    let pro_week_per_hour = tables
        .equivalent_at(
            SEVEN_DAY,
            None,
            "claude-opus-5-5",
            input(100),
            now,
            now,
            now,
        )
        .unwrap()
        * 3_600.0;
    let read = |sessions: &serde_json::Value| {
        let row = session_row(sessions, "s1");
        (
            row["burn_window"].as_str().map(String::from),
            row["burn"].as_f64(),
            row["burn_equivalent"].as_f64(),
        )
    };
    let five_hour_burn = share_per_hour(FIVE_HOUR, input(145));
    let (window, burn, equivalent) =
        read(&json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await);
    assert!(
        window.as_deref() == Some(FIVE_HOUR)
            && close(burn, five_hour_burn)
            && close(equivalent, pro_week_per_hour),
        "before any poll the account has both windows, so the burn reads the 5-hour window and the figure the Pro \
         week: {:?} against {five_hour_burn} and {pro_week_per_hour}",
        (window, burn, equivalent)
    );
    parts
        .router
        .report(phoebis_routing::Report::Sample {
            account_id: "business".into(),
            sample: UtilizationSample {
                observed_at: now,
                poll: true,
                ..UtilizationSample::default()
            }
            .with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.3,
                    reset: Some(now + 3 * 86_400_000),
                    observed_at: now,
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    fake_upstream::wait_for_async(std::time::Duration::from_secs(2), || {
        let router = parts.router.clone();
        async move {
            router
                .reported_samples()
                .await
                .unwrap()
                .iter()
                .any(|(id, sample)| id == "business" && sample.polled.is_some())
                .then_some(())
        }
    })
    .await;
    let week_burn = share_per_hour(SEVEN_DAY, input(100));
    let (window, burn, equivalent) =
        read(&json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await);
    assert!(
        window.as_deref() == Some(SEVEN_DAY)
            && close(burn, week_burn)
            && close(equivalent, pro_week_per_hour),
        "a poll that carried the week alone moves the burn to the plain copy's share of the account's own week: \
         {:?} against {week_burn} and {pro_week_per_hour}",
        (window, burn, equivalent)
    );
}

/// A clean poll of `business` at `observed_at` carrying `windows`, each at 30% of its allowance.
fn poll_of(windows: &[&str], observed_at: i64) -> phoebis_routing::Report {
    let mut sample = phoebis_routing::UtilizationSample {
        observed_at,
        poll: true,
        ..phoebis_routing::UtilizationSample::default()
    };
    for window in windows {
        sample = sample.with(
            window,
            phoebis_routing::WindowReading {
                utilization: 0.3,
                reset: Some(observed_at + 3_600_000),
                observed_at,
                source: phoebis_routing::WindowSource::Poll,
                plan: None,
                tier: None,
            },
        );
    }
    phoebis_routing::Report::Sample {
        account_id: "business".into(),
        sample,
        incarnation: 1,
        source_request_id: None,
    }
}

/// Waits until the router holds `windows`, sorted, as `business`'s shared window set.
async fn polled_set(router: &phoebis_routing::RouterHandle, windows: &[&str]) {
    fake_upstream::wait_for_async(std::time::Duration::from_secs(2), || async {
        router
            .reported_samples()
            .await
            .unwrap()
            .iter()
            .any(|(id, sample)| {
                id == "business"
                    && sample.polled.as_ref().is_some_and(|set| {
                        set.iter().map(String::as_str).eq(windows.iter().copied())
                    })
            })
            .then_some(())
    })
    .await
}

/// Every reader that names `business` alone over request `r-fable` and its session `s1`: each share map it answers,
/// the session row's own three among them, then each burn point of the account with tokens.
async fn named_account_reads(
    app: &axum::Router,
    cookie: &str,
) -> (
    Vec<(&'static str, serde_json::Value)>,
    Vec<(&'static str, serde_json::Value)>,
) {
    use common::{get, json};
    let read = |path: &'static str| async move { json(get(app, path, Some(cookie)).await).await };
    let row = |body: &serde_json::Value, key: &str| {
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == key)
            .unwrap_or_else(|| panic!("a row for {key}: {body}"))["cost_shares"]
            .clone()
    };
    let burn_point = |body: &serde_json::Value, group: &str| {
        body["points"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["group"] == group && p["tokens"].as_u64().is_some_and(|t| t > 0))
            .unwrap_or_else(|| panic!("a point of {group} with tokens: {body}"))
            .clone()
    };
    let list = read("/admin/requests").await;
    let listed = list["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "r-fable")
        .unwrap_or_else(|| panic!("the request is listed: {list}"))["cost_shares"]
        .clone();
    let detail = read("/admin/requests/r-fable").await;
    let filtered =
        read("/admin/analytics/breakdown?range=24h&group_by=model&accounts=business").await;
    let by_account = read("/admin/analytics/breakdown?range=24h&group_by=account").await;
    let series = read("/admin/analytics/series?range=24h&group_by=account").await;
    let series_group = series["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap().iter())
        .find(|g| g["key"] == "business")
        .unwrap_or_else(|| panic!("a bucket carries business: {series}"))["cost_shares"]
        .clone();
    let account_burn = read("/admin/analytics/burn?range=24h").await;
    let session_burn =
        read("/admin/analytics/burn?range=24h&group_by=session&accounts=business").await;
    let session = session_row(&read("/admin/sessions").await, "s1");
    let agent_entry = session["agent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["agent_id"] == "main")
        .unwrap_or_else(|| panic!("main's entry on the session row: {session}"))["spend"]["shares"]
        ["business"]
        .clone();
    (
        vec![
            ("the request list", listed),
            (
                "the request detail",
                detail["request"]["cost_shares"].clone(),
            ),
            (
                "the breakdown under the filter",
                row(&filtered, "claude-fable-5-1"),
            ),
            (
                "the breakdown under the account grouping",
                row(&by_account, "business"),
            ),
            ("the series under the account grouping", series_group),
            (
                "the session row",
                session["spend"]["shares"]["business"].clone(),
            ),
            (
                "the session row's probes",
                session["spend"]["probes"]["shares"]["business"].clone(),
            ),
            ("the session row's agent entry", agent_entry),
        ],
        vec![
            (
                "the account's burn point",
                burn_point(&account_burn, "business"),
            ),
            ("the session's burn point", burn_point(&session_burn, "s1")),
        ],
    )
}

#[tokio::test]
async fn a_named_accounts_shares_leave_out_the_window_its_set_lacks() {
    use common::login;
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    let now = common::NOW;
    let parts =
        common::admin_parts_full_costed(phoebis_ledger::test_tables::subscription_costs()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    // The landing records the attempt's tier off the router's registry snapshot, so the tier lands first.
    name_the_tier(&parts.store, "business", "default_claude_max_20x");
    parts.router.reload().await.unwrap();
    // Fable carries a week of its own, which stays whatever the shared set holds. The request is main's, so the
    // session row answers an agent entry beside its own spend, and a probe of the session at the same instant gives
    // the row's probes a share map of their own.
    let at = now - 3_600_000;
    for (id, kind, origin, agent_id) in [
        ("r-fable", "inference", "client", Some("main")),
        ("r-fable-probe", "probe", "keepalive", None),
    ] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 500),
                key_id: Some("desktop".into()),
                pool_id: Some("main".into()),
                session_id: Some("s1".into()),
                agent_id: agent_id.map(String::from),
                model: "claude-fable-5-1".into(),
                canonical_model: "claude-fable-5-1".into(),
                kind: kind.into(),
                origin: origin.into(),
                status: Some(200),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            attempts: vec![AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "business".into(),
                account_kind: phoebis_store::AccountKind::Oauth,
                started_at: at,
                outcome: "succeeded".into(),
                input_tokens: Some(1_000),
                output_tokens: Some(100),
                ..Default::default()
            }],
            bodies: None,
        });
        fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
            parts.db.request(id).await.unwrap()
        })
        .await;
    }
    let keys = |shares: &serde_json::Value| {
        let mut keys: Vec<String> = shares
            .as_object()
            .unwrap_or_else(|| panic!("a share map: {shares}"))
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    parts
        .router
        .report(poll_of(&["five_hour", "seven_day"], now - 60_000))
        .await;
    polled_set(&parts.router, &["five_hour", "seven_day"]).await;
    let (shares, points) = named_account_reads(&parts.app, &cookie).await;
    for (reader, map) in &shares {
        assert_eq!(
            keys(map),
            ["five_hour", "seven_day", "seven_day_fable"],
            "an account whose set holds both windows keeps its 5-hour share on {reader}: {map}"
        );
    }
    for (reader, point) in &points {
        assert!(
            point["five_hour_share"].is_number() && point["seven_day_share"].is_number(),
            "an account whose set holds both windows keeps its 5-hour share on {reader}: {point}"
        );
    }
    parts.router.report(poll_of(&["seven_day"], now)).await;
    polled_set(&parts.router, &["seven_day"]).await;
    let (shares, points) = named_account_reads(&parts.app, &cookie).await;
    for (reader, map) in &shares {
        assert_eq!(
            keys(map),
            ["seven_day", "seven_day_fable"],
            "the poll dropped the 5-hour window, so {reader} answers no 5-hour share and keeps the model week: \
             {map}"
        );
    }
    for (reader, point) in &points {
        assert!(
            point["five_hour_share"].is_null() && point["seven_day_share"].is_number(),
            "the poll dropped the 5-hour window, so {reader} answers its 5-hour share null beside the week's: \
             {point}"
        );
    }
    parts
        .router
        .report(poll_of(&["five_hour"], now + 60_000))
        .await;
    polled_set(&parts.router, &["five_hour"]).await;
    let (_, points) = named_account_reads(&parts.app, &cookie).await;
    for (reader, point) in &points {
        assert!(
            point["five_hour_share"].is_number() && point["seven_day_share"].is_null(),
            "the poll dropped the 7-day window, so {reader} answers its 7-day share null beside the 5-hour one: \
             {point}"
        );
    }
}

#[tokio::test]
async fn a_plan_funded_request_on_a_tier_the_cost_model_does_not_size_keeps_the_accounts_key_over_an_empty_share_map(
) {
    use common::{get, json, login};
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    let now = common::NOW;
    let parts =
        common::admin_parts_full_costed(phoebis_ledger::test_tables::subscription_costs()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    // An enterprise plan is one the router prices by its tier, and the test cost model sizes no enterprise tier.
    parts
        .store
        .set_profile(
            "business",
            r#"{"organization":{"organization_type":"claude_enterprise"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_enterprise".into()),
                ..Default::default()
            },
        )
        .unwrap();
    parts.router.reload().await.unwrap();
    let at = now - 3_600_000;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-unsized".into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
            agent_id: Some("main".into()),
            model: "claude-opus-5-5".into(),
            canonical_model: "claude-opus-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-unsized".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: phoebis_store::AccountKind::Oauth,
            started_at: at,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..Default::default()
        }],
        bodies: None,
    });
    fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        parts.db.request("r-unsized").await.unwrap()
    })
    .await;
    let session = session_row(
        &json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await,
        "s1",
    );
    assert_eq!(
        session["spend"]["shares"],
        serde_json::json!({"business": {}}),
        "no window priced, and the account's key still marks that a plan paid: {session}"
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
            burn_unmetered_model: None,
            last_input_tokens: 100,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: None,
            mark: None,
            auxiliary_only: false,
            effort: None,
            ended: false,
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
            burn_unmetered_model: None,
            last_input_tokens: 100,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: None,
            mark: None,
            auxiliary_only: false,
            effort: None,
            ended: false,
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
            row("pin-no-request")["forked_from"].as_str(),
            row("pin-no-request")["fork_depth"].as_i64(),
        ),
        (None, None),
        "a pin the ledger holds no client request for at all reads null on both fork fields"
    );
}

#[tokio::test]
async fn a_pin_used_within_the_hour_names_its_session_to_the_fork_facts_when_its_requests_predate_the_window(
) {
    use common::{get, json, landed, login, NOW};
    use phoebis_ledger::{Record, RequestRecord};
    let parts = sessions_harness_with(|generation| {
        vec![context_pin(
            "pinned-forked",
            "main",
            "claude-opus-5",
            "business",
            generation,
            10_000,
            NOW - 1_000,
        )]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    // Older than the 24-hour activity window, so the activity read never sees the session and only its pin can
    // name it to the fork facts.
    let at = NOW - 90_000_000;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-forked-old".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            key_generation: common::generation(&parts.store, "desktop"),
            session_id: Some("pinned-forked".into()),
            model: "claude-opus-5".into(),
            canonical_model: "claude-opus-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            forked_from: Some("base".into()),
            fork_depth: Some(2),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    });
    landed(&parts, "r-forked-old").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = session_row(&sessions, "pinned-forked");
    assert_eq!(
        (
            row["forked_from"].as_str(),
            row["fork_depth"].as_i64(),
            row["spend"]["requests"].as_u64()
        ),
        (Some("base"), Some(2), Some(1)),
        "the pin lists the session with no request in the window, still names it to the fork facts and counts the \
         request over the session's life: {row}"
    );
}

#[tokio::test]
async fn a_request_groups_under_its_own_session_with_that_sessions_pin() {
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
            burn_unmetered_model: None,
            last_input_tokens: 100,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: None,
            mark: None,
            auxiliary_only: false,
            effort: None,
            ended: false,
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
        from_account_id: Some("direct".into()),
    }));

    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["spend"]["requests"].as_u64() == Some(1))
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
            rows[0]["spend"]["requests"].as_u64(),
        ),
        (Some("s1"), Some(true), Some(1)),
        "the row is the session's own"
    );

    let detail = wait_json(&parts.app, "/admin/requests/r-plain", &cookie, |v| {
        v["events"].as_array().is_some_and(|e| !e.is_empty())
    })
    .await;
    assert_eq!(detail["request"]["session_id"].as_str(), Some("s1"));
    assert!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "pin_migrated"),
        "the request's events carry the move of the pin it was routed by: {detail}"
    );
}

/// A pin of `agent` on `session` holding a last prompt of `last_input_tokens`, used at `last_used_at` and refreshed
/// then too, as a turn that refreshes the pin stamps both instants.
fn context_pin(
    session: &str,
    agent: &str,
    model: &str,
    account: &str,
    key_generation: i64,
    last_input_tokens: u64,
    last_used_at: i64,
) -> phoebis_store::PinRow {
    phoebis_store::PinRow {
        agent_id: agent.into(),
        model: model.into(),
        last_input_tokens,
        last_used_at,
        last_refresh_at: last_used_at,
        lane: phoebis_routing::lane_key(model, "standard", "none", "-"),
        ..burning_opus_pin(session, account, key_generation, common::NOW, 0.0, 0.0)
    }
}

/// Registers `session` with the companion and each of `agents` under it, main and each agent with one observed turn so
/// the keepalive's view carries its recipe, then releases each of `released` as the mod reports it.
fn register_companion(parts: &common::Full, session: &str, agents: &[&str], released: &[&str]) {
    let generation = common::generation(&parts.store, "desktop");
    let mut sequence = 0;
    let mut report = |event: &str, agent: Option<&str>, state: Option<&str>| {
        sequence += 1;
        let request_generation = state.map(|_| {
            parts
                .keepalive
                .agent_status("desktop", session, agent.unwrap())
                .unwrap()
                .request_generation
        });
        parts
            .keepalive
            .apply_event(
                "desktop",
                generation,
                phoebis_keepalive::Event {
                    event: event.into(),
                    session_id: session.into(),
                    instance_id: "i1".into(),
                    sequence,
                    agent_id: agent.map(String::from),
                    state: state.map(String::from),
                    request_generation,
                    ..Default::default()
                }
                .try_into()
                .unwrap(),
            )
            .unwrap();
    };
    report("session_start", None, None);
    let observe = |agent: &str| {
        parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: format!("k-{session}-{agent}"), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: generation, pool_id: Some("main".into()), session_id: Some(session.into()), agent_id: Some(agent.to_string()), parent_agent_id: (agent != "main").then(|| "main".into()), owner_id: None, model: "claude-opus-5".into(), canonical_model: "claude-opus-5".into(), wire_model: "claude-opus-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-opus-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: common::NOW, fallback_digest: None }));
    };
    observe("main");
    for agent in agents {
        report("agent_start", Some(agent), None);
        observe(agent);
    }
    for agent in released {
        report("state", Some(agent), Some("released"));
    }
}

/// A client request of `agent` on `session` under the desktop key's `key_generation`, naming its model `model`.
fn spelled_request(
    id: &str,
    session: &str,
    agent: &str,
    model: &str,
    at: i64,
    key_generation: i64,
) -> phoebis_ledger::Record {
    phoebis_ledger::Record::Request {
        request: phoebis_ledger::RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            key_generation,
            session_id: Some(session.into()),
            agent_id: Some(agent.into()),
            model: model.into(),
            canonical_model: model.trim_end_matches("[1m]").into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    }
}

/// A row's prompt, effort and agent lines, each `None` where the row leaves it out.
fn context_of(row: &serde_json::Value) -> [Option<serde_json::Value>; 3] {
    ["last_input_tokens", "effort", "agent_contexts"].map(|member| row.get(member).cloned())
}

/// The context window members a row or one of its lines answers, which the route leaves out.
fn dropped_members(row: &serde_json::Value) -> Vec<String> {
    let lines = row["agent_contexts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    std::iter::once(row)
        .chain(lines.iter())
        .flat_map(|value| {
            [
                "cached_prefix_tokens",
                "context_share",
                "context_window_tokens",
            ]
            .into_iter()
            .filter(move |member| value.get(*member).is_some())
            .map(String::from)
        })
        .collect()
}

/// An agent line as the route answers it, under the harness pins' five-minute TTL and with no first-seen record, with
/// the agent's lifecycle word where the engine holds its record. The harness pins burn nothing, so a line's burn reads
/// 0 with `burn_at` at the rig's clock, in the unit of the account its own pin sits on: a share of the 5-hour window
/// on `business`, whose tier the harness names, with plan dollars of 0 on a model the harness's price list names, as
/// it names every model these cases pin, and dollars with no window, no Pro-week figure and no plan dollars on the
/// API-key account `direct`. A `context_pin` was written ten milliseconds before the rig's clock and refreshed at its
/// last use, so its cache stays warm until that use plus the TTL.
// Each argument is a member the case pins, so a struct of them would only rename the call sites.
#[allow(clippy::too_many_arguments)]
fn agent_line(
    agent: &str,
    model: &str,
    account: &str,
    effort: Option<&str>,
    last_input_tokens: u64,
    last_used_at: i64,
    warm: bool,
    lifecycle: Option<&str>,
) -> serde_json::Value {
    let plan = account == "business";
    serde_json::json!({
        "agent_id": agent,
        "model": model,
        "account_id": account,
        "effort": effort,
        "last_input_tokens": last_input_tokens,
        "cache_ttl_ms": 300_000,
        "last_used_at": last_used_at,
        "warm": warm,
        "warm_until": last_used_at + 300_000,
        "burn": 0.0,
        "burn_unit": if plan { "window_share" } else { "usd" },
        "burn_window": plan.then_some("five_hour"),
        "burn_equivalent": plan.then_some(0.0),
        "burn_usd": plan.then_some(0.0),
        "burn_at": common::NOW,
        "created_at": null,
        "pinned_at": common::NOW - 10,
        "ended": false,
        "lifecycle": lifecycle,
    })
}

#[tokio::test]
async fn a_plain_session_row_answers_its_pins_prompt_and_effort_and_lists_the_pin_as_mains_line() {
    use common::{login, wait_json, NOW};
    use serde_json::{json, Value};
    let parts = sessions_harness_with(|generation| {
        vec![
            phoebis_store::PinRow {
                effort: Some("high".into()),
                ..context_pin(
                    "s-plain",
                    "main",
                    "claude-opus-5",
                    "business",
                    generation,
                    50_000,
                    NOW - 5,
                )
            },
            context_pin(
                "s-fresh",
                "main",
                "claude-opus-5",
                "business",
                generation,
                0,
                NOW - 5,
            ),
        ]
    })
    .await;
    let generation = common::generation(&parts.store, "desktop");
    let cookie = login(&parts.app, "admin-secret").await;
    // A request of the key id's earlier holder, so the list carries a historical row.
    parts.ledger.record(spelled_request(
        "r-earlier",
        "s-earlier",
        "main",
        "claude-opus-5",
        NOW - 2_000,
        generation - 1,
    ));
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s-earlier")
    })
    .await;
    assert_eq!(
        context_of(&session_row(&sessions, "s-plain")),
        [
            Some(json!(50_000)),
            Some(json!("high")),
            Some(json!([agent_line(
                "main",
                "claude-opus-5",
                "business",
                Some("high"),
                50_000,
                NOW - 5,
                true,
                None
            )])),
        ],
        "the pin's prompt and effort on the row, and the pin as main's line, warm a few milliseconds after its last use"
    );
    assert_eq!(
        context_of(&session_row(&sessions, "s-fresh")),
        [
            Some(json!(0)),
            Some(Value::Null),
            Some(json!([agent_line(
                "main",
                "claude-opus-5",
                "business",
                None,
                0,
                NOW - 5,
                true,
                None
            )])),
        ],
        "a pin with no turn yet reads no prompt, and a request that named no effort reads none"
    );
    let earlier = session_row(&sessions, "s-earlier");
    assert_eq!(earlier["historical"], json!(true));
    assert_eq!(
        context_of(&earlier),
        [Some(Value::Null), Some(Value::Null), Some(json!([]))],
        "a historical row holds no pin and draws no line"
    );
    for row in sessions["sessions"].as_array().unwrap() {
        assert_eq!(
            dropped_members(row),
            Vec::<String>::new(),
            "no row or line answers a context window: {row}"
        );
    }
}

#[tokio::test]
async fn a_companion_session_lists_main_and_each_unreleased_agents_conversation_pins_as_lines() {
    use common::{login, wait_json, NOW};
    use serde_json::json;
    let parts = sessions_harness_with(|generation| {
        let pin = |session, agent, model, account, prompt, used| {
            context_pin(session, agent, model, account, generation, prompt, used)
        };
        vec![
            phoebis_store::PinRow {
                effort: Some("high".into()),
                ..pin("s1", "main", "claude-opus-5", "business", 80_000, NOW - 5)
            },
            // Main's older conversation pin on another model draws a line of its own.
            pin("s1", "main", "claude-sonnet-5", "business", 5_000, NOW - 50),
            phoebis_store::PinRow {
                effort: Some("low".into()),
                ..pin("s1", "worker-1", "claude-opus-5", "direct", 30_000, NOW - 5)
            },
            pin("s1", "worker-1", "claude-sonnet-5", "direct", 0, NOW - 5),
            pin(
                "s1",
                "worker-2",
                "claude-sonnet-5",
                "business",
                12_000,
                NOW - 5,
            ),
            pin(
                "s1",
                "worker-3",
                "claude-opus-5",
                "business",
                7_000,
                NOW - 5,
            ),
            // An agent the companion never registered has no recipe.
            pin("s1", "ghost", "claude-opus-5", "business", 9_000, NOW - 5),
            pin("s2", "main", "claude-opus-5", "business", 20_000, NOW - 5),
            pin(
                "s2",
                "worker-9",
                "claude-opus-5",
                "business",
                1_000,
                NOW - 5,
            ),
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    register_companion(
        &parts,
        "s1",
        &["worker-1", "worker-2", "worker-3"],
        &["worker-3"],
    );
    register_companion(&parts, "s2", &["worker-9"], &["worker-9"]);
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        ["s1", "s2"].iter().all(|session| {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["session_id"] == *session && r["companion"] == "reporting")
        })
    })
    .await;
    let s1 = session_row(&sessions, "s1");
    assert_eq!(
        (s1["agent_id"].as_str(), s1["model"].as_str()),
        (Some("main"), Some("claude-opus-5")),
        "the row is main's conversation pin used last"
    );
    assert_eq!(
        context_of(&s1),
        [
            Some(json!(80_000)),
            Some(json!("high")),
            Some(json!([
                agent_line("main", "claude-opus-5", "business", Some("high"), 80_000, NOW - 5, true, Some("busy")),
                agent_line("main", "claude-sonnet-5", "business", None, 5_000, NOW - 50, true, Some("busy")),
                agent_line("worker-1", "claude-opus-5", "direct", Some("low"), 30_000, NOW - 5, true, Some("busy")),
                agent_line("worker-1", "claude-sonnet-5", "direct", None, 0, NOW - 5, true, Some("busy")),
                agent_line("worker-2", "claude-sonnet-5", "business", None, 12_000, NOW - 5, true, Some("busy")),
            ])),
        ],
        "main's figures on the row, and one line per conversation pin of main and of each agent the companion has not \
         released, main first, then by agent and model"
    );
    assert_eq!(
        context_of(&session_row(&sessions, "s2")),
        [
            Some(json!(20_000)),
            Some(serde_json::Value::Null),
            Some(json!([agent_line(
                "main",
                "claude-opus-5",
                "business",
                None,
                20_000,
                NOW - 5,
                true,
                Some("busy")
            )])),
        ],
        "a companion session whose every other agent is released lists main's line alone"
    );
    assert_eq!(dropped_members(&s1), Vec::<String>::new(), "{s1}");
}

#[tokio::test]
async fn a_session_whose_requests_named_the_long_context_twin_answers_its_pins_prompts_and_no_window(
) {
    use common::{login, wait_json, NOW};
    use serde_json::{json, Value};
    let parts = sessions_harness_with(|generation| {
        let pin = |session, agent, model, account, prompt| {
            context_pin(session, agent, model, account, generation, prompt, NOW - 5)
        };
        vec![
            pin("s-long", "main", "claude-opus-5", "business", 300_000),
            pin("s-long", "worker-1", "claude-opus-5", "direct", 100_000),
            pin("s-long", "worker-2", "claude-sonnet-5", "direct", 40_000),
            pin("s-root", "main", "claude-opus-5", "business", 300_000),
        ]
    })
    .await;
    let generation = common::generation(&parts.store, "desktop");
    let cookie = login(&parts.app, "admin-secret").await;
    register_companion(&parts, "s-long", &["worker-1", "worker-2"], &[]);
    for (id, session, agent, model, at) in [
        ("r1", "s-long", "main", "claude-opus-5", NOW - 60_000),
        ("r2", "s-long", "worker-1", "claude-opus-5", NOW - 40_000),
        ("r3", "s-long", "main", "claude-opus-5[1m]", NOW - 30_000),
        ("r4", "s-root", "main", "claude-opus-5[1m]", NOW - 30_000),
    ] {
        parts
            .ledger
            .record(spelled_request(id, session, agent, model, at, generation));
    }
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        [("s-long", 3), ("s-root", 1)]
            .iter()
            .all(|(session, requests)| {
                v["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["session_id"] == *session && r["spend"]["requests"] == *requests)
            })
    })
    .await;
    let line = |agent, model, account, prompt, lifecycle| {
        agent_line(
            agent,
            model,
            account,
            None,
            prompt,
            NOW - 5,
            true,
            lifecycle,
        )
    };
    assert_eq!(
        context_of(&session_row(&sessions, "s-long")),
        [
            Some(json!(300_000)),
            Some(Value::Null),
            Some(json!([
                line("main", "claude-opus-5", "business", 300_000, Some("busy")),
                line("worker-1", "claude-opus-5", "direct", 100_000, Some("busy")),
                line("worker-2", "claude-sonnet-5", "direct", 40_000, Some("busy")),
            ])),
        ],
        "main's newest request named the twin, and the row and its lines read each pin's prompt alone"
    );
    assert_eq!(
        context_of(&session_row(&sessions, "s-root")),
        [
            Some(json!(300_000)),
            Some(Value::Null),
            Some(json!([line(
                "main",
                "claude-opus-5",
                "business",
                300_000,
                None
            )])),
        ],
        "a session whose only request named the twin reads its pin's prompt"
    );
    for row in sessions["sessions"].as_array().unwrap() {
        assert_eq!(
            dropped_members(row),
            Vec::<String>::new(),
            "no row or line answers a context window: {row}"
        );
    }
}

#[tokio::test]
async fn a_session_row_reads_mains_conversation_pin_beside_a_later_auxiliary_only_pin_of_main() {
    use common::{login, wait_json, NOW};
    use serde_json::json;
    let parts = sessions_harness_with(|generation| {
        vec![
            phoebis_store::PinRow {
                effort: Some("high".into()),
                ..context_pin(
                    "s1",
                    "main",
                    "claude-opus-5",
                    "business",
                    generation,
                    40_000,
                    NOW - 50,
                )
            },
            // A title request renewed main's auxiliary-only pin after the conversation's last turn.
            phoebis_store::PinRow {
                auxiliary_only: true,
                effort: Some("low".into()),
                ..context_pin(
                    "s1",
                    "main",
                    "claude-haiku-4-5",
                    "direct",
                    generation,
                    1_000,
                    NOW - 5,
                )
            },
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1")
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (
            row["agent_id"].as_str(),
            row["model"].as_str(),
            row["account_id"].as_str(),
            row["last_used_at"].as_i64()
        ),
        (
            Some("main"),
            Some("claude-opus-5"),
            Some("business"),
            Some(NOW - 50)
        ),
        "the row reads main's conversation pin, never the auxiliary-only pin used later: {row}"
    );
    assert_eq!(
        context_of(&row),
        [
            Some(json!(40_000)),
            Some(json!("high")),
            Some(json!([agent_line(
                "main",
                "claude-opus-5",
                "business",
                Some("high"),
                40_000,
                NOW - 50,
                true,
                None
            )])),
        ],
        "main's line reads the conversation pin, and the auxiliary-only pin draws none"
    );
}

#[tokio::test]
async fn a_conversation_pin_reads_cold_on_its_row_and_line_from_its_last_refresh_though_an_auxiliary_request_used_it_since(
) {
    use common::{login, wait_json, NOW};
    use serde_json::json;
    let parts = sessions_harness_with(|generation| {
        // A title request used main's conversation pin after the turn that last refreshed it, so its last use sits
        // inside the five-minute TTL and its refresh six minutes back.
        vec![phoebis_store::PinRow {
            last_refresh_at: NOW - 360_000,
            ..context_pin(
                "s1",
                "main",
                "claude-opus-5",
                "business",
                generation,
                40_000,
                NOW - 5,
            )
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1")
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (
            row["warm"].clone(),
            row["agent_contexts"][0]["agent_id"].clone(),
            row["agent_contexts"][0]["warm"].clone(),
            row["agent_contexts"][0]["warm_until"].clone()
        ),
        (json!(false), json!("main"), json!(false), json!(NOW - 60_000)),
        "the cache entry lives from the pin's last refresh, so the row and main's line read cold though a request used \
         the pin inside the TTL, and the line's entry ended a minute back, five minutes after that refresh: {row}"
    );
}

#[tokio::test]
async fn a_session_whose_main_holds_an_auxiliary_pin_alone_reads_its_subagents_conversation_pin_and_no_main_line(
) {
    use common::{login, wait_json, NOW};
    use serde_json::json;
    let parts = sessions_harness_with(|generation| {
        vec![
            phoebis_store::PinRow {
                auxiliary_only: true,
                ..context_pin(
                    "s1",
                    "main",
                    "claude-haiku-4-5",
                    "business",
                    generation,
                    1_000,
                    NOW - 5,
                )
            },
            phoebis_store::PinRow {
                effort: Some("low".into()),
                ..context_pin(
                    "s1",
                    "worker-1",
                    "claude-opus-5",
                    "direct",
                    generation,
                    25_000,
                    NOW - 50,
                )
            },
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1")
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (
            row["agent_id"].as_str(),
            row["model"].as_str(),
            row["account_id"].as_str(),
            row["pinned"].as_bool()
        ),
        (
            Some("worker-1"),
            Some("claude-opus-5"),
            Some("direct"),
            Some(true)
        ),
        "with no conversation pin of main the row reads the subagent's: {row}"
    );
    assert_eq!(
        context_of(&row),
        [
            Some(json!(25_000)),
            Some(json!("low")),
            Some(json!([agent_line(
                "worker-1",
                "claude-opus-5",
                "direct",
                Some("low"),
                25_000,
                NOW - 50,
                true,
                None
            )])),
        ],
        "the subagent's line, and none for main's auxiliary-only pin"
    );
}

#[tokio::test]
async fn a_session_whose_one_pin_is_mains_auxiliary_one_reads_a_pinless_row_from_its_activity_and_no_line(
) {
    use common::{login, wait_json, NOW};
    use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
    use serde_json::{json, Value};
    let parts = sessions_harness_with(|generation| {
        vec![phoebis_store::PinRow {
            auxiliary_only: true,
            ..context_pin(
                "s1",
                "main",
                "claude-haiku-4-5",
                "business",
                generation,
                1_000,
                NOW - 5,
            )
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    // The title request that placed the pin, its attempt on another account than the pin's, so the row's account
    // tells the ledger's from the pin's.
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-title".into(),
            received_at: NOW - 1_000,
            finished_at: Some(NOW - 500),
            key_id: Some("desktop".into()),
            key_generation: common::generation(&parts.store, "desktop"),
            session_id: Some("s1".into()),
            agent_id: Some("main".into()),
            model: "claude-haiku-4-5".into(),
            canonical_model: "claude-haiku-4-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            request_class: Some("title".into()),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-title".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: phoebis_store::AccountKind::ApiKey,
            started_at: NOW - 1_000,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        bodies: None,
    });
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["spend"]["requests"] == 1)
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (
            row["agent_id"].clone(),
            row["model"].as_str(),
            row["account_id"].as_str(),
            row["pinned"].as_bool()
        ),
        (
            Value::Null,
            Some("claude-haiku-4-5"),
            Some("direct"),
            Some(false)
        ),
        "no conversation pin stands, so the row reads the ledger's model and serving account: {row}"
    );
    for member in [
        "epoch",
        "created_at",
        "last_used_at",
        "burn",
        "burn_unit",
        "burn_equivalent",
        "burn_at",
        "runway_ms",
        "runway_bounded",
        "factor",
        "speed",
        "cycle_ms",
        "cache_ttl_ms",
        "last_input_tokens",
        "effort",
        "last_refresh_at",
        "shed_from",
        "shed_reason",
    ] {
        assert_eq!(
            row[member],
            Value::Null,
            "{member} reads null on the row with no pin: {row}"
        );
    }
    assert_eq!(
        row["agent_contexts"],
        json!([]),
        "the auxiliary-only pin draws no line"
    );
}

#[tokio::test]
async fn a_session_whose_every_request_was_refused_answers_its_newest_requests_client_and_a_served_one_keeps_its_own(
) {
    use common::{get, json, landed, login, NOW};
    use phoebis_ledger::{Record, RequestRecord};
    use serde_json::Value;
    let parts = sessions_harness_with(|_| Vec::new()).await;
    let generation = common::generation(&parts.store, "desktop");
    let cookie = login(&parts.app, "admin-secret").await;
    let served = |id: &str, session: &str, at: i64, user_agent: Option<&str>| RequestRecord {
        id: id.into(),
        received_at: at,
        finished_at: Some(at + 3),
        key_id: Some("desktop".into()),
        key_generation: generation,
        session_id: Some(session.into()),
        agent_id: Some("main".into()),
        model: "claude-opus-5".into(),
        canonical_model: "claude-opus-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        user_agent: user_agent.map(String::from),
        ..Default::default()
    };
    // What `finish` sets on a refusal, since the harness's `record` skips the assembler.
    let refused = |id: &str, session: &str, at: i64, user_agent: &str| RequestRecord {
        outcome: "rejected".into(),
        refused: true,
        ..served(id, session, at, Some(user_agent))
    };
    for request in [
        refused(
            "r-refused",
            "s-refused",
            NOW - 1_000,
            "claude-cli/2.1.310 (external, cli)",
        ),
        // The served request sent no user agent, which the row keeps over the later refusal's.
        served("r-served", "s-served", NOW - 2_000, None),
        refused(
            "r-refused-later",
            "s-served",
            NOW - 1_000,
            "claude-cli/2.1.310 (external, cli)",
        ),
    ] {
        let id = request.id.clone();
        parts.ledger.record(Record::Request {
            request,
            attempts: vec![],
            bodies: None,
        });
        landed(&parts, &id).await;
    }
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    assert_eq!(
        (
            session_row(&sessions, "s-refused")["user_agent"].clone(),
            session_row(&sessions, "s-served")["user_agent"].clone(),
        ),
        (
            Value::from("claude-cli/2.1.310 (external, cli)"),
            Value::Null
        ),
        "a session whose every request was refused answers its newest request's client, and one with a served request \
         keeps the newest served request's, which sent none: {sessions}"
    );
}

#[tokio::test]
async fn a_sessions_creation_time_is_mains_first_placement_across_a_failover_and_a_second_pin_and_each_line_reads_its_own_agents(
) {
    use common::{get, json, login, NOW};
    use phoebis_routing::SelectResult;
    let parts = slow_down_harness(false).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let selected = |result: SelectResult| match result {
        SelectResult::Selected(selection) => selection,
        other => panic!("{other:?}"),
    };
    let first = selected(agent_turn(&parts, "s2", "main", "claude-sonnet-5", "r-1", NOW).await);
    assert_eq!(first.account.id, "business");
    // A rate limit on the request's account fails the pin over, and the session's next request lands on direct.
    let snapshot = parts.router.snapshot();
    let business = snapshot.account("business").unwrap();
    parts
        .clock_cell
        .store(NOW + 500, std::sync::atomic::Ordering::SeqCst);
    parts
        .router
        .report(phoebis_routing::Report::Cooldown {
            account_id: "business".into(),
            row: phoebis_store::CooldownRow {
                account_id: "business".into(),
                scope: phoebis_store::CooldownScope::Account,
                until: NOW + 3_600_000,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: business.incarnation,
            },
            pin: first.pin.clone(),
            incarnation: business.incarnation,
            lifecycle_generation: business.lifecycle_generation,
            credential_generation: snapshot.credential_generation("business").unwrap_or(1),
        })
        .await;
    for (agent, model, request_id, at) in [
        ("main", "claude-sonnet-5", "r-2", NOW + 1_000),
        // A second pin of main, on another model.
        ("main", "claude-opus-5", "r-3", NOW + 2_000),
        ("worker-1", "claude-sonnet-5", "r-4", NOW + 3_000),
    ] {
        parts
            .clock_cell
            .store(at, std::sync::atomic::Ordering::SeqCst);
        let landed = selected(agent_turn(&parts, "s2", agent, model, request_id, at).await);
        assert_eq!(landed.account.id, "direct", "{request_id}");
    }
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = session_row(&sessions, "s2");
    assert_eq!(
        (
            row["model"].as_str(),
            row["created_at"].as_i64(),
            row["pinned_at"].as_i64()
        ),
        (Some("claude-opus-5"), Some(NOW), Some(NOW + 2_000)),
        "the row reads main's newest conversation pin, the session's first placement and the instant that pin was \
         written: {row}"
    );
    let lines: Vec<(&str, &str, Option<i64>, Option<i64>)> = row["agent_contexts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| {
            (
                line["agent_id"].as_str().unwrap(),
                line["model"].as_str().unwrap(),
                line["created_at"].as_i64(),
                line["pinned_at"].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("main", "claude-opus-5", Some(NOW), Some(NOW + 2_000)),
            ("main", "claude-sonnet-5", Some(NOW), Some(NOW + 1_000)),
            (
                "worker-1",
                "claude-sonnet-5",
                Some(NOW + 3_000),
                Some(NOW + 3_000)
            ),
        ],
        "each line reads its own agent's first placement and the instant its pin was written on its account, the \
         failover's landing for main's first pin"
    );
}

#[tokio::test]
async fn a_pinless_row_whose_main_holds_an_auxiliary_pin_alone_reads_mains_record_as_its_creation_time(
) {
    use common::{login, wait_json, NOW};
    use serde_json::json;
    let seed = sessions_seed(|generation| {
        vec![phoebis_store::PinRow {
            auxiliary_only: true,
            ..context_pin(
                "s1",
                "main",
                "claude-haiku-4-5",
                "business",
                generation,
                1_000,
                NOW - 5,
            )
        }]
    })
    .await;
    seed.store
        .insert_first_seen(&phoebis_store::FirstSeenRow {
            key_id: "desktop".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            first_seen_at: NOW - 1_000,
        })
        .unwrap();
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.record(spelled_request(
        "r-title",
        "s1",
        "main",
        "claude-haiku-4-5",
        NOW - 1_000,
        common::generation(&parts.store, "desktop"),
    ));
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["spend"]["requests"] == 1)
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (
            row["pinned"].as_bool(),
            row["created_at"].as_i64(),
            row["agent_contexts"].clone()
        ),
        (Some(false), Some(NOW - 1_000), json!([])),
        "the row holds no conversation pin and reads its creation time from main's record: {row}"
    );
}

/// Main's auxiliary pin and the subagent's pin carry no record of main, as rows placed before the record existed do.
#[tokio::test]
async fn a_session_whose_main_has_no_record_reads_no_creation_time_while_its_subagents_line_reads_its_own(
) {
    use common::{login, wait_json, NOW};
    let seed = sessions_seed(|generation| {
        vec![
            phoebis_store::PinRow {
                auxiliary_only: true,
                ..context_pin(
                    "s1",
                    "main",
                    "claude-haiku-4-5",
                    "business",
                    generation,
                    1_000,
                    NOW - 5,
                )
            },
            context_pin(
                "s1",
                "worker-1",
                "claude-opus-5",
                "direct",
                generation,
                25_000,
                NOW - 50,
            ),
        ]
    })
    .await;
    seed.store
        .insert_first_seen(&phoebis_store::FirstSeenRow {
            key_id: "desktop".into(),
            session_id: "s1".into(),
            agent_id: "worker-1".into(),
            first_seen_at: NOW - 2_000,
        })
        .unwrap();
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1")
    })
    .await;
    let row = session_row(&sessions, "s1");
    assert_eq!(
        (
            row["agent_id"].as_str(),
            row["created_at"].clone(),
            row["agent_contexts"][0]["agent_id"].as_str(),
            row["agent_contexts"][0]["created_at"].as_i64()
        ),
        (
            Some("worker-1"),
            serde_json::Value::Null,
            Some("worker-1"),
            Some(NOW - 2_000)
        ),
        "the row reads the subagent's pin and no creation time, and the subagent's line its own record: {row}"
    );
}

#[tokio::test]
async fn a_session_no_companion_reports_leaves_after_the_timeout_and_its_old_lines_leave_while_newer_ones_stay(
) {
    use common::{login, wait_json, NOW};
    use serde_json::json;
    // A ten-minute timeout, so every pin below stays inside the pool's one-hour idle TTL.
    const IDLE_MS: i64 = 600_000;
    let parts = sessions_harness_bounded(
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_millis(IDLE_MS as u64),
        |generation| {
            let pin = |session, agent, account, used| {
                context_pin(
                    session,
                    agent,
                    "claude-opus-5",
                    account,
                    generation,
                    10_000,
                    used,
                )
            };
            vec![
                pin("s-idle", "main", "business", NOW - IDLE_MS - 1),
                pin("s-mixed", "main", "business", NOW - 5),
                pin("s-mixed", "worker-1", "direct", NOW - 400_000),
                pin("s-mixed", "worker-2", "direct", NOW - IDLE_MS - 1),
                pin("s-companion", "main", "business", NOW - IDLE_MS - 1),
            ]
        },
    )
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    register_companion(&parts, "s-companion", &[], &[]);
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s-companion" && r["companion"] == "reporting")
    })
    .await;
    let listed: Vec<&str> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["session_id"].as_str())
        .collect();
    assert!(
        !listed.contains(&"s-idle"),
        "a session no companion reports leaves once its last use is older than the timeout: {listed:?}"
    );
    assert_eq!(
        context_of(&session_row(&sessions, "s-mixed"))[2],
        Some(json!([
            agent_line("main", "claude-opus-5", "business", None, 10_000, NOW - 5, true, None),
            agent_line("worker-1", "claude-opus-5", "direct", None, 10_000, NOW - 400_000, false, None),
        ])),
        "a line whose pin was used within the timeout stays, warm or cold by its cache TTL, and an older one leaves"
    );
    assert_eq!(
        context_of(&session_row(&sessions, "s-companion"))[2],
        Some(json!([agent_line(
            "main",
            "claude-opus-5",
            "business",
            None,
            10_000,
            NOW - IDLE_MS - 1,
            false,
            Some("busy")
        )])),
        "a companion row stays past the timeout within the session expiry, its line kept by the engine's own lifecycle"
    );
}

/// The session ids `GET /admin/sessions` lists once every one of `requests` is counted, under a ten-minute timeout.
async fn listed_under_a_ten_minute_timeout(
    pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>,
    requests: &[(&str, &str, i64)],
) -> Vec<String> {
    use common::{login, wait_json};
    let parts = sessions_harness_bounded(
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_secs(600),
        pins,
    )
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let generation = common::generation(&parts.store, "desktop");
    for (id, session, at) in requests {
        parts.ledger.record(spelled_request(
            id,
            session,
            "main",
            "claude-opus-5",
            *at,
            generation,
        ));
    }
    wait_json(&parts.app, "/admin/requests", &cookie, |v| {
        v["requests"].as_array().unwrap().len() == requests.len()
    })
    .await;
    let sessions =
        common::json(common::get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["session_id"].as_str().map(String::from))
        .collect()
}

#[tokio::test]
async fn a_session_no_companion_reports_and_holding_no_pin_leaves_once_its_last_request_is_older_than_the_timeout(
) {
    use common::NOW;
    let listed = listed_under_a_ten_minute_timeout(
        |_| Vec::new(),
        &[
            ("r-idle", "s-idle", NOW - 601_000),
            ("r-recent", "s-recent", NOW - 1_000),
        ],
    )
    .await;
    assert_eq!(
        listed,
        ["s-recent"],
        "the row's one instant is its last request, which leaves it past the timeout and keeps it inside"
    );
}

#[tokio::test]
async fn a_session_no_companion_reports_stays_while_its_last_request_is_inside_the_timeout_though_its_pin_is_older(
) {
    use common::NOW;
    let listed = listed_under_a_ten_minute_timeout(
        |generation| {
            vec![context_pin(
                "s-auxiliary-traffic",
                "main",
                "claude-opus-5",
                "business",
                generation,
                10_000,
                NOW - 601_000,
            )]
        },
        &[("r-title", "s-auxiliary-traffic", NOW - 1_000)],
    )
    .await;
    assert_eq!(
        listed,
        ["s-auxiliary-traffic"],
        "the later of the pin's last use and the last request decides, and the request is inside the timeout"
    );
}

#[tokio::test]
async fn a_session_no_companion_reports_stays_while_a_subagents_pin_is_inside_the_timeout_though_mains_is_older(
) {
    use common::{get, json, login, NOW};
    use std::sync::atomic::Ordering;
    // A ten-minute timeout, so every pin below stays inside the pool's one-hour idle TTL.
    const IDLE_MS: i64 = 600_000;
    let parts = sessions_harness_bounded(
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_millis(IDLE_MS as u64),
        |generation| {
            let pin = |agent, account, used| {
                context_pin(
                    "s-in-flight",
                    agent,
                    "claude-opus-5",
                    account,
                    generation,
                    10_000,
                    used,
                )
            };
            vec![
                pin("main", "business", NOW - IDLE_MS - 1),
                pin("worker-1", "direct", NOW - 5),
            ]
        },
    )
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    // The subagent's request is still in flight, so the ledger holds no row of the session.
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s-in-flight")
        .map(|r| {
            (
                r["agent_id"].clone(),
                r["last_used_at"].clone(),
                r["last_request_at"].clone(),
                r["warm"].clone(),
                r["agent_contexts"].clone(),
            )
        });
    assert_eq!(
        row,
        Some((
            serde_json::json!("main"),
            serde_json::json!(NOW - 5),
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!([agent_line(
                "worker-1",
                "claude-opus-5",
                "direct",
                None,
                10_000,
                NOW - 5,
                true,
                None
            )]),
        )),
        "the subagent's use inside the timeout keeps the row, which reads main's pin with the subagent's newer use as \
         its last use and reads cold by main's own use past its cache TTL, and main's older line leaves: {sessions}"
    );
    // Past the timeout from the subagent's last use, still inside the session expiry and the pool's idle TTL.
    parts
        .clock_cell
        .store(NOW - 5 + IDLE_MS + 1, Ordering::SeqCst);
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    assert!(
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["session_id"] != "s-in-flight"),
        "the row leaves once the session's newest pin use is older than the timeout: {sessions}"
    );
}

#[tokio::test]
async fn an_ended_session_with_no_pin_and_no_request_leaves_once_its_end_is_older_than_the_timeout()
{
    use common::{get, json, login, NOW};
    use std::sync::atomic::Ordering;
    let parts = sessions_harness_bounded(
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_secs(600),
        |_| Vec::new(),
    )
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let generation = common::generation(&parts.store, "desktop");
    // Each session registers and ends at `at`, sending no request, as a client opened and closed at once does.
    for (session, at) in [
        ("s-ended-long-ago", NOW - 601_000),
        ("s-ended-now", NOW - 1_000),
    ] {
        parts.clock_cell.store(at, Ordering::SeqCst);
        for (sequence, event) in [(1, "session_start"), (2, "session_end")] {
            parts
                .keepalive
                .apply_event(
                    "desktop",
                    generation,
                    phoebis_keepalive::Event {
                        event: event.into(),
                        session_id: session.into(),
                        instance_id: "i1".into(),
                        sequence,
                        ..Default::default()
                    }
                    .try_into()
                    .unwrap(),
                )
                .unwrap();
        }
    }
    parts.clock_cell.store(NOW, Ordering::SeqCst);
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let listed: Vec<(&str, &str, Option<i64>)> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["session_id"].as_str().unwrap(),
                r["companion"].as_str().unwrap(),
                r["companion_since"].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [("s-ended-now", "none", Some(NOW - 1_000))],
        "an ended session with neither a pin nor a request is measured by its end, which the timeout bounds"
    );
}

/// A guard: the session expiry already leaves a pin older than it out of the read, so a timeout longer than the expiry
/// never keeps a row the expiry dropped.
#[tokio::test]
async fn the_lesser_of_the_timeout_and_the_session_expiry_bounds_a_session_no_companion_reports() {
    use common::{login, wait_json, NOW};
    let parts = sessions_harness_bounded(
        std::time::Duration::from_secs(1_800),
        std::time::Duration::from_secs(3_600),
        |generation| {
            let pin = |session, used| {
                context_pin(
                    session,
                    "main",
                    "claude-opus-5",
                    "business",
                    generation,
                    10_000,
                    used,
                )
            };
            vec![
                pin("s-past-expiry", NOW - 2_000_000),
                pin("s-inside", NOW - 1_000_000),
            ]
        },
    )
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s-inside")
    })
    .await;
    let listed: Vec<&str> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["session_id"].as_str())
        .collect();
    assert_eq!(
        listed,
        ["s-inside"],
        "idle for 2,000 s past a 30-minute expiry, a row leaves under the one-hour timeout"
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
    for member in [
        "identified_by",
        "identity_depth",
        "identity_root",
        "routing_session_id",
    ] {
        assert!(
            row.get(member).is_none(),
            "the row carries no {member}: {row}"
        );
        assert!(
            detail["request"].get(member).is_none(),
            "the detail carries no {member}: {detail}"
        );
    }
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
            scope: phoebis_store::CooldownScope::parse(scope).unwrap(),
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
    let standing = || async {
        let shown = json(get(&parts.app, "/admin/accounts/business", Some(&cookie)).await).await;
        let mut scopes: Vec<String> = shown["cooldowns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["scope"].as_str().unwrap().to_string())
            .collect();
        scopes.sort_unstable();
        scopes
    };
    assert_eq!(standing().await, ["account", "fast", "fast_credits"]);
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

#[tokio::test]
async fn an_array_body_is_refused_as_no_object_on_the_pool_and_key_routes() {
    use common::{get, json, login, patch, post, put};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    for (route, body) in [
        (
            "/admin/pools",
            r#"{"id":"main","label":"Main","members":[{"account_id":"business","priority":0}]}"#,
        ),
        (
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        ),
    ] {
        assert_eq!(
            post(&app, route, body, Some(&cookie)).await.status(),
            201,
            "{route}"
        );
    }
    let pools = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let keys = json(get(&app, "/admin/keys", Some(&cookie)).await).await;
    // Serde's sequence form reads `[[]]` as a members list with no entry, so a parse that took it would empty the pool.
    for (method, route, body) in [
        ("POST", "/admin/pools", "[]"),
        ("PATCH", "/admin/pools/main", "[]"),
        ("PUT", "/admin/pools/main/members", "[]"),
        ("PUT", "/admin/pools/main/members", "[[]]"),
        ("POST", "/admin/keys", "[]"),
        ("PATCH", "/admin/keys/desktop", "[]"),
    ] {
        let response = match method {
            "POST" => post(&app, route, body, Some(&cookie)).await,
            "PATCH" => patch(&app, route, body, Some(&cookie)).await,
            _ => put(&app, route, body, Some(&cookie)).await,
        };
        let status = response.status().as_u16();
        assert_eq!(
            (status, json(response).await["error"].clone()),
            (400, serde_json::json!("body must be a JSON object")),
            "{method} {route} {body}"
        );
    }
    assert_eq!(
        json(get(&app, "/admin/pools", Some(&cookie)).await).await,
        pools,
        "no refused pool write landed"
    );
    assert_eq!(
        json(get(&app, "/admin/keys", Some(&cookie)).await).await,
        keys,
        "no refused key write landed"
    );
}

/// A key body naming a member the input does not read is refused whole with the pool routes' object sentence, so a
/// misspelled field never saves the rest of the body.
#[tokio::test]
async fn the_key_create_and_edit_refuse_an_unknown_member_and_take_a_known_body() {
    use common::{get, json, login, patch, post};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let pool = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","members":[{"account_id":"business","priority":0}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(pool.status(), 201);
    let keys = json(get(&app, "/admin/keys", Some(&cookie)).await).await;
    let refused = post(
        &app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main","labels":"misspelled"}"#,
        Some(&cookie),
    )
    .await;
    let status = refused.status().as_u16();
    assert_eq!(
        (status, json(refused).await["error"].clone()),
        (400, serde_json::json!("body must be a JSON object")),
        "the create refuses the unknown member"
    );
    assert_eq!(
        json(get(&app, "/admin/keys", Some(&cookie)).await).await,
        keys,
        "the refused create wrote no key"
    );
    let created = post(
        &app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201, "a known body creates the key");
    let refused = patch(
        &app,
        "/admin/keys/desktop",
        r#"{"label":"Renamed","labels":"misspelled"}"#,
        Some(&cookie),
    )
    .await;
    let status = refused.status().as_u16();
    assert_eq!(
        (status, json(refused).await["error"].clone()),
        (400, serde_json::json!("body must be a JSON object")),
        "the edit refuses the unknown member"
    );
    let listed = json(get(&app, "/admin/keys", Some(&cookie)).await).await;
    let desktop = listed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["id"] == "desktop")
        .expect("the created key is listed");
    assert_eq!(
        desktop["label"], "Desktop",
        "the refused edit wrote no label"
    );
    let edited = patch(
        &app,
        "/admin/keys/desktop",
        r#"{"label":"Renamed"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(edited.status(), 200, "a known body edits the key");
    assert_eq!(json(edited).await["label"], "Renamed");
}

#[tokio::test]
async fn an_array_body_is_refused_as_no_object_on_the_account_create_and_edit() {
    use common::{get, json, login, patch, post};
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let accounts = json(get(&app, "/admin/accounts", Some(&cookie)).await).await;
    // Serde's sequence form reads a filled array positionally, and every edit member defaults, so a parse that took
    // one would create the account or land the edit.
    let requests = [
        ("POST", "/admin/accounts", "[]"),
        (
            "POST",
            "/admin/accounts",
            r#"["arrayed","oauth","Arrayed"]"#,
        ),
        ("PATCH", "/admin/accounts/business", "[]"),
        (
            "PATCH",
            "/admin/accounts/business",
            r#"["Relabeled",false]"#,
        ),
    ];
    let mut answers = Vec::new();
    for (method, route, body) in requests {
        let response = match method {
            "POST" => post(&app, route, body, Some(&cookie)).await,
            _ => patch(&app, route, body, Some(&cookie)).await,
        };
        let status = response.status().as_u16();
        let answer = json(response).await;
        answers.push((
            format!("{method} {route} {body}"),
            status,
            answer["error"].clone(),
            answer["label"].clone(),
        ));
    }
    assert_eq!(
        answers,
        requests
            .map(|(method, route, body)| (
                format!("{method} {route} {body}"),
                400,
                serde_json::json!("body must be a JSON object"),
                serde_json::Value::Null,
            ))
            .to_vec()
    );
    assert_eq!(
        json(get(&app, "/admin/accounts", Some(&cookie)).await).await,
        accounts,
        "no refused account write landed"
    );
}
