use phoebis_routing::{resolve, Candidate, KeyRefusal, RegistrySnapshot, ResolveError};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, ModelRule, OveragePolicy, PoolMember,
    PoolRow, RegistryLoad, RulesMode, Strategy, SubStrategy,
};

const DIGEST_KEY: [u8; 32] = [7; 32];

fn account(id: &str) -> AccountRow {
    AccountRow {
        id: id.into(),
        kind: AccountKind::ApiKey,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Ready,
    }
}

fn pool(id: &str, enabled: bool, rules: Vec<ModelRule>, body_capture_hours: u32) -> PoolRow {
    PoolRow {
        id: id.into(),
        label: id.into(),
        enabled,
        strategy: Strategy::PriorityRandom,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        model_rules: rules,
        keepalive_json: "{}".into(),
        body_capture_hours,
        prefer_subscription: false,
        api_key_sub_strategy: SubStrategy::PriorityRandom,
        cache_ttl_json: "{}".into(),
        cache_diagnostics: None,
        created_at: 0,
        updated_at: 0,
    }
}

fn key(id: &str, pool_id: &str, secret: &str, rules: Vec<ModelRule>, mode: RulesMode) -> ApiKeyRow {
    ApiKeyRow {
        id: id.into(),
        label: id.into(),
        notes: None,
        pool_id: pool_id.into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        model_rules: rules,
        model_rules_mode: mode,
        long_context_twins: true,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: phoebis_store::digest_with(&DIGEST_KEY, secret.as_bytes()).to_vec(),
        prefix: phoebis_store::key_prefix(secret),
        created_at: 0,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
    }
}

fn snapshot() -> RegistrySnapshot {
    let load = RegistryLoad {
        accounts: vec![account("a"), account("b"), account("f")],
        credential_generations: Vec::new(),
        keys: vec![
            key(
                "desktop",
                "main",
                "phoebis-desktopsecret0000",
                Vec::new(),
                RulesMode::Extend,
            ),
            ApiKeyRow {
                body_capture_hours: Some(6),
                ..key(
                    "laptop",
                    "main",
                    "phoebis-laptopsecret00000",
                    vec![
                        ModelRule {
                            pattern: "claude-opus-5".into(),
                            not: false,
                        },
                        ModelRule {
                            pattern: "*".into(),
                            not: true,
                        },
                    ],
                    RulesMode::Replace,
                )
            },
            ApiKeyRow {
                body_capture_hours: Some(0),
                ..key(
                    "quiet",
                    "main",
                    "phoebis-quietsecret000000",
                    Vec::new(),
                    RulesMode::Extend,
                )
            },
            ApiKeyRow {
                revoked_at: Some(1),
                enabled: false,
                ..key(
                    "gone",
                    "main",
                    "phoebis-gonesecret0000000",
                    Vec::new(),
                    RulesMode::Extend,
                )
            },
            ApiKeyRow {
                expires_at: Some(500),
                ..key(
                    "old",
                    "main",
                    "phoebis-oldsecret00000000",
                    Vec::new(),
                    RulesMode::Extend,
                )
            },
            key(
                "dark",
                "off",
                "phoebis-darksecret0000000",
                Vec::new(),
                RulesMode::Extend,
            ),
            key(
                "solo",
                "empty",
                "phoebis-solosecret0000000",
                Vec::new(),
                RulesMode::Extend,
            ),
        ],
        members: vec![
            PoolMember {
                pool_id: "main".into(),
                account_id: "a".into(),
                priority: 0,
            },
            PoolMember {
                pool_id: "main".into(),
                account_id: "b".into(),
                priority: 1,
            },
            PoolMember {
                pool_id: "main".into(),
                account_id: "f".into(),
                priority: 0,
            },
        ],
        pools: vec![
            pool(
                "main",
                true,
                vec![ModelRule {
                    pattern: "claude-haiku-4-5*".into(),
                    not: true,
                }],
                24,
            ),
            pool("off", false, Vec::new(), 0),
            pool("empty", true, Vec::new(), 0),
        ],
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
    };
    RegistrySnapshot::from_load(load, DIGEST_KEY, 1_000, 1)
}

#[test]
fn keys_authenticate_by_digest_and_every_dead_key_is_refused() {
    let s = snapshot();
    assert_eq!(
        s.authenticate("phoebis-desktopsecret0000", 1_000).unwrap().0.id,
        "desktop"
    );
    assert!(matches!(
        s.authenticate("phoebis-nope", 1_000),
        Err(KeyRefusal::Unknown)
    ));
    assert!(matches!(
        s.authenticate("phoebis-gonesecret0000000", 1_000),
        Err(KeyRefusal::Revoked)
    ));
    assert!(matches!(
        s.authenticate("phoebis-oldsecret00000000", 1_000),
        Err(KeyRefusal::Expired)
    ));
    assert_eq!(
        s.authenticate("phoebis-oldsecret00000000", 400).unwrap().0.id,
        "old"
    );
    assert!(matches!(
        s.authenticate("phoebis-darksecret0000000", 1_000),
        Err(KeyRefusal::PoolDisabled)
    ));
}

#[test]
fn body_capture_hours_is_the_key_override_else_the_pool_value() {
    let s = snapshot();
    assert_eq!(
        s.body_capture_hours(s.key("desktop").unwrap()),
        24,
        "null inherits the pool"
    );
    assert_eq!(
        s.body_capture_hours(s.key("laptop").unwrap()),
        6,
        "a number overrides"
    );
    assert_eq!(
        s.body_capture_hours(s.key("quiet").unwrap()),
        0,
        "zero disables"
    );
    assert_eq!(
        s.body_capture_hours(s.key("dark").unwrap()),
        0,
        "a pool with the default captures nothing"
    );
}

#[test]
fn every_member_is_a_candidate_in_priority_order_with_the_folded_spelling() {
    let s = snapshot();
    let desktop = s.key("desktop").unwrap();
    let ids = |c: Vec<Candidate>| {
        c.into_iter()
            .map(|c| (c.account_id, c.priority, c.upstream_model))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(resolve(&s, desktop, "vendor/model-a", "vendor/model-a").unwrap()),
        [
            ("a".to_string(), 0, "vendor/model-a".to_string()),
            ("f".to_string(), 0, "vendor/model-a".to_string()),
            ("b".to_string(), 1, "vendor/model-a".to_string()),
        ]
    );
    assert_eq!(
        ids(resolve(
            &s,
            desktop,
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5"
        )
        .unwrap()),
        [
            ("a".to_string(), 0, "claude-sonnet-4-5".to_string()),
            ("f".to_string(), 0, "claude-sonnet-4-5".to_string()),
            ("b".to_string(), 1, "claude-sonnet-4-5".to_string()),
        ]
    );
}

#[test]
fn the_processed_list_refuses_a_model_before_any_member_is_named() {
    let s = snapshot();
    let laptop = s.key("laptop").unwrap();
    let desktop = s.key("desktop").unwrap();
    assert!(
        matches!(
            resolve(&s, laptop, "claude-sonnet-5", "claude-sonnet-5"),
            Err(ResolveError::ModelNotAllowed)
        ),
        "laptop's own list refuses what it does not name"
    );
    assert!(
        matches!(
            resolve(&s, desktop, "claude-haiku-4-5", "claude-haiku-4-5"),
            Err(ResolveError::ModelNotAllowed)
        ),
        "the pool's entry applies when the key holds none of its own"
    );
}

#[test]
fn a_pool_with_no_member_answers_an_empty_list() {
    let s = snapshot();
    let solo = s.key("solo").unwrap();
    assert_eq!(
        resolve(&s, solo, "claude-sonnet-5", "claude-sonnet-5").unwrap(),
        Vec::new()
    );
}
