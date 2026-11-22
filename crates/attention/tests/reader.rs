//! The twenty-two conditions through the reader: each kind's hold and clear, the series at 0 for a subject in the
//! clear, the router's own components among them, a deleted subject's series gone unless a parked record still names
//! it, a refresh run read back by a second reader, a listener's pair read on the read's clock, an unpriced model the
//! ledger's landing saw, the keepalive engine's standing, and the gauge the metrics listener scrapes from the same
//! read.
mod common;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use http_body_util::BodyExt;
use phoebis_attention::{
    Attention, AttentionError, Condition, KeepaliveCap, Kind, Link, PairFacts, Subject,
    SubjectKind, BACKOFF_ATTENTION_AFTER_MS,
};
use phoebis_bus::{Kind as RequestKind, Origin, Outcome, QUEUE_DEPTH};
use phoebis_ledger::{AttemptRecord, Metrics, PriceRevisions, Pricing, Record, RequestRecord};
use phoebis_oauth::PollOutcome;
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AccountUpdate, ApiKeyRow, AttemptOutcome, Cancellation,
    FeatureOverride, KeyFeatureAccounts, MemberWrite, NewBundle, PoolRow, Secret, StateCause,
    StorageKey, Store,
};
use serde_json::json;
use tower::ServiceExt;

/// The condition of `kind` about `subject`, if the read carries a series for it.
fn find<'a>(conditions: &'a [Condition], kind: Kind, subject: &str) -> Option<&'a Condition> {
    conditions
        .iter()
        .find(|c| c.kind == kind && c.subject.id == subject)
}

async fn read(attention: &Arc<Attention>, at: i64) -> Vec<Condition> {
    attention.conditions(at).await.unwrap()
}

/// Every series of the read as `(kind, subject, holds)`, sorted, since the pools and the drift records come in
/// hash order.
fn series(conditions: &[Condition]) -> Vec<(&'static str, String, bool)> {
    let mut out: Vec<_> = conditions
        .iter()
        .map(|c| (c.kind.as_str(), c.subject.id.clone(), c.holds))
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn every_subject_a_kind_applies_to_has_a_series_at_zero_while_nothing_holds() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    assert_eq!(
        series(&read(&attention, NOW).await),
        [
            ("destination_spent", "business".to_string(), false),
            ("expired_between_turns", "keepalive".to_string(), false),
            ("feature_account_unavailable", "business".to_string(), false),
            ("funding_required", "direct".to_string(), false),
            ("keepalive_bus_gaps", "keepalive".to_string(), false),
            ("ledger_record_parked", "main".to_string(), false),
            ("limit_reset_ending", "business".to_string(), false),
            ("meter_drift", "business".to_string(), false),
            ("poll_backoff", "business".to_string(), false),
            ("pool_exhausted", "main".to_string(), false),
            ("reauth_required", "business".to_string(), false),
            ("reauth_required", "direct".to_string(), false),
            ("refresh_failed", "business".to_string(), false),
            ("shed_no_runway", "main".to_string(), false),
            ("slowed_long", "business".to_string(), false),
            ("wall_reached", "business".to_string(), false),
            ("writer_degraded", "ledger".to_string(), false),
            ("writer_degraded", "routing".to_string(), false),
        ],
        "every account carries its re-auth state, an API key its funding, and an OAuth account its refresh run, \
         its refused feature selections, its spent destinations, its walls, its slowed spans and its meter drift and, \
         before any poll status exists, \
         its backoff and, as an account the poll asks, its limit resets about to end, \
         while every pool carries its parked records and its rounds without runway, each writer its state, \
         and the keepalive engine its bus gaps and its expiries between turns"
    );
}

#[tokio::test]
async fn each_loaded_pair_has_a_series_of_both_certificate_kinds_at_zero_while_nothing_holds() {
    let rig = rig().await;
    let pairs = Arc::new(Pairs::default());
    pairs.set(vec![
        pair("proxy.tls", NOW + 90 * DAY),
        pair("admin.tls", NOW + 90 * DAY),
    ]);
    let (attention, _arm) = rig.attention_with_pairs(None, Some(pairs));
    assert_eq!(
        series(&read(&attention, NOW).await),
        [
            ("certificate_expiring", "admin.tls".to_string(), false),
            ("certificate_expiring", "proxy.tls".to_string(), false),
            ("certificate_reload_failed", "admin.tls".to_string(), false),
            ("certificate_reload_failed", "proxy.tls".to_string(), false),
            ("destination_spent", "business".to_string(), false),
            ("expired_between_turns", "keepalive".to_string(), false),
            ("feature_account_unavailable", "business".to_string(), false),
            ("funding_required", "direct".to_string(), false),
            ("keepalive_bus_gaps", "keepalive".to_string(), false),
            ("ledger_record_parked", "main".to_string(), false),
            ("limit_reset_ending", "business".to_string(), false),
            ("meter_drift", "business".to_string(), false),
            ("poll_backoff", "business".to_string(), false),
            ("pool_exhausted", "main".to_string(), false),
            ("reauth_required", "business".to_string(), false),
            ("reauth_required", "direct".to_string(), false),
            ("refresh_failed", "business".to_string(), false),
            ("shed_no_runway", "main".to_string(), false),
            ("slowed_long", "business".to_string(), false),
            ("wall_reached", "business".to_string(), false),
            ("writer_degraded", "ledger".to_string(), false),
            ("writer_degraded", "routing".to_string(), false),
        ],
        "each listener with a pair carries both certificate series beside the rest"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="certificate_expiring",subject="proxy.tls"} 0"#)
            && text.contains(
                r#"phoebis_attention{kind="certificate_reload_failed",subject="admin.tls"} 0"#
            ),
        "{text}"
    );
}

#[tokio::test]
async fn a_pool_exhaustion_holds_from_its_first_exhausted_outcome_until_a_request_of_the_pool_succeeds(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    for (id, at) in [("r1", NOW + 1_000), ("r2", NOW + 2_000)] {
        attention.observe(&observed(id, Some("desktop"), "main"));
        attention.observe(&complete(id, Outcome::Exhausted, None, None, at));
    }
    let conditions = read(&attention, NOW + 3_000).await;
    let held = find(&conditions, Kind::PoolExhausted, "main").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.pools,
            &held.link,
            &held.detail
        ),
        (
            true,
            Some(NOW + 1_000),
            &vec!["main".to_string()],
            &Some(Link {
                page: "pools",
                id: "main".into()
            }),
            &json!({ "key": "desktop", "pool": "main" })
        ),
        "the run dates from its first exhaustion"
    );
    assert_eq!(held.subject.label, "Main");
    // A request that failed or was rejected says nothing about whether the pool can serve.
    for (id, outcome) in [("r3", Outcome::Failed), ("r4", Outcome::Rejected)] {
        attention.observe(&observed(id, Some("desktop"), "main"));
        attention.observe(&complete(id, outcome, None, None, NOW + 4_000));
    }
    assert!(
        find(
            &read(&attention, NOW + 4_000).await,
            Kind::PoolExhausted,
            "main"
        )
        .unwrap()
        .holds
    );
    attention.observe(&observed("r5", Some("desktop"), "main"));
    attention.observe(&complete("r5", Outcome::Succeeded, None, None, NOW + 5_000));
    let cleared = read(&attention, NOW + 5_000).await;
    let cleared = find(&cleared, Kind::PoolExhausted, "main").unwrap();
    assert_eq!(
        (cleared.holds, cleared.since),
        (false, None),
        "a success clears it, and the series stays at 0"
    );
}

#[tokio::test]
async fn an_exhausted_completion_holds_its_pool_with_no_observation_before_it() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    attention.observe(&complete("r1", Outcome::Exhausted, None, None, NOW));
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::PoolExhausted, "main").unwrap();
    assert_eq!(
        (held.holds, held.since, &held.detail),
        (
            true,
            Some(NOW),
            &json!({ "key": "desktop", "pool": "main" })
        )
    );
}

#[tokio::test]
async fn a_drift_refusal_holds_until_a_request_from_that_version_is_relayed_and_keeps_its_series() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    assert!(
        find(&read(&attention, NOW).await, Kind::ClientDrift, "2.1.274").is_none(),
        "a version never refused has no series"
    );
    attention.observe(&observed("d1", Some("desktop"), "main"));
    attention.observe(&complete(
        "d1",
        Outcome::Rejected,
        Some("hash"),
        Some("2.1.274"),
        NOW + 1_000,
    ));
    let conditions = read(&attention, NOW + 1_000).await;
    let drift = find(&conditions, Kind::ClientDrift, "2.1.274").unwrap();
    assert_eq!(
        (
            drift.holds,
            drift.since,
            &drift.pools,
            &drift.link,
            &drift.detail
        ),
        (
            true,
            Some(NOW + 1_000),
            &vec!["main".to_string()],
            &Some(Link {
                page: "keys",
                id: "desktop".into()
            }),
            &json!({ "client_version": "2.1.274", "key": "desktop", "trigger": "hash" })
        )
    );
    // A request of that version refused for another reason, and one of another version relayed, leave it held.
    attention.observe(&observed("d2", Some("desktop"), "main"));
    attention.observe(&complete(
        "d2",
        Outcome::Failed,
        None,
        Some("2.1.274"),
        NOW + 2_000,
    ));
    attention.observe(&observed("d3", Some("desktop"), "main"));
    attention.observe(&complete(
        "d3",
        Outcome::Succeeded,
        None,
        Some("2.1.280"),
        NOW + 2_000,
    ));
    assert!(
        find(
            &read(&attention, NOW + 2_000).await,
            Kind::ClientDrift,
            "2.1.274"
        )
        .unwrap()
        .holds
    );
    attention.observe(&observed("d4", Some("desktop"), "main"));
    attention.observe(&complete(
        "d4",
        Outcome::Succeeded,
        None,
        Some("2.1.274"),
        NOW + 3_000,
    ));
    let conditions = read(&attention, NOW + 3_000).await;
    let cleared = find(&conditions, Kind::ClientDrift, "2.1.274")
        .expect("a cleared version keeps its series");
    assert_eq!((cleared.holds, cleared.since), (false, None));
    attention.observe(&observed("d5", Some("desktop"), "main"));
    attention.observe(&complete(
        "d5",
        Outcome::Rejected,
        Some("version_suffix"),
        Some("2.1.274"),
        NOW + 9_000,
    ));
    let conditions = read(&attention, NOW + 9_000).await;
    let again = find(&conditions, Kind::ClientDrift, "2.1.274").unwrap();
    assert_eq!(
        (again.holds, again.since, &again.detail["trigger"]),
        (true, Some(NOW + 9_000), &json!("version_suffix")),
        "a refusal after a clear starts a new run"
    );
}

#[tokio::test]
async fn a_keyless_drift_refusal_names_no_key_and_carries_no_link() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    attention.observe(&observed("d1", None, "main"));
    attention.observe(&complete_as(
        "d1",
        None,
        "main",
        Origin::Client,
        RequestKind::Inference,
        Outcome::Rejected,
        Some("hash"),
        Some("2.1.274"),
        NOW,
    ));
    let conditions = read(&attention, NOW).await;
    let drift = find(&conditions, Kind::ClientDrift, "2.1.274").unwrap();
    assert_eq!(
        (
            drift.holds,
            &drift.link,
            drift.pools.is_empty(),
            &drift.detail["key"]
        ),
        (true, &None, true, &json!(null))
    );
}

#[tokio::test]
async fn a_drift_refusal_without_a_version_clears_on_a_relayed_request_without_one() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    attention.observe(&observed("d1", Some("desktop"), "main"));
    attention.observe(&complete("d1", Outcome::Rejected, Some("hash"), None, NOW));
    assert!(
        find(&read(&attention, NOW).await, Kind::ClientDrift, "unknown")
            .unwrap()
            .holds
    );
    attention.observe(&observed("d2", Some("desktop"), "main"));
    attention.observe(&complete("d2", Outcome::Succeeded, None, None, NOW + 1_000));
    let conditions = read(&attention, NOW + 1_000).await;
    let cleared = find(&conditions, Kind::ClientDrift, "unknown").unwrap();
    assert_eq!((cleared.holds, cleared.since), (false, None));
}

/// The drift series' subjects in the read, sorted.
fn drift_subjects(conditions: &[Condition]) -> Vec<String> {
    let mut out: Vec<String> = conditions
        .iter()
        .filter(|c| c.kind == Kind::ClientDrift)
        .map(|c| c.subject.id.clone())
        .collect();
    out.sort();
    out
}

fn refuse(attention: &Attention, id: &str, version: &str, at: i64) {
    attention.observe(&observed(id, Some("desktop"), "main"));
    attention.observe(&complete(
        id,
        Outcome::Rejected,
        Some("hash"),
        Some(version),
        at,
    ));
}

#[tokio::test]
async fn a_drift_condition_clears_only_on_a_relayed_client_inference_of_its_version() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    refuse(&attention, "d1", "2.1.274", NOW);
    // A count carries no stamped line and a keepalive request is the router's own, so neither says the build passes
    // the stamp check again.
    for (id, origin, kind) in [
        ("c1", Origin::Client, RequestKind::CountTokens),
        ("k1", Origin::Keepalive, RequestKind::Inference),
    ] {
        attention.observe(&observed_as(id, Some("desktop"), "main", origin, kind));
        attention.observe(&complete_as(
            id,
            Some("desktop"),
            "main",
            origin,
            kind,
            Outcome::Succeeded,
            None,
            Some("2.1.274"),
            NOW + 1_000,
        ));
        assert!(
            find(
                &read(&attention, NOW + 1_000).await,
                Kind::ClientDrift,
                "2.1.274"
            )
            .unwrap()
            .holds,
            "a succeeded {} {} leaves the condition held",
            origin.as_str(),
            kind.as_str()
        );
    }
    attention.observe(&observed("i1", Some("desktop"), "main"));
    attention.observe(&complete(
        "i1",
        Outcome::Succeeded,
        None,
        Some("2.1.274"),
        NOW + 2_000,
    ));
    assert!(
        !find(
            &read(&attention, NOW + 2_000).await,
            Kind::ClientDrift,
            "2.1.274"
        )
        .unwrap()
        .holds
    );
}

#[tokio::test]
async fn a_flood_of_refused_versions_keeps_the_newest_sixteen_series_on_the_read_and_the_scrape() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    for n in 0..40 {
        refuse(&attention, &format!("d{n}"), &format!("2.1.{n}"), NOW + n);
    }
    let conditions = read(&attention, NOW + 40).await;
    let mut want: Vec<String> = (24..40).map(|n| format!("2.1.{n}")).collect();
    want.sort();
    assert_eq!(
        drift_subjects(&conditions),
        want,
        "the oldest versions make room for the newest"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with(r#"phoebis_attention{kind="client_drift","#))
            .count(),
        16,
        "{text}"
    );
    // A kept version still clears on a relayed request.
    attention.observe(&observed("i1", Some("desktop"), "main"));
    attention.observe(&complete(
        "i1",
        Outcome::Succeeded,
        None,
        Some("2.1.39"),
        NOW + 50,
    ));
    let cleared = read(&attention, NOW + 50).await;
    let cleared = find(&cleared, Kind::ClientDrift, "2.1.39").unwrap();
    assert_eq!((cleared.holds, cleared.since), (false, None));
}

#[tokio::test]
async fn at_the_cap_a_new_version_evicts_the_oldest_cleared_version_before_any_holding_one() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    for n in 0..16 {
        refuse(&attention, &format!("d{n}"), &format!("2.1.{n}"), NOW + n);
    }
    for (id, version) in [("i1", "2.1.9"), ("i2", "2.1.5")] {
        attention.observe(&observed(id, Some("desktop"), "main"));
        attention.observe(&complete(
            id,
            Outcome::Succeeded,
            None,
            Some(version),
            NOW + 20,
        ));
    }
    refuse(&attention, "d16", "2.1.16", NOW + 30);
    let subjects = drift_subjects(&read(&attention, NOW + 30).await);
    assert_eq!(subjects.len(), 16);
    assert!(
        !subjects.contains(&"2.1.5".to_string()) && subjects.contains(&"2.1.9".to_string()),
        "the cleared version whose run started first goes, the holding 2.1.0 stays: {subjects:?}"
    );
    assert!(subjects.contains(&"2.1.0".to_string()));
}

#[tokio::test]
async fn a_version_that_does_not_read_as_a_release_files_under_one_other_subject() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let long = format!("2.1.{}", "9".repeat(196));
    let widest = format!("1.{}", "2".repeat(30));
    let too_wide = format!("1.{}", "2".repeat(31));
    assert_eq!((long.len(), widest.len(), too_wide.len()), (200, 32, 33));
    for (n, version) in [
        "2.1.286-beta",
        long.as_str(),
        too_wide.as_str(),
        "2.1.286.1.2",
        "2..1",
        "2.1.",
        "2.1.286.1",
        widest.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        refuse(&attention, &format!("d{n}"), version, NOW);
    }
    let conditions = read(&attention, NOW).await;
    let mut want = vec![widest.clone(), "2.1.286.1".to_string(), "other".to_string()];
    want.sort();
    assert_eq!(
        drift_subjects(&conditions),
        want,
        "up to four digit runs within 32 bytes keep their version"
    );
    let other = find(&conditions, Kind::ClientDrift, "other").unwrap();
    assert_eq!(
        (
            other.holds,
            &other.subject.label,
            &other.detail["client_version"]
        ),
        (true, &"other".to_string(), &json!("other"))
    );
    let (_, text) = scrape(&rig, &attention).await;
    assert!(
        text.contains(r#"phoebis_attention{kind="client_drift",subject="other"} 1"#)
            && !text.contains("2.1.286-beta")
            && !text.contains(&long),
        "{text}"
    );
}

/// Moves the account to `state` through the account edit stamped `at`, the instant the registry records for it.
fn mark(rig: &Rig, id: &str, state: AccountState, at: i64) {
    let row = rig.store.account(id).unwrap().unwrap();
    rig.store
        .commit_account_update(&AccountUpdate {
            id,
            expected_revision: row.revision,
            incarnation: row.incarnation,
            lifecycle_generation: row.lifecycle_generation,
            label: &row.label,
            secret: None,
            state: Some((state, StateCause::Operator)),
            at,
            actor: "test",
            enabled: None,
            cancellation: None,
        })
        .unwrap();
}

#[tokio::test]
async fn the_account_states_hold_by_kind_and_date_from_the_instant_the_registry_recorded_for_the_change(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    mark(&rig, "business", AccountState::ReauthRequired, NOW + 1_000);
    mark(&rig, "direct", AccountState::FundingRequired, NOW + 2_000);
    let conditions = read(&attention, NOW + 5_000).await;
    let reauth = find(&conditions, Kind::ReauthRequired, "business").unwrap();
    assert_eq!(
        (reauth.holds, reauth.since, &reauth.pools, &reauth.detail),
        (
            true,
            Some(NOW + 1_000),
            &vec!["main".to_string()],
            &json!({ "account": "business", "label": "business" })
        ),
        "the item dates from the instant the registry recorded for the change"
    );
    let funding = find(&conditions, Kind::FundingRequired, "direct").unwrap();
    assert_eq!((funding.holds, funding.since), (true, Some(NOW + 2_000)));
    assert_eq!(
        funding.link,
        Some(Link {
            page: "accounts",
            id: "direct".into()
        })
    );
    mark(&rig, "business", AccountState::Ready, NOW + 6_000);
    mark(&rig, "direct", AccountState::Ready, NOW + 6_000);
    let conditions = read(&attention, NOW + 7_000).await;
    assert!(
        !find(&conditions, Kind::ReauthRequired, "business")
            .unwrap()
            .holds
    );
    assert!(
        !find(&conditions, Kind::FundingRequired, "direct")
            .unwrap()
            .holds
    );
    mark(&rig, "business", AccountState::ReauthRequired, NOW + 8_000);
    let conditions = read(&attention, NOW + 9_000).await;
    assert_eq!(
        find(&conditions, Kind::ReauthRequired, "business")
            .unwrap()
            .since,
        Some(NOW + 8_000),
        "a state that cleared and came back dates from the write that set it again"
    );
    // The relay marks an API key the upstream refuses as it marks an OAuth login whose refresh is gone.
    mark(&rig, "direct", AccountState::ReauthRequired, NOW + 10_000);
    let conditions = read(&attention, NOW + 12_000).await;
    let refused = find(&conditions, Kind::ReauthRequired, "direct").unwrap();
    assert_eq!(
        (refused.holds, refused.since, &refused.pools, &refused.link),
        (
            true,
            Some(NOW + 10_000),
            &vec!["main".to_string()],
            &Some(Link {
                page: "accounts",
                id: "direct".into()
            })
        )
    );
    assert!(
        !find(&conditions, Kind::FundingRequired, "direct")
            .unwrap()
            .holds
    );
}

/// Sets the Artifacts selection of `pool` and reloads, since the feature kind reads the router's snapshot.
async fn select_artifacts(rig: &Rig, pool: &str, account: Option<&str>) {
    let mut row = rig.store.pool(pool).unwrap().unwrap();
    row.feature_accounts.artifacts = account.map(Into::into);
    rig.store
        .update_pool_with_members_and_guardrails(&row, row.updated_at, None, None, "test")
        .unwrap();
    rig.router.reload().await.unwrap();
}

#[tokio::test]
async fn an_ended_account_raises_the_feature_kind_while_a_selection_names_it_and_none_without_one()
{
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    rig.store
        .set_canceled(
            "a",
            Cancellation::On {
                ends_at: Some(NOW - DAY),
            },
        )
        .unwrap();
    select_artifacts(&rig, "main", Some("a")).await;
    assert_eq!(
        find(
            &read(&attention, NOW).await,
            Kind::FeatureAccountUnavailable,
            "a"
        )
        .map(|c| c.holds),
        Some(true),
        "a ready account past its subscription's end refuses the pool's Artifacts calls"
    );
    select_artifacts(&rig, "main", None).await;
    assert_eq!(
        find(
            &read(&attention, NOW).await,
            Kind::FeatureAccountUnavailable,
            "a"
        )
        .map(|c| c.holds),
        Some(false),
        "an ended account no selection names refuses no feature call"
    );
}

#[tokio::test]
async fn an_account_a_pool_selects_without_being_its_member_joins_that_pools_card() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    mark(&rig, "a", AccountState::ReauthRequired, NOW);
    rig.store
        .insert_pool_with_members(
            &PoolRow::for_tests("home"),
            Some(&[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }]),
            "test",
        )
        .unwrap();
    let mut guest = PoolRow::for_tests("guest");
    guest.feature_accounts.connectors = Some("a".into());
    rig.store.insert_pool(&guest, "test").unwrap();
    rig.router.reload().await.unwrap();
    let conditions = read(&attention, NOW).await;
    let item = find(&conditions, Kind::FeatureAccountUnavailable, "a").unwrap();
    assert_eq!(
        (item.holds, &item.pools),
        (true, &vec!["guest".to_string()]),
        "the card is the selecting pool's, and the pool the account is a member of selects nothing"
    );
}

#[tokio::test]
async fn an_account_that_two_pools_and_a_key_select_names_each_pool_once_and_its_selections_in_order(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    mark(&rig, "business", AccountState::ReauthRequired, NOW);
    let mut main = rig.store.pool("main").unwrap().unwrap();
    main.feature_accounts.connectors = Some("business".into());
    rig.store
        .update_pool_with_members_and_guardrails(&main, main.updated_at, None, None, "test")
        .unwrap();
    let desktop = rig.store.api_key("desktop").unwrap().unwrap();
    rig.store
        .update_api_key(
            &ApiKeyRow {
                feature_accounts: KeyFeatureAccounts {
                    artifacts: FeatureOverride::Account("business".into()),
                    connectors: FeatureOverride::Inherit,
                },
                ..desktop.clone()
            },
            desktop.updated_at,
            "test",
        )
        .unwrap();
    // An id ahead of `main`, stored behind it, so the cards follow the ids and not the writes.
    let mut guest = PoolRow::for_tests("guest");
    guest.feature_accounts.artifacts = Some("business".into());
    rig.store.insert_pool(&guest, "test").unwrap();
    rig.router.reload().await.unwrap();
    let conditions = read(&attention, NOW).await;
    let item = find(&conditions, Kind::FeatureAccountUnavailable, "business").unwrap();
    assert_eq!(
        (item.holds, &item.pools, &item.detail["selections"]),
        (
            true,
            &vec!["guest".to_string(), "main".to_string()],
            &json!([
                { "feature": "artifacts", "key": null, "pool": "guest" },
                { "feature": "connectors", "key": null, "pool": "main" },
                { "feature": "artifacts", "key": "desktop", "pool": "main" },
            ])
        )
    );
}

#[tokio::test]
async fn an_ended_accounts_item_dates_from_the_end_that_has_passed() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.store
        .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
        .unwrap();
    rig.store
        .set_canceled(
            "a",
            Cancellation::On {
                ends_at: Some(NOW + DAY),
            },
        )
        .unwrap();
    assert!(rig.store.set_plan_ended("a", NOW - DAY).unwrap());
    select_artifacts(&rig, "main", Some("a")).await;
    let conditions = read(&attention, NOW).await;
    let item = find(&conditions, Kind::FeatureAccountUnavailable, "a").unwrap();
    assert_eq!(
        (item.holds, item.since),
        (true, Some(NOW - DAY)),
        "the plan's end a day ago dates the item, not the cancellation's end a day ahead"
    );
}

#[tokio::test]
async fn an_unready_accounts_feature_item_dates_from_the_change_to_its_state() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    mark(&rig, "business", AccountState::ReauthRequired, NOW + 1_000);
    select_artifacts(&rig, "main", Some("business")).await;
    let conditions = read(&attention, NOW + 5_000).await;
    let item = find(&conditions, Kind::FeatureAccountUnavailable, "business").unwrap();
    assert_eq!((item.holds, item.since), (true, Some(NOW + 1_000)));
}

fn resolve(rig: &Rig, outcome: AttemptOutcome) -> u64 {
    let id = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store.resolve_refresh_attempt(id, 1, outcome).unwrap();
    id
}

#[tokio::test]
async fn two_failed_refreshes_in_a_row_hold_until_one_commits_and_date_from_the_first() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    resolve(
        &rig,
        AttemptOutcome::Failed {
            cause: "invalid_grant".into(),
            detail: "invalid_grant".into(),
        },
    );
    let conditions = read(&attention, NOW).await;
    let one = find(&conditions, Kind::RefreshFailed, "business").unwrap();
    assert_eq!(
        (one.holds, &one.detail["failures"]),
        (false, &json!(1)),
        "one failure is a token endpoint having a bad moment"
    );
    // A millisecond apart on the store's clock, so the run's start can only be read off the older attempt.
    rig.at(NOW + 1);
    let lost = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(
            lost,
            1,
            AttemptOutcome::Uncertain {
                cause: "lost".into(),
                detail: "lost".into(),
            },
        )
        .unwrap();
    let attempts = rig
        .store
        .refresh_histories(&[("business", 1)], 16)
        .unwrap()
        .remove("business")
        .unwrap()
        .attempts;
    assert!(attempts[1].started_at < attempts[0].started_at);
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::RefreshFailed, "business").unwrap();
    assert_eq!(
        (held.holds, held.since, &held.detail),
        (
            true,
            Some(attempts[1].started_at),
            &json!({ "account": "business", "failures": 2, "last_error": "lost", "outcome": "uncertain" })
        ),
        "a lost refresh counts as a failure, and the run dates from its first attempt"
    );
    rig.store
        .resolve_refresh_attempt(lost, 1, AttemptOutcome::Committed)
        .unwrap();
    let conditions = read(&attention, NOW).await;
    let cleared = find(&conditions, Kind::RefreshFailed, "business").unwrap();
    assert_eq!((cleared.holds, cleared.since), (false, None));
}

/// A committed retry closes the lost attempt beside it `superseded` under the commit's own time, so the two rows tie on
/// `resolved_at`: the retry's higher id reads first and ends the run, so the lost row counts no failure.
#[tokio::test]
async fn a_lost_refresh_a_committed_retry_supersedes_counts_no_second_failure() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let lost = resolve(
        &rig,
        AttemptOutcome::Uncertain {
            cause: "refresh_response_lost".into(),
            detail: "refresh response lost".into(),
        },
    );
    // A millisecond on the store's clock, so the commit settles the lost row again under a later time than its own.
    rig.at(NOW + 1);
    let retry = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .commit_credentials(
            "business",
            1,
            1,
            None,
            Some(retry),
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let attempts = rig
        .store
        .refresh_histories(&[("business", 1)], 16)
        .unwrap()
        .remove("business")
        .unwrap()
        .attempts;
    let rows: Vec<_> = attempts
        .iter()
        .map(|a| (a.id, a.outcome.as_str(), a.detail.as_deref()))
        .collect();
    assert_eq!(
        rows,
        [
            (retry, "committed", None),
            (lost, "failed", Some("superseded"))
        ],
        "the commit settles both rows, the retry first"
    );
    assert_eq!(attempts[0].resolved_at, attempts[1].resolved_at);
    let cleared = read(&attention, NOW).await;
    let cleared = find(&cleared, Kind::RefreshFailed, "business").unwrap();
    assert_eq!(
        (cleared.holds, &cleared.detail["failures"]),
        (false, &json!(0)),
        "the superseded row is no failure"
    );

    // A refresh that fails after the commit still counts, once: one failure holds nothing, the second holds.
    resolve(
        &rig,
        AttemptOutcome::Failed {
            cause: "invalid_grant".into(),
            detail: "invalid_grant".into(),
        },
    );
    let one = read(&attention, NOW).await;
    let one = find(&one, Kind::RefreshFailed, "business").unwrap();
    assert_eq!((one.holds, &one.detail["failures"]), (false, &json!(1)));
    resolve(
        &rig,
        AttemptOutcome::Failed {
            cause: "invalid_grant".into(),
            detail: "invalid_grant".into(),
        },
    );
    let held = read(&attention, NOW).await;
    let held = find(&held, Kind::RefreshFailed, "business").unwrap();
    assert_eq!((held.holds, &held.detail["failures"]), (true, &json!(2)));
}

/// A pairing commits a bundle under no attempt, settling the retry still pending as `superseded`: that row exists only
/// because a newer credential committed, so it ends the run as a commit does.
#[tokio::test]
async fn a_pairing_that_supersedes_a_pending_retry_leaves_no_refresh_run() {
    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let store =
        Arc::new(Store::open_in_memory(&StorageKey::generate(), clock_of(&clock_cell)).unwrap());
    let rig = rig_with_store(store.clone(), clock_cell).await;
    let failed = resolve(
        &rig,
        AttemptOutcome::Failed {
            cause: "invalid_grant".into(),
            detail: "invalid_grant".into(),
        },
    );
    rig.at(NOW + 1);
    let pending = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.at(NOW + 2);
    rig.store
        .commit_credentials(
            "business",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let rows: Vec<_> = rig
        .store
        .refresh_histories(&[("business", 1)], 16)
        .unwrap()
        .remove("business")
        .unwrap()
        .attempts
        .into_iter()
        .map(|a| (a.id, a.outcome.as_str().to_string(), a.detail))
        .collect();
    assert_eq!(
        rows,
        [
            (
                pending,
                "failed".to_string(),
                Some("superseded".to_string())
            ),
            (
                failed,
                "failed".to_string(),
                Some("invalid_grant".to_string())
            ),
        ]
    );
    let (attention, _arm) = rig.attention(None);
    let conditions = read(&attention, NOW).await;
    let cleared = find(&conditions, Kind::RefreshFailed, "business").unwrap();
    assert_eq!(
        (cleared.holds, cleared.since, &cleared.detail),
        (
            false,
            None,
            &json!({ "account": "business", "failures": 0, "last_error": null, "outcome": null })
        ),
        "neither the superseded retry nor the failure before the new credential counts"
    );
}

#[tokio::test]
async fn a_refresh_run_is_read_back_by_a_second_reader_over_the_same_store() {
    let rig = rig().await;
    let (first, mut arm) = rig.attention(None);
    for detail in ["one", "two"] {
        resolve(
            &rig,
            AttemptOutcome::Failed {
                cause: detail.into(),
                detail: detail.into(),
            },
        );
    }
    assert!(
        find(&read(&first, NOW).await, Kind::RefreshFailed, "business")
            .unwrap()
            .holds
    );
    arm.abort();
    drop(first);
    let (second, _arm) = rig.attention(None);
    assert!(
        find(&read(&second, NOW).await, Kind::RefreshFailed, "business")
            .unwrap()
            .holds,
        "the run lives in the store, so a restart keeps it"
    );
}

fn usage_answer(status: u16) -> fake_upstream::Respond {
    let body = match status {
        200 => phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Usage,
        )
        .to_vec(),
        _ => b"{}".to_vec(),
    };
    fake_upstream::Respond::Json {
        status,
        headers: vec![],
        body,
    }
}

async fn usage_upstream(status: u16) -> fake_upstream::FakeUpstream {
    fake_upstream::FakeUpstream::start(vec![fake_upstream::Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: usage_answer(status),
    }])
    .await
}

#[tokio::test]
async fn a_poll_backoff_holds_thirty_minutes_into_a_run_of_429s_through_a_failed_poll_and_clears_on_a_clean_one(
) {
    let up = usage_upstream(429).await;
    let rig = rig().await;
    let usage = usage_over(&rig, &up);
    let (attention, _arm) = rig.attention(Some(usage.clone()));
    let before = read(&attention, NOW).await;
    let unpolled = find(&before, Kind::PollBackoff, "business").unwrap();
    assert_eq!(
        (unpolled.holds, &unpolled.detail["backoff_since"]),
        (false, &json!(null)),
        "an account the poll asks carries its series before its first poll status exists"
    );
    assert!(
        find(&before, Kind::PollBackoff, "direct").is_none(),
        "the poll never asks an API key"
    );
    usage.poll("business").await.unwrap();
    // A second 429 once the first step's two minutes have passed: the run keeps its start and climbs the ladder.
    rig.at(NOW + 29 * 60_000);
    usage.poll("business").await.unwrap();
    let short = read(&attention, NOW + BACKOFF_ATTENTION_AFTER_MS - 1).await;
    assert!(
        !find(&short, Kind::PollBackoff, "business").unwrap().holds,
        "short of thirty minutes nothing holds"
    );
    let conditions = read(&attention, NOW + BACKOFF_ATTENTION_AFTER_MS).await;
    let held = find(&conditions, Kind::PollBackoff, "business").unwrap();
    assert_eq!(
        (held.holds, held.since, &held.detail),
        (
            true,
            Some(NOW),
            &json!({
                "account": "business",
                "backoff_since": NOW,
                "next_attempt_at": NOW + 29 * 60_000 + 240_000,
                "rate_limited_in_a_row": 2,
            })
        )
    );
    // A threshold of 0 applies the switch at the rule's next request, whatever it has counted so far. A failed poll
    // is no clean answer, so the run and its condition stand through it.
    up.set_rule_after(0, "/api/oauth/usage", usage_answer(500));
    rig.at(NOW + 29 * 60_000 + 240_000);
    usage.poll("business").await.unwrap();
    assert_eq!(
        usage.poll_status("business").unwrap().last_outcome,
        PollOutcome::Failed
    );
    assert!(
        find(
            &read(&attention, NOW + 29 * 60_000 + 240_000).await,
            Kind::PollBackoff,
            "business"
        )
        .unwrap()
        .holds,
        "a 500 inside the run clears nothing"
    );
    up.set_rule_after(0, "/api/oauth/usage", usage_answer(200));
    rig.at(NOW + 29 * 60_000 + 300_000);
    usage.poll("business").await.unwrap();
    assert_eq!(
        usage.poll_status("business").unwrap().last_outcome,
        PollOutcome::Clean
    );
    let conditions = read(&attention, NOW + 29 * 60_000 + 300_000).await;
    assert!(
        !find(&conditions, Kind::PollBackoff, "business")
            .unwrap()
            .holds
    );
}

#[tokio::test]
async fn a_deleted_account_and_pool_lose_their_series_at_the_next_read() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    attention.observe(&observed("r1", Some("desktop"), "main"));
    attention.observe(&complete("r1", Outcome::Exhausted, None, None, NOW));
    assert!(
        find(&read(&attention, NOW).await, Kind::PoolExhausted, "main")
            .unwrap()
            .holds
    );
    rig.store.delete_api_key("desktop", "test").unwrap();
    rig.store.delete_pool("main", "test").unwrap();
    rig.store.delete_account("direct", "test").unwrap();
    rig.router.reload().await.unwrap();
    let conditions = read(&attention, NOW).await;
    assert!(find(&conditions, Kind::PoolExhausted, "main").is_none());
    assert!(find(&conditions, Kind::FundingRequired, "direct").is_none());
    assert!(find(&conditions, Kind::ReauthRequired, "direct").is_none());
    assert!(
        find(&conditions, Kind::ReauthRequired, "business").is_some_and(|c| c.pools.is_empty()),
        "an account left in no pool keeps its series and lists none"
    );
}

#[tokio::test]
async fn the_bus_arm_reads_the_request_outcomes_off_the_bus() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.bus.publish(observed("r1", Some("desktop"), "main"));
    rig.bus
        .publish(complete("r1", Outcome::Exhausted, None, None, NOW));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !find(&read(&attention, NOW).await, Kind::PoolExhausted, "main")
        .unwrap()
        .holds
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the arm never read the exhaustion"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_completion_behind_more_events_than_the_arms_queue_holds_reaches_the_arm_with_no_drop() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // No await between the publishes, so the arm cannot drain its queue while the burst arrives.
    for _ in 0..=QUEUE_DEPTH {
        rig.bus.publish(observed("burst", Some("desktop"), "main"));
    }
    rig.bus
        .publish(complete("r1", Outcome::Exhausted, None, None, NOW));
    let stats = rig
        .bus
        .stats()
        .into_iter()
        .find(|(name, _)| *name == "attention")
        .unwrap()
        .1;
    let observed_it = tokio::time::timeout(Duration::from_secs(5), async {
        while !find(&read(&attention, NOW).await, Kind::PoolExhausted, "main")
            .unwrap()
            .holds
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    assert_eq!(
        (
            observed_it,
            stats.dropped.load(std::sync::atomic::Ordering::SeqCst)
        ),
        (true, 0),
        "the burst never reached the arm's queue, so the completion found room and the arm read it"
    );
}

/// One scrape of the metrics listener over the reader's gauge source: its status and its text.
async fn scrape(rig: &Rig, attention: &Arc<Attention>) -> (u16, String) {
    let app = phoebis_ledger::metrics_router(
        Arc::new(Metrics::new()),
        rig.router.clone(),
        Arc::new(Pricing::new(PriceRevisions::shipped())),
        rig.bus.clone(),
        rig.clock(),
        Duration::from_secs(60),
        None,
        Some(attention.gauge_source()),
        Some(rig.ledger.clone()),
    );
    let response = app
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn a_failed_holder_read_fails_the_conditions_and_the_scrape() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.store.debug_fail_next_refresh_read();
    assert!(attention.conditions(NOW).await.is_err());
    rig.store.debug_fail_next_refresh_read();
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(
        (status, text.as_str()),
        (500, "attention read failed"),
        "a scrape without the evidence would read 0 for a held condition"
    );
    assert_eq!(
        scrape(&rig, &attention).await.0,
        200,
        "the next scrape answers again"
    );
}

/// The `parked_records` table as the ledger's migration creates it.
const PARKED_RECORDS: &str = "CREATE TABLE parked_records (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  parked_at INTEGER NOT NULL,
  last_tried_at INTEGER NOT NULL,
  request_id TEXT NOT NULL,
  pool_id TEXT,
  account_id TEXT,
  record TEXT NOT NULL,
  error TEXT NOT NULL
);";

fn ledger_sql(rig: &Rig, sql: &str) {
    rig.db
        .write(|c| {
            c.execute_batch(sql)?;
            Ok(())
        })
        .unwrap();
}

/// Parks one priced record of `pool` (none for `None`) on `account` at `at`. Its record is not JSON, so the ledger's
/// start pass, which lands every parked row whose record parses, never takes the row out from under the read.
fn park(rig: &Rig, pool: Option<&str>, account: &str, at: i64) {
    let pool = pool.map_or("NULL".to_string(), |p| format!("'{p}'"));
    ledger_sql(
        rig,
        &format!(
            "INSERT INTO parked_records (parked_at, last_tried_at, request_id, pool_id, account_id, record, error) \
             VALUES ({at}, {at}, 'parked-{at}', {pool}, '{account}', 'not json', 'refused by test');"
        ),
    );
}

fn pool_subject(id: &str, label: &str) -> Subject {
    Subject {
        kind: SubjectKind::Pool,
        id: id.into(),
        label: label.into(),
    }
}

fn router_subject(id: &str) -> Subject {
    Subject {
        kind: SubjectKind::Router,
        id: id.into(),
        label: id.into(),
    }
}

#[tokio::test]
async fn a_parked_priced_record_holds_the_condition_on_its_pool_until_the_table_empties() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    park(&rig, Some("main"), "business", NOW - 60_000);
    let conditions = read(&attention, NOW).await;
    let parked = find(&conditions, Kind::LedgerRecordParked, "main").unwrap();
    assert_eq!(
        (
            parked.holds,
            parked.since,
            &parked.subject,
            &parked.pools,
            &parked.link,
            &parked.detail
        ),
        (
            true,
            Some(NOW - 60_000),
            &pool_subject("main", "Main"),
            &vec!["main".to_string()],
            &Some(Link {
                page: "pools",
                id: "main".into()
            }),
            &json!({ "accounts": ["business"], "count": 1, "pool": "main" })
        ),
        "the condition dates from the oldest parked row and opens the pool's card"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="ledger_record_parked",subject="main"} 1"#),
        "{text}"
    );
    ledger_sql(&rig, "DELETE FROM parked_records;");
    let conditions = read(&attention, NOW).await;
    let cleared = find(&conditions, Kind::LedgerRecordParked, "main")
        .expect("a pool keeps its series once its rows are gone");
    assert_eq!(
        (cleared.holds, cleared.since, &cleared.detail),
        (
            false,
            None,
            &json!({ "accounts": [], "count": 0, "pool": "main" })
        ),
        "the condition clears on its own when the table empties"
    );
}

#[tokio::test]
async fn a_parked_record_without_a_pool_holds_under_the_router_subject_until_its_row_goes() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    assert!(
        find(&read(&attention, NOW).await, Kind::LedgerRecordParked, "-").is_none(),
        "no record without a pool, no series for one"
    );
    park(&rig, None, "direct", NOW - 1_000);
    let conditions = read(&attention, NOW).await;
    let parked = find(&conditions, Kind::LedgerRecordParked, "-").unwrap();
    assert_eq!(
        (
            parked.holds,
            parked.since,
            &parked.subject,
            parked.pools.is_empty(),
            &parked.link,
            &parked.detail
        ),
        (
            true,
            Some(NOW - 1_000),
            &router_subject("-"),
            true,
            &None,
            &json!({ "accounts": ["direct"], "count": 1, "pool": null })
        )
    );
    assert!(
        !find(&conditions, Kind::LedgerRecordParked, "main")
            .unwrap()
            .holds,
        "a record without a pool holds no pool's condition"
    );
    ledger_sql(&rig, "DELETE FROM parked_records;");
    assert!(
        find(&read(&attention, NOW).await, Kind::LedgerRecordParked, "-").is_none(),
        "the series leaves with the rows"
    );
}

/// One routing event row as the router's mirror writes it, through the ledger's file.
fn routing_event(
    rig: &Rig,
    at: i64,
    kind: &str,
    account: &str,
    pool: Option<&str>,
    detail: serde_json::Value,
) {
    let pool = pool.map_or("NULL".to_string(), |p| format!("'{p}'"));
    let detail = detail.to_string().replace('\'', "''");
    ledger_sql(
        rig,
        &format!(
            "INSERT INTO events (at, kind, account_id, pool_id, detail) \
             VALUES ({at}, '{kind}', '{account}', {pool}, '{detail}');"
        ),
    );
}

/// Folds the routing conditions at `at` through the ledger's writer, as the hourly tick does, after the ledger's own
/// fold at start has run, so that one never lands over this one. The rig's clock moves to `at` too.
async fn fold_at(rig: &Rig, at: i64) {
    let stats = rig.ledger.stats();
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (stats
            .condition_folds
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1)
            .then_some(())
    })
    .await;
    rig.at(at);
    let tables = tables_of(COSTS);
    rig.db
        .write(|c| {
            let tx = c.transaction()?;
            phoebis_ledger::fold_routing_conditions(&tx, &tables, at)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
}

fn account_subject(id: &str) -> Subject {
    Subject {
        kind: SubjectKind::Account,
        id: id.into(),
        label: id.into(),
    }
}

const MINUTE: i64 = 60_000;

#[tokio::test]
async fn a_round_that_marked_nothing_for_want_of_runway_holds_the_condition_on_its_pool_for_a_day()
{
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    routing_event(
        &rig,
        NOW - 30 * MINUTE,
        "shedding_round",
        "business",
        Some("main"),
        json!({ "window": "five_hour", "projection": 0.99, "outcome": "nothing_to_mark", "reason": "no_runway" }),
    );
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::ShedNoRunway, "main").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.pools,
            &held.link,
            &held.detail
        ),
        (
            true,
            Some(NOW - 30 * MINUTE),
            &pool_subject("main", "Main"),
            &vec!["main".to_string()],
            &Some(Link {
                page: "pools",
                id: "main".into()
            }),
            &json!({
                "accounts": ["business"],
                "rounds": 1,
                "setting": ["five_hour_ceiling", "weekly_ceiling"],
                "setting_values": { "five_hour_ceiling": 0.95, "weekly_ceiling": 1.0 },
            })
        ),
        "the condition dates from the round, opens the pool's card and carries the pool's ceilings beside their names"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="shed_no_runway",subject="main"} 1"#),
        "{text}"
    );
    fold_at(&rig, NOW + 25 * HOUR).await;
    let cleared = find(
        &read(&attention, NOW + 25 * HOUR).await,
        Kind::ShedNoRunway,
        "main",
    )
    .cloned()
    .expect("a pool keeps its series once its rounds are a day old");
    assert_eq!(
        (cleared.holds, cleared.since, &cleared.detail),
        (
            false,
            None,
            &json!({ "accounts": [], "rounds": 0, "setting": ["five_hour_ceiling", "weekly_ceiling"] })
        ),
        "the condition clears once the trailing day holds no such round"
    );
}

#[tokio::test]
async fn a_move_whose_destination_was_spent_within_the_hour_holds_the_condition_on_that_account() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // The pool's own ceiling, so the value the condition carries beside the setting is the case's.
    let mut pool = rig.store.pool("main").unwrap().unwrap();
    pool.five_hour_ceiling = 0.85;
    rig.store
        .update_pool_with_members_and_guardrails(&pool, pool.updated_at, None, None, "test")
        .unwrap();
    rig.router.reload().await.unwrap();
    routing_event(
        &rig,
        NOW - 50 * MINUTE,
        "pin_migrated",
        "business",
        None,
        json!({ "reason": "spent", "from_account": "direct", "fit": "fits" }),
    );
    routing_event(
        &rig,
        NOW - 10 * MINUTE,
        "window_spent",
        "business",
        None,
        json!({ "window": "five_hour", "source": "head" }),
    );
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::DestinationSpent, "business").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.pools,
            &held.link,
            &held.detail
        ),
        (
            true,
            Some(NOW - 50 * MINUTE),
            &account_subject("business"),
            &vec!["main".to_string()],
            &Some(Link {
                page: "accounts",
                id: "business".into()
            }),
            &json!({
                "fits": { "fits": 1 },
                "moves": 1,
                "setting": ["five_hour_ceiling"],
                "setting_values": { "five_hour_ceiling": 0.85 },
                "windows": ["five_hour"],
            })
        ),
        "the condition dates from the move, opens the account and names the setting with the value of the \
         account's pool"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="destination_spent",subject="business"} 1"#),
        "{text}"
    );
    fold_at(&rig, NOW + 25 * HOUR).await;
    let cleared = find(
        &read(&attention, NOW + 25 * HOUR).await,
        Kind::DestinationSpent,
        "business",
    )
    .cloned()
    .expect("an OAuth account keeps its series once its moves are a day old");
    assert_eq!(
        (cleared.holds, cleared.since, &cleared.detail),
        (
            false,
            None,
            &json!({ "fits": {}, "moves": 0, "setting": [], "windows": [] })
        ),
        "the condition clears once the trailing day holds no such move"
    );
}

#[tokio::test]
async fn a_routing_condition_on_an_account_the_registry_no_longer_holds_opens_no_page_and_joins_no_card(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    routing_event(
        &rig,
        NOW - 50 * MINUTE,
        "pin_migrated",
        "gone",
        None,
        json!({ "reason": "spent", "from_account": "direct", "fit": "fits" }),
    );
    routing_event(
        &rig,
        NOW - 10 * MINUTE,
        "window_spent",
        "gone",
        None,
        json!({ "window": "five_hour", "source": "head" }),
    );
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::DestinationSpent, "gone").unwrap();
    assert_eq!(
        (held.holds, &held.subject, &held.pools, &held.link),
        (true, &account_subject("gone"), &Vec::<String>::new(), &None)
    );
}

#[tokio::test]
async fn an_account_that_reached_its_wall_under_a_ceiling_holds_the_condition_with_the_projection_error(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // The pool's own values, so the values the condition carries beside its settings are the case's.
    let mut pool = rig.store.pool("main").unwrap().unwrap();
    pool.shedding.shed_cap = 0.3;
    pool.five_hour_ceiling = 0.9;
    rig.store
        .update_pool_with_members_and_guardrails(&pool, pool.updated_at, None, None, "test")
        .unwrap();
    rig.router.reload().await.unwrap();
    // A round two hours before the wall projected the 5-hour window at 0.97, so the wall came 0.03 past it.
    routing_event(
        &rig,
        NOW - 2 * HOUR,
        "shedding_round",
        "business",
        Some("main"),
        json!({ "window": "five_hour", "projection": 0.97, "outcome": "marked 1", "reason": null }),
    );
    routing_event(
        &rig,
        NOW - HOUR,
        "window_spent",
        "business",
        None,
        json!({ "window": "five_hour", "source": "predicted" }),
    );
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    assert!(
        !find(&conditions, Kind::ShedNoRunway, "main").unwrap().holds,
        "a round that marked a pin holds no pool's condition"
    );
    let held = find(&conditions, Kind::WallReached, "business").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.pools,
            &held.link,
            &held.detail
        ),
        (
            true,
            Some(NOW - HOUR),
            &account_subject("business"),
            &vec!["main".to_string()],
            &Some(Link {
                page: "accounts",
                id: "business".into()
            }),
            &json!({
                "early_ms": null,
                "projection_error": 1.0 - 0.97,
                "setting": ["shed_cap", "five_hour_ceiling"],
                "setting_values": { "shed_cap": 0.3, "five_hour_ceiling": 0.9 },
                "walls": 1,
                "windows": ["five_hour"],
            })
        ),
        "a predicted wall counts, the error is how far under the wall the round projected, and the settings carry \
         the values of the account's pool"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="wall_reached",subject="business"} 1"#),
        "{text}"
    );
    fold_at(&rig, NOW + 25 * HOUR).await;
    let cleared = find(
        &read(&attention, NOW + 25 * HOUR).await,
        Kind::WallReached,
        "business",
    )
    .cloned()
    .expect("an OAuth account keeps its series once its walls are a day old");
    assert_eq!(
        (cleared.holds, cleared.since, &cleared.detail),
        (
            false,
            None,
            &json!({
                "early_ms": null,
                "projection_error": null,
                "setting": ["shed_cap"],
                "walls": 0,
                "windows": [],
            })
        ),
        "the condition clears once the trailing day holds no wall"
    );
}

/// `direct`, the API-key member of `main`, is no live subject of an account's conditions, which take OAuth accounts
/// alone, so its row reads the values of no pool.
#[tokio::test]
async fn a_wall_row_with_no_pool_on_an_api_key_member_reads_no_setting_values() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    routing_event(
        &rig,
        NOW - HOUR,
        "window_spent",
        "direct",
        None,
        json!({ "window": "five_hour", "source": "head" }),
    );
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::WallReached, "direct").unwrap();
    assert_eq!(
        (held.holds, held.detail.get("setting_values")),
        (true, None),
        "the wall holds and names its settings with no values: {}",
        held.detail
    );
}

#[tokio::test]
async fn an_account_slowed_past_twice_its_pools_threshold_in_the_day_holds_the_condition_on_that_account(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // The pool's own threshold, so the value the condition carries beside the setting is the case's.
    let mut pool = rig.store.pool("main").unwrap().unwrap();
    pool.slow_down.slow_down_stretch_seconds = 2_700;
    rig.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    rig.router.reload().await.unwrap();
    let started = |at: i64, factor: f64| {
        routing_event(
            &rig,
            at,
            "slow_down_started",
            "business",
            Some("main"),
            json!({
                "factor": factor,
                "stretch_ms": 2_400_000,
                "threshold_ms": 2_700_000,
                "demand": 0.1,
                "reset": null,
            }),
        )
    };
    let ended = |at: i64, since: i64, duration_ms: i64, factor_min: f64| {
        routing_event(
            &rig,
            at,
            "slow_down_ended",
            "business",
            Some("main"),
            json!({
                "since": since,
                "duration_ms": duration_ms,
                "factor_min": factor_min,
                "held_ms": 0,
            }),
        )
    };
    // A request of `business` the slow-down held for `held_ms`, through the ledger's landing, which books the wait
    // into its hour's rollup row.
    let held_request = |id: &str, at: i64, held_ms: i64| {
        rig.db
            .insert_request_at(
                &RequestRecord {
                    id: id.into(),
                    received_at: at,
                    key_id: Some("desktop".into()),
                    pool_id: Some("main".into()),
                    canonical_model: "claude-sonnet-5".into(),
                    model: "claude-sonnet-5".into(),
                    kind: "inference".into(),
                    origin: "client".into(),
                    status: Some(200),
                    outcome: "succeeded".into(),
                    attempts: 1,
                    held: Some(
                        json!({ "waits": [{ "reason": "slow_down", "held_ms": held_ms }] })
                            .to_string(),
                    ),
                    ..Default::default()
                },
                &[AttemptRecord {
                    request_id: id.into(),
                    attempt: 1,
                    account_id: "business".into(),
                    account_kind: AccountKind::Oauth,
                    started_at: at,
                    outcome: "succeeded".into(),
                    ..Default::default()
                }],
                None,
                &PriceRevisions::shipped(),
            )
            .unwrap()
    };
    // A span closed inside the day and one open since 50 minutes before the fold, inside which the slow-down held a
    // request for an hour and another for 40 minutes: 100 minutes, past the bar of twice the pool's 45 minutes.
    started(NOW - 2 * HOUR, 0.5);
    ended(
        NOW - 2 * HOUR + 52 * MINUTE,
        NOW - 2 * HOUR,
        50 * MINUTE,
        0.4,
    );
    started(NOW - 50 * MINUTE, 0.35);
    held_request("r-1", NOW - 2 * HOUR, HOUR);
    held_request("r-2", NOW - 40 * MINUTE, 40 * MINUTE);
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::SlowedLong, "business").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.pools,
            &held.link,
            &held.detail
        ),
        (
            true,
            Some(NOW - 2 * HOUR),
            &account_subject("business"),
            &vec!["main".to_string()],
            &Some(Link {
                page: "accounts",
                id: "business".into()
            }),
            &json!({
                "bar_ms": 5_400_000,
                "held": 2,
                "held_ms": 6_000_000,
                "open": true,
                "setting": ["slow_down_stretch_seconds"],
                "setting_values": { "slow_down_stretch_seconds": 2_700 },
                "spans": 2,
            })
        ),
        "the condition dates from the oldest span's start, opens the account and carries the bar and the pool's \
         threshold"
    );
    assert!(
        Kind::ShedNoRunway < Kind::SlowedLong,
        "a slow-down that worked for long stops no traffic, so it ranks after every routing condition"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="slowed_long",subject="business"} 1"#),
        "{text}"
    );
    // The open span ends, and a day later the trailing 24 hours hold none of the spans.
    ended(NOW + 10 * MINUTE, NOW - 50 * MINUTE, 58 * MINUTE, 0.35);
    fold_at(&rig, NOW + 25 * HOUR).await;
    let cleared = find(
        &read(&attention, NOW + 25 * HOUR).await,
        Kind::SlowedLong,
        "business",
    )
    .cloned()
    .expect("an OAuth account keeps its series once its spans are a day old");
    assert_eq!(
        (cleared.holds, cleared.since, &cleared.detail),
        (
            false,
            None,
            &json!({
                "bar_ms": null,
                "held": 0,
                "held_ms": 0,
                "open": false,
                "setting": ["slow_down_stretch_seconds"],
                "spans": 0,
            })
        ),
        "the condition clears once the trailing day holds no span, with no start row to read a bar from"
    );
}

/// One reconciled 5-hour window of `business` as the ledger's hourly fold writes it, through the ledger's file.
fn reconciled(rig: &Rig, window_end: i64, rise: f64, predicted: f64, ratio: f64) {
    ledger_sql(
        rig,
        &format!(
            "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
             last_reading_at, observed_rise, predicted_share, ratio, requests, computed_at) \
             VALUES ('business', 'five_hour', {window_end}, {}, {}, {rise}, {predicted}, {ratio}, 4, {NOW});",
            window_end - 3 * HOUR,
            window_end - HOUR,
        ),
    );
}

#[tokio::test]
async fn meter_drift_holds_on_an_off_window_and_clears_when_the_newest_reads_inside() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    reconciled(&rig, NOW - 2 * HOUR, 0.3, 0.15, 2.0);
    fold_at(&rig, NOW).await;
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::MeterDrift, "business").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.pools,
            &held.link,
            &held.detail
        ),
        (
            true,
            Some(NOW - 2 * HOUR),
            &account_subject("business"),
            &vec!["main".to_string()],
            &Some(Link {
                page: "accounts",
                id: "business".into()
            }),
            &json!({
                "effective": "2026-01-01",
                "source": "test",
                "windows": [{
                    "observed_rise": 0.3,
                    "predicted_share": 0.15,
                    "ratio": 2.0,
                    "requests": 4,
                    "window": "five_hour",
                    "window_end": NOW - 2 * HOUR,
                }],
            })
        ),
        "the window's newest row charged twice the table, so the condition dates from its end and names the cost \
         model in force"
    );
    let drift = held.detail.clone();
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="meter_drift",subject="business"} 1"#),
        "{text}"
    );
    reconciled(&rig, NOW - HOUR, 0.04, 0.04 / 3.0, 3.0);
    fold_at(&rig, NOW + MINUTE).await;
    let small = find(
        &read(&attention, NOW + MINUTE).await,
        Kind::MeterDrift,
        "business",
    )
    .cloned()
    .unwrap();
    assert_eq!(
        (small.holds, small.since, &small.detail),
        (true, Some(NOW - 2 * HOUR), &drift),
        "a rise under five percent is a few quanta of the meter and counts nothing, so the newer row leaves the drift \
         of the row before it standing"
    );
    reconciled(&rig, NOW - 30 * MINUTE, 0.3, 0.3, 1.0);
    fold_at(&rig, NOW + 2 * MINUTE).await;
    let inside = find(
        &read(&attention, NOW + 2 * MINUTE).await,
        Kind::MeterDrift,
        "business",
    )
    .cloned()
    .unwrap();
    assert_eq!(
        (inside.holds, inside.since, &inside.detail),
        (
            false,
            None,
            &json!({ "effective": null, "source": null, "windows": [] })
        ),
        "a newer row inside the band clears the window"
    );
}

#[tokio::test]
async fn a_parked_record_keeps_its_condition_after_its_pool_is_deleted() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // An id the registry does not hold, as the pool of a record parked before the pool was deleted.
    park(&rig, Some("gone"), "business", NOW - 5_000);
    let conditions = read(&attention, NOW).await;
    let parked = find(&conditions, Kind::LedgerRecordParked, "gone").unwrap();
    assert_eq!(
        (
            parked.holds,
            parked.since,
            &parked.subject,
            &parked.link,
            parked.pools.is_empty(),
            &parked.detail
        ),
        (
            true,
            Some(NOW - 5_000),
            &pool_subject("gone", "gone"),
            &None,
            true,
            &json!({ "accounts": ["business"], "count": 1, "pool": "gone" })
        ),
        "the deleted pool is named by its id, with no card to open"
    );
    ledger_sql(&rig, "DELETE FROM parked_records;");
    assert!(
        find(
            &read(&attention, NOW).await,
            Kind::LedgerRecordParked,
            "gone"
        )
        .is_none(),
        "the deleted pool's series leaves with its rows"
    );
}

#[tokio::test]
async fn a_failed_parked_read_fails_the_attention_read_as_the_refresh_read_does() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    ledger_sql(&rig, "DROP TABLE parked_records;");
    assert!(
        matches!(
            attention.conditions(NOW).await,
            Err(AttentionError::Ledger(_))
        ),
        "a read without the parked rows would report a held condition clear"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(
        (status, text.as_str()),
        (500, "attention read failed"),
        "a scrape without the evidence would read 0 for a held condition"
    );
    ledger_sql(&rig, PARKED_RECORDS);
    assert!(
        find(
            &read(&attention, NOW).await,
            Kind::LedgerRecordParked,
            "main"
        )
        .is_some_and(|c| !c.holds),
        "the read answers again once the table is back"
    );
}

/// A finished request of pool `main` served by `business`, priced by its million input tokens.
fn priced(id: &str) -> Record {
    Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: NOW,
            finished_at: Some(NOW + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: AccountKind::Oauth,
            started_at: NOW,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            ..Default::default()
        }],
        bodies: None,
    }
}

#[tokio::test]
async fn a_degraded_ledger_holds_writer_degraded_from_the_first_read_that_saw_it() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // The file refuses every request row, so the next batch fails and the ledger reads degraded; the rig's clock
    // stands still, so the batch's retry never comes due and the state holds.
    ledger_sql(
        &rig,
        "CREATE TRIGGER refuse BEFORE INSERT ON requests BEGIN SELECT RAISE(ABORT, 'refused by test'); END;",
    );
    rig.ledger.record_request(priced("r1")).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !rig.ledger.degraded() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ledger never read degraded"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let first = read(&attention, NOW + 1_000).await;
    let held = find(&first, Kind::WriterDegraded, "ledger").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.link,
            held.pools.is_empty(),
            &held.detail
        ),
        (
            true,
            Some(NOW + 1_000),
            &router_subject("ledger"),
            &None,
            true,
            &json!({ "component": "ledger" })
        ),
        "the ledger records no time for its state, so the first read that saw it dates it"
    );
    assert!(
        !find(&first, Kind::WriterDegraded, "routing").unwrap().holds,
        "the routing state writer has its own series"
    );
    let second = read(&attention, NOW + 5_000).await;
    assert_eq!(
        find(&second, Kind::WriterDegraded, "ledger").unwrap().since,
        Some(NOW + 1_000),
        "a later read keeps the date of the first read that saw it"
    );
}

/// A read waiting on the registry never holds the bus arm: the registry's reads come before the reader's state lock,
/// so a stalled registry file leaves `observe` free while the read waits.
#[tokio::test]
async fn a_read_waiting_on_the_registry_leaves_the_bus_arm_free() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn({
        let store = rig.store.clone();
        move || {
            store
                .write_routing(|_| {
                    held_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                    Ok(())
                })
                .unwrap();
        }
    });
    held_rx.recv().unwrap();
    let before = rig.store.debug_lock_count();
    let reading = std::thread::spawn({
        let attention = attention.clone();
        // The read runs on a runtime of the thread's own. The case's runtime runs no task while the body below waits,
        // and a task of the rig that ran beside the case on a second worker would move the lock count too.
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime
                .block_on(attention.conditions(NOW))
                .map(|read| read.len())
        }
    });
    // The counter moves as a caller reaches for the registry's connection, before it blocks on the holder, so
    // once it moves the read is the one waiting there.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while rig.store.debug_lock_count() == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the read never reached the registry"
        );
        std::thread::yield_now();
    }
    let observing = std::thread::spawn({
        let attention = attention.clone();
        move || refuse(&attention, "r1", "2.1.999", NOW)
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let free = loop {
        if observing.is_finished() {
            break true;
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
        std::thread::yield_now();
    };
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(free, "observe waited on a read held by the registry");
    assert!(!reading.is_finished() || reading.join().unwrap().is_ok());
}

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// A listener's pair as the router serves it, loaded a day before `NOW` with no failed reload.
fn pair(key: &str, not_after_ms: i64) -> PairFacts {
    let listener = key.trim_end_matches(".tls");
    PairFacts {
        key: key.into(),
        cert_file: format!("/etc/phoebis/{listener}.pem").into(),
        key_file: format!("/etc/phoebis/{listener}.key").into(),
        not_after_ms,
        loaded_at: NOW - DAY,
        failed: None,
    }
}

#[tokio::test]
async fn a_pair_within_thirty_days_of_its_end_holds_certificate_expiring_since_thirty_days_before_it(
) {
    let rig = rig().await;
    let pairs = Arc::new(Pairs::default());
    let (attention, _arm) = rig.attention_with_pairs(None, Some(pairs.clone()));

    pairs.set(vec![pair("proxy.tls", NOW + 30 * DAY)]);
    let conditions = read(&attention, NOW).await;
    let held = find(&conditions, Kind::CertificateExpiring, "proxy.tls").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.link,
            held.pools.is_empty(),
            &held.detail
        ),
        (
            true,
            Some(NOW),
            &router_subject("proxy.tls"),
            &None,
            true,
            &json!({
                "cert_file": "/etc/phoebis/proxy.pem",
                "days_left": 30,
                "expires": NOW + 30 * DAY,
            })
        ),
        "thirty days left holds, dated thirty days before the end"
    );

    pairs.set(vec![pair("proxy.tls", NOW + 31 * DAY)]);
    let clear = read(&attention, NOW).await;
    let clear = find(&clear, Kind::CertificateExpiring, "proxy.tls").unwrap();
    assert_eq!(
        (clear.holds, clear.since, &clear.detail["days_left"]),
        (false, None, &json!(31)),
        "thirty-one days left reads clear"
    );

    pairs.set(vec![pair("proxy.tls", NOW - 3 * DAY)]);
    let past = read(&attention, NOW).await;
    let past = find(&past, Kind::CertificateExpiring, "proxy.tls").unwrap();
    assert_eq!(
        (past.holds, past.since, &past.detail["days_left"]),
        (true, Some(NOW - 33 * DAY), &json!(-3)),
        "a leaf past its end still holds, its days left negative"
    );
}

#[tokio::test]
async fn a_leaf_thirty_days_and_twelve_hours_from_its_end_holds_no_expiring_item() {
    let rig = rig().await;
    let pairs = Arc::new(Pairs::default());
    pairs.set(vec![pair("proxy.tls", NOW + 30 * DAY + 12 * HOUR)]);
    let (attention, _arm) = rig.attention_with_pairs(None, Some(pairs));
    let conditions = read(&attention, NOW).await;
    let clear = find(&conditions, Kind::CertificateExpiring, "proxy.tls").unwrap();
    assert_eq!(
        (clear.holds, clear.since, &clear.detail["days_left"]),
        (false, None, &json!(30)),
        "thirty whole days left and part of a day is still outside the thirty days"
    );
}

#[tokio::test]
async fn a_leaf_exactly_thirty_days_from_its_end_holds_the_expiring_item_since_the_read() {
    let rig = rig().await;
    let pairs = Arc::new(Pairs::default());
    let (attention, _arm) = rig.attention_with_pairs(None, Some(pairs.clone()));

    // One pair installed once and read at twelve-hour steps across its onset: no read dates the item after itself.
    let not_after = NOW + 61 * DAY;
    pairs.set(vec![pair("proxy.tls", not_after)]);
    for (at, expiring) in [
        (NOW + 30 * DAY + 12 * HOUR, (false, None)),
        (NOW + 31 * DAY, (true, Some(NOW + 31 * DAY))),
        (NOW + 31 * DAY + 12 * HOUR, (true, Some(NOW + 31 * DAY))),
    ] {
        let conditions = read(&attention, at).await;
        let c = find(&conditions, Kind::CertificateExpiring, "proxy.tls").unwrap();
        assert!(
            c.since.is_none_or(|since| since <= at),
            "at {at}: since {:?} is after the read",
            c.since
        );
        assert_eq!((c.holds, c.since), expiring, "at {at}");
    }
}

#[tokio::test]
async fn a_refresh_token_within_three_days_of_its_end_holds_pairing_expiring_since_three_days_before_it(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    for (id, label, end) in [
        ("lead", "Lead", NOW + 3 * DAY),
        ("outside", "Outside", NOW + 3 * DAY + 1),
        ("ended", "Ended", NOW - 1_000),
    ] {
        rig.store
            .insert_account(&AccountRow {
                id: id.into(),
                kind: AccountKind::Oauth,
                label: label.into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
                state_cause: StateCause::Paired,
            })
            .unwrap();
        rig.store
            .commit_credentials(
                id,
                1,
                1,
                None,
                None,
                &NewBundle {
                    access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                    refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                    issued_at: NOW,
                    expires_at: NOW + HOUR,
                    refresh_expires_at: Some(end),
                    scopes: "user:profile user:inference".into(),
                },
            )
            .unwrap();
    }

    let conditions = read(&attention, NOW).await;
    let lead = find(&conditions, Kind::PairingExpiring, "lead").unwrap();
    assert_eq!(
        (lead.holds, lead.since, &lead.link, &lead.detail),
        (
            true,
            Some(NOW),
            &Some(Link {
                page: "accounts",
                id: "lead".into()
            }),
            &json!({ "account": "lead", "days_left": 3, "expires": NOW + 3 * DAY, "label": "Lead" })
        ),
        "an end exactly three days out holds, dated from the read"
    );
    let outside = find(&conditions, Kind::PairingExpiring, "outside").unwrap();
    assert_eq!(
        (outside.holds, outside.since, &outside.detail["days_left"]),
        (false, None, &json!(3)),
        "an end a millisecond past the three days reads clear"
    );
    let ended = find(&conditions, Kind::PairingExpiring, "ended").unwrap();
    assert_eq!(
        (ended.holds, ended.since, &ended.detail["days_left"]),
        (true, Some(NOW - 3 * DAY - 1_000), &json!(-1)),
        "an end a second ago still holds, its days left negative"
    );
}

#[tokio::test]
async fn a_failed_reload_holds_certificate_reload_failed_until_a_reload_succeeds() {
    let rig = rig().await;
    let pairs = Arc::new(Pairs::default());
    let (attention, _arm) = rig.attention_with_pairs(None, Some(pairs.clone()));
    let line = "proxy.tls.key_file /etc/phoebis/proxy.key: does not match the certificate in /etc/phoebis/proxy.pem";

    pairs.set(vec![PairFacts {
        failed: Some((NOW, line.to_string())),
        ..pair("proxy.tls", NOW + 90 * DAY)
    }]);
    let conditions = read(&attention, NOW + 1_000).await;
    let held = find(&conditions, Kind::CertificateReloadFailed, "proxy.tls").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.link,
            held.pools.is_empty(),
            &held.detail
        ),
        (
            true,
            Some(NOW),
            &router_subject("proxy.tls"),
            &None,
            true,
            &json!({
                "cert_file": "/etc/phoebis/proxy.pem",
                "error": line,
                "key_file": "/etc/phoebis/proxy.key",
                "serving_since": NOW - DAY,
            })
        ),
        "dated at the failed reload, naming the pair still served"
    );
    assert!(
        !find(&conditions, Kind::CertificateExpiring, "proxy.tls")
            .unwrap()
            .holds,
        "a failed reload says nothing about the served leaf's end"
    );

    pairs.set(vec![PairFacts {
        loaded_at: NOW + 2_000,
        ..pair("proxy.tls", NOW + 90 * DAY)
    }]);
    let cleared = read(&attention, NOW + 3_000).await;
    let cleared = find(&cleared, Kind::CertificateReloadFailed, "proxy.tls").unwrap();
    assert_eq!(
        (cleared.holds, cleared.since),
        (false, None),
        "a reload that lands clears it"
    );
}

#[test]
fn the_certificate_kinds_rank_among_the_others() {
    let mut kinds = vec![
        Kind::PollBackoff,
        Kind::WallReached,
        Kind::CertificateExpiring,
        Kind::ReauthRequired,
        Kind::MeterDrift,
        Kind::LedgerRecordParked,
        Kind::CertificateReloadFailed,
        Kind::PoolExhausted,
        Kind::RefreshFailed,
        Kind::PairingExpiring,
        Kind::ModelUnpriced,
        Kind::WriterDegraded,
        Kind::FundingRequired,
        Kind::ClientDrift,
        Kind::LimitResetEnding,
    ];
    kinds.sort();
    assert_eq!(
        kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        [
            "pool_exhausted",
            "client_drift",
            "writer_degraded",
            "certificate_reload_failed",
            "reauth_required",
            "funding_required",
            "refresh_failed",
            "pairing_expiring",
            "certificate_expiring",
            "limit_reset_ending",
            "ledger_record_parked",
            "poll_backoff",
            "model_unpriced",
            "meter_drift",
            "wall_reached",
        ]
    );
}

/// A grant in the upstream's field names: one reset of one, usable now, that clears the Fable week from 2023-10-01 to
/// `ends_at`.
fn grant(id: &str, ends_at: &str) -> serde_json::Value {
    json!({
        "clears": ["seven_day_overage_included"],
        "ends_at": ends_at,
        "id": id,
        "label": "Launch reset",
        "resets_left": 1,
        "resets_total": 1,
        "starts_at": "2023-10-01T00:00:00Z",
        "usable_now": true,
    })
}

/// Stores `block` on `business` as the block read does: parsed from a usage body and written as its typed form under
/// the row's holder and epoch, which a block write leaves as they are.
fn store_resets(rig: &Rig, block: serde_json::Value) {
    let body = json!({ "cedar_ember": block }).to_string();
    let parsed = phoebis_oauth::parse_limit_resets(body.as_bytes())
        .unwrap()
        .expect("the body carries a block");
    let stored = serde_json::to_string(&parsed).unwrap();
    assert!(
        rig.store
            .set_limit_resets("business", 1, 1, 0, Some(&stored), NOW)
            .unwrap(),
        "the block lands on the rig's row"
    );
}

/// The reset item of `business`, read under the rig's warning of five days.
async fn reset_item(attention: &Arc<Attention>, at: i64) -> Condition {
    find(
        &read(attention, at).await,
        Kind::LimitResetEnding,
        "business",
    )
    .cloned()
    .expect("an account the poll asks carries the reset series")
}

#[tokio::test]
async fn a_reset_holds_from_the_threshold_before_its_end() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);

    store_resets(
        &rig,
        json!({ "eligible": true, "grants": [grant("rg-b-1", "2023-11-20T22:13:20Z")] }),
    );
    let outside = reset_item(&attention, NOW).await;
    assert_eq!(
        (outside.holds, outside.since),
        (false, None),
        "an end six days out lies outside the five days"
    );

    store_resets(
        &rig,
        json!({ "eligible": true, "grants": [grant("rg-b-1", "2023-11-18T22:13:20Z")] }),
    );
    let inside = reset_item(&attention, NOW).await;
    assert_eq!(
        (
            inside.holds,
            inside.since,
            &inside.detail["days_left"],
            &inside.detail["ends_at"]
        ),
        (
            true,
            Some(1_699_913_600_000),
            &json!(4),
            &json!(1_700_345_600_000_i64)
        ),
        "an end four days out holds, dated five days before the end and not from the read"
    );

    store_resets(
        &rig,
        json!({ "eligible": true, "grants": [grant("rg-b-1", "2023-11-19T22:13:20Z")] }),
    );
    let edge = reset_item(&attention, NOW).await;
    assert_eq!(
        (
            edge.holds,
            edge.since,
            &edge.detail["days_left"],
            &edge.detail["ends_at"]
        ),
        (
            true,
            Some(1_700_000_000_000),
            &json!(5),
            &json!(1_700_432_000_000_i64)
        ),
        "an end exactly five days out holds, dated from the read"
    );
}

#[tokio::test]
async fn a_used_reset_ends_nothing() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let mut used = grant("rg-b-1", "2023-11-18T22:13:20Z");
    used["resets_left"] = json!(0);
    store_resets(&rig, json!({ "eligible": true, "grants": [used] }));
    let item = reset_item(&attention, NOW).await;
    assert_eq!(
        (item.holds, item.since),
        (false, None),
        "a grant inside the five days with no reset left holds nothing"
    );
}

#[tokio::test]
async fn the_reset_item_counts_the_resets_that_end_inside_the_threshold() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let mut two = grant("rg-b-1", "2023-11-17T22:13:20Z");
    two["resets_left"] = json!(2);
    two["resets_total"] = json!(2);
    store_resets(
        &rig,
        json!({
            "eligible": true,
            "grants": [
                two,
                grant("rg-b-2", "2023-11-24T22:13:20Z"),
                grant("rg-b-3", "2023-11-13T22:13:20Z"),
            ],
        }),
    );
    let item = reset_item(&attention, NOW).await;
    assert_eq!(
        (
            item.holds,
            &item.detail["ending"],
            &item.detail["resets_left"],
            &item.detail["grant"]
        ),
        (true, &json!(2), &json!(3), &json!("rg-b-1")),
        "the two resets ending in three days count, the grant ten days out and the ended one do not, and the \
         account's count leaves the ended grant out"
    );
}

#[tokio::test]
async fn a_reset_shorter_than_the_threshold_is_dated_from_its_start() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let mut short = grant("rg-b-1", "2023-11-16T22:13:20Z");
    short["starts_at"] = json!("2023-11-12T22:13:20Z");
    store_resets(&rig, json!({ "eligible": true, "grants": [short] }));
    let item = reset_item(&attention, NOW).await;
    assert_eq!(
        (item.holds, item.since),
        (true, Some(1_699_827_200_000)),
        "a grant that began two days back and ends in two is dated from its start, never before it existed"
    );
}

/// A usage body with the 5-hour window under its limit and the Fable week at `percent`.
fn fable_week(percent: u32) -> fake_upstream::Respond {
    fake_upstream::Respond::Json {
        status: 200,
        headers: vec![],
        body: json!({
            "five_hour": { "utilization": 12.5, "resets_at": null },
            "limits": [{
                "kind": "weekly_scoped",
                "scope": { "model": { "display_name": "Fable" } },
                "percent": percent,
                "resets_at": null,
            }],
        })
        .to_string()
        .into_bytes(),
    }
}

#[tokio::test]
async fn the_wall_offers_a_reset_only_for_a_window_at_its_limit_now() {
    let up = fake_upstream::FakeUpstream::start(vec![fake_upstream::Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: fable_week(46),
    }])
    .await;
    let rig = rig().await;
    let usage = usage_over(&rig, &up);
    let (attention, _arm) = rig.attention(Some(usage.clone()));
    routing_event(
        &rig,
        NOW - HOUR,
        "window_spent",
        "business",
        None,
        json!({ "window": "seven_day_fable", "source": "head" }),
    );
    fold_at(&rig, NOW).await;
    store_resets(
        &rig,
        json!({ "eligible": true, "grants": [grant("rg-b-1", "2023-11-24T22:13:20Z")] }),
    );

    usage.poll("business").await.unwrap();
    let under = read(&attention, NOW).await;
    let wall = find(&under, Kind::WallReached, "business").unwrap();
    assert_eq!(
        (wall.holds, wall.detail.get("reset")),
        (true, None),
        "the row names the Fable week spent an hour ago, and the reading has it under its limit now"
    );

    up.set_rule_after(0, "/api/oauth/usage", fable_week(100));
    rig.at(NOW + 5 * MINUTE);
    usage.poll("business").await.unwrap();
    let at_limit = read(&attention, NOW + 5 * MINUTE).await;
    let wall = find(&at_limit, Kind::WallReached, "business").unwrap();
    assert_eq!(
        (wall.holds, wall.detail.get("reset")),
        (
            true,
            Some(&json!({
                "clears": ["seven_day_fable"],
                "grant": "rg-b-1",
                "label": "Launch reset",
            }))
        ),
        "the reading has the Fable week at its limit, which the usable grant clears under the router's name"
    );
}

#[tokio::test]
async fn a_reset_on_a_cooldown_clears_no_wall() {
    let up = fake_upstream::FakeUpstream::start(vec![fake_upstream::Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: fable_week(100),
    }])
    .await;
    let rig = rig().await;
    let usage = usage_over(&rig, &up);
    let (attention, _arm) = rig.attention(Some(usage.clone()));
    routing_event(
        &rig,
        NOW - HOUR,
        "window_spent",
        "business",
        None,
        json!({ "window": "seven_day_fable", "source": "head" }),
    );
    fold_at(&rig, NOW).await;
    // The grant keeps `usable_now` true, as a claim's fold leaves the other grants under the cooldown it writes.
    store_resets(
        &rig,
        json!({
            "cooldown_until": "2023-11-15T22:13:20Z",
            "eligible": true,
            "grants": [grant("rg-b-1", "2023-11-24T22:13:20Z")],
        }),
    );

    usage.poll("business").await.unwrap();
    let conditions = read(&attention, NOW).await;
    let wall = find(&conditions, Kind::WallReached, "business").unwrap();
    assert_eq!(
        (wall.holds, wall.detail.get("reset")),
        (true, None),
        "a grant whose reset waits out the block's cooldown clears no wall, though it has a reset left"
    );
}

#[tokio::test]
async fn a_plan_that_ended_is_offered_no_reset() {
    let up = fake_upstream::FakeUpstream::start(vec![fake_upstream::Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: fable_week(100),
    }])
    .await;
    let rig = rig().await;
    let usage = usage_over(&rig, &up);
    let (attention, _arm) = rig.attention(Some(usage.clone()));
    routing_event(
        &rig,
        NOW - HOUR,
        "window_spent",
        "business",
        None,
        json!({ "window": "seven_day_fable", "source": "head" }),
    );
    fold_at(&rig, NOW).await;
    store_resets(
        &rig,
        json!({ "eligible": true, "grants": [grant("rg-b-1", "2023-11-18T22:13:20Z")] }),
    );
    usage.poll("business").await.unwrap();

    let asked = read(&attention, NOW).await;
    let item = find(&asked, Kind::LimitResetEnding, "business").unwrap();
    let wall = find(&asked, Kind::WallReached, "business").unwrap();
    assert_eq!(
        (item.holds, wall.holds, &wall.detail["reset"]["grant"]),
        (true, true, &json!("rg-b-1")),
        "a plan the poll asks warns of the reset that ends in four days, and its wall offers it"
    );

    assert!(rig.store.set_plan_ended("business", NOW).unwrap());
    let ended = read(&attention, NOW).await;
    let wall = find(&ended, Kind::WallReached, "business").unwrap();
    assert_eq!(
        (
            find(&ended, Kind::LimitResetEnding, "business").map(|c| c.holds),
            wall.holds,
            wall.detail.get("reset")
        ),
        (None, true, None),
        "a plan that ended carries no reset item, and the wall the day's fold wrote offers no reset"
    );
}

/// A finished request of pool `main` on `model`, received at `at`, whose `attempts` attempts on `account` each used a
/// thousand input tokens: plan-funded on `business`, billed on `direct`.
fn on_model(id: &str, model: &str, account: &str, at: i64, attempts: u32) -> Record {
    let account_kind = if account == "direct" {
        AccountKind::ApiKey
    } else {
        AccountKind::Oauth
    };
    Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: model.into(),
            model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts,
            ..Default::default()
        },
        attempts: (1..=attempts)
            .map(|attempt| AttemptRecord {
                request_id: id.into(),
                attempt,
                account_id: account.into(),
                account_kind,
                started_at: at,
                outcome: "succeeded".into(),
                input_tokens: Some(1_000),
                ..Default::default()
            })
            .collect(),
        bodies: None,
    }
}

/// The requests the `model_unpriced` item about `model` counts, 0 while the read carries none.
fn unpriced_requests(conditions: &[Condition], model: &str) -> u64 {
    find(conditions, Kind::ModelUnpriced, model)
        .map_or(0, |c| c.detail["requests"].as_u64().unwrap())
}

/// The read once the item about `model` counts `requests` or more: the ledger records it after the landing commits,
/// so a read waits for it rather than for the row.
async fn read_counting(
    attention: &Arc<Attention>,
    at: i64,
    model: &str,
    requests: u64,
) -> Vec<Condition> {
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        let conditions = read(attention, at).await;
        (unpriced_requests(&conditions, model) >= requests).then_some(conditions)
    })
    .await
}

#[tokio::test]
async fn a_plan_request_on_an_unpriced_model_holds_one_item_counting_each_landed_request_once() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // Apart from the clock, so the item's date reads the request's arrival rather than the read.
    let arrived = NOW - 60_000;
    rig.ledger
        .record_request(on_model("r1", "claude-opus-5", "business", arrived, 1))
        .await;
    let first = read_counting(&attention, NOW, "claude-opus-5", 1).await;
    let held = find(&first, Kind::ModelUnpriced, "claude-opus-5").unwrap();
    assert_eq!(
        (
            held.holds,
            held.since,
            &held.subject,
            &held.link,
            held.pools.is_empty(),
            &held.detail
        ),
        (
            true,
            Some(arrived),
            &Subject {
                kind: SubjectKind::Model,
                id: "claude-opus-5".into(),
                label: "claude-opus-5".into(),
            },
            &None,
            true,
            &json!({
                "model": "claude-opus-5",
                "requests": 1,
            })
        ),
        "the item names the model and the requests it served, opens no page and belongs to no pool card"
    );
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="model_unpriced",subject="claude-opus-5"} 1"#),
        "{text}"
    );
    rig.ledger
        .record_request(on_model("r2", "claude-opus-5-20260101", "business", NOW, 1))
        .await;
    let second = read_counting(&attention, NOW, "claude-opus-5", 2).await;
    assert_eq!(
        (
            unpriced_requests(&second, "claude-opus-5"),
            find(&second, Kind::ModelUnpriced, "claude-opus-5")
                .unwrap()
                .since,
            find(&second, Kind::ModelUnpriced, "claude-opus-5-20260101").is_none()
        ),
        (2, Some(arrived), true),
        "a dated id counts under its undated name, and the item keeps the first request's date"
    );
    rig.ledger
        .record_request(on_model("r3", "claude-opus-5", "business", NOW, 2))
        .await;
    let third = read_counting(&attention, NOW, "claude-opus-5", 3).await;
    assert_eq!(
        unpriced_requests(&third, "claude-opus-5"),
        3,
        "a request whose two attempts both answered counts once"
    );
}

#[tokio::test]
async fn a_weighted_model_and_a_billed_request_raise_no_item_and_a_model_of_no_weighted_tier_raises_one(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    let mut classifier = on_model("r-classifier", "claude-opus-9", "business", NOW, 1);
    let Record::Request { request, .. } = &mut classifier else {
        unreachable!("on_model builds a request record")
    };
    request.classifier = Some("client".into());
    for record in [
        on_model("r-weighted", "claude-opus-5-5", "business", NOW, 1),
        on_model("r-mystery", "claude-mystery-2", "business", NOW, 1),
        on_model("r-billed", "claude-opus-5", "direct", NOW, 1),
        classifier,
        // Lands behind the four, so the read that sees its item sees whatever their landings recorded.
        on_model("r-marker", "claude-sonnet-5", "business", NOW, 1),
    ] {
        rig.ledger.record_request(record).await;
    }
    let conditions = read_counting(&attention, NOW, "claude-sonnet-5", 1).await;
    assert_eq!(
        conditions
            .iter()
            .filter(|c| c.kind == Kind::ModelUnpriced)
            .map(|c| c.subject.id.as_str())
            .collect::<Vec<_>>(),
        ["claude-mystery-2", "claude-sonnet-5"],
        "a model without a weight raises its item whatever its model tier, a weighted model and a classifier call \
         raise none, and an API key's request reads no window share"
    );
}

#[tokio::test]
async fn a_model_weighted_on_the_5_hour_window_alone_raises_its_item() {
    // The first entry, in force at the rig's clock, weighs `claude-opus-5` on the 5-hour window alone, so its 5-hour
    // share prices while its Pro-week figure stays null.
    let five_hour_only = COSTS.replacen(
        "    overrides:\n",
        "    overrides:\n      claude-opus-5: {weight_by_window: {five_hour: 2}}\n",
        1,
    );
    let rig = rig_with_tables(tables_of(&five_hour_only)).await;
    let (attention, _arm) = rig.attention(None);
    for record in [
        on_model("r-five-hour", "claude-opus-5", "business", NOW, 1),
        // Lands behind it, so the read that sees its item sees whatever the first landing recorded.
        on_model("r-marker", "claude-sonnet-5", "business", NOW, 1),
    ] {
        rig.ledger.record_request(record).await;
    }
    let conditions = read_counting(&attention, NOW, "claude-sonnet-5", 1).await;
    assert_eq!(
        conditions
            .iter()
            .filter(|c| c.kind == Kind::ModelUnpriced)
            .map(|c| c.subject.id.as_str())
            .collect::<Vec<_>>(),
        ["claude-opus-5", "claude-sonnet-5"],
        "a weight on the 5-hour window leaves the Pro-week figure unpriced, so the model raises its item"
    );
}

#[tokio::test]
async fn a_revision_that_weights_the_model_clears_its_item_at_the_next_landing_and_its_series_reads_0(
) {
    let (first, second) = COSTS
        .split_once("  - effective: 2026-02-01\n")
        .expect("the cost fixture carries a second entry");
    let weighted = format!(
        "{first}  - effective: 2026-02-01\n{}",
        second.replacen("claude-opus-5: null", "claude-opus-5: 3", 1)
    );
    let rig = rig_with_tables(tables_of(&weighted)).await;
    let (attention, _arm) = rig.attention(None);
    rig.ledger
        .record_request(on_model("r1", "claude-opus-5", "business", NOW, 1))
        .await;
    read_counting(&attention, NOW, "claude-opus-5", 1).await;
    let later = phoebis_ledger::effective_instant("2026-02-01").unwrap() + 43_200_000;
    rig.at(later);
    rig.ledger
        .record_request(on_model("r2", "claude-opus-5", "business", later, 1))
        .await;
    let cleared = fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        // At an instant the first entry governs, where the read clears nothing itself, so the landing's clear alone
        // answers it.
        let conditions = read(&attention, NOW).await;
        find(&conditions, Kind::ModelUnpriced, "claude-opus-5")
            .is_some_and(|c| !c.holds)
            .then_some(conditions)
    })
    .await;
    let item = find(&cleared, Kind::ModelUnpriced, "claude-opus-5").unwrap();
    assert_eq!(
        (item.since, &item.detail),
        (
            None,
            &json!({
                "model": "claude-opus-5",
                "requests": 0,
            })
        ),
        "the landing under the revision that weights the model clears the date and the count"
    );
    let (_, text) = scrape(&rig, &attention).await;
    assert!(
        text.contains(r#"phoebis_attention{kind="model_unpriced",subject="claude-opus-5"} 0"#),
        "the cleared model reads 0 under the same labels rather than vanishing: {text}"
    );
}

#[tokio::test]
async fn a_keepalive_cap_holds_its_item_from_ninety_percent_of_its_max() {
    let rig = rig().await;
    let standing = Arc::new(Standing::idle());
    let (attention, _arm) = rig.attention_with_keepalive(standing.clone());
    let fill = |count| {
        standing.0.lock().unwrap().caps = vec![KeepaliveCap {
            name: "sessions",
            count,
            max: 100,
        }];
    };
    fill(89);
    assert!(
        !find(
            &read(&attention, NOW).await,
            Kind::KeepaliveCap,
            "keepalive.sessions"
        )
        .unwrap()
        .holds,
        "89 of 100 sits under the bar"
    );
    fill(90);
    let conditions = read(&attention, NOW + MINUTE).await;
    let cap = find(&conditions, Kind::KeepaliveCap, "keepalive.sessions").unwrap();
    assert_eq!(
        (
            cap.holds,
            cap.since,
            &cap.subject,
            cap.pools.is_empty(),
            &cap.link,
            &cap.detail
        ),
        (
            true,
            Some(NOW + MINUTE),
            &Subject {
                kind: SubjectKind::Router,
                id: "keepalive.sessions".into(),
                label: "Keepalive".into(),
            },
            true,
            &None,
            &json!({ "cap": "sessions", "count": 90, "max": 100, "pct": 0.9 })
        ),
        "90 of 100 raises the item, dated by the read that first saw it"
    );
}

/// The reads after the clear date a re-raised item by its own run and keep that instant while it holds, which breaks
/// when the pushes run after the read's prune of the first-seen entries and every read dates the item afresh.
#[tokio::test]
async fn a_bus_gap_holds_its_item_for_a_day_and_clears_after() {
    let rig = rig().await;
    let standing = Arc::new(Standing::idle());
    let (attention, _arm) = rig.attention_with_keepalive(standing.clone());
    let gap_at = |at| {
        let mut standing = standing.0.lock().unwrap();
        standing.bus_gaps = 1;
        standing.last_bus_gap_at = Some(at);
    };
    let holds = async |at| {
        find(
            &read(&attention, at).await,
            Kind::KeepaliveBusGaps,
            "keepalive",
        )
        .unwrap()
        .holds
    };
    assert!(!holds(NOW).await, "no gap raises nothing");
    gap_at(NOW - DAY - MINUTE);
    assert!(
        !holds(NOW).await,
        "a gap a day and a minute old raises nothing"
    );
    gap_at(NOW - DAY + MINUTE);
    let conditions = read(&attention, NOW).await;
    let gap = find(&conditions, Kind::KeepaliveBusGaps, "keepalive").unwrap();
    assert_eq!(
        (gap.holds, gap.since, &gap.detail),
        (
            true,
            Some(NOW),
            &json!({ "gaps": 1, "last_at": NOW - DAY + MINUTE })
        ),
        "a gap 23 hours and 59 minutes old raises the item, dated by the read that first saw it"
    );
    let later = read(&attention, NOW + 2 * MINUTE).await;
    let gap = find(&later, Kind::KeepaliveBusGaps, "keepalive").unwrap();
    assert_eq!(
        (gap.holds, gap.since),
        (false, None),
        "a day after the last gap the item clears"
    );
    gap_at(NOW + 3 * MINUTE);
    let raised = read(&attention, NOW + 4 * MINUTE).await;
    let gap = find(&raised, Kind::KeepaliveBusGaps, "keepalive").unwrap();
    assert_eq!(
        (gap.holds, gap.since),
        (true, Some(NOW + 4 * MINUTE)),
        "a gap after the clear raises the item again, dated by its new run, never the earlier one"
    );
    let held = read(&attention, NOW + 5 * MINUTE).await;
    let gap = find(&held, Kind::KeepaliveBusGaps, "keepalive").unwrap();
    assert_eq!(
        gap.since,
        Some(NOW + 4 * MINUTE),
        "the item keeps its first-seen instant while it holds"
    );
}

/// A finished request of pool `main` whose serving attempt read nothing from the cache under the miss reason `none`
/// after a previous request of 1,000 tokens, which the ledger's rollup counts as a cache entry that expired between
/// turns.
fn expired_turn(id: &str) -> Record {
    Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: NOW,
            finished_at: Some(NOW + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: AccountKind::Oauth,
            started_at: NOW,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            cache_read_tokens: Some(0),
            cache_miss_reason: Some("none".into()),
            previous_input_tokens: Some(1_000),
            ..Default::default()
        }],
        bodies: None,
    }
}

/// The read's expiry item as `(holds, detail)`.
async fn expiry_item(attention: &Arc<Attention>) -> (bool, serde_json::Value) {
    let conditions = read(attention, NOW).await;
    let item = find(&conditions, Kind::ExpiredBetweenTurns, "keepalive").unwrap();
    (item.holds, item.detail.clone())
}

/// Records `expired_turn("r1")` and waits until the item holds, so a clear read after it is taken with the expiry
/// landed.
async fn expiry_landed(rig: &Rig, attention: &Arc<Attention>) -> serde_json::Value {
    rig.ledger.record_request(expired_turn("r1")).await;
    // The rollup lands after the call returns, so the read waits for the item rather than for the call.
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        let (holds, detail) = expiry_item(attention).await;
        holds.then_some(detail)
    })
    .await
}

#[tokio::test]
async fn expiries_between_turns_hold_their_item_only_while_their_pool_warms_caches() {
    let rig = rig().await;
    let standing = Arc::new(Standing::idle());
    standing.0.lock().unwrap().warming = BTreeSet::from(["main".to_string()]);
    let (attention, _arm) = rig.attention_with_keepalive(standing.clone());
    assert_eq!(
        expiry_landed(&rig, &attention).await,
        json!({ "expired": 1 })
    );
    standing.0.lock().unwrap().warming = BTreeSet::from(["other".to_string()]);
    assert_eq!(
        expiry_item(&attention).await,
        (false, json!({ "expired": 0 })),
        "an expiry on a pool whose policy is off counts nothing while another pool warms caches"
    );
    standing.0.lock().unwrap().warming = BTreeSet::new();
    assert!(
        !find(
            &read(&attention, NOW).await,
            Kind::ExpiredBetweenTurns,
            "keepalive"
        )
        .unwrap()
        .holds,
        "with no pool warming caches the same expiry raises nothing"
    );
}

#[tokio::test]
async fn expiries_between_turns_hold_their_item_only_under_a_key_with_the_keepalive_permission() {
    let rig = rig().await;
    let standing = Arc::new(Standing::idle());
    standing.0.lock().unwrap().warming = BTreeSet::from(["main".to_string()]);
    let (attention, _arm) = rig.attention_with_keepalive(standing);
    assert_eq!(
        expiry_landed(&rig, &attention).await,
        json!({ "expired": 1 }),
        "the seed's key holds the permission, so its expiry raises the item"
    );
    let mut key = rig.store.api_key("desktop").unwrap().unwrap();
    key.companion_permissions.keepalive = false;
    rig.store
        .update_api_key_with_guardrails(&key, key.updated_at, None, "test")
        .unwrap();
    rig.router.reload().await.unwrap();
    assert_eq!(
        expiry_item(&attention).await,
        (false, json!({ "expired": 0 })),
        "an expiry under a key the keepalive may not probe for counts nothing on a warming pool"
    );
}
