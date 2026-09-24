use phoebis_store::{
    AccountKind, AccountRow, AccountState, CooldownRow, OverageRow, PinRow, RekeyWrite,
    ResolutionRow, StorageKey, Store,
};

const SPELLING: &str = "claude-sonnet-4-5";
const DATED: &str = "claude-sonnet-4-5-20250929";

fn store_with(ids: &[&str]) -> Store {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    seed_accounts(&store, ids);
    store
}

fn seed_accounts(store: &Store, ids: &[&str]) {
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
        incarnation: 1,
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
        key_generation: 1,
        incarnation: 1,
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
        incarnation: 1,
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
        key_generation: 1,
        incarnation: 1,
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

#[test]
fn the_routing_rows_carry_the_holder_they_were_recorded_for() {
    let store = store_with(&["a"]);
    let pin = PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        model: "claude-sonnet-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        migrated_at: None,
        key_generation: 7,
        incarnation: 3,
    };
    store.upsert_pin(&pin).unwrap();
    store
        .upsert_cooldown(&CooldownRow {
            incarnation: 3,
            ..cooldown("a", "account", 10_000)
        })
        .unwrap();
    let reading = OverageRow {
        account_id: "a".into(),
        in_use: true,
        since: Some(50),
        reset: None,
        observed_at: 100,
        source: "sample".into(),
        incarnation: 3,
    };
    store.upsert_overage(&reading).unwrap();
    let load = store.load_routing().unwrap();
    assert_eq!(
        (load.pins[0].key_generation, load.pins[0].incarnation),
        (7, 3)
    );
    assert_eq!(load.cooldowns[0].incarnation, 3);
    assert_eq!(load.overage[0].incarnation, 3);

    store
        .upsert_pin(&PinRow {
            epoch: 2,
            key_generation: 8,
            incarnation: 4,
            ..pin
        })
        .unwrap();
    let load = store.load_routing().unwrap();
    assert_eq!(
        (load.pins[0].key_generation, load.pins[0].incarnation),
        (8, 4),
        "a pin made again is the new holders'"
    );
    assert!(
        store
            .upsert_overage(&OverageRow {
                in_use: false,
                observed_at: 90,
                incarnation: 4,
                ..reading
            })
            .unwrap(),
        "a reading from another holder of the id is no older than the one it replaces"
    );
    let overage = &store.load_routing().unwrap().overage[0];
    assert_eq!((overage.in_use, overage.incarnation), (false, 4));
}

fn spelling_pin() -> PinRow {
    PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        model: SPELLING.into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        migrated_at: None,
        key_generation: 1,
        incarnation: 1,
    }
}

/// The rows one landing of the spelling's resolution moves: the pin and the model cooldown of account `a`, and
/// the spelling row beside them.
fn rekey_of_a() -> RekeyWrite {
    RekeyWrite {
        resolution: ResolutionRow {
            spelling: SPELLING.into(),
            dated_id: DATED.into(),
            resolved_at: 1_500,
        },
        delete_pins: vec![("k".into(), "s".into(), SPELLING.into())],
        upsert_pins: vec![PinRow {
            model: DATED.into(),
            ..spelling_pin()
        }],
        delete_cooldowns: vec![("a".into(), format!("model:{SPELLING}"))],
        upsert_cooldowns: vec![cooldown("a", &format!("model:{DATED}"), 10_000)],
    }
}

#[test]
fn a_rekey_that_fails_on_its_last_statement_leaves_every_row_as_it_was() {
    let store = store_with(&["a"]);
    store.upsert_pin(&spelling_pin()).unwrap();
    store
        .upsert_cooldown(&cooldown("a", &format!("model:{SPELLING}"), 10_000))
        .unwrap();
    // `cooldowns.account_id REFERENCES accounts(id)` is what refuses the last statement below; the store turns
    // `foreign_keys` on for its connection, which a row naming an unknown account shows on its own.
    assert!(
        store
            .upsert_cooldown(&cooldown("ghost", "model:claude-opus-5", 10_000))
            .is_err(),
        "the foreign key is enforced"
    );
    let before = store.load_routing().unwrap();
    let mut rekey = rekey_of_a();
    rekey
        .upsert_cooldowns
        .push(cooldown("ghost", &format!("model:{DATED}"), 10_000));
    assert!(store.rekey_resolution(&rekey).is_err());
    let after = store.load_routing().unwrap();
    assert_eq!(after.pins, before.pins, "the pin stays under the spelling");
    assert_eq!(
        after.cooldowns, before.cooldowns,
        "the cooldown stays under the spelling"
    );
    assert!(
        after.resolutions.is_empty(),
        "and no spelling row points at rows that never moved"
    );
}

#[test]
fn a_rekey_read_back_after_a_reopen_carries_the_moved_rows_and_the_spelling_row_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    {
        let store = Store::open(&path, &storage_key).unwrap();
        seed_accounts(&store, &["a"]);
        store.upsert_pin(&spelling_pin()).unwrap();
        store
            .upsert_cooldown(&cooldown("a", &format!("model:{SPELLING}"), 10_000))
            .unwrap();
        store.rekey_resolution(&rekey_of_a()).unwrap();
    }
    let load = Store::open(&path, &storage_key)
        .unwrap()
        .load_routing()
        .unwrap();
    let rekey = rekey_of_a();
    assert_eq!(load.pins, rekey.upsert_pins);
    assert_eq!(load.cooldowns, rekey.upsert_cooldowns);
    assert_eq!(load.resolutions, vec![rekey.resolution]);
}
