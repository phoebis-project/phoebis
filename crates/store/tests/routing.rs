use phoebis_routing::lane_key;
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AffinityChain, AffinityRow, BudgetRow, BudgetScope,
    CooldownRow, CreditsRow, OverageRow, PinBurn, PinKey, PinRow, RekeyWrite, ResolutionRow,
    ShedPin, ShedRoundRow, StateCause, StorageKey, Store, WindowRow,
};

const SPELLING: &str = "claude-sonnet-4-5";
const DATED: &str = "claude-sonnet-4-5-20250929";

/// A pin with the engine columns at unremarkable defaults, so a test that only cares about a handful of
/// fields does not have to spell every one.
fn pin(key_id: &str, session_id: &str, model: &str, account_id: &str) -> PinRow {
    PinRow {
        key_id: key_id.into(),
        session_id: session_id.into(),
        agent_id: "main".into(),
        model: model.into(),
        account_id: account_id.into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        key_generation: 1,
        incarnation: 1,
        burn: PinBurn::default(),
        burn_at: None,
        burn_banded: PinBurn::default(),
        last_input_tokens: 0,
        last_refresh_at: 1,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
        lane: lane_key(model, "standard", "none", "-"),
    }
}

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
                state_cause: StateCause::Operator,
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
    let seeded = PinRow {
        burn: PinBurn {
            input: 12.5,
            ..PinBurn::default()
        },
        burn_at: Some(2),
        burn_banded: PinBurn {
            input: 18.125,
            ..PinBurn::default()
        },
        last_input_tokens: 4_000,
        last_refresh_at: 3,
        cache_ttl_ms: 3_600_000,
        shed_from: Some("c".into()),
        shed_at: Some(4),
        static_hash: Some("static-hash".into()),
        tail_hash: Some("tail-hash".into()),
        ..pin("k", "s", "claude-sonnet-5", "a")
    };
    store.upsert_pin(&seeded).unwrap();
    store
        .upsert_pin(&PinRow {
            account_id: "b".into(),
            epoch: 2,
            last_used_at: 9,
            ..seeded.clone()
        })
        .unwrap();
    let pins = store.load_routing().unwrap().pins;
    assert_eq!(pins.len(), 1);
    let row = &pins[0];
    assert_eq!(
        (
            row.account_id.as_str(),
            row.epoch,
            row.last_used_at,
            row.burn.input,
            row.burn_at,
            row.last_input_tokens,
            row.last_refresh_at,
            row.cache_ttl_ms,
            row.shed_from.as_deref(),
            row.shed_at,
            row.static_hash.as_deref(),
            row.tail_hash.as_deref(),
        ),
        (
            "b",
            2,
            9,
            12.5,
            Some(2),
            4_000,
            3,
            3_600_000,
            Some("c"),
            Some(4),
            Some("static-hash"),
            Some("tail-hash"),
        )
    );
    assert_eq!(
        row.burn_banded.input, 18.125,
        "the banded copy reads back beside the plain one"
    );
    store.delete_account("b", "test").unwrap();
    assert!(
        store.load_routing().unwrap().pins.is_empty(),
        "foreign keys cascade the pin"
    );
}

#[test]
fn a_pins_typed_burn_and_its_banded_copy_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let burning = PinRow {
        burn: PinBurn {
            cache_read: 1.0,
            input: 2.0,
            output: 3.0,
            write_1h: 4.0,
            write_5m: 5.0,
        },
        burn_at: Some(7),
        burn_banded: PinBurn {
            cache_read: 1.5,
            input: 2.5,
            output: 3.5,
            write_1h: 4.5,
            write_5m: 5.5,
        },
        ..pin("k", "s", "claude-opus-5", "a")
    };
    {
        let store = Store::open(&path, &storage_key).unwrap();
        seed_accounts(&store, &["a"]);
        store.upsert_pin(&burning).unwrap();
    }
    let reopened = Store::open(&path, &storage_key)
        .unwrap()
        .load_routing()
        .unwrap()
        .pins;
    assert_eq!(
        reopened
            .iter()
            .map(|p| (p.burn, p.burn_at, p.burn_banded))
            .collect::<Vec<_>>(),
        vec![(burning.burn, Some(7), burning.burn_banded)],
        "every type of both copies reads back as written"
    );
}

#[test]
fn a_pins_lane_and_an_affinity_entrys_lane_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let standard = pin("k", "s", "claude-opus-5-5", "a");
    let fast = lane_key("claude-opus-5-5", "fast", "adaptive", "*");
    {
        let store = Store::open(&path, &storage_key).unwrap();
        seed_accounts(&store, &["a"]);
        store.upsert_pin(&standard).unwrap();
        store
            .upsert_pin(&PinRow {
                lane: fast.clone(),
                ..standard.clone()
            })
            .unwrap();
        let entry = affinity_row("claude-opus-5-5", "hash-1", "a");
        store.upsert_affinity(std::slice::from_ref(&entry)).unwrap();
        store
            .upsert_affinity(&[AffinityRow {
                lane: fast.clone(),
                ..entry
            }])
            .unwrap();
    }
    let reopened = Store::open(&path, &storage_key)
        .unwrap()
        .load_routing()
        .unwrap();
    assert_eq!(
        reopened
            .pins
            .iter()
            .map(|p| p.lane.as_str())
            .collect::<Vec<_>>(),
        vec![fast.as_str()],
        "the pin's second lane replaced its first"
    );
    assert_eq!(
        reopened
            .affinity
            .iter()
            .map(|r| r.lane.as_str())
            .collect::<Vec<_>>(),
        vec![fast.as_str()],
        "so did the entry's"
    );
}

#[test]
fn a_days_budget_adds_each_settled_share() {
    let store = store_with(&["a"]);
    let charge = |five_hour_share| BudgetRow {
        scope: BudgetScope::Account,
        id: "a".into(),
        usd: 0.0,
        five_hour_share,
    };
    store
        .add_keepalive_budget("2026-09-29", &charge(0.25))
        .unwrap();
    store
        .add_keepalive_budget("2026-09-29", &charge(0.1))
        .unwrap();
    let read = store.keepalive_budgets("2026-09-29").unwrap();
    assert_eq!(read.len(), 1, "one row per day and account: {read:?}");
    assert!(
        (read[0].five_hour_share - 0.35).abs() < 1e-12,
        "the shares add: {read:?}"
    );
}

#[test]
fn two_agents_of_one_session_hold_two_pins() {
    let store = store_with(&["a"]);
    let main = pin("k", "s", "claude-sonnet-5", "a");
    let subagent = PinRow {
        agent_id: "sub-1".into(),
        ..main.clone()
    };
    store.upsert_pin(&main).unwrap();
    store.upsert_pin(&subagent).unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins.len(),
        2,
        "the two agents of one session each hold their own pin"
    );
    assert!(store.delete_pin(&subagent.key()).unwrap());
    assert_eq!(
        store.load_routing().unwrap().pins,
        vec![main],
        "the other agent's pin is untouched"
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

fn window_row(window: &str, utilization: f64, observed_at: i64, incarnation: u64) -> WindowRow {
    WindowRow {
        account_id: "a".into(),
        incarnation,
        observed_at,
        reset: Some(observed_at + 3_600_000),
        utilization,
        window: window.into(),
    }
}

#[test]
fn usage_windows_survive_a_reopen_move_only_forward_and_leave_with_their_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let rows = vec![
        window_row("five_hour", 0.5, 1_000, 1),
        window_row("seven_day", 0.25, 1_000, 1),
        window_row("seven_day_fable", 0.75, 1_000, 1),
    ];
    {
        let store = Store::open(&path, &storage_key).unwrap();
        seed_accounts(&store, &["a"]);
        for row in &rows {
            assert!(store.upsert_window(row).unwrap(), "{row:?}");
        }
    }
    let store = Store::open(&path, &storage_key).unwrap();
    assert_eq!(
        store.load_routing().unwrap().windows,
        rows,
        "the three rows reopen as written"
    );
    let older = WindowRow {
        utilization: 0.9,
        observed_at: 900,
        ..rows[0].clone()
    };
    assert!(
        !store.upsert_window(&older).unwrap(),
        "an older reading writes nothing"
    );
    assert_eq!(store.load_routing().unwrap().windows[0], rows[0]);
    let newer = WindowRow {
        utilization: 0.6,
        observed_at: 2_000,
        ..rows[0].clone()
    };
    assert!(store.upsert_window(&newer).unwrap());
    assert_eq!(
        store.load_routing().unwrap().windows[0],
        newer,
        "a newer reading replaces the row"
    );
    store.delete_account("a", "test").unwrap();
    assert!(
        store.load_routing().unwrap().windows.is_empty(),
        "the rows leave with their account"
    );
    assert_eq!(
        store.purge_stale_windows().unwrap(),
        0,
        "the cascade took them, so the purge finds none"
    );
}

#[test]
fn a_window_of_another_holder_of_the_id_is_never_written_loaded_or_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let store = Store::open(&path, &storage_key).unwrap();
    seed_accounts(&store, &["a"]);
    store.delete_account("a", "test").unwrap();
    seed_accounts(&store, &["a"]);
    assert_eq!(store.account("a").unwrap().unwrap().incarnation, 2);
    assert!(
        !store
            .upsert_window(&window_row("five_hour", 0.9, 1_000, 1))
            .unwrap(),
        "a write queued for the deleted holder lands nowhere"
    );
    assert!(store.load_routing().unwrap().windows.is_empty());
    drop(store);
    // A row the replacement never wrote, as a write from before the incarnation fence would have left it.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO usage_windows (account_id, window, utilization, reset, observed_at, incarnation) \
         VALUES ('a', 'seven_day', 0.9, NULL, 1000, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &storage_key).unwrap();
    assert!(
        store.load_routing().unwrap().windows.is_empty(),
        "the previous holder's row is never loaded"
    );
    assert_eq!(store.purge_stale_windows().unwrap(), 1);
    assert_eq!(
        store.purge_stale_windows().unwrap(),
        0,
        "and the purge deleted it"
    );
}

#[test]
fn the_replacements_own_window_replaces_a_stale_row_of_another_holder_however_new_its_date() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let store = Store::open(&path, &storage_key).unwrap();
    seed_accounts(&store, &["a"]);
    store.delete_account("a", "test").unwrap();
    seed_accounts(&store, &["a"]);
    drop(store);
    // A row the replacement never wrote, dated past the replacement's first reading.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO usage_windows (account_id, window, utilization, reset, observed_at, incarnation) \
         VALUES ('a', 'five_hour', 0.9, NULL, 5000, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &storage_key).unwrap();
    let replacement = window_row("five_hour", 0.2, 3_000, 2);
    assert!(
        store.upsert_window(&replacement).unwrap(),
        "the replacement's own reading replaces the stale row rather than waiting for the purge"
    );
    assert_eq!(store.load_routing().unwrap().windows, [replacement]);
}

fn credits_row(enabled: bool, observed_at: i64, incarnation: u64) -> CreditsRow {
    CreditsRow {
        account_id: "a".into(),
        incarnation,
        enabled,
        disabled_reason: (!enabled).then(|| "out_of_credits".to_string()),
        utilization: enabled.then_some(0.2),
        observed_at,
    }
}

#[test]
fn the_credits_state_survives_a_reopen_moves_only_forward_and_stays_with_its_holder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let blocked = credits_row(false, 1_000, 1);
    {
        let store = Store::open(&path, &storage_key).unwrap();
        seed_accounts(&store, &["a"]);
        assert!(store.upsert_credits(&blocked).unwrap());
    }
    let store = Store::open(&path, &storage_key).unwrap();
    assert_eq!(
        store.load_routing().unwrap().credits,
        std::slice::from_ref(&blocked),
        "the state reopens as written"
    );
    assert!(
        !store.upsert_credits(&credits_row(true, 900, 1)).unwrap(),
        "an older reading writes nothing"
    );
    let newer = credits_row(true, 2_000, 1);
    assert!(store.upsert_credits(&newer).unwrap());
    assert_eq!(
        store.load_routing().unwrap().credits,
        [newer],
        "a newer reading replaces it"
    );
    store.delete_account("a", "test").unwrap();
    seed_accounts(&store, &["a"]);
    assert!(
        store.load_routing().unwrap().credits.is_empty(),
        "the state left with its account"
    );
    assert!(
        !store.upsert_credits(&blocked).unwrap(),
        "a write queued for the deleted holder lands nowhere"
    );
    drop(store);
    // A row the replacement never wrote, as a write from before the incarnation fence would have left it.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO usage_credits (account_id, enabled, disabled_reason, utilization, observed_at, incarnation) \
         VALUES ('a', 0, 'out_of_credits', NULL, 5000, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &storage_key).unwrap();
    assert!(
        store.load_routing().unwrap().credits.is_empty(),
        "the previous holder's state is never loaded"
    );
    let replacement = credits_row(true, 3_000, 2);
    assert!(
        store.upsert_credits(&replacement).unwrap(),
        "the replacement's own reading replaces a stale row, however new its date"
    );
    assert_eq!(store.load_routing().unwrap().credits, [replacement]);
}

#[test]
fn the_purge_drops_a_credits_state_of_another_holder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let storage_key = StorageKey::generate();
    let store = Store::open(&path, &storage_key).unwrap();
    seed_accounts(&store, &["a"]);
    store.delete_account("a", "test").unwrap();
    seed_accounts(&store, &["a"]);
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO usage_credits (account_id, enabled, disabled_reason, utilization, observed_at, incarnation) \
         VALUES ('a', 0, 'out_of_credits', NULL, 1000, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &storage_key).unwrap();
    assert_eq!(store.purge_stale_windows().unwrap(), 1);
    assert_eq!(
        store.purge_stale_windows().unwrap(),
        0,
        "and the purge deleted it"
    );
}

#[test]
fn the_routing_rows_carry_the_holder_they_were_recorded_for() {
    let store = store_with(&["a"]);
    let seeded = PinRow {
        key_generation: 7,
        incarnation: 3,
        ..pin("k", "s", "claude-sonnet-5", "a")
    };
    store.upsert_pin(&seeded).unwrap();
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
            ..seeded
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
    pin("k", "s", SPELLING, "a")
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
        delete_pins: vec![PinKey {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: SPELLING.into(),
        }],
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

#[test]
fn write_routing_rolls_back_the_whole_batch_on_a_refused_write() {
    let store = store_with(&["a"]);
    let refused = store.write_routing(|tx| {
        tx.upsert_pin(&pin("k", "s", "claude-sonnet-5", "a"))?;
        tx.upsert_cooldown(&cooldown("ghost", "account", 10_000))
    });
    assert!(
        refused.is_err(),
        "`cooldowns.account_id REFERENCES accounts(id)` refuses the unknown account's row"
    );
    let load = store.load_routing().unwrap();
    assert!(
        load.pins.is_empty(),
        "the pin written ahead of the refusal rolled back with it"
    );
    assert!(load.cooldowns.is_empty());
}

fn affinity_row(model: &str, hash: &str, account_id: &str) -> AffinityRow {
    AffinityRow {
        model: model.into(),
        hash: hash.into(),
        chain: AffinityChain::Message,
        account_id: account_id.into(),
        incarnation: 1,
        written_at: 1_000,
        ttl_ms: 300_000,
        prefix_tokens: 500,
        key_id: "k".into(),
        key_generation: 1,
        root: "root-1".into(),
        lane: lane_key(model, "standard", "none", "-"),
        depth: 1,
    }
}

#[test]
fn affinity_rows_land_together_replace_by_model_and_hash_and_delete_by_key() {
    let store = store_with(&["a"]);
    let rows = vec![
        affinity_row("claude-sonnet-5", "hash-1", "a"),
        affinity_row("claude-sonnet-5", "hash-2", "a"),
        affinity_row("claude-opus-5", "hash-3", "a"),
    ];
    store.upsert_affinity(&rows).unwrap();
    let replacement = AffinityRow {
        written_at: 2_000,
        ..affinity_row("claude-sonnet-5", "hash-1", "a")
    };
    store.upsert_affinity(&[replacement]).unwrap();
    let loaded = store.load_routing().unwrap().affinity;
    assert_eq!(
        loaded.len(),
        3,
        "the row under the existing key replaced rather than added"
    );
    assert_eq!(
        loaded
            .iter()
            .find(|r| r.hash == "hash-1")
            .unwrap()
            .written_at,
        2_000,
        "the replacement landed"
    );
    assert_eq!(
        loaded
            .iter()
            .map(|r| (r.model.as_str(), r.hash.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("claude-opus-5", "hash-3"),
            ("claude-sonnet-5", "hash-1"),
            ("claude-sonnet-5", "hash-2"),
        ],
        "the load reads model, hash order"
    );
    let deleted = store
        .delete_affinity(&[
            ("claude-sonnet-5".into(), "hash-1".into()),
            ("claude-opus-5".into(), "hash-3".into()),
        ])
        .unwrap();
    assert_eq!(deleted, 2);
    let remaining = store.load_routing().unwrap().affinity;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].hash, "hash-2");
    store.upsert_affinity(&rows).unwrap();
    let batch: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.model.clone(), r.hash.clone()))
        .collect();
    assert_eq!(
        (
            store.delete_affinity(&batch).unwrap(),
            store.load_routing().unwrap().affinity.len()
        ),
        (3, 0),
        "a batch of three leaves in one call"
    );
}

#[test]
fn an_affinity_row_cascades_with_its_account() {
    let store = store_with(&["a"]);
    store
        .upsert_affinity(&[affinity_row("claude-sonnet-5", "hash-1", "a")])
        .unwrap();
    store.delete_account("a", "test").unwrap();
    assert!(
        store.load_routing().unwrap().affinity.is_empty(),
        "foreign keys cascade the row"
    );
}

fn shed_key(session_id: &str) -> PinKey {
    PinKey {
        key_id: "k".into(),
        session_id: session_id.into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
    }
}

fn shed_pin(session_id: &str, epoch: u64, marked: bool) -> ShedPin {
    ShedPin {
        key: shed_key(session_id),
        epoch,
        marked,
    }
}

/// The loaded rows as `(pool, account, session, epoch, round_at, marked)`, in a fixed order.
fn loaded_rounds(store: &Store) -> Vec<(String, String, String, u64, i64, bool)> {
    let mut rows: Vec<_> = store
        .load_routing()
        .unwrap()
        .shed_rounds
        .into_iter()
        .map(|r| {
            (
                r.pool_id,
                r.account_id,
                r.session_id,
                r.epoch,
                r.round_at,
                r.marked,
            )
        })
        .collect();
    rows.sort();
    rows
}

fn round_row(
    pool: &str,
    account: &str,
    session: &str,
    epoch: u64,
    round_at: i64,
    marked: bool,
) -> (String, String, String, u64, i64, bool) {
    (
        pool.into(),
        account.into(),
        session.into(),
        epoch,
        round_at,
        marked,
    )
}

#[test]
fn a_round_replaces_the_pool_s_and_account_s_rows_and_loads_back() {
    let store = store_with(&["a", "b"]);
    store
        .replace_shed_round(
            "p",
            "a",
            1,
            1_000,
            &[shed_pin("s1", 4, true), shed_pin("s2", 5, false)],
        )
        .unwrap();
    // Another pool's round on the account and the pool's round on another account stand apart from it.
    store
        .replace_shed_round("q", "a", 1, 1_500, &[shed_pin("s9", 9, true)])
        .unwrap();
    store
        .replace_shed_round("p", "b", 1, 1_500, &[shed_pin("s8", 8, false)])
        .unwrap();
    store
        .write_routing(|tx| {
            tx.replace_shed_round(
                "p",
                "a",
                1,
                2_000,
                &[shed_pin("s2", 5, true), shed_pin("s3", 6, false)],
            )
        })
        .unwrap();
    assert_eq!(
        loaded_rounds(&store),
        vec![
            round_row("p", "a", "s2", 5, 2_000, true),
            round_row("p", "a", "s3", 6, 2_000, false),
            round_row("p", "b", "s8", 8, 1_500, false),
            round_row("q", "a", "s9", 9, 1_500, true),
        ],
        "the second round of p on a replaced the first whole, s1 with it, and left the other rounds alone"
    );
    let row = store
        .load_routing()
        .unwrap()
        .shed_rounds
        .into_iter()
        .find(|r| r.pool_id == "p" && r.account_id == "a" && r.session_id == "s2")
        .unwrap();
    assert_eq!(
        row,
        ShedRoundRow {
            pool_id: "p".into(),
            account_id: "a".into(),
            incarnation: 1,
            key_id: "k".into(),
            session_id: "s2".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            epoch: 5,
            round_at: 2_000,
            marked: true,
        }
    );
}

#[test]
fn a_pin_s_removal_or_replacement_clears_its_mark_and_keeps_the_round() {
    let store = store_with(&["a", "b"]);
    let on_a = |session: &str, epoch: u64| PinRow {
        epoch,
        ..pin("k", session, "claude-sonnet-5", "a")
    };
    for (session, epoch) in [("gone", 1), ("moved", 2), ("renewed", 3)] {
        store.upsert_pin(&on_a(session, epoch)).unwrap();
    }
    store
        .replace_shed_round(
            "p",
            "a",
            1,
            1_000,
            &[
                shed_pin("gone", 1, true),
                shed_pin("moved", 2, true),
                shed_pin("renewed", 3, true),
            ],
        )
        .unwrap();
    assert!(store.delete_pin(&shed_key("gone")).unwrap());
    store
        .write_routing(|tx| {
            // A move lands the pin on b under a fresh epoch; a renewal keeps the pin's account and epoch.
            tx.upsert_pin(&PinRow {
                account_id: "b".into(),
                epoch: 7,
                ..on_a("moved", 2)
            })?;
            tx.upsert_pin(&PinRow {
                last_used_at: 900,
                ..on_a("renewed", 3)
            })
        })
        .unwrap();
    assert_eq!(
        loaded_rounds(&store),
        vec![
            round_row("p", "a", "gone", 1, 1_000, false),
            round_row("p", "a", "moved", 2, 1_000, false),
            round_row("p", "a", "renewed", 3, 1_000, true),
        ],
        "the removed and the replaced pins' marks left with them, every row and its round time stayed, and the \
         renewed pin kept its mark"
    );
    store.upsert_pin(&on_a("renewed", 8)).unwrap();
    assert_eq!(
        loaded_rounds(&store)[2],
        round_row("p", "a", "renewed", 3, 1_000, false),
        "a fresh pin under the key on the same account takes another epoch, so the old pin's mark leaves"
    );
}

#[test]
fn a_replaced_account_s_rounds_do_not_load() {
    let store = store_with(&["a"]);
    store.delete_account("a", "test").unwrap();
    seed_accounts(&store, &["a"]);
    assert_eq!(store.account("a").unwrap().unwrap().incarnation, 2);
    // A round formed against the deleted holder and written after the replacement took the id.
    store
        .replace_shed_round("p", "a", 1, 1_000, &[shed_pin("s1", 1, true)])
        .unwrap();
    assert!(
        store.load_routing().unwrap().shed_rounds.is_empty(),
        "the previous holder's round is never read back"
    );
    store
        .replace_shed_round("p", "a", 2, 2_000, &[shed_pin("s1", 1, true)])
        .unwrap();
    assert_eq!(
        loaded_rounds(&store),
        vec![round_row("p", "a", "s1", 1, 2_000, true)],
        "the replacement's own round loads"
    );
}

#[test]
fn a_deleted_account_takes_its_round_rows() {
    let store = store_with(&["a", "b"]);
    store
        .replace_shed_round("p", "a", 1, 1_000, &[shed_pin("s1", 1, true)])
        .unwrap();
    store
        .replace_shed_round("p", "b", 1, 1_000, &[shed_pin("s2", 2, true)])
        .unwrap();
    store.delete_account("a", "test").unwrap();
    // The purge with no cut counts every row the table still holds.
    assert_eq!(
        store.purge_shed_rounds_before(i64::MAX).unwrap(),
        1,
        "foreign keys cascade the deleted account's row and leave b's"
    );
}

#[test]
fn rounds_past_the_cut_are_purged_and_newer_ones_stay() {
    let store = store_with(&["a", "b"]);
    store
        .replace_shed_round(
            "p",
            "a",
            1,
            1_000,
            &[shed_pin("s1", 1, true), shed_pin("s2", 2, false)],
        )
        .unwrap();
    store
        .replace_shed_round("q", "a", 1, 5_000, &[shed_pin("s3", 3, true)])
        .unwrap();
    store
        .replace_shed_round("p", "b", 1, 4_000, &[shed_pin("s4", 4, false)])
        .unwrap();
    assert_eq!(
        store.purge_shed_rounds_before(4_000).unwrap(),
        2,
        "both rows of the round at 1,000 are past the cut"
    );
    assert_eq!(
        loaded_rounds(&store),
        vec![
            round_row("p", "b", "s4", 4, 4_000, false),
            round_row("q", "a", "s3", 3, 5_000, true),
        ],
        "a round at the cut or after it stays"
    );
    assert_eq!(
        store
            .write_routing(|tx| tx.purge_shed_rounds_before(4_001))
            .unwrap(),
        1
    );
    assert_eq!(
        loaded_rounds(&store),
        vec![round_row("q", "a", "s3", 3, 5_000, true)]
    );
}

#[test]
fn an_affinity_row_keeps_its_depth_through_a_write_and_a_load() {
    let store = store_with(&["a"]);
    store
        .upsert_affinity(&[AffinityRow {
            depth: 3,
            ..affinity_row("claude-sonnet-5", "hash-3", "a")
        }])
        .unwrap();
    store
        .write_routing(|tx| {
            tx.upsert_affinity(&[AffinityRow {
                depth: 7,
                ..affinity_row("claude-sonnet-5", "hash-7", "a")
            }])
        })
        .unwrap();
    let depths = |store: &Store| {
        store
            .load_routing()
            .unwrap()
            .affinity
            .into_iter()
            .map(|r| (r.hash, r.depth))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        depths(&store),
        vec![("hash-3".to_string(), 3), ("hash-7".to_string(), 7)]
    );
    store
        .upsert_affinity(&[AffinityRow {
            depth: 4,
            written_at: 2_000,
            ..affinity_row("claude-sonnet-5", "hash-3", "a")
        }])
        .unwrap();
    assert_eq!(
        depths(&store)[0],
        ("hash-3".to_string(), 4),
        "a row replaced under its key carries the replacement's depth"
    );
}
