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
    // Computed rather than a literal, so the assertion holds whichever anchor day `derived_end` settles on.
    let derived = phoebis_routing::term::derived_end(
        &phoebis_store::SubscriptionTerm {
            account_id: "business".into(),
            since: Some(1_773_482_400_000),
            trial_ends_at: Some(1_790_899_200_000),
            cancelled: false,
            ends_at: None,
            tier: None,
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
async fn the_usage_get_answers_the_polls_latest_snapshot_and_standing_without_a_fetch() {
    use common::{get, json, login, post, NOW};
    use std::sync::atomic::Ordering;
    // A zero TTL, so every refresh below past the backoff reaches the endpoint and the GET's own reads show.
    let h = common::admin_parts_polled(std::time::Duration::ZERO).await;
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
            "last_error": "the usage endpoint rate-limited the account; next attempt at 1970-01-01 00:18:50 UTC",
            "last_outcome": "rate_limited",
            "next_attempt_at": NOW + 10_000 + 120_000,
            "rate_limited_in_a_row": 1,
        }),
        "the first 429 holds the account for the ladder's first step, two minutes"
    );
    let held = json(get(&h.app, path, Some(&cookie)).await).await;
    assert!(
        held["error"].is_null(),
        "an account with windows never reads as an error: {held}"
    );
    assert_eq!(held["windows"], first["windows"]);
    assert_eq!(held["fetched_at"], NOW, "the windows are the clean read's");
    assert_eq!(held["poll"], refused["poll"]);

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
async fn the_usage_get_says_no_credential_is_stored_for_a_disconnected_or_unpaired_account() {
    use common::{delete, get, json, login};
    let h = common::admin_parts_polled(std::time::Duration::from_secs(60)).await;
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
        Some("no credential is stored for this account; pair it to load quota".into()),
        true,
        Some(0),
    );
    let disconnected = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(answer(&disconnected), no_credential, "{disconnected}");
    h.store
        .set_state("business", phoebis_store::AccountState::Unpaired)
        .unwrap();
    let unpaired = json(get(&h.app, path, Some(&cookie)).await).await;
    assert_eq!(answer(&unpaired), no_credential, "{unpaired}");
}

#[tokio::test]
async fn the_accounts_list_carries_each_oauth_rows_poll_standing_and_null_for_an_anthropic_key_account(
) {
    use common::{get, json, login, post, NOW};
    let h = common::admin_parts_polled(std::time::Duration::from_secs(60)).await;
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
        "error": "no credential is stored for this account; pair it to load quota",
        "fetched_at": null,
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
        serde_json::json!({ "error": usage["error"], "fetched_at": usage["fetched_at"], "poll": usage["poll"] }),
    );
    assert_eq!(row("spare"), no_credential);
    assert!(
        row("direct").is_null(),
        "the poll never reads an Anthropic-key account"
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
async fn an_account_whose_snapshot_holds_no_windows_answers_the_windows_the_router_holds() {
    use common::{get, json, login, wait_json, NOW};
    use phoebis_routing::{Report, UtilizationSample, WindowReading, FIVE_HOUR, SEVEN_DAY};
    let h = common::admin_parts_polled(std::time::Duration::from_secs(60)).await;
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
                    },
                )
                .with(
                    SEVEN_DAY,
                    WindowReading {
                        utilization: 0.5,
                        reset: Some(NOW + 86_400_000),
                        observed_at: NOW - 60_000,
                    },
                ),
            incarnation: 1,
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
/// run and read more than a day ago, are the windows the account view lists, as the usage `GET` answers them.
#[tokio::test]
async fn an_account_the_ledger_holds_no_sample_of_today_lists_the_windows_the_router_holds() {
    use common::{get, json, login, wait_json, NOW};
    use phoebis_routing::{
        Report, UtilizationSample, WindowReading, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
    };
    const HOUR: i64 = 3_600_000;
    let h = common::admin_parts_polled(std::time::Duration::from_secs(60)).await;
    let cookie = login(&h.app, "admin-secret").await;
    let read_at = NOW - 30 * HOUR;
    let reading = |utilization, reset| WindowReading {
        utilization,
        reset,
        observed_at: read_at,
    };
    h.router
        .report(Report::Sample {
            account_id: "business".into(),
            sample: UtilizationSample::default()
                .with(FIVE_HOUR, reading(0.9, Some(NOW - 25 * HOUR)))
                .with(SEVEN_DAY, reading(0.5, Some(NOW + 72 * HOUR)))
                .with(SEVEN_DAY_FABLE, reading(0.2, Some(NOW + 72 * HOUR))),
            incarnation: 1,
        })
        .await;
    wait_json(&h.app, "/admin/accounts/business/usage", &cookie, |v| {
        v["error"].is_null()
    })
    .await;
    // The 5-hour window's reset has passed, so the router predicts it empty until the next reading names one.
    let held = serde_json::json!([
        { "id": "five_hour", "observed_at": read_at, "resets_at": null, "utilization": 0.0 },
        { "id": "seven_day", "observed_at": read_at, "resets_at": NOW + 72 * HOUR, "utilization": 0.5 },
        { "id": "fable", "observed_at": read_at, "resets_at": NOW + 72 * HOUR, "utilization": 0.2 },
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
        bare.headers().get("x-content-type-options").unwrap(),
        "nosniff",
        "and no more sniffable"
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
    let after = common::NOW + 11 * 60_000;
    parts.clock_cell.store(after, Ordering::SeqCst);
    assert_eq!(
        parts.vault.sweep(after),
        1,
        "the confirmed record leaves the vault"
    );
    assert_eq!(
        parts.pairing.sweep(after),
        1,
        "and the ended session leaves the pairings"
    );
    assert_eq!(get(&parts.app, &path, Some(&cookie)).await.status(), 404);
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
        burn_at: Some(now - 5),
        burn_banded: input(banded),
        last_input_tokens: 1_000,
        last_refresh_at: now - 5,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
        lane: phoebis_routing::lane_key("claude-opus-5", "standard", "none", "-"),
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
    common::admin_parts_full_on(seed.store.clone()).await
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
    // plain one. The subscription pin burns Opus 5.5, which the cost table measures; the api_key pin Opus 5.
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
    // Opus 5.5: 0.01461 of the Max 20x 5-hour window per million input tokens; Opus 5: $5 per million.
    let share_per_hour = 145.0 * 0.01461 / 1_000_000.0 * 3_600.0;
    let usd_per_hour = 100.0 * 5.0 / 1_000_000.0 * 3_600.0;
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
    // The price table lists no Opus 5.5 on this base, so its dollar reading is the table's dearest-rate fallback.
    let s1_usd_per_hour = phoebis_ledger::PriceTable::shipped()
        .rate_table()
        .price_burn(
            "claude-opus-5-5",
            &phoebis_store::PinBurn {
                input: 100.0,
                ..phoebis_store::PinBurn::default()
            },
        )
        .unwrap()
        * 3_600.0;
    assert_eq!(
        row(&sessions, "s1")["burn_at"].as_i64(),
        Some(now - 5),
        "the fold's own timestamp, not the cache clock"
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
async fn a_pinned_sessions_burn_reads_its_share_rate_on_the_account_it_sits_on() {
    use common::{login, wait_json};
    let now = common::NOW;
    let parts = sessions_harness_with(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-opus-5-5".into(),
            ..burning_opus_pin("s1", "business", generation, now, 0.0, 0.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A million Opus 5.5 input tokens at the fixed clock, which the peak band never covers, folded over the
    // pool's default 600-second burn window.
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
            },
            now,
            usage: Some(phoebis_routing::PinUsage {
                input: 1_000_000,
                ..phoebis_routing::PinUsage::default()
            }),
            origin: "client",
            cache_ttl_ms: 300_000,
            fast: false,
        })
        .await;
    let window = 600.0;
    let on_max_20x = 0.01461 * 3_600.0 / window;
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
    name_the_tier(&parts.store, "business", "default_claude_pro");
    let on_pro = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        !close(session_row(v, "s1")["burn"].as_f64(), on_max_20x)
    })
    .await;
    assert!(
        close(burn_of(&on_pro, "s1").0, on_max_20x / 0.05),
        "the same tokens read on Pro are that share over Pro's 5-hour factor: {:?}",
        burn_of(&on_pro, "s1")
    );
    name_the_tier(&parts.store, "business", "default_claude_team");
    let on_team = wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        session_row(v, "s1")["burn"].is_null()
    })
    .await;
    assert_eq!(
        burn_of(&on_team, "s1"),
        (None, Some("window_share".into())),
        "a tier the cost table lacks sizes no window, so the figure is null rather than a guess"
    );
}

#[tokio::test]
async fn a_pin_on_a_model_with_an_unmeasured_cell_reads_no_burn() {
    // The pin burns Sonnet 5, whose every cell is null: the share is unpriced, and the same pin in overage prices
    // in dollars, since the price table lists the model.
    use common::{get, json, login, wait_json};
    let now = common::NOW;
    let parts = sessions_harness_with(|generation| {
        vec![phoebis_store::PinRow {
            model: "claude-sonnet-5".into(),
            ..burning_opus_pin("s1", "business", generation, now, 100.0, 100.0)
        }]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    assert_eq!(
        burn_of(&sessions, "s1"),
        (None, Some("window_share".into())),
        "a null cell under the burn prices no share"
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
            last_input_tokens: 100,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
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
            last_input_tokens: 100,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
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
            last_input_tokens: 100,
            last_refresh_at: now - 1_000,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
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
