mod common;
use std::collections::BTreeMap;

use phoebis_alerts::{BusArm, Kind, Poller, ScopeType};
use phoebis_bus::{Event, Kind as RequestKind, Origin, Outcome, RequestObserved, ResponseChunk};
use phoebis_routing::{Report, UtilizationSample};
use phoebis_store::AccountState;
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
    let mut arm = BusArm::new(rig.alerts.clone());
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
    let events = fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        let rows = rig.db.events(0, i64::MAX, 100).unwrap();
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
    let mut arm = BusArm::new(rig.alerts.clone());
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
    let mut arm = BusArm::new(rig.alerts.clone());
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
            expires_at: NOW + 86_400_000,
        }),
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        rig.db.bodies("r9").unwrap().map(|_| ())
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
    let mut poller = Poller::new(rig.alerts.clone(), &inputs(&rig, spent));
    rig.store
        .set_state("business", AccountState::ReauthRequired)
        .unwrap();
    rig.store
        .set_state("direct", AccountState::FundingRequired)
        .unwrap();
    let attempt = rig.store.open_refresh_attempt("business", 1, 1).unwrap();
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
    let attempt = rig.store.open_refresh_attempt("business", 1, 1).unwrap();
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
    let attempt = rig.store.open_refresh_attempt("business", 1, 1).unwrap();
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
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut poller = Poller::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
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
