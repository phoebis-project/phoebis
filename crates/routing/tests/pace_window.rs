//! The window a Pace score names on the record: the model's week for a request whose model has one, the shared 7-day
//! window otherwise, and none for an api_key member, which its worst bucket scores.
use std::sync::Arc;

use phoebis_routing::{
    decide, lane_key, resolve, ClientClass, DecisionRecord, PaceFacts, Portability,
    RegistrySnapshot, RoutingState, SelectRequest, UtilizationSample, WindowReading, WindowSource,
    FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{
    AccountKind, AccountRow, ApiKeyRow, CompanionPermissions, KeyFeatureAccounts, PoolMember,
    PoolRow, RegistryLoad, RoutingLoad, RulesMode, Strategy,
};
use rand::rngs::StdRng;
use rand::SeedableRng;

const HOUR: i64 = 3_600_000;

/// Key `k` on Pace pool `p` over the subscription members `a` and `b` and the api_key member `c`.
fn snapshot() -> RegistrySnapshot {
    let account = |(id, kind): (&str, AccountKind)| AccountRow::for_tests(id, kind);
    let load = RegistryLoad {
        accounts: [
            ("a", AccountKind::Oauth),
            ("b", AccountKind::Oauth),
            ("c", AccountKind::ApiKey),
        ]
        .map(account)
        .to_vec(),
        credential_generations: Vec::new(),
        keys: vec![ApiKeyRow {
            id: "k".into(),
            label: "k".into(),
            notes: None,
            pool_id: "p".into(),
            enabled: true,
            revoked_at: None,
            expires_at: None,
            model_rules: Vec::new(),
            model_rules_mode: RulesMode::Extend,
            long_context_twins: true,
            companion_permissions: CompanionPermissions::default(),
            body_capture_hours: None,
            secret_digest: vec![0; 32],
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
        members: ["a", "b", "c"]
            .map(|id| PoolMember {
                pool_id: "p".into(),
                account_id: id.into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            })
            .to_vec(),
        pools: vec![PoolRow {
            strategy: Strategy::Pace,
            ..PoolRow::for_tests("p")
        }],
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
        terms: Vec::new(),
        ..Default::default()
    };
    RegistrySnapshot::from_load(load, [0; 32], 0, 1)
}

/// A fresh selection on key `k` for `model` under a session that holds no pin yet.
fn request(s: &RegistrySnapshot, model: &str, now: i64) -> SelectRequest {
    let key = s.key("k").unwrap();
    SelectRequest {
        request_id: "r-1".into(),
        attempt: 1,
        seq: 1,
        key_id: "k".into(),
        key_generation: key.created_at,
        agent_id: "main".into(),
        session_id: "s1".into(),
        canonical_model: model.into(),
        folded_model: model.into(),
        candidates: resolve(s, key, model, model).unwrap(),
        portability: Portability::Portable,
        credit_account: None,
        attempted: Vec::new(),
        now,
        received_at: now,
        fallback_ms: 60_000,
        revision: s.revision,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        boundaries: Arc::from([]),
        client: ClientClass::ClaudeCode,
        entrypoint: None,
        static_boundaries: Arc::from([]),
        marker: false,
        parent_agent_id: None,
        failed_over: None,
        pin_ended: false,
        holds: Vec::new(),
        dialed: false,
        held_out: false,
        lane: lane_key(model, "standard", "none", "-"),
        fast: false,
        classifier: false,
        body_tokens: 0,
        effort: None,
    }
}

fn reading(utilization: f64, reset: Option<i64>, observed_at: i64) -> WindowReading {
    WindowReading {
        utilization,
        reset,
        observed_at,
        source: WindowSource::Head,
        plan: None,
        tier: None,
    }
}

/// The shared 5-hour and 7-day windows a subscription member reports, with no reset.
fn sample(five_hour: f64, seven_day: f64, observed_at: i64) -> UtilizationSample {
    UtilizationSample {
        observed_at,
        ..Default::default()
    }
    .with(FIVE_HOUR, reading(five_hour, None, observed_at))
    .with(SEVEN_DAY, reading(seven_day, None, observed_at))
}

/// The Fable week and its reset over a sample that already carries the shared windows.
fn with_fable(base: UtilizationSample, fable: f64, fable_reset: Option<i64>) -> UtilizationSample {
    let observed_at = base.observed_at;
    base.with(SEVEN_DAY_FABLE, reading(fable, fable_reset, observed_at))
}

fn rng() -> StdRng {
    StdRng::seed_from_u64(7)
}

fn pace_of<'a>(record: &'a DecisionRecord, account: &str) -> &'a PaceFacts {
    record
        .candidates
        .iter()
        .find(|c| c.account_id == account)
        .and_then(|c| c.pace.as_ref())
        .unwrap_or_else(|| panic!("{account} carries no pace facts"))
}

fn window_of(record: &DecisionRecord, account: &str) -> Option<String> {
    pace_of(record, account).window.clone()
}

#[test]
fn a_fable_request_scores_its_members_on_the_model_week_and_names_it() {
    let s = snapshot();
    let mut st = RoutingState::from_load(RoutingLoad::default());
    for id in ["a", "b"] {
        st.record_sample(
            id,
            with_fable(sample(0.2, 0.3, 1_000), 0.4, Some(1_000 + 3 * HOUR)),
            1_000,
        );
    }
    let d = decide(
        &mut st,
        &s,
        &request(&s, "claude-fable-5-1", 1_000),
        &mut rng(),
    );
    for id in ["a", "b"] {
        assert_eq!(window_of(&d.record, id).as_deref(), Some(SEVEN_DAY_FABLE));
        let pace = pace_of(&d.record, id);
        assert!(
            (pace.remaining - 0.6).abs() < 1e-9,
            "{id} reads the Fable week's 0.4 used, not the shared week's 0.3"
        );
        assert_eq!(pace.reset, Some(1_000 + 3 * HOUR));
    }
}

#[test]
fn a_haiku_request_scores_its_members_on_the_seven_day_window_and_names_it() {
    let s = snapshot();
    let mut st = RoutingState::from_load(RoutingLoad::default());
    for id in ["a", "b"] {
        st.record_sample(id, sample(0.2, 0.3, 1_000), 1_000);
    }
    let d = decide(
        &mut st,
        &s,
        &request(&s, "claude-haiku-4-5", 1_000),
        &mut rng(),
    );
    for id in ["a", "b"] {
        assert_eq!(window_of(&d.record, id).as_deref(), Some(SEVEN_DAY));
        assert!((pace_of(&d.record, id).remaining - 0.7).abs() < 1e-9);
    }
}

#[test]
fn an_api_key_member_names_no_window_since_its_worst_bucket_scores_it() {
    let s = snapshot();
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample(
        "c",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            "itpm:claude-haiku-4-5",
            reading(0.3, Some(1_000 + 60_000), 1_000),
        ),
        1_000,
    );
    let d = decide(
        &mut st,
        &s,
        &request(&s, "claude-haiku-4-5", 1_000),
        &mut rng(),
    );
    let pace = pace_of(&d.record, "c");
    assert_eq!(pace.window, None);
    assert_eq!(pace.hours, None);
    assert!(
        (pace.remaining - 0.7).abs() < 1e-9,
        "the worst bucket's share left scores it"
    );
}

#[test]
fn a_fable_request_whose_member_has_not_read_the_model_week_scores_it_untouched_on_that_week() {
    let s = snapshot();
    let mut st = RoutingState::from_load(RoutingLoad::default());
    for id in ["a", "b"] {
        st.record_sample(id, sample(0.2, 0.3, 1_000), 1_000);
    }
    let d = decide(
        &mut st,
        &s,
        &request(&s, "claude-fable-5-1", 1_000),
        &mut rng(),
    );
    for id in ["a", "b"] {
        assert_eq!(
            window_of(&d.record, id).as_deref(),
            Some(SEVEN_DAY_FABLE),
            "{id} names the model week it has not read, never the shared week"
        );
        let pace = pace_of(&d.record, id);
        assert_eq!((pace.remaining, pace.reset), (1.0, None));
    }
}
