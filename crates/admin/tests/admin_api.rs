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
