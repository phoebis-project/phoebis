//! The cache lane end to end: the relay composes it from the body and the beta line, the selection stamps it on
//! the pin, and the affinity report carries it onto the entries the answer writes.
mod common;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_routing::lane_key;
use phoebis_store::{PinRow, StorageKey, Store};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const SPELLING: &str = "claude-sonnet-4-5";

/// A listing that names the fixture's model undated, so the request's canonical model stays the spelling the
/// fixture sends and the lanes below read it.
fn undated_listing() -> Vec<Rule> {
    vec![Rule {
        path_prefix: "/v1/models?limit=1000".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: serde_json::json!({
                "data": [{
                    "id": SPELLING,
                    "display_name": "Claude Sonnet 4.5",
                    "created_at": "2025-09-29",
                    "max_input_tokens": 1_000_000,
                    "max_tokens": 128_000,
                }],
            })
            .to_string()
            .into_bytes(),
        },
    }]
}

fn turn(body: &[u8]) -> Request<Body> {
    let mut req = request_from_fixture(inference_head(FixtureKind::OauthNonFirstParty), body, KEY);
    set_header(&mut req, "x-claude-code-session-id", "s1");
    req
}

/// The session's pin once the selection at `now` has reached the store.
async fn pin_at(store: &Store, now: i64) -> PinRow {
    wait_for(Duration::from_secs(2), || {
        store
            .load_routing()
            .unwrap()
            .pins
            .into_iter()
            .find(|p| p.session_id == "s1" && p.last_used_at == now)
    })
    .await
}

#[tokio::test]
async fn a_speed_switch_reads_the_pin_cold_and_stamps_the_lane_on_the_pin_and_the_entries() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let catalog_up = FakeUpstream::start(undated_listing()).await;
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let h = harness_with_store_and_catalog(
        store.clone(),
        &up,
        catalog_up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        CATALOG_WINDOW,
    )
    .await;
    // Every profile body carries `thinking` with a budget and no `output_config`.
    let standard = lane_key(SPELLING, "standard", "enabled:31999", "-");
    let fast = lane_key(SPELLING, "fast", "enabled:31999", "-");

    let first = body(BodyFixture::Oauth);
    let resp = h.app.clone().oneshot(turn(first)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let pin = pin_at(&store, NOW).await;
    assert_eq!(
        (pin.lane.as_str(), pin.last_refresh_at),
        (standard.as_str(), NOW),
        "a fresh pin takes the request's lane, warm"
    );
    let first_rows: HashSet<String> = wait_for(Duration::from_secs(2), || {
        let rows = store.load_routing().unwrap().affinity;
        (!rows.is_empty()).then(|| rows.into_iter().map(|r| r.hash).collect())
    })
    .await;

    let mut next: serde_json::Value = serde_json::from_slice(first).unwrap();
    next["speed"] = "fast".into();
    next["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"role": "user", "content": "again"}));
    let next = serde_json::to_vec(&next).unwrap();
    let second_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    let resp = h.app.clone().oneshot(turn(&next)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let pin = pin_at(&store, second_at).await;
    assert_eq!(
        (pin.lane.as_str(), pin.last_refresh_at),
        (fast.as_str(), 0),
        "the same conversation on another speed is another cache"
    );
    let second_rows = wait_for(Duration::from_secs(2), || {
        let rows: Vec<_> = store
            .load_routing()
            .unwrap()
            .affinity
            .into_iter()
            .filter(|r| !first_rows.contains(&r.hash))
            .collect();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert!(
        second_rows.iter().all(|r| r.lane == fast),
        "the entries the fast answer wrote carry its lane: {second_rows:?}"
    );

    let third = with_control_entry(&next, "low");
    let per_turn = |body: &[u8]| {
        let mut req = turn(body);
        let betas = req.headers()["anthropic-beta"]
            .to_str()
            .unwrap()
            .to_string();
        set_header(
            &mut req,
            "anthropic-beta",
            &format!("{betas},per-turn-control-2026-07-01"),
        );
        req
    };
    let third_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    let resp = h.app.clone().oneshot(per_turn(&third)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        pin_at(&store, third_at).await.lane,
        lane_key(SPELLING, "fast", "enabled:31999", "*"),
        "under the per-turn beta the effort leaves the lane"
    );

    let mut fourth: serde_json::Value = serde_json::from_slice(&third).unwrap();
    let messages = fourth["messages"].as_array_mut().unwrap();
    messages.push(serde_json::json!({"role": "assistant", "content": "done"}));
    messages.push(serde_json::json!({"role": "user", "content": "and now"}));
    let fourth = with_control_entry(&serde_json::to_vec(&fourth).unwrap(), "high");
    let fourth_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    let resp = h.app.clone().oneshot(per_turn(&fourth)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        pin_at(&store, fourth_at).await.last_refresh_at,
        fourth_at,
        "another per-turn level under the beta keeps the session warm"
    );
}
