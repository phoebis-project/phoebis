mod common;
use phoebis_alerts::{body, signature, Kind, ScopeType, BACKOFF};
use phoebis_store::Secret;
use common::*;

#[tokio::test]
async fn a_secret_signs_the_body_a_failing_endpoint_gets_three_attempts_and_a_test_posts_once() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("signed", Kind::FundingRequired, ScopeType::AnyAccount, None),
            Some(Some(&Secret::new("s3cret"))),
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    rig.webhook.answer(Err("connect timeout".into()));
    rig.webhook.answer(Ok(503));
    rig.webhook.answer(Ok(200));
    rig.alerts.signal(&signal(
        Kind::FundingRequired,
        Some("direct"),
        None,
        None,
        "direct",
    ));
    settled(&rig).await;
    let posts = rig.webhook.posts();
    assert_eq!(posts.len(), 3, "three attempts until the 200");
    let raw = serde_json::to_vec(&posts[0].body).unwrap();
    assert_eq!(
        posts[0].signature.as_deref(),
        Some(signature(&Secret::new("s3cret"), &raw).as_str())
    );
    assert!(posts[0]
        .signature
        .as_deref()
        .unwrap()
        .starts_with("sha256="));
    let event = &rig.store.open_alert_events().unwrap()[0];
    let rows = rig.store.alert_deliveries(&event.id).unwrap();
    assert_eq!(
        rows.iter()
            .map(|d| (d.attempt, d.status, d.error.as_deref()))
            .collect::<Vec<_>>(),
        [
            (1, None, Some("connect timeout")),
            (2, Some(503), None),
            (3, Some(200), None)
        ]
    );
    assert!(rows.iter().all(|d| d.state == "opened"));
    let text = rig
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(r#"phoebis_alert_deliveries_total{outcome="delivered",rule="signed"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"phoebis_alert_deliveries_total{outcome="failed",rule="signed"} 2"#),
        "{text}"
    );
    // A dead endpoint: three attempts, all failed, and the event stays open with its rows.
    rig.webhook.answer(Err("refused".into()));
    rig.webhook.answer(Err("refused".into()));
    rig.webhook.answer(Err("refused".into()));
    rig.alerts.signal(&signal(
        Kind::FundingRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    settled(&rig).await;
    let dead = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .find(|e| e.subject == "business")
        .unwrap();
    assert_eq!(rig.store.alert_deliveries(&dead.id).unwrap().len(), 3);
    assert_eq!(rig.webhook.posts().len(), 6);
    // The test delivery: one post, state test, no event row.
    rig.webhook.answer(Ok(204));
    let result = rig.alerts.test("signed").await;
    assert_eq!((result.status, result.error.as_deref()), (Some(204), None));
    let last = rig.webhook.posts().pop().unwrap();
    assert_eq!(
        (last.body["state"].as_str(), last.body["subject"].as_str()),
        (Some("test"), Some("test"))
    );
    assert!(last.signature.is_some());
    assert_eq!(
        rig.store.open_alert_events().unwrap().len(),
        2,
        "a test opens nothing"
    );
    assert_eq!(BACKOFF.map(|d| d.as_secs()), [1, 4, 16]);
}

#[tokio::test]
async fn a_resolve_waits_behind_the_open_it_follows() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ordered", Kind::ReauthRequired, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    // The open's first attempt hangs in the receiver; the event resolves meanwhile; the resolve must not overtake.
    let hold = rig.webhook.hold_posts();
    rig.alerts.signal(&signal(
        Kind::ReauthRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        rig.webhook.posts().len(),
        1,
        "the open is in flight and held"
    );
    rig.alerts.clear(Kind::ReauthRequired, "business");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        rig.webhook.posts().len(),
        1,
        "the resolve waits in the event's queue while the open is held"
    );
    assert_eq!(
        rig.store.alert_events(Some("resolved"), 10).unwrap().len(),
        1,
        "the store already says resolved"
    );
    rig.webhook.release_posts(&hold);
    settled(&rig).await;
    let states: Vec<String> = rig
        .webhook
        .posts()
        .iter()
        .map(|p| p.body["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        states,
        ["opened", "resolved"],
        "the receiver saw the open before the resolve"
    );
    let event = &rig.store.alert_events(Some("resolved"), 10).unwrap()[0];
    let rows = rig.store.alert_deliveries(&event.id).unwrap();
    assert_eq!(
        rows.iter().map(|d| d.state.as_str()).collect::<Vec<_>>(),
        ["opened", "resolved"]
    );
}

#[test]
fn the_body_is_the_spec_shape_and_the_signature_is_hmac_sha256_hex() {
    use phoebis_alerts::{Rule, Scope};
    let rule = Rule {
        id: "r1".into(),
        kind: Kind::ReauthRequired,
        scope: Scope {
            kind: ScopeType::AnyAccount,
            id: None,
        },
        threshold: None,
        window: None,
        webhook_url: "u".into(),
        has_secret: true,
        enabled: true,
    };
    let event = phoebis_store::AlertEventRow {
        id: "e1".into(),
        rule_id: "r1".into(),
        kind: "reauth_required".into(),
        subject: "business".into(),
        state: "open".into(),
        opened_at: 5,
        resolved_at: None,
        detail_json: Some(r#"{"account":"business"}"#.into()),
    };
    let raw = body(&rule, &event, "opened", "phoebis");
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        [
            "detail",
            "kind",
            "opened_at",
            "resolved_at",
            "router",
            "rule",
            "state",
            "subject"
        ]
    );
    assert_eq!(v["detail"]["account"], "business");
    assert_eq!(
        signature(
            &Secret::new("key"),
            b"The quick brown fox jumps over the lazy dog"
        ),
        "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}
