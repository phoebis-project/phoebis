//! Alert rules seal their webhook secret at rest, and an event opens at most once per rule and subject
//! while it stays open.

use phoebis_store::{
    AlertDeliveryRow, AlertEventRow, AlertRuleRow, Secret, StorageKey, Store, StoreError,
};

fn rule(id: &str) -> AlertRuleRow {
    AlertRuleRow {
        id: id.into(),
        kind: "reauth_required".into(),
        scope_type: "any_account".into(),
        scope_id: None,
        threshold: None,
        window: None,
        webhook_url: "http://127.0.0.1:1/hook".into(),
        has_secret: false,
        enabled: true,
        created_at: 1_000,
        updated_at: 1_000,
    }
}

#[test]
fn rules_are_stored_with_a_sealed_secret_that_only_has_secret_reveals() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    assert_eq!(store.schema_version().unwrap(), 20);
    store
        .upsert_alert_rule(&rule("r1"), Some(Some(&Secret::new("hunter2"))), "admin")
        .unwrap();
    let stored = store.alert_rule("r1").unwrap().unwrap();
    assert!(stored.has_secret);
    assert_eq!(
        store.alert_rule_secret("r1").unwrap().unwrap().expose(),
        "hunter2"
    );
    assert!(
        !store
            .debug_dump_table("alert_rules")
            .unwrap()
            .iter()
            .any(|blob| blob.windows(7).any(|w| w == b"hunter2")),
        "the secret is sealed at rest"
    );
    store
        .upsert_alert_rule(
            &AlertRuleRow {
                webhook_url: "http://127.0.0.1:1/other".into(),
                ..rule("r1")
            },
            None,
            "admin",
        )
        .unwrap();
    assert_eq!(
        store.alert_rule_secret("r1").unwrap().unwrap().expose(),
        "hunter2",
        "an update without a secret keeps the stored one"
    );
    store
        .upsert_alert_rule(&rule("r1"), Some(None), "admin")
        .unwrap();
    assert!(
        store.alert_rule_secret("r1").unwrap().is_none()
            && !store.alert_rule("r1").unwrap().unwrap().has_secret
    );
    store
        .upsert_alert_rule(
            &AlertRuleRow {
                webhook_url: "http://127.0.0.1:1/a".into(),
                ..rule("r2")
            },
            None,
            "admin",
        )
        .unwrap();
    assert_eq!(
        store
            .alert_rules()
            .unwrap()
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["r2", "r1"],
        "sorted by webhook url then id"
    );
    let kinds: Vec<String> = store
        .registry_changes_after(0, 10)
        .unwrap()
        .into_iter()
        .map(|c| c.kind)
        .collect();
    assert!(
        kinds.contains(&"alert_rule_created".into())
            && kinds.contains(&"alert_rule_updated".into())
    );
    assert!(!store
        .registry_changes_after(0, 10)
        .unwrap()
        .iter()
        .any(|c| c.diff_json.contains("hunter2")));
    assert!(matches!(
        store.delete_alert_rule("nope", "admin"),
        Err(StoreError::AlertRuleNotFound(_))
    ));
}

#[test]
fn events_open_once_per_rule_and_subject_resolve_and_cascade_with_their_deliveries() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.upsert_alert_rule(&rule("r1"), None, "admin").unwrap();
    let event = AlertEventRow {
        id: "e1".into(),
        rule_id: "r1".into(),
        kind: "reauth_required".into(),
        subject: "business".into(),
        state: "open".into(),
        opened_at: 2_000,
        resolved_at: None,
        detail_json: None,
    };
    assert!(store.insert_alert_event(&event).unwrap());
    assert!(
        !store
            .insert_alert_event(&AlertEventRow {
                id: "e1-again".into(),
                ..event.clone()
            })
            .unwrap(),
        "a second open row for the rule and subject is refused by the unique open index"
    );
    assert!(store.alert_event("e1-again").unwrap().is_none());
    assert!(
        store
            .insert_alert_event(&AlertEventRow {
                subject: "mail".into(),
                ..event.clone()
            })
            .is_err(),
        "a reused id is an error, not the `false` that says someone else holds the subject"
    );
    assert!(
        store
            .insert_alert_event(&AlertEventRow {
                id: "e-bogus".into(),
                subject: "bogus".into(),
                state: "bogus".into(),
                ..event.clone()
            })
            .is_err(),
        "a state outside the two the check names would escape the open index and the purge"
    );
    assert_eq!(
        store
            .open_alert_event("r1", "business")
            .unwrap()
            .unwrap()
            .id,
        "e1"
    );
    assert!(store.open_alert_event("r1", "mail").unwrap().is_none());
    let id = store
        .insert_alert_delivery(&AlertDeliveryRow {
            id: 0,
            event_id: Some("e1".into()),
            rule_id: "r1".into(),
            kind: "reauth_required".into(),
            subject: "business".into(),
            event: None,
            detail_json: None,
            state: "opened".into(),
            attempt: 1,
            at: 2_001,
            status: Some(500),
            error: None,
        })
        .unwrap();
    store
        .insert_alert_delivery(&AlertDeliveryRow {
            id: 0,
            event_id: Some("e1".into()),
            rule_id: "r1".into(),
            kind: "reauth_required".into(),
            subject: "business".into(),
            event: None,
            detail_json: None,
            state: "opened".into(),
            attempt: 2,
            at: 3_001,
            status: Some(200),
            error: None,
        })
        .unwrap();
    let deliveries = store.alert_deliveries("e1").unwrap();
    assert_eq!(
        (
            deliveries.len(),
            deliveries[0].id,
            deliveries[0].status,
            deliveries[1].attempt
        ),
        (2, id, Some(500), 2)
    );
    assert!(store.resolve_alert_event("e1", 4_000).unwrap());
    assert!(
        !store.resolve_alert_event("e1", 4_001).unwrap(),
        "resolving twice is a no-op"
    );
    assert_eq!(
        store.alert_event("e1").unwrap().unwrap().resolved_at,
        Some(4_000)
    );
    assert!(store.open_alert_events().unwrap().is_empty());
    assert_eq!(store.alert_events(Some("resolved"), 10).unwrap().len(), 1);
    assert_eq!(store.alert_events(Some("open"), 10).unwrap().len(), 0);
    assert!(
        store
            .insert_alert_event(&AlertEventRow {
                id: "e2".into(),
                opened_at: 5_000,
                ..event.clone()
            })
            .unwrap(),
        "the subject reopens once its earlier event is resolved"
    );
    assert_eq!(
        store.alert_events(None, 10).unwrap()[0].id,
        "e2",
        "newest first"
    );
    assert_eq!(
        store.purge_alert_events_before(4_500, 0, 0).unwrap(),
        1,
        "the resolved one is purged, the open one stays"
    );
    assert!(
        store.alert_deliveries("e1").unwrap().is_empty(),
        "the purge took the resolved event's deliveries with it"
    );
    // The open event needs a delivery of its own, or the rule delete has no delivery row left to cascade
    // through and the assertion below would pass on the purge's work.
    store
        .insert_alert_delivery(&AlertDeliveryRow {
            id: 0,
            event_id: Some("e2".into()),
            rule_id: "r1".into(),
            kind: "reauth_required".into(),
            subject: "business".into(),
            event: None,
            detail_json: None,
            state: "opened".into(),
            attempt: 1,
            at: 5_001,
            status: Some(200),
            error: None,
        })
        .unwrap();
    assert_eq!(store.alert_deliveries("e2").unwrap().len(), 1);
    store.delete_alert_rule("r1", "admin").unwrap();
    assert!(
        store.alert_event("e2").unwrap().is_none()
            && store.alert_deliveries("e2").unwrap().is_empty(),
        "deleting the rule takes its open event and that event's deliveries"
    );
}

#[test]
fn a_sealed_secret_copied_onto_another_rule_does_not_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    let store = Store::open(&path, &key).unwrap();
    store
        .upsert_alert_rule(&rule("r1"), Some(Some(&Secret::new("hunter2"))), "admin")
        .unwrap();
    store.upsert_alert_rule(&rule("r2"), None, "admin").unwrap();
    drop(store);
    // Write access to the file is not access to the secret: the sealed bytes move onto a rule whose webhook
    // url the mover controls, which is the attack the aad exists to stop.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "UPDATE alert_rules SET \
           secret_sealed = (SELECT secret_sealed FROM alert_rules WHERE id = 'r1'), \
           secret_nonce = (SELECT secret_nonce FROM alert_rules WHERE id = 'r1') \
         WHERE id = 'r2';",
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(
        store.alert_rule_secret("r1").unwrap().unwrap().expose(),
        "hunter2",
        "the rule it was sealed for still opens it"
    );
    assert!(
        matches!(store.alert_rule_secret("r2"), Err(StoreError::Crypto)),
        "the aad names the rule, so the same blob does not open on another one"
    );
}

#[test]
fn a_rule_change_records_the_webhook_host_and_never_the_token_in_its_path() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .upsert_alert_rule(
            &AlertRuleRow {
                webhook_url: "https://hooks.example:8443/services/T0/B0/s3cret-token?key=s3cret"
                    .into(),
                ..rule("r1")
            },
            None,
            "admin",
        )
        .unwrap();
    let change = store.registry_changes_after(0, 10).unwrap().pop().unwrap();
    let diff: serde_json::Value = serde_json::from_str(&change.diff_json).unwrap();
    assert_eq!(diff["webhook_host"], "hooks.example:8443");
    assert!(
        !change.diff_json.contains("s3cret") && !change.diff_json.contains("/services/"),
        "a viewer reads registry changes, so the path and the query stay out: {}",
        change.diff_json
    );
}

#[test]
fn a_question_is_purged_on_the_body_retention_while_its_event_stays() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.upsert_alert_rule(&rule("r1"), None, "admin").unwrap();
    let asked = |id: &str, subject: &str, opened_at: i64, expires_at: Option<i64>| AlertEventRow {
        id: id.into(),
        rule_id: "r1".into(),
        kind: "ask_user_question".into(),
        subject: subject.into(),
        state: "open".into(),
        opened_at,
        resolved_at: None,
        detail_json: Some(
            serde_json::json!({
                "key": "desktop",
                "question": "Which branch should I push to?",
                "question_expires_at": expires_at,
            })
            .to_string(),
        ),
    };
    assert!(store
        .insert_alert_event(&asked("old", "desktop:s1", 1_000, None))
        .unwrap());
    assert!(store
        .insert_alert_event(&asked("fresh", "desktop:s2", 9_000, None))
        .unwrap());
    // The key that captures for an hour under a longer global cap: the event is young enough for the cap and
    // the body it quoted is already gone.
    assert!(store
        .insert_alert_event(&asked("short", "desktop:s3", 9_000, Some(9_500)))
        .unwrap());
    assert!(store
        .insert_alert_event(&asked("long", "desktop:s4", 9_000, Some(90_000)))
        .unwrap());
    assert_eq!(
        store.purge_alert_events_before(0, 5_000, 10_000).unwrap(),
        0,
        "the question purge deletes no event of its own"
    );
    let detail = |id: &str| {
        serde_json::from_str::<serde_json::Value>(
            &store.alert_event(id).unwrap().unwrap().detail_json.unwrap(),
        )
        .unwrap()
    };
    assert_eq!(detail("old")["question"], serde_json::Value::Null);
    assert_eq!(
        detail("old")["key"],
        "desktop",
        "the rest of the detail is left alone"
    );
    assert_eq!(
        detail("fresh")["question"],
        "Which branch should I push to?"
    );
    assert_eq!(
        detail("short")["question"],
        serde_json::Value::Null,
        "the question goes with the body it came out of, not with the global cap"
    );
    assert_eq!(
        detail("long")["question"],
        "Which branch should I push to?",
        "a body still in retention keeps its question"
    );
    assert!(
        store.alert_event("old").unwrap().is_some(),
        "the event outlives the prompt content it carried"
    );
}

#[test]
fn a_lifecycle_delivery_lands_without_an_event_lists_newest_first_and_is_purged_on_the_resolved_mark(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.upsert_alert_rule(&rule("r1"), None, "test").unwrap();
    let mut ids = Vec::new();
    for (at, event) in [(1_000, "session_started"), (2_000, "session_ended")] {
        ids.push(
            store
                .insert_alert_delivery(&AlertDeliveryRow {
                    id: 0,
                    event_id: None,
                    rule_id: "r1".into(),
                    kind: "reauth_required".into(),
                    subject: "desktop, session s1".into(),
                    event: Some(event.into()),
                    detail_json: Some(format!(r#"{{"event":"{event}","session":"s1"}}"#)),
                    state: "event".into(),
                    attempt: 1,
                    at,
                    status: Some(200),
                    error: None,
                })
                .unwrap(),
        );
    }
    let listed = store.lifecycle_deliveries(None, 10).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|d| (d.id, d.event.as_deref()))
            .collect::<Vec<_>>(),
        [
            (ids[1], Some("session_ended")),
            (ids[0], Some("session_started"))
        ]
    );
    assert_eq!(listed[0].event_id, None);
    assert_eq!(
        store
            .lifecycle_deliveries(Some("session_idle"), 10)
            .unwrap(),
        []
    );
    assert_eq!(
        store
            .lifecycle_deliveries(Some("reauth_required"), 1)
            .unwrap()
            .len(),
        1
    );
    store.purge_alert_events_before(1_500, 0, 3_000).unwrap();
    assert_eq!(
        store.lifecycle_deliveries(None, 10).unwrap().len(),
        1,
        "the older delivery aged out on the resolved-events mark"
    );
}

#[test]
fn deliveries_for_several_events_come_back_grouped() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.upsert_alert_rule(&rule("r1"), None, "test").unwrap();
    let mut expected: Vec<(String, Vec<u64>)> = Vec::new();
    for subject in ["e1", "e2", "e3"] {
        let event = AlertEventRow {
            id: subject.into(),
            rule_id: "r1".into(),
            kind: "reauth_required".into(),
            subject: subject.into(),
            state: "open".into(),
            opened_at: 2_000,
            resolved_at: None,
            detail_json: None,
        };
        assert!(store.insert_alert_event(&event).unwrap());
        let mut ids = Vec::new();
        for attempt in [1, 2] {
            ids.push(
                store
                    .insert_alert_delivery(&AlertDeliveryRow {
                        id: 0,
                        event_id: Some(subject.into()),
                        rule_id: "r1".into(),
                        kind: "reauth_required".into(),
                        subject: subject.into(),
                        event: None,
                        detail_json: None,
                        state: "opened".into(),
                        attempt,
                        at: 2_000 + attempt as i64,
                        status: Some(200),
                        error: None,
                    })
                    .unwrap(),
            );
        }
        ids.sort_unstable();
        expected.push((subject.into(), ids));
    }
    let grouped = store
        .alert_deliveries_for(&["e1", "e2", "e3", "no-such-event"])
        .unwrap();
    assert_eq!(grouped.len(), 3, "the id with no deliveries carries no key");
    for (event_id, ids) in &expected {
        let mut got: Vec<u64> = grouped[event_id].iter().map(|d| d.id).collect();
        got.sort_unstable();
        assert_eq!(&got, ids);
        assert!(grouped[event_id]
            .iter()
            .all(|d| d.event_id == Some(event_id.clone())));
    }
    assert!(!grouped.contains_key("no-such-event"));
}
