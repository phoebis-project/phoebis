use phoebis_guardrails::{Condition, GuardrailAction, GuardrailMode, Operator, Selector, Subject};
use phoebis_routing::{CompiledGuardrails, KeyRefusal, Presented, RefusedKey, RegistrySnapshot};
use phoebis_store::{
    digest_with, ApiKeyRow, AttachmentSubject, CompanionPermissions, GuardrailAttachment,
    GuardrailRow, KeyFeatureAccounts, PoolRow, RegistryLoad, RetiredSecret, RulesMode, Strategy,
    SubscriptionTerm,
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
            model_rules: Vec::new(),
            model_rules_mode: RulesMode::Extend,
            long_context_twins: true,
            companion_permissions: CompanionPermissions::default(),
            body_capture_hours: None,
            secret_digest: digest_with(&DIGEST_KEY, b"two").to_vec(),
            prefix: "00000000".into(),
            incarnation: 1,
            created_at: 0,
            updated_at: 0,
            rotated_at: None,
            last_used_at: None,
            retired_secret_last_used_at: None,
            auto_mode_server: None,
            feature_accounts: KeyFeatureAccounts::default(),
        }],
        members: Vec::new(),
        pools: vec![PoolRow {
            strategy: Strategy::Priority,
            prefer_subscription: false,
            ..PoolRow::for_tests("p")
        }],
        retired_secrets: vec![RetiredSecret {
            key_id: "k".into(),
            secret_digest: digest_with(&DIGEST_KEY, b"one").to_vec(),
            prefix: "00000000".into(),
            retired_at: 1_000,
            expires_at: 2_000,
        }],
        reused_key_ids: Vec::new(),
        ..Default::default()
    }
}

#[test]
fn a_retired_secret_authenticates_its_key_until_the_grace_ends() {
    let s = RegistrySnapshot::from_load(load(None), DIGEST_KEY, 0, 1);
    assert_eq!(s.authenticate("one", 1_999).unwrap().0.id, "k");
    assert_eq!(s.authenticate("one", 1_999).unwrap().1, Presented::Retired);
    assert!(matches!(
        s.authenticate("one", 2_000),
        Err(RefusedKey {
            refusal: KeyRefusal::Unknown,
            ..
        })
    ));
    assert!(
        s.authenticate("two", 5_000).is_ok(),
        "the live secret needs no grace"
    );
    assert_eq!(s.authenticate("two", 5_000).unwrap().1, Presented::Live);
    assert!(matches!(
        s.authenticate("three", 1_000),
        Err(RefusedKey {
            refusal: KeyRefusal::Unknown,
            ..
        })
    ));
}

#[test]
fn a_retired_secret_of_a_revoked_key_is_refused_as_revoked() {
    let s = RegistrySnapshot::from_load(load(Some(1_500)), DIGEST_KEY, 0, 1);
    assert!(matches!(
        s.authenticate("one", 1_600),
        Err(RefusedKey {
            refusal: KeyRefusal::Revoked,
            ..
        })
    ));
}

#[test]
fn priced_tier_answers_the_stored_tier_for_a_plan_the_cost_model_prices_and_none_for_any_other() {
    // One tier the cost table holds on every term, so the plan word alone decides each reading. The table judges a
    // Team or Enterprise word itself, so those plans answer the word.
    const TIER: &str = "default_claude_max_20x";
    let expected = [
        ("free", None),
        ("pro", Some(TIER)),
        ("max_5x", Some(TIER)),
        ("max_20x", Some(TIER)),
        ("max", Some(TIER)),
        ("team", Some(TIER)),
        ("enterprise", Some(TIER)),
        ("usage_based", None),
    ];
    let term = |id: &str, plan: Option<&str>| SubscriptionTerm {
        account_id: id.into(),
        tier: Some(TIER.into()),
        plan: plan.map(String::from),
        ..SubscriptionTerm::default()
    };
    let mut terms: Vec<_> = expected
        .iter()
        .map(|&(word, _)| term(word, Some(word)))
        .collect();
    terms.push(term("unplanned", None));
    let s = RegistrySnapshot::from_load(
        RegistryLoad {
            terms,
            ..load(None)
        },
        DIGEST_KEY,
        0,
        1,
    );
    for (word, priced) in expected {
        assert_eq!(s.plan(word), Some(word), "the term reads its own plan word");
        assert_eq!(s.priced_tier(word), priced, "the {word} plan's priced tier");
    }
    assert_eq!(
        (s.plan("unplanned"), s.priced_tier("unplanned")),
        (None, None),
        "a term with no plan prices nothing, whatever its tier"
    );
}

/// One `last_user` message condition over `pattern`, so a pattern the compiler refuses fails the whole row.
fn guardrail(id: &str, enabled: bool, pattern: &str) -> GuardrailRow {
    GuardrailRow {
        id: id.into(),
        name: format!("{id} name"),
        description: String::new(),
        enabled,
        mode: GuardrailMode::Observe,
        action: GuardrailAction::Block,
        conditions: vec![Condition {
            subject: Subject::Message,
            operator: Operator::Matches,
            value: Some(pattern.into()),
            name: None,
            path: None,
            selector: Some(Selector::LastUser),
        }],
        preset: None,
        incarnation: 1,
        created_at: 0,
        updated_at: 0,
    }
}

fn attached(guardrail_id: &str, subject: AttachmentSubject, position: u32) -> GuardrailAttachment {
    GuardrailAttachment {
        guardrail_id: guardrail_id.into(),
        subject,
        position,
    }
}

/// `k1` in pool `p` and `k2` in pool `q`. `g0` sits last on `p`, `g1` on both `p` and `k1`, `g3` is disabled, `g4`'s
/// lookaround is a pattern the compiler refuses, and `g5` is attached to nothing.
fn guarded_load() -> RegistryLoad {
    let base = load(None);
    let key = |id: &str, pool_id: &str| ApiKeyRow {
        id: id.into(),
        label: id.into(),
        pool_id: pool_id.into(),
        secret_digest: digest_with(&DIGEST_KEY, id.as_bytes()).to_vec(),
        ..base.keys[0].clone()
    };
    let pool = |id: &str| PoolRow {
        strategy: Strategy::Priority,
        prefer_subscription: false,
        ..PoolRow::for_tests(id)
    };
    let p = || AttachmentSubject::Pool("p".into());
    let k1 = || AttachmentSubject::Key("k1".into());
    RegistryLoad {
        // Kind first as the load answers them, so a build in this order lists the key's rows ahead of its pool's.
        // `g0` at the pool's last position is written ahead of the pool's first, so a build that skips the sort by
        // position lists it first, and its id sorts first, so a build in id order does too.
        guardrail_attachments: vec![
            attached("g2", k1(), 0),
            attached("g1", k1(), 1),
            attached("g0", p(), 3),
            attached("g1", p(), 0),
            attached("g3", p(), 1),
            attached("g4", p(), 2),
        ],
        guardrails: vec![
            guardrail("g0", true, "zero"),
            guardrail("g1", true, "one"),
            guardrail("g2", true, "two"),
            guardrail("g3", false, "three"),
            guardrail("g4", true, "(?=x)"),
            guardrail("g5", true, "five"),
        ],
        keys: vec![key("k1", "p"), key("k2", "q")],
        pools: vec![pool("p"), pool("q")],
        ..base
    }
}

fn guarded() -> RegistrySnapshot {
    RegistrySnapshot::from_load(guarded_load(), DIGEST_KEY, 0, 1)
        .with_guardrails(CompiledGuardrails::build(&guarded_load()))
}

fn ids_for<'a>(s: &'a RegistrySnapshot, key_id: &str) -> Vec<&'a str> {
    s.guardrails_for(s.key(key_id).unwrap())
        .iter()
        .map(|g| g.id.as_str())
        .collect()
}

#[test]
fn build_lists_a_keys_pool_guardrails_before_its_own_and_a_guardrail_on_both_once_at_its_pool_position(
) {
    assert_eq!(ids_for(&guarded(), "k1"), ["g1", "g0", "g2"]);
}

#[test]
fn build_keeps_the_names_of_a_disabled_guardrail_one_whose_pattern_does_not_compile_and_one_attached_to_nothing(
) {
    let s = guarded();
    let names = s.guardrails();
    assert_eq!(names.name_of("g3"), Some("g3 name"));
    assert_eq!(names.name_of("g4"), Some("g4 name"));
    assert_eq!(
        names.name_of("g5"),
        Some("g5 name"),
        "a row attached to nothing keeps its name"
    );
    assert_eq!(names.name_of("ghost"), None);
}

#[test]
fn guardrails_for_answers_an_empty_slice_for_a_key_without_attachments() {
    let s = guarded();
    assert!(s.guardrails_for(s.key("k2").unwrap()).is_empty());
}
