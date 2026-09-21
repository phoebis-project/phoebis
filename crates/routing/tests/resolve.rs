use phoebis_routing::{resolve, KeyRefusal, RegistrySnapshot, ResolveError};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, ModelSource, OveragePolicy, PoolMember,
    PoolRow, RegistryLoad, ServedModel, Strategy, SubStrategy,
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

fn pool(
    id: &str,
    enabled: bool,
    default_allowed: Option<Vec<&str>>,
    body_capture_hours: u32,
) -> PoolRow {
    PoolRow {
        id: id.into(),
        label: id.into(),
        enabled,
        strategy: Strategy::PriorityRandom,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        default_allowed_models: default_allowed.map(|l| l.into_iter().map(String::from).collect()),
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

fn key(id: &str, pool_id: &str, secret: &str, allowed: Option<Vec<&str>>) -> ApiKeyRow {
    ApiKeyRow {
        id: id.into(),
        label: id.into(),
        notes: None,
        pool_id: pool_id.into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        allowed_models: allowed.map(|l| l.into_iter().map(String::from).collect()),
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

fn served(account: &str, upstream: &str, selected: bool) -> ServedModel {
    ServedModel {
        account_id: account.into(),
        upstream_id: upstream.into(),
        canonical_id: phoebis_store::canonical_id(upstream),
        selected,
        source: ModelSource::Discovered,
        long_context: false,
        missing: false,
        is_new: false,
    }
}

fn snapshot() -> RegistrySnapshot {
    let load = RegistryLoad {
        accounts: vec![account("a"), account("b"), account("f")],
        credential_generations: Vec::new(),
        keys: vec![
            key("desktop", "main", "phoebis-desktopsecret0000", None),
            ApiKeyRow {
                body_capture_hours: Some(6),
                ..key(
                    "laptop",
                    "main",
                    "phoebis-laptopsecret00000",
                    Some(vec!["claude-opus-5"]),
                )
            },
            ApiKeyRow {
                body_capture_hours: Some(0),
                ..key("quiet", "main", "phoebis-quietsecret000000", None)
            },
            ApiKeyRow {
                revoked_at: Some(1),
                enabled: false,
                ..key("gone", "main", "phoebis-gonesecret0000000", None)
            },
            ApiKeyRow {
                expires_at: Some(500),
                ..key("old", "main", "phoebis-oldsecret00000000", None)
            },
            key("dark", "off", "phoebis-darksecret0000000", None),
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
        models: vec![
            served("a", "claude-sonnet-5", true),
            served("a", "claude-opus-5", true),
            served("b", "claude-sonnet-5", true),
            served("b", "claude-opus-5", false),
            served("f", "Vendor/Model-A", true),
        ],
        pools: vec![
            pool(
                "main",
                true,
                Some(vec!["claude-sonnet-5", "claude-opus-5", "vendor/model-a"]),
                24,
            ),
            pool("off", false, None, 0),
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
fn the_key_list_overrides_the_pool_list_and_unserved_models_are_refused() {
    let s = snapshot();
    let desktop = s.key("desktop").unwrap();
    let laptop = s.key("laptop").unwrap();
    let ids = |c: Vec<phoebis_routing::Candidate>| {
        c.into_iter()
            .map(|c| (c.account_id, c.priority, c.upstream_model))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(resolve(&s, desktop, "claude-sonnet-5").unwrap()),
        [
            ("a".to_string(), 0, "claude-sonnet-5".to_string()),
            ("b".into(), 1, "claude-sonnet-5".into())
        ]
    );
    assert_eq!(
        ids(resolve(&s, desktop, "claude-opus-5").unwrap()),
        [("a".to_string(), 0, "claude-opus-5".to_string())],
        "b's row is not selected"
    );
    assert!(matches!(
        resolve(&s, laptop, "claude-sonnet-5"),
        Err(ResolveError::ModelNotAllowed)
    ));
    assert!(
        matches!(
            resolve(&s, desktop, "claude-haiku-4-5"),
            Err(ResolveError::ModelNotAllowed)
        ),
        "the pool list applies when the key has none"
    );
    assert!(
        matches!(resolve(&s, desktop, "vendor/model-a"), Ok(ref c) if c.len() == 1 && c[0].upstream_model == "Vendor/Model-A"),
        "the provider's spelling travels with the candidate"
    );
    let open = RegistrySnapshot::from_load(
        RegistryLoad {
            pools: vec![pool("main", true, None, 24), pool("off", false, None, 0)],
            retired_secrets: Vec::new(),
            ..load_of(&snapshot())
        },
        DIGEST_KEY,
        1,
        2,
    );
    assert!(matches!(
        resolve(&open, open.key("desktop").unwrap(), "claude-haiku-4-5"),
        Err(ResolveError::ModelUnserved)
    ));
}

/// Rebuilds the load a snapshot was made from, for the one test that varies a pool.
fn load_of(s: &RegistrySnapshot) -> RegistryLoad {
    RegistryLoad {
        accounts: s.accounts().cloned().collect(),
        credential_generations: s
            .accounts()
            .filter_map(|a| Some((a.id.clone(), s.credential_generation(&a.id)?)))
            .collect(),
        keys: s.keys().cloned().collect(),
        members: s.pools().flat_map(|p| s.members(&p.id).to_vec()).collect(),
        models: s
            .accounts()
            .flat_map(|a| {
                ["claude-sonnet-5", "claude-opus-5", "vendor/model-a"]
                    .into_iter()
                    .filter_map(|m| s.served(&a.id, m).cloned())
            })
            .collect(),
        pools: s.pools().cloned().collect(),
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
    }
}
