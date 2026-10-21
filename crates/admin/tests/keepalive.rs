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
    assert_eq!(
        json(created).await["keepalive"],
        serde_json::json!({
            "enabled": false,
            "max_idle_seconds": 3_600,
            "max_parked_seconds": null,
            "scheduled_horizon_seconds": null
        }),
        "a pool created without a policy renders the four fields with their defaults"
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
        r#"{"keepalive":{"lead_seconds_5m":90}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refused.status(),
        400,
        "a removed field is refused as any unknown field"
    );
    let error = json(refused).await["error"].as_str().unwrap().to_string();
    assert!(error.contains("unknown field `lead_seconds_5m`"), "{error}");
    let refused = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"max_parked_seconds":59}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .contains("max_parked_seconds must be between 60 and 604800, not 59"));
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
    let enabled = patch(&app, "/admin/pools/main", r#"{"affinity_ttl_seconds":600,"keepalive":{"enabled":true,"max_idle_seconds":900,"max_parked_seconds":7200}}"#, Some(&cookie)).await;
    assert_eq!(enabled.status(), 200);
    assert_eq!(
        json(enabled).await["keepalive"],
        serde_json::json!({
            "enabled": true,
            "max_idle_seconds": 900,
            "max_parked_seconds": 7_200,
            "scheduled_horizon_seconds": null
        }),
        "a null the PATCH left alone stays null on the way out"
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
            main["keepalive"]["max_idle_seconds"].as_u64(),
            main["keepalive"]["max_parked_seconds"].as_u64()
        ),
        (Some(false), Some(900), Some(7_200)),
        "the fields survive the toggle"
    );
    let cleared = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"max_parked_seconds":null}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(cleared.status(), 200);
    assert!(
        json(cleared).await["keepalive"]["max_parked_seconds"].is_null(),
        "a null sets the field back to the recipe's break-even"
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
        r#"{"id":"typo","label":"Typo","strategy":"priority","keepalive":{"max_idle_second":60}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(json(refused).await["error"]
        .as_str()
        .unwrap()
        .starts_with("keepalive is not a policy object"));
    let refused_horizon = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"scheduled_horizon_seconds":604801}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(refused_horizon.status(), 400);
    assert!(json(refused_horizon).await["error"]
        .as_str()
        .unwrap()
        .contains("scheduled_horizon_seconds must be between 60 and 604800, not 604801"));
}

#[tokio::test]
async fn a_stored_policy_naming_a_removed_field_reads_as_its_four_fields() {
    let (app, _up, store, router) = common::admin_parts().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let mut row = store.pool("main").unwrap().unwrap();
    row.keepalive_json = r#"{"enabled":true,"lead_seconds_5m":90}"#.into();
    store.update_pool(&row, row.updated_at, "test").unwrap();
    router.reload().await.unwrap();
    let listed = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let main = listed["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main")
        .unwrap();
    assert_eq!(
        main["keepalive"],
        serde_json::json!({
            "enabled": true,
            "max_idle_seconds": 3_600,
            "max_parked_seconds": null,
            "scheduled_horizon_seconds": null
        }),
        "the stored row stays on with the three defaults rather than reading as off"
    );
    let patched = patch(
        &app,
        "/admin/pools/main",
        r#"{"keepalive":{"max_idle_seconds":600}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        patched.status(),
        200,
        "a PATCH over the stored row merges onto its four fields"
    );
    assert_eq!(
        (
            json(patched).await["keepalive"]["enabled"].as_bool(),
            store
                .pool("main")
                .unwrap()
                .unwrap()
                .keepalive_json
                .contains("lead_seconds_5m")
        ),
        (Some(true), false),
        "the save writes the four fields alone"
    );
}

#[tokio::test]
async fn a_pool_answers_its_resolved_ceilings_and_a_pool_with_no_member_answers_null() {
    let (app, _up) = common::admin_app().await;
    let cookie = login(&app, "admin-secret").await;
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}],"cache_ttl":{"enabled":true},"model_rules":[{"pattern":"claude-opus-5-5"},{"pattern":"*","not":true}],"keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"empty","label":"Empty","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let listed = json(get(&app, "/admin/pools", Some(&cookie)).await).await;
    let pool = |id: &str| {
        listed["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()
            .clone()
    };
    let resolved = pool("main")["keepalive_resolved"].clone();
    let seconds = |member: &str| {
        (
            resolved[member]["min"].as_f64(),
            resolved[member]["max"].as_f64(),
        )
    };
    assert_eq!(
        [
            seconds("break_even_seconds"),
            seconds("idle_seconds"),
            seconds("parked_seconds"),
            seconds("horizon_seconds")
        ],
        [
            (Some(158_760.0), Some(158_760.0)),
            (Some(3_600.0), Some(3_600.0)),
            (Some(158_760.0), Some(158_760.0)),
            (Some(158_760.0), Some(158_760.0))
        ],
        "an OAuth member, a one-hour main TTL and one model: the shipped plan's break-even, the hour the policy \
         sets and the two null ceilings at the break-even"
    );
    let share = seconds("idle_share");
    assert!(
        share.0 == share.1 && (share.0.unwrap() - 0.0222).abs() < 5e-5,
        "{share:?}"
    );
    assert!(
        pool("empty")["keepalive_resolved"].is_null(),
        "a pool with no member has nothing to resolve over"
    );
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
            row["keepalive"]["state"].is_null(),
            row["keepalive"]["cause"].is_null()
        ),
        (Some("reporting"), Some(1), Some(false), Some(0), true, true)
    );
    assert!(
        row["companion_since"].is_null()
            && row["account_id"].is_null()
            && row["presence_age_seconds"].is_null()
    );
    assert!(
        json(get(&parts.app, "/admin/keepalive/recipes", Some(&cookie)).await).await["recipes"]
            .as_array()
            .unwrap()
            .is_empty(),
        "an agent that has sent no request has no row"
    );
    // A retained recipe shows the main agent's companion state and the next probe.
    let now = common::NOW;
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r1".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: common::generation(&parts.store, "desktop"), pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now }));
    parts.keepalive.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
        },
    ));
    parts.keepalive.observe(phoebis_bus::Event::RequestComplete(
        phoebis_bus::RequestComplete {
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
            client_contract: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            held: None,
            diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            passthrough: None,
            finished_at: now,
        },
    ));
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
            "agent_state",
            "cause",
            "credited_today",
            "deadline",
            "key_id",
            "last_guard",
            "last_probe_at",
            "last_request_at",
            "model",
            "next_probe_at",
            "park_kind",
            "pool_id",
            "probes_today",
            "session_id",
            "state",
            "ttl_seconds",
            "wake_at"
        ]
    );
    assert_eq!(
        (
            recipe["agent_id"].as_str(),
            recipe["state"].as_str(),
            recipe["cause"].as_str(),
            recipe["agent_state"].as_str(),
            recipe["last_request_at"].as_i64(),
            recipe["ttl_seconds"].as_i64(),
            recipe["last_guard"].as_str()
        ),
        (
            Some("main"),
            Some("retained"),
            Some("scheduled"),
            Some("busy"),
            Some(now),
            Some(300),
            Some("rearm")
        ),
        "the first request re-armed a fresh recipe"
    );
    assert_eq!(
        (
            recipe["account_id"].as_str(),
            recipe["park_kind"].is_null(),
            recipe["deadline"].is_null(),
            recipe["wake_at"].is_null(),
            recipe["last_probe_at"].is_null(),
            recipe["probes_today"].as_u64(),
            recipe["credited_today"].as_u64()
        ),
        (Some("direct"), true, true, true, true, Some(0), Some(0)),
        "a busy agent that has not been probed yet"
    );
    // An auxiliary request never re-arms, but the guard's own verdict still reaches the view.
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved {
        request_id: "r2".into(),
        origin: phoebis_bus::Origin::Client,
        kind: phoebis_bus::Kind::Inference,
        key_id: Some("desktop".into()),
        key_generation: common::generation(&parts.store, "desktop"),
        pool_id: Some("main".into()),
        session_id: Some("s1".into()),
        agent_id: Some("main".into()),
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: false,
        headers: vec![],
        body: bytes::Bytes::from(
            r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
        body_capture_hours: 0,
        shape: phoebis_bus::RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: phoebis_bus::ClientFacts {
            request_class: Some("title".into()),
            ..phoebis_bus::ClientFacts::default()
        },
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        guardrail_id: None,
        guardrails: Vec::new(),
        received_at: now,
    }));
    let recipes = json(get(&parts.app, "/admin/keepalive/recipes", Some(&cookie)).await).await;
    let recipe = &recipes["recipes"][0];
    assert_eq!(
        recipe["last_guard"].as_str(),
        Some("auxiliary"),
        "the title request left the recipe alone, and the view shows the guard's own note for it"
    );
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap();
    assert_eq!(
        row["keepalive"]["last_guard"].as_str(),
        Some("auxiliary"),
        "the session's keepalive summary reads the same guard note the recipe view does"
    );
    assert_eq!(
        row["keepalive"]["next_probe_at"].as_i64(),
        recipe["next_probe_at"].as_i64()
    );
    assert_eq!(
        row["keepalive"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["cause", "last_guard", "next_probe_at", "state"],
        "the session's keepalive summary carries no balance"
    );
    assert_eq!(row["keepalive"]["cause"].as_str(), Some("scheduled"));
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
            "bus_gaps",
            "expired_between_turns_24h",
            "limits",
            "pools",
            "probes_24h"
        ]
    );
    assert_eq!(
        (
            overview["agents"]["busy"].as_u64(),
            overview["bus_gaps"].as_u64(),
            overview["limits"]["sessions"]["count"].as_u64(),
            overview["limits"]["agents"]["count"].as_u64()
        ),
        (Some(1), Some(0), Some(1), Some(1))
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
    let recipes = json(get(&parts.app, "/admin/keepalive/recipes", Some(&cookie)).await).await;
    assert_eq!(
        recipes["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (
                r["agent_id"].as_str(),
                r["state"].as_str(),
                r["cause"].as_str()
            ))
            .collect::<Vec<_>>(),
        [(Some("main"), Some("ended"), Some("released"))],
        "the release discards the recipe and the agent's row stays with the cause"
    );
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap();
    assert!(
        row["keepalive"]["state"].is_null() && row["keepalive"]["cause"].is_null(),
        "the session's summary reads main's recipe, which the release discarded"
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (
            system["keepalive"]["sessions"].as_u64(),
            system["keepalive"]["running_probes"].as_u64()
        ),
        (Some(1), Some(0))
    );
}

/// The ten idle-age buckets as the overview answers them, `reached` raised in the buckets `reached` names.
fn idle_returns(reached: &[usize]) -> serde_json::Value {
    let bounds = phoebis_keepalive::IDLE_AGE_BOUNDS_SECONDS;
    serde_json::Value::Array(
        bounds
            .iter()
            .enumerate()
            .map(|(i, from)| {
                serde_json::json!({
                    "from_seconds": from,
                    "reached": u64::from(reached.contains(&i)),
                    "returned": 0,
                    "to_seconds": bounds.get(i + 1),
                })
            })
            .collect(),
    )
}

/// Every wasted reason word at zero but the ones `counts` names.
fn wasted_reasons(counts: &[(&str, u64)]) -> serde_json::Value {
    let mut reasons = serde_json::Map::new();
    for word in [
        "cold_return",
        "evicted",
        "expired",
        "horizon",
        "idle_limit",
        "parked_limit",
        "released",
        "replaced",
        "session_ended",
    ] {
        let count = counts
            .iter()
            .find(|(w, _)| *w == word)
            .map_or(0, |(_, n)| *n);
        reasons.insert(word.into(), count.into());
    }
    serde_json::Value::Object(reasons)
}

/// The overview answers the session count, the agent count and the retained bytes each beside its cap, and per pool the
/// last 24 hours' probes, credits and wasted probes joined by pool id with the idle-age counters: a pool one source
/// alone names lists with zeros for the other, and a row filed under no pool lists under its account, as the counters
/// key it.
#[tokio::test]
async fn the_overview_answers_the_counts_beside_their_caps_and_each_pools_day_and_idle_returns() {
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
    let now = common::NOW;
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r1".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: generation, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now }));
    parts.keepalive.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
        },
    ));
    parts.keepalive.observe(phoebis_bus::Event::RequestComplete(
        phoebis_bus::RequestComplete {
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
            client_contract: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            held: None,
            diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            passthrough: None,
            finished_at: now,
        },
    ));
    // Main goes idle, and the next tick opens its idle window in the first bucket.
    let request_generation = parts
        .keepalive
        .agent_status("desktop", "s1", "main")
        .unwrap()
        .request_generation;
    parts
        .keepalive
        .apply_event(
            "desktop",
            generation,
            Event {
                event: "state".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 2,
                agent_id: Some("main".into()),
                state: Some("idle".into()),
                request_generation: Some(request_generation),
                ..Default::default()
            },
        )
        .unwrap();
    parts.keepalive.step().await;
    let retained = parts.keepalive.stats().retained_bytes;
    assert!(retained > 0, "the request's recipe is retained");
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        overview["limits"],
        serde_json::json!({
            "agents": { "count": 1, "max": 1_024 },
            "retained_bytes": { "count": retained, "max": 268_435_456 },
            "sessions": { "count": 1, "max": 128 },
        }),
        "each count beside the cap the bootstrap file sets"
    );
    assert_eq!(
        overview["pools"],
        serde_json::json!([{
            "credited_24h": 0,
            "idle_returns": idle_returns(&[0]),
            "pool_id": "main",
            "probes_24h": 0,
            "wasted_24h": 0,
            "wasted_reasons_24h": wasted_reasons(&[]),
        }]),
        "a pool with idle-age counters and no ledger rows lists with zeros"
    );
    // Two probes in the day, one on a pool the engine holds no counters for, one a day ago, a credit, wasted probes
    // with their reasons, and one wasted probe filed under no pool.
    parts
        .db
        .write(|c| {
            let tx = c.transaction()?;
            for (id, pool, at) in [
                ("p1", "main", now),
                ("p2", "night", now),
                ("p0", "main", now - 25 * 3_600_000),
            ] {
                phoebis_ledger::fold_request(
                    &tx,
                    &phoebis_ledger::RequestRecord {
                        id: id.into(),
                        received_at: at,
                        key_id: Some("desktop".into()),
                        pool_id: Some(pool.into()),
                        canonical_model: "claude-sonnet-5".into(),
                        kind: "probe".into(),
                        outcome: "succeeded".into(),
                        status: Some(200),
                        ..Default::default()
                    },
                    &[],
                    None,
                    &phoebis_ledger::PriceRevisions::shipped(),
                )?;
            }
            let delta = |pool: &str, account: &str, kind: &str| phoebis_ledger::KeepaliveDelta {
                at: now,
                key_id: "desktop".into(),
                pool_id: pool.into(),
                account_id: account.into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: kind.into(),
                ..Default::default()
            };
            phoebis_ledger::fold_keepalive(
                &tx,
                &phoebis_ledger::KeepaliveDelta {
                    kept_warm_requests: 1,
                    kept_warm_5m_tokens: 2_000,
                    ..delta("main", "direct", "inference")
                },
            )?;
            phoebis_ledger::fold_keepalive(
                &tx,
                &phoebis_ledger::KeepaliveDelta {
                    wasted_probes: 3,
                    wasted_probe_reasons: [
                        ("horizon".to_string(), 1),
                        ("idle_limit".to_string(), 2),
                    ]
                    .into(),
                    ..delta("main", "direct", "probe")
                },
            )?;
            phoebis_ledger::fold_keepalive(
                &tx,
                &phoebis_ledger::KeepaliveDelta {
                    wasted_probes: 1,
                    wasted_probe_reasons: [("expired".to_string(), 1)].into(),
                    ..delta("", "solo", "probe")
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        overview["pools"],
        serde_json::json!([
            {
                "credited_24h": 1,
                "idle_returns": idle_returns(&[0]),
                "pool_id": "main",
                "probes_24h": 1,
                "wasted_24h": 3,
                "wasted_reasons_24h": wasted_reasons(&[("horizon", 1), ("idle_limit", 2)]),
            },
            {
                "credited_24h": 0,
                "idle_returns": idle_returns(&[]),
                "pool_id": "night",
                "probes_24h": 1,
                "wasted_24h": 0,
                "wasted_reasons_24h": wasted_reasons(&[]),
            },
            {
                "credited_24h": 0,
                "idle_returns": idle_returns(&[]),
                "pool_id": "solo",
                "probes_24h": 0,
                "wasted_24h": 1,
                "wasted_reasons_24h": wasted_reasons(&[("expired", 1)]),
            },
        ]),
        "the day's rows join the counters by pool id, the day-old probe outside the window"
    );
    assert_eq!(
        overview["probes_24h"]["ok"].as_u64(),
        Some(2),
        "the router-wide count reads the same window"
    );
    // An ended session keeps its entry until the expiry and holds no slot, so the count beside the cap leaves it out.
    for (event, sequence) in [("session_start", 1), ("session_end", 2)] {
        parts
            .keepalive
            .apply_event(
                "desktop",
                generation,
                Event {
                    event: event.into(),
                    session_id: "s2".into(),
                    instance_id: "i2".into(),
                    sequence,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    assert_eq!(parts.keepalive.stats().sessions, 2, "the ended entry stays");
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        overview["limits"]["sessions"],
        serde_json::json!({ "count": 1, "max": 128 })
    );
}

#[tokio::test]
async fn the_overview_counts_the_plan_funded_answered_probes_apart_from_the_billed_ones() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    // One answered probe on the subscription account's plan and one through the API key account.
    for (id, account, account_kind) in [
        ("probe-plan", "business", "oauth"),
        ("probe-billed", "direct", "api_key"),
    ] {
        parts
            .db
            .insert_request(
                &phoebis_ledger::RequestRecord {
                    id: id.into(),
                    received_at: now,
                    finished_at: Some(now + 500),
                    key_id: Some("desktop".into()),
                    pool_id: Some("main".into()),
                    canonical_model: "claude-sonnet-5".into(),
                    kind: "probe".into(),
                    origin: "keepalive".into(),
                    outcome: "succeeded".into(),
                    status: Some(200),
                    attempts: 1,
                    ..Default::default()
                },
                &[phoebis_ledger::AttemptRecord {
                    request_id: id.into(),
                    attempt: 1,
                    account_id: account.into(),
                    account_kind: account_kind.into(),
                    started_at: now,
                    finished_at: Some(now + 500),
                    outcome: "succeeded".into(),
                    input_tokens: Some(100),
                    output_tokens: Some(10),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();
    }
    let rows = parts
        .db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .unwrap();
    let plan_served: u64 = rows
        .iter()
        .filter(|r| {
            r.kind == "probe"
                && phoebis_ledger::Funding::of_rollup(&r.funding) == phoebis_ledger::Funding::Plan
        })
        .map(|r| r.succeeded)
        .sum();
    assert_eq!(
        plan_served, 1,
        "the rows file one answered probe under the plan: {rows:?}"
    );
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        (
            overview["probes_24h"]["ok"].as_u64(),
            overview["probes_24h"]["plan_ok"].as_u64()
        ),
        (Some(2), Some(plan_served)),
        "both answered probes count, the plan-funded one apart: {overview}"
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
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r3".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: g, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now }));
    parts.keepalive.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "r3".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
        },
    ));
    parts.keepalive.observe(phoebis_bus::Event::RequestComplete(
        phoebis_bus::RequestComplete {
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
            client_contract: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            held: None,
            diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            passthrough: None,
            finished_at: now,
        },
    ));
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

/// A deleted key's session outlives the key in the engine until the engine's next tick ends it: the row must read
/// as historical, with no companion, agents, or recipe, and the console must not be able to release it by naming
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
    let held =
        |engine: &phoebis_keepalive::Engine| engine.sessions().iter().any(|s| s.session_id == "s1");
    assert!(
        held(&parts.keepalive),
        "the engine holds the session until its next tick"
    );
    parts.keepalive.step().await;
    assert!(
        !held(&parts.keepalive),
        "the next tick ends the deleted key's session in the engine"
    );
}

/// The console decodes a recipe's cause against the `RecipeCause` union of its API types, so a word the engine
/// serializes that the union lacks would read as no cause at all, and a word the union keeps that the engine dropped
/// is a label nothing can reach.
#[test]
fn every_engine_reason_is_a_cause_word_the_console_types_list() {
    use phoebis_keepalive::Reason;
    use std::collections::BTreeSet;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/api/types.ts");
    let types = std::fs::read_to_string(&path).unwrap();
    let start = types
        .find("export type RecipeCause =")
        .expect("types.ts declares the RecipeCause union");
    let union = &types[start..];
    let union = &union[..union.find(';').expect("the union ends with a semicolon")];
    let listed: BTreeSet<&str> = union.split('\'').skip(1).step_by(2).collect();
    let engine: BTreeSet<&str> = Reason::ALL.iter().map(|reason| reason.as_str()).collect();
    assert_eq!(listed, engine);
    for reason in Reason::ALL {
        assert_eq!(
            serde_json::to_value(reason).unwrap(),
            serde_json::json!(reason.as_str()),
            "the cause is serialized as the word itself"
        );
    }
}

/// A `session_end` leaves the session listed, as the tombstone the lifecycle protocol needs, but nothing is
/// reporting for it any more: the view says so, which keeps it out of the console's companion count.
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
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    assert_eq!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["session_id"].as_str(), r["companion"].as_str()))
            .collect::<Vec<_>>(),
        [(Some("s1"), Some("reporting"))],
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
    assert_eq!(
        (
            row["keepalive"].clone(),
            row["presence_age_seconds"].clone()
        ),
        (
            serde_json::json!({"cause": null, "last_guard": null, "next_probe_at": null, "state": null}),
            serde_json::Value::Null
        ),
        "no recipe and no presence reported"
    );
    assert_eq!(
        listed["sessions"].as_array().map(Vec::len),
        Some(1),
        "the ended session is the one row"
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

/// The engine refuses an operator's release once the shutdown has quiesced it, so the final save stays what the next
/// process restores: the route answers 503 rather than reporting a release that changed nothing.
#[tokio::test]
async fn a_release_after_the_engines_quiesce_answers_503() {
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
    parts.keepalive.quiesce();
    let released = post(
        &parts.app,
        "/admin/sessions/desktop/s1/release",
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(released.status(), 503);
}
