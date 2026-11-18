//! `GET /admin/feature-calls` over rows the ledger wrote from the bus: the cursor's order and each filter.
mod common;

use axum::Router;
use common::{admin_parts_full, get, json, login, wait_json, NOW};
use phoebis_bus::{Event, FeatureCall, FeatureEnd, FeatureOutcome};
use serde_json::Value;

/// A call sent upstream under `account` and answered 200, the one end that leaves a row.
fn sent(
    id: &str,
    received_at: i64,
    key: &str,
    feature: &'static str,
    account: &str,
    outcome: FeatureOutcome,
) -> Event {
    Event::FeatureCall(FeatureCall {
        duration_ms: 40,
        end: FeatureEnd::Sent {
            account_id: account.into(),
            error_code: None,
            feature,
            key_id: key.into(),
            outcome,
            status: Some(200),
        },
        id: id.into(),
        method: "POST".into(),
        path: "/api/frame/artifacts".into(),
        received_at,
    })
}

fn ids(page: &Value) -> Vec<String> {
    page["feature_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect()
}

async fn listed(app: &Router, cookie: &str, query: &str) -> Vec<String> {
    let path = format!("/admin/feature-calls?{query}");
    ids(&json(get(app, &path, Some(cookie)).await).await)
}

/// The ledger lands a write a batch after the publish, so a paged or filtered read waits for every row first and a
/// write in progress never reads as a short page.
async fn wait_for_rows(app: &Router, cookie: &str, count: usize) {
    wait_json(app, "/admin/feature-calls", cookie, |page| {
        page["feature_calls"].as_array().unwrap().len() == count
    })
    .await;
}

#[tokio::test]
async fn the_cursor_neither_skips_nor_repeats() {
    let parts = admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for id in ["fc_a", "fc_b", "fc_c"] {
        parts
            .bus
            .publish_fact(sent(
                id,
                NOW,
                "k1",
                "artifacts",
                "a",
                FeatureOutcome::Forwarded,
            ))
            .await;
    }
    wait_for_rows(&parts.app, &cookie, 3).await;
    let first = json(get(&parts.app, "/admin/feature-calls?limit=2", Some(&cookie)).await).await;
    assert_eq!(ids(&first), ["fc_c", "fc_b"], "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("a row lies beyond the first page: {first}"));
    let path = format!("/admin/feature-calls?limit=2&cursor={cursor}");
    let second = json(get(&parts.app, &path, Some(&cookie)).await).await;
    assert_eq!(
        (ids(&second), &second["next_cursor"]),
        (vec!["fc_a".to_string()], &Value::Null),
        "{second}"
    );
}

#[tokio::test]
async fn a_filter_narrows_the_list() {
    let parts = admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let later = NOW + 1_000;
    parts
        .bus
        .publish_fact(sent(
            "fc_a",
            NOW,
            "k1",
            "connectors",
            "a",
            FeatureOutcome::Failed,
        ))
        .await;
    parts
        .bus
        .publish_fact(sent(
            "fc_b",
            later,
            "k2",
            "artifacts",
            "b",
            FeatureOutcome::Forwarded,
        ))
        .await;
    wait_for_rows(&parts.app, &cookie, 2).await;
    let app = &parts.app;
    assert_eq!(
        [
            listed(app, &cookie, "accounts=b").await,
            listed(app, &cookie, "features=connectors").await,
            listed(app, &cookie, "keys=k2").await,
            listed(app, &cookie, "outcomes=failed").await,
            listed(app, &cookie, &format!("from={later}")).await,
            listed(app, &cookie, &format!("to={NOW}")).await,
        ],
        [
            vec!["fc_b"],
            vec!["fc_a"],
            vec!["fc_b"],
            vec!["fc_a"],
            vec!["fc_b"],
            vec!["fc_a"],
        ],
        "accounts, features, keys, outcomes, from at the later row, to at the earlier row"
    );
}
