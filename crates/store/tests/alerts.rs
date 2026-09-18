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
    assert_eq!(store.schema_version().unwrap(), 10);
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
            event_id: "e1".into(),
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
            event_id: "e1".into(),
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
        store.purge_alert_events_before(4_500).unwrap(),
        1,
        "the resolved one is purged, the open one stays"
    );
    store.delete_alert_rule("r1", "admin").unwrap();
    assert!(
        store.alert_event("e2").unwrap().is_none()
            && store.alert_deliveries("e1").unwrap().is_empty(),
        "the cascade"
    );
}
