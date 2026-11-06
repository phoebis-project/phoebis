//! The cache lane end to end: the relay composes it from the body and the beta line, the selection stamps it on
//! the pin, and the affinity report carries it onto the entries the answer writes.
mod common;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_routing::lane_key;
use phoebis_store::{PinRow, StorageKey, Store};
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

/// The session's pin once the selection at `now` has reached the router.
async fn pin_at(h: &Harness, now: i64) -> PinRow {
    wait_for_async(Duration::from_secs(2), || async move {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.session_id == "s1" && p.last_used_at == now)
    })
    .await
}

/// Every boundary of `body`'s two chains, the hashes the router keys an answer's affinity entries on.
fn boundaries_of(body: &[u8]) -> Vec<String> {
    let chains = phoebis_relay::Chains::read(&serde_json::from_slice(body).unwrap())
        .expect("the body is an object");
    chains
        .static_boundaries
        .iter()
        .chain(chains.message_boundaries.iter())
        .cloned()
        .collect()
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
    )
    .await;
    // Every profile body carries `thinking` with a budget and no `output_config`.
    let standard = lane_key(SPELLING, "standard", "enabled:31999", "-");
    let fast = lane_key(SPELLING, "fast", "enabled:31999", "-");

    let first = body(BodyFixture::Oauth);
    let resp = h.app.clone().oneshot(turn(first)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let pin = pin_at(&h, NOW).await;
    assert_eq!(
        (pin.lane.as_str(), pin.last_refresh_at),
        (standard.as_str(), NOW),
        "a fresh pin takes the request's lane, warm"
    );
    let first_rows: HashSet<String> = boundaries_of(first).into_iter().collect();
    wait_for(Duration::from_secs(2), || {
        let map = h.router.affinity();
        let entries = map.read().unwrap();
        first_rows
            .iter()
            .any(|hash| entries.entry(SPELLING, hash).is_some())
            .then_some(())
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
    let pin = pin_at(&h, second_at).await;
    assert_eq!(
        (pin.lane.as_str(), pin.last_refresh_at),
        (fast.as_str(), 0),
        "the same conversation on another speed is another cache"
    );
    let second_rows = wait_for(Duration::from_secs(2), || {
        let map = h.router.affinity();
        let entries = map.read().unwrap();
        let lanes: Vec<(String, String)> = boundaries_of(&next)
            .into_iter()
            .filter(|hash| !first_rows.contains(hash))
            .filter_map(|hash| {
                let lane = entries.entry(SPELLING, &hash)?.lane.clone();
                Some((hash, lane))
            })
            .collect();
        (!lanes.is_empty()).then_some(lanes)
    })
    .await;
    assert!(
        second_rows.iter().all(|(_, lane)| *lane == fast),
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
        pin_at(&h, third_at).await.lane,
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
        pin_at(&h, fourth_at).await.last_refresh_at,
        fourth_at,
        "another per-turn level under the beta keeps the session warm"
    );
}

/// Reads the answer to its end, so the stream's renewal report carries the turn's own instant before the case moves
/// the clock to the next one.
async fn answered(resp: axum::response::Response) {
    assert_eq!(resp.status(), 200);
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_pin_carries_the_effort_its_turn_named_by_the_newest_control_entry_or_the_top_level_member(
) {
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
    )
    .await;
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
    let continued = |body: &[u8], text: &str| {
        let mut value: serde_json::Value = serde_json::from_slice(body).unwrap();
        let messages = value["messages"].as_array_mut().unwrap();
        messages.push(serde_json::json!({"role": "assistant", "content": "done"}));
        messages.push(serde_json::json!({"role": "user", "content": text}));
        serde_json::to_vec(&value).unwrap()
    };

    // The profile body carries no `output_config` and no control entry.
    let first = body(BodyFixture::Oauth).to_vec();
    answered(h.app.clone().oneshot(turn(&first)).await.unwrap()).await;
    assert_eq!(
        pin_at(&h, NOW).await.effort,
        None,
        "a turn naming no effort stamps none"
    );

    let low = with_control_entry(&continued(&first, "next"), "low");
    let low_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    answered(h.app.clone().oneshot(per_turn(&low)).await.unwrap()).await;
    assert_eq!(
        pin_at(&h, low_at).await.effort.as_deref(),
        Some("low"),
        "a control entry names the turn's effort"
    );

    let high = with_control_entry(&continued(&low, "and now"), "high");
    let high_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    answered(h.app.clone().oneshot(per_turn(&high)).await.unwrap()).await;
    assert_eq!(
        pin_at(&h, high_at).await.effort.as_deref(),
        Some("high"),
        "the newest control entry wins over the one before it"
    );

    let mut top_level: serde_json::Value =
        serde_json::from_slice(&continued(&first, "plainly")).unwrap();
    top_level["output_config"] = serde_json::json!({"effort": "medium"});
    let top_level = serde_json::to_vec(&top_level).unwrap();
    let top_level_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    answered(h.app.clone().oneshot(turn(&top_level)).await.unwrap()).await;
    assert_eq!(
        pin_at(&h, top_level_at).await.effort.as_deref(),
        Some("medium"),
        "with no control entry the top-level member names the effort"
    );

    let mut empty: serde_json::Value =
        serde_json::from_slice(&continued(&top_level, "and quietly")).unwrap();
    empty["output_config"] = serde_json::json!({"effort": ""});
    let empty = serde_json::to_vec(&empty).unwrap();
    let empty_at = h.clock.fetch_add(1_000, Ordering::SeqCst) + 1_000;
    answered(h.app.clone().oneshot(turn(&empty)).await.unwrap()).await;
    assert_eq!(
        pin_at(&h, empty_at).await.effort,
        None,
        "an empty top-level effort stamps none"
    );
}
