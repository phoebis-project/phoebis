//! The six conditions through the reader: each kind's hold and clear, the series at 0 for a subject in the clear, a
//! deleted subject's series gone, a refresh run read back by a second reader, and the gauge the metrics listener
//! scrapes from the same read.
mod common;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use http_body_util::BodyExt;
use phoebis_attention::{Attention, Condition, Kind, Link, BACKOFF_ATTENTION_AFTER_MS};
use phoebis_bus::Outcome;
use phoebis_ledger::{Metrics, PriceTable, Pricing};
use phoebis_oauth::PollOutcome;
use phoebis_store::{AccountState, AttemptOutcome};
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
            ("poll_backoff", "business".to_string(), false),
            ("pool_exhausted", "main".to_string(), false),
            ("reauth_required", "business".to_string(), false),
            ("refresh_failed", "business".to_string(), false),
        ],
        "an Anthropic key carries funding alone, an OAuth account its state, its refresh run and, before any poll \
         status exists, its backoff"
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
    let lost = rig.store.open_refresh_attempt("business", 1, 1, 1).unwrap();
    rig.store
        .resolve_refresh_attempt(lost, 1, AttemptOutcome::Uncertain("lost".into()))
        .unwrap();
    let attempts = rig
        .store
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap();
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
