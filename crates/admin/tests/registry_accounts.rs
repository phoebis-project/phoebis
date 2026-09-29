mod common;
use common::{backing_off_parts, delete, get, json, login, patch, post, succeed_token_requests};
use fake_upstream::Respond;

#[tokio::test]
async fn creating_an_api_key_account_verifies_it_and_a_rejected_key_is_refused() {
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/accounts",
        r#"{"id":"console","kind":"api_key","label":"Console","secret":"sk-ant-api03-NEW"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert_eq!(body["state"].as_str(), Some("ready"));
    assert!(body.get("secret").is_none() && !body.to_string().contains("sk-ant-api03-NEW"));
    let probe = up
        .requests()
        .into_iter()
        .find(|r| r.line.starts_with("GET /v1/models?limit=1000"))
        .expect("a discovery request");
    assert!(probe
        .headers
        .iter()
        .any(|(n, v)| n == "x-api-key" && v == "sk-ant-api03-NEW"));
    assert_eq!(
        post(
            &app,
            "/admin/accounts",
            r#"{"id":"console","kind":"api_key","label":"Again","secret":"sk-ant-api03-NEW"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        409
    );
    up.add_credential_rule(
        "sk-ant-api03-BAD",
        "/v1/models",
        Respond::Json {
            status: 401,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let rejected = post(
        &app,
        "/admin/accounts",
        r#"{"id":"bad","kind":"api_key","label":"Bad","secret":"sk-ant-api03-BAD"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected.status(), 400);
    assert_eq!(
        get(&app, "/admin/accounts/bad", Some(&cookie))
            .await
            .status(),
        404,
        "a rejected creation leaves no row"
    );
    assert_eq!(
        post(
            &app,
            "/admin/accounts",
            r#"{"id":"Bad Id","kind":"api_key","label":"x","secret":"s"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        post(
            &app,
            "/admin/accounts",
            r#"{"id":"o2","kind":"oauth","label":"x","secret":"s"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "an oauth account takes no secret"
    );
    let oauth = post(
        &app,
        "/admin/accounts",
        r#"{"id":"o2","kind":"oauth","label":"Second"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(json(oauth).await["state"], "unpaired");
}

#[tokio::test]
async fn patching_deleting_and_disconnecting_accounts() {
    let (app, _up, _store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    assert_eq!(
        post(&app, "/admin/accounts/direct/pause", "", Some(&cookie))
            .await
            .status(),
        204
    );
    assert!(
        !router.snapshot().account("direct").unwrap().enabled,
        "a pause is published to the router, not only written to the store"
    );
    assert_eq!(
        post(&app, "/admin/accounts/direct/resume", "", Some(&cookie))
            .await
            .status(),
        204
    );
    assert!(
        router.snapshot().account("direct").unwrap().enabled,
        "and so is a resume"
    );
    let patched = patch(
        &app,
        "/admin/accounts/direct",
        r#"{"label":"Direct 2","enabled":false}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(patched.status(), 200);
    let body = json(patched).await;
    assert_eq!(
        (body["label"].as_str(), body["enabled"].as_bool()),
        (Some("Direct 2"), Some(false))
    );
    assert_eq!(
        patch(
            &app,
            "/admin/accounts/business",
            r#"{"secret":"sk-ant-api03-NOPE"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "an oauth account refuses a secret with 400"
    );
    assert_eq!(
        delete(&app, "/admin/accounts/direct/credentials", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        json(get(&app, "/admin/accounts/direct", Some(&cookie)).await).await["state"],
        "unpaired",
        "an api_key account without its secret is unpaired"
    );
    let rekeyed = patch(
        &app,
        "/admin/accounts/direct",
        r#"{"secret":"sk-ant-api03-AGAIN"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        json(rekeyed).await["state"],
        "ready",
        "a new secret that verifies makes it ready again"
    );
    assert_eq!(
        delete(&app, "/admin/accounts/direct", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        get(&app, "/admin/accounts/direct", Some(&cookie))
            .await
            .status(),
        404
    );
    let viewer = login(&app, "viewer-secret").await;
    assert_eq!(
        patch(
            &app,
            "/admin/accounts/business",
            r#"{"label":"x"}"#,
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        delete(&app, "/admin/accounts/business", Some(&viewer))
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn an_account_write_ignores_a_member_the_route_does_not_name() {
    let (app, _up, _store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/accounts",
        r#"{"id":"extra","kind":"api_key","label":"Extra","secret":"sk-ant-api03-EXTRA","region":"eu"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert!(
        body.get("region").is_none(),
        "a member the route does not name is dropped, not echoed"
    );
    let patched = patch(
        &app,
        "/admin/accounts/extra",
        r#"{"label":"Extra 2","region":"eu"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(patched.status(), 200);
    let body = json(patched).await;
    assert_eq!(body["label"].as_str(), Some("Extra 2"));
    assert!(body.get("region").is_none());
}

#[tokio::test]
async fn a_rejected_replacement_secret_leaves_the_original_credential_and_the_snapshot_unchanged() {
    let (app, up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    up.add_credential_rule(
        "sk-ant-api03-BAD",
        "/v1/models",
        Respond::Json {
            status: 401,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let before = router.snapshot().revision;
    let refused = patch(
        &app,
        "/admin/accounts/direct",
        r#"{"enabled":false,"label":"Half","secret":"sk-ant-api03-BAD"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    let row = store.account("direct").unwrap().unwrap();
    assert_eq!(
        (row.label.as_str(), row.enabled, row.state),
        ("Direct", true, phoebis_store::AccountState::Ready),
        "no field was half-applied"
    );
    assert_eq!(
        store
            .api_key_credential("direct")
            .unwrap()
            .unwrap()
            .0
            .expose(),
        common::API_KEY,
        "the original key is still the live one"
    );
    assert_eq!(router.snapshot().revision, before, "nothing was published");
    let probe = up
        .requests()
        .into_iter()
        .rfind(|r| r.line.starts_with("GET /v1/models"))
        .unwrap();
    assert!(
        probe
            .headers
            .iter()
            .any(|(n, v)| n == "x-api-key" && v == "sk-ant-api03-BAD"),
        "discovery ran with the staged secret"
    );
}

#[tokio::test]
async fn a_paired_account_routes_without_a_catalog_fetch() {
    let (app, up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for(&app, &cookie, "business").await;
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
    assert!(
        up.requests()
            .iter()
            .all(|r| !r.line.starts_with("GET /v1/models")),
        "the pairing confirm probes nothing"
    );
    // No served-model gate stands between the catalog and routing any more: every enabled member of a
    // pool is a candidate for a model the rules allow, whatever the live catalog answers.
    assert_eq!(
        store.account("business").unwrap().unwrap().state,
        phoebis_store::AccountState::Ready
    );
    assert!(
        router.snapshot().account("business").is_some(),
        "a request routes"
    );
}

#[tokio::test]
async fn a_write_whose_reload_fails_answers_503_and_publishes_on_the_next_reload() {
    let (app, _up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    store.debug_fail_next_load();
    let refused = patch(
        &app,
        "/admin/accounts/direct",
        r#"{"label":"Written"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 503);
    let body = json(refused).await;
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("could not be reloaded"));
    assert_eq!(
        store.account("direct").unwrap().unwrap().label,
        "Written",
        "the row was written"
    );
    assert_eq!(
        router.snapshot().account("direct").unwrap().label,
        "Direct",
        "the snapshot still shows the old label"
    );
    router.reload().await.unwrap();
    assert_eq!(
        router.snapshot().account("direct").unwrap().label,
        "Written"
    );
}

#[tokio::test]
async fn a_disconnect_between_the_exchange_and_the_confirm_answers_a_conflict() {
    let (app, _up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let id = common::exchanged_session_for(&app, &cookie, "business").await;
    store.bump_lifecycle("business").unwrap();
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
        "the account was disconnected while pairing"
    );
}

#[tokio::test]
async fn the_catalog_route_answers_the_live_list_caches_it_and_refreshes_on_demand() {
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    up.set_rule_after(
        0,
        "/v1/models",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::json!({
                "data": [
                    { "id": "claude-opus-5", "display_name": "Claude Opus 5", "created_at": "2025-11-01", "max_input_tokens": 1_000_000, "max_tokens": 64_000 },
                    { "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5", "created_at": "2025-09-29", "max_input_tokens": 1_000_000, "max_tokens": 128_000 },
                ],
            })
            .to_string()
            .into_bytes(),
        },
    );
    let probes = || {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /v1/models"))
            .count()
    };
    let listed = json(get(&app, "/admin/accounts/direct/catalog", Some(&cookie)).await).await;
    let models = listed["models"].as_array().unwrap();
    assert_eq!(
        models
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["claude-opus-5", "claude-sonnet-4-5"],
        "sorted by id"
    );
    assert_eq!(models[0]["display_name"], "Claude Opus 5");
    assert_eq!(models[0]["created_at"], "2025-11-01");
    assert_eq!(models[0]["max_input_tokens"], 1_000_000);
    assert_eq!(models[0]["max_tokens"], 64_000);
    assert!(listed["error"].is_null());

    let before = probes();
    let second = json(get(&app, "/admin/accounts/direct/catalog", Some(&cookie)).await).await;
    assert_eq!(
        second["models"].as_array().unwrap().len(),
        2,
        "the cache answers"
    );
    assert_eq!(probes(), before, "a second GET adds no probe");

    let refreshed = json(
        get(
            &app,
            "/admin/accounts/direct/catalog?refresh=1",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(refreshed["models"].as_array().unwrap().len(), 2);
    assert_eq!(probes(), before + 1, "?refresh=1 adds one");

    let viewer = login(&app, "viewer-secret").await;
    assert_eq!(
        get(&app, "/admin/accounts/direct/catalog", Some(&viewer))
            .await
            .status(),
        200,
        "a viewer may GET"
    );

    up.set_rule_after(
        0,
        "/v1/models",
        Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let failed = json(
        get(
            &app,
            "/admin/accounts/direct/catalog?refresh=1",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(failed["models"].as_array().unwrap().len(), 0);
    assert_eq!(failed["error"], "the models endpoint answered 503");
}

#[tokio::test]
async fn cooldowns_and_funding_are_cleared_by_an_operator_and_shown_on_the_account() {
    let (app, _up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    router.report(phoebis_routing::Report::Cooldown {
        account_id: "direct".into(),
        row: phoebis_store::CooldownRow {
            account_id: "direct".into(),
            scope: "model:claude-opus-5".into(),
            until: common::NOW + 60_000,
            set_at: 0,
            reason: "rate_limit".into(),
            window: "retry_after".into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    // The task records the cooldown in its own state before it queues the write, so a row on disk means the
    // snapshot the admin route reads already has it.
    fake_upstream::wait_for(std::time::Duration::from_secs(2), || {
        store
            .load_routing()
            .unwrap()
            .cooldowns
            .into_iter()
            .find(|c| c.scope == "model:claude-opus-5")
    })
    .await;
    let shown = json(get(&app, "/admin/accounts/direct", Some(&cookie)).await).await;
    assert_eq!(shown["cooldowns"][0]["scope"], "model:claude-opus-5");
    assert_eq!(
        delete(
            &app,
            "/admin/accounts/direct/cooldowns/model:claude-opus-5",
            Some(&cookie)
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        delete(
            &app,
            "/admin/accounts/direct/cooldowns/model:claude-opus-5",
            Some(&cookie)
        )
        .await
        .status(),
        404
    );
    assert_eq!(
        post(
            &app,
            "/admin/accounts/direct/clear-funding",
            "",
            Some(&cookie)
        )
        .await
        .status(),
        409,
        "not in funding_required"
    );
    store
        .set_state("direct", phoebis_store::AccountState::FundingRequired)
        .unwrap();
    assert_eq!(
        post(
            &app,
            "/admin/accounts/direct/clear-funding",
            "",
            Some(&cookie)
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        json(get(&app, "/admin/accounts/direct", Some(&cookie)).await).await["state"],
        "ready"
    );
}

#[tokio::test]
async fn a_second_slot_cannot_be_paired_to_an_upstream_account_the_first_holds() {
    // The same login the pairing test drives, against a slot whose upstream account is already bound to `business`.
    let (app, _up) = common::admin_app_paired().await;
    let cookie = login(&app, "admin-secret").await;
    assert_eq!(
        post(
            &app,
            "/admin/accounts",
            r#"{"id":"second","kind":"oauth","label":"Second"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        201
    );
    let id = common::exchanged_session_for(&app, &cookie, "second").await;
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{id}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 409);
    assert!(json(confirmed).await["error"]
        .as_str()
        .unwrap()
        .contains("already paired"));
}

#[tokio::test]
async fn the_account_view_carries_its_profile_windows_pools_totals_and_models() {
    use phoebis_ledger::{
        AttemptRecord, ExtraUsageSampleRecord, QuotaSampleRecord, Record, RequestRecord,
    };
    use phoebis_store::SubscriptionRow;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    parts.store.set_profile(
        "business",
        r#"{"account":{"uuid":"u","email_address":"business@example.invalid","display_name":"Biz","has_extra_usage_enabled":true,"has_claude_max":true},"organization":{"uuid":"o","name":"Biz Org","organization_type":"claude_max","billing_type":"stripe_subscription","rate_limit_tier":"default_claude_max_5x","subscription_created_at":"2026-03-14T10:00:00Z","claude_code_trial_ends_at":"2026-10-02T00:00:00+00:00"}}"#,
        &SubscriptionRow {
            plan: Some("max".into()),
            tier: Some("default_claude_max_5x".into()),
            since: Some(1_773_482_400_000),
            trial_ends_at: Some(1_790_899_200_000),
        },
    ).unwrap();
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":2}]}"#, Some(&cookie)).await;
    let bucket = phoebis_ledger::bucket_of(now - 60_000);
    parts
        .db
        .insert_samples(
            &[
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "5h".into(),
                    utilization: Some(0.62),
                    reset_at: Some(now + 3_600_000),
                    source: "poll".into(),
                    observed_at: None,
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "fable".into(),
                    utilization: Some(0.1),
                    reset_at: None,
                    source: "poll".into(),
                    observed_at: None,
                },
            ],
            Some(&ExtraUsageSampleRecord {
                account_id: "business".into(),
                bucket,
                is_enabled: true,
                in_use: Some(false),
                monthly_limit: Some(5_000),
                used_credits: Some(1_234),
                utilization: Some(0.25),
                currency: Some("USD".into()),
                disabled_reason: None,
                overage_reset: None,
                source: "poll".into(),
            }),
        )
        .unwrap();
    for (id, at) in [
        ("r1", now - 3_600_000),
        ("r2", now - 3 * 86_400_000),
        ("r3", now - 30 * 86_400_000),
    ] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 100),
                key_id: Some("k".into()),
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
                account_id: "business".into(),
                account_kind: "oauth".into(),
                started_at: at,
                outcome: "succeeded".into(),
                input_tokens: Some(10),
                output_tokens: Some(5),
                estimated_usd: Some(0.01),
                ..Default::default()
            }],
            bodies: None,
        });
    }
    let accounts = common::wait_json(&parts.app, "/admin/accounts", &cookie, |v| {
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "business" && a["requests"]["lifetime"].as_u64() == Some(3))
    })
    .await;
    let listed = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap()
        .clone();
    assert_eq!(
        (
            listed["profile"]["rate_limit_tier"].as_str(),
            listed["profile"]["billing_type"].as_str(),
            listed["profile"]["has_extra_usage_enabled"].as_bool()
        ),
        (
            Some("default_claude_max_5x"),
            Some("stripe_subscription"),
            Some(true)
        )
    );
    assert_eq!(
        listed["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["5h", "fable"],
        "sorted by id"
    );
    assert_eq!(listed["windows"][0]["utilization"].as_f64(), Some(0.62));
    assert_eq!(
        (
            listed["extra_usage"]["monthly_limit"].as_u64(),
            listed["extra_usage"]["in_use"].as_bool()
        ),
        (Some(5_000), Some(false))
    );
    assert_eq!(
        (
            listed["pools"][0]["pool_id"].as_str(),
            listed["pools"][0]["priority"].as_u64()
        ),
        (Some("main"), Some(2))
    );
    assert_eq!(
        (
            listed["requests"]["last_24h"].as_u64(),
            listed["requests"]["last_7d"].as_u64()
        ),
        (Some(1), Some(2))
    );
    assert_eq!(listed["profile"]["display_name"], "Biz");
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
        now,
    );
    assert_eq!(
        listed["subscription"],
        serde_json::json!({
            "plan": "max",
            "tier": "default_claude_max_5x",
            "since": 1_773_482_400_000_i64,
            "trial_ends_at": 1_790_899_200_000_i64,
            "ends_at": null,
            "derived_ends_at": derived,
        }),
        "never cancelled, so the stored end is null"
    );
    assert_eq!(listed["cancelled"], false);
    let direct = json(get(&parts.app, "/admin/accounts/direct", Some(&cookie)).await).await;
    assert!(
        direct["profile"].is_null()
            && direct["extra_usage"].is_null()
            && direct["windows"].as_array().unwrap().is_empty()
            && direct["subscription"].is_null()
    );
    // An OAuth account whose profile carries none of the five fields answers every subscription field null,
    // the same as a row never written.
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts",
            r#"{"id":"second","kind":"oauth","label":"Second"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        201
    );
    parts
        .store
        .set_profile(
            "second",
            r#"{"account":{"uuid":"s","email_address":"second@example.invalid"}}"#,
            &SubscriptionRow::default(),
        )
        .unwrap();
    let second = json(get(&parts.app, "/admin/accounts/second", Some(&cookie)).await).await;
    assert_eq!(
        second["subscription"],
        serde_json::json!({"plan": null, "tier": null, "since": null, "trial_ends_at": null, "ends_at": null, "derived_ends_at": null})
    );
    assert_eq!(second["cancelled"], false);
    // An OAuth account that has never paired (no profile document at all) answers `subscription: null`
    // outright, distinct from a paired account whose profile carries none of the five fields.
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts",
            r#"{"id":"third","kind":"oauth","label":"Third"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        201
    );
    let third = json(get(&parts.app, "/admin/accounts/third", Some(&cookie)).await).await;
    assert!(third["subscription"].is_null());
    assert!(
        !json(get(&parts.app, "/admin/accounts", Some(&cookie)).await)
            .await
            .to_string()
            .contains("PLACEHOLDER"),
        "no credential in the list"
    );
}

#[tokio::test]
async fn a_cancelled_account_stores_the_end_it_was_saved_with_and_answers_the_derived_one_to_prefill(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .store
        .set_profile(
            "business",
            r#"{"account":{"uuid":"u","email_address":"business@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_5x".into()),
                since: Some(1_000_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
    let end = 1_790_000_000_000_i64;
    let cancelled = json(
        patch(
            &parts.app,
            "/admin/accounts/business",
            &format!(r#"{{"cancelled":true,"subscription_ends_at":{end}}}"#),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            cancelled["cancelled"].as_bool(),
            cancelled["subscription"]["ends_at"].as_i64(),
        ),
        (Some(true), Some(end)),
        "the route answers exactly the end it was saved with"
    );
    // The saved end is not the anchor the console prefills the form with next time; the derivation is its own
    // question, read from `since` and the trial alone.
    let derived = phoebis_routing::term::derived_end(
        &phoebis_store::SubscriptionTerm {
            account_id: "business".into(),
            since: Some(1_000_000),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(end),
            tier: None,
        },
        common::NOW,
    );
    assert_eq!(
        cancelled["subscription"]["derived_ends_at"].as_i64(),
        derived,
        "a start is on file, so the prefill derives an end of its own"
    );
    // An end alone is accepted on the resulting row: the account is already cancelled, so the flag needs no
    // repeating.
    let new_end = end + 1_000;
    let end_only = json(
        patch(
            &parts.app,
            "/admin/accounts/business",
            &format!(r#"{{"subscription_ends_at":{new_end}}}"#),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            end_only["cancelled"].as_bool(),
            end_only["subscription"]["ends_at"].as_i64(),
        ),
        (Some(true), Some(new_end)),
        "an end alone is accepted while the resulting row is already cancelled"
    );
    // The flag alone, re-affirming a cancellation already on file, falls back to the stored end rather than
    // needing it repeated.
    let flag_only = json(
        patch(
            &parts.app,
            "/admin/accounts/business",
            r#"{"cancelled":true}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            flag_only["cancelled"].as_bool(),
            flag_only["subscription"]["ends_at"].as_i64(),
        ),
        (Some(true), Some(new_end)),
        "the flag alone keeps the stored end standing"
    );
    // A cancelled OAuth account with no profile at all still answers the flag and the stored end: the flag
    // must never read false for lack of a profile.
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts",
            r#"{"id":"unpaired","kind":"oauth","label":"Unpaired"}"#,
            Some(&cookie),
        )
        .await
        .status(),
        201
    );
    let unpaired_cancelled = json(
        patch(
            &parts.app,
            "/admin/accounts/unpaired",
            &format!(r#"{{"cancelled":true,"subscription_ends_at":{end}}}"#),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            unpaired_cancelled["cancelled"].as_bool(),
            unpaired_cancelled["subscription"]["ends_at"].as_i64(),
            unpaired_cancelled["subscription"]["derived_ends_at"].as_i64(),
        ),
        (Some(true), Some(end), None),
        "no since to derive from, but the flag and the stored end round-trip regardless"
    );
    // Cancelling off drops the stored end, per `set_cancelled`'s own rule.
    let restored = json(
        patch(
            &parts.app,
            "/admin/accounts/business",
            r#"{"cancelled":false}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            restored["cancelled"].as_bool(),
            restored["subscription"]["ends_at"].as_i64(),
        ),
        (Some(false), None),
        "the end leaves with the flag"
    );
}

#[tokio::test]
async fn an_end_without_the_flag_a_flag_without_an_end_and_a_flag_on_an_api_key_account_are_refused(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let bare_end = patch(
        &parts.app,
        "/admin/accounts/business",
        r#"{"subscription_ends_at":1790000000000}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(bare_end.status(), 400);
    assert_eq!(
        json(bare_end).await["error"].as_str(),
        Some("subscription_ends_at needs cancelled")
    );
    let bare_flag = patch(
        &parts.app,
        "/admin/accounts/business",
        r#"{"cancelled":true}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(bare_flag.status(), 400);
    assert_eq!(
        json(bare_flag).await["error"].as_str(),
        Some("cancelled needs subscription_ends_at")
    );
    let on_api_key = patch(
        &parts.app,
        "/admin/accounts/direct",
        r#"{"cancelled":true,"subscription_ends_at":1790000000000}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(on_api_key.status(), 400);
    assert_eq!(
        json(on_api_key).await["error"].as_str(),
        Some("only an oauth account is cancelled")
    );
    let unchanged = json(get(&parts.app, "/admin/accounts/business", Some(&cookie)).await).await;
    assert_eq!(
        unchanged["cancelled"], false,
        "every refusal above changed nothing"
    );
}

#[tokio::test]
async fn a_backoff_that_opened_no_attempt_shows_up_as_the_credentials_last_error() {
    let (app, up, _store, manager, clock_cell) = backing_off_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let accounts = json(get(&app, "/admin/accounts", Some(&cookie)).await).await;
    let business = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap();
    assert_eq!(
        business["credential"]["last_error"], "refresh is waiting out a backoff",
        "a failure that opened no attempt still says why the account is unavailable"
    );

    succeed_token_requests(&up);
    // Past the backoff the rejection armed, so this acquire actually reaches the token endpoint again.
    clock_cell.fetch_add(61_000, std::sync::atomic::Ordering::SeqCst);
    assert!(manager.acquire("business").await.is_ok());
    let accounts = json(get(&app, "/admin/accounts", Some(&cookie)).await).await;
    let business = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap();
    assert_eq!(
        business["credential"]["last_error"],
        serde_json::Value::Null,
        "a successful refresh clears the recorded error"
    );
    up.shutdown().await;
}
