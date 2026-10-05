//! The eight conditions through the reader: each kind's hold and clear, the series at 0 for a subject in the clear,
//! the router's own components among them, a deleted subject's series gone unless a parked record still names it, a
//! refresh run read back by a second reader, and the gauge the metrics listener scrapes from the same read.
mod common;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use http_body_util::BodyExt;
use phoebis_attention::{
    Attention, AttentionError, Condition, Kind, Link, Subject, SubjectKind,
    BACKOFF_ATTENTION_AFTER_MS,
};
use phoebis_bus::{Kind as RequestKind, Origin, Outcome};
use phoebis_ledger::{AttemptRecord, Metrics, PriceTable, Pricing, Record, RequestRecord};
use phoebis_oauth::PollOutcome;
use phoebis_store::{AccountState, AttemptOutcome, NewBundle, Secret, StorageKey, Store};
use serde_json::json;
use tower::ServiceExt;

/// The condition of `kind` about `subject`, if the read carries a series for it.
fn find<'a>(conditions: &'a [Condition], kind: Kind, subject: &str) -> Option<&'a Condition> {
    conditions
        .iter()
        .find(|c| c.kind == kind && c.subject.id == subject)
}

fn read(attention: &Attention, at: i64) -> Vec<Condition> {
    attention.conditions(at).unwrap()
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
        series(&read(&attention, NOW)),
        [
            ("funding_required", "direct".to_string(), false),
            ("ledger_record_parked", "main".to_string(), false),
            ("poll_backoff", "business".to_string(), false),
            ("pool_exhausted", "main".to_string(), false),
            ("reauth_required", "business".to_string(), false),
            ("reauth_required", "direct".to_string(), false),
            ("refresh_failed", "business".to_string(), false),
            ("writer_degraded", "ledger".to_string(), false),
            ("writer_degraded", "routing".to_string(), false),
        ],
        "every account carries its re-auth state, an Anthropic key its funding, and an OAuth account its refresh run \
         and, before any poll status exists, its backoff; every pool its parked records, and each writer its state"
    );
}

#[tokio::test]
async fn a_pool_exhaustion_holds_from_its_first_exhausted_outcome_until_a_request_of_the_pool_succeeds(
) {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // A completion with no observation names no pool, so it decides nothing.
    attention.observe(&complete("stray", Outcome::Exhausted, None, None, NOW));
    assert!(
        !find(&read(&attention, NOW), Kind::PoolExhausted, "main")
            .unwrap()
            .holds
    );
    for (id, at) in [("r1", NOW + 1_000), ("r2", NOW + 2_000)] {
        attention.observe(&observed(id, Some("desktop"), "main"));
        attention.observe(&complete(id, Outcome::Exhausted, None, None, at));
    }
    let conditions = read(&attention, NOW + 3_000);
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
        find(&read(&attention, NOW + 4_000), Kind::PoolExhausted, "main")
            .unwrap()
            .holds
    );
    attention.observe(&observed("r5", Some("desktop"), "main"));
    attention.observe(&complete("r5", Outcome::Succeeded, None, None, NOW + 5_000));
    let cleared = read(&attention, NOW + 5_000);
    let cleared = find(&cleared, Kind::PoolExhausted, "main").unwrap();
    assert_eq!(
        (cleared.holds, cleared.since),
        (false, None),
        "a success clears it, and the series stays at 0"
    );
}

#[tokio::test]
async fn a_drift_refusal_holds_until_a_request_from_that_version_is_relayed_and_keeps_its_series() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    assert!(
        find(&read(&attention, NOW), Kind::ClientDrift, "2.1.274").is_none(),
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
    let conditions = read(&attention, NOW + 1_000);
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
        find(&read(&attention, NOW + 2_000), Kind::ClientDrift, "2.1.274")
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
    let conditions = read(&attention, NOW + 3_000);
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
    let conditions = read(&attention, NOW + 9_000);
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
    attention.observe(&complete(
        "d1",
        Outcome::Rejected,
        Some("hash"),
        Some("2.1.274"),
        NOW,
    ));
    let conditions = read(&attention, NOW);
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
        find(&read(&attention, NOW), Kind::ClientDrift, "unknown")
            .unwrap()
            .holds
    );
    attention.observe(&observed("d2", Some("desktop"), "main"));
    attention.observe(&complete("d2", Outcome::Succeeded, None, None, NOW + 1_000));
    let conditions = read(&attention, NOW + 1_000);
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
        attention.observe(&complete(
            id,
            Outcome::Succeeded,
            None,
            Some("2.1.274"),
            NOW + 1_000,
        ));
        assert!(
            find(&read(&attention, NOW + 1_000), Kind::ClientDrift, "2.1.274")
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
        !find(&read(&attention, NOW + 2_000), Kind::ClientDrift, "2.1.274")
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
    let conditions = read(&attention, NOW + 40);
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
    let cleared = read(&attention, NOW + 50);
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
    let subjects = drift_subjects(&read(&attention, NOW + 30));
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
    let conditions = read(&attention, NOW);
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

#[tokio::test]
async fn the_account_states_hold_by_kind_and_date_from_the_first_read_that_saw_them() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.store
        .set_state("business", AccountState::ReauthRequired)
        .unwrap();
    rig.store
        .set_state("direct", AccountState::FundingRequired)
        .unwrap();
    read(&attention, NOW + 1_000);
    let conditions = read(&attention, NOW + 5_000);
    let reauth = find(&conditions, Kind::ReauthRequired, "business").unwrap();
    assert_eq!(
        (reauth.holds, reauth.since, &reauth.pools, &reauth.detail),
        (
            true,
            Some(NOW + 1_000),
            &vec!["main".to_string()],
            &json!({ "account": "business", "label": "business" })
        ),
        "the registry records no time for a state change, so the first read that saw it dates it"
    );
    let funding = find(&conditions, Kind::FundingRequired, "direct").unwrap();
    assert_eq!((funding.holds, funding.since), (true, Some(NOW + 1_000)));
    assert_eq!(
        funding.link,
        Some(Link {
            page: "accounts",
            id: "direct".into()
        })
    );
    rig.store
        .set_state("business", AccountState::Ready)
        .unwrap();
    rig.store.set_state("direct", AccountState::Ready).unwrap();
    let conditions = read(&attention, NOW + 6_000);
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
    rig.store
        .set_state("business", AccountState::ReauthRequired)
        .unwrap();
    let conditions = read(&attention, NOW + 9_000);
    assert_eq!(
        find(&conditions, Kind::ReauthRequired, "business")
            .unwrap()
            .since,
        Some(NOW + 9_000),
        "a state that cleared and came back dates from the read that saw it again"
    );
    // The relay marks an Anthropic key the upstream refuses as it marks an OAuth login whose refresh is gone.
    rig.store
        .set_state("direct", AccountState::ReauthRequired)
        .unwrap();
    let conditions = read(&attention, NOW + 12_000);
    let refused = find(&conditions, Kind::ReauthRequired, "direct").unwrap();
    assert_eq!(
        (refused.holds, refused.since, &refused.pools, &refused.link),
        (
            true,
            Some(NOW + 12_000),
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

fn resolve(rig: &Rig, outcome: AttemptOutcome) -> u64 {
    let id = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store.resolve_refresh_attempt(id, 1, outcome).unwrap();
    id
}

#[tokio::test]
async fn two_failed_refreshes_in_a_row_hold_until_one_commits_and_date_from_the_first() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    resolve(&rig, AttemptOutcome::Failed("invalid_grant".into()));
    let conditions = read(&attention, NOW);
    let one = find(&conditions, Kind::RefreshFailed, "business").unwrap();
    assert_eq!(
        (one.holds, &one.detail["failures"]),
        (false, &json!(1)),
        "one failure is a token endpoint having a bad moment"
    );
    // Apart by more than a millisecond, so the run's start can only be read off the older attempt.
    tokio::time::sleep(Duration::from_millis(2)).await;
    let lost = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(lost, 1, AttemptOutcome::Uncertain("lost".into()))
        .unwrap();
    let attempts = rig
        .store
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap();
    assert!(attempts[1].started_at < attempts[0].started_at);
    let conditions = read(&attention, NOW);
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
    let conditions = read(&attention, NOW);
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
        AttemptOutcome::Uncertain("refresh response lost".into()),
    );
    // Apart by more than a millisecond, so the commit settles the lost row again under a later time than its own.
    tokio::time::sleep(Duration::from_millis(2)).await;
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
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap();
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
    let cleared = read(&attention, NOW);
    let cleared = find(&cleared, Kind::RefreshFailed, "business").unwrap();
    assert_eq!(
        (cleared.holds, &cleared.detail["failures"]),
        (false, &json!(0)),
        "the superseded row is no failure"
    );

    // A refresh that fails after the commit still counts, once: one failure holds nothing, the second holds.
    resolve(&rig, AttemptOutcome::Failed("invalid_grant".into()));
    let one = read(&attention, NOW);
    let one = find(&one, Kind::RefreshFailed, "business").unwrap();
    assert_eq!((one.holds, &one.detail["failures"]), (false, &json!(1)));
    resolve(&rig, AttemptOutcome::Failed("invalid_grant".into()));
    let held = read(&attention, NOW);
    let held = find(&held, Kind::RefreshFailed, "business").unwrap();
    assert_eq!((held.holds, &held.detail["failures"]), (true, &json!(2)));
}

/// A pairing commits a bundle under no attempt, settling the retry still pending as `superseded`: that row exists only
/// because a newer credential committed, so it ends the run as a commit does.
#[tokio::test]
async fn a_pairing_that_supersedes_a_pending_retry_leaves_no_refresh_run() {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let rig = rig_with_store(store.clone()).await;
    let failed = resolve(&rig, AttemptOutcome::Failed("invalid_grant".into()));
    tokio::time::sleep(Duration::from_millis(2)).await;
    let pending = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
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
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap()
        .into_iter()
        .map(|a| (a.id, a.outcome, a.detail))
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
    let conditions = read(&attention, NOW);
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
        resolve(&rig, AttemptOutcome::Failed(detail.into()));
    }
    assert!(
        find(&read(&first, NOW), Kind::RefreshFailed, "business")
            .unwrap()
            .holds
    );
    arm.abort();
    drop(first);
    let (second, _arm) = rig.attention(None);
    assert!(
        find(&read(&second, NOW), Kind::RefreshFailed, "business")
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
async fn a_poll_backoff_holds_thirty_minutes_into_a_run_of_429s_and_clears_on_a_clean_poll() {
    let up = usage_upstream(429).await;
    let rig = rig().await;
    let usage = usage_over(&rig, &up);
    let (attention, _arm) = rig.attention(Some(usage.clone()));
    let before = read(&attention, NOW);
    let unpolled = find(&before, Kind::PollBackoff, "business").unwrap();
    assert_eq!(
        (unpolled.holds, &unpolled.detail["backoff_since"]),
        (false, &json!(null)),
        "an account the poll asks carries its series before its first poll status exists"
    );
    assert!(
        find(&before, Kind::PollBackoff, "direct").is_none(),
        "the poll never asks an Anthropic key"
    );
    usage.poll("business").await.unwrap();
    // A second 429 once the first step's two minutes have passed: the run keeps its start and climbs the ladder.
    rig.at(NOW + 29 * 60_000);
    usage.poll("business").await.unwrap();
    let short = read(&attention, NOW + BACKOFF_ATTENTION_AFTER_MS - 1);
    assert!(
        !find(&short, Kind::PollBackoff, "business").unwrap().holds,
        "short of thirty minutes nothing holds"
    );
    let conditions = read(&attention, NOW + BACKOFF_ATTENTION_AFTER_MS);
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
    // A threshold of 0 applies the switch at the rule's next request, whatever it has counted so far.
    up.set_rule_after(0, "/api/oauth/usage", usage_answer(200));
    rig.at(NOW + 29 * 60_000 + 240_000);
    usage.poll("business").await.unwrap();
    assert_eq!(
        usage.poll_status("business").unwrap().last_outcome,
        PollOutcome::Clean
    );
    let conditions = read(&attention, NOW + 29 * 60_000 + 240_000);
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
        find(&read(&attention, NOW), Kind::PoolExhausted, "main")
            .unwrap()
            .holds
    );
    rig.store.delete_api_key("desktop", "test").unwrap();
    rig.store.delete_pool("main", "test").unwrap();
    rig.store.delete_account("direct", "test").unwrap();
    rig.router.reload().await.unwrap();
    let conditions = read(&attention, NOW);
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
    while !find(&read(&attention, NOW), Kind::PoolExhausted, "main")
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

/// One scrape of the metrics listener over the reader's gauge source: its status and its text.
async fn scrape(rig: &Rig, attention: &Arc<Attention>) -> (u16, String) {
    let app = phoebis_ledger::metrics_router(
        Arc::new(Metrics::new()),
        rig.router.clone(),
        Arc::new(Pricing::new(PriceTable::shipped())),
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
async fn the_scrape_reads_a_drift_version_1_after_its_refusal_and_0_after_a_relayed_request() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    attention.observe(&observed("d1", Some("desktop"), "main"));
    attention.observe(&complete(
        "d1",
        Outcome::Rejected,
        Some("hash"),
        Some("2.1.274"),
        NOW,
    ));
    let (status, text) = scrape(&rig, &attention).await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="client_drift",subject="2.1.274"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"phoebis_attention{kind="poll_backoff",subject="business"} 0"#),
        "a polled account with no poll status yet scrapes its backoff series at 0: {text}"
    );
    attention.observe(&observed("d2", Some("desktop"), "main"));
    attention.observe(&complete(
        "d2",
        Outcome::Succeeded,
        None,
        Some("2.1.274"),
        NOW,
    ));
    let (_, text) = scrape(&rig, &attention).await;
    assert!(
        text.contains(r#"phoebis_attention{kind="client_drift",subject="2.1.274"} 0"#),
        "the cleared version reads 0 under the same labels rather than vanishing: {text}"
    );
}

#[tokio::test]
async fn a_failed_holder_read_fails_the_conditions_and_the_scrape() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    rig.store.debug_fail_next_refresh_read();
    assert!(attention.conditions(NOW).is_err());
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
    let conditions = read(&attention, NOW);
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
    let conditions = read(&attention, NOW);
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
        find(&read(&attention, NOW), Kind::LedgerRecordParked, "-").is_none(),
        "no record without a pool, no series for one"
    );
    park(&rig, None, "direct", NOW - 1_000);
    let conditions = read(&attention, NOW);
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
        find(&read(&attention, NOW), Kind::LedgerRecordParked, "-").is_none(),
        "the series leaves with the rows"
    );
}

#[tokio::test]
async fn a_parked_record_keeps_its_condition_after_its_pool_is_deleted() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    // An id the registry does not hold, as the pool of a record parked before the pool was deleted.
    park(&rig, Some("gone"), "business", NOW - 5_000);
    let conditions = read(&attention, NOW);
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
        find(&read(&attention, NOW), Kind::LedgerRecordParked, "gone").is_none(),
        "the deleted pool's series leaves with its rows"
    );
}

#[tokio::test]
async fn a_failed_parked_read_fails_the_attention_read_as_the_refresh_read_does() {
    let rig = rig().await;
    let (attention, _arm) = rig.attention(None);
    ledger_sql(&rig, "DROP TABLE parked_records;");
    assert!(
        matches!(attention.conditions(NOW), Err(AttentionError::Ledger(_))),
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
        find(&read(&attention, NOW), Kind::LedgerRecordParked, "main").is_some_and(|c| !c.holds),
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
            account_kind: "oauth".into(),
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
    let first = read(&attention, NOW + 1_000);
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
    let second = read(&attention, NOW + 5_000);
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
    let reading = std::thread::spawn({
        let attention = attention.clone();
        move || attention.conditions(NOW).map(|read| read.len())
    });
    // Long enough for the read to reach the registry's connection and wait on it.
    std::thread::sleep(Duration::from_millis(200));
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
        std::thread::sleep(Duration::from_millis(5));
    };
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(free, "observe waited on a read held by the registry");
    assert!(!reading.is_finished() || reading.join().unwrap().is_ok());
}
