use phoebis_store::{
    AccountKind, AccountRow, AccountState, CooldownRow, OverageRow, PinRow, StorageKey, Store,
};

fn store_with(ids: &[&str]) -> Store {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    for id in ids {
        store
            .insert_account(&AccountRow {
                id: (*id).into(),
                kind: AccountKind::ApiKey,
                label: (*id).into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
    }
    store
}

fn cooldown(account: &str, scope: &str, until: i64) -> CooldownRow {
    CooldownRow {
        account_id: account.into(),
        scope: scope.into(),
        until,
        reason: "rate_limit".into(),
        window: "five_hour".into(),
        source_request_id: None,
    }
}

#[test]
fn a_cooldown_deadline_is_never_shortened_and_expired_rows_are_purged() {
    let store = store_with(&["a"]);
    store
        .upsert_cooldown(&cooldown("a", "account", 10_000))
        .unwrap();
    store
        .upsert_cooldown(&cooldown("a", "account", 5_000))
        .unwrap();
    assert_eq!(store.load_routing().unwrap().cooldowns[0].until, 10_000);
    let mut longer = cooldown("a", "account", 20_000);
    longer.window = "seven_day".into();
    store.upsert_cooldown(&longer).unwrap();
    let row = &store.load_routing().unwrap().cooldowns[0];
    assert_eq!((row.until, row.window.as_str()), (20_000, "seven_day"));
    store
        .upsert_cooldown(&cooldown("a", "model:claude-opus-5", 1_000))
        .unwrap();
    assert_eq!(store.purge_cooldowns_before(1_500).unwrap(), 1);
    assert_eq!(store.load_routing().unwrap().cooldowns.len(), 1);
    assert!(store.delete_cooldown("a", "account").unwrap());
    assert!(!store.delete_cooldown("a", "account").unwrap());
}

#[test]
fn pins_are_upserted_deleted_and_dropped_with_their_account() {
    let store = store_with(&["a", "b"]);
    let pin = PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        model: "claude-sonnet-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
    };
    store.upsert_pin(&pin).unwrap();
    store
        .upsert_pin(&PinRow {
            account_id: "b".into(),
            epoch: 2,
            last_used_at: 9,
            ..pin.clone()
        })
        .unwrap();
    let pins = store.load_routing().unwrap().pins;
    assert_eq!(
        (
            pins.len(),
            pins[0].account_id.as_str(),
            pins[0].epoch,
            pins[0].last_used_at
        ),
        (1, "b", 2, 9)
    );
    store.delete_account("b", "test").unwrap();
    assert!(
        store.load_routing().unwrap().pins.is_empty(),
        "foreign keys cascade the pin"
    );
}

#[test]
fn overage_evidence_only_moves_forward_in_time() {
    let store = store_with(&["a"]);
    let newer = OverageRow {
        account_id: "a".into(),
        in_use: true,
        since: Some(50),
        reset: Some(900),
        observed_at: 100,
        source: "head".into(),
    };
    assert!(store.upsert_overage(&newer).unwrap());
    let older = OverageRow {
        in_use: false,
        observed_at: 90,
        ..newer.clone()
    };
    assert!(
        !store.upsert_overage(&older).unwrap(),
        "a late head cannot roll the state back"
    );
    assert!(store.load_routing().unwrap().overage[0].in_use);
}
