mod common;
use phoebis_alerts::{Kind, LifecycleEvent, ScopeType, Signal};
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
    let any_event = open.iter().find(|e| e.rule_id == "any").unwrap();
    assert!(
        rig.store
            .alert_deliveries(&any_event.id)
            .unwrap()
            .iter()
            .all(|d| d.detail_json.is_none()),
        "a stateful delivery reads its body off the event; the row itself carries no copy of the detail"
    );
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
    let events = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        let rows = rig.db.events(0, i64::MAX, 100).await.unwrap();
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
    // The rule this test upserts reaches the ledger's mirror as a registry_changed row of its own, polled on
    // the rig's own tick, so the wait names the row it is actually waiting for.
    let events = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        let rows = rig.db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "alert_opened")
            .then_some(rows)
    })
    .await;
    assert_eq!(
        events.iter().filter(|e| e.kind == "alert_opened").count(),
        1
    );
}

#[tokio::test]
async fn a_lifecycle_event_posts_one_delivery_per_matching_rule_opens_nothing_and_logs_lifecycle() {
    let rig = rig().await;
    for (id, scope, sid) in [
        ("all", ScopeType::All, None),
        ("desktop", ScopeType::Key, Some("desktop")),
        ("laptop", ScopeType::Key, Some("laptop")),
    ] {
        rig.store
            .upsert_alert_rule(&rule(id, Kind::SessionActive, scope, sid), None, "test")
            .unwrap();
    }
    rig.store
        .upsert_alert_rule(
            &rule("off", Kind::SessionActive, ScopeType::All, None).with_enabled(false),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let signal = Signal {
        detail: serde_json::json!({ "key": "desktop", "pool": "main", "session": "s1", "source": "request" }),
        ..signal(
            Kind::SessionActive,
            None,
            Some("desktop"),
            Some("main"),
            "desktop, session s1",
        )
    };
    assert!(rig
        .alerts
        .deliver_event(&signal, LifecycleEvent::SessionStarted));
    assert!(
        rig.alerts
            .deliver_event(&signal, LifecycleEvent::SessionStarted),
        "a repeat posts again"
    );
    settled(&rig).await;
    let posts = rig.webhook.posts();
    assert_eq!(
        posts.len(),
        4,
        "two rules match, twice; the laptop key's rule and the disabled one never"
    );
    let mut rules: Vec<&str> = posts
        .iter()
        .map(|p| p.body["rule"].as_str().unwrap())
        .collect();
    rules.sort_unstable();
    assert_eq!(rules, ["all", "all", "desktop", "desktop"]);
    assert_eq!(
        posts[0]
            .body
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
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
    assert_eq!(
        (
            posts[0].body["state"].as_str(),
            posts[0].body["opened_at"].as_i64(),
            &posts[0].body["resolved_at"]
        ),
        (Some("event"), Some(NOW), &serde_json::Value::Null)
    );
    assert_eq!(posts[0].body["detail"]["event"], "session_started");
    assert_eq!(posts[0].body["detail"]["session"], "s1");
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "nothing opens"
    );
    let (deliveries, _) = rig.store.lifecycle_deliveries(None, None, 10).unwrap();
    assert_eq!(deliveries.len(), 4);
    assert!(deliveries.iter().all(|d| d.event_id.is_none()
        && d.event.as_deref() == Some("session_started")
        && d.kind == "session_active"
        && d.subject == "desktop, session s1"
        && d.state == "event"));
    let events = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        let rows = rig.db.events(0, i64::MAX, 100).await.unwrap();
        (rows.iter().filter(|e| e.kind == "lifecycle").count() == 4).then_some(rows)
    })
    .await;
    let row = &events[0];
    assert_eq!(
        (row.key_id.as_deref(), row.session_id.as_deref()),
        (Some("desktop"), Some("s1"))
    );
    let detail: serde_json::Value = serde_json::from_str(row.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        (detail["event"].as_str(), detail["subject"].as_str()),
        (Some("session_started"), Some("desktop, session s1"))
    );
    assert!(events
        .iter()
        .all(|e| e.kind != "alert_opened" && e.kind != "alert_resolved"));
    assert_eq!(rig.alerts.stats().open, 0);
}

#[tokio::test]
async fn a_burst_of_lifecycle_events_on_a_held_webhook_is_delivered_in_full_and_in_order() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule(
                "parked",
                Kind::SessionParked,
                ScopeType::Key,
                Some("desktop"),
            ),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    // The first delivery hangs in the receiver; the rest wait on the rule's subject queue, which holds
    // however many a park and resume cycle produces while the receiver is slow.
    let hold = rig.webhook.hold_posts();
    let signal = signal(
        Kind::SessionParked,
        None,
        Some("desktop"),
        Some("main"),
        "desktop, session s1",
    );
    let expected: Vec<&str> = (0..20)
        .map(|i| {
            if i % 2 == 0 {
                "session_parked"
            } else {
                "session_resumed"
            }
        })
        .collect();
    for i in 0..20 {
        let event = if i % 2 == 0 {
            LifecycleEvent::SessionParked
        } else {
            LifecycleEvent::SessionResumed
        };
        assert!(rig.alerts.deliver_event(&signal, event));
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        rig.webhook.posts().len(),
        1,
        "the first delivery is in flight and held"
    );
    rig.webhook.release_posts(&hold);
    settled(&rig).await;
    let posted: Vec<String> = rig
        .webhook
        .posts()
        .iter()
        .map(|p| p.body["detail"]["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        posted, expected,
        "twenty deliveries, none dropped, in the order posted"
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
    assert_eq!(Kind::ALL.len(), 13);
    assert_eq!(
        Kind::parse("keepalive_cost_cap"),
        Some(Kind::KeepaliveCostCap)
    );
    assert_eq!(Kind::parse("poll_backoff"), Some(Kind::PollBackoff));
    assert_eq!(
        allowed_scopes(Kind::PollBackoff),
        &[ScopeType::Account, ScopeType::AnyAccount],
        "a backoff is an account's, so a pool or key scope has nothing to match"
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

#[tokio::test]
async fn a_resolve_past_the_questions_expiry_posts_no_question_text() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("question", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let expires_at = NOW + 5_000;
    rig.alerts.signal(&Signal {
        kind: Kind::AskUserQuestion,
        account_id: None,
        key_id: None,
        pool_id: None,
        subject: "s-1".into(),
        detail: serde_json::json!({ "question": "which file?", "question_expires_at": expires_at }),
    });
    settled(&rig).await;
    let opened = rig.webhook.posts();
    assert_eq!(
        opened[0].body["detail"]["question"], "which file?",
        "the open carried the text in time"
    );
    let event = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    rig.clock
        .store(expires_at + 1_000, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.alerts.resolve(&event.id).unwrap());
    settled(&rig).await;
    let resolve = rig.webhook.posts().last().unwrap().body.clone();
    assert_eq!(
        resolve["detail"]["question"],
        serde_json::Value::Null,
        "{resolve}"
    );
}

#[tokio::test]
async fn a_retry_that_crosses_the_expiry_posts_no_question_text_either() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};

    let expires_at = NOW + 5_000;
    // The retry's own clock read has no seam of its own; the hook jumps time forward the moment it sees the
    // first attempt's post land, which is the one thing that tells the retry apart from the first try.
    let clock_slot: Arc<OnceLock<Arc<std::sync::atomic::AtomicI64>>> = Arc::new(OnceLock::new());
    let webhook_slot: Arc<OnceLock<Arc<FakeWebhook>>> = Arc::new(OnceLock::new());
    let jumped = Arc::new(AtomicBool::new(false));
    let hook: ClockHook = {
        let clock_slot = clock_slot.clone();
        let webhook_slot = webhook_slot.clone();
        let jumped = jumped.clone();
        Arc::new(move || {
            let (Some(clock), Some(webhook)) = (clock_slot.get(), webhook_slot.get()) else {
                return;
            };
            if !jumped.load(Ordering::SeqCst) && !webhook.posts().is_empty() {
                jumped.store(true, Ordering::SeqCst);
                clock.store(expires_at + 1_000, Ordering::SeqCst);
            }
        })
    };
    let rig = rig_with_hook(
        Arc::new(phoebis_store::Store::open_in_memory(&phoebis_store::StorageKey::generate()).unwrap()),
        hook,
    )
    .await;
    clock_slot.set(rig.clock.clone()).ok();
    webhook_slot.set(rig.webhook.clone()).ok();
    rig.store
        .upsert_alert_rule(
            &rule("question", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    rig.webhook.answer(Err("connect timeout".into()));
    rig.webhook.answer(Ok(200));
    rig.alerts.signal(&Signal {
        kind: Kind::AskUserQuestion,
        account_id: None,
        key_id: None,
        pool_id: None,
        subject: "s-1".into(),
        detail: serde_json::json!({ "question": "which file?", "question_expires_at": expires_at }),
    });
    settled(&rig).await;
    let posts = rig.webhook.posts();
    assert_eq!(posts.len(), 2, "one failed attempt, one retry");
    assert_eq!(
        posts[0].body["detail"]["question"], "which file?",
        "the first attempt was still inside the deadline"
    );
    assert_eq!(
        posts[1].body["detail"]["question"],
        serde_json::Value::Null,
        "the retry crossed the deadline"
    );
}
