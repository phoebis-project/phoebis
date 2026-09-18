mod common;
use phoebis_alerts::{Kind, ScopeType, Signal};
use common::*;

#[tokio::test]
async fn a_signal_opens_one_event_per_matching_rule_and_subject_and_a_clear_resolves_it() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("any", Kind::ReauthRequired, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &rule(
                "biz",
                Kind::ReauthRequired,
                ScopeType::Account,
                Some("business"),
            ),
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &rule("off", Kind::ReauthRequired, ScopeType::AnyAccount, None).with_enabled(false),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    rig.alerts.signal(&signal(
        Kind::ReauthRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    rig.alerts.signal(&signal(
        Kind::ReauthRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    settled(&rig).await;
    let open = rig.store.open_alert_events().unwrap();
    // Both events opened on the same tick, so the store's order falls to the random ids; the claim is one
    // event per rule, not an order.
    let mut opened_by = open.iter().map(|e| e.rule_id.as_str()).collect::<Vec<_>>();
    opened_by.sort_unstable();
    assert_eq!(
        opened_by,
        ["any", "biz"],
        "one per rule, once, disabled rules skipped"
    );
    let posts = rig.webhook.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        (
            posts[0].body["state"].as_str(),
            posts[0].body["kind"].as_str(),
            posts[0].body["subject"].as_str(),
            posts[0].body["router"].as_str()
        ),
        (
            Some("opened"),
            Some("reauth_required"),
            Some("business"),
            Some("phoebis-test")
        )
    );
    assert!(
        posts[0].body["opened_at"].is_i64()
            && posts[0].body["resolved_at"].is_null()
            && posts[0].body["rule"] == "any"
    );
    assert!(posts.iter().all(|p| p.signature.is_none()));
    rig.alerts.signal(&signal(
        Kind::ReauthRequired,
        Some("direct"),
        None,
        None,
        "direct",
    ));
    settled(&rig).await;
    assert_eq!(
        rig.store.open_alert_events().unwrap().len(),
        3,
        "a second subject opens its own event on the any_account rule only"
    );
    rig.clock
        .store(NOW + 60_000, std::sync::atomic::Ordering::SeqCst);
    rig.alerts.clear(Kind::ReauthRequired, "business");
    settled(&rig).await;
    assert_eq!(rig.store.open_alert_events().unwrap().len(), 1);
    let posts = rig.webhook.posts();
    assert_eq!(posts.len(), 5);
    assert_eq!(
        (
            posts[3].body["state"].as_str(),
            posts[3].body["resolved_at"].as_i64()
        ),
        (Some("resolved"), Some(NOW + 60_000))
    );
    let events = fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        let rows = rig.db.events(0, i64::MAX, 100).unwrap();
        (rows.iter().filter(|e| e.kind == "alert_resolved").count() == 2).then_some(rows)
    })
    .await;
    assert_eq!(
        events.iter().filter(|e| e.kind == "alert_opened").count(),
        3
    );
    let opened = events.iter().find(|e| e.kind == "alert_opened").unwrap();
    assert_eq!(opened.account_id.as_deref(), Some("business"));
    let detail: serde_json::Value =
        serde_json::from_str(opened.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        (detail["rule"].as_str(), detail["subject"].as_str()),
        (Some("any"), Some("business"))
    );
    let stats = rig.alerts.stats();
    assert_eq!(
        (
            stats.rules,
            stats.open,
            stats.deliveries_ok,
            stats.deliveries_failed
        ),
        (3, 1, 5, 0)
    );
}

#[tokio::test]
async fn two_concurrent_openers_of_one_subject_land_one_event_and_one_delivery() {
    use std::sync::Arc;
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    // Two enrichment workers for two agents of one session reach the engine at once; a Notify releases both on
    // the same tick so neither sees the other's row before its own insert.
    let go = Arc::new(tokio::sync::Notify::new());
    let mut tasks = Vec::new();
    for agent in ["main", "sub"] {
        let (alerts, go) = (rig.alerts.clone(), go.clone());
        let signal = Signal {
            detail: serde_json::json!({ "agent": agent, "key": "desktop", "question": null, "session": "s1" }),
            ..signal(
                Kind::AskUserQuestion,
                None,
                Some("desktop"),
                Some("main"),
                "desktop, session s1",
            )
        };
        tasks.push(tokio::spawn(async move {
            go.notified().await;
            alerts.signal(&signal);
        }));
    }
    tokio::task::yield_now().await;
    go.notify_waiters();
    for task in tasks {
        task.await.unwrap();
    }
    settled(&rig).await;
    assert_eq!(
        rig.store.open_alert_events().unwrap().len(),
        1,
        "the unique open index admits one row"
    );
    assert_eq!(
        rig.webhook.posts().len(),
        1,
        "the opener whose insert did not land posts nothing"
    );
    let events = fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        let rows = rig.db.events(0, i64::MAX, 100).unwrap();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        events.iter().filter(|e| e.kind == "alert_opened").count(),
        1
    );
}

#[test]
fn scopes_admit_what_the_spec_table_allows() {
    use phoebis_alerts::{allowed_scopes, Rule, Scope, Signal};
    let rule = |kind: Kind, scope: ScopeType, id: Option<&str>| Rule {
        id: "r".into(),
        kind,
        scope: Scope {
            kind: scope,
            id: id.map(String::from),
        },
        threshold: None,
        window: None,
        webhook_url: "u".into(),
        has_secret: false,
        enabled: true,
    };
    let sig = |kind: Kind, a: Option<&str>, k: Option<&str>, p: Option<&str>| Signal {
        kind,
        account_id: a.map(String::from),
        key_id: k.map(String::from),
        pool_id: p.map(String::from),
        subject: "s".into(),
        detail: serde_json::json!({}),
    };
    assert!(
        rule(Kind::PoolExhausted, ScopeType::Pool, Some("main")).matches(&sig(
            Kind::PoolExhausted,
            None,
            Some("desktop"),
            Some("main")
        ))
    );
    assert!(
        !rule(Kind::PoolExhausted, ScopeType::Pool, Some("other")).matches(&sig(
            Kind::PoolExhausted,
            None,
            None,
            Some("main")
        ))
    );
    assert!(
        rule(Kind::AskUserQuestion, ScopeType::Key, Some("desktop")).matches(&sig(
            Kind::AskUserQuestion,
            Some("business"),
            Some("desktop"),
            Some("main")
        ))
    );
    assert!(
        !rule(Kind::AskUserQuestion, ScopeType::All, None).matches(&sig(
            Kind::ClientDrift,
            None,
            None,
            None
        )),
        "kinds must match"
    );
    assert!(
        rule(Kind::UtilizationAbove, ScopeType::AnyAccount, None).matches(&sig(
            Kind::UtilizationAbove,
            Some("business"),
            None,
            None
        ))
    );
    assert!(
        !rule(Kind::UtilizationAbove, ScopeType::AnyAccount, None).matches(&sig(
            Kind::UtilizationAbove,
            None,
            None,
            None
        ))
    );
    assert_eq!(allowed_scopes(Kind::ClientDrift), &[ScopeType::All]);
    assert!(
        allowed_scopes(Kind::ReauthRequired).contains(&ScopeType::AnyAccount)
            && !allowed_scopes(Kind::ReauthRequired).contains(&ScopeType::Key)
    );
    assert_eq!(Kind::ALL.len(), 8);
    assert_eq!(
        Kind::parse("keepalive_cost_cap"),
        Some(Kind::KeepaliveCostCap)
    );
}

#[test]
fn an_ask_user_question_is_found_across_split_frames_and_its_text_in_a_body() {
    use phoebis_alerts::{
        question_in_document, question_in_event, question_text, SseCutter, SSE_BUFFER_MAX,
    };
    let frame = b"event: content_block_start\ndata: {\"type\": \"content_block_start\", \"index\": 1, \"content_block\": {\"type\": \"tool_use\", \"id\": \"toolu_1\", \"name\": \"AskUserQuestion\", \"input\": {}}}\n\n";
    // Every byte boundary: the frame arrives as two transport pieces and the event completes on the second.
    for cut in 1..frame.len() - 1 {
        let mut cutter = SseCutter::default();
        let first = cutter.push(&frame[..cut]);
        let second = cutter.push(&frame[cut..]);
        let events: Vec<Vec<u8>> = first.into_iter().chain(second).collect();
        assert_eq!(events.len(), 1, "split at {cut}");
        assert!(
            question_in_event(&events[0]),
            "split at {cut}: spaces after colons are valid JSON"
        );
    }
    let mut cutter = SseCutter::default();
    let two = cutter.push(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"AskUserQuestion is a tool\"}}\r\n\r\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Read\",\"input\":{}}}\n\n");
    assert_eq!(two.len(), 2, "CRLF and LF frames both cut");
    assert!(
        !question_in_event(&two[0]),
        "a text delta that mentions the name is not a block start"
    );
    assert!(
        !question_in_event(&two[1]),
        "another tool's block start is not it"
    );
    let mut cutter = SseCutter::default();
    assert!(
        cutter.push(&vec![b'x'; SSE_BUFFER_MAX + 1]).is_empty() && cutter.overflowed(),
        "past the cap the response stops being scanned"
    );
    assert!(question_in_document(
        br#"{"id":"msg","content":[{"type":"text","text":"Sure."},{"type":"tool_use","name":"AskUserQuestion","input":{}}]}"#
    ));
    assert!(!question_in_document(
        br#"{"content":[{"type":"text","text":"AskUserQuestion"}]}"#
    ));
    let body = br#"{"id":"msg","content":[{"type":"text","text":"Sure."},{"type":"tool_use","id":"toolu_1","name":"AskUserQuestion","input":{"questions":[{"question":"Rewrite in place or swap?","header":"Migration","options":[]}]}}]}"#;
    assert_eq!(
        question_text(body).as_deref(),
        Some("Rewrite in place or swap?")
    );
    assert_eq!(
        question_text(
            br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Which one?"}}]}"#
        )
        .as_deref(),
        Some("Which one?")
    );
    let sse = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"questions\\\":[{\\\"question\\\":\\\"Relax or fix?\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\",\\\"header\\\":\\\"Tests\\\"}]}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n";
    assert_eq!(
        question_text(sse).as_deref(),
        Some("Relax or fix?"),
        "a streamed body reassembles the tool input from its deltas"
    );
    assert_eq!(question_text(b"not json"), None);
}
