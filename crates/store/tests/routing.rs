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
                incarnation: 1,
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
        set_at: 0,
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
fn a_cooldown_records_when_it_was_set_and_the_winning_deadline_brings_its_own() {
    let store = store_with(&["a"]);
    let mut first = cooldown("a", "account", 10_000);
    first.set_at = 1_000;
    store.upsert_cooldown(&first).unwrap();
    let row = store.load_routing().unwrap().cooldowns.pop().unwrap();
    assert_eq!((row.until, row.set_at), (10_000, 1_000));
    let mut shorter = cooldown("a", "account", 5_000);
    shorter.set_at = 2_000;
    store.upsert_cooldown(&shorter).unwrap();
    let row = store.load_routing().unwrap().cooldowns.pop().unwrap();
    assert_eq!(
        (row.until, row.set_at),
        (10_000, 2_000),
        "the deadline that lost leaves the standing one in place, dated by the newer rejection"
    );
    let mut longer = cooldown("a", "account", 20_000);
    longer.set_at = 3_000;
    store.upsert_cooldown(&longer).unwrap();
    let row = store.load_routing().unwrap().cooldowns.pop().unwrap();
    assert_eq!(
        (row.until, row.set_at),
        (20_000, 3_000),
        "and the one that wins brings its own, so a sample is compared against the rejection it belongs to"
    );
}

#[test]
fn a_repeat_rejection_advances_the_set_time_under_the_standing_deadline() {
    let store = store_with(&["a"]);
    let mut first = cooldown("a", "account", 10_000);
    first.set_at = 1_000;
    store.upsert_cooldown(&first).unwrap();
    let mut again = cooldown("a", "account", 10_000);
    again.set_at = 2_000;
    store.upsert_cooldown(&again).unwrap();
    let row = store.load_routing().unwrap().cooldowns.pop().unwrap();
    assert_eq!(
        (row.until, row.set_at),
        (10_000, 2_000),
        "the deadline stands and the row dates the latest rejection behind it"
    );
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
        migrated_at: None,
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

#[test]
fn a_pin_carries_when_it_last_migrated_for_utilization() {
    let store = store_with(&["a", "b"]);
    let pin = PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        model: "claude-sonnet-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        migrated_at: None,
    };
    store.upsert_pin(&pin).unwrap();
    store
        .upsert_pin(&PinRow {
            account_id: "b".into(),
            epoch: 2,
            migrated_at: Some(7),
            ..pin.clone()
        })
        .unwrap();
    assert_eq!(store.load_routing().unwrap().pins[0].migrated_at, Some(7));
    store
        .upsert_pin(&PinRow {
            epoch: 3,
            migrated_at: None,
            ..pin
        })
        .unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins[0].migrated_at,
        None,
        "a fresh pin clears the dwell"
    );
}
