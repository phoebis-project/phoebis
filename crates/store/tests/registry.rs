use phoebis_guardrails::{Condition, GuardrailAction, GuardrailMode, Operator, Selector, Subject};
use phoebis_store::{
    answers_stamp_token, stamp_token, AccountKind, AccountRow, AccountState, AccountUpdate,
    Affinity, AffinityChain, AffinityMode, ApiKeyRow, AttachmentSubject, BudgetPeriod, CanaryRead,
    Failover, GuardrailAttachment, GuardrailRow, Identity, MemberWrite, MemoryKey, MemoryRecord,
    MemoryStore, ModelRule, NewBundle, OveragePolicy, PinBurn, PinMark, PinRow, PoolRow,
    ReturnToPlan, RulesMode, Secret, SessionPlacement, SlowDown, StateCause, StorageKey, Store,
    StoreError, Strategy, SubagentPlacement, SubscriptionRow, WarmSessions, WindowRow,
};

fn account(id: &str, kind: AccountKind) -> AccountRow {
    AccountRow::for_tests(id, kind)
}

fn pool(id: &str) -> PoolRow {
    PoolRow {
        strategy: Strategy::Priority,
        prefer_subscription: false,
        created_at: 1,
        updated_at: 1,
        ..PoolRow::for_tests(id)
    }
}

fn key(store: &Store, id: &str, pool_id: &str, secret: &str) -> ApiKeyRow {
    ApiKeyRow {
        id: id.into(),
        label: id.into(),
        notes: None,
        pool_id: pool_id.into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        model_rules: Vec::new(),
        model_rules_mode: RulesMode::Extend,
        long_context_twins: true,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: store.api_key_digest(secret).to_vec(),
        prefix: phoebis_store::key_prefix(secret),
        incarnation: 1,
        created_at: 1,
        updated_at: 0,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
        auto_mode_server: None,
    }
}

fn member(account_id: &str, priority: u32) -> MemberWrite {
    MemberWrite {
        account_id: account_id.into(),
        priority,
        budget_daily_usd: None,
        budget_monthly_usd: None,
    }
}

/// A label edit through the one account write, fenced on the revision the caller read. The incarnation and the
/// generation are the row's own at call time, so only the revision half of the admin route's fence can refuse here.
fn rename(
    store: &Store,
    id: &str,
    revision: u64,
    label: &str,
    actor: &str,
) -> Result<(), StoreError> {
    let (incarnation, generation) = store
        .account(id)?
        .map_or((0, 0), |row| (row.incarnation, row.lifecycle_generation));
    store.commit_account_update(&AccountUpdate {
        id,
        expected_revision: revision,
        incarnation,
        lifecycle_generation: generation,
        label,
        secret: None,
        state: None,
        at: jiff::Timestamp::now().as_millisecond(),
        actor,
        enabled: None,
        cancellation: None,
    })
}

#[test]
fn a_version_one_registry_opens_at_three_with_the_new_columns_at_their_defaults_and_no_all_time_counters(
) {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    {
        let store = Store::open(&path, &key).unwrap();
        store
            .insert_account(&account("a", AccountKind::Oauth))
            .unwrap();
        store
            .upsert_pin(&PinRow {
                key_id: "k".into(),
                session_id: "s".into(),
                agent_id: "main".into(),
                model: "claude-opus-5".into(),
                account_id: "a".into(),
                epoch: 1,
                created_at: 1,
                last_used_at: 1,
                key_generation: 1,
                incarnation: 1,
                burn: PinBurn::default(),
                burn_at: None,
                burn_banded: PinBurn::default(),
                burn_fast: PinBurn::default(),
                burn_unmetered: PinBurn::default(),
                last_input_tokens: 0,
                last_classifier: false,
                last_refresh_at: 1,
                cache_ttl_ms: 300_000,
                shed: None,
                static_hash: None,
                tail_hash: None,
                lane: String::new(),
                entrypoint: String::new(),
                mark: None,
                auxiliary_only: true,
                effort: "high".into(),
            })
            .unwrap();
        let incarnation = store.account("a").unwrap().unwrap().incarnation;
        assert!(store
            .upsert_window(&WindowRow {
                account_id: "a".into(),
                incarnation,
                observed_at: 1_000,
                plan: Some("max".into()),
                reset: None,
                tier: Some("max_20x".into()),
                utilization: 0.5,
                window: "five_hour".into(),
            })
            .unwrap());
    }
    // The file as a version-1 build left it: the steps' columns and tables taken back, the counters' table in the
    // baseline's shape with an all-time row, and the marker at 1.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE pins DROP COLUMN auxiliary_only;
         ALTER TABLE pins DROP COLUMN effort;
         ALTER TABLE usage_windows DROP COLUMN plan;
         ALTER TABLE usage_windows DROP COLUMN tier;
         DROP TABLE session_first_seen;
         DROP TABLE usage_window_sets;
         DROP TABLE keepalive_idle_returns;
         CREATE TABLE IF NOT EXISTS keepalive_idle_returns (
           pool_id TEXT NOT NULL, bucket INTEGER NOT NULL, reached INTEGER NOT NULL, returned INTEGER NOT NULL,
           PRIMARY KEY (pool_id, bucket)
         );
         INSERT INTO keepalive_idle_returns (pool_id, bucket, reached, returned) VALUES ('p', 0, 3, 2);
         UPDATE meta SET value = '1' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(store.schema_version().unwrap(), 3);
    assert!(
        store.load_keepalive().unwrap().2.is_empty(),
        "the all-time counters leave with their table"
    );
    let load = store.load_routing().unwrap();
    assert_eq!(
        load.pins
            .iter()
            .map(|p| (p.session_id.as_str(), p.auxiliary_only, p.effort.as_str()))
            .collect::<Vec<_>>(),
        [("s", false, "")],
        "a pin from before the step reads the two columns' defaults"
    );
    assert_eq!(
        load.windows
            .iter()
            .map(|w| (w.window.as_str(), w.plan.as_deref(), w.tier.as_deref()))
            .collect::<Vec<_>>(),
        [("five_hour", None, None)],
        "a reading from before the step reads no plan and no tier"
    );
}

#[test]
fn a_pool_inserted_without_a_placement_reads_by_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .insert_pool(
            &PoolRow {
                affinity: Affinity::default(),
                ..PoolRow::for_tests("built")
            },
            "test",
        )
        .unwrap();
    assert_eq!(
        store
            .pool("built")
            .unwrap()
            .unwrap()
            .affinity
            .subagent_placement,
        SubagentPlacement::ByStrategy,
        "the row builder's default"
    );
}

#[test]
fn every_burn_copy_of_a_pin_and_the_columns_after_them_read_back_as_written() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let before = PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        agent_id: "main".into(),
        model: "claude-opus-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        key_generation: 1,
        incarnation: 1,
        burn: PinBurn {
            input: 1.0,
            ..PinBurn::default()
        },
        burn_at: Some(5),
        burn_banded: PinBurn {
            input: 1.0,
            ..PinBurn::default()
        },
        burn_fast: PinBurn::default(),
        burn_unmetered: PinBurn::default(),
        last_input_tokens: 1_000,
        last_classifier: false,
        last_refresh_at: 1,
        cache_ttl_ms: 300_000,
        shed: None,
        static_hash: None,
        tail_hash: None,
        lane: String::new(),
        entrypoint: String::new(),
        mark: None,
        auxiliary_only: false,
        effort: String::new(),
    };
    store.upsert_pin(&before).unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins,
        vec![before.clone()],
        "a pin written with no unmetered burn reads a zero unmetered copy"
    );
    // A distinct value in every copy and every column after them, so a read left on another column's offset shows.
    let copy = |base: f64| PinBurn {
        cache_read: base + 0.1,
        input: base + 0.2,
        output: base + 0.3,
        write_1h: base + 0.4,
        write_5m: base + 0.5,
    };
    let pin = PinRow {
        burn: copy(1.0),
        burn_banded: copy(2.0),
        burn_fast: copy(3.0),
        burn_unmetered: copy(4.0),
        mark: Some(PinMark {
            at: 7,
            on: "a".into(),
            window: "five_hour".into(),
        }),
        entrypoint: "cli".into(),
        ..before
    };
    store.upsert_pin(&pin).unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins,
        vec![pin],
        "every copy and every column after them reads back as written"
    );
}

#[test]
fn a_pins_last_classifier_flag_reads_back_as_written_and_a_standard_renewal_clears_it() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let before = PinRow {
        key_id: "k".into(),
        session_id: "s".into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 1,
        last_used_at: 1,
        key_generation: 1,
        incarnation: 1,
        burn: PinBurn::default(),
        burn_at: Some(5),
        burn_banded: PinBurn::default(),
        burn_fast: PinBurn::default(),
        burn_unmetered: PinBurn::default(),
        last_input_tokens: 1_000,
        last_classifier: false,
        last_refresh_at: 1,
        cache_ttl_ms: 300_000,
        shed: None,
        static_hash: None,
        tail_hash: None,
        lane: String::new(),
        entrypoint: String::new(),
        mark: None,
        auxiliary_only: false,
        effort: String::new(),
    };
    store.upsert_pin(&before).unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins,
        vec![before.clone()],
        "a pin a standard request renewed reads the flag unset"
    );
    let classified = PinRow {
        last_input_tokens: 600,
        last_classifier: true,
        ..before
    };
    store.upsert_pin(&classified).unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins,
        vec![classified.clone()],
        "a pin a classifier call renewed last reads back with the flag set"
    );
    let renewed = PinRow {
        last_input_tokens: 2_000,
        last_classifier: false,
        ..classified
    };
    store.upsert_pin(&renewed).unwrap();
    assert_eq!(
        store.load_routing().unwrap().pins,
        vec![renewed],
        "an upsert over the row clears the flag a standard renewal clears"
    );
}

#[test]
fn the_plan_ended_flag_is_set_once_cleared_once_and_read_by_the_term_and_the_registry_load() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    for id in ["ended", "plan"] {
        store
            .insert_account(&account(id, AccountKind::Oauth))
            .unwrap();
    }
    let revision = |id: &str| store.account(id).unwrap().unwrap().revision;
    let flag = |id: &str| store.subscription_term(id).unwrap().unwrap().plan_ended_at;
    let before = revision("ended");
    assert!(
        store.set_plan_ended("ended", 5_000).unwrap(),
        "the first no-plan read sets the flag"
    );
    assert!(
        revision("ended") > before,
        "the write bumps the revision, so the next reload publishes it"
    );
    assert!(
        !store.set_plan_ended("ended", 6_000).unwrap(),
        "a flag that stands keeps its instant"
    );
    assert_eq!((flag("ended"), flag("plan")), (Some(5_000), None));
    let loaded = store
        .load_registry()
        .unwrap()
        .terms
        .into_iter()
        .map(|term| (term.account_id, term.plan_ended_at))
        .collect::<Vec<_>>();
    assert_eq!(
        loaded,
        [
            ("ended".to_string(), Some(5_000)),
            ("plan".to_string(), None)
        ],
        "the registry load reads the flag"
    );
    let before = revision("ended");
    assert!(store.clear_plan_ended("ended").unwrap());
    assert!(revision("ended") > before);
    assert!(
        !store.clear_plan_ended("ended").unwrap(),
        "nothing to clear once cleared"
    );
    assert_eq!(flag("ended"), None);
    assert!(matches!(
        store.set_plan_ended("absent", 1),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn a_pool_round_trips_its_slow_down_and_return_to_plan_fields() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let written = PoolRow {
        slow_down: SlowDown {
            slow_down: true,
            slow_down_stretch_seconds: 600,
            slow_down_hold_seconds: 30,
        },
        failover: Failover {
            return_to_plan: ReturnToPlan::Never,
            return_to_plan_dwell_seconds: 0,
            ..Failover::default()
        },
        ..pool("knobs")
    };
    store.insert_pool(&written, "test").unwrap();
    let read = |store: &Store| {
        store
            .load_registry()
            .unwrap()
            .pools
            .into_iter()
            .find(|p| p.id == "knobs")
            .unwrap()
    };
    let created = read(&store);
    assert_eq!(
        (created.slow_down, created.failover),
        (written.slow_down, written.failover),
        "the insert stores the slow-down and the return-to-plan fields"
    );
    let edit = PoolRow {
        slow_down: SlowDown {
            slow_down: false,
            slow_down_stretch_seconds: 18_000,
            slow_down_hold_seconds: 300,
        },
        failover: Failover {
            return_to_plan: ReturnToPlan::WhenCold,
            return_to_plan_dwell_seconds: 86_400,
            ..created.failover
        },
        ..created.clone()
    };
    store
        .update_pool(&edit, created.updated_at, "test")
        .unwrap();
    let edited = read(&store);
    assert_eq!(
        (edited.slow_down, edited.failover),
        (edit.slow_down, edit.failover),
        "the update stores them"
    );
    let five = |diff: &serde_json::Value| {
        [
            "slow_down",
            "slow_down_stretch_seconds",
            "slow_down_hold_seconds",
            "return_to_plan",
            "return_to_plan_dwell_seconds",
        ]
        .map(|name| diff[name].clone())
    };
    let logged: Vec<(String, [serde_json::Value; 5])> = store
        .registry_changes_after(0, 100)
        .unwrap()
        .into_iter()
        .filter(|change| change.target == "knobs")
        .map(|change| {
            let diff: serde_json::Value = serde_json::from_str(&change.diff_json).unwrap();
            (change.kind, five(&diff))
        })
        .collect();
    assert_eq!(
        logged,
        [
            (
                "pool_created".to_string(),
                [
                    serde_json::json!(true),
                    serde_json::json!(600),
                    serde_json::json!(30),
                    serde_json::json!("never"),
                    serde_json::json!(0)
                ]
            ),
            (
                "pool_updated".to_string(),
                [
                    serde_json::json!({ "from": true, "to": false }),
                    serde_json::json!({ "from": 600, "to": 18_000 }),
                    serde_json::json!({ "from": 30, "to": 300 }),
                    serde_json::json!({ "from": "never", "to": "when_cold" }),
                    serde_json::json!({ "from": 0, "to": 86_400 })
                ]
            ),
        ],
        "the change log names the five fields on the create's snapshot and in the edit's pairs"
    );
}

#[test]
fn a_pool_created_without_the_stretch_reads_two_and_a_half_hours() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_pool(
            &PoolRow {
                slow_down: SlowDown::default(),
                ..pool("built")
            },
            "test",
        )
        .unwrap();
    let stretch_of = |store: &Store, id: &str| {
        store
            .pool(id)
            .unwrap()
            .unwrap()
            .slow_down
            .slow_down_stretch_seconds
    };
    assert_eq!(
        stretch_of(&store, "built"),
        9_000,
        "the row builder's default"
    );
}

#[test]
fn the_subscription_is_stored_with_the_profile_and_reads_back_with_nulls_before_the_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    assert_eq!(
        store.subscription("a").unwrap().unwrap(),
        SubscriptionRow::default(),
        "a row never written answers every field None"
    );
    let row = SubscriptionRow {
        plan: Some("max".into()),
        tier: Some("default_claude_max_5x".into()),
        since: Some(1_773_482_400_000),
        trial_ends_at: None,
    };
    store.set_profile("a", "{}", &row).unwrap();
    assert_eq!(store.subscription("a").unwrap().unwrap(), row);
    assert_eq!(store.profile_json("a").unwrap().as_deref(), Some("{}"));
}

#[test]
fn the_canceled_flag_and_its_end_are_the_operators_alone_and_survive_a_profile_write() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let revision = || store.account("a").unwrap().unwrap().revision;
    let before = revision();
    store.set_canceled("a", true, Some(9_000)).unwrap();
    assert!(revision() > before, "the write bumps the revision");
    store
        .set_profile(
            "a",
            "{}",
            &SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_5x".into()),
                since: Some(1_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (term.canceled, term.ends_at, term.since),
        (true, Some(9_000), Some(1_000)),
        "the flag and its end are the operator's alone; the start is the profile's"
    );
    let after_profile = revision();
    store.set_canceled("a", false, Some(9_000)).unwrap();
    assert!(
        revision() > after_profile,
        "clearing the flag bumps the revision too"
    );
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (term.canceled, term.ends_at),
        (false, None),
        "the end is dropped the moment the flag goes off, whatever end was passed"
    );
}

#[test]
fn the_term_classes_a_usage_based_seat_from_its_stored_tier_or_seat_tier_and_never_an_api_key_account(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    for (id, kind) in [
        ("by-tier", AccountKind::Oauth),
        ("by-seat", AccountKind::Oauth),
        ("plan", AccountKind::Oauth),
        ("key", AccountKind::ApiKey),
    ] {
        store.insert_account(&account(id, kind)).unwrap();
    }
    let profile = |id: &str, tier: &str, seat_tier: Option<&str>| {
        let document = serde_json::json!({
            "organization": {"rate_limit_tier": tier, "seat_tier": seat_tier},
        });
        store
            .set_profile(
                id,
                &document.to_string(),
                &SubscriptionRow {
                    tier: Some(tier.into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
    };
    profile("by-tier", "default_claude_zero", None);
    profile(
        "by-seat",
        "default_claude_max_5x",
        Some("enterprise_usage_based"),
    );
    profile("plan", "default_claude_max_5x", Some("standard"));
    // A document naming the class on an api_key row: the class reads the kind first.
    profile("key", "default_claude_zero", Some("enterprise_usage_based"));
    let classes = |store: &Store| {
        let loaded: Vec<(String, bool)> = store
            .load_registry()
            .unwrap()
            .terms
            .into_iter()
            .map(|term| (term.account_id, term.usage_based))
            .collect();
        let read: Vec<(String, bool)> = ["by-seat", "by-tier", "key", "plan"]
            .into_iter()
            .map(|id| {
                let term = store.subscription_term(id).unwrap().unwrap();
                (term.account_id, term.usage_based)
            })
            .collect();
        (loaded, read)
    };
    let expected: Vec<(String, bool)> = [
        ("by-seat", true),
        ("by-tier", true),
        ("key", false),
        ("plan", false),
    ]
    .into_iter()
    .map(|(id, class)| (id.to_string(), class))
    .collect();
    assert_eq!(
        classes(&store),
        (expected.clone(), expected),
        "the tier or the seat tier classes an oauth seat, at the load and through the term read alike"
    );
    // The seat tier alone moves the class: the document changes under the tier the store already held.
    profile(
        "plan",
        "default_claude_max_5x",
        Some("enterprise_usage_based"),
    );
    profile("by-tier", "default_claude_max_5x", None);
    let moved: Vec<(String, bool)> = [
        ("by-seat", true),
        ("by-tier", false),
        ("key", false),
        ("plan", true),
    ]
    .into_iter()
    .map(|(id, class)| (id.to_string(), class))
    .collect();
    assert_eq!(
        classes(&store),
        (moved.clone(), moved),
        "a stored profile moves the class at the next read, with no column of its own"
    );
}

#[test]
fn the_term_resolves_the_plan_from_the_organization_type_first_and_the_tier_second() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let profiles: [(&str, Option<&str>, &str, Option<&str>); 8] = [
        ("free", Some("claude_free"), "default_claude_ai", None),
        (
            "max",
            Some("claude_max"),
            "default_claude_max_unnamed",
            None,
        ),
        (
            "max-20x",
            Some("claude_max"),
            "default_claude_max_20x",
            None,
        ),
        (
            "seat-by-tier",
            Some("claude_free"),
            "default_claude_zero",
            None,
        ),
        (
            "seat-by-seat-tier",
            Some("claude_enterprise"),
            "default_claude_max_5x",
            Some("enterprise_usage_based"),
        ),
        ("untyped-20x", None, "default_claude_max_20x", None),
        ("untyped-unnamed", None, "default_claude_unnamed", None),
        ("untyped-zero", None, "default_claude_zero", None),
    ];
    store
        .insert_account(&account("bare", AccountKind::Oauth))
        .unwrap();
    for (id, organization_type, tier, seat_tier) in profiles {
        store
            .insert_account(&account(id, AccountKind::Oauth))
            .unwrap();
        let document = serde_json::json!({
            "organization": {
                "organization_type": organization_type,
                "rate_limit_tier": tier,
                "seat_tier": seat_tier,
            },
        });
        store
            .set_profile(
                id,
                &document.to_string(),
                &SubscriptionRow {
                    tier: Some(tier.into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
    }
    let expected: Vec<(String, Option<String>)> = [
        ("bare", None),
        ("free", Some("free")),
        ("max", Some("max")),
        ("max-20x", Some("max_20x")),
        ("seat-by-seat-tier", Some("usage_based")),
        ("seat-by-tier", Some("usage_based")),
        ("untyped-20x", Some("max_20x")),
        ("untyped-unnamed", None),
        ("untyped-zero", Some("usage_based")),
    ]
    .into_iter()
    .map(|(id, plan)| (id.to_string(), plan.map(String::from)))
    .collect();
    let loaded: Vec<(String, Option<String>)> = store
        .load_registry()
        .unwrap()
        .terms
        .into_iter()
        .map(|term| (term.account_id, term.plan))
        .collect();
    let read: Vec<(String, Option<String>)> = expected
        .iter()
        .map(|(id, _)| {
            let term = store.subscription_term(id).unwrap().unwrap();
            (term.account_id, term.plan)
        })
        .collect();
    assert_eq!(
        (loaded, read),
        (expected.clone(), expected),
        "the type names the plan before the tier, the class reads above both, and no document reads none, at the \
         load and through the term read alike"
    );
}

#[test]
fn a_pool_reopens_on_and_a_key_reopens_inheriting_until_set_off() {
    let dir = tempfile::tempdir().unwrap();
    let key_store = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key_store).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let desktop = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&desktop, "test").unwrap();
    drop(store);
    let store = Store::open(&path, &key_store).unwrap();
    assert!(store.pool("main").unwrap().unwrap().auto_mode_server);
    let mut read = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(read.auto_mode_server, None, "a new key inherits the pool's");
    read.auto_mode_server = Some(false);
    store
        .update_api_key(&read, read.updated_at, "test")
        .unwrap();
    drop(store);
    let store = Store::open(&path, &key_store).unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().auto_mode_server,
        Some(false)
    );
    let change = store
        .registry_changes_after(0, 100)
        .unwrap()
        .into_iter()
        .rfind(|c| c.kind == "key_updated")
        .unwrap();
    let diff: serde_json::Value = serde_json::from_str(&change.diff_json).unwrap();
    assert_eq!(
        diff["auto_mode_server"],
        serde_json::json!({ "from": null, "to": false })
    );
}

#[test]
fn a_key_created_with_its_own_auto_mode_setting_reopens_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let key_store = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key_store).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    for (id, secret, setting) in [
        ("desktop", "phoebis-desktop-secret", true),
        ("laptop", "phoebis-laptop-secret", false),
    ] {
        let mut row = key(&store, id, "main", secret);
        row.auto_mode_server = Some(setting);
        store.insert_api_key(&row, "test").unwrap();
    }
    drop(store);
    let store = Store::open(&path, &key_store).unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().auto_mode_server,
        Some(true)
    );
    assert_eq!(
        store.api_key("laptop").unwrap().unwrap().auto_mode_server,
        Some(false),
        "an override set at the create is stored as set, on or off"
    );
}

/// Both writes stamp themselves from the same clock, so a delete and the create that follows it can land in
/// one millisecond. The stamps are pinned equal here rather than raced for, so the case the change log's
/// order answers and a comparison of the two clocks does not is the one this test runs every time.
#[test]
fn an_id_created_again_in_the_millisecond_it_was_deleted_in_is_a_reused_one() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    store
        .insert_api_key(
            &key(&store, "back", "main", "phoebis-back-reissued"),
            "test",
        )
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE api_keys SET created_at = (SELECT at FROM registry_changes WHERE target = 'back' AND kind = 'key_deleted' ORDER BY id DESC LIMIT 1) WHERE id = 'back'",
        [],
    )
    .unwrap();

    assert_eq!(
        store.load_registry().unwrap().reused_key_ids,
        vec!["back".to_string()],
        "the id carries a holder before this one whatever the two stamps read"
    );
}

#[test]
fn a_key_created_again_under_a_deleted_id_reads_as_reused_after_the_purge_ages_out_the_delete() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    store
        .insert_api_key(
            &key(&store, "back", "main", "phoebis-back-reissued"),
            "test",
        )
        .unwrap();
    // Every change is past the retention, so only what the purge's keep rule names stays.
    store.purge_history(i64::MAX).unwrap();
    assert_eq!(
        store.load_registry().unwrap().reused_key_ids,
        vec!["back".to_string()]
    );
}

/// A deleted key's memory rows can never be read again, so `delete_api_key` cascades `session_memory` by
/// `key_id` the same way it cascades `pins` rather than leaving them for the retention purge.
#[test]
fn deleting_a_key_drops_its_session_memory_so_a_reissued_id_reads_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    let memory_key = MemoryKey {
        key_id: "back".into(),
        key_generation: 1_000,
        session_id: "s-1".into(),
        agent_id: "main".into(),
    };
    store
        .save(&[(
            memory_key.clone(),
            MemoryRecord {
                record_version: 1,
                payload: "{}".into(),
                last_seen: 1,
            },
        )])
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    assert_eq!(
        MemoryStore::load(&store, &memory_key).unwrap(),
        None,
        "the deleted key's memory goes with the key"
    );
}

#[test]
fn a_key_created_in_its_previous_holders_millisecond_takes_the_next_generation() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "back", "main", "phoebis-back-secret"), "test")
        .unwrap();
    store.delete_api_key("back", "test").unwrap();
    // The first holder's create is moved a minute past the wall clock: the state the second insert sees when
    // it lands in the first's millisecond, pinned rather than raced for.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE registry_changes SET at = at + 60000 WHERE target = 'back' AND kind = 'key_created'",
        [],
    )
    .unwrap();
    let first: i64 = conn
        .query_row(
            "SELECT MAX(at) FROM registry_changes WHERE target = 'back' AND kind = 'key_created'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    store
        .insert_api_key(
            &key(&store, "back", "main", "phoebis-back-reissued"),
            "test",
        )
        .unwrap();
    let second = store.api_key("back").unwrap().unwrap().created_at;
    assert_eq!(second, first + 1, "one millisecond, two generations");
    assert_eq!(
        store.load_registry().unwrap().reused_key_ids,
        vec!["back".to_string()],
        "the change log still names the id as one with an earlier holder"
    );
}

#[test]
fn two_slots_cannot_bind_one_upstream_account() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .insert_account(&account("b", AccountKind::Oauth))
        .unwrap();
    let identity = Identity {
        account_uuid: "uuid-1".into(),
        email: "x@example.invalid".into(),
        organization_uuid: None,
    };
    store.set_identity("a", &identity).unwrap();
    assert!(matches!(
        store.set_identity("b", &identity),
        Err(StoreError::IdentityTaken)
    ));
    store.set_identity("a", &identity).unwrap();
}

#[test]
fn an_account_created_under_a_held_id_answers_id_taken() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let row = account("a", AccountKind::Oauth);
    store.create_account(&row, None, 1, "admin").unwrap();
    let error = store.create_account(&row, None, 2, "admin").unwrap_err();
    assert!(
        matches!(&error, StoreError::IdTaken(id) if id == "a"),
        "{error:?}"
    );
}

#[test]
fn a_key_created_under_a_held_id_answers_id_taken() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "phoebis-first"), "test")
        .unwrap();
    let error = store
        .insert_api_key(&key(&store, "k", "main", "phoebis-second"), "test")
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::IdTaken(id) if id == "k"),
        "{error:?}"
    );
}

#[test]
fn a_pool_created_under_a_held_id_answers_id_taken() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_pool_with_members(&pool("main"), None, "test")
        .unwrap();
    let error = store
        .insert_pool_with_members(&pool("main"), None, "test")
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::IdTaken(id) if id == "main"),
        "{error:?}"
    );
}

#[test]
fn pools_members_and_keys_round_trip_and_a_pool_with_keys_cannot_be_deleted() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[member("a", 2)], "test")
        .unwrap();
    assert_eq!(store.pool_members("main").unwrap()[0].priority, 2);
    assert!(matches!(
        store.set_pool_members("main", &[member("ghost", 0)], "test"),
        Err(StoreError::AccountNotFound(_))
    ));
    let k = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&k, "test").unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().prefix,
        "01234567"
    );
    let pool_rules = vec![
        ModelRule {
            pattern: "claude-opus-5".into(),
            not: false,
        },
        ModelRule {
            pattern: "*".into(),
            not: true,
        },
    ];
    let key_rules = vec![ModelRule {
        pattern: "claude-sonnet-5".into(),
        not: false,
    }];
    store
        .update_pool(
            &PoolRow {
                body_capture_hours: 24,
                strategy: Strategy::Pace,
                window_ceiling: 0.8,
                model_rules: pool_rules.clone(),
                ..pool("main")
            },
            store.pool("main").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                body_capture_hours: Some(6),
                model_rules: key_rules.clone(),
                model_rules_mode: RulesMode::Replace,
                long_context_twins: false,
                ..k.clone()
            },
            store.api_key("desktop").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    let updated = store.pool("main").unwrap().unwrap();
    let updated_key = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(
        (
            updated.body_capture_hours,
            updated.strategy,
            updated.window_ceiling,
            updated.model_rules.clone(),
            updated_key.body_capture_hours,
            updated_key.model_rules.clone(),
            updated_key.model_rules_mode,
            updated_key.long_context_twins,
        ),
        (
            24,
            Strategy::Pace,
            0.8,
            pool_rules,
            Some(6),
            key_rules,
            RulesMode::Replace,
            false,
        )
    );
    assert_eq!(
        store
            .load_registry()
            .unwrap()
            .pools
            .iter()
            .find(|p| p.id == "main")
            .map(|p| (p.strategy, p.window_ceiling)),
        Some((Strategy::Pace, 0.8)),
        "and the load every reader starts from reads both back"
    );
    assert!(matches!(
        store.delete_pool("main", "test"),
        Err(StoreError::PoolInUse(_))
    ));
    store.revoke_api_key("desktop", 5, "test").unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().revoked_at,
        Some(5)
    );
    store.delete_api_key("desktop", "test").unwrap();
    store.delete_pool("main", "test").unwrap();
    assert!(store.pool("main").unwrap().is_none());
    let kinds: Vec<String> = store
        .registry_changes_after(0, 100)
        .unwrap()
        .into_iter()
        .map(|c| c.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "pool_created",
            "pool_members_set",
            "key_created",
            "pool_updated",
            "key_updated",
            "key_revoked",
            "key_deleted",
            "pool_deleted"
        ]
    );
}

#[test]
fn a_new_pool_waits_out_a_throttle_of_up_to_a_minute_by_default() {
    assert_eq!(Failover::default().retry_in_place_seconds, 60);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("router.sqlite"), &StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let pools = store.pools().unwrap();
    let main = pools.iter().find(|p| p.id == "main").unwrap();
    assert_eq!(main.failover.retry_in_place_seconds, 60);
}

#[test]
fn an_api_key_secret_carries_a_generation_that_fences_state_verdicts() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    assert_eq!(store.api_key_credential("a").unwrap().unwrap().1, 1);
    store.set_api_key_secret("a", &Secret::new("k2")).unwrap();
    let (secret, generation) = store.api_key_credential("a").unwrap().unwrap();
    assert_eq!((secret.expose(), generation), ("k2", 2));
    assert_eq!(
        store.credential_generation("a").unwrap(),
        Some(2),
        "the kind-agnostic read answers the same generation"
    );
    assert_eq!(store.credential_generation("nobody").unwrap(), None);
    let before = store.account("a").unwrap().unwrap().revision;
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                1
            )
            .unwrap(),
        None,
        "a verdict about the replaced key changes nothing"
    );
    let row = store.account("a").unwrap().unwrap();
    assert_eq!((row.state, row.revision), (AccountState::Ready, before));
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                2
            )
            .unwrap(),
        Some((before + 1, AccountState::Ready)),
        "the moved row's revision after the write and the state it held before"
    );
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                2
            )
            .unwrap(),
        Some((before + 2, AccountState::ReauthRequired)),
        "a repeated verdict lands under its fences and names the state it found unchanged"
    );
}

#[test]
fn each_state_writer_stores_its_word_beside_the_state() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let read = |id: &str| {
        let row = store.account(id).unwrap().unwrap();
        (row.state, row.state_cause)
    };
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    assert_eq!(read("a"), (AccountState::Ready, StateCause::Operator));
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    assert_eq!(read("a"), (AccountState::Ready, StateCause::Paired));
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                1,
                1,
                2
            )
            .unwrap(),
        None
    );
    assert_eq!(
        read("a"),
        (AccountState::Ready, StateCause::Paired),
        "a write its fences refuse leaves the state and its word"
    );
    assert!(store
        .set_state_if_generations(
            "a",
            AccountState::ReauthRequired,
            StateCause::AuthFailed,
            1,
            1,
            1
        )
        .unwrap()
        .is_some());
    assert_eq!(
        read("a"),
        (AccountState::ReauthRequired, StateCause::AuthFailed)
    );
    let row = store.account("a").unwrap().unwrap();
    store
        .commit_account_update(&AccountUpdate {
            id: "a",
            expected_revision: row.revision,
            incarnation: row.incarnation,
            lifecycle_generation: row.lifecycle_generation,
            label: "a",
            secret: None,
            state: Some((AccountState::Ready, StateCause::Operator)),
            at: 1,
            actor: "admin",
            enabled: None,
            cancellation: None,
        })
        .unwrap();
    assert_eq!(read("a"), (AccountState::Ready, StateCause::Operator));
    store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    let revision = store.account("a").unwrap().unwrap().revision;
    rename(&store, "a", revision, "renamed", "admin").unwrap();
    assert_eq!(
        read("a"),
        (AccountState::Ready, StateCause::Paired),
        "an edit that stages no state leaves the word a pairing stored"
    );
    store.bump_lifecycle("a", StateCause::Operator).unwrap();
    assert_eq!(
        read("a"),
        (AccountState::Disconnected, StateCause::Operator)
    );
    store
        .create_account(
            &AccountRow {
                state: AccountState::Unpaired,
                state_cause: StateCause::Paired,
                ..account("b", AccountKind::Oauth)
            },
            None,
            1,
            "admin",
        )
        .unwrap();
    assert_eq!(
        read("b"),
        (AccountState::Unpaired, StateCause::Paired),
        "the creation writes the row's own word"
    );
}

#[test]
fn set_enabled_leaves_the_state_cause() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    for enabled in [false, true] {
        store.set_enabled("a", enabled).unwrap();
        let row = store.account("a").unwrap().unwrap();
        assert_eq!(
            (row.enabled, row.state, row.state_cause),
            (enabled, AccountState::Ready, StateCause::Paired)
        );
    }
}

#[test]
fn a_row_written_without_the_column_s_word_reads_unrecorded() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    drop(Store::open(&path, &key).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO accounts (id, kind, label, enabled, state, created_at, updated_at) \
         VALUES ('a', 'oauth', 'a', 1, 'ready', 1, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().state_cause,
        StateCause::Unrecorded,
        "the column's default is the word a row written without it reads"
    );
}

#[test]
fn a_stored_word_outside_the_list_fails_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &key).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE accounts SET state_cause = 'nonsense' WHERE id = 'a'",
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &key).unwrap();
    let error = store.account("a").unwrap_err();
    assert!(
        matches!(
            &error,
            StoreError::Sqlite(rusqlite::Error::InvalidColumnType(_, column, _))
                if column == "state_cause"
        ),
        "{error:?}"
    );
}

/// Reads a word as a row on disk holds it, through the type's own column reader.
fn stored_word<T: rusqlite::types::FromSql>(word: &str) -> T {
    rusqlite::Connection::open_in_memory()
        .unwrap()
        .query_row("SELECT ?1", [word], |r| r.get(0))
        .unwrap_or_else(|error| panic!("{word}: {error}"))
}

#[test]
fn every_stored_word_of_the_text_enums_reads_as_its_variant() {
    assert_eq!(stored_word::<AccountKind>("oauth"), AccountKind::Oauth);
    assert_eq!(stored_word::<AccountKind>("api_key"), AccountKind::ApiKey);

    assert_eq!(
        stored_word::<AccountState>("unpaired"),
        AccountState::Unpaired
    );
    assert_eq!(stored_word::<AccountState>("ready"), AccountState::Ready);
    assert_eq!(
        stored_word::<AccountState>("reauth_required"),
        AccountState::ReauthRequired
    );
    assert_eq!(
        stored_word::<AccountState>("funding_required"),
        AccountState::FundingRequired
    );
    assert_eq!(
        stored_word::<AccountState>("disconnected"),
        AccountState::Disconnected
    );

    assert_eq!(
        stored_word::<StateCause>("auth_failed"),
        StateCause::AuthFailed
    );
    assert_eq!(stored_word::<StateCause>("deleted"), StateCause::Deleted);
    assert_eq!(
        stored_word::<StateCause>("funding_refused"),
        StateCause::FundingRefused
    );
    assert_eq!(
        stored_word::<StateCause>("funding_retry"),
        StateCause::FundingRetry
    );
    assert_eq!(stored_word::<StateCause>("operator"), StateCause::Operator);
    assert_eq!(stored_word::<StateCause>("paired"), StateCause::Paired);
    assert_eq!(
        stored_word::<StateCause>("refresh_failed"),
        StateCause::RefreshFailed
    );
    assert_eq!(
        stored_word::<StateCause>("unrecorded"),
        StateCause::Unrecorded
    );

    assert_eq!(stored_word::<Strategy>("pace"), Strategy::Pace);
    assert_eq!(stored_word::<Strategy>("priority"), Strategy::Priority);
    assert_eq!(stored_word::<Strategy>("round_robin"), Strategy::RoundRobin);

    assert_eq!(stored_word::<WarmSessions>("always"), WarmSessions::Always);
    assert_eq!(stored_word::<WarmSessions>("never"), WarmSessions::Never);
    assert_eq!(
        stored_word::<WarmSessions>("when_needed"),
        WarmSessions::WhenNeeded
    );

    assert_eq!(stored_word::<ReturnToPlan>("always"), ReturnToPlan::Always);
    assert_eq!(stored_word::<ReturnToPlan>("never"), ReturnToPlan::Never);
    assert_eq!(
        stored_word::<ReturnToPlan>("when_cold"),
        ReturnToPlan::WhenCold
    );

    assert_eq!(stored_word::<RulesMode>("extend"), RulesMode::Extend);
    assert_eq!(stored_word::<RulesMode>("replace"), RulesMode::Replace);

    assert_eq!(stored_word::<OveragePolicy>("allow"), OveragePolicy::Allow);
    assert_eq!(stored_word::<OveragePolicy>("never"), OveragePolicy::Never);
    assert_eq!(
        stored_word::<OveragePolicy>("prefer_plan"),
        OveragePolicy::PreferPlan
    );

    assert_eq!(stored_word::<AffinityMode>("always"), AffinityMode::Always);
    assert_eq!(
        stored_word::<AffinityMode>("when_cached"),
        AffinityMode::WhenCached
    );

    assert_eq!(
        stored_word::<SubagentPlacement>("by_cohort"),
        SubagentPlacement::ByCohort
    );
    assert_eq!(
        stored_word::<SubagentPlacement>("by_strategy"),
        SubagentPlacement::ByStrategy
    );
    assert_eq!(
        stored_word::<SubagentPlacement>("with_main"),
        SubagentPlacement::WithMain
    );

    assert_eq!(
        stored_word::<SessionPlacement>("by_strategy"),
        SessionPlacement::ByStrategy
    );
    assert_eq!(
        stored_word::<SessionPlacement>("by_warm_prefix"),
        SessionPlacement::ByWarmPrefix
    );

    assert_eq!(stored_word::<BudgetPeriod>("day"), BudgetPeriod::Day);
    assert_eq!(stored_word::<BudgetPeriod>("month"), BudgetPeriod::Month);

    assert_eq!(
        stored_word::<AffinityChain>("message"),
        AffinityChain::Message
    );
    assert_eq!(
        stored_word::<AffinityChain>("static"),
        AffinityChain::Static
    );
}

#[test]
fn two_pool_edits_from_one_read_answer_stale_on_the_second() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let read = store.pool("main").unwrap().unwrap();
    store
        .update_pool(
            &PoolRow {
                label: "First".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert!(matches!(
        store.update_pool(
            &PoolRow {
                label: "Second".into(),
                ..read.clone()
            },
            read.updated_at,
            "test"
        ),
        Err(StoreError::Stale)
    ));
    let first = store.pool("main").unwrap().unwrap();
    assert_eq!(first.label, "First", "the stale edit wrote nothing");
    assert!(
        first.updated_at > read.updated_at,
        "a write inside the stamp's own millisecond still moves it"
    );
    store
        .set_pool_members("main", &[member("a", 0)], "test")
        .unwrap();
    assert!(
        matches!(
            store.update_pool_with_members(
                &PoolRow {
                    label: "Third".into(),
                    ..first.clone()
                },
                first.updated_at,
                Some(&[]),
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "a member write moves the stamp, so an edit read before it cannot land over it"
    );
    assert_eq!(
        (
            store.pool("main").unwrap().unwrap().label,
            store.pool_members("main").unwrap().len()
        ),
        ("First".to_string(), 1),
        "neither half of the stale edit landed"
    );
    assert!(matches!(
        store.update_pool(&pool("ghost"), 1, "test"),
        Err(StoreError::PoolNotFound(_))
    ));
}

/// Accounts `a` and `b`, pools `p1` holding `a` and `p2` holding no one.
fn a_in_p1(store: &Store) {
    for id in ["a", "b"] {
        store
            .insert_account(&account(id, AccountKind::ApiKey))
            .unwrap();
    }
    store.insert_pool(&pool("p1"), "test").unwrap();
    store.insert_pool(&pool("p2"), "test").unwrap();
    store
        .set_pool_members("p1", &[member("a", 0)], "test")
        .unwrap();
}

#[test]
fn a_member_write_naming_an_account_in_another_pool_is_refused_and_writes_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    a_in_p1(&store);
    let before = store.pool("p2").unwrap().unwrap();
    let changes = store.registry_changes_after(0, 100).unwrap();
    let in_p1 = |result: Result<i64, StoreError>| match result {
        Err(StoreError::AccountInPool {
            account_id,
            pool_id,
        }) => (account_id, pool_id) == ("a".to_string(), "p1".to_string()),
        _ => false,
    };
    assert!(
        in_p1(store.set_pool_members("p2", &[member("a", 0)], "test")),
        "the member list's own write"
    );
    assert!(
        in_p1(
            store
                .insert_pool_with_members(&pool("p3"), Some(&[member("a", 0)]), "test")
                .map(|row| row.updated_at)
        ),
        "a create carrying members"
    );
    assert!(
        in_p1(store.update_pool_with_members(
            &PoolRow {
                label: "Renamed".into(),
                ..before.clone()
            },
            before.updated_at,
            Some(&[member("a", 0), member("b", 1)]),
            "test"
        )),
        "an edit carrying members"
    );
    let after = store.pool("p2").unwrap().unwrap();
    assert_eq!(
        (after.label, after.updated_at),
        (before.label, before.updated_at),
        "the refused edit moved neither the settings nor the stamp"
    );
    assert!(store.pool_members("p2").unwrap().is_empty());
    assert!(
        store.pool("p3").unwrap().is_none(),
        "the refused create left no row"
    );
    assert_eq!(
        store
            .pool_members("p1")
            .unwrap()
            .iter()
            .map(|m| m.account_id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(
        store.registry_changes_after(0, 100).unwrap().len(),
        changes.len(),
        "no write recorded a change, a pool_members_set row included"
    );
}

#[test]
fn a_write_that_keeps_an_account_in_its_own_pool_lands() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    a_in_p1(&store);
    store
        .set_pool_members("p1", &[member("a", 2)], "test")
        .unwrap();
    assert_eq!(store.pool_members("p1").unwrap()[0].priority, 2);
    let read = store.pool("p1").unwrap().unwrap();
    store
        .update_pool_with_members(&read, read.updated_at, Some(&[member("a", 5)]), "test")
        .unwrap();
    assert_eq!(store.pool_members("p1").unwrap()[0].priority, 5);
}

/// The list `budgeted_pool` stores: `a` first with a daily budget and `b` second with a monthly one, so a compare
/// reads every column of a member.
fn budgeted_members() -> Vec<MemberWrite> {
    vec![
        MemberWrite {
            budget_daily_usd: Some(5.0),
            ..member("a", 0)
        },
        MemberWrite {
            budget_monthly_usd: Some(50.0),
            ..member("b", 1)
        },
    ]
}

/// Accounts `a` and `b` in pool `main` as `budgeted_members` lists them.
fn budgeted_pool(store: &Store) {
    for id in ["a", "b"] {
        store
            .insert_account(&account(id, AccountKind::ApiKey))
            .unwrap();
    }
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &budgeted_members(), "test")
        .unwrap();
}

fn change_count(store: &Store) -> usize {
    store.registry_changes_after(0, 1_000).unwrap().len()
}

#[test]
fn a_member_list_equal_to_the_stored_one_writes_no_change_row_and_moves_no_stamp() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    budgeted_pool(&store);
    let before = store.pool("main").unwrap().unwrap();
    let changes = change_count(&store);
    let stamp = store
        .set_pool_members("main", &budgeted_members(), "test")
        .unwrap();
    let after = store.pool("main").unwrap().unwrap();
    assert_eq!(
        (stamp, after.updated_at, after.incarnation),
        (before.updated_at, before.updated_at, before.incarnation),
        "the write answers the stamp it read and leaves the row's own"
    );
    assert_eq!(
        change_count(&store),
        changes,
        "the equal list records no change row"
    );
}

#[test]
fn a_member_list_that_differs_in_order_priority_or_budget_writes_the_list_again() {
    let stored = budgeted_members();
    let reordered = vec![stored[1].clone(), stored[0].clone()];
    let mut reprioritized = stored.clone();
    reprioritized[1].priority = 2;
    let mut rebudgeted = stored.clone();
    rebudgeted[0].budget_daily_usd = Some(6.0);
    for (what, list) in [
        ("a reordered list", reordered),
        ("a changed priority", reprioritized),
        ("a changed budget", rebudgeted),
    ] {
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        budgeted_pool(&store);
        let before = store.pool("main").unwrap().unwrap();
        let changes = change_count(&store);
        let stamp = store.set_pool_members("main", &list, "test").unwrap();
        assert!(stamp > before.updated_at, "{what} moves the stamp");
        assert_eq!(
            store.pool("main").unwrap().unwrap().updated_at,
            stamp,
            "{what}"
        );
        assert_eq!(
            change_count(&store),
            changes + 1,
            "{what} records one change row"
        );
        assert_eq!(last_change(&store, "main").0, "pool_members_set", "{what}");
    }
}

#[test]
fn a_pool_save_that_changes_the_member_list_alone_records_the_list_and_no_pool_edit_and_moves_the_stamp(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    budgeted_pool(&store);
    let read = store.pool("main").unwrap().unwrap();
    let stored = budgeted_members();
    let reordered = vec![stored[1].clone(), stored[0].clone()];
    let start = store.newest_registry_change_id().unwrap().unwrap_or(0);
    let stamp = store
        .update_pool_with_members(&read, read.updated_at, Some(&reordered), "test")
        .unwrap();
    assert!(stamp > read.updated_at, "the list's write moves the stamp");
    assert_eq!(store.pool("main").unwrap().unwrap().updated_at, stamp);
    let kinds: Vec<(String, String)> = store
        .registry_changes_after(start, 100)
        .unwrap()
        .into_iter()
        .map(|change| (change.kind, change.target))
        .collect();
    assert_eq!(
        kinds,
        [("pool_members_set".to_string(), "main".to_string())],
        "the unchanged row's pairs are empty, so the save records the list's row alone"
    );
}

#[test]
fn a_pool_save_that_changes_no_field_no_policy_no_member_and_no_guardrail_writes_nothing_and_answers_the_stamp_it_was_given(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    budgeted_pool(&store);
    let read = store.pool("main").unwrap().unwrap();
    let changes = change_count(&store);
    let stamp = store
        .update_pool_with_members_and_guardrails(
            &read,
            read.updated_at,
            Some(&budgeted_members()),
            Some(&[]),
            "test",
        )
        .unwrap();
    assert_eq!(stamp, read.updated_at, "the stamp the save was given");
    assert_eq!(
        format!("{:?}", store.pool("main").unwrap().unwrap()),
        format!("{read:?}"),
        "the row as it was read, its stamp and incarnation among it"
    );
    assert_eq!(
        change_count(&store),
        changes,
        "neither the pool's edit nor its member list records a change row"
    );
}

#[test]
fn a_pool_save_whose_keepalive_policy_differs_in_key_order_and_whitespace_alone_writes_nothing_and_answers_the_stamp_it_was_given(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_pool(
            &PoolRow {
                keepalive_json: r#"{"enabled":true,"max_idle_seconds":43200}"#.into(),
                ..pool("main")
            },
            "test",
        )
        .unwrap();
    let read = store.pool("main").unwrap().unwrap();
    let changes = change_count(&store);
    let stamp = store
        .update_pool(
            &PoolRow {
                keepalive_json: r#"{ "max_idle_seconds": 43200, "enabled": true }"#.into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(stamp, read.updated_at, "the stamp the save was given");
    assert_eq!(
        format!("{:?}", store.pool("main").unwrap().unwrap()),
        format!("{read:?}"),
        "the row as it was read, its policy text among it"
    );
    assert_eq!(
        change_count(&store),
        changes,
        "the same policy in another spelling records no change row"
    );
}

#[test]
fn a_pool_save_that_changes_nothing_under_a_stale_stamp_or_another_incarnation_is_refused_as_stale()
{
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    budgeted_pool(&store);
    let first = store.pool("main").unwrap().unwrap();
    store
        .update_pool(
            &PoolRow {
                label: "Renamed".into(),
                ..first.clone()
            },
            first.updated_at,
            "test",
        )
        .unwrap();
    let read = store.pool("main").unwrap().unwrap();
    let changes = change_count(&store);
    assert!(
        matches!(
            store.update_pool_with_members(
                &read,
                first.updated_at,
                Some(&budgeted_members()),
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "a stamp a later edit moved past"
    );
    assert!(
        matches!(
            store.update_pool_with_members(
                &PoolRow {
                    incarnation: read.incarnation + 1,
                    ..read.clone()
                },
                read.updated_at,
                Some(&budgeted_members()),
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "a row read from another incarnation of the pool"
    );
    assert_eq!(
        format!("{:?}", store.pool("main").unwrap().unwrap()),
        format!("{read:?}")
    );
    assert_eq!(change_count(&store), changes);
}

#[test]
fn a_pool_save_that_changes_the_keepalive_policy_alone_writes_one_pool_edit_naming_the_member_it_moved_and_moves_the_stamp(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_pool(
            &PoolRow {
                keepalive_json: r#"{"enabled":true,"max_idle_seconds":43200,"max_parked_seconds":null,"scheduled_horizon_seconds":null}"#.into(),
                ..pool("main")
            },
            "test",
        )
        .unwrap();
    let read = store.pool("main").unwrap().unwrap();
    let changes = change_count(&store);
    let policy = r#"{"enabled":true,"max_idle_seconds":86400,"max_parked_seconds":null,"scheduled_horizon_seconds":null}"#;
    let stamp = store
        .update_pool(
            &PoolRow {
                keepalive_json: policy.into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert!(
        stamp > read.updated_at,
        "the policy's write moves the stamp"
    );
    let after = store.pool("main").unwrap().unwrap();
    assert_eq!(
        (after.keepalive_json.as_str(), after.updated_at),
        (policy, stamp)
    );
    assert_eq!(change_count(&store), changes + 1);
    assert_eq!(
        last_change(&store, "main"),
        (
            "pool_updated".to_string(),
            serde_json::json!({
                "keepalive.max_idle_seconds": { "from": 43200, "to": 86400 },
            })
        ),
        "the one policy member the save moved, under its dotted name"
    );
}

#[test]
fn a_pool_created_with_a_keepalive_policy_lists_each_member_under_its_dotted_name_in_its_snapshot()
{
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_pool(
            &PoolRow {
                keepalive_json: r#"{"enabled":true,"max_idle_seconds":43200,"max_parked_seconds":7200,"scheduled_horizon_seconds":1800}"#.into(),
                ..pool("main")
            },
            "test",
        )
        .unwrap();
    let (kind, diff) = last_change(&store, "main");
    let policy: serde_json::Map<String, serde_json::Value> = diff
        .as_object()
        .unwrap()
        .iter()
        .filter(|(field, _)| field.starts_with("keepalive."))
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect();
    assert_eq!(
        (kind.as_str(), serde_json::Value::Object(policy)),
        (
            "pool_created",
            serde_json::json!({
                "keepalive.enabled": true,
                "keepalive.max_idle_seconds": 43200,
                "keepalive.max_parked_seconds": 7200,
                "keepalive.scheduled_horizon_seconds": 1800,
            })
        ),
        "the snapshot names each member of the stored policy with its value"
    );
}

#[test]
fn a_first_save_over_a_pool_stored_with_an_empty_policy_names_each_member_it_gave_a_value_from_null(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let read = store.pool("main").unwrap().unwrap();
    assert_eq!(read.keepalive_json, "{}");
    // The whole policy the admin route writes once a member moves off the defaults it reads `{}` as.
    let policy = r#"{"enabled":false,"max_idle_seconds":86400,"max_parked_seconds":null,"scheduled_horizon_seconds":null}"#;
    store
        .update_pool(
            &PoolRow {
                keepalive_json: policy.into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(
        last_change(&store, "main"),
        (
            "pool_updated".to_string(),
            serde_json::json!({
                "keepalive.enabled": { "from": null, "to": false },
                "keepalive.max_idle_seconds": { "from": null, "to": 86400 },
            })
        ),
        "each member the save gave a value, from null, and no pair for a null beside a missing member"
    );
}

#[test]
fn a_pool_created_with_an_empty_member_list_records_no_member_row_and_one_created_with_a_member_records_both(
) {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    let start = store.newest_registry_change_id().unwrap().unwrap_or(0);
    store
        .insert_pool_with_members(&pool("empty"), Some(&[]), "test")
        .unwrap();
    store
        .insert_pool_with_members(&pool("held"), Some(&[member("a", 0)]), "test")
        .unwrap();
    let kinds: Vec<(String, String)> = store
        .registry_changes_after(start, 100)
        .unwrap()
        .into_iter()
        .map(|change| (change.kind, change.target))
        .collect();
    assert_eq!(
        kinds,
        [
            ("pool_created".to_string(), "empty".to_string()),
            ("pool_created".to_string(), "held".to_string()),
            ("pool_members_set".to_string(), "held".to_string()),
        ]
    );
}

#[test]
fn the_index_refuses_a_direct_insert_the_check_would_have_caught() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    a_in_p1(&store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let refused = conn
        .execute(
            "INSERT INTO pool_members (pool_id, account_id, priority, budget_daily_usd, budget_monthly_usd) \
             VALUES ('p2', 'a', 0, NULL, NULL)",
            [],
        )
        .unwrap_err();
    assert!(
        matches!(
            &refused,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
        ),
        "{refused:?}"
    );
}

#[test]
fn two_key_edits_from_one_read_answer_stale_on_the_second() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-0123456789abcdef"),
            "test",
        )
        .unwrap();
    let read = store.api_key("desktop").unwrap().unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                label: "First".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert!(matches!(
        store.update_api_key(
            &ApiKeyRow {
                label: "Second".into(),
                ..read.clone()
            },
            read.updated_at,
            "test"
        ),
        Err(StoreError::Stale)
    ));
    let first = store.api_key("desktop").unwrap().unwrap();
    assert_eq!(first.label, "First", "the stale edit wrote nothing");
    assert!(
        first.updated_at > read.updated_at,
        "a write inside the stamp's own millisecond still moves it"
    );
    store.revoke_api_key("desktop", 5, "test").unwrap();
    assert!(
        matches!(
            store.update_api_key(&first, first.updated_at, "test"),
            Err(StoreError::Stale)
        ),
        "an edit read before the revocation cannot switch the revoked key back on"
    );
    assert!(!store.api_key("desktop").unwrap().unwrap().enabled);
    assert!(matches!(
        store.update_api_key(&key(&store, "ghost", "main", "phoebis-ghost"), 0, "test"),
        Err(StoreError::KeyNotFound(_))
    ));
}

/// The replacement's stamp is pinned to its predecessor's rather than raced for, so the two rows differ in their
/// incarnation alone and the token is what tells them apart.
#[test]
fn a_pool_deleted_and_created_again_inside_one_millisecond_refuses_the_predecessors_token() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    let first = store.insert_pool(&pool("main"), "test").unwrap();
    assert_eq!(
        first.incarnation, 1,
        "a new id starts at the first incarnation"
    );
    let token = stamp_token(first.incarnation, first.updated_at);
    store.delete_pool("main", "test").unwrap();
    let second = store.insert_pool(&pool("main"), "test").unwrap();
    assert_eq!(
        second.incarnation, 2,
        "the insert answers the incarnation it wrote"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE pools SET updated_at = ?1 WHERE id = 'main'",
        [first.updated_at],
    )
    .unwrap();
    drop(conn);
    let read = store.pool("main").unwrap().unwrap();
    assert_eq!((read.incarnation, read.updated_at), (2, first.updated_at));
    assert_ne!(stamp_token(read.incarnation, read.updated_at), token);
    assert!(
        !answers_stamp_token(read.incarnation, read.updated_at, token),
        "the replacement never answers its predecessor's token"
    );
    assert!(
        matches!(
            store.update_pool(
                &PoolRow {
                    label: "Stale".into(),
                    ..first.clone()
                },
                first.updated_at,
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "an edit read before the deletion carries the predecessor's incarnation"
    );
    assert_eq!(store.pool("main").unwrap().unwrap().label, "main");
    store
        .update_pool(
            &PoolRow {
                label: "Fresh".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(store.pool("main").unwrap().unwrap().label, "Fresh");
}

#[test]
fn a_key_deleted_and_created_again_inside_one_millisecond_refuses_the_predecessors_token() {
    let dir = tempfile::tempdir().unwrap();
    let storage_key = StorageKey::generate();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &storage_key).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let first = store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-desktop-one"),
            "test",
        )
        .unwrap();
    assert_eq!(
        first.incarnation, 1,
        "a new id starts at the first incarnation"
    );
    let token = stamp_token(first.incarnation, first.updated_at);
    store.delete_api_key("desktop", "test").unwrap();
    let second = store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-desktop-two"),
            "test",
        )
        .unwrap();
    assert_eq!(
        second.incarnation, 2,
        "the insert answers the incarnation it wrote"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE api_keys SET updated_at = ?1 WHERE id = 'desktop'",
        [first.updated_at],
    )
    .unwrap();
    drop(conn);
    let read = store.api_key("desktop").unwrap().unwrap();
    assert_eq!((read.incarnation, read.updated_at), (2, first.updated_at));
    assert_ne!(stamp_token(read.incarnation, read.updated_at), token);
    assert!(
        !answers_stamp_token(read.incarnation, read.updated_at, token),
        "the replacement never answers its predecessor's token"
    );
    assert!(
        matches!(
            store.update_api_key(
                &ApiKeyRow {
                    label: "Stale".into(),
                    ..first.clone()
                },
                first.updated_at,
                "test"
            ),
            Err(StoreError::Stale)
        ),
        "an edit read before the deletion carries the predecessor's incarnation"
    );
    assert_eq!(store.api_key("desktop").unwrap().unwrap().label, "desktop");
    store
        .update_api_key(
            &ApiKeyRow {
                label: "Fresh".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(store.api_key("desktop").unwrap().unwrap().label, "Fresh");
}

#[test]
fn a_stale_account_update_carrying_enabled_and_a_cancellation_writes_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let read = store.account("a").unwrap().unwrap();
    rename(&store, "a", read.revision, "Moved", "admin").unwrap();
    let stale = AccountUpdate {
        id: "a",
        expected_revision: read.revision,
        incarnation: read.incarnation,
        lifecycle_generation: read.lifecycle_generation,
        label: "Stale",
        secret: None,
        state: None,
        at: 5,
        actor: "admin",
        enabled: Some(false),
        cancellation: Some((true, Some(9_000))),
    };
    assert!(matches!(
        store.commit_account_update(&stale),
        Err(StoreError::Stale)
    ));
    let row = store.account("a").unwrap().unwrap();
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (row.label.as_str(), row.enabled, term.canceled, term.ends_at),
        ("Moved", true, false, None),
        "no staged field of the stale edit landed"
    );
    store
        .commit_account_update(&AccountUpdate {
            expected_revision: row.revision,
            ..stale
        })
        .unwrap();
    let landed = store.account("a").unwrap().unwrap();
    let term = store.subscription_term("a").unwrap().unwrap();
    assert_eq!(
        (
            landed.label.as_str(),
            landed.enabled,
            landed.state,
            term.canceled,
            term.ends_at,
            landed.revision
        ),
        (
            "Stale",
            false,
            AccountState::Ready,
            true,
            Some(9_000),
            row.revision + 1
        ),
        "the same edit built from the moved row lands every staged field in one revision"
    );
}

/// An api-key account holding a secret at generation 1, as every fresh slot under an id does.
fn insert_ready_api_key_account(store: &Store, id: &str) {
    store
        .insert_account(&account(id, AccountKind::ApiKey))
        .unwrap();
    store.set_api_key_secret(id, &Secret::new("k1")).unwrap();
}

fn seeded_api_key_account(id: &str) -> Store {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    insert_ready_api_key_account(&store, id);
    store
}

#[test]
fn a_state_verdict_from_a_previous_incarnation_changes_nothing() {
    let store = seeded_api_key_account("a");
    let (incarnation, lifecycle, credential) = (1, 1, 1);
    store.delete_account("a", "test").unwrap();
    insert_ready_api_key_account(&store, "a");
    let moved = store
        .set_state_if_generations(
            "a",
            AccountState::FundingRequired,
            StateCause::FundingRefused,
            incarnation,
            lifecycle,
            credential,
        )
        .unwrap();
    assert_eq!(moved, None, "the verdict named a holder that is gone");
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(row.state, AccountState::Ready);
    assert_eq!(
        store
            .set_state_if_generations(
                "a",
                AccountState::FundingRequired,
                StateCause::FundingRefused,
                2,
                1,
                1
            )
            .unwrap(),
        Some((row.revision + 1, AccountState::Ready)),
        "the holder the tombstone numbered 2 takes its own verdicts"
    );
}

#[test]
fn an_account_update_commits_its_secret_and_label_together_or_not_at_all() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k1")),
            1,
            "admin",
        )
        .unwrap();
    assert_eq!(store.api_key_credential("a").unwrap().unwrap().1, 1);
    let update = AccountUpdate {
        id: "a",
        expected_revision: 1,
        incarnation: 1,
        lifecycle_generation: 1,
        label: "Renamed",
        secret: Some(&Secret::new("k2")),
        state: Some((AccountState::Ready, StateCause::Operator)),
        at: 5,
        actor: "admin",
        enabled: None,
        cancellation: None,
    };
    store.commit_account_update(&update).unwrap();
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(row.label.as_str(), "Renamed");
    assert_eq!(
        store.api_key_credential("a").unwrap().unwrap().0.expose(),
        "k2"
    );
    let stale = AccountUpdate {
        lifecycle_generation: 7,
        label: "Never",
        ..update
    };
    assert!(matches!(
        store.commit_account_update(&stale),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        store.account("a").unwrap().unwrap().label,
        "Renamed",
        "a stale update writes nothing"
    );
    // The commit above moved the revision, so the same edit verified against the row before it is now about
    // an account whose endpoint and secret nobody checked together.
    let superseded = AccountUpdate {
        label: "Never",
        ..update
    };
    assert!(matches!(
        store.commit_account_update(&superseded),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        (
            store.account("a").unwrap().unwrap().label,
            store.api_key_credential("a").unwrap().unwrap().0.expose()
        ),
        ("Renamed".to_string(), "k2"),
        "an edit verified against a superseded revision writes nothing"
    );
}

#[test]
fn a_recreated_account_never_answers_an_edit_token_its_predecessor_answered() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(&account("a", AccountKind::Oauth), None, 1, "admin")
        .unwrap();
    let mut answered = Vec::new();
    for label in ["One", "Two"] {
        let row = store.account("a").unwrap().unwrap();
        answered.push(row.edit_token());
        rename(&store, "a", row.revision, label, "admin").unwrap();
    }
    answered.push(store.account("a").unwrap().unwrap().edit_token());
    store.delete_account("a", "admin").unwrap();
    store
        .create_account(&account("a", AccountKind::Oauth), None, 2, "admin")
        .unwrap();
    for label in ["One", "Two", "Three"] {
        let row = store.account("a").unwrap().unwrap();
        assert!(
            !answered.contains(&row.edit_token()),
            "revision {} of the replacement answers a token its predecessor answered",
            row.revision
        );
        rename(&store, "a", row.revision, label, "admin").unwrap();
    }
}

/// The bundle a pairing's confirm hands the store, with tokens that are obviously not real.
fn paired_bundle() -> NewBundle {
    NewBundle {
        access_token: Secret::new("at-test-not-a-token"),
        refresh_token: Some(Secret::new("rt-test-not-a-token")),
        issued_at: 1_799_996_400_000,
        expires_at: 1_800_000_000_000,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    }
}

/// The row a confirm builds for a pairing started before its row existed: bound to `account_uuid` and born paired.
fn paired_row(id: &str, account_uuid: &str) -> AccountRow {
    AccountRow {
        identity: Some(Identity {
            account_uuid: account_uuid.into(),
            email: format!("{id}@example.invalid"),
            organization_uuid: None,
        }),
        state_cause: StateCause::Paired,
        ..account(id, AccountKind::Oauth)
    }
}

#[test]
fn a_paired_account_is_born_ready_with_its_credential_attempt_and_change_in_one_write() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let subscription = SubscriptionRow {
        plan: Some("max".into()),
        tier: Some("default_claude_max_5x".into()),
        since: Some(1_773_482_400_000),
        trial_ends_at: None,
    };
    let profile = r#"{"account":{"uuid":"00000000-0000-4000-8000-000000000001"}}"#;
    let generation = store
        .create_paired_account(
            &paired_row("fresh", "00000000-0000-4000-8000-000000000001"),
            &paired_bundle(),
            profile,
            &subscription,
            5,
            "admin",
        )
        .unwrap();
    assert_eq!(generation, 1);
    let row = store.account("fresh").unwrap().unwrap();
    assert_eq!(
        (
            row.state,
            row.state_cause,
            row.incarnation,
            row.lifecycle_generation,
            row.revision
        ),
        (AccountState::Ready, StateCause::Paired, 1, 1, 1)
    );
    assert_eq!(
        row.identity.as_ref().map(|i| i.account_uuid.as_str()),
        Some("00000000-0000-4000-8000-000000000001")
    );
    let credential = store.credentials("fresh").unwrap().unwrap();
    assert_eq!(
        (credential.generation, credential.lifecycle_generation),
        (1, 1)
    );
    assert_eq!(credential.access_token.expose(), "at-test-not-a-token");
    let attempts = store
        .newest_resolved_refresh_attempts("fresh", 1, 10)
        .unwrap();
    assert_eq!(attempts.len(), 1, "the pairing's own resolved attempt");
    assert_eq!(
        (
            attempts[0].outcome.as_str(),
            attempts[0].detail.as_deref(),
            attempts[0].credential_generation,
            attempts[0].resolved_at
        ),
        ("committed", Some("pairing"), 1, 5)
    );
    let changes = store.registry_changes_after(0, 10).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        (
            changes[0].kind.as_str(),
            changes[0].target.as_str(),
            changes[0].actor.as_str(),
            changes[0].at
        ),
        ("account_created", "fresh", "admin", 5)
    );
    assert_eq!(
        store.profile_json("fresh").unwrap().as_deref(),
        Some(profile),
        "the profile lands in the same write"
    );
    let term = store.subscription_term("fresh").unwrap().unwrap();
    assert_eq!(
        (term.since, term.trial_ends_at),
        (Some(1_773_482_400_000), None)
    );

    let unchanged = |store: &Store| {
        (
            store.accounts().unwrap(),
            store.credentials("fresh").unwrap().map(|c| c.generation),
            store
                .newest_resolved_refresh_attempts("fresh", 1, 10)
                .unwrap()
                .len(),
            store.registry_changes_after(0, 10).unwrap().len(),
        )
    };
    let before = unchanged(&store);
    let same_id = store.create_paired_account(
        &paired_row("fresh", "00000000-0000-4000-8000-000000000009"),
        &paired_bundle(),
        "{}",
        &subscription,
        6,
        "admin",
    );
    assert!(
        matches!(&same_id, Err(StoreError::IdTaken(id)) if id == "fresh"),
        "a taken id answers as one: {same_id:?}"
    );
    let same_identity = store.create_paired_account(
        &paired_row("second", "00000000-0000-4000-8000-000000000001"),
        &paired_bundle(),
        "{}",
        &subscription,
        7,
        "admin",
    );
    assert!(
        matches!(same_identity, Err(StoreError::IdentityTaken)),
        "an identity another row holds is named: {same_identity:?}"
    );
    assert!(store.account("second").unwrap().is_none());
    assert!(store.credentials("second").unwrap().is_none());
    assert_eq!(
        unchanged(&store),
        before,
        "neither refused write left anything behind"
    );
}

#[test]
fn an_edit_token_too_wide_for_a_console_number_is_refused_by_its_own_row() {
    let row = AccountRow {
        incarnation: 1 << 21,
        ..account("a", AccountKind::Oauth)
    };
    assert!(
        row.edit_token() < 1 << 53,
        "the token stays an integer a console number holds exactly"
    );
    assert!(
        !row.answers_edit_token(row.edit_token()),
        "a token that cannot name the row's incarnation fences nothing onto it"
    );
    // A pool's or a key's token packs the incarnation above the stamp's 42 bits, which hold every stamp to 2109.
    let widest = (1 << 42) - 1;
    assert!(
        stamp_token((1 << 11) - 1, widest) < 1 << 53 && stamp_token(1 << 11, widest) < 1 << 53,
        "the stamp token stays an integer a console number holds exactly"
    );
    assert!(answers_stamp_token(
        (1 << 11) - 1,
        widest,
        stamp_token((1 << 11) - 1, widest)
    ));
    assert!(
        !answers_stamp_token(1 << 11, widest, stamp_token(1 << 11, widest)),
        "a stamp token that cannot name the row's incarnation fences nothing onto it"
    );
}

#[test]
fn an_account_update_read_from_a_deleted_account_is_stale_on_the_one_that_took_its_id() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k1")),
            1,
            "admin",
        )
        .unwrap();
    let read = store.account("a").unwrap().unwrap();
    store.delete_account("a", "admin").unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k2")),
            2,
            "admin",
        )
        .unwrap();
    let replacement = store.account("a").unwrap().unwrap();
    assert_eq!(
        (replacement.revision, replacement.lifecycle_generation),
        (read.revision, read.lifecycle_generation),
        "the replacement restarts both counters the read carried"
    );
    let stale = AccountUpdate {
        id: "a",
        expected_revision: read.revision,
        incarnation: read.incarnation,
        lifecycle_generation: read.lifecycle_generation,
        label: "Stale",
        secret: Some(&Secret::new("k3")),
        state: Some((AccountState::Ready, StateCause::Operator)),
        at: 5,
        actor: "admin",
        enabled: Some(false),
        cancellation: None,
    };
    assert!(matches!(
        store.commit_account_update(&stale),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        (
            store.account("a").unwrap().unwrap(),
            store
                .api_key_credential("a")
                .unwrap()
                .unwrap()
                .0
                .expose()
                .to_string()
        ),
        (replacement.clone(), "k2".to_string()),
        "no staged field of the edit read from the deleted account landed"
    );
    store
        .commit_account_update(&AccountUpdate {
            incarnation: replacement.incarnation,
            ..stale
        })
        .unwrap();
    assert_eq!(
        store.account("a").unwrap().unwrap().label,
        "Stale",
        "the same edit built from the replacement lands"
    );
}

#[test]
fn every_write_that_changes_a_dispatch_input_moves_the_account_revision() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    let revision = || store.account("a").unwrap().unwrap().revision;
    assert_eq!(
        (revision(), store.account_revision("a").unwrap()),
        (1, Some(1))
    );
    rename(&store, "a", 1, "Renamed", "admin").unwrap();
    assert_eq!(revision(), 2, "a label update");
    store.set_enabled("a", false).unwrap();
    assert_eq!(revision(), 3, "an enable flip");
    store.set_api_key_secret("a", &Secret::new("k1")).unwrap();
    assert_eq!(revision(), 4, "a secret rotation");
    store
        .set_state("a", AccountState::FundingRequired, StateCause::Operator)
        .unwrap();
    assert_eq!(revision(), 5, "a state change");
    assert_eq!(store.account_revision("nobody").unwrap(), None);
}

#[test]
fn deleting_an_account_cascades_and_the_load_reads_everything() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[member("a", 0)], "test")
        .unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!(
        (load.accounts.len(), load.pools.len(), load.members.len()),
        (1, 1, 1)
    );
    store.delete_account("a", "test").unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!((load.accounts.len(), load.members.len()), (0, 0));
    assert!(matches!(
        store.delete_account("a", "test"),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn the_newest_change_id_is_the_mirrors_starting_watermark() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    assert_eq!(store.newest_registry_change_id().unwrap(), None);
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    rename(&store, "a", 1, "first", "admin").unwrap();
    rename(&store, "a", 2, "second", "admin").unwrap();
    let newest = store.newest_registry_change_id().unwrap().unwrap();
    let changes = store.registry_changes_after(0, 100).unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        newest,
        changes.iter().map(|c| c.id).max().unwrap(),
        "the watermark is the largest id the log holds"
    );
    assert!(
        store
            .registry_changes_after(newest, 100)
            .unwrap()
            .is_empty(),
        "so a mirror starting there replays nothing"
    );
}

#[test]
fn every_key_write_stamps_updated_at_and_a_use_leaves_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store.insert_pool(&pool("p"), "test").unwrap();
    let created_at = store
        .insert_api_key(&key(&store, "k", "p", "secret-one"), "test")
        .unwrap()
        .created_at;
    let stamped = || store.api_key("k").unwrap().unwrap().updated_at;
    assert_eq!(stamped(), created_at, "the create dates the row");
    // Cleared beside the store between the writes, so each write has to stamp the row itself.
    let raw = rusqlite::Connection::open(&path).unwrap();
    let clear = || {
        raw.execute("UPDATE api_keys SET updated_at = 0", [])
            .unwrap();
    };
    clear();
    store.touch_api_key("k", 5, false, None).unwrap();
    assert_eq!(stamped(), 0, "a use is no change to the row");
    let row = store.api_key("k").unwrap().unwrap();
    let read_at = row.updated_at;
    let answered = store
        .update_api_key(
            &ApiKeyRow {
                label: "renamed".into(),
                ..row
            },
            read_at,
            "test",
        )
        .unwrap();
    assert_ne!(answered, 0);
    assert_eq!(stamped(), answered, "an edit answers the stamp it wrote");
    clear();
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("secret-two"),
            &phoebis_store::key_prefix("secret-two"),
            7,
            "test",
        )
        .unwrap();
    assert_ne!(stamped(), 0, "a rotation stamps the row");
    clear();
    store.revoke_api_key("k", 9, "test").unwrap();
    assert_ne!(stamped(), 0, "a revocation stamps the row");
}

#[test]
fn two_key_updates_inside_one_millisecond_read_two_different_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store.insert_pool(&pool("p"), "test").unwrap();
    let created_at = store
        .insert_api_key(&key(&store, "k", "p", "secret-one"), "test")
        .unwrap()
        .created_at;
    // A stored stamp an hour past the clock is the state several writes inside one millisecond leave: the clock
    // has not moved past the newest stamp, so each write has to step past it to read as a change.
    let ahead = created_at + 3_600_000;
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE api_keys SET updated_at = ?1", [ahead])
        .unwrap();
    let read = store.api_key("k").unwrap().unwrap();
    let first = store
        .update_api_key(
            &ApiKeyRow {
                label: "first".into(),
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    let moved = store.api_key("k").unwrap().unwrap();
    let second = store
        .update_api_key(
            &ApiKeyRow {
                label: "second".into(),
                ..moved.clone()
            },
            moved.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(
        (first, second),
        (ahead + 1, ahead + 2),
        "each update steps one past the stamp it fenced on"
    );
    assert_eq!(store.api_key("k").unwrap().unwrap().updated_at, second);
    assert!(
        matches!(
            store.update_api_key(&moved, moved.updated_at, "test"),
            Err(StoreError::Stale)
        ),
        "an edit read between the two cannot pass the fence"
    );
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("secret-two"),
            &phoebis_store::key_prefix("secret-two"),
            7,
            "test",
        )
        .unwrap();
    assert_eq!(
        store.api_key("k").unwrap().unwrap().updated_at,
        ahead + 3,
        "a rotation steps the same way"
    );
    store.revoke_api_key("k", 9, "test").unwrap();
    assert_eq!(
        store.api_key("k").unwrap().unwrap().updated_at,
        ahead + 4,
        "and so does a revocation"
    );
}

#[test]
fn the_history_purge_cuts_old_resolved_attempts_and_changes_and_keeps_what_their_readers_need() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::Oauth))
        .unwrap();
    let cutoff = 1_000_000;
    {
        let mut raw = rusqlite::Connection::open(&path).unwrap();
        let tx = raw.transaction().unwrap();
        let attempt = |account_id: &str, resolved_at: Option<i64>, outcome: Option<&str>| {
            tx.execute(
                "INSERT INTO refresh_attempts (account_id, incarnation, lifecycle_generation, \
                 credential_generation, started_at, resolved_at, outcome) VALUES (?1, 1, 1, 1, ?2, ?3, ?4)",
                rusqlite::params![account_id, resolved_at.unwrap_or(10), resolved_at, outcome],
            )
            .unwrap();
        };
        for i in 0..2_500 {
            let outcome = if i % 2 == 0 { "failed" } else { "committed" };
            attempt("a", Some(1_000 + i), Some(outcome));
        }
        // Still open: the startup quarantine reads it, whatever its age.
        attempt("a", Some(500), Some("uncertain"));
        attempt("c", None, None);
        // A holder no refresh touched since: its newest two are what the attention reader and the account
        // view read.
        for resolved_at in [100, 200, 300] {
            attempt("b", Some(resolved_at), Some("failed"));
        }
        // Two uncertain attempts a later sweep settled after the holder's newest one: the newest two by settling
        // are the older pair, and the account view's last attempt reads the newest by id.
        for resolved_at in [400, 500, 150] {
            attempt("d", Some(resolved_at), Some("failed"));
        }
        let change = |at: i64, kind: &str, target: &str| {
            tx.execute(
                "INSERT INTO registry_changes (at, actor, kind, target, diff_json) VALUES (?1, 'test', ?2, ?3, '{}')",
                rusqlite::params![at, kind, target],
            )
            .unwrap();
        };
        for i in 0..1_500 {
            change(1_000 + i, "pool_updated", "p");
        }
        // A reused id: the newest create stamps the next create's generation past it, and the deletes age out.
        change(10, "key_created", "gone");
        change(11, "key_deleted", "gone");
        change(12, "key_created", "gone");
        change(13, "key_deleted", "gone");
        tx.commit().unwrap();
    }
    for _ in 0..2 {
        let id = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
        store
            .resolve_refresh_attempt(id, 1, phoebis_store::AttemptOutcome::Failed("x".into()))
            .unwrap();
    }
    let watermark = store.newest_registry_change_id().unwrap().unwrap();
    store.insert_pool(&pool("p"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "k", "p", "secret"), "test")
        .unwrap();
    let fresh: Vec<u64> = store
        .registry_changes_after(watermark, 100)
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(fresh.len(), 2);

    let purged = store.purge_history(cutoff).unwrap();
    assert_eq!(
        purged,
        phoebis_store::PurgedHistory {
            attempts: 2_500 + 1,
            changes: 1_500 + 3,
        }
    );
    let raw = rusqlite::Connection::open(&path).unwrap();
    let attempts: Vec<(String, Option<i64>, Option<String>)> = raw
        .prepare("SELECT account_id, resolved_at, outcome FROM refresh_attempts WHERE resolved_at IS NULL OR resolved_at < ?1 ORDER BY id")
        .unwrap()
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        attempts,
        [
            ("a".into(), Some(500), Some("uncertain".into())),
            ("c".into(), None, None),
            ("b".into(), Some(200), Some("failed".into())),
            ("b".into(), Some(300), Some("failed".into())),
            ("d".into(), Some(400), Some("failed".into())),
            ("d".into(), Some(500), Some("failed".into())),
            ("d".into(), Some(150), Some("failed".into())),
        ],
        "the open rows, each untouched holder's newest two and its newest by id stay"
    );
    assert_eq!(
        store
            .last_refresh_attempt("d", 1)
            .unwrap()
            .and_then(|a| a.resolved_at),
        Some(150)
    );
    assert_eq!(
        store
            .newest_resolved_refresh_attempts("a", 1, 10)
            .unwrap()
            .len(),
        3,
        "the fresh attempts stay beside the open one"
    );
    let kept: Vec<(i64, String)> = raw
        .prepare("SELECT at, kind FROM registry_changes WHERE at < ?1 ORDER BY id")
        .unwrap()
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        kept,
        [(12, "key_created".into())],
        "the reused id keeps its newest create"
    );
    assert_eq!(
        store
            .registry_changes_after(watermark, 100)
            .unwrap()
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        fresh,
        "a mirror's watermark still reads the fresh changes"
    );
}

#[test]
fn a_pool_stores_round_robin_and_the_subscription_preference_is_the_operators_alone() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let mut row = pool("rr");
    row.strategy = Strategy::RoundRobin;
    row.prefer_subscription = true;
    store.insert_pool(&row, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert_eq!(
        (read.strategy, read.prefer_subscription),
        (Strategy::RoundRobin, true)
    );
    row.prefer_subscription = false;
    store.update_pool(&row, read.updated_at, "test").unwrap();
    let read = store.pool("rr").unwrap().unwrap();
    assert!(
        !read.prefer_subscription,
        "the flag is the operator's answer alone; no strategy overrides it"
    );
    let change = store.registry_changes_after(0, 10).unwrap().pop().unwrap();
    assert!(change
        .diff_json
        .contains(r#""prefer_subscription":{"from":true,"to":false}"#));
}

/// The kind and the diff of the newest change the log holds for `target`.
fn last_change(store: &Store, target: &str) -> (String, serde_json::Value) {
    let change = store
        .registry_changes_after(0, 1_000)
        .unwrap()
        .into_iter()
        .rfind(|c| c.target == target)
        .unwrap();
    (
        change.kind,
        serde_json::from_str(&change.diff_json).unwrap(),
    )
}

/// An account write of `label` fenced on the row as it reads now, staging nothing else.
fn account_edit<'a>(store: &Store, id: &'a str, label: &'a str) -> AccountUpdate<'a> {
    let row = store.account(id).unwrap().unwrap();
    AccountUpdate {
        id,
        expected_revision: row.revision,
        incarnation: row.incarnation,
        lifecycle_generation: row.lifecycle_generation,
        label,
        secret: None,
        state: None,
        at: 5,
        actor: "admin",
        enabled: None,
        cancellation: None,
    }
}

#[test]
fn a_pool_edit_records_each_field_it_changed_with_its_value_before_and_after_and_nothing_else() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let created = pool("main");
    store
        .insert_pool(
            &PoolRow {
                failover: Failover {
                    in_flight_cap: 8,
                    ..created.failover
                },
                ..created
            },
            "test",
        )
        .unwrap();
    let read = store.pool("main").unwrap().unwrap();
    let stamp = store
        .update_pool(
            &PoolRow {
                label: "Main pool".into(),
                failover: Failover {
                    in_flight_cap: 4,
                    ..read.failover
                },
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(
        last_change(&store, "main"),
        (
            "pool_updated".to_string(),
            serde_json::json!({
                "label": { "from": "main", "to": "Main pool" },
                "in_flight_cap": { "from": 8, "to": 4 },
            })
        ),
        "the two fields the edit changed and no other"
    );
    let edited = store.pool("main").unwrap().unwrap();
    store.update_pool(&edited, stamp, "test").unwrap();
    assert_eq!(
        last_change(&store, "main"),
        (
            "pool_updated".to_string(),
            serde_json::json!({
                "label": { "from": "main", "to": "Main pool" },
                "in_flight_cap": { "from": 8, "to": 4 },
            })
        ),
        "an edit that changed no field records no row, so the two-field edit stays the newest"
    );
}

#[test]
fn a_key_edit_records_each_field_it_changed_with_its_value_before_and_after_and_nothing_else() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-0123456789abcdef"),
            "test",
        )
        .unwrap();
    let read = store.api_key("desktop").unwrap().unwrap();
    let stamp = store
        .update_api_key(
            &ApiKeyRow {
                label: "Desktop".into(),
                enabled: false,
                ..read.clone()
            },
            read.updated_at,
            "test",
        )
        .unwrap();
    assert_eq!(
        last_change(&store, "desktop"),
        (
            "key_updated".to_string(),
            serde_json::json!({
                "label": { "from": "desktop", "to": "Desktop" },
                "enabled": { "from": true, "to": false },
            })
        ),
        "the two fields the edit changed and no other"
    );
    let edited = store.api_key("desktop").unwrap().unwrap();
    store.update_api_key(&edited, stamp, "test").unwrap();
    assert_eq!(
        last_change(&store, "desktop"),
        ("key_updated".to_string(), serde_json::json!({})),
        "an edit that changed no field"
    );
}

#[test]
fn a_creation_records_the_rows_snapshot() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(
            &key(&store, "desktop", "main", "phoebis-0123456789abcdef"),
            "test",
        )
        .unwrap();
    for (target, kind, label) in [
        ("main", "pool_created", "main"),
        ("desktop", "key_created", "desktop"),
    ] {
        let (logged, diff) = last_change(&store, target);
        let diff = diff.as_object().unwrap().clone();
        assert_eq!(
            (logged.as_str(), &diff["label"]),
            (kind, &serde_json::json!(label)),
            "{kind} names the label with its value"
        );
        assert!(
            diff.values()
                .all(|v| !v.is_object() || v.get("from").is_none()),
            "{kind} holds values, not pairs: {diff:?}"
        );
    }
}

#[test]
fn an_account_edit_of_the_label_and_the_end_records_two_pairs_and_no_secret_flag() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("plan", AccountKind::Oauth))
        .unwrap();
    store.set_canceled("plan", true, Some(1_000)).unwrap();
    store
        .commit_account_update(&AccountUpdate {
            cancellation: Some((true, Some(2_000))),
            ..account_edit(&store, "plan", "Plan")
        })
        .unwrap();
    assert_eq!(
        last_change(&store, "plan"),
        (
            "account_updated".to_string(),
            serde_json::json!({
                "label": { "from": "plan", "to": "Plan" },
                "subscription_ends_at": { "from": 1_000, "to": 2_000 },
            })
        ),
        "the label and the end, the flag the edit left as it stood, and no secret"
    );
}

#[test]
fn an_uncancel_records_the_end_the_update_drops_as_null() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("plan", AccountKind::Oauth))
        .unwrap();
    store.set_canceled("plan", true, Some(2_000)).unwrap();
    // The admin route stages an uncancel with the stored end, and the update drops it with the flag.
    store
        .commit_account_update(&AccountUpdate {
            cancellation: Some((false, Some(2_000))),
            ..account_edit(&store, "plan", "plan")
        })
        .unwrap();
    assert_eq!(
        store.subscription_term("plan").unwrap().unwrap().ends_at,
        None
    );
    assert_eq!(
        last_change(&store, "plan").1,
        serde_json::json!({
            "canceled": { "from": true, "to": false },
            "subscription_ends_at": { "from": 2_000, "to": null },
        })
    );
}

#[test]
fn an_account_edit_that_stages_a_secret_records_the_flag_beside_its_pairs_and_never_the_secret() {
    let store = seeded_api_key_account("vendor");
    let secret = Secret::new("sk-ant-api03-REPLACED");
    store
        .commit_account_update(&AccountUpdate {
            enabled: Some(false),
            secret: Some(&secret),
            state: Some((AccountState::Ready, StateCause::Operator)),
            ..account_edit(&store, "vendor", "Vendor")
        })
        .unwrap();
    assert_eq!(
        store
            .api_key_credential("vendor")
            .unwrap()
            .unwrap()
            .0
            .expose(),
        "sk-ant-api03-REPLACED"
    );
    let change = store
        .registry_changes_after(0, 100)
        .unwrap()
        .into_iter()
        .rfind(|c| c.kind == "account_updated")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&change.diff_json).unwrap(),
        serde_json::json!({
            "label": { "from": "vendor", "to": "Vendor" },
            "enabled": { "from": true, "to": false },
            "secret_replaced": true,
        }),
        "the state the secret stages was already ready, so it reads no pair"
    );
    assert!(
        !change.diff_json.contains("REPLACED"),
        "{}",
        change.diff_json
    );
}

#[test]
fn an_edit_of_an_absent_pool_key_or_account_answers_not_found_and_writes_nothing() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    let logged = store.registry_changes_after(0, 100).unwrap().len();
    assert!(matches!(
        store.update_pool(&pool("ghost"), 1, "test"),
        Err(StoreError::PoolNotFound(id)) if id == "ghost"
    ));
    assert!(matches!(
        store.update_api_key(&key(&store, "ghost", "main", "phoebis-ghost"), 0, "test"),
        Err(StoreError::KeyNotFound(id)) if id == "ghost"
    ));
    assert!(matches!(
        rename(&store, "ghost", 1, "x", "admin"),
        Err(StoreError::AccountNotFound(id)) if id == "ghost"
    ));
    assert_eq!(
        store.registry_changes_after(0, 100).unwrap().len(),
        logged,
        "no change is recorded"
    );
    assert!(
        store.pool("ghost").unwrap().is_none()
            && store.api_key("ghost").unwrap().is_none()
            && store.account("ghost").unwrap().is_none()
    );
}

#[test]
fn an_account_update_is_fenced_on_the_revision_it_was_built_from() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    let row = account("vendor", AccountKind::ApiKey);
    store
        .create_account(&row, Some(&Secret::new("k1")), 1, "admin")
        .unwrap();
    let built_from = store.account_revision("vendor").unwrap().unwrap();
    rename(&store, "vendor", built_from, "Renamed", "admin").unwrap();
    assert_eq!(
        store.account("vendor").unwrap().unwrap().label.as_str(),
        "Renamed"
    );
    assert!(
        matches!(
            rename(&store, "vendor", built_from, "Renamed Again", "admin"),
            Err(StoreError::Stale)
        ),
        "the revision the caller read is gone, so the edit it assembled is about a row that moved"
    );
    assert_eq!(store.account("vendor").unwrap().unwrap().label, "Renamed");
    assert!(matches!(
        rename(&store, "nobody", 1, "x", "admin"),
        Err(StoreError::AccountNotFound(_))
    ));
}

/// Runs the key-loss recovery as `docs/site/operations.md` prints it, read from the page so the two cannot drift.
#[test]
fn the_key_loss_recovery_opens_the_registry_under_a_new_key_with_its_accounts_and_pools_intact() {
    let page = include_str!("../../../docs/site/operations.md");
    let mut sequences = page.split("<<'SQL'\n").skip(1);
    let sequence = sequences
        .next()
        .and_then(|rest| rest.split("\nSQL\n").next())
        .expect("the page prints the recovery sequence as a heredoc");
    assert!(
        sequences.next().is_none(),
        "the page prints one recovery sequence"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let lost = StorageKey::generate();
    let store = Store::open(&path, &lost).unwrap();
    store
        .insert_account(&account("claude", AccountKind::Oauth))
        .unwrap();
    store
        .commit_credentials(
            "claude",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("at-test-not-a-token"),
                refresh_token: Some(Secret::new("rt-test-not-a-token")),
                issued_at: 1_799_996_400_000,
                expires_at: 1_800_000_000_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    store.open_refresh_attempt("claude", 1, 1, 1).unwrap();
    store
        .create_account(
            &account("vendor", AccountKind::ApiKey),
            Some(&Secret::new("sk-ant-test-not-a-key")),
            1,
            "admin",
        )
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[member("claude", 1), member("vendor", 2)], "test")
        .unwrap();
    let desktop = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&desktop, "test").unwrap();
    drop(store);

    let new = StorageKey::generate();
    assert!(matches!(
        Store::open(&path, &new),
        Err(StoreError::KeyMismatch { .. })
    ));
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(sequence)
        .unwrap();
    assert_eq!(
        Store::read_canary(&path, &new).unwrap(),
        CanaryRead::Absent,
        "nothing sealed is left to prove a key against"
    );

    let store = Store::open(&path, &new).unwrap();
    assert_eq!(
        Store::read_canary(&path, &new).unwrap(),
        CanaryRead::Matches,
        "the start under the new key sealed a new canary"
    );
    assert!(matches!(
        Store::read_canary(&path, &lost),
        Err(StoreError::KeyMismatch { .. })
    ));
    let states: Vec<(String, AccountState)> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|row| (row.id, row.state))
        .collect();
    assert_eq!(
        states,
        vec![
            ("claude".to_string(), AccountState::Disconnected),
            ("vendor".to_string(), AccountState::Unpaired),
        ],
        "each account rests where the Disconnect action leaves it"
    );
    assert!(store.credentials("claude").unwrap().is_none());
    assert!(store.api_key_secret("vendor").unwrap().is_none());
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    assert!(store.pool("main").unwrap().is_some());
    let members: Vec<String> = store
        .pool_members("main")
        .unwrap()
        .into_iter()
        .map(|member| member.account_id)
        .collect();
    assert_eq!(members, vec!["claude".to_string(), "vendor".to_string()]);
    assert_eq!(store.api_key("desktop").unwrap().unwrap().pool_id, "main");
}

/// A guardrail named after its id, enabled, observing, with no preset stamp and no stamps of its own, matching the
/// auxiliary class unless the case gives its conditions.
fn guardrail(id: &str, conditions: Option<Vec<Condition>>) -> GuardrailRow {
    GuardrailRow {
        id: id.into(),
        name: id.into(),
        description: String::new(),
        enabled: true,
        mode: GuardrailMode::Observe,
        action: GuardrailAction::Block,
        conditions: conditions.unwrap_or_else(|| {
            vec![Condition {
                subject: Subject::RequestClass,
                operator: Operator::Equals,
                value: Some("auxiliary".into()),
                name: None,
                path: None,
                selector: None,
            }]
        }),
        preset_id: None,
        preset_version: None,
        incarnation: 1,
        created_at: 0,
        updated_at: 0,
    }
}

/// The subjects every guardrail case attaches to: the pools `main` and `other`, `k1` in `main` and `k2` in `other`.
fn seed_subjects(store: &Store) {
    store.insert_pool(&pool("main"), "test").unwrap();
    store.insert_pool(&pool("other"), "test").unwrap();
    store
        .insert_api_key(&key(store, "k1", "main", "phoebis-k1-secret"), "test")
        .unwrap();
    store
        .insert_api_key(&key(store, "k2", "other", "phoebis-k2-secret"), "test")
        .unwrap();
}

fn guarded_store() -> Store {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    seed_subjects(&store);
    store
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|id| id.to_string()).collect()
}

/// One subject's guardrails with their positions, in the order the subject runs them.
fn attached(store: &Store, subject: &AttachmentSubject) -> Vec<(String, u32)> {
    store
        .guardrail_attachments()
        .unwrap()
        .into_iter()
        .filter(|attachment| &attachment.subject == subject)
        .map(|attachment| (attachment.guardrail_id, attachment.position))
        .collect()
}

fn at(list: &[(&str, u32)]) -> Vec<(String, u32)> {
    list.iter()
        .map(|(id, position)| (id.to_string(), *position))
        .collect()
}

fn on_pool(id: &str) -> AttachmentSubject {
    AttachmentSubject::Pool(id.into())
}

fn on_key(id: &str) -> AttachmentSubject {
    AttachmentSubject::Key(id.into())
}

#[test]
fn a_guardrail_round_trips_with_its_conditions_mode_and_preset_stamp() {
    let store = guarded_store();
    let row = GuardrailRow {
        description: "the summary fork".into(),
        mode: GuardrailMode::Enforce,
        preset_id: Some("subagent-status-summary".into()),
        preset_version: Some(1),
        ..guardrail(
            "g1",
            Some(vec![
                Condition {
                    subject: Subject::RequestClass,
                    operator: Operator::Equals,
                    value: Some("auxiliary".into()),
                    name: None,
                    path: None,
                    selector: None,
                },
                Condition {
                    subject: Subject::Header,
                    operator: Operator::Present,
                    value: None,
                    name: Some("x-team".into()),
                    path: None,
                    selector: None,
                },
                Condition {
                    subject: Subject::Body,
                    operator: Operator::Gt,
                    value: Some("4".into()),
                    name: None,
                    path: Some("tools.length".into()),
                    selector: None,
                },
                Condition {
                    subject: Subject::Message,
                    operator: Operator::Matches,
                    value: Some("(?i)summar".into()),
                    name: None,
                    path: None,
                    selector: Some(Selector::LastUser),
                },
            ]),
        )
    };
    let written = store.insert_guardrail(&row, &[], &[], "test").unwrap();
    assert_eq!(written.incarnation, 1);
    assert!(written.created_at > 0);
    assert_eq!(written.updated_at, written.created_at);
    assert_eq!(
        written,
        GuardrailRow {
            created_at: written.created_at,
            updated_at: written.updated_at,
            ..row
        },
        "the committed row carries what was written with the store's own stamps"
    );
    assert_eq!(store.guardrail("g1").unwrap(), Some(written.clone()));
    assert_eq!(store.guardrails().unwrap(), vec![written]);
    assert_eq!(store.guardrail("ghost").unwrap(), None);
}

#[test]
fn a_guardrail_edit_is_fenced_on_its_stamp_and_its_incarnation() {
    let store = guarded_store();
    let written = store
        .insert_guardrail(&guardrail("g1", None), &[], &[], "test")
        .unwrap();
    let edit = GuardrailRow {
        name: "renamed".into(),
        ..written.clone()
    };
    assert!(matches!(
        store.update_guardrail(&edit, written.updated_at - 1, None, None, "test"),
        Err(StoreError::Stale)
    ));
    assert!(matches!(
        store.update_guardrail(
            &GuardrailRow {
                incarnation: 2,
                ..edit.clone()
            },
            written.updated_at,
            None,
            None,
            "test"
        ),
        Err(StoreError::Stale)
    ));
    assert!(matches!(
        store.update_guardrail(
            &GuardrailRow {
                id: "ghost".into(),
                ..edit.clone()
            },
            written.updated_at,
            None,
            None,
            "test"
        ),
        Err(StoreError::GuardrailNotFound(id)) if id == "ghost"
    ));
    assert_eq!(
        store.guardrail("g1").unwrap(),
        Some(written.clone()),
        "a refused edit writes nothing"
    );
    let stamped = store
        .update_guardrail(&edit, written.updated_at, None, None, "test")
        .unwrap();
    assert!(stamped > written.updated_at);
    assert_eq!(
        store.guardrail("g1").unwrap(),
        Some(GuardrailRow {
            updated_at: stamped,
            ..edit
        })
    );
}

#[test]
fn a_guardrail_created_again_under_a_deleted_id_restarts_at_the_next_incarnation() {
    let store = guarded_store();
    let first = store
        .insert_guardrail(&guardrail("g1", None), &[], &[], "test")
        .unwrap();
    assert_eq!(first.incarnation, 1);
    store.delete_guardrail("g1", "test").unwrap();
    assert_eq!(store.guardrail("g1").unwrap(), None);
    assert!(matches!(
        store.delete_guardrail("g1", "test"),
        Err(StoreError::GuardrailNotFound(id)) if id == "g1"
    ));
    let second = store
        .insert_guardrail(&guardrail("g1", None), &[], &[], "test")
        .unwrap();
    assert_eq!(second.incarnation, 2);
    assert!(
        matches!(
            store.update_guardrail(&first, second.updated_at, None, None, "test"),
            Err(StoreError::Stale)
        ),
        "an edit read from the deleted guardrail does not land on its replacement"
    );
    store.delete_guardrail("g1", "test").unwrap();
    let third = store
        .insert_guardrail(&guardrail("g1", None), &[], &[], "test")
        .unwrap();
    assert_eq!(third.incarnation, 3);
}

#[test]
fn deleting_a_guardrail_removes_its_attachments_and_deleting_a_pool_or_a_key_removes_theirs() {
    let store = guarded_store();
    for id in ["g1", "g2"] {
        store
            .insert_guardrail(
                &guardrail(id, None),
                &ids(&["main", "other"]),
                &ids(&["k1", "k2"]),
                "test",
            )
            .unwrap();
    }
    assert_eq!(store.guardrail_attachments().unwrap().len(), 8);
    store.delete_guardrail("g1", "test").unwrap();
    let left: Vec<String> = store
        .guardrail_attachments()
        .unwrap()
        .into_iter()
        .map(|attachment| attachment.guardrail_id)
        .collect();
    assert_eq!(left, ids(&["g2", "g2", "g2", "g2"]));
    store.delete_api_key("k2", "test").unwrap();
    assert!(attached(&store, &on_key("k2")).is_empty());
    let k1 = store.api_key("k1").unwrap().unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                pool_id: "other".into(),
                ..k1.clone()
            },
            k1.updated_at,
            "test",
        )
        .unwrap();
    store.delete_pool("main", "test").unwrap();
    assert_eq!(
        store.guardrail_attachments().unwrap(),
        vec![
            GuardrailAttachment {
                guardrail_id: "g2".into(),
                subject: on_key("k1"),
                position: 1,
            },
            GuardrailAttachment {
                guardrail_id: "g2".into(),
                subject: on_pool("other"),
                position: 1,
            },
        ]
    );
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .insert_api_key(&key(&store, "k2", "other", "phoebis-k2-again"), "test")
        .unwrap();
    assert!(
        attached(&store, &on_pool("main")).is_empty() && attached(&store, &on_key("k2")).is_empty(),
        "a pool and a key created again under a deleted id start with no guardrail"
    );
}

#[test]
fn the_guardrail_side_appends_a_new_subject_keeps_a_held_position_and_detaches_one_left_out() {
    let store = guarded_store();
    let g1 = store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let g2 = store
        .insert_guardrail(&guardrail("g2", None), &ids(&["main"]), &[], "test")
        .unwrap();
    assert_eq!(
        attached(&store, &on_pool("main")),
        at(&[("g1", 0), ("g2", 1)])
    );
    let main_stamp = store.pool("main").unwrap().unwrap().updated_at;
    let other_stamp = store.pool("other").unwrap().unwrap().updated_at;
    let k1_stamp = store.api_key("k1").unwrap().unwrap().updated_at;
    store
        .update_guardrail(&g1, g1.updated_at, Some(&ids(&["other"])), None, "test")
        .unwrap();
    assert_eq!(attached(&store, &on_pool("main")), at(&[("g2", 1)]));
    assert_eq!(attached(&store, &on_pool("other")), at(&[("g1", 0)]));
    assert_eq!(attached(&store, &on_key("k1")), at(&[("g1", 0)]));
    store
        .update_guardrail(
            &g2,
            g2.updated_at,
            Some(&ids(&["main", "other"])),
            None,
            "test",
        )
        .unwrap();
    assert_eq!(
        attached(&store, &on_pool("main")),
        at(&[("g2", 1)]),
        "a held subject keeps its position"
    );
    assert_eq!(
        attached(&store, &on_pool("other")),
        at(&[("g1", 0), ("g2", 1)]),
        "a new subject takes the end of its order"
    );
    assert_eq!(
        (
            store.pool("main").unwrap().unwrap().updated_at > main_stamp,
            store.pool("other").unwrap().unwrap().updated_at > other_stamp,
            store.api_key("k1").unwrap().unwrap().updated_at,
        ),
        (true, true, k1_stamp),
        "a write from the guardrail's side stamps the subjects whose list it changed and no other"
    );
}

#[test]
fn the_subject_side_replaces_the_list_whole_in_the_given_order() {
    let store = guarded_store();
    let g1 = store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let g2 = store
        .insert_guardrail(
            &guardrail("g2", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let main = store.pool("main").unwrap().unwrap();
    let stamped = store
        .update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&ids(&["g2", "g1"])),
            "test",
        )
        .unwrap();
    assert_eq!(
        attached(&store, &on_pool("main")),
        at(&[("g2", 0), ("g1", 1)])
    );
    assert_eq!(store.pool("main").unwrap().unwrap().updated_at, stamped);
    let k1 = store.api_key("k1").unwrap().unwrap();
    store
        .update_api_key_with_guardrails(&k1, k1.updated_at, Some(&[]), "test")
        .unwrap();
    assert!(attached(&store, &on_key("k1")).is_empty());
    assert_eq!(
        (
            store.guardrail("g1").unwrap().unwrap().updated_at > g1.updated_at,
            store.guardrail("g2").unwrap().unwrap().updated_at > g2.updated_at,
        ),
        (true, true),
        "a write from the subject's side stamps each guardrail whose membership it changed"
    );
}

#[test]
fn a_pool_save_opened_before_a_guardrail_side_attachment_is_refused_stale_and_a_reread_carries_the_new_list(
) {
    let store = guarded_store();
    for id in ["g1", "g2"] {
        store
            .insert_guardrail(&guardrail(id, None), &ids(&["main"]), &[], "test")
            .unwrap();
    }
    let main = store.pool("main").unwrap().unwrap();
    store
        .insert_guardrail(&guardrail("g3", None), &ids(&["main"]), &[], "test")
        .unwrap();
    assert!(matches!(
        store.update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&ids(&["g2", "g1"])),
            "test",
        ),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        attached(&store, &on_pool("main")),
        at(&[("g1", 0), ("g2", 1), ("g3", 2)]),
        "the attachment the stale save would have dropped stands"
    );
}

#[test]
fn a_key_save_opened_before_a_guardrail_side_attachment_is_refused_stale() {
    let store = guarded_store();
    for id in ["g1", "g2"] {
        store
            .insert_guardrail(&guardrail(id, None), &[], &ids(&["k1"]), "test")
            .unwrap();
    }
    let k1 = store.api_key("k1").unwrap().unwrap();
    store
        .insert_guardrail(&guardrail("g3", None), &[], &ids(&["k1"]), "test")
        .unwrap();
    assert!(matches!(
        store.update_api_key_with_guardrails(&k1, k1.updated_at, Some(&ids(&["g2", "g1"])), "test"),
        Err(StoreError::Stale)
    ));
    assert_eq!(
        attached(&store, &on_key("k1")),
        at(&[("g1", 0), ("g2", 1), ("g3", 2)]),
        "the attachment the stale save would have dropped stands"
    );
}

#[test]
fn delete_guardrail_stamps_every_subject_it_was_attached_to() {
    let store = guarded_store();
    store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let stamps = |store: &Store| {
        (
            store.pool("main").unwrap().unwrap().updated_at,
            store.api_key("k1").unwrap().unwrap().updated_at,
            store.pool("other").unwrap().unwrap().updated_at,
        )
    };
    let (main, k1, other) = stamps(&store);
    store.delete_guardrail("g1", "test").unwrap();
    let (main_after, k1_after, other_after) = stamps(&store);
    assert_eq!(
        (main_after > main, k1_after > k1, other_after),
        (true, true, other),
        "the delete stamps the pool and the key it was attached to and no other"
    );
}

#[test]
fn a_guardrail_save_opened_before_a_pool_side_detach_is_refused_stale() {
    let store = guarded_store();
    store
        .insert_guardrail(&guardrail("g1", None), &ids(&["main"]), &[], "test")
        .unwrap();
    let g2 = store
        .insert_guardrail(&guardrail("g2", None), &[], &[], "test")
        .unwrap();
    let g1 = store.guardrail("g1").unwrap().unwrap();
    let main = store.pool("main").unwrap().unwrap();
    store
        .update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&ids(&["g2"])),
            "test",
        )
        .unwrap();
    assert!(matches!(
        store.update_guardrail(&g1, g1.updated_at, None, None, "test"),
        Err(StoreError::Stale)
    ));
    assert!(
        store.guardrail("g1").unwrap().unwrap().updated_at > g1.updated_at,
        "the detach moved the guardrail's stamp"
    );
    assert!(
        store.guardrail("g2").unwrap().unwrap().updated_at > g2.updated_at,
        "the attach in the same write moved the attached guardrail's stamp"
    );
}

#[test]
fn a_write_that_changes_no_list_moves_no_stamp_on_the_other_side() {
    let store = guarded_store();
    for id in ["g1", "g2"] {
        store
            .insert_guardrail(&guardrail(id, None), &ids(&["main"]), &[], "test")
            .unwrap();
    }
    let main = store.pool("main").unwrap().unwrap();
    let g1 = store.guardrail("g1").unwrap().unwrap();
    let edited = GuardrailRow {
        description: "edited".into(),
        ..g1.clone()
    };
    let stamped = store
        .update_guardrail(&edited, g1.updated_at, None, None, "test")
        .unwrap();
    store
        .update_guardrail(&edited, stamped, Some(&ids(&["main"])), None, "test")
        .unwrap();
    assert_eq!(
        store.pool("main").unwrap().unwrap().updated_at,
        main.updated_at,
        "a guardrail write that keeps the pool's membership leaves the pool's stamp"
    );
    let guardrail_stamps =
        |store: &Store| ["g1", "g2"].map(|id| store.guardrail(id).unwrap().unwrap().updated_at);
    let before = guardrail_stamps(&store);
    store
        .update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&ids(&["g2", "g1"])),
            "test",
        )
        .unwrap();
    assert_eq!(
        attached(&store, &on_pool("main")),
        at(&[("g2", 0), ("g1", 1)])
    );
    assert_eq!(
        guardrail_stamps(&store),
        before,
        "a reorder from the pool's side leaves the guardrails' stamps"
    );
}

#[test]
fn a_subject_write_naming_an_unknown_guardrail_writes_nothing() {
    let store = guarded_store();
    store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let changes = store.registry_changes_after(0, 1_000).unwrap().len();
    let main = store.pool("main").unwrap().unwrap();
    assert!(matches!(
        store.update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&ids(&["g1", "ghost"])),
            "test",
        ),
        Err(StoreError::GuardrailNotFound(id)) if id == "ghost"
    ));
    let k1 = store.api_key("k1").unwrap().unwrap();
    assert!(matches!(
        store.update_api_key_with_guardrails(&k1, k1.updated_at, Some(&ids(&["ghost"])), "test"),
        Err(StoreError::GuardrailNotFound(id)) if id == "ghost"
    ));
    assert_eq!(attached(&store, &on_pool("main")), at(&[("g1", 0)]));
    assert_eq!(attached(&store, &on_key("k1")), at(&[("g1", 0)]));
    assert_eq!(
        store.pool("main").unwrap().unwrap().updated_at,
        main.updated_at
    );
    assert_eq!(
        store.api_key("k1").unwrap().unwrap().updated_at,
        k1.updated_at
    );
    assert_eq!(
        store.registry_changes_after(0, 1_000).unwrap().len(),
        changes
    );
}

#[test]
fn the_bound_is_checked_inside_the_write_on_a_key_a_pool_and_a_pool_move() {
    let store = guarded_store();
    store.insert_pool(&pool("empty"), "test").unwrap();
    let sixty_four: Vec<String> = (1..=64).map(|n| format!("g{n:02}")).collect();
    for id in &sixty_four {
        store
            .insert_guardrail(&guardrail(id, None), &[], &[], "test")
            .unwrap();
    }
    // The sixty-fifth exists before either side names it, so each update refusal below is the bound's and never a
    // missing id's. Each insert refusal names a guardrail of its own, so a write past the bound shows in the readback.
    store
        .insert_guardrail(&guardrail("g65", None), &[], &[], "test")
        .unwrap();
    let sixty_five: Vec<String> = sixty_four
        .iter()
        .cloned()
        .chain(["g65".to_string()])
        .collect();
    let state = |store: &Store| {
        (
            store.guardrail_attachments().unwrap(),
            store.api_key("k2").unwrap().unwrap().pool_id,
            store.guardrails().unwrap(),
        )
    };

    let empty = store.pool("empty").unwrap().unwrap();
    store
        .update_pool_with_members_and_guardrails(
            &empty,
            empty.updated_at,
            None,
            Some(&sixty_four),
            "test",
        )
        .unwrap();
    let before = state(&store);
    assert!(matches!(
        store.insert_guardrail(&guardrail("g66", None), &ids(&["empty"]), &[], "test"),
        Err(StoreError::PoolGuardrailBound(id)) if id == "empty"
    ));
    assert_eq!(state(&store), before);
    let empty = store.pool("empty").unwrap().unwrap();
    assert!(matches!(
        store.update_pool_with_members_and_guardrails(
            &empty,
            empty.updated_at,
            None,
            Some(&sixty_five),
            "test",
        ),
        Err(StoreError::PoolGuardrailBound(id)) if id == "empty"
    ));
    assert_eq!(state(&store), before);

    let main = store.pool("main").unwrap().unwrap();
    store
        .update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&sixty_four),
            "test",
        )
        .unwrap();
    let before = state(&store);
    assert!(matches!(
        store.insert_guardrail(&guardrail("g66", None), &[], &ids(&["k1"]), "test"),
        Err(StoreError::KeyGuardrailBound(id)) if id == "k1"
    ));
    assert_eq!(state(&store), before);
    let k1 = store.api_key("k1").unwrap().unwrap();
    assert!(matches!(
        store.update_api_key_with_guardrails(&k1, k1.updated_at, Some(&ids(&["g65"])), "test"),
        Err(StoreError::KeyGuardrailBound(id)) if id == "k1"
    ));
    assert_eq!(state(&store), before);

    let k2 = store.api_key("k2").unwrap().unwrap();
    store
        .update_api_key_with_guardrails(&k2, k2.updated_at, Some(&ids(&["g65"])), "test")
        .unwrap();
    let before = state(&store);
    let k2 = store.api_key("k2").unwrap().unwrap();
    assert!(matches!(
        store.update_api_key(
            &ApiKeyRow {
                pool_id: "main".into(),
                ..k2.clone()
            },
            k2.updated_at,
            "test"
        ),
        Err(StoreError::KeyGuardrailBound(id)) if id == "k2"
    ));
    assert_eq!(state(&store), before);
    assert_eq!(store.api_key("k2").unwrap().unwrap().pool_id, "other");
}

#[test]
fn a_pool_side_write_that_takes_a_keys_own_list_past_the_bound_is_refused_for_the_key() {
    let store = guarded_store();
    let sixty_five: Vec<String> = (1..=65).map(|n| format!("g{n:02}")).collect();
    for id in &sixty_five {
        store
            .insert_guardrail(&guardrail(id, None), &[], &[], "test")
            .unwrap();
    }
    let k1 = store.api_key("k1").unwrap().unwrap();
    store
        .update_api_key_with_guardrails(&k1, k1.updated_at, Some(&sixty_five[..63]), "test")
        .unwrap();
    let state = |store: &Store| {
        (
            store.guardrail_attachments().unwrap(),
            store.guardrails().unwrap(),
        )
    };
    let before = state(&store);

    let main = store.pool("main").unwrap().unwrap();
    let refused = store
        .update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&sixty_five[63..]),
            "test",
        )
        .unwrap_err();
    assert!(
        matches!(&refused, StoreError::KeyGuardrailBound(id) if id == "k1"),
        "{refused}"
    );
    assert_eq!(
        refused.to_string(),
        "key k1 would hold more than 64 guardrails"
    );
    assert_eq!(state(&store), before, "the refused write left every list");

    let main = store.pool("main").unwrap().unwrap();
    store
        .update_pool_with_members_and_guardrails(
            &main,
            main.updated_at,
            None,
            Some(&sixty_five[63..64]),
            "test",
        )
        .expect("a pool list that brings the key's union to the bound is written");
}

#[test]
fn the_load_answers_the_guardrails_by_id_and_the_attachments_in_position_order() {
    let store = guarded_store();
    store
        .insert_guardrail(
            &guardrail("g2", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main", "other"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!(
        load.guardrails
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["g1", "g2"]
    );
    assert_eq!(load.guardrails, store.guardrails().unwrap());
    let attachment =
        |guardrail_id: &str, subject: AttachmentSubject, position: u32| GuardrailAttachment {
            guardrail_id: guardrail_id.into(),
            subject,
            position,
        };
    assert_eq!(
        load.guardrail_attachments,
        vec![
            attachment("g2", on_key("k1"), 0),
            attachment("g1", on_key("k1"), 1),
            attachment("g2", on_pool("main"), 0),
            attachment("g1", on_pool("main"), 1),
            attachment("g1", on_pool("other"), 0),
        ]
    );
}

#[test]
fn the_change_log_records_the_three_guardrail_kinds_and_the_guardrails_member_on_a_pool_and_a_key_edit(
) {
    let store = guarded_store();
    let start = store.newest_registry_change_id().unwrap().unwrap_or(0);
    let g1 = store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    store
        .insert_guardrail(&guardrail("g2", None), &[], &[], "test")
        .unwrap();
    store
        .update_guardrail(
            &GuardrailRow {
                name: "renamed".into(),
                mode: GuardrailMode::Enforce,
                ..g1.clone()
            },
            g1.updated_at,
            Some(&ids(&["main", "other"])),
            None,
            "test",
        )
        .unwrap();
    for _ in 0..2 {
        let main = store.pool("main").unwrap().unwrap();
        store
            .update_pool_with_members_and_guardrails(
                &main,
                main.updated_at,
                None,
                Some(&ids(&["g2", "g1"])),
                "test",
            )
            .unwrap();
    }
    let k1 = store.api_key("k1").unwrap().unwrap();
    store
        .update_api_key_with_guardrails(&k1, k1.updated_at, Some(&ids(&["g2"])), "test")
        .unwrap();
    let k1 = store.api_key("k1").unwrap().unwrap();
    store.update_api_key(&k1, k1.updated_at, "test").unwrap();
    store.delete_guardrail("g2", "test").unwrap();
    let changes: Vec<(String, String, serde_json::Value)> = store
        .registry_changes_after(start, 100)
        .unwrap()
        .into_iter()
        .map(|change| {
            (
                change.kind,
                change.target,
                serde_json::from_str(&change.diff_json).unwrap(),
            )
        })
        .collect();
    let kinds: Vec<(&str, &str)> = changes
        .iter()
        .map(|(kind, target, _)| (kind.as_str(), target.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("guardrail_created", "g1"),
            ("guardrail_created", "g2"),
            ("guardrail_updated", "g1"),
            ("pool_updated", "main"),
            ("key_updated", "k1"),
            ("key_updated", "k1"),
            ("guardrail_deleted", "g2"),
        ]
    );
    let created = &changes[0].2;
    assert_eq!(created["name"], "g1");
    assert_eq!(created["mode"], "observe");
    assert_eq!(created["pools"], serde_json::json!(["main"]));
    assert_eq!(created["keys"], serde_json::json!(["k1"]));
    assert_eq!(
        changes[2].2,
        serde_json::json!({
            "name": {"from": "g1", "to": "renamed"},
            "mode": {"from": "observe", "to": "enforce"},
            "pools": {"from": ["main"], "to": ["main", "other"]},
        }),
        "the update's pairs, its pools among them"
    );
    assert_eq!(
        changes[3].2["guardrails"],
        serde_json::json!({ "from": ["g1"], "to": ["g2", "g1"] })
    );
    assert_eq!(
        changes[4].2["guardrails"],
        serde_json::json!({ "from": ["g1"], "to": ["g2"] })
    );
    assert!(
        changes[5].2.get("guardrails").is_none(),
        "a key edit without a list adds no member"
    );
    assert_eq!(changes[6].2, serde_json::json!({}));
}

/// The diffs a guardrail's writes logged after `start`, oldest first.
fn guardrail_diffs(store: &Store, start: u64) -> Vec<serde_json::Value> {
    store
        .registry_changes_after(start, 100)
        .unwrap()
        .into_iter()
        .map(|change| serde_json::from_str(&change.diff_json).unwrap())
        .collect()
}

#[test]
fn a_guardrail_create_logs_its_pools_and_keys_by_id_as_its_update_does() {
    let store = guarded_store();
    let start = store.newest_registry_change_id().unwrap().unwrap_or(0);
    let g1 = store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["other", "main"]),
            &ids(&["k2", "k1"]),
            "test",
        )
        .unwrap();
    store
        .update_guardrail(
            &g1,
            g1.updated_at,
            Some(&ids(&["other", "main"])),
            Some(&ids(&["k2", "k1"])),
            "test",
        )
        .unwrap();
    let diffs = guardrail_diffs(&store, start);
    assert_eq!(diffs[0]["pools"], serde_json::json!(["main", "other"]));
    assert_eq!(diffs[0]["keys"], serde_json::json!(["k1", "k2"]));
    assert_eq!(
        diffs[1],
        serde_json::json!({}),
        "the update reads the lists in the create's order, so the same lists written again change nothing"
    );
}

#[test]
fn a_guardrail_update_logs_each_changed_member_as_a_from_and_to_pair_and_nothing_for_the_rest() {
    let store = guarded_store();
    let g1 = store
        .insert_guardrail(
            &guardrail("g1", None),
            &ids(&["main"]),
            &ids(&["k1"]),
            "test",
        )
        .unwrap();
    let start = store.newest_registry_change_id().unwrap().unwrap_or(0);
    let renamed = GuardrailRow {
        name: "renamed".into(),
        enabled: false,
        ..g1.clone()
    };
    let at = store
        .update_guardrail(
            &renamed,
            g1.updated_at,
            Some(&ids(&["other", "main"])),
            Some(&[]),
            "test",
        )
        .unwrap();
    store
        .update_guardrail(&renamed, at, None, None, "test")
        .unwrap();
    assert_eq!(
        guardrail_diffs(&store, start),
        [
            serde_json::json!({
                "name": {"from": "g1", "to": "renamed"},
                "enabled": {"from": true, "to": false},
                "pools": {"from": ["main"], "to": ["main", "other"]},
                "keys": {"from": ["k1"], "to": []},
            }),
            serde_json::json!({}),
        ],
        "a member the write left as it was, a list included, carries no pair"
    );
}

#[test]
fn a_row_whose_conditions_do_not_parse_or_whose_mode_is_an_unknown_word_fails_the_load() {
    for column in ["conditions_json", "mode"] {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();
        let path = dir.path().join("router.sqlite");
        let store = Store::open(&path, &key).unwrap();
        seed_subjects(&store);
        store
            .insert_guardrail(&guardrail("g1", None), &ids(&["main"]), &[], "test")
            .unwrap();
        assert!(store.load_registry().is_ok());
        drop(store);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            &format!("UPDATE guardrails SET {column} = 'x' WHERE id = 'g1'"),
            [],
        )
        .unwrap();
        drop(conn);
        let store = Store::open(&path, &key).unwrap();
        assert!(
            matches!(
                store.load_registry(),
                Err(StoreError::Sqlite(
                    rusqlite::Error::FromSqlConversionFailure(..)
                ))
            ),
            "a {column} the store cannot read fails the load"
        );
    }
}
