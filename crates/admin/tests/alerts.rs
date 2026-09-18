//! The alerts routes: rules with their validation and their secret kept out of every answer, events with their
//! deliveries and the state filter, the test delivery, the operator's resolve, and the viewer's limits.
mod common;
use common::{delete, get, json, login, patch, post};

#[tokio::test]
async fn rules_are_created_validated_edited_tested_and_deleted_without_leaking_the_secret() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
    let created = post(&parts.app, "/admin/alerts/rules", r#"{"kind":"utilization_above","scope":{"type":"pool","id":"main"},"threshold":0.9,"window":"7d","webhook_url":"http://127.0.0.1:1/hook","secret":"s3cret"}"#, Some(&cookie)).await;
    assert_eq!(created.status(), 201);
    let rule = json(created).await;
    assert_eq!(
        rule.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [
            "created_at",
            "enabled",
            "has_secret",
            "id",
            "kind",
            "scope",
            "threshold",
            "updated_at",
            "webhook_url",
            "window"
        ]
    );
    assert_eq!(
        (
            rule["has_secret"].as_bool(),
            rule["enabled"].as_bool(),
            rule["scope"]["type"].as_str(),
            rule["window"].as_str()
        ),
        (Some(true), Some(true), Some("pool"), Some("7d"))
    );
    assert!(!rule.to_string().contains("s3cret"));
    let id = rule["id"].as_str().unwrap().to_string();
    assert!(
        id.len() == 21
            && id.starts_with("rule_")
            && id[5..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "the id is rule_ and 16 lowercase hex characters, not {id}"
    );
    for (body, why) in [
        (
            r#"{"kind":"nope","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/h"}"#,
            "unknown kind",
        ),
        (
            r#"{"kind":"client_drift","scope":{"type":"pool","id":"main"},"webhook_url":"http://127.0.0.1:1/h"}"#,
            "client_drift is all-scoped",
        ),
        (
            r#"{"kind":"reauth_required","scope":{"type":"account","id":"ghost"},"webhook_url":"http://127.0.0.1:1/h"}"#,
            "an id that names nothing",
        ),
        (
            r#"{"kind":"reauth_required","scope":{"type":"any_account","id":"business"},"webhook_url":"http://127.0.0.1:1/h"}"#,
            "an id where none belongs",
        ),
        (
            r#"{"kind":"utilization_above","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/h"}"#,
            "no threshold",
        ),
        (
            r#"{"kind":"utilization_above","scope":{"type":"all"},"threshold":1.5,"window":"7d","webhook_url":"http://127.0.0.1:1/h"}"#,
            "threshold out of range",
        ),
        (
            r#"{"kind":"utilization_above","scope":{"type":"all"},"threshold":0.5,"window":"1h","webhook_url":"http://127.0.0.1:1/h"}"#,
            "unknown window",
        ),
        (
            r#"{"kind":"pool_exhausted","scope":{"type":"all"},"threshold":0.5,"webhook_url":"http://127.0.0.1:1/h"}"#,
            "a threshold on a kind without one",
        ),
        (
            r#"{"kind":"pool_exhausted","scope":{"type":"all"},"webhook_url":"ftp://x/y"}"#,
            "not http",
        ),
    ] {
        assert_eq!(
            post(&parts.app, "/admin/alerts/rules", body, Some(&cookie))
                .await
                .status(),
            400,
            "{why}"
        );
    }
    let unknown_scope = post(
        &parts.app,
        "/admin/alerts/rules",
        r#"{"kind":"client_drift","scope":{"type":"nope"},"webhook_url":"http://127.0.0.1:1/h"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(unknown_scope.status(), 400);
    assert!(
        json(unknown_scope).await["error"]
            .as_str()
            .unwrap()
            .contains("scope.type is"),
        "an unknown scope type is refused by name rather than as a malformed body"
    );
    let edited = patch(
        &parts.app,
        &format!("/admin/alerts/rules/{id}"),
        r#"{"threshold":0.8,"enabled":false}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(edited.status(), 200);
    let edited = json(edited).await;
    assert_eq!(
        (
            edited["threshold"].as_f64(),
            edited["enabled"].as_bool(),
            edited["has_secret"].as_bool()
        ),
        (Some(0.8), Some(false), Some(true)),
        "an edit without a secret keeps it"
    );
    assert!(!edited.to_string().contains("s3cret"));
    let cleared = json(
        patch(
            &parts.app,
            &format!("/admin/alerts/rules/{id}"),
            r#"{"secret":""}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(cleared["has_secret"], false);
    assert_eq!(
        patch(
            &parts.app,
            &format!("/admin/alerts/rules/{id}"),
            r#"{"scope":{"type":"key","id":"nope"}}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    let listed = json(get(&parts.app, "/admin/alerts/rules", Some(&cookie)).await).await;
    assert_eq!(listed["rules"].as_array().unwrap().len(), 1);
    parts.webhook.answer(Ok(204));
    let tested = json(
        post(
            &parts.app,
            &format!("/admin/alerts/rules/{id}/test"),
            "",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (tested["status"].as_u64(), tested["error"].is_null()),
        (Some(204), true)
    );
    let sent = parts.webhook.posts().pop().unwrap();
    assert_eq!(
        (sent.body["state"].as_str(), sent.body["kind"].as_str()),
        (Some("test"), Some("utilization_above"))
    );
    assert_eq!(
        delete(
            &parts.app,
            &format!("/admin/alerts/rules/{id}"),
            Some(&cookie)
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        delete(
            &parts.app,
            &format!("/admin/alerts/rules/{id}"),
            Some(&cookie)
        )
        .await
        .status(),
        404
    );
    assert!(
        json(get(&parts.app, "/admin/alerts/rules", Some(&cookie)).await).await["rules"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        patch(
            &parts.app,
            &format!("/admin/alerts/rules/{id}"),
            "{}",
            Some(&cookie)
        )
        .await
        .status(),
        404,
        "an edit of a rule that is gone"
    );
    assert_eq!(
        post(
            &parts.app,
            &format!("/admin/alerts/rules/{id}/test"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        404,
        "a test delivery for a rule that is gone"
    );
}

#[tokio::test]
async fn events_list_with_their_deliveries_filter_by_state_and_resolve_by_hand_and_the_viewer_reads_only(
) {
    use phoebis_alerts::{Kind, Signal};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let rule = json(post(&parts.app, "/admin/alerts/rules", r#"{"kind":"client_drift","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/hook"}"#, Some(&cookie)).await).await;
    parts.webhook.answer(Err("refused".into()));
    parts.webhook.answer(Ok(500));
    parts.webhook.answer(Ok(200));
    parts.alerts.signal(&Signal {
        kind: Kind::ClientDrift,
        account_id: None,
        key_id: Some("desktop".into()),
        pool_id: None,
        subject: "client 2.1.274".into(),
        detail: serde_json::json!({ "client_version": "2.1.274", "key": "desktop", "trigger": "hash" }),
    });
    parts.alerts.wait_idle().await;
    let events = json(get(&parts.app, "/admin/alerts/events", Some(&cookie)).await).await;
    let event = &events["events"][0];
    assert_eq!(
        event
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [
            "deliveries",
            "detail",
            "id",
            "kind",
            "opened_at",
            "resolved_at",
            "rule_id",
            "state",
            "subject"
        ]
    );
    assert_eq!(
        (
            event["state"].as_str(),
            event["subject"].as_str(),
            event["rule_id"].as_str(),
            event["detail"]["trigger"].as_str()
        ),
        (
            Some("open"),
            Some("client 2.1.274"),
            rule["id"].as_str(),
            Some("hash")
        )
    );
    assert_eq!(
        event["deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (
                d["attempt"].as_u64(),
                d["status"].as_u64(),
                d["error"].as_str().map(String::from)
            ))
            .collect::<Vec<_>>(),
        [
            (Some(1), None, Some("refused".into())),
            (Some(2), Some(500), None),
            (Some(3), Some(200), None)
        ]
    );
    let id = event["id"].as_str().unwrap().to_string();
    assert_eq!(
        json(
            get(
                &parts.app,
                "/admin/alerts/events?state=resolved",
                Some(&cookie)
            )
            .await
        )
        .await["events"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        get(&parts.app, "/admin/alerts/events?state=nope", Some(&cookie))
            .await
            .status(),
        400
    );
    let deliveries = json(
        get(
            &parts.app,
            &format!("/admin/alerts/events/{id}/deliveries"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(deliveries["deliveries"].as_array().unwrap().len(), 3);
    let viewer = login(&parts.app, "viewer-secret").await;
    assert_eq!(
        get(&parts.app, "/admin/alerts/rules", Some(&viewer))
            .await
            .status(),
        200
    );
    assert_eq!(
        get(&parts.app, "/admin/alerts/events", Some(&viewer))
            .await
            .status(),
        200
    );
    assert_eq!(
        post(
            &parts.app,
            &format!("/admin/alerts/events/{id}/resolve"),
            "",
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        post(&parts.app, "/admin/alerts/rules", "{}", Some(&viewer))
            .await
            .status(),
        403
    );
    parts.webhook.answer(Ok(200));
    let resolved = post(
        &parts.app,
        &format!("/admin/alerts/events/{id}/resolve"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(resolved.status(), 200);
    assert_eq!(json(resolved).await["state"], "resolved");
    assert_eq!(
        post(
            &parts.app,
            &format!("/admin/alerts/events/{id}/resolve"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        post(
            &parts.app,
            "/admin/alerts/events/nope/resolve",
            "",
            Some(&cookie)
        )
        .await
        .status(),
        404
    );
    parts.alerts.wait_idle().await;
    assert_eq!(
        json(
            get(
                &parts.app,
                "/admin/alerts/events?state=resolved",
                Some(&cookie)
            )
            .await
        )
        .await["events"][0]["deliveries"]
            .as_array()
            .unwrap()
            .len(),
        4,
        "the resolve delivery joins the list"
    );
    assert_eq!(
        get(
            &parts.app,
            "/admin/alerts/events/nope/deliveries",
            Some(&cookie)
        )
        .await
        .status(),
        404
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (
            system["alerts"]["rules"].as_u64(),
            system["alerts"]["open"].as_u64(),
            system["alerts"]["deliveries_ok"].as_u64(),
            system["alerts"]["deliveries_failed"].as_u64()
        ),
        (Some(1), Some(0), Some(2), Some(0))
    );
}

/// The question in a detail is text out of a captured response body, which the viewer may not read on
/// `/admin/requests/{id}/bodies` either.
#[tokio::test]
async fn a_viewer_reads_an_event_without_the_question_its_detail_carries() {
    use phoebis_alerts::{Kind, Signal};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let created = post(&parts.app, "/admin/alerts/rules", r#"{"kind":"ask_user_question","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/hook"}"#, Some(&cookie)).await;
    assert_eq!(created.status(), 201);
    parts.alerts.signal(&Signal {
        kind: Kind::AskUserQuestion,
        account_id: None,
        key_id: Some("desktop".into()),
        pool_id: None,
        subject: "sess-1".into(),
        detail: serde_json::json!({ "agent": "main", "key": "desktop", "question": "Which branch should I push?", "session": "sess-1" }),
    });
    parts.alerts.wait_idle().await;
    let seen = json(get(&parts.app, "/admin/alerts/events", Some(&cookie)).await).await;
    let detail = &seen["events"][0]["detail"];
    assert_eq!(
        detail["question"].as_str(),
        Some("Which branch should I push?")
    );
    assert!(
        detail.get("question_redacted").is_none(),
        "an admin reads the text and is told nothing about redaction"
    );
    let viewer = login(&parts.app, "viewer-secret").await;
    let seen = json(get(&parts.app, "/admin/alerts/events", Some(&viewer)).await).await;
    let event = &seen["events"][0];
    assert_eq!(
        (
            event["detail"]["question"].as_null(),
            event["detail"]["question_redacted"].as_bool(),
            event["detail"]["session"].as_str()
        ),
        (Some(()), Some(true), Some("sess-1")),
        "the text goes, the redaction is stated, every other field stays"
    );
    assert!(!seen.to_string().contains("Which branch"));
    let id = event["id"].as_str().unwrap();
    let deliveries = json(
        get(
            &parts.app,
            &format!("/admin/alerts/events/{id}/deliveries"),
            Some(&viewer),
        )
        .await,
    )
    .await;
    assert_eq!(deliveries["deliveries"].as_array().unwrap().len(), 1);
}

/// The alerts crate posts wherever a rule points and follows no redirect, so these routes are the only place a
/// destination is checked. Every other harness leaves the check off, since its receiver is on loopback.
#[tokio::test]
async fn a_rule_may_not_aim_its_webhook_at_an_address_only_the_router_can_reach() {
    let parts = common::admin_parts_full_refusing_local_webhooks().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let rule = |url: &str| {
        format!(r#"{{"kind":"client_drift","scope":{{"type":"all"}},"webhook_url":"{url}"}}"#)
    };
    for (url, reason) in [
        ("http://127.0.0.1:9000/hook", "loopback"),
        ("http://localhost/hook", "loopback"),
        ("http://localhost./hook", "loopback"),
        ("http://hook.localhost./hook", "loopback"),
        ("http://[::1]/hook", "loopback"),
        // An IPv4-mapped or IPv4-compatible address reaches the IPv4 host, so it is refused as that host.
        ("http://[::ffff:127.0.0.1]/hook", "loopback"),
        ("http://[0:0:0:0:0:ffff:7f00:1]/hook", "loopback"),
        ("http://[::127.0.0.1]/hook", "loopback"),
        ("http://169.254.169.254/latest/meta-data", "link-local"),
        ("http://[fe80::1]/hook", "link-local"),
        (
            "http://[::ffff:169.254.169.254]/latest/meta-data",
            "link-local",
        ),
        ("http://0.0.0.0/hook", "the unspecified address"),
        ("http://[::]/hook", "the unspecified address"),
    ] {
        let refused = post(&parts.app, "/admin/alerts/rules", &rule(url), Some(&cookie)).await;
        assert_eq!(refused.status(), 400, "{url}");
        let body = json(refused).await;
        assert!(
            body["error"].as_str().unwrap().contains(reason),
            "{url}: the refusal names its reason, not {body}"
        );
    }
    let created = post(
        &parts.app,
        "/admin/alerts/rules",
        &rule("https://hooks.example.invalid/x"),
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let id = json(created).await["id"].as_str().unwrap().to_string();
    assert_eq!(
        patch(
            &parts.app,
            &format!("/admin/alerts/rules/{id}"),
            &format!(r#"{{"webhook_url":"{}"}}"#, "http://127.0.0.1:1/hook"),
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "an edit is held to the same destinations as a create"
    );
}

#[tokio::test]
async fn a_rule_with_an_open_event_refuses_a_change_of_condition_until_it_resolves() {
    use phoebis_alerts::{Kind, Signal};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let rule = json(post(&parts.app, "/admin/alerts/rules", r#"{"kind":"client_drift","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/hook"}"#, Some(&cookie)).await).await;
    let id = rule["id"].as_str().unwrap().to_string();
    parts.webhook.answer(Ok(200));
    parts.alerts.signal(&Signal {
        kind: Kind::ClientDrift,
        account_id: None,
        key_id: Some("desktop".into()),
        pool_id: None,
        subject: "client 2.1.274".into(),
        detail: serde_json::json!({ "client_version": "2.1.274", "key": "desktop" }),
    });
    parts.alerts.wait_idle().await;
    let path = format!("/admin/alerts/rules/{id}");
    let refused = patch(
        &parts.app,
        &path,
        r#"{"kind":"pool_exhausted"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 409);
    assert_eq!(
        json(refused).await["error"].as_str(),
        Some("the condition cannot change while 1 event is open for this rule")
    );
    let stored_kind = |rules: serde_json::Value| {
        rules["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"].as_str() == Some(id.as_str()))
            .unwrap()["kind"]
            .as_str()
            .map(String::from)
    };
    assert_eq!(
        stored_kind(json(get(&parts.app, "/admin/alerts/rules", Some(&cookie)).await).await),
        Some("client_drift".into()),
        "the refused patch wrote nothing"
    );
    assert_eq!(
        patch(&parts.app, &path, r#"{"enabled":false}"#, Some(&cookie))
            .await
            .status(),
        200,
        "every other field still edits while an event is open"
    );
    assert_eq!(
        patch(
            &parts.app,
            &path,
            r#"{"kind":"client_drift"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200,
        "the same kind is not a change"
    );
    let events = json(get(&parts.app, "/admin/alerts/events", Some(&cookie)).await).await;
    let event = events["events"][0]["id"].as_str().unwrap().to_string();
    parts.webhook.answer(Ok(200));
    assert_eq!(
        post(
            &parts.app,
            &format!("/admin/alerts/events/{event}/resolve"),
            "",
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    parts.alerts.wait_idle().await;
    assert_eq!(
        patch(
            &parts.app,
            &path,
            r#"{"kind":"pool_exhausted"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200,
        "the condition moves once nothing is open under it"
    );
}
