mod common;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use phoebis_alerts::{question_subject, BusArm, Kind, Poller, ScopeType, Tracked};
use phoebis_bus::{Event, Kind as RequestKind, Origin, Outcome, RequestObserved, ResponseChunk};
use phoebis_routing::{Report, UtilizationSample};
use phoebis_store::{AccountState, AlertEventRow};
use common::*;

/// A `RequestObserved` of another origin or kind on the session, which must not count as the user's answer.
fn observed_as(id: &str, session: &str, origin: Origin, kind: RequestKind) -> Event {
    match observed(id, "desktop", "main", session, 0) {
        Event::RequestObserved(o) => Event::RequestObserved(RequestObserved { origin, kind, ..o }),
        other => other,
    }
}

fn chunk(id: &str, body: &'static [u8]) -> Event {
    Event::ResponseChunk(ResponseChunk {
        request_id: id.into(),
        attempt: 1,
        index: 0,
        body: bytes::Bytes::from_static(body),
        phase: None,
    })
}

#[tokio::test]
async fn the_bus_arm_opens_pool_exhausted_client_drift_and_ask_user_question_and_clears_them() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("pool", Kind::PoolExhausted, ScopeType::Pool, Some("main")),
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &rule("drift", Kind::ClientDrift, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &rule(
                "ask",
                Kind::AskUserQuestion,
                ScopeType::Key,
                Some("desktop"),
            ),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("r1", "desktop", "main", "s1", 0));
    arm.observe(complete("r1", Outcome::Exhausted, 429, None, None));
    arm.observe(observed("r2", "desktop", "main", "s1", 0));
    arm.observe(complete("r2", Outcome::Exhausted, 429, None, None));
    settled(&rig).await;
    let open = rig.store.open_alert_events().unwrap();
    assert_eq!(
        open.iter()
            .map(|e| (e.kind.as_str(), e.subject.as_str()))
            .collect::<Vec<_>>(),
        [("pool_exhausted", "main")],
        "one event however many exhaustions"
    );
    arm.observe(observed("r3", "desktop", "main", "s1", 0));
    arm.observe(complete("r3", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "a success in the pool resolves it"
    );
    arm.observe(observed("r4", "desktop", "main", "s2", 0));
    arm.observe(complete(
        "r4",
        Outcome::Rejected,
        400,
        Some("hash"),
        Some("2.1.274"),
    ));
    settled(&rig).await;
    let drift = &rig.store.open_alert_events().unwrap()[0];
    assert_eq!(
        (drift.kind.as_str(), drift.subject.as_str()),
        ("client_drift", "client 2.1.274")
    );
    let detail: serde_json::Value =
        serde_json::from_str(drift.detail_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (
            detail["trigger"].as_str(),
            detail["client_version"].as_str(),
            detail["key"].as_str()
        ),
        (Some("hash"), Some("2.1.274"), Some("desktop"))
    );
    assert!(
        rig.alerts.resolve(&drift.id).unwrap(),
        "client drift resolves by hand"
    );
    settled(&rig).await;
    assert!(rig.store.open_alert_events().unwrap().is_empty());
    // A question in the stream, its frame split across two transport pieces: the event carries key, session,
    // agent, and no text since nothing was captured.
    arm.observe(observed("r5", "desktop", "main", "s3", 0));
    arm.observe(chunk("r5", b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"na"));
    arm.observe(chunk("r5", b"me\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
    arm.observe(complete("r5", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    let ask = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "ask_user_question")
        .unwrap();
    assert_eq!(ask.subject, "desktop, session s3");
    let detail: serde_json::Value =
        serde_json::from_str(ask.detail_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (
            detail["key"].as_str(),
            detail["session"].as_str(),
            detail["agent"].as_str(),
            detail["question"].is_null()
        ),
        (Some("desktop"), Some("s3"), Some("main"), true)
    );
    // Neither a keepalive probe nor a token count on the session is the user's answer.
    arm.observe(observed_as(
        "p1",
        "s3",
        Origin::Keepalive,
        RequestKind::Probe,
    ));
    arm.observe(observed_as(
        "c1",
        "s3",
        Origin::Client,
        RequestKind::CountTokens,
    ));
    settled(&rig).await;
    assert_eq!(
        rig.store.open_alert_events().unwrap().len(),
        1,
        "a probe and a count_tokens leave the question open"
    );
    arm.observe(observed("r6", "desktop", "main", "s3", 0));
    settled(&rig).await;
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "the session's next client inference resolves it"
    );
    // A buffered response is scanned as one document.
    arm.observe(match observed("r8", "desktop", "main", "s5", 0) {
        Event::RequestObserved(o) => Event::RequestObserved(RequestObserved { stream: false, ..o }),
        other => other,
    });
    arm.observe(chunk(
        "r8",
        br#"{"id":"msg","content":[{"type":"tool_use","name":"AskUserQuestion","input":{}}]}"#,
    ));
    arm.observe(complete("r8", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    assert!(
        rig.store
            .open_alert_events()
            .unwrap()
            .iter()
            .any(|e| e.subject == "desktop, session s5"),
        "a buffered tool_use block opens the question too"
    );
    let events = fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        let rows = rig.db.events(0, i64::MAX, 100).await.unwrap();
        (rows.iter().filter(|e| e.kind == "alert_resolved").count() == 3).then_some(rows)
    })
    .await;
    assert!(events.iter().any(|e| e.kind == "alert_opened"
        && e.session_id.as_deref() == Some("s3")
        && e.key_id.as_deref() == Some("desktop")));
}

#[tokio::test]
async fn a_captured_body_gives_the_question_its_text() {
    use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("r7", "desktop", "main", "s4", 24));
    arm.observe(chunk("r7", b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
    arm.observe(complete("r7", Outcome::Succeeded, 200, None, None));
    // The ledger lands the body a moment later, as it does in production.
    rig.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r7".into(),
            received_at: NOW,
            finished_at: Some(NOW + 100),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s4".into()),
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
        attempts: vec![AttemptRecord {
            request_id: "r7".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: NOW,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        bodies: Some(BodyRecord {
            request_id: "r7".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(
                br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[{"question":"Ship it?"}]}}]}"#
                    .to_vec(),
            ),
            request_len_seen: b"{}".len() as u64,
            response_len_seen: br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[{"question":"Ship it?"}]}}]}"#.len() as u64,
            expires_at: NOW + 86_400_000,
        }),
    });
    let ask = fake_upstream::wait_for(std::time::Duration::from_secs(10), || {
        rig.store
            .open_alert_events()
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "ask_user_question")
    })
    .await;
    let detail: serde_json::Value =
        serde_json::from_str(ask.detail_json.as_deref().unwrap()).unwrap();
    assert_eq!(detail["question"].as_str(), Some("Ship it?"));
    assert_eq!(
        detail["question_expires_at"].as_i64(),
        Some(NOW + 86_400_000),
        "the text is held to the deadline of the body it was read out of"
    );
}

#[tokio::test]
async fn a_question_answered_while_its_body_was_being_read_opens_nothing() {
    use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("r9", "desktop", "main", "s6", 24));
    arm.observe(chunk("r9", b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
    arm.observe(complete("r9", Outcome::Succeeded, 200, None, None));
    // The body is not in the ledger yet, so the enrichment worker is polling; the user's next turn arrives first.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    arm.observe(observed("r10", "desktop", "main", "s6", 24));
    // Now the body lands and the worker's next poll reads it.
    rig.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r9".into(),
            received_at: NOW,
            finished_at: Some(NOW + 100),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s6".into()),
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
        attempts: vec![AttemptRecord {
            request_id: "r9".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: NOW,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        bodies: Some(BodyRecord {
            request_id: "r9".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(
                br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Late?"}}]}"#
                    .to_vec(),
            ),
            request_len_seen: b"{}".len() as u64,
            response_len_seen: br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Late?"}}]}"#.len() as u64,
            expires_at: NOW + 86_400_000,
        }),
    });
    fake_upstream::wait_for_async(std::time::Duration::from_secs(5), || async {
        rig.db.bodies("r9").await.unwrap().map(|_| ())
    })
    .await;
    fake_upstream::stays_for(std::time::Duration::from_secs(2), || {
        rig.store.open_alert_events().unwrap().is_empty()
    })
    .await;
    assert!(
        rig.webhook.posts().is_empty(),
        "no stale question was opened or posted"
    );
}

#[tokio::test]
async fn a_question_that_completes_after_the_next_turn_opens_nothing_without_a_capture() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("r13", "desktop", "main", "s8", 0));
    arm.observe(chunk("r13", b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
    // The user answered on the next turn while the asking response was still finishing.
    arm.observe(observed("r14", "desktop", "main", "s8", 0));
    arm.observe(complete("r13", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "the question was answered before it could open"
    );
    assert!(rig.webhook.posts().is_empty(), "and nothing was posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answering_turn_cannot_land_between_a_questions_claim_and_its_event() {
    use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
    use phoebis_store::{StorageKey, Store};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, OnceLock};

    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let held: Arc<OnceLock<Arc<Mutex<BusArm>>>> = Arc::new(OnceLock::new());
    let armed = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let turn: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let runtime = tokio::runtime::Handle::current();
    let hook: ClockHook = {
        let (held, store, armed, fired, turn) = (
            held.clone(),
            store.clone(),
            armed.clone(),
            fired.clone(),
            turn.clone(),
        );
        Arc::new(move || {
            if !armed.load(Ordering::SeqCst) {
                return;
            }
            let Some(arm) = held.get() else { return };
            let Ok(arm) = arm.try_lock() else { return };
            // The claim on the session is made and its event is not in the store yet: the interval in which
            // a turn that consumed the flag would clear a question nobody has written. The opening signal
            // reads the clock from inside it, so this fires there.
            let claimed = arm.question_claimed("desktop", "s7");
            drop(arm);
            let written = store
                .open_alert_events()
                .is_ok_and(|o| o.iter().any(|e| e.kind == Kind::AskUserQuestion.as_str()));
            if !claimed || written {
                return;
            }
            armed.store(false, Ordering::SeqCst);
            fired.store(true, Ordering::SeqCst);
            let arm = held.get().expect("the arm is built").clone();
            let runtime = runtime.clone();
            let thread = std::thread::spawn(move || {
                let _entered = runtime.enter();
                arm.lock()
                    .unwrap()
                    .observe(observed("r12", "desktop", "main", "s7", 24));
            });
            // Long enough for that turn to run to its end if nothing holds it back.
            std::thread::sleep(Duration::from_millis(200));
            *turn.lock().unwrap() = Some(thread);
        })
    };
    let rig = rig_with_hook(store, hook).await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let arm = Arc::new(Mutex::new(BusArm::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
    )));
    held.set(arm.clone()).ok();
    {
        let mut arm = arm.lock().unwrap();
        arm.observe(observed("r11", "desktop", "main", "s7", 24));
        arm.observe(chunk("r11", b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
        arm.observe(complete("r11", Outcome::Succeeded, 200, None, None));
    }
    armed.store(true, Ordering::SeqCst);
    // The body lands the way the ledger lands it, which is what releases the worker's poll.
    rig.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r11".into(),
            received_at: NOW,
            finished_at: Some(NOW + 100),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s7".into()),
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
        attempts: vec![AttemptRecord {
            request_id: "r11".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: NOW,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        bodies: Some(BodyRecord {
            request_id: "r11".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(
                br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Ship it?"}}]}"#
                    .to_vec(),
            ),
            request_len_seen: b"{}".len() as u64,
            response_len_seen: br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Ship it?"}}]}"#.len() as u64,
            expires_at: NOW + 86_400_000,
        }),
    });
    let thread =
        fake_upstream::wait_for(Duration::from_secs(10), || turn.lock().unwrap().take()).await;
    thread.join().unwrap();
    settled(&rig).await;
    assert!(
        fired.load(Ordering::SeqCst),
        "the hook never found its window, so this test proved nothing"
    );
    assert!(
        rig.store
            .open_alert_events()
            .unwrap()
            .iter()
            .all(|e| e.kind != Kind::AskUserQuestion.as_str()),
        "the answering turn resolved the question it answered"
    );
    let states: Vec<String> = rig
        .webhook
        .posts()
        .iter()
        .map(|p| p.body["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        states,
        ["opened", "resolved"],
        "no notification leaves a receiver holding a question nobody will close"
    );
}

#[tokio::test]
async fn the_poller_reads_account_state_refresh_attempts_samples_and_the_cap() {
    let rig = rig().await;
    for (id, kind, scope, sid) in [
        ("reauth", Kind::ReauthRequired, ScopeType::AnyAccount, None),
        (
            "funding",
            Kind::FundingRequired,
            ScopeType::Account,
            Some("direct"),
        ),
        ("refresh", Kind::RefreshFailed, ScopeType::AnyAccount, None),
        ("cap", Kind::KeepaliveCostCap, ScopeType::Pool, Some("main")),
        ("ask", Kind::AskUserQuestion, ScopeType::All, None),
    ] {
        rig.store
            .upsert_alert_rule(&rule(id, kind, scope, sid), None, "test")
            .unwrap();
    }
    rig.store
        .upsert_alert_rule(
            &phoebis_store::AlertRuleRow {
                threshold: Some(0.9),
                window: Some("7d".into()),
                ..rule(
                    "util",
                    Kind::UtilizationAbove,
                    ScopeType::Pool,
                    Some("main"),
                )
            },
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &phoebis_store::AlertRuleRow {
                threshold: Some(0.9),
                window: Some("7d".into()),
                ..rule(
                    "util-key",
                    Kind::UtilizationAbove,
                    ScopeType::Key,
                    Some("desktop"),
                )
            },
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &phoebis_store::AlertRuleRow {
                threshold: Some(0.9),
                window: Some("7d".into()),
                ..rule(
                    "util-other",
                    Kind::UtilizationAbove,
                    ScopeType::Key,
                    Some("nobody"),
                )
            },
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut spent = BTreeMap::new();
    spent.insert("main".to_string(), 1.5);
    let mut poller = Poller::new(rig.alerts.clone(), &inputs(&rig, spent), Tracked::default());
    rig.store
        .set_state("business", AccountState::ReauthRequired)
        .unwrap();
    rig.store
        .set_state("direct", AccountState::FundingRequired)
        .unwrap();
    let attempt = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Failed("invalid_grant".into()),
        )
        .unwrap();
    rig.router.report(Report::Sample {
        account_id: "business".into(),
        sample: UtilizationSample {
            five_hour: Some(0.5),
            five_hour_reset: None,
            seven_day: Some(0.95),
            seven_day_reset: None,
            fable: None,
            fable_reset: None,
            observed_at: NOW,
        },
        incarnation: 1,
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    poller.tick().await;
    settled(&rig).await;
    let open: Vec<(String, String)> = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .map(|e| (e.kind, e.subject))
        .collect();
    assert!(open.contains(&("reauth_required".into(), "business".into())));
    assert!(open.contains(&("funding_required".into(), "direct".into())));
    assert!(
        open.contains(&("keepalive_cost_cap".into(), "main".into())),
        "$1.50 spent against a $1.00 cap"
    );
    assert!(
        open.contains(&("utilization_above".into(), "business".into())),
        "0.95 on 7d against 0.9"
    );
    let utilization: Vec<String> = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "utilization_above")
        .map(|e| e.rule_id)
        .collect();
    assert!(
        utilization.contains(&"util".into()) && utilization.contains(&"util-key".into()),
        "the pool-scoped and the key-scoped rules both open through the poller: {utilization:?}"
    );
    assert!(
        !utilization.contains(&"util-other".into()),
        "a key scope the account does not serve stays closed"
    );
    assert!(
        !open.iter().any(|(k, _)| k == "refresh_failed"),
        "one failure is not two in a row"
    );
    let attempt = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Failed("invalid_grant".into()),
        )
        .unwrap();
    poller.tick().await;
    settled(&rig).await;
    assert!(rig
        .store
        .open_alert_events()
        .unwrap()
        .iter()
        .any(|e| e.kind == "refresh_failed" && e.subject == "business"));
    // Every condition lifts. The committed refresh lands before the second poller exists, so its constructor,
    // not its tick, is what resolves the failure.
    rig.store
        .set_state("business", AccountState::Ready)
        .unwrap();
    rig.store.set_state("direct", AccountState::Ready).unwrap();
    let attempt = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(attempt, 1, phoebis_store::AttemptOutcome::Committed)
        .unwrap();
    rig.router.report(Report::Sample {
        account_id: "business".into(),
        sample: UtilizationSample {
            five_hour: Some(0.5),
            five_hour_reset: None,
            seven_day: Some(0.86),
            seven_day_reset: None,
            fable: None,
            fable_reset: None,
            observed_at: NOW + 1,
        },
        incarnation: 1,
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        Tracked::default(),
    );
    settled(&rig).await;
    assert!(
        !rig.store
            .open_alert_events()
            .unwrap()
            .iter()
            .any(|e| e.kind == "refresh_failed"),
        "a commit that landed before the poller started still resolves the failure"
    );
    poller.tick().await;
    settled(&rig).await;
    let left: Vec<String> = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        left.contains(&"utilization_above".into()),
        "0.86 is under 0.9 but not under 0.85, so it stays"
    );
    assert!(!left.iter().any(|k| k != "utilization_above"), "{left:?}");
    rig.router.report(Report::Sample {
        account_id: "business".into(),
        sample: UtilizationSample {
            five_hour: Some(0.5),
            five_hour_reset: None,
            seven_day: Some(0.84),
            seven_day_reset: None,
            fable: None,
            fable_reset: None,
            observed_at: NOW + 2,
        },
        incarnation: 1,
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    poller.tick().await;
    settled(&rig).await;
    assert!(rig.store.open_alert_events().unwrap().is_empty());
    // An open question resolves at the session expiry.
    assert!(rig
        .store
        .insert_alert_event(&phoebis_store::AlertEventRow {
            id: "old".into(),
            rule_id: "ask".into(),
            kind: "ask_user_question".into(),
            subject: "desktop, session s9".into(),
            state: "open".into(),
            opened_at: NOW - 3_600_001,
            resolved_at: None,
            detail_json: None,
        })
        .unwrap());
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rig.store.alert_event("old").unwrap().unwrap().state,
        "resolved"
    );
}

#[tokio::test]
async fn at_start_a_refresh_failure_whose_account_is_gone_clears_and_one_still_failing_stays_open()
{
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("refresh", Kind::RefreshFailed, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let attempt = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Failed("invalid_grant".into()),
        )
        .unwrap();
    for (id, subject) in [("gone", "direct"), ("failing", "business")] {
        assert!(rig
            .store
            .insert_alert_event(&AlertEventRow {
                id: id.into(),
                rule_id: "refresh".into(),
                kind: "refresh_failed".into(),
                subject: subject.into(),
                state: "open".into(),
                opened_at: NOW - 1_000,
                resolved_at: None,
                detail_json: None,
            })
            .unwrap());
    }
    rig.store.delete_account("direct", "test").unwrap();
    let _poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        Tracked::default(),
    );
    settled(&rig).await;
    assert_eq!(
        rig.store.alert_event("gone").unwrap().unwrap().state,
        "resolved",
        "the account the failure was about is gone, so nothing is left to fail"
    );
    assert_eq!(
        rig.store.alert_event("failing").unwrap().unwrap().state,
        "open",
        "the account's last attempt failed, and nothing has committed since"
    );
}

/// A sub-agent's turn: the same key and session as its parent's, with a parent above it.
fn sub_agent(id: &str, session: &str) -> Event {
    match observed(id, "desktop", "main", session, 0) {
        Event::RequestObserved(o) => Event::RequestObserved(RequestObserved {
            agent_id: Some("sub".into()),
            parent_agent_id: Some("main".into()),
            ..o
        }),
        other => other,
    }
}

fn ask_event(id: &str, session: &str, detail: Option<&str>) -> AlertEventRow {
    AlertEventRow {
        id: id.into(),
        rule_id: "ask".into(),
        kind: "ask_user_question".into(),
        subject: question_subject("desktop", session),
        state: "open".into(),
        opened_at: NOW,
        resolved_at: None,
        detail_json: detail.map(String::from),
    }
}

#[tokio::test]
async fn a_session_is_tracked_only_for_a_client_turn_and_only_until_the_expiry() {
    let rig = rig().await;
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("r1", "desktop", "main", "s1", 0));
    assert_eq!(arm.tracked_sessions(), 1);
    arm.observe(observed_as(
        "p1",
        "s2",
        Origin::Keepalive,
        RequestKind::Probe,
    ));
    arm.observe(observed_as(
        "c1",
        "s2",
        Origin::Client,
        RequestKind::CountTokens,
    ));
    assert_eq!(
        arm.tracked_sessions(),
        1,
        "a probe and a count_tokens track no session of their own"
    );
    rig.clock.store(NOW + 3_600_001, Ordering::SeqCst);
    arm.observe(observed("r2", "desktop", "main", "s3", 0));
    assert_eq!(
        arm.tracked_sessions(),
        1,
        "the session past the expiry is swept, leaving the one just seen"
    );
}

#[tokio::test]
async fn an_event_left_open_for_a_healthy_account_resolves_on_the_next_tick() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("reauth", Kind::ReauthRequired, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        Tracked::default(),
    );
    poller.tick().await;
    settled(&rig).await;
    // What a clear that failed against the store leaves behind: the account is healthy and the event is open.
    rig.store
        .insert_alert_event(&AlertEventRow {
            id: "left-open".into(),
            rule_id: "reauth".into(),
            kind: "reauth_required".into(),
            subject: "business".into(),
            state: "open".into(),
            opened_at: NOW,
            resolved_at: None,
            detail_json: None,
        })
        .unwrap();
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rig.store.alert_event("left-open").unwrap().unwrap().state,
        "resolved",
        "the clear is level-triggered, so a later tick still catches it"
    );
}

#[tokio::test]
async fn a_tick_with_no_open_event_issues_no_clear_and_one_with_open_events_clears_only_those() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("cap", Kind::KeepaliveCostCap, ScopeType::Pool, Some("main")),
            None,
            "test",
        )
        .unwrap();
    rig.store
        .upsert_alert_rule(
            &phoebis_store::AlertRuleRow {
                threshold: Some(0.9),
                window: Some("7d".into()),
                ..rule("util", Kind::UtilizationAbove, ScopeType::AnyAccount, None)
            },
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    rig.router.report(Report::Sample {
        account_id: "business".into(),
        sample: UtilizationSample {
            five_hour: Some(0.5),
            five_hour_reset: None,
            seven_day: Some(0.5),
            seven_day_reset: None,
            fable: None,
            fable_reset: None,
            observed_at: NOW,
        },
        incarnation: 1,
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut spent = BTreeMap::new();
    spent.insert("main".to_string(), 0.1);
    let mut poller = Poller::new(rig.alerts.clone(), &inputs(&rig, spent), Tracked::default());
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rig.alerts.clears(),
        0,
        "under the cap and under the threshold with nothing open, the tick has nothing to clear"
    );
    for (id, rule_id, kind, subject) in [
        ("cap-open", "cap", "keepalive_cost_cap", "main"),
        ("util-open", "util", "utilization_above", "business"),
    ] {
        assert!(rig
            .store
            .insert_alert_event(&AlertEventRow {
                id: id.into(),
                rule_id: rule_id.into(),
                kind: kind.into(),
                subject: subject.into(),
                state: "open".into(),
                opened_at: NOW,
                resolved_at: None,
                detail_json: None,
            })
            .unwrap());
    }
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rig.alerts.clears(),
        2,
        "one clear per open event, none for the pairs with nothing open"
    );
    for id in ["cap-open", "util-open"] {
        assert_eq!(
            rig.store.alert_event(id).unwrap().unwrap().state,
            "resolved"
        );
    }
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rig.alerts.clears(),
        2,
        "resolved rows are not open, so the next tick clears nothing"
    );
}

#[tokio::test]
async fn a_refresh_failed_delivery_names_whether_the_attempt_failed_or_went_uncertain() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("refresh", Kind::RefreshFailed, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        Tracked::default(),
    );
    for outcome in [
        phoebis_store::AttemptOutcome::Failed("invalid_grant".into()),
        phoebis_store::AttemptOutcome::Uncertain("refresh response lost".into()),
    ] {
        let attempt = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
        rig.store
            .resolve_refresh_attempt(attempt, 1, outcome)
            .unwrap();
        poller.tick().await;
    }
    settled(&rig).await;
    let event = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "refresh_failed")
        .expect("two in a row, the second uncertain, open the event");
    let detail: serde_json::Value =
        serde_json::from_str(event.detail_json.as_deref().unwrap()).unwrap();
    assert_eq!(detail["outcome"], "uncertain", "{detail}");
    assert_eq!(detail["failures"], 2);
    assert_eq!(detail["last_error"], "refresh response lost");
    let posted = rig.webhook.posts();
    assert_eq!(
        posted[0].body["detail"]["outcome"], "uncertain",
        "the delivery carries the outcome"
    );
}

/// The refused half of the same rule: two failures the token endpoint answered, so the operator reads that the
/// credential was refused rather than that its answer was lost.
#[tokio::test]
async fn two_failed_refreshes_name_the_failed_outcome_on_the_event_and_the_delivery() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("refresh", Kind::RefreshFailed, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        Tracked::default(),
    );
    for _ in 0..2 {
        let attempt = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
        rig.store
            .resolve_refresh_attempt(
                attempt,
                1,
                phoebis_store::AttemptOutcome::Failed("invalid_grant".into()),
            )
            .unwrap();
        poller.tick().await;
    }
    settled(&rig).await;
    let event = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "refresh_failed")
        .expect("two in a row open the event");
    let detail: serde_json::Value =
        serde_json::from_str(event.detail_json.as_deref().unwrap()).unwrap();
    assert_eq!(detail["outcome"], "failed", "{detail}");
    assert_eq!(detail["failures"], 2);
    assert_eq!(detail["last_error"], "invalid_grant");
    let posted = rig.webhook.posts();
    assert_eq!(
        posted[0].body["detail"]["outcome"], "failed",
        "the delivery carries the outcome"
    );
}

#[tokio::test]
async fn a_question_open_before_the_arm_existed_resolves_on_the_next_client_turn() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    // One event with the detail the arm writes and one with none, so both ways of naming the parties are read.
    assert!(rig
        .store
        .insert_alert_event(&ask_event(
            "detailed",
            "s1",
            Some(r#"{"agent":"main","key":"desktop","question":null,"session":"s1"}"#)
        ))
        .unwrap());
    assert!(rig
        .store
        .insert_alert_event(&ask_event("bare", "s2", None))
        .unwrap());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("r1", "desktop", "main", "s1", 0));
    arm.observe(observed("r2", "desktop", "main", "s2", 0));
    settled(&rig).await;
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "a restarted arm still knows both sessions hold a question"
    );
}

#[tokio::test]
async fn a_sub_agent_turn_neither_answers_a_question_nor_invalidates_one_being_read() {
    use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("q1", "desktop", "main", "s7", 24));
    arm.observe(chunk("q1", b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
    arm.observe(complete("q1", Outcome::Succeeded, 200, None, None));
    // The sub-agent's turn arrives while the enrichment worker is still polling for the body.
    tokio::time::sleep(Duration::from_millis(100)).await;
    arm.observe(sub_agent("sub1", "s7"));
    rig.ledger.record(Record::Request {
        request: RequestRecord {
            id: "q1".into(),
            received_at: NOW,
            finished_at: Some(NOW + 100),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s7".into()),
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
        attempts: vec![AttemptRecord {
            request_id: "q1".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: NOW,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        bodies: Some(BodyRecord {
            request_id: "q1".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(
                br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Which one?"}}]}"#
                    .to_vec(),
            ),
            request_len_seen: b"{}".len() as u64,
            response_len_seen: br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Which one?"}}]}"#.len() as u64,
            expires_at: NOW + 86_400_000,
        }),
    });
    let ask = fake_upstream::wait_for(Duration::from_secs(10), || {
        rig.store
            .open_alert_events()
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "ask_user_question")
    })
    .await;
    assert_eq!(ask.subject, question_subject("desktop", "s7"));
    // A second sub-agent turn is not the answer the event waits for either.
    arm.observe(sub_agent("sub2", "s7"));
    fake_upstream::stays_for(Duration::from_secs(1), || {
        !rig.store.open_alert_events().unwrap().is_empty()
    })
    .await;
    arm.observe(observed("q2", "desktop", "main", "s7", 0));
    settled(&rig).await;
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "the top-level turn is what answers it"
    );
}

#[tokio::test]
async fn a_stream_still_sending_chunks_keeps_its_entry_past_the_idle_window() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("pool", Kind::PoolExhausted, ScopeType::Pool, Some("main")),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("long", "desktop", "main", "s1", 0));
    for minute in [5, 10, 15, 20] {
        rig.clock.store(NOW + minute * 60_000, Ordering::SeqCst);
        arm.observe(chunk("long", b"data: {}\n\n"));
    }
    arm.observe(complete("long", Outcome::Exhausted, 429, None, None));
    settled(&rig).await;
    assert_eq!(
        rig.store
            .open_alert_events()
            .unwrap()
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        ["pool_exhausted"],
        "twenty minutes of chunks is not an idle request"
    );
}

#[tokio::test]
async fn a_question_signalled_long_after_admission_is_tracked_from_the_signal() {
    use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::All, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(observed("q1", "desktop", "main", "s1", 24));
    arm.observe(chunk("q1", b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"AskUserQuestion\",\"input\":{}}}\n\n"));
    // The answer streams for half an hour, so the event opens that much later than the turn that asked for it.
    for minute in [10, 20, 30] {
        rig.clock.store(NOW + minute * 60_000, Ordering::SeqCst);
        arm.observe(chunk("q1", b"data: {}\n\n"));
    }
    arm.observe(complete("q1", Outcome::Succeeded, 200, None, None));
    rig.ledger.record(Record::Request {
        request: RequestRecord {
            id: "q1".into(),
            received_at: NOW,
            finished_at: Some(NOW + 30 * 60_000),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
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
        attempts: vec![AttemptRecord {
            request_id: "q1".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: NOW,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        bodies: Some(BodyRecord {
            request_id: "q1".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(
                br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Still there?"}}]}"#
                    .to_vec(),
            ),
            request_len_seen: b"{}".len() as u64,
            response_len_seen: br#"{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"question":"Still there?"}}]}"#.len() as u64,
            expires_at: NOW + 86_400_000,
        }),
    });
    let ask = fake_upstream::wait_for(Duration::from_secs(10), || {
        rig.store
            .open_alert_events()
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "ask_user_question")
    })
    .await;
    assert_eq!(ask.opened_at, NOW + 30 * 60_000);
    // Just under the expiry measured from the signal, which is an hour and a half past the admission.
    rig.clock.store(NOW + 89 * 60_000, Ordering::SeqCst);
    arm.observe(observed_as(
        "p1",
        "s2",
        Origin::Keepalive,
        RequestKind::Probe,
    ));
    assert_eq!(
        arm.tracked_sessions(),
        1,
        "the entry outlives the event it tracks"
    );
    arm.observe(observed("q2", "desktop", "main", "s1", 0));
    settled(&rig).await;
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "the next turn still answers the question"
    );
}
