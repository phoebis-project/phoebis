//! The keepalive policy on a pool: typed on the way in, whole on the way out, and gated by affinity; the
//! sessions, recipes, and overview views the admin API renders from the engine.
mod common;
use common::{delete, get, json, login, patch, post};

#[tokio::test]
async fn a_pool_policy_is_typed_gated_by_affinity_and_rendered_whole() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
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
        r#"{"id":"warm","label":"Warm","strategy":"priority","keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        with_policy.status(),
        201,
        "the default affinity of 3,600 admits an enabled policy at creation"
    );
    let refused = post(
        &app,
        "/admin/pools",
        r#"{"id":"typo","label":"Typo","strategy":"priority","keepalive":{"warm_subagents":1}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .starts_with("keepalive is not a policy object"));
    let quiet = post(
        &app,
        "/admin/pools",
        r#"{"id":"quiet","label":"Quiet","strategy":"priority","keepalive":{"enabled":true,"warm_subagents":false}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(quiet.status(), 201);
    assert_eq!(json(quiet).await["keepalive"]["warm_subagents"], false);
    let untouched = post(
        &app,
        "/admin/pools",
        r#"{"id":"loud","label":"Loud","strategy":"priority","keepalive":{}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(untouched.status(), 201);
    assert_eq!(json(untouched).await["keepalive"]["warm_subagents"], true);
    let shared = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"probe_cap_window_share_per_day":0.1}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(shared.status(), 200);
    assert_eq!(
        json(shared).await["keepalive"]["probe_cap_window_share_per_day"].as_f64(),
        Some(0.1)
    );
    let refused_share = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"probe_cap_window_share_per_day":1.5}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused_share.status(), 400);
    assert!(json(refused_share).await["error"]
        .as_str()
        .unwrap()
        .contains("probe_cap_window_share_per_day must be null or between 0 and 1"));
}

use phoebis_keepalive::Event;

#[tokio::test]
async fn the_sessions_view_carries_the_companion_and_the_recipes_and_release_discards_them() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
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
            common::generation(&parts.store, "desktop"),
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
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r1".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: common::generation(&parts.store, "desktop"), pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), presented_retired: false, rotation_at: None, received_at: now }));
    parts
        .keepalive
        .observe(phoebis_bus::Event::AccountSelected(phoebis_bus::AccountSelected {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
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
            response: phoebis_bus::ResponseFacts::default(),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: None,
            client_drift: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
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
            "net_meter_tokens",
            "net_usd",
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
            recipe["ttl_seconds"].as_i64(),
            recipe["net_usd"].as_f64(),
            recipe["net_meter_tokens"].as_f64()
        ),
        (
            Some("main"),
            Some("armed"),
            true,
            Some(now),
            Some(300),
            Some(0.0),
            Some(0.0)
        ),
        "no probe has landed yet, so both balances still read zero"
    );
    assert_eq!(
        row["keepalive"]["next_probe_at"].as_i64(),
        recipe["next_probe_at"].as_i64()
    );
    assert_eq!(
        (
            row["keepalive"]["net_usd"].as_f64(),
            row["keepalive"]["net_meter_tokens"].as_f64()
        ),
        (
            recipe["net_usd"].as_f64(),
            recipe["net_meter_tokens"].as_f64()
        ),
        "the session's keepalive summary reads the same main-agent balances the recipe view does"
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

#[tokio::test]
async fn a_session_row_groups_by_key_generation_and_a_historical_row_is_read_only() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let g = common::generation(&parts.store, "desktop");
    let now = common::NOW;
    // A holder the ledger alone still remembers, one generation behind the live one, and the live holder's
    // own row, under the same session id: a key id created again must not merge the two.
    for (id, generation) in [("r1", g), ("r2", g - 1)] {
        parts.ledger.record(phoebis_ledger::Record::Request {
            request: phoebis_ledger::RequestRecord {
                id: id.into(),
                received_at: now,
                key_id: Some("desktop".into()),
                key_generation: generation,
                session_id: Some("s1".into()),
                model: "claude-sonnet-5".into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                ..Default::default()
            },
            attempts: vec![],
            bodies: None,
        });
    }
    parts
        .keepalive
        .apply_event(
            "desktop",
            g,
            Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r3".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: g, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), presented_retired: false, rotation_at: None, received_at: now }));
    parts
        .keepalive
        .observe(phoebis_bus::Event::AccountSelected(phoebis_bus::AccountSelected {
            request_id: "r3".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
        }));
    parts
        .keepalive
        .observe(phoebis_bus::Event::RequestComplete(phoebis_bus::RequestComplete {
            request_id: "r3".into(),
            attempt: 1,
            account_id: Some("direct".into()),
            outcome: phoebis_bus::Outcome::Succeeded,
            status: 200,
            usage: Some(phoebis_bus::Usage {
                cache_write_5m_tokens: Some(2_000),
                ..Default::default()
            }),
            response: phoebis_bus::ResponseFacts::default(),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: None,
            client_drift: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            finished_at: now,
        }));
    let listed = common::wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["session_id"] == "s1")
            .count()
            == 2
    })
    .await;
    let rows: Vec<_> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["session_id"] == "s1")
        .collect();
    let live = rows
        .iter()
        .find(|r| r["key_generation"] == g)
        .expect("the live generation is listed");
    assert_eq!(
        (
            live["historical"].as_bool(),
            live["companion"].as_str(),
            live["agents"].as_u64()
        ),
        (Some(false), Some("reporting"), Some(1))
    );
    assert!(
        live["keepalive"]["state"].is_string(),
        "the live generation carries a recipe before the release, so the release below can be seen to discard it"
    );
    let historical = rows
        .iter()
        .find(|r| r["key_generation"] == g - 1)
        .expect("the historical generation is listed");
    assert_eq!(
        (
            historical["historical"].as_bool(),
            historical["companion"].as_str(),
            historical["agents"].as_u64(),
            historical["keepalive"]["state"].is_null()
        ),
        (Some(true), Some("none"), Some(0), true)
    );
    let released = post(
        &parts.app,
        "/admin/sessions/desktop/s1/release",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(released.status(), 204);
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let rows: Vec<_> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["session_id"] == "s1")
        .collect();
    let live = rows.iter().find(|r| r["key_generation"] == g).unwrap();
    assert!(
        live["keepalive"]["state"].is_null(),
        "the release discarded the live generation's recipe"
    );
    let historical = rows.iter().find(|r| r["key_generation"] == g - 1).unwrap();
    assert_eq!(
        (
            historical["historical"].as_bool(),
            historical["companion"].as_str(),
            historical["agents"].as_u64()
        ),
        (Some(true), Some("none"), Some(0)),
        "a release reaches the live generation alone; the historical row is untouched"
    );
}

/// A deleted key's session outlives the key in the engine until the idle sweep clears it: the row must read as
/// historical, with no companion, agents, or recipe, and the console must not be able to release it by naming
/// it directly.
#[tokio::test]
async fn a_deleted_keys_session_is_historical_with_no_companion_and_its_release_is_refused() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let g = common::generation(&parts.store, "desktop");
    parts
        .keepalive
        .apply_event(
            "desktop",
            g,
            Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let listed = common::wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["session_id"] == "s1")
    })
    .await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .expect("the registered session is listed while the key is live");
    assert_eq!(
        (row["historical"].as_bool(), row["companion"].as_str()),
        (Some(false), Some("reporting")),
        "live before the key is deleted"
    );
    assert_eq!(
        delete(&parts.app, "/admin/keys/desktop", Some(&cookie))
            .await
            .status(),
        204
    );
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .expect(
            "the ledger and the engine both still remember the session the deleted key left behind",
        );
    assert_eq!(
        (
            row["historical"].as_bool(),
            row["companion"].as_str(),
            row["agents"].as_u64(),
            row["keepalive"]["state"].is_null()
        ),
        (Some(true), Some("none"), Some(0), true),
        "the deleted key's session outlives it in the engine, but the row reads as historical and read-only"
    );
    assert_eq!(
        post(
            &parts.app,
            "/admin/sessions/desktop/s1/release",
            "",
            Some(&cookie)
        )
        .await
        .status(),
        404,
        "the console cannot release a historical row by naming it directly"
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
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
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
        .apply_event(
            "desktop",
            common::generation(&parts.store, "desktop"),
            start(1, "session_start"),
        )
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
        .apply_event(
            "desktop",
            common::generation(&parts.store, "desktop"),
            start(2, "session_end"),
        )
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

/// A probe is the router's own traffic: it never presents the key, so it must not move `last_used_at`, which
/// only a request the key itself authenticated should touch.
#[tokio::test]
async fn a_probe_leaves_the_keys_last_used_at_alone() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    parts.router.reload().await.unwrap();
    let created = json(
        post(
            &parts.app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    let secret = created["secret"].as_str().unwrap().to_string();
    let generation = common::generation(&parts.store, "desktop");
    parts
        .keepalive
        .apply_event(
            "desktop",
            generation,
            Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    phoebis_keepalive::spawn_observer(parts.keepalive.clone());
    parts.up.add_credential_rule(
        common::API_KEY,
        "/v1/messages",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0},"output_tokens":10}}"#.to_vec(),
        },
    );
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let proxy = phoebis_relay::router(parts.relay.clone());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &secret)
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "s1")
        .header("x-claude-code-agent-id", "main")
        .body(axum::body::Body::from(
            r#"{"model":"claude-sonnet-4-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();
    let resp = proxy.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        parts
            .store
            .api_key("desktop")
            .unwrap()
            .unwrap()
            .last_used_at
            .is_some()
            .then_some(())
    })
    .await;
    let touched = json(get(&parts.app, "/admin/keys", Some(&cookie)).await).await["keys"][0]
        ["last_used_at"]
        .as_i64();
    assert!(touched.is_some(), "the client request touched the key");
    let due = fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        parts
            .keepalive
            .agent_status("desktop", "s1", "main")
            .and_then(|s| s.next_probe_at)
    })
    .await;
    parts
        .clock_cell
        .store(due + 1_000, std::sync::atomic::Ordering::SeqCst);
    parts.keepalive.step().await;
    parts.keepalive.wait_idle().await;
    assert!(
        parts
            .keepalive
            .agent_status("desktop", "s1", "main")
            .unwrap()
            .probes
            >= 1,
        "the probe ran"
    );
    // The router task processes commands in order, so a round trip through it waits out anything a probe
    // queued before this point.
    parts.router.reload().await.unwrap();
    let after = json(get(&parts.app, "/admin/keys", Some(&cookie)).await).await["keys"][0]
        ["last_used_at"]
        .as_i64();
    assert_eq!(
        after, touched,
        "a probe must not move the key's last_used_at"
    );
}
