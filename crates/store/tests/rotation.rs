use phoebis_store::{
    ApiKeyRow, PoolRow, RetiredSecret, RulesMode, StorageKey, Store, Strategy, ROTATION_GRACE_MS,
};

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

#[test]
fn a_rotation_retires_the_previous_secret_for_the_grace_and_a_second_rotation_replaces_it() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "t").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "one"), "t")
        .unwrap();

    let prefix_one = store.api_key("k").unwrap().unwrap().prefix;
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("two"),
            &phoebis_store::key_prefix("two"),
            1_000,
            "t",
        )
        .unwrap();
    assert_eq!(
        store.load_registry().unwrap().retired_secrets,
        vec![RetiredSecret {
            key_id: "k".into(),
            secret_digest: store.api_key_digest("one").to_vec(),
            prefix: prefix_one,
            retired_at: 1_000,
            expires_at: 1_000 + ROTATION_GRACE_MS,
        }]
    );

    let prefix_two = store.api_key("k").unwrap().unwrap().prefix;
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("three"),
            &phoebis_store::key_prefix("three"),
            2_000,
            "t",
        )
        .unwrap();
    assert_eq!(
        store.load_registry().unwrap().retired_secrets,
        vec![RetiredSecret {
            key_id: "k".into(),
            secret_digest: store.api_key_digest("two").to_vec(),
            prefix: prefix_two,
            retired_at: 2_000,
            expires_at: 2_000 + ROTATION_GRACE_MS,
        }],
        "the second rotation replaces the grace, it never stacks a second retired secret"
    );

    let prefix_three = store.api_key("k").unwrap().unwrap().prefix;
    let past_the_first_grace = 2_000 + ROTATION_GRACE_MS;
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("four"),
            &phoebis_store::key_prefix("four"),
            past_the_first_grace,
            "t",
        )
        .unwrap();
    assert_eq!(
        store.load_registry().unwrap().retired_secrets,
        vec![RetiredSecret {
            key_id: "k".into(),
            secret_digest: store.api_key_digest("three").to_vec(),
            prefix: prefix_three,
            retired_at: past_the_first_grace,
            expires_at: past_the_first_grace + ROTATION_GRACE_MS,
        }],
        "the row from the second rotation expired exactly at this instant and is swept"
    );
}

#[test]
fn a_touch_stamps_the_retired_secret_only_for_the_rotation_it_authenticated_under() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "t").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "one"), "t")
        .unwrap();
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("two"),
            &phoebis_store::key_prefix("two"),
            500,
            "t",
        )
        .unwrap();
    let rotated_at = store.api_key("k").unwrap().unwrap().rotated_at;
    store.touch_api_key("k", 1_000, true, rotated_at).unwrap();
    assert_eq!(
        store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at,
        Some(1_000),
        "a retired touch carrying the row's own rotation stamps it"
    );
    store
        .rotate_api_key(
            "k",
            &store.api_key_digest("three"),
            &phoebis_store::key_prefix("three"),
            2_000,
            "t",
        )
        .unwrap();
    assert_eq!(
        store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at,
        None,
        "the second rotation clears the stamp its predecessor left"
    );
    store.touch_api_key("k", 3_000, true, rotated_at).unwrap();
    assert_eq!(
        store.api_key("k").unwrap().unwrap().retired_secret_last_used_at,
        None,
        "a retired touch still carrying the first rotation cannot stamp the grace the second one started"
    );
    store.touch_api_key("k", 4_000, false, None).unwrap();
    assert_eq!(
        store
            .api_key("k")
            .unwrap()
            .unwrap()
            .retired_secret_last_used_at,
        None,
        "a live touch never stamps the retired secret"
    );
}

#[test]
fn a_revocation_drops_the_retired_secret_and_a_deletion_cascades_it() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "t").unwrap();

    store
        .insert_api_key(&key(&store, "k1", "main", "one"), "t")
        .unwrap();
    store
        .rotate_api_key(
            "k1",
            &store.api_key_digest("two"),
            &phoebis_store::key_prefix("two"),
            1_000,
            "t",
        )
        .unwrap();
    store.revoke_api_key("k1", 2_000, "t").unwrap();
    assert!(store.load_registry().unwrap().retired_secrets.is_empty());

    store
        .insert_api_key(&key(&store, "k2", "main", "uno"), "t")
        .unwrap();
    store
        .rotate_api_key(
            "k2",
            &store.api_key_digest("dos"),
            &phoebis_store::key_prefix("dos"),
            1_000,
            "t",
        )
        .unwrap();
    store.delete_api_key("k2", "t").unwrap();
    assert!(store.load_registry().unwrap().retired_secrets.is_empty());
}

#[test]
fn a_deleted_keys_id_is_created_again_under_a_fresh_secret() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "t").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "one"), "t")
        .unwrap();
    store.delete_api_key("k", "t").unwrap();

    store
        .insert_api_key(&key(&store, "k", "main", "two"), "t")
        .unwrap();
    assert_eq!(
        store.api_key("k").unwrap().unwrap().prefix,
        phoebis_store::key_prefix("two")
    );
    store
        .insert_api_key(&key(&store, "k2", "main", "three"), "t")
        .unwrap();
}
