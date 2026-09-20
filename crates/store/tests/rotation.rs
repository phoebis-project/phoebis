use phoebis_store::{
    ApiKeyRow, OveragePolicy, PoolRow, RetiredSecret, StorageKey, Store, StoreError, Strategy,
    SubStrategy, ROTATION_GRACE_MS,
};

fn pool(id: &str) -> PoolRow {
    PoolRow {
        id: id.into(),
        label: id.into(),
        enabled: true,
        strategy: Strategy::PriorityRandom,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        default_allowed_models: None,
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        prefer_subscription: false,
        api_key_sub_strategy: SubStrategy::PriorityRandom,
        cache_ttl_json: "{}".into(),
        cache_diagnostics: None,
        created_at: 1,
        updated_at: 1,
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
        allowed_models: None,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: store.api_key_digest(secret).to_vec(),
        prefix: phoebis_store::key_prefix(secret),
        created_at: 1,
        rotated_at: None,
        last_used_at: None,
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
fn a_deleted_keys_id_cannot_be_created_again() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store.insert_pool(&pool("main"), "t").unwrap();
    store
        .insert_api_key(&key(&store, "k", "main", "one"), "t")
        .unwrap();
    store.delete_api_key("k", "t").unwrap();

    match store.insert_api_key(&key(&store, "k", "main", "two"), "t") {
        Err(StoreError::KeyIdRetired(id)) => assert_eq!(id, "k"),
        other => panic!("expected KeyIdRetired, got {other:?}"),
    }
    store
        .insert_api_key(&key(&store, "k2", "main", "three"), "t")
        .unwrap();
}
