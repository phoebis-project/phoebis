use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, ApiKeyRow, Identity, ModelSource,
    OveragePolicy, PoolRow, Secret, ServedModelWrite, StorageKey, Store, StoreError, Strategy,
};

fn account(id: &str, kind: AccountKind) -> AccountRow {
    AccountRow {
        id: id.into(),
        kind,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        state: AccountState::Ready,
    }
}

fn pool(id: &str) -> PoolRow {
    PoolRow {
        id: id.into(),
        label: id.into(),
        enabled: true,
        strategy: Strategy::PriorityRandom,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        default_allowed_models: None,
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
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
fn the_schema_is_migrated_once_and_the_version_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let store = Store::open(&dir.path().join("router.sqlite"), &key).unwrap();
    assert_eq!(store.schema_version().unwrap(), 2);
    drop(store);
    let again = Store::open(&dir.path().join("router.sqlite"), &key).unwrap();
    assert_eq!(
        again.schema_version().unwrap(),
        2,
        "a second open runs no migration and keeps the version"
    );
    assert!(again.pools().unwrap().is_empty());
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
fn pools_members_and_keys_round_trip_and_a_pool_with_keys_cannot_be_deleted() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[("a".into(), 2)], "test")
        .unwrap();
    assert_eq!(store.pool_members("main").unwrap()[0].priority, 2);
    assert!(matches!(
        store.set_pool_members("main", &[("ghost".into(), 0)], "test"),
        Err(StoreError::AccountNotFound(_))
    ));
    let k = key(&store, "desktop", "main", "phoebis-0123456789abcdef");
    store.insert_api_key(&k, "test").unwrap();
    assert_eq!(
        store.api_key("desktop").unwrap().unwrap().prefix,
        "01234567"
    );
    store
        .update_pool(
            &PoolRow {
                body_capture_hours: 24,
                ..pool("main")
            },
            "test",
        )
        .unwrap();
    store
        .update_api_key(
            &ApiKeyRow {
                body_capture_hours: Some(6),
                ..k.clone()
            },
            "test",
        )
        .unwrap();
    assert_eq!(
        (
            store.pool("main").unwrap().unwrap().body_capture_hours,
            store
                .api_key("desktop")
                .unwrap()
                .unwrap()
                .body_capture_hours
        ),
        (24, Some(6))
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
fn a_catalog_sync_adds_new_ids_unselected_flags_vanished_ones_and_keeps_manual_rows() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    let first = store
        .apply_catalog_sync(
            "a",
            &[
                ("claude-sonnet-5".into(), false),
                ("claude-opus-5".into(), true),
            ],
            10,
            "catalog",
        )
        .unwrap();
    assert!(first.first_sync);
    assert!(store
        .served_models("a")
        .unwrap()
        .iter()
        .all(|m| m.selected && !m.is_new));
    store
        .put_served_models(
            "a",
            &[
                ServedModelWrite {
                    upstream_id: "claude-sonnet-5".into(),
                    selected: true,
                    long_context: false,
                },
                ServedModelWrite {
                    upstream_id: "claude-opus-5".into(),
                    selected: false,
                    long_context: true,
                },
                ServedModelWrite {
                    upstream_id: "Vendor/Model-A".into(),
                    selected: true,
                    long_context: false,
                },
            ],
            "admin",
        )
        .unwrap();
    let second = store
        .apply_catalog_sync(
            "a",
            &[
                ("claude-sonnet-5".into(), false),
                ("claude-haiku-4-5".into(), false),
            ],
            20,
            "catalog",
        )
        .unwrap();
    assert_eq!(second.added, ["claude-haiku-4-5"]);
    assert_eq!(second.missing, ["claude-opus-5"]);
    let rows = store.served_models("a").unwrap();
    let by_id = |id: &str| rows.iter().find(|m| m.upstream_id == id).unwrap();
    assert!(!by_id("claude-haiku-4-5").selected && by_id("claude-haiku-4-5").is_new);
    assert!(
        by_id("claude-opus-5").missing && !by_id("claude-opus-5").selected,
        "selected is left as the operator wrote it"
    );
    assert_eq!(by_id("Vendor/Model-A").source, ModelSource::Manual);
    assert_eq!(by_id("Vendor/Model-A").canonical_id, "vendor/model-a");
    assert!(matches!(
        store.put_served_models(
            "a",
            &[
                ServedModelWrite {
                    upstream_id: "Vendor/Model-A".into(),
                    selected: true,
                    long_context: false
                },
                ServedModelWrite {
                    upstream_id: "vendor/model-a".into(),
                    selected: true,
                    long_context: false
                },
            ],
            "admin"
        ),
        Err(StoreError::CanonicalClash { .. })
    ));
    assert!(
        !store
            .seed_catalog("a", &[("claude-fable-5-1".into(), true)], 30, "catalog")
            .unwrap(),
        "a seed never overwrites a populated catalog"
    );
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
    assert!(
        !store
            .set_state_if_generations("a", AccountState::ReauthRequired, 1, 1)
            .unwrap(),
        "a verdict about the replaced key changes nothing"
    );
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
    assert!(store
        .set_state_if_generations("a", AccountState::ReauthRequired, 1, 2)
        .unwrap());
}

#[test]
fn an_account_update_commits_secret_fields_and_catalog_together_or_not_at_all() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .create_account(
            &account("a", AccountKind::ApiKey),
            Some(&Secret::new("k1")),
            Some(&[("claude-sonnet-5".into(), false)]),
            None,
            1,
            "admin",
        )
        .unwrap();
    assert_eq!(
        (
            store.served_models("a").unwrap().len(),
            store.api_key_credential("a").unwrap().unwrap().1
        ),
        (1, 1)
    );
    let update = AccountUpdate {
        id: "a",
        lifecycle_generation: 1,
        label: "Renamed",
        secret: Some(&Secret::new("k2")),
        catalog: Some(&[("gpt-5".into(), false)]),
        catalog_error: None,
        state: AccountState::Ready,
        at: 5,
        actor: "admin",
    };
    store.commit_account_update(&update).unwrap();
    let row = store.account("a").unwrap().unwrap();
    assert_eq!(row.label.as_str(), "Renamed");
    assert_eq!(
        store.api_key_credential("a").unwrap().unwrap().0.expose(),
        "k2"
    );
    assert_eq!(
        store.served_models("a").unwrap().len(),
        2,
        "the catalog was applied as a sync"
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
}

#[test]
fn deleting_an_account_cascades_and_the_load_reads_everything() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store.insert_pool(&pool("main"), "test").unwrap();
    store
        .set_pool_members("main", &[("a".into(), 0)], "test")
        .unwrap();
    store
        .apply_catalog_sync("a", &[("claude-sonnet-5".into(), false)], 1, "catalog")
        .unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!(
        (
            load.accounts.len(),
            load.pools.len(),
            load.members.len(),
            load.models.len()
        ),
        (1, 1, 1, 1)
    );
    store.delete_account("a", "test").unwrap();
    let load = store.load_registry().unwrap();
    assert_eq!(
        (load.accounts.len(), load.members.len(), load.models.len()),
        (0, 0, 0)
    );
    assert!(matches!(
        store.delete_account("a", "test"),
        Err(StoreError::AccountNotFound(_))
    ));
}

#[test]
fn a_catalog_sync_keeps_a_manual_rows_source_and_selection() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .insert_account(&account("a", AccountKind::ApiKey))
        .unwrap();
    store
        .put_served_models(
            "a",
            &[ServedModelWrite {
                upstream_id: "Vendor/Model-A".into(),
                selected: false,
                long_context: false,
            }],
            "admin",
        )
        .unwrap();
    let first = store
        .apply_catalog_sync(
            "a",
            &[
                ("Vendor/Model-A".into(), true),
                ("claude-sonnet-5".into(), false),
            ],
            10,
            "catalog",
        )
        .unwrap();
    assert_eq!(first.added, ["claude-sonnet-5"]);
    let rows = store.served_models("a").unwrap();
    let by_id = |id: &str| rows.iter().find(|m| m.upstream_id == id).unwrap();
    let manual = by_id("Vendor/Model-A");
    assert_eq!(manual.source, ModelSource::Manual);
    assert!(
        !manual.selected,
        "the discovery reporting a manual row never changes its selection"
    );
    assert!(
        manual.long_context,
        "the discovery's long-context flag applies even to a manual row"
    );
    assert!(!manual.missing);
    let new_row = by_id("claude-sonnet-5");
    assert!(!new_row.selected && new_row.is_new);

    let second = store
        .apply_catalog_sync("a", &[("claude-sonnet-5".into(), false)], 20, "catalog")
        .unwrap();
    assert!(
        second.missing.is_empty(),
        "a manual row omitted from a sync is never flagged missing"
    );
    assert!(
        store
            .served_models("a")
            .unwrap()
            .iter()
            .any(|m| m.upstream_id == "Vendor/Model-A"),
        "a manual row omitted from a sync is never removed"
    );
}
