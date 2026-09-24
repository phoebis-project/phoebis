//! Acceptance: the viewer secret reads every view and refreshes usage, and gets 403 on every mutation, on key
//! creation, and on captured bodies. The route list is the table in routes.rs; a route added there without an
//! entry here fails the count assertion at the end.
mod common;
use common::{delete, get, json, login, patch, post, put};

const GETS: &[&str] = &[
    "/admin/accounts",
    "/admin/accounts/business",
    "/admin/accounts/business/catalog",
    "/admin/accounts/business/quota?range=7d",
    "/admin/accounts/business/usage",
    "/admin/alerts/deliveries",
    "/admin/alerts/events",
    "/admin/alerts/rules",
    "/admin/analytics/accounts?range=24h",
    "/admin/analytics/breakdown?range=24h&group_by=key",
    "/admin/analytics/series?range=24h&group_by=key",
    "/admin/dashboard",
    "/admin/keepalive/overview",
    "/admin/keepalive/recipes",
    "/admin/keys",
    "/admin/keys/desktop/models",
    "/admin/live/snapshot",
    "/admin/models/state",
    "/admin/pools",
    "/admin/pools/main/traffic?range=24h",
    "/admin/pricing",
    "/admin/requests",
    "/admin/session",
    "/admin/sessions",
    "/admin/sessions/costs",
    "/admin/settings",
    "/admin/system",
];

#[tokio::test]
async fn the_viewer_reads_every_view_refreshes_usage_and_is_refused_every_mutation() {
    let parts = common::admin_parts_full().await;
    let admin = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"business","priority":0}]}"#, Some(&admin)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&admin),
    )
    .await;
    let rule = json(
        post(
            &parts.app,
            "/admin/alerts/rules",
            r#"{"kind":"ask_user_question","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/h"}"#,
            Some(&admin),
        )
        .await,
    )
    .await;
    let rule_id = rule["id"].as_str().unwrap().to_string();
    // A sentinel question copied out of a captured body: the one detail field a viewer must not read.
    parts.alerts.signal(&phoebis_alerts::Signal {
        kind: phoebis_alerts::Kind::AskUserQuestion,
        account_id: None,
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        subject: "desktop, session s1".into(),
        detail: serde_json::json!({ "agent": "main", "key": "desktop", "question": "SENTINEL: rewrite in place?", "session": "s1" }),
    });
    parts.alerts.wait_idle().await;
    let events = json(get(&parts.app, "/admin/alerts/events", Some(&admin)).await).await;
    let event_id = events["events"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        events["events"][0]["detail"]["question"], "SENTINEL: rewrite in place?",
        "the admin reads the text"
    );
    assert!(events["events"][0]["detail"]
        .get("question_redacted")
        .is_none());
    let pairing = json(
        post(
            &parts.app,
            "/admin/oauth/sessions",
            r#"{"account_id":"business","login":"claude_ai"}"#,
            Some(&admin),
        )
        .await,
    )
    .await;
    let pairing_id = pairing["id"].as_str().unwrap().to_string();
    parts.ledger.record(phoebis_ledger::Record::Request {
        request: phoebis_ledger::RequestRecord {
            id: "cap".into(),
            received_at: common::NOW,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            model: "m".into(),
            canonical_model: "m".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            bodies_captured: true,
            ..Default::default()
        },
        attempts: vec![],
        bodies: Some(phoebis_ledger::BodyRecord {
            request_id: "cap".into(),
            request_body: b"{}".to_vec(),
            response_body: None,
            request_len_seen: 2,
            response_len_seen: 0,
            expires_at: common::NOW + 1,
        }),
    });
    fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        parts.db.bodies("cap").await.unwrap().map(|_| ())
    })
    .await;

    let viewer = login(&parts.app, "viewer-secret").await;
    let deliveries = format!("/admin/alerts/events/{event_id}/deliveries");
    let pairing_path = format!("/admin/oauth/sessions/{pairing_id}");
    for path in GETS.iter().copied().chain([
        deliveries.as_str(),
        pairing_path.as_str(),
        "/admin/requests/cap",
    ]) {
        let status = get(&parts.app, path, Some(&viewer)).await.status();
        // The viewer never started this pairing, so the route answers 404 for it rather than leaking another
        // session's state.
        let expected = if path.starts_with("/admin/oauth/sessions/") {
            404
        } else {
            200
        };
        assert_eq!(status, expected, "GET {path}");
    }
    assert_eq!(
        get(&parts.app, "/admin/requests/cap/bodies", Some(&viewer))
            .await
            .status(),
        403,
        "captured bodies"
    );
    let seen = json(get(&parts.app, "/admin/alerts/events", Some(&viewer)).await).await;
    assert!(
        seen["events"][0]["detail"]["question"].is_null(),
        "the viewer never reads a question copied from a captured body"
    );
    assert_eq!(seen["events"][0]["detail"]["question_redacted"], true);
    assert_eq!(
        seen["events"][0]["detail"]["session"], "s1",
        "every other detail field stays"
    );
    assert!(!seen.to_string().contains("SENTINEL"));
    assert_eq!(
        post(
            &parts.app,
            "/admin/accounts/business/usage/refresh",
            "",
            Some(&viewer)
        )
        .await
        .status(),
        200,
        "the one POST a viewer may call"
    );
    let mutations: Vec<(&str, String, &str)> = vec![
        (
            "POST",
            "/admin/accounts".into(),
            r#"{"id":"x","kind":"oauth","label":"x"}"#,
        ),
        (
            "PATCH",
            "/admin/accounts/business".into(),
            r#"{"label":"x"}"#,
        ),
        ("DELETE", "/admin/accounts/business".into(), ""),
        ("POST", "/admin/accounts/business/clear-funding".into(), ""),
        (
            "DELETE",
            "/admin/accounts/business/cooldowns/account".into(),
            "",
        ),
        ("DELETE", "/admin/accounts/business/credentials".into(), ""),
        ("POST", "/admin/accounts/business/pause".into(), ""),
        ("POST", "/admin/accounts/business/resume".into(), ""),
        ("POST", "/admin/accounts/direct/verify".into(), ""),
        (
            "POST",
            format!("/admin/alerts/events/{event_id}/resolve"),
            "",
        ),
        ("POST", "/admin/alerts/rules".into(), "{}"),
        ("PATCH", format!("/admin/alerts/rules/{rule_id}"), "{}"),
        ("DELETE", format!("/admin/alerts/rules/{rule_id}"), ""),
        ("POST", format!("/admin/alerts/rules/{rule_id}/test"), ""),
        (
            "POST",
            "/admin/keys".into(),
            r#"{"id":"k2","label":"k2","pool_id":"main"}"#,
        ),
        ("PATCH", "/admin/keys/desktop".into(), "{}"),
        ("DELETE", "/admin/keys/desktop".into(), ""),
        ("POST", "/admin/keys/desktop/revoke".into(), ""),
        ("POST", "/admin/keys/desktop/rotate".into(), ""),
        (
            "POST",
            "/admin/oauth/sessions".into(),
            r#"{"account_id":"business"}"#,
        ),
        ("DELETE", format!("/admin/oauth/sessions/{pairing_id}"), ""),
        (
            "POST",
            format!("/admin/oauth/sessions/{pairing_id}/callback"),
            "{}",
        ),
        (
            "POST",
            format!("/admin/oauth/sessions/{pairing_id}/confirm"),
            "",
        ),
        (
            "POST",
            "/admin/pools".into(),
            r#"{"id":"p2","label":"p2","strategy":"priority_random"}"#,
        ),
        ("PATCH", "/admin/pools/main".into(), "{}"),
        ("DELETE", "/admin/pools/main".into(), ""),
        (
            "PUT",
            "/admin/pools/main/members".into(),
            r#"{"members":[]}"#,
        ),
        (
            "POST",
            "/admin/pricing/import".into(),
            r#"{"url":"http://127.0.0.1:1/p"}"#,
        ),
        (
            "POST",
            "/admin/pricing/import/confirm".into(),
            r#"{"token":"x"}"#,
        ),
        ("POST", "/admin/sessions/desktop/s1/release".into(), ""),
    ];
    for (method, path, body) in &mutations {
        let status = match *method {
            "POST" => post(&parts.app, path, body, Some(&viewer)).await.status(),
            "PATCH" => patch(&parts.app, path, body, Some(&viewer)).await.status(),
            "PUT" => put(&parts.app, path, body, Some(&viewer)).await.status(),
            _ => delete(&parts.app, path, Some(&viewer)).await.status(),
        };
        assert_eq!(status, 403, "{method} {path}");
    }
    assert_eq!(
        delete(&parts.app, "/admin/session", Some(&viewer))
            .await
            .status(),
        204,
        "ending its own session is the viewer's other allowed mutation"
    );
    // Every route in the table is read or refused above, apart from the live WebSocket, which takes a
    // handshake rather than a request, and the two catch-alls that answer an unknown path. rustfmt wraps a
    // long `.route(` call over several lines, so the count runs over the file with its whitespace removed.
    let table =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/routes.rs")).unwrap();
    let routed = table
        .split_whitespace()
        .collect::<String>()
        .matches(".route(\"/admin/")
        .count();
    assert_eq!(routed, 54, "a route joined the table; add it to this test");
    // The state the mutations would have changed is as the admin left it.
    assert_eq!(
        json(get(&parts.app, "/admin/keys", Some(&admin)).await).await["keys"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        json(get(&parts.app, "/admin/alerts/rules", Some(&admin)).await).await["rules"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
