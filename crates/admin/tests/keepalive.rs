//! The keepalive policy on a pool: typed on the way in, whole on the way out, and gated by affinity; the
//! sessions, recipes, and overview views the admin API renders from the engine.
mod common;
use common::{get, json, login, patch, post};

#[tokio::test]
async fn a_pool_policy_is_typed_gated_by_affinity_and_rendered_whole() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority_random"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let body = json(created).await;
    assert_eq!(body["keepalive"]["enabled"], false);
    assert_eq!(
        (
            body["keepalive"]["lead_seconds_5m"].as_u64(),
            body["keepalive"]["retry_max_seconds"].as_u64(),
            body["keepalive"]["probe_cost_cap_usd_per_day"].is_null()
        ),
        (Some(60), Some(300), true),
        "a pool created without a policy renders the defaults"
    );
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"enabled":true,"probe_enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("keepalive is not a policy object"));
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"cold_probe_limit":0}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("cold_probe_limit must be between 1 and 20"));
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"keepalive":"on"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"affinity_ttl_seconds":120,"keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refused.status(),
        400,
        "enabling with affinity below 300 s in the same request"
    );
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("affinity_ttl_seconds of at least 300"));
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":120}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"keepalive":{"enabled":true}}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "enabling on a pool whose affinity is already short"
    );
    let enabled = patch(&app, "/admin/pools/main", r#"{"affinity_ttl_seconds":600,"keepalive":{"enabled":true,"lead_seconds_5m":90,"probe_cost_cap_usd_per_day":2.5}}"#, Some(&cookie)).await;
    assert_eq!(enabled.status(), 200);
    let body = json(enabled).await;
    assert_eq!(
        (
            body["keepalive"]["enabled"].as_bool(),
            body["keepalive"]["lead_seconds_5m"].as_u64(),
            body["keepalive"]["probe_cost_cap_usd_per_day"].as_f64(),
            body["keepalive"]["jitter_seconds_1h"].as_u64()
        ),
        (Some(true), Some(90), Some(2.5), Some(30))
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":299}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "lowering affinity under an enabled policy"
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":0}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "turning affinity off under an enabled policy"
    );
    assert_eq!(
        patch(
            &app,
            "/admin/pools/main",
            r#"{"affinity_ttl_seconds":0,"keepalive":{"enabled":false}}"#,
            Some(&cookie)
        )
        .await
        .status(),
        200,
        "both in one request is consistent"
    );
    let listed = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let main = listed["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main")
        .unwrap();
    assert_eq!(
        (
            main["keepalive"]["enabled"].as_bool(),
            main["keepalive"]["lead_seconds_5m"].as_u64()
        ),
        (Some(false), Some(90)),
        "the tuning survives the toggle"
    );
    let with_policy = post(
        &app,
        "/admin/pools",
        r#"{"id":"warm","label":"Warm","strategy":"priority_random","keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        with_policy.status(),
        201,
        "the default affinity of 3,600 admits an enabled policy at creation"
    );
}

use phoebis_keepalive::Event;

#[tokio::test]
async fn the_sessions_view_carries_the_companion_and_the_recipes_and_release_discards_them() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    // A registration alone lists the session.
    parts
        .keepalive
        .apply_event(
            "desktop",
            Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .expect("a registered session is listed");
    assert_eq!(
        (
            row["companion"].as_str(),
            row["agents"].as_u64(),
            row["pinned"].as_bool(),
            row["requests"].as_u64(),
            row["keepalive"]["state"].is_null()
        ),
        (Some("reporting"), Some(1), Some(false), Some(0), true)
    );
    assert!(row["companion_since"].is_null() && row["account_id"].is_null());
    // A retained recipe shows the main agent's companion state and the next probe.
    let now = common::NOW;
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r1".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, received_at: now }));
    parts
        .keepalive
        .observe(phoebis_bus::Event::AccountSelected(phoebis_bus::AccountSelected {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(1),
            started_at: now,
        }));
    parts
        .keepalive
        .observe(phoebis_bus::Event::RequestComplete(phoebis_bus::RequestComplete {
            request_id: "r1".into(),
            attempt: 1,
            account_id: Some("direct".into()),
            outcome: phoebis_bus::Outcome::Succeeded,
            status: 200,
            usage: Some(phoebis_bus::Usage {
                cache_write_5m_tokens: Some(2_000),
                ..Default::default()
            }),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: None,
            client_drift: None,
            client_version: None,
            response_bytes: None,
            finished_at: now,
        }));
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap();
    assert_eq!(
        row["keepalive"]["state"].as_str(),
        Some("busy"),
        "the main agent holds a recipe and is busy"
    );
    let recipes = json(get(&parts.app, "/admin/keepalive/recipes", Some(&cookie)).await).await;
    let recipe = &recipes["recipes"][0];
    assert_eq!(
        recipe
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [
            "account_id",
            "agent_id",
            "key_id",
            "last_request_at",
            "model",
            "next_probe_at",
            "pool_id",
            "reason",
            "session_id",
            "state",
            "ttl_seconds"
        ]
    );
    assert_eq!(
        (
            recipe["agent_id"].as_str(),
            recipe["state"].as_str(),
            recipe["reason"].is_null(),
            recipe["last_request_at"].as_i64(),
            recipe["ttl_seconds"].as_i64()
        ),
        (Some("main"), Some("armed"), true, Some(now), Some(300))
    );
    assert_eq!(
        row["keepalive"]["next_probe_at"].as_i64(),
        recipe["next_probe_at"].as_i64()
    );
    // Silent after the lease lapses: the recipe stays and the row says since when.
    parts
        .clock_cell
        .store(now + 91_000, std::sync::atomic::Ordering::SeqCst);
    parts.keepalive.expire();
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap();
    assert_eq!(
        (row["companion"].as_str(), row["companion_since"].as_i64()),
        (Some("silent"), Some(now))
    );
    // The overview counts what the view shows.
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        overview
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [
            "agents",
            "cost_today",
            "expired_between_turns_24h",
            "net_saved_7d_usd",
            "probes_24h",
            "sessions",
            "sessions_with_companion"
        ]
    );
    assert_eq!(
        (
            overview["agents"]["busy"].as_u64(),
            overview["sessions"].as_u64(),
            overview["sessions_with_companion"].as_u64(),
            overview["cost_today"][0]["pool_id"].as_str(),
            overview["cost_today"][0]["cap_usd"].is_null()
        ),
        (Some(1), Some(1), Some(1), Some("main"), true)
    );
    // Release discards the recipes; with no pin the router releases nothing, and the route still answers 204.
    let released = post(
        &parts.app,
        "/admin/sessions/desktop/s1/release",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(released.status(), 204);
    assert!(
        json(get(&parts.app, "/admin/keepalive/recipes", Some(&cookie)).await).await["recipes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let viewer = login(&parts.app, "viewer-secret").await;
    assert_eq!(
        get(&parts.app, "/admin/keepalive/recipes", Some(&viewer))
            .await
            .status(),
        200
    );
    assert_eq!(
        get(&parts.app, "/admin/keepalive/overview", Some(&viewer))
            .await
            .status(),
        200
    );
    assert_eq!(
        post(
            &parts.app,
            "/admin/sessions/desktop/s1/release",
            "",
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
    let settings = json(get(&parts.app, "/admin/settings", Some(&viewer)).await).await;
    assert_eq!(
        (
            settings["keepalive"]["lease_seconds"].as_u64(),
            settings["keepalive"]["oauth_prewarm_verified"].as_bool()
        ),
        (Some(90), Some(false))
    );
    let system = json(get(&parts.app, "/admin/system", Some(&viewer)).await).await;
    assert_eq!(
        (
            system["keepalive"]["sessions"].as_u64(),
            system["keepalive"]["running_probes"].as_u64()
        ),
        (Some(1), Some(0))
    );
}

#[test]
fn every_engine_reason_maps_onto_the_view_vocabulary() {
    use phoebis_keepalive::{view_reason, Reason};
    let unheld = [
        Reason::AwaitingRequest,
        Reason::AwaitingSelection,
        Reason::Probing,
        Reason::Scheduled,
    ];
    for reason in Reason::ALL {
        let mapped = view_reason(*reason);
        assert_eq!(
            mapped.is_none(),
            unheld.contains(reason),
            "{reason:?} -> {mapped:?}"
        );
    }
    assert_eq!(
        serde_json::to_string(&view_reason(Reason::RepeatedColdProbe)).unwrap(),
        r#""cold_probes""#
    );
    assert_eq!(
        serde_json::to_string(&view_reason(Reason::ProbeQuotaExhausted)).unwrap(),
        r#""quota_exhausted""#
    );
}

/// A `session_end` leaves the session listed, as the tombstone the lifecycle protocol needs, but nothing is
/// reporting for it any more: the view says so, and the overview leaves it out of the companion count.
#[tokio::test]
async fn an_ended_session_is_listed_without_a_companion() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let start = |sequence, event: &str| Event {
        event: event.into(),
        session_id: "s1".into(),
        instance_id: "i1".into(),
        sequence,
        ..Default::default()
    };
    parts
        .keepalive
        .apply_event("desktop", start(1, "session_start"))
        .unwrap();
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        (
            overview["sessions"].as_u64(),
            overview["sessions_with_companion"].as_u64()
        ),
        (Some(1), Some(1)),
        "the registered session is reporting"
    );
    parts
        .keepalive
        .apply_event("desktop", start(2, "session_end"))
        .unwrap();
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .expect("an ended session is still listed");
    assert_eq!(
        (row["companion"].as_str(), row["companion_since"].as_i64()),
        (Some("none"), Some(common::NOW)),
        "the end time is when the companion stopped reporting"
    );
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        (
            overview["sessions"].as_u64(),
            overview["sessions_with_companion"].as_u64()
        ),
        (Some(1), Some(0))
    );
}
