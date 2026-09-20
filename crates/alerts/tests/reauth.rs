//! Acceptance: a forced reauth_required opens one event, posts one signed webhook, and posts a resolve on
//! re-pairing; a failing endpoint records three attempts.
mod common;
use std::sync::Arc;
use std::time::Duration;

use phoebis_alerts::{signature, spawn_inputs, Kind, ReqwestWebhook, ScopeType};
use phoebis_store::{AccountState, Secret};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};

#[tokio::test]
async fn a_forced_reauth_posts_one_signed_webhook_and_a_resolve_and_a_dead_endpoint_gets_three_attempts(
) {
    let hook = FakeUpstream::start_plain(vec![Rule {
        path_prefix: "/hook".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"ok".to_vec(),
        },
    }])
    .await;
    let rig = rig_with(Arc::new(ReqwestWebhook::new(true))).await;
    let live = phoebis_store::AlertRuleRow {
        webhook_url: format!("http://127.0.0.1:{}/hook", hook.addr().port()),
        ..rule("live", Kind::ReauthRequired, ScopeType::AnyAccount, None)
    };
    rig.store
        .upsert_alert_rule(&live, Some(Some(&Secret::new("s3cret"))), "test")
        .unwrap();
    let dead = phoebis_store::AlertRuleRow {
        webhook_url: "http://127.0.0.1:9/hook".into(),
        ..rule("dead", Kind::ReauthRequired, ScopeType::AnyAccount, None)
    };
    rig.store.upsert_alert_rule(&dead, None, "test").unwrap();
    rig.alerts.reload().unwrap();
    let handles = spawn_inputs(
        rig.alerts.clone(),
        common::inputs(&rig, Default::default()).with_tick(Duration::from_millis(50)),
    );
    rig.store
        .set_state("business", AccountState::ReauthRequired)
        .unwrap();
    let opened = fake_upstream::wait_for(Duration::from_secs(5), || {
        hook.requests()
            .into_iter()
            .find(|r| r.body.windows(8).any(|w| w == b"\"opened\""))
    })
    .await;
    let body: serde_json::Value = serde_json::from_slice(&opened.body).unwrap();
    assert_eq!(
        (
            body["kind"].as_str(),
            body["subject"].as_str(),
            body["state"].as_str(),
            body["rule"].as_str()
        ),
        (
            Some("reauth_required"),
            Some("business"),
            Some("opened"),
            Some("live")
        )
    );
    let signed = opened
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("x-router-signature"))
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(signed, signature(&Secret::new("s3cret"), &opened.body));
    assert!(opened
        .headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("content-type") && v == "application/json"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        hook.requests().len(),
        1,
        "one event, one post, however many ticks passed"
    );
    rig.store
        .set_state("business", AccountState::Ready)
        .unwrap();
    let resolved = fake_upstream::wait_for(Duration::from_secs(5), || {
        hook.requests()
            .into_iter()
            .find(|r| r.body.windows(10).any(|w| w == b"\"resolved\""))
    })
    .await;
    let body: serde_json::Value = serde_json::from_slice(&resolved.body).unwrap();
    assert!(body["resolved_at"].is_i64() && body["opened_at"].is_i64());
    let dead_event = fake_upstream::wait_for(Duration::from_secs(5), || {
        rig.store
            .alert_events(Some("resolved"), 10)
            .unwrap()
            .into_iter()
            .find(|e| e.rule_id == "dead")
    })
    .await;
    let rows = fake_upstream::wait_for(Duration::from_secs(5), || {
        let r = rig.store.alert_deliveries(&dead_event.id).unwrap();
        (r.iter().filter(|d| d.state == "opened").count() == 3).then_some(r)
    })
    .await;
    assert!(
        rows.iter()
            .filter(|d| d.state == "opened")
            .all(|d| d.status.is_none() && d.error.is_some()),
        "three failed attempts against a closed port: {rows:?}"
    );
    handles.shutdown().await;
    hook.shutdown().await;
}
