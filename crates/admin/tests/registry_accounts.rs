mod common;
use common::{delete, get, json, login, patch, post, put};
use fake_upstream::Respond;

#[tokio::test]
async fn creating_an_api_key_account_verifies_it_seeds_its_catalog_and_a_rejected_key_is_refused() {
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
    assert_eq!(
        (body["state"].as_str(), body["served_models_count"].as_u64()),
        (Some("ready"), Some(1))
    );
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
async fn a_paired_account_routes_its_discovered_models_without_a_manual_sync() {
    let (app, _up, store, router) = common::admin_parts().await;
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
        store
            .served_models("business")
            .unwrap()
            .iter()
            .any(|m| m.upstream_id == "claude-sonnet-4-5" && m.selected),
        "the confirm ran a sync"
    );
    assert!(
        router
            .snapshot()
            .served("business", "claude-sonnet-4-5")
            .is_some(),
        "and the snapshot routes it"
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
async fn served_models_are_listed_written_and_synced_and_a_clash_is_refused() {
    let (app, up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    up.set_rule_after(0, "/v1/models", Respond::Json { status: 200, headers: vec![], body: br#"{"data":[{"id":"claude-sonnet-4-5"},{"id":"claude-opus-5","max_input_tokens":1000000}]}"#.to_vec() });
    let synced = post(
        &app,
        "/admin/accounts/direct/models/sync",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(synced.status(), 200);
    assert_eq!(json(synced).await["added"].as_array().unwrap().len(), 2);
    let listed = json(get(&app, "/admin/accounts/direct/models", Some(&cookie)).await).await;
    assert_eq!(listed["models"].as_array().unwrap().len(), 2);
    assert_eq!(listed["built_in"], false);
    assert!(listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["upstream_id"] == "claude-opus-5" && m["long_context"] == true));
    let written = put(&app, "/admin/accounts/direct/models", r#"{"models":[{"upstream_id":"claude-sonnet-4-5","selected":false},{"upstream_id":"claude-opus-5","selected":true},{"upstream_id":"Vendor/Model-A","selected":true}]}"#, Some(&cookie)).await;
    assert_eq!(written.status(), 200);
    let listed = json(written).await;
    assert!(listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["upstream_id"] == "Vendor/Model-A" && m["source"] == "manual"));
    assert_eq!(put(&app, "/admin/accounts/direct/models", r#"{"models":[{"upstream_id":"a/b","selected":true},{"upstream_id":"A/B","selected":true}]}"#, Some(&cookie)).await.status(), 409);
    let viewer = login(&app, "viewer-secret").await;
    assert_eq!(
        get(&app, "/admin/accounts/direct/models", Some(&viewer))
            .await
            .status(),
        200
    );
    assert_eq!(
        post(
            &app,
            "/admin/accounts/direct/models/sync",
            "",
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        put(
            &app,
            "/admin/accounts/direct/models",
            r#"{"models":[]}"#,
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
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
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
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
async fn creating_an_account_whose_catalog_carries_two_spellings_records_the_clash() {
    let (app, up, _store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    up.set_rule_after(
        0,
        "/v1/models",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"Vendor/Model-A"},{"id":"vendor/model-a"}]}"#.to_vec(),
        },
    );
    let created = post(
        &app,
        "/admin/accounts",
        r#"{"id":"vendor","kind":"api_key","label":"Vendor","secret":"vendor-secret"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert_eq!(
        (body["state"].as_str(), body["served_models_count"].as_u64()),
        (Some("ready"), Some(0)),
        "the row is stored with no catalog rows"
    );
    assert!(
        body["catalog_error"]
            .as_str()
            .is_some_and(|e| e.contains("two spellings")),
        "the clash is recorded: {}",
        body["catalog_error"]
    );
    assert_eq!(
        patch(
            &app,
            "/admin/accounts/vendor",
            r#"{"label":"Vendor 2"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200,
        "and the account can still be edited"
    );
}

#[tokio::test]
async fn a_sync_that_cannot_publish_answers_503_with_the_catalog_written() {
    let (app, up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    up.set_rule_after(
        0,
        "/v1/models",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"claude-sonnet-4-5"}]}"#.to_vec(),
        },
    );
    store.debug_fail_next_load();
    let refused = post(
        &app,
        "/admin/accounts/direct/models/sync",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 503);
    assert_eq!(
        refused.headers().get("x-phoebis-error-code").unwrap(),
        "registry_unavailable"
    );
    assert!(
        store
            .served_models("direct")
            .unwrap()
            .iter()
            .any(|m| m.upstream_id == "claude-sonnet-4-5"),
        "the rows were written; only the publication failed"
    );
    assert!(
        router
            .snapshot()
            .served("direct", "claude-sonnet-4-5")
            .is_none(),
        "and routing still serves the catalog it had"
    );
    router.reload().await.unwrap();
    assert!(router
        .snapshot()
        .served("direct", "claude-sonnet-4-5")
        .is_some());
}

#[tokio::test]
async fn a_discovered_id_clashing_with_a_stored_spelling_is_recorded_as_a_catalog_error() {
    let (app, up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    assert_eq!(
        put(
            &app,
            "/admin/accounts/direct/models",
            r#"{"models":[{"upstream_id":"Vendor/Model-A","selected":true}]}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    up.add_credential_rule(
        "sk-ant-api03-NEW",
        "/v1/models",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"vendor/model-a"}]}"#.to_vec(),
        },
    );
    let patched = patch(
        &app,
        "/admin/accounts/direct",
        r#"{"secret":"sk-ant-api03-NEW"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(patched.status(), 200);
    let listed = json(get(&app, "/admin/accounts/direct/models", Some(&cookie)).await).await;
    assert!(
        listed["catalog_error"]
            .as_str()
            .is_some_and(|e| e.contains("vendor/model-a")),
        "the clash is recorded rather than rolling the edit back"
    );
    assert_eq!(
        store
            .served_models("direct")
            .unwrap()
            .iter()
            .map(|m| m.upstream_id.clone())
            .collect::<Vec<_>>(),
        ["Vendor/Model-A"],
        "the stored catalog is untouched"
    );
    assert_eq!(
        store
            .api_key_credential("direct")
            .unwrap()
            .unwrap()
            .0
            .expose(),
        "sk-ant-api03-NEW",
        "and the verified secret still committed"
    );
}

#[tokio::test]
async fn a_sync_that_cannot_publish_says_whether_the_registry_was_written() {
    let (app, up, store, _router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    up.set_rule_after(
        0,
        "/v1/models",
        Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"claude-sonnet-4-5"}]}"#.to_vec(),
        })),
    );
    let syncing = {
        let app = app.clone();
        let cookie = cookie.clone();
        tokio::spawn(async move {
            post(
                &app,
                "/admin/accounts/direct/models/sync",
                "",
                Some(&cookie),
            )
            .await
        })
    };
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        up.requests()
            .iter()
            .any(|r| r.line.starts_with("GET /v1/models"))
            .then_some(())
    })
    .await;
    // The row moves under the discovery, so the result is stale and nothing is written.
    store
        .update_account("direct", "Direct (renamed)", None, "test")
        .unwrap();
    store.debug_fail_next_load();
    up.release();
    let refused = syncing.await.unwrap();
    assert_eq!(refused.status(), 503);
    assert_eq!(
        refused.headers().get("x-phoebis-error-code").unwrap(),
        "registry_unavailable"
    );
    let body = json(refused).await;
    assert_eq!(body["written"], false);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("nothing was written"),
        "{body}"
    );
    assert!(
        store.served_models("direct").unwrap().is_empty(),
        "the stale discovery wrote no rows"
    );
    // A discovery error: the error is recorded, no catalog row lands, and the body says so.
    up.set_rule_after(
        0,
        "/v1/models",
        Respond::Json {
            status: 500,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    store.debug_fail_next_load();
    let refused = post(
        &app,
        "/admin/accounts/direct/models/sync",
        "",
        Some(&cookie),
    )
    .await;
    let body = json(refused).await;
    assert_eq!(body["written"], false);
    assert!(
        body["error"].as_str().unwrap().contains("discovery failed"),
        "{body}"
    );
}

#[tokio::test]
async fn the_account_view_carries_its_profile_windows_pools_totals_and_models() {
    use phoebis_ledger::{
        AttemptRecord, ExtraUsageSampleRecord, QuotaSampleRecord, Record, RequestRecord,
    };
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    parts.store.set_profile_json("business", r#"{"account":{"uuid":"u","email_address":"business@example.invalid","display_name":"Biz","has_extra_usage_enabled":true},"organization":{"uuid":"o","name":"Biz Org","organization_type":"claude_max","billing_type":"stripe_subscription","rate_limit_tier":"default_claude_max_5x"}}"#).unwrap();
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"business","priority":2}]}"#, Some(&cookie)).await;
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
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "fable".into(),
                    utilization: Some(0.1),
                    reset_at: None,
                    source: "poll".into(),
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
    assert!(listed["served_models"].is_array() && listed["profile"]["display_name"] == "Biz");
    let direct = json(get(&parts.app, "/admin/accounts/direct", Some(&cookie)).await).await;
    assert!(
        direct["profile"].is_null()
            && direct["extra_usage"].is_null()
            && direct["windows"].as_array().unwrap().is_empty()
    );
    assert!(
        !json(get(&parts.app, "/admin/accounts", Some(&cookie)).await)
            .await
            .to_string()
            .contains("PLACEHOLDER"),
        "no credential in the list"
    );
}
