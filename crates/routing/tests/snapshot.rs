use phoebis_routing::{KeyRefusal, RegistrySnapshot};
use phoebis_store::{
    digest_with, ApiKeyRow, OveragePolicy, PoolRow, RegistryLoad, RetiredSecret, Strategy,
    SubStrategy,
};

const DIGEST_KEY: [u8; 32] = [0; 32];

fn load(revoked_at: Option<i64>) -> RegistryLoad {
    RegistryLoad {
        accounts: Vec::new(),
        credential_generations: Vec::new(),
        keys: vec![ApiKeyRow {
            id: "k".into(),
            label: "k".into(),
            notes: None,
            pool_id: "p".into(),
            enabled: true,
            revoked_at,
            expires_at: None,
            allowed_models: None,
            keepalive_opt_out: false,
            body_capture_hours: None,
            secret_digest: digest_with(&DIGEST_KEY, b"two").to_vec(),
            prefix: "00000000".into(),
            created_at: 0,
            rotated_at: None,
            last_used_at: None,
        }],
        members: Vec::new(),
        models: Vec::new(),
        pools: vec![PoolRow {
            id: "p".into(),
            label: "p".into(),
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
            created_at: 0,
            updated_at: 0,
        }],
        retired_secrets: vec![RetiredSecret {
            key_id: "k".into(),
            secret_digest: digest_with(&DIGEST_KEY, b"one").to_vec(),
            prefix: "00000000".into(),
            retired_at: 1_000,
            expires_at: 2_000,
        }],
        reused_key_ids: Vec::new(),
    }
}

#[test]
fn a_usage_floor_stands_only_where_an_id_had_a_holder_before_this_one() {
    let mut load = load(None);
    load.keys.push(ApiKeyRow {
        id: "reused".into(),
        created_at: 9_000,
        secret_digest: digest_with(&DIGEST_KEY, b"three").to_vec(),
        ..load.keys[0].clone()
    });
    load.reused_key_ids = vec!["reused".into()];
    let s = RegistrySnapshot::from_load(load, DIGEST_KEY, 0, 1);
    assert_eq!(
        s.usage_floor("reused"),
        Some(9_000),
        "the current holder's creation is the line between it and the key that had the id before"
    );
    assert_eq!(
        s.usage_floor("k"),
        None,
        "the first holder of an id reads from wherever the window starts"
    );
    assert_eq!(
        s.usage_floor("gone"),
        None,
        "an id this snapshot does not hold has no floor to report"
    );
}

#[test]
fn a_retired_secret_authenticates_its_key_until_the_grace_ends() {
    let s = RegistrySnapshot::from_load(load(None), DIGEST_KEY, 0, 1);
    assert_eq!(s.authenticate("one", 1_999).unwrap().id, "k");
    assert!(matches!(
        s.authenticate("one", 2_000),
        Err(KeyRefusal::Unknown)
    ));
    assert!(
        s.authenticate("two", 5_000).is_ok(),
        "the live secret needs no grace"
    );
    assert!(matches!(
        s.authenticate("three", 1_000),
        Err(KeyRefusal::Unknown)
    ));
}

#[test]
fn a_retired_secret_of_a_revoked_key_is_refused_as_revoked() {
    let s = RegistrySnapshot::from_load(load(Some(1_500)), DIGEST_KEY, 0, 1);
    assert!(matches!(
        s.authenticate("one", 1_600),
        Err(KeyRefusal::Revoked)
    ));
}
