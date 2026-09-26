mod common;

#[tokio::test]
async fn login_sets_a_cookie_and_viewer_cannot_mutate() {
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
    let viewer = post(
        &app,
        "/admin/session",
        r#"{"secret":"viewer-secret"}"#,
        None,
    )
    .await;
    let vcookie = cookie_of(&viewer);
    assert_eq!(
        get(&app, "/admin/accounts", Some(&vcookie)).await.status(),
        200
    );
    assert_eq!(
        post(&app, "/admin/accounts/business/pause", "", Some(&vcookie))
            .await
            .status(),
        403
    );
    assert_eq!(
        post(
            &app,
            "/admin/accounts/business/usage/refresh",
            "",
            Some(&vcookie)
        )
        .await
        .status(),
        200,
        "usage refresh is the one POST a viewer may call"
    );
    assert_eq!(
        delete(&app, "/admin/session", Some(&vcookie))
            .await
            .status(),
        204,
        "a viewer can end its own session"
    );
    assert_eq!(
        get(&app, "/admin/accounts", Some(&vcookie)).await.status(),
        401,
        "the logged-out cookie no longer resolves"
    );
    assert_eq!(
        post(&app, "/admin/accounts/business/pause", "", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(get(&app, "/admin/accounts", None).await.status(), 401);
}

#[tokio::test]
async fn the_cookie_carries_secure_exactly_when_plain_http_is_off() {
    use common::{post, post_from};
    let (loopback, _up) = common::admin_app().await;
    let plain = post(
        &loopback,
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
    )
    .await;
    assert_eq!(plain.status(), 204);
    assert!(!cookie_attributes(&plain).contains("Secure"));

    let (pinned, _up) = common::admin_app_with_origin("https://phoebis.example").await;
    let secure = post_from(
        &pinned,
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
        "https://phoebis.example",
    )
    .await;
    assert_eq!(secure.status(), 204);
    // The whole attribute, not a substring of one: `contains` would also pass on a cookie value that happened
    // to carry the word.
    assert!(cookie_attributes(&secure).ends_with("; Secure"));
}

#[tokio::test]
async fn an_origin_set_under_plain_http_is_still_enforced() {
    use common::{json, post_from};
    let (app, _up) = common::admin_app_plain_with_origin("http://phoebis.lan:8081").await;
    let forged = post_from(
        &app,
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
        "http://elsewhere",
    )
    .await;
    assert_eq!(forged.status(), 403);
    assert_eq!(json(forged).await["error"], "origin is not allowed");

    let allowed = post_from(
        &app,
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
        "http://phoebis.lan:8081",
    )
    .await;
    assert_eq!(allowed.status(), 204);
    assert!(!cookie_attributes(&allowed).contains("Secure"));
}

/// A WebSocket handshake carries a cookie and no body, so a forged page could open the live feed with nothing
/// but the browser's own credentials; the origin check covers the upgrade exactly as it covers a write.
#[tokio::test]
async fn an_origin_set_under_plain_http_refuses_a_websocket_upgrade() {
    use common::{json, login_from};
    let (app, _up) = common::admin_app_plain_with_origin("http://phoebis.lan:8081").await;
    let cookie = login_from(&app, "admin-secret", "http://phoebis.lan:8081").await;
    let forged = handshake(&app, &cookie, "http://elsewhere").await;
    assert_eq!(forged.status(), 403);
    assert_eq!(json(forged).await["error"], "origin is not allowed");

    // Past the gate the route itself answers: an in-process call carries no connection to hand over, so the
    // upgrade extractor refuses it, and that refusal is the proof the origin check admitted the request.
    let allowed = handshake(&app, &cookie, "http://phoebis.lan:8081").await;
    assert_eq!(allowed.status(), 400);
    assert_eq!(
        json(allowed).await["error"],
        "this route upgrades to a WebSocket"
    );
}

/// The head a browser sends to open `/admin/live`, with the `Origin` under test.
async fn handshake(
    app: &axum::Router,
    cookie: &str,
    origin: &str,
) -> axum::http::Response<axum::body::Body> {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/admin/live")
        .header("cookie", format!("phoebis_admin={cookie}"))
        .header("origin", origin)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(axum::body::Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
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
async fn without_a_configured_origin_a_foreign_origin_is_refused_and_a_missing_one_is_admitted() {
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

#[tokio::test]
async fn a_confirmed_pairing_stores_the_plan_and_the_start_at_once() {
    use common::{get, json, login, post};
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let mut profile: serde_json::Value = serde_json::from_slice(
        phoebis_profile::fixtures::control_response(phoebis_profile::fixtures::ControlFixture::Profile),
    )
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
    // Computed rather than a literal, so the assertion holds whichever anchor day `derived_end` settles on.
    let derived = phoebis_routing::term::derived_end(
        &phoebis_store::SubscriptionTerm {
            account_id: "business".into(),
            since: Some(1_773_482_400_000),
            trial_ends_at: Some(1_790_899_200_000),
            cancelled: false,
            ends_at: None,
        },
        common::NOW,
    );
    assert_eq!(
        acct["subscription"],
        serde_json::json!({
            "plan": "max",
            "tier": "default_claude_max_5x",
            "since": 1_773_482_400_000_i64,
            "trial_ends_at": 1_790_899_200_000_i64,
            "ends_at": null,
            "derived_ends_at": derived,
        }),
        "the confirm stored the plan and the start with no poll having run"
    );
}

#[tokio::test]
async fn usage_is_cached_and_exposed_without_secrets() {
    use common::{get, json, login, post};
    let (app, up) = common::admin_app_paired().await;
    let cookie = login(&app, "admin-secret").await;
    let first = json(get(&app, "/admin/accounts/business/usage", Some(&cookie)).await).await;
    assert_eq!(first["windows"][0]["key"], "five_hour");
    let _second = get(&app, "/admin/accounts/business/usage", Some(&cookie)).await;
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count(),
        1
    );
    let refreshed = post(
        &app,
        "/admin/accounts/business/usage/refresh",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(refreshed.status(), 200);
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count(),
        1,
        "a manual refresh inside the cache TTL is served from the cache"
    );
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

#[tokio::test]
async fn verifying_an_api_key_account_reports_the_upstream_status() {
    use common::{json, login, post, API_KEY};
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
    assert_eq!(
        post(&app, "/admin/accounts/business/verify", "", Some(&cookie))
            .await
            .status(),
        400,
        "an OAuth account has no key to verify"
    );
}

#[tokio::test]
async fn mutations_require_the_configured_origin() {
    use common::{cookie_of, get, post, post_from};
    const ORIGIN: &str = "https://phoebis.example";
    let (app, _up) = common::admin_app_with_origin(ORIGIN).await;
    assert_eq!(
        post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None)
            .await
            .status(),
        403,
        "a forced login is the one mutation an unauthenticated caller can reach"
    );
    let ok = post_from(
        &app,
        "/admin/session",
        r#"{"secret":"admin-secret"}"#,
        None,
        ORIGIN,
    )
    .await;
    assert_eq!(ok.status(), 204);
    let cookie = cookie_of(&ok);
    let bare = post(&app, "/admin/accounts/business/pause", "", Some(&cookie)).await;
    assert_eq!(bare.status(), 403);
    assert_eq!(
        bare.headers().get("cache-control").unwrap(),
        "no-store",
        "a refusal is no more cacheable than an answer"
    );
    assert_eq!(
        post_from(
            &app,
            "/admin/accounts/business/pause",
            "",
            Some(&cookie),
            "https://evil.example"
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        post_from(
            &app,
            "/admin/accounts/business/pause",
            "",
            Some(&cookie),
            ORIGIN
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
async fn a_confirmed_pairing_supersedes_the_attempt_that_quarantined_the_slot() {
    use common::{get, json, login, post, NOW};
    // No disconnect anywhere in this test: the operator pairs the quarantined slot again, which is the one
    // way out that used to leave the uncertain attempt blocking every later refresh.
    let (app, _up, store, manager) =
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
    let token = manager
        .acquire("business")
        .await
        .expect("the confirmed grant cleared the attempt that was blocking refreshes");
    assert_eq!(token.secret.expose(), "sk-ant-oat01-PLACEHOLDER");
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
    assert_eq!(
        post(&app, "/admin/oauth/sessions", start, Some(&cookie))
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
    assert!(vault.take_exchange(&id).is_none(), "the bundle is gone");
}

#[tokio::test]
async fn the_session_route_answers_the_role_and_settings_say_which_secrets_are_configured() {
    use common::{cookie_of, delete, get, json, post};
    let (app, _up) = common::admin_app().await;
    let anonymous = get(&app, "/admin/session", None).await;
    assert_eq!(anonymous.status(), 401);
    assert_eq!(anonymous.headers()["cache-control"], "no-store");
    let admin = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    let cookie = cookie_of(&admin);
    let me = json(get(&app, "/admin/session", Some(&cookie)).await).await;
    assert_eq!(me["role"], "admin");
    assert!(
        me["session_id"].as_str().is_some_and(|id| id != cookie),
        "the cookie value stays in the store"
    );
    let viewer = post(
        &app,
        "/admin/session",
        r#"{"secret":"viewer-secret"}"#,
        None,
    )
    .await;
    let vcookie = cookie_of(&viewer);
    assert_eq!(
        json(get(&app, "/admin/session", Some(&vcookie)).await).await["role"],
        "viewer"
    );
    let settings = json(get(&app, "/admin/settings", Some(&vcookie)).await).await;
    assert_eq!(
        (
            settings["access"]["admin_configured"].as_bool(),
            settings["access"]["viewer_configured"].as_bool()
        ),
        (Some(true), Some(true))
    );
    assert_eq!(
        settings["compatibility"]["cache_diagnostics"].as_bool(),
        Some(false)
    );
    assert!(settings["compatibility"]["profile"].as_str().is_some());
    assert!(
        !settings.to_string().contains("secret"),
        "no secret and no variable name in the settings answer"
    );
    assert_eq!(
        delete(&app, "/admin/session", Some(&vcookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        get(&app, "/admin/session", Some(&vcookie)).await.status(),
        401
    );
}

#[tokio::test]
async fn a_session_row_carries_the_burn_in_the_unit_the_accounts_funding_selects() {
    use common::{generation, get, json, login, post, wait_json};
    // The router task loads its pins at spawn, so the pool, the key, and the pins are written through one
    // harness and the store handed to the one under test.
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
    let key_generation = generation(&seed.store, "desktop");
    let now = common::NOW;
    let pin = |session: &str, account: &str| phoebis_store::PinRow {
        key_id: "desktop".into(),
        session_id: session.into(),
        model: "claude-sonnet-5".into(),
        account_id: account.into(),
        epoch: 1,
        created_at: now - 10,
        last_used_at: now - 5,
        key_generation,
        incarnation: 1,
        burn: 42.0,
        burn_at: Some(now - 5),
        burn_usd: 0.5,
        last_input_tokens: 1_000,
        last_refresh_at: now - 5,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
    };
    seed.store.upsert_pin(&pin("s1", "business")).unwrap();
    seed.store.upsert_pin(&pin("s2", "direct")).unwrap();
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = |sessions: &serde_json::Value, session: &str| {
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
            row(&sessions, "s1")["burn"].as_f64(),
            row(&sessions, "s1")["burn_unit"].as_str()
        ),
        (Some(42.0), Some("meter_tokens")),
        "a subscription account funded by its plan reads the meter average"
    );
    assert_eq!(
        (
            row(&sessions, "s2")["burn"].as_f64(),
            row(&sessions, "s2")["burn_unit"].as_str()
        ),
        (Some(0.5), Some("usd")),
        "an api_key pin always reads the dollar average"
    );
    assert_eq!(
        row(&sessions, "s1")["burn_at"].as_i64(),
        Some(now - 5),
        "the fold's own timestamp, not the cache clock"
    );
    // Pushed into overage with a reset ahead of now: the same pin reads the dollar average instead.
    parts.router.report(phoebis_routing::Report::Overage {
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
    });
    let overage_sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["burn_unit"] == "usd")
    })
    .await;
    assert_eq!(
        (
            row(&overage_sessions, "s1")["burn"].as_f64(),
            row(&overage_sessions, "s1")["burn_unit"].as_str()
        ),
        (Some(0.5), Some("usd")),
        "overage evidence whose reset is ahead moves a subscription account's reading to dollars"
    );
    // A reset that has already passed no longer counts as overage: a raw `in_use` read would still answer
    // `usd` here, since the flag itself never cleared. A later `observed_at` than the ahead-of-now report
    // above, or the router's own newest-wins rule drops this one on the floor.
    parts.router.report(phoebis_routing::Report::Overage {
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
    });
    let recovered_sessions = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1" && r["burn_unit"] == "meter_tokens")
    })
    .await;
    assert_eq!(
        (
            row(&recovered_sessions, "s1")["burn"].as_f64(),
            row(&recovered_sessions, "s1")["burn_unit"].as_str()
        ),
        (Some(42.0), Some("meter_tokens")),
        "a passed reset reads as not in overage, so the reading returns to the meter"
    );
}
