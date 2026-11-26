//! The keepalive policy on a pool: typed on the way in, whole on the way out, and gated by affinity; the
//! sessions, recipes, and overview views the admin API renders from the engine.
mod common;
use common::{delete, get, json, login, patch, post};
use phoebis_store::AccountKind;
use rusqlite::TransactionBehavior;

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
    let parts =
        common::admin_parts_full_costed(phoebis_ledger::test_tables::subscription_costs()).await;
    let app = &parts.app;
    let cookie = login(app, "admin-secret").await;
    let created = post(
        app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}],"cache_ttl":{"enabled":true},"model_rules":[{"pattern":"claude-opus-5-5"},{"pattern":"*","not":true}],"keepalive":{"enabled":true}}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let created = post(
        app,
        "/admin/pools",
        r#"{"id":"empty","label":"Empty","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    let listed = json(get(app, "/admin/pools", Some(&cookie)).await).await;
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
    // A one-hour TTL probed six minutes ahead is one probe every 3,240 seconds, and the test cost model's 1-hour write
    // at 2 against its read at 0.1 breaks even after 19 of them, 61,560 seconds.
    assert_eq!(
        [
            seconds("break_even_seconds"),
            seconds("idle_seconds"),
            seconds("parked_seconds"),
            seconds("horizon_seconds")
        ],
        [
            (Some(61_560.0), Some(61_560.0)),
            (Some(3_600.0), Some(3_600.0)),
            (Some(61_560.0), Some(61_560.0)),
            (Some(61_560.0), Some(61_560.0))
        ],
        "an OAuth member, a one-hour main TTL and one model: the test cost model's break-even, the hour the policy \
         sets and the two null ceilings at the break-even"
    );
    // The policy's idle hour holds 10/9 probes at a twentieth of a write each, 1/18 of a write.
    let share = seconds("idle_share");
    assert!(
        share.0 == share.1 && (share.0.unwrap() - 0.05556).abs() < 5e-5,
        "{share:?}"
    );
    assert!(
        pool("empty")["keepalive_resolved"].is_null(),
        "a pool with no member has nothing to resolve over"
    );
}

#[tokio::test]
async fn the_sessions_view_carries_the_companion_and_the_recipes() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    // A registration alone lists the session.
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 1
        }),
    )
    .await;
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
            row["spend"].is_null(),
            row["keepalive"]["state"].is_null(),
            row["keepalive"]["cause"].is_null()
        ),
        (Some("reporting"), Some(1), Some(false), true, true, true)
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
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r1".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: common::generation(&parts.store, "desktop"), pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, instance_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now, fallback_digest: None, cache_marker: false }));
    parts.keepalive.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "direct".into(),
            incarnation: parts.store.account("direct").unwrap().unwrap().incarnation,
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
            previous_input_tokens: None,
            entry_expired: None,
            agent_released: None,
        },
    ));
    parts.keepalive.observe(phoebis_bus::Event::RequestComplete(
        phoebis_bus::RequestComplete {
            request_id: "r1".into(),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            origin: phoebis_bus::Origin::Client,
            kind: phoebis_bus::Kind::Inference,
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
            thinking_diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            classifier: None,
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
        instance_id: None,
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
        boundaries: Vec::new().into(),
        marked: Vec::new().into(),
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
        fallback_digest: None,
        cache_marker: false,
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
        (
            overview["agents"]["busy"].as_u64(),
            overview["bus_gaps"].as_u64(),
            overview["limits"]["sessions"]["count"].as_u64(),
            overview["limits"]["agents"]["count"].as_u64()
        ),
        (Some(1), Some(0), Some(1), Some(1))
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(system["keepalive"]["running_probes"].as_u64(), Some(0));
}

/// Each reported value differs from every other, so a member that answers another member's value fails here. The case
/// calls no flush behind a facts report, so each title reaches the route through a save that the handler started.
#[tokio::test]
async fn a_session_row_answers_the_facts_its_companion_reported() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    let titled = |title: &'static str| {
        move |v: &serde_json::Value| {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["session_id"] == "s1" && r["facts"]["title"] == title)
        }
    };
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    // The save that the registration asked for can carry the first title, so the case reads the members behind a
    // second one.
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "title": "Survey the breakwater"
    }))
    .await;
    common::wait_json(
        &parts.app,
        "/admin/sessions",
        &cookie,
        titled("Survey the breakwater"),
    )
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 3,
        "branch": "fix/checkout-retry",
        "cwd": "/home/dev/work/storefront/web/src",
        "project": "/home/dev/work/storefront/web",
        "repository": "https://git.example/acme/storefront.git",
        "repository_root": "/home/dev/work/storefront",
        "title": "Checkout retry rework"
    }))
    .await;
    common::wait_json(
        &parts.app,
        "/admin/sessions",
        &cookie,
        titled("Checkout retry rework"),
    )
    .await;
    // The agent's reports follow both polls, so the save that they ask for lands no title in the place of a title's own
    // save.
    let started = report(serde_json::json!({
        "protocol": 1,
        "event": "agent_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 4,
        "agent_id": "a1"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 5,
        "agent_id": "a1",
        "request_generation": started["request_generation"],
        "agent_type": "code-reviewer",
        "description": "Review the retry loop",
        "name": "reviewer"
    }))
    .await;
    let listed = common::wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        v["sessions"].as_array().unwrap().iter().any(|r| {
            r["session_id"] == "s1" && r["agent_facts"].as_array().is_some_and(|a| a.len() == 1)
        })
    })
    .await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .expect("a registered session is listed");
    assert_eq!(
        row["facts"],
        serde_json::json!({
            "branch": "fix/checkout-retry",
            "cwd": "/home/dev/work/storefront/web/src",
            "project": "/home/dev/work/storefront/web",
            "repository": "git.example/acme/storefront",
            "repository_root": "/home/dev/work/storefront",
            "title": "Checkout retry rework"
        }),
        "the row answers the session's facts as the router kept them, the remote reduced"
    );
    assert_eq!(
        row["agent_facts"],
        serde_json::json!([{
            "agent_id": "a1",
            "agent_type": "code-reviewer",
            "description": "Review the retry loop",
            "name": "reviewer"
        }]),
        "the agent that reported facts has an entry, and main, which reported none, has none"
    );
}

/// The pool `main` behind the key `desktop`, as the cases of the stored facts create them, with the admin's cookie and
/// the key's secret.
async fn stored_facts_harness() -> (common::Full, String, String) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    (parts, cookie, secret)
}

/// The row of `session` in an answer of `GET /admin/sessions`.
fn row_of<'a>(listed: &'a serde_json::Value, session: &str) -> &'a serde_json::Value {
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == session)
        .unwrap_or_else(|| panic!("no row for {session}: {listed}"))
}

/// The engine of the restarted router holds no record of the ended session, and a request of the session keeps its row
/// listed, so the title and the subagent's name come from the stored sets alone.
#[tokio::test]
async fn a_finished_sessions_row_answers_its_stored_title_and_its_agents_name_after_a_restart() {
    let (parts, _, secret) = stored_facts_harness().await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "title": "Reindex the lighthouse archive"
    }))
    .await;
    let started = report(serde_json::json!({
        "protocol": 1,
        "event": "agent_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 3,
        "agent_id": "a1"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 4,
        "agent_id": "a1",
        "request_generation": started["request_generation"],
        "name": "archive-scout"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_end",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 5
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    // The load drops the ended session and saves the removal of its rows ahead of the read.
    let restarted =
        common::admin_parts_full_on(parts.store.clone(), parts.clock_cell.clone()).await;
    restarted.keepalive.load_persisted().await;
    restarted.ledger.record(classed_request(
        "r1",
        "s1",
        "main",
        "claude-opus-5",
        None,
        common::NOW,
        common::generation(&restarted.store, "desktop"),
    ));
    common::landed(&restarted, "r1").await;
    let cookie = login(&restarted.app, "admin-secret").await;
    let listed = json(get(&restarted.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = row_of(&listed, "s1");
    assert_eq!(
        (row["companion"].as_str(), row["facts"]["title"].as_str()),
        (Some("none"), Some("Reindex the lighthouse archive")),
        "the finished session's row answers its stored title: {row}"
    );
    assert_eq!(
        row["agent_facts"],
        serde_json::json!([{
            "agent_id": "a1",
            "agent_type": null,
            "description": null,
            "name": "archive-scout"
        }]),
        "the ended subagent keeps its stored name on the row"
    );
}

/// The new instance's record holds main alone, so the earlier subagent's name stands in the registry alone. The case
/// saves twice, so the registration's removal of the subagent's saved row lands behind the write of its set.
#[tokio::test]
async fn a_session_registered_again_by_another_instance_answers_the_earlier_agents_facts() {
    let (parts, cookie, secret) = stored_facts_harness().await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    let started = report(serde_json::json!({
        "protocol": 1,
        "event": "agent_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "agent_id": "a1"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 3,
        "agent_id": "a1",
        "request_generation": started["request_generation"],
        "name": "tide-table-writer"
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i2",
        "sequence": 1,
        "previous_instance_id": "i1"
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    assert_eq!(
        row_of(&listed, "s1")["agent_facts"],
        serde_json::json!([{
            "agent_id": "a1",
            "agent_type": null,
            "description": null,
            "name": "tide-table-writer"
        }]),
        "the earlier instance's subagent keeps its stored name on the row of the session registered again"
    );
}

/// The new instance reports no title, and the earlier instance's title stays off the row of the session registered
/// again.
#[tokio::test]
async fn a_title_of_the_earlier_instance_is_gone_behind_the_registration() {
    let (parts, cookie, secret) = stored_facts_harness().await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "title": "Chart the estuary"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i2",
        "sequence": 1,
        "previous_instance_id": "i1"
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = row_of(&listed, "s1");
    assert_eq!(
        (row["companion"].as_str(), row["facts"]["title"].as_str()),
        (Some("reporting"), None),
        "the new instance's row answers no title of the earlier one: {row}"
    );
}

/// The second reports state the title alone and no member of the subagent, so the row of the session's project and
/// the subagent's row leave the registry.
#[tokio::test]
async fn a_report_that_empties_a_set_deletes_its_stored_row() {
    let (parts, cookie, secret) = stored_facts_harness().await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    let started = report(serde_json::json!({
        "protocol": 1,
        "event": "agent_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "agent_id": "a1"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 3,
        "branch": "tide-gauge",
        "title": "Dredge the fairway"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 4,
        "agent_id": "a1",
        "request_generation": started["request_generation"],
        "name": "sounding-lead"
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = row_of(&listed, "s1");
    assert_eq!(
        row["facts"]["branch"], "tide-gauge",
        "the row answers the branch ahead of the report that empties the project's set"
    );
    assert_eq!(
        row["agent_facts"],
        serde_json::json!([{
            "agent_id": "a1",
            "agent_type": null,
            "description": null,
            "name": "sounding-lead"
        }]),
        "the row answers the subagent's name ahead of the report that empties its set"
    );
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 5,
        "title": "Dredge the fairway"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 6,
        "agent_id": "a1",
        "request_generation": started["request_generation"]
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = row_of(&listed, "s1");
    assert_eq!(
        row["facts"],
        serde_json::json!({
            "branch": null,
            "cwd": null,
            "project": null,
            "repository": null,
            "repository_root": null,
            "title": "Dredge the fairway"
        }),
        "the report that names the title alone takes the branch off the row"
    );
    assert_eq!(
        row["agent_facts"],
        serde_json::json!([]),
        "the report that names no member of the subagent takes its entry off the row"
    );
}

/// The case calls no flush, so the title leaves the row through the save that the second registration asked for.
#[tokio::test]
async fn a_registration_that_names_the_project_alone_drops_the_stored_title_and_keeps_the_project()
{
    let (parts, cookie, secret) = stored_facts_harness().await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "project": "/home/dev/work/harbor-charts",
        "title": "Dredge the inner basin"
    }))
    .await;
    common::wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        let facts = &row_of(v, "s1")["facts"];
        facts["title"] == "Dredge the inner basin"
            && facts["project"] == "/home/dev/work/harbor-charts"
    })
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 3,
        "reported_facts": ["project"]
    }))
    .await;
    let listed = common::wait_json(&parts.app, "/admin/sessions", &cookie, |v| {
        row_of(v, "s1")["facts"]["title"].is_null()
    })
    .await;
    let facts = &row_of(&listed, "s1")["facts"];
    assert_eq!(
        (facts["project"].as_str(), facts["title"].as_str()),
        (Some("/home/dev/work/harbor-charts"), None),
        "the registration that names the project alone keeps the stored project and drops the title: {facts}"
    );
}

/// The released subagent's record leaves an hour behind its release, so its stored name stands with no record of the
/// agent in the engine when the registration drops the kind.
#[tokio::test]
async fn a_registration_that_names_no_agents_drops_each_agents_stored_set() {
    let (parts, _, secret) = stored_facts_harness().await;
    let report =
        |body: serde_json::Value| common::report_as_the_mod(&parts.keepalive, &secret, body);
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 1
    }))
    .await;
    let first = report(serde_json::json!({
        "protocol": 1,
        "event": "agent_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 2,
        "agent_id": "a1"
    }))
    .await;
    let second = report(serde_json::json!({
        "protocol": 1,
        "event": "agent_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 3,
        "agent_id": "a2"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 4,
        "agent_id": "a1",
        "request_generation": first["request_generation"],
        "name": "buoy-tender"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "agent_facts",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 5,
        "agent_id": "a2",
        "request_generation": second["request_generation"],
        "name": "lock-keeper"
    }))
    .await;
    report(serde_json::json!({
        "protocol": 1,
        "event": "state",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 6,
        "agent_id": "a2",
        "request_generation": second["request_generation"],
        "state": "released"
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    common::step(&parts, std::time::Duration::from_secs(3_601));
    // The step outlasts the admin session's idle bound. The session's lease lapsed too, so no report but the
    // registration goes out until it stands.
    let cookie = login(&parts.app, "admin-secret").await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = row_of(&listed, "s1");
    assert_eq!(
        (row["agents"].as_u64(), &row["agent_facts"]),
        (
            Some(2),
            &serde_json::json!([
                {"agent_id": "a1", "agent_type": null, "description": null, "name": "buoy-tender"},
                {"agent_id": "a2", "agent_type": null, "description": null, "name": "lock-keeper"}
            ])
        ),
        "the engine holds no record of the released subagent, and both stored names stand: {row}"
    );
    report(serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "s1",
        "instance_id": "i1",
        "sequence": 7,
        "reported_facts": ["project", "title"]
    }))
    .await;
    parts.keepalive.flush_persisted().await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    assert_eq!(
        row_of(&listed, "s1")["agent_facts"],
        serde_json::json!([]),
        "the registration that names no agents drops the stored set of each agent, the one with no record among them"
    );
}

/// No agent of a session whose own option holds the cache warming off holds a recipe that its row could stand on, so
/// the row reads the cause and the state from the session's word, while a session with the option on that sent nothing
/// reads neither.
#[tokio::test]
async fn a_session_registered_with_the_warming_off_reads_session_opted_out_and_ended_on_its_row() {
    let (parts, cookie, secret) = stored_facts_harness().await;
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s-off",
            "instance_id": "i-off",
            "sequence": 1,
            "cache_warming": false
        }),
    )
    .await;
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s-on",
            "instance_id": "i-on",
            "sequence": 1,
            "cache_warming": true
        }),
    )
    .await;
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let summary = |session: &str| {
        let keepalive = &row_of(&listed, session)["keepalive"];
        (keepalive["cause"].clone(), keepalive["state"].clone())
    };
    assert_eq!(
        (summary("s-off"), summary("s-on")),
        (
            (
                serde_json::json!("session_opted_out"),
                serde_json::json!("ended")
            ),
            (serde_json::Value::Null, serde_json::Value::Null)
        ),
        "{listed}"
    );
}

/// The ten idle-age buckets as the overview answers them, `reached` raised in the buckets `reached` names.
fn idle_returns(reached: &[usize]) -> serde_json::Value {
    let bounds = phoebis_session_engine::IDLE_AGE_BOUNDS_SECONDS;
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
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    let generation = common::generation(&parts.store, "desktop");
    let started = common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 1
        }),
    )
    .await;
    let now = common::NOW;
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r1".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: generation, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, instance_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now, fallback_digest: None, cache_marker: false }));
    parts.keepalive.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "direct".into(),
            incarnation: parts.store.account("direct").unwrap().unwrap().incarnation,
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
            previous_input_tokens: None,
            entry_expired: None,
            agent_released: None,
        },
    ));
    parts.keepalive.observe(phoebis_bus::Event::RequestComplete(
        phoebis_bus::RequestComplete {
            request_id: "r1".into(),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            origin: phoebis_bus::Origin::Client,
            kind: phoebis_bus::Kind::Inference,
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
            thinking_diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            classifier: None,
            finished_at: now,
        },
    ));
    // Main goes idle, and the next tick opens its idle window in the first bucket. The report goes out under the
    // generation the registration answered, which main's request moved since.
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "event": "state",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 2,
            "agent_id": "main",
            "state": "idle",
            "request_generation": started["request_generation"]
        }),
    )
    .await;
    parts.keepalive.step().await;
    let retained = parts.keepalive.stats().warming.retained_bytes;
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
            // The transaction takes the write lock at its start, as the ledger's own writes do: the fold reads before
            // it writes, and the purge task commits on a connection of its own.
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                    probes_skipped: 2,
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
    assert_eq!(
        overview["probes_24h"]["skipped"].as_u64(),
        Some(2),
        "the skipped count reads its own column beside the day's four wasted probes"
    );
    // An ended session keeps its entry until the expiry and holds no slot, so the count beside the cap leaves it out.
    let s2 = |event: &str, sequence: u64| {
        common::report_as_the_mod(
            &parts.keepalive,
            &secret,
            serde_json::json!({
                "protocol": 1,
                "event": event,
                "session_id": "s2",
                "instance_id": "i2",
                "sequence": sequence
            }),
        )
    };
    s2("session_start", 1).await;
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r2".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: generation, pool_id: Some("main".into()), session_id: Some("s2".into()), agent_id: Some("main".into()), parent_agent_id: None, instance_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now, fallback_digest: None, cache_marker: false }));
    s2("session_end", 2).await;
    assert_eq!(
        parts.keepalive.stats().sessions.entries,
        2,
        "the ended entry stays"
    );
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        overview["limits"]["sessions"],
        serde_json::json!({ "count": 1, "max": 128 })
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (
            system["sessions"]["live"].as_u64(),
            system["sessions"]["entries"].as_u64()
        ),
        (Some(1), Some(2)),
        "the system document's session count is the overview's, and its entries keep the ended one: {system}"
    );
}

/// A session that fills the running engine's session cap raises an item of the session caps' kind under the cap's
/// config key and none of the byte cap's, which breaks when the admin hands the reader the session caps as the byte
/// cap's list and the byte cap as the session caps'.
#[tokio::test]
async fn a_full_session_cap_of_the_running_engine_raises_a_session_cap_item_and_no_keepalive_cap_item(
) {
    let parts = common::admin_parts_full_with_session_caps(phoebis_session_engine::SessionCaps {
        max_agents: 8,
        max_sessions: 1,
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    let generation = common::generation(&parts.store, "desktop");
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s-capped",
            "instance_id": "i-capped",
            "sequence": 1
        }),
    )
    .await;
    // The session takes the one slot at its first request, which the registration alone does not.
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r-capped".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: generation, pool_id: Some("main".into()), session_id: Some("s-capped".into()), agent_id: Some("main".into()), parent_agent_id: None, instance_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: common::NOW, fallback_digest: None, cache_marker: false }));
    let body = json(get(&parts.app, "/admin/attention", Some(&cookie)).await).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|item| item["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["session_cap"],
        "the full session cap raises its own kind, and the byte cap under its max raises nothing: {body}"
    );
    assert_eq!(
        (
            &items[0]["subject"]["id"],
            &items[0]["subject"]["label"],
            &items[0]["detail"]
        ),
        (
            &serde_json::json!("sessions.max_sessions"),
            &serde_json::json!("Sessions"),
            &serde_json::json!({ "cap": "sessions", "count": 1, "max": 1, "pct": 1.0 })
        ),
        "the item names the config key that sets the cap, and its detail reads the one session against the max of 1"
    );
}

/// The overview lists no row for an inference rollup row, and the day's expired count still takes its expiries.
#[tokio::test]
async fn the_overviews_expired_count_sums_every_row_of_the_day_the_inference_rows_included() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    // A marked body's serving attempt under the reason `none`, past its entry's lifetime and of an agent the keepalive
    // still held, that reads none of the previous request's 1,000 tokens back files one expiry. The two pools file two
    // rows, the first holding two requests.
    parts
        .db
        .write(|c| {
            // The transaction takes the write lock at its start, as the ledger's own writes do: the fold reads before
            // it writes, and the purge task commits on a connection of its own.
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (id, pool) in [("r1", "main"), ("r2", "main"), ("r3", "night")] {
                phoebis_ledger::fold_request(
                    &tx,
                    &phoebis_ledger::RequestRecord {
                        id: id.into(),
                        received_at: now,
                        key_id: Some("desktop".into()),
                        pool_id: Some(pool.into()),
                        canonical_model: "claude-sonnet-5".into(),
                        kind: "inference".into(),
                        outcome: "succeeded".into(),
                        status: Some(200),
                        cache_marker: Some(true),
                        ..Default::default()
                    },
                    &[phoebis_ledger::AttemptRecord {
                        request_id: id.into(),
                        attempt: 1,
                        account_id: "direct".into(),
                        outcome: "succeeded".into(),
                        input_tokens: Some(10),
                        cache_miss_reason: Some("none".into()),
                        previous_input_tokens: Some(1_000),
                        entry_expired: Some(true),
                        agent_released: Some(false),
                        ..Default::default()
                    }],
                    None,
                    &phoebis_ledger::PriceRevisions::shipped(),
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(overview["expired_between_turns_24h"].as_u64(), Some(3));
}

const DAY: i64 = 86_400_000;
const MINUTE: i64 = 60_000;

/// The ten idle-age buckets as the overview answers them, each bucket's `reached` and `returned` as named.
fn idle_returns_counted(reached: [u64; 10], returned: [u64; 10]) -> serde_json::Value {
    let bounds = phoebis_session_engine::IDLE_AGE_BOUNDS_SECONDS;
    serde_json::Value::Array(
        bounds
            .iter()
            .enumerate()
            .map(|(i, from)| {
                serde_json::json!({
                    "from_seconds": from,
                    "reached": reached[i],
                    "returned": returned[i],
                    "to_seconds": bounds.get(i + 1),
                })
            })
            .collect(),
    )
}

/// One companion session of key `desktop` on the overview's harness, its reports numbered in order.
struct Companion<'a> {
    parts: &'a common::Full,
    generation: i64,
    /// Main's request generation as the last answer carried it, every answer here being main's.
    main_generation: u64,
    session: &'static str,
    sequence: u64,
}

impl<'a> Companion<'a> {
    async fn start(parts: &'a common::Full, session: &'static str) -> Companion<'a> {
        let mut companion = Companion {
            parts,
            generation: common::generation(&parts.store, "desktop"),
            main_generation: 0,
            session,
            sequence: 0,
        };
        companion.report("session_start", None).await;
        companion
    }

    async fn report(&mut self, event: &str, state: Option<&str>) {
        self.sequence += 1;
        let mut body = serde_json::json!({
            "event": event,
            "session_id": self.session,
            "instance_id": self.session,
            "sequence": self.sequence
        });
        if let Some(state) = state {
            body["agent_id"] = "main".into();
            body["state"] = state.into();
            body["request_generation"] = self.main_generation.into();
        }
        let secret = self
            .parts
            .desktop_secret
            .as_deref()
            .expect("the harness created the key desktop");
        let ack = common::report_as_the_mod(&self.parts.keepalive, secret, body).await;
        self.main_generation = ack["request_generation"]
            .as_u64()
            .expect("an acknowledgment names main's generation");
    }

    /// Main's request at the harness clock's instant on the chain `boundaries` names, answered.
    fn main_request(&self, id: &str, boundaries: &[&str]) {
        let now = (self.parts.clock_cell).load(std::sync::atomic::Ordering::SeqCst);
        self.parts
            .keepalive
            .observe(phoebis_bus::Event::RequestObserved(
                phoebis_bus::RequestObserved {
                    request_id: id.into(),
                    origin: phoebis_bus::Origin::Client,
                    kind: phoebis_bus::Kind::Inference,
                    key_id: Some("desktop".into()),
                    key_generation: self.generation,
                    pool_id: Some("main".into()),
                    session_id: Some(self.session.into()),
                    agent_id: Some("main".into()),
                    parent_agent_id: None,
                    instance_id: None,
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
                    static_hash: Some("static".into()),
                    boundaries: boundaries.iter().map(|b| b.to_string()).collect(),
                    marked: Vec::new().into(),
                    client: phoebis_bus::ClientFacts::default(),
                    auto_mode: None,
                    fork: phoebis_bus::Fork::default(),
                    presented_retired: false,
                    rotation_at: None,
                    guardrail_id: None,
                    guardrails: Vec::new(),
                    received_at: now,
                    fallback_digest: None,
                    cache_marker: false,
                },
            ));
        self.parts
            .keepalive
            .observe(phoebis_bus::Event::AccountSelected(
                phoebis_bus::AccountSelected {
                    request_id: id.into(),
                    attempt: 1,
                    account_id: "direct".into(),
                    incarnation: self
                        .parts
                        .store
                        .account("direct")
                        .unwrap()
                        .unwrap()
                        .incarnation,
                    account_kind: "api_key".into(),
                    usage_based: false,
                    pin_epoch: Some(1),
                    started_at: now,
                    cache_ttl_ms: None,
                    selection: None,
                    previous_input_tokens: None,
                    entry_expired: None,
                    agent_released: None,
                },
            ));
        self.parts
            .keepalive
            .observe(phoebis_bus::Event::RequestComplete(
                phoebis_bus::RequestComplete {
                    request_id: id.into(),
                    key_id: Some("desktop".into()),
                    pool_id: Some("main".into()),
                    origin: phoebis_bus::Origin::Client,
                    kind: phoebis_bus::Kind::Inference,
                    attempt: 1,
                    account_id: Some("direct".into()),
                    outcome: phoebis_bus::Outcome::Succeeded,
                    status: 200,
                    usage: None,
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
                    thinking_diagnostics: None,
                    message_id: None,
                    skipped_events: 0,
                    interruption: None,
                    classifier: None,
                    finished_at: now,
                },
            ));
    }

    /// The harness clock moved to `until` in steps inside the companion's lease, each followed by a heartbeat, as an
    /// attending companion renews it.
    async fn attend_to(&mut self, until: i64) {
        let cell = &self.parts.clock_cell;
        while cell.load(std::sync::atomic::Ordering::SeqCst) < until {
            let now = cell.load(std::sync::atomic::Ordering::SeqCst);
            let next = (now + phoebis_session_engine::LEASE_MS / 2).min(until);
            cell.store(next, std::sync::atomic::Ordering::SeqCst);
            self.report("heartbeat", None).await;
        }
    }
}

/// A pool `main` over `direct` with keepalive off, so no probe runs whatever the clock does, and the key `desktop` in
/// it, whose secret the harness holds: main's idle windows count under `main` all the same.
async fn idle_returns_harness() -> common::Full {
    let mut parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}]}"#,
        Some(&cookie),
    )
    .await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    parts.desktop_secret = Some(secret);
    parts
}

/// The pool `main`'s idle returns as the overview answers them under `query`, read under a login of its own, since an
/// admin session ends 12 hours after its login and the cases move the clock days ahead.
async fn main_idle_returns(parts: &common::Full, query: &str) -> serde_json::Value {
    let cookie = login(&parts.app, "admin-secret").await;
    let overview = json(
        get(
            &parts.app,
            &format!("/admin/keepalive/overview{query}"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    overview["pools"]
        .as_array()
        .unwrap_or_else(|| panic!("no pools: {overview}"))
        .iter()
        .find(|pool| pool["pool_id"] == "main")
        .map(|pool| pool["idle_returns"].clone())
        .unwrap_or_else(|| panic!("no pool main: {overview}"))
}

/// A window on the harness's day reaches the first two buckets and returns in the second, and one ten days later
/// reaches the first alone: the overview sums the current UTC day and the 6 before it unless `range` asks for 30.
#[tokio::test]
async fn the_overview_sums_the_idle_returns_of_the_last_7_utc_days_and_of_30_under_range_30d() {
    let parts = idle_returns_harness().await;
    let mut first = Companion::start(&parts, "s1").await;
    first.main_request("r1", &["b1"]);
    first.report("state", Some("idle")).await;
    parts.keepalive.step().await;
    first.attend_to(common::NOW + 6 * MINUTE).await;
    first.main_request("r2", &["b1", "b2"]);
    parts
        .clock_cell
        .store(common::NOW + 10 * DAY, std::sync::atomic::Ordering::SeqCst);
    let mut later = Companion::start(&parts, "s2").await;
    later.main_request("r3", &["d1"]);
    later.report("state", Some("idle")).await;
    parts.keepalive.step().await;
    let last_seven = idle_returns_counted([1, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]);
    assert_eq!(
        main_idle_returns(&parts, "").await,
        last_seven,
        "the default range is the last 7 days"
    );
    assert_eq!(main_idle_returns(&parts, "?range=7d").await, last_seven);
    assert_eq!(
        main_idle_returns(&parts, "?range=30d").await,
        idle_returns_counted(
            [2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "the last 30 days hold both windows"
    );
}

/// A window reaches its eighth bucket before midnight and returns in it after: the return counts on the reach's day,
/// so a range that starts on the day after answers neither, and no bucket answers a return without its reach.
#[tokio::test]
async fn a_return_after_midnight_counts_on_its_reachs_day_so_a_range_from_the_next_day_answers_neither(
) {
    let parts = idle_returns_harness().await;
    let midnight = (common::NOW / DAY + 1) * DAY;
    let mut companion = Companion::start(&parts, "s1").await;
    companion.main_request("r1", &["b1"]);
    companion.report("state", Some("idle")).await;
    parts.keepalive.step().await;
    companion.attend_to(midnight - MINUTE).await;
    parts.keepalive.step().await;
    companion.attend_to(midnight + MINUTE).await;
    companion.main_request("r2", &["b1", "b2"]);
    let both = idle_returns_counted(
        [1, 1, 1, 1, 1, 1, 1, 1, 0, 0],
        [0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
    );
    // Five days after the return's day, the last 7 days still start on the reach's day.
    parts.clock_cell.store(
        midnight + 5 * DAY + MINUTE,
        std::sync::atomic::Ordering::SeqCst,
    );
    assert_eq!(
        main_idle_returns(&parts, "?range=7d").await,
        both,
        "the reach's day inside the range"
    );
    parts.clock_cell.store(
        midnight + 6 * DAY + MINUTE,
        std::sync::atomic::Ordering::SeqCst,
    );
    assert_eq!(
        main_idle_returns(&parts, "?range=7d").await,
        idle_returns_counted([0; 10], [0; 10]),
        "the range starts on the return's day, after the reach's"
    );
    assert_eq!(main_idle_returns(&parts, "?range=30d").await, both);
}

#[tokio::test]
async fn the_overview_refuses_a_range_other_than_7d_or_30d() {
    let parts = idle_returns_harness().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for range in ["24h", "7D", "", "7d,30d"] {
        let refused = get(
            &parts.app,
            &format!("/admin/keepalive/overview?range={range}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(refused.status(), 400, "{range:?}");
        assert_eq!(
            json(refused).await,
            serde_json::json!({ "error": "range is 7d or 30d" }),
            "{range:?}"
        );
    }
}

#[tokio::test]
async fn the_overview_counts_the_plan_funded_answered_probes_apart_from_the_billed_ones() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    // One answered probe on the subscription account's plan and one through the API key account.
    for (id, account, account_kind) in [
        ("probe-plan", "business", AccountKind::Oauth),
        ("probe-billed", "direct", AccountKind::ApiKey),
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
                    account_kind,
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
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    assert_eq!(
        (
            overview["probes_24h"]["ok"].as_u64(),
            overview["probes_24h"]["plan_ok"].as_u64()
        ),
        (Some(2), Some(1)),
        "both answered probes count, the plan-funded one apart: {overview}"
    );
}

#[tokio::test]
async fn a_session_row_groups_by_key_generation_and_a_historical_row_is_read_only() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
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
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 1
        }),
    )
    .await;
    parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: "r3".into(), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: g, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, instance_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now, fallback_digest: None, cache_marker: false }));
    parts.keepalive.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "r3".into(),
            attempt: 1,
            account_id: "direct".into(),
            incarnation: parts.store.account("direct").unwrap().unwrap().incarnation,
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
            previous_input_tokens: None,
            entry_expired: None,
            agent_released: None,
        },
    ));
    parts.keepalive.observe(phoebis_bus::Event::RequestComplete(
        phoebis_bus::RequestComplete {
            request_id: "r3".into(),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            origin: phoebis_bus::Origin::Client,
            kind: phoebis_bus::Kind::Inference,
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
            thinking_diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            classifier: None,
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
        "the live generation carries the recipe its request armed"
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
}

#[tokio::test]
async fn a_session_row_on_a_usage_based_seat_reads_its_burn_in_dollars() {
    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#, Some(&seeding)).await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    seed.store
        .set_profile(
            "business",
            r#"{"organization":{"rate_limit_tier":"default_claude_zero"}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_zero".into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    let now = common::NOW;
    let input = |input: f64| phoebis_store::PinBurn {
        input,
        ..phoebis_store::PinBurn::default()
    };
    // The router task reads its pins once, at spawn, so the pin is written before the harness that serves the view.
    seed.store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "desktop".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            model: "claude-opus-5".into(),
            account_id: "business".into(),
            epoch: 1,
            created_at: now - 10,
            last_used_at: now - 5,
            key_generation: common::generation(&seed.store, "desktop"),
            incarnation: 1,
            burn: input(100.0),
            burn_at: Some(now),
            burn_banded: input(145.0),
            burn_fast: phoebis_store::PinBurn::default(),
            burn_unmetered: phoebis_store::PinBurn::default(),
            burn_unmetered_model: None,
            last_input_tokens: 1_000,
            last_refresh_at: now - 5,
            cache_ttl_ms: 300_000,
            shed: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-opus-5", "standard", "none", "-"),
            entrypoint: None,
            mark: None,
            auxiliary_only: false,
            effort: None,
            ended: false,
            thinking_sums: Default::default(),
            account_bound: false,
            compacted: false,
        })
        .unwrap();
    let parts = common::admin_parts_full_on(seed.store.clone(), seed.clock_cell.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap_or_else(|| panic!("no row for s1: {sessions}"));
    assert_eq!(
        (&row["burn_unit"], &row["burn_equivalent"]),
        (&serde_json::json!("usd"), &serde_json::Value::Null),
        "the seat bills in dollars, as the selector reads it, with no window equivalent: {row}"
    );
}

/// A conversation pin of `agent` on `session` on `business` under `model`, used five milliseconds before the rig's
/// clock and holding a last prompt.
fn line_pin(session: &str, agent: &str, model: &str, key_generation: i64) -> phoebis_store::PinRow {
    let now = common::NOW;
    phoebis_store::PinRow {
        key_id: "desktop".into(),
        session_id: session.into(),
        agent_id: agent.into(),
        model: model.into(),
        account_id: "business".into(),
        epoch: 1,
        created_at: now - 10,
        last_used_at: now - 5,
        key_generation,
        incarnation: 1,
        burn: phoebis_store::PinBurn::default(),
        burn_at: Some(now),
        burn_banded: phoebis_store::PinBurn::default(),
        burn_fast: phoebis_store::PinBurn::default(),
        burn_unmetered: phoebis_store::PinBurn::default(),
        burn_unmetered_model: None,
        last_input_tokens: 100_000,
        last_refresh_at: now - 5,
        cache_ttl_ms: 300_000,
        shed: None,
        static_hash: None,
        tail_hash: None,
        lane: phoebis_routing::lane_key(model, "standard", "none", "-"),
        entrypoint: None,
        mark: None,
        auxiliary_only: false,
        effort: None,
        ended: false,
        thinking_sums: Default::default(),
        account_bound: false,
        compacted: false,
    }
}

/// The pool `main` over `business` behind key `desktop` holding `pins`, written through one harness and served by a
/// second one, since the router task loads its pins at spawn. The second one holds the key's secret, which verifies
/// against the store they share.
async fn lines_harness(pins: impl Fn(i64) -> Vec<phoebis_store::PinRow>) -> common::Full {
    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#, Some(&seeding)).await;
    let secret = common::created_key_secret(
        &seed.app,
        &seeding,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    for pin in pins(common::generation(&seed.store, "desktop")) {
        seed.store.upsert_pin(&pin).unwrap();
    }
    let mut parts = common::admin_parts_full_on(seed.store.clone(), seed.clock_cell.clone()).await;
    parts.desktop_secret = Some(secret);
    parts
}

/// Registers `session` with the companion, with one observed turn of main and of each of `agents`, so the engine holds
/// a recipe for each and the row is a companion row.
async fn register_with_the_engine(parts: &common::Full, session: &str, agents: &[&str]) {
    let generation = common::generation(&parts.store, "desktop");
    let engine = &parts.keepalive;
    let secret = parts
        .desktop_secret
        .as_deref()
        .expect("the harness created the key desktop");
    let mut sequence = 0;
    let mut report = move |event: &str, agent: Option<&str>| {
        sequence += 1;
        let mut body = serde_json::json!({
            "protocol": 1,
            "event": event,
            "session_id": session,
            "instance_id": "i1",
            "sequence": sequence
        });
        if let Some(agent) = agent {
            body["agent_id"] = agent.into();
        }
        common::report_as_the_mod(engine, secret, body)
    };
    let observe = |agent: &str| {
        parts.keepalive.observe(phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved { request_id: format!("k-{session}-{agent}"), origin: phoebis_bus::Origin::Client, kind: phoebis_bus::Kind::Inference, key_id: Some("desktop".into()), key_generation: generation, pool_id: Some("main".into()), session_id: Some(session.into()), agent_id: Some(agent.to_string()), parent_agent_id: (agent != "main").then(|| "main".into()), instance_id: None, model: "claude-opus-5".into(), canonical_model: "claude-opus-5".into(), wire_model: "claude-opus-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-opus-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: phoebis_bus::RequestShape::default(), static_hash: None, boundaries: Vec::new().into(), marked: Vec::new().into(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: common::NOW, fallback_digest: None, cache_marker: false }));
    };
    report("session_start", None).await;
    observe("main");
    for agent in agents {
        report("agent_start", Some(agent)).await;
        observe(agent);
    }
}

/// A client request of `agent` on `session` naming `model`, classed as the client's `x-claude-code-request-class`
/// header named it, `None` for a caller that sends no such header.
fn classed_request(
    id: &str,
    session: &str,
    agent: &str,
    model: &str,
    class: Option<&str>,
    at: i64,
    key_generation: i64,
) -> phoebis_ledger::Record {
    phoebis_ledger::Record::Request {
        request: phoebis_ledger::RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            key_generation,
            session_id: Some(session.into()),
            agent_id: Some(agent.into()),
            model: model.into(),
            canonical_model: model.trim_end_matches("[1m]").into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            request_class: class.map(String::from),
            ..Default::default()
        },
        attempts: vec![],
        bodies: None,
    }
}

/// Each of `sessions`' rows' agent lines, read once every row counts the requests its pair names.
async fn lines(
    parts: &common::Full,
    cookie: &str,
    sessions: &[(&str, u64)],
) -> Vec<serde_json::Value> {
    let listed = common::wait_json(&parts.app, "/admin/sessions", cookie, |v| {
        sessions.iter().all(|(session, requests)| {
            v["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["session_id"] == *session && r["spend"]["requests"] == *requests)
        })
    })
    .await;
    sessions
        .iter()
        .map(|(session, _)| {
            listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["session_id"] == *session)
                .map(|r| r["agent_contexts"].clone())
                .unwrap_or_default()
        })
        .collect()
}

/// The line the route answers for a `line_pin` of `agent` under `model` used at `last_used_at`, with the agent's
/// lifecycle word where the engine holds its record. A `line_pin` burns nothing on the plan account `business`, whose
/// tier `lines_harness` leaves unnamed, so its burn is a 5-hour share the route cannot size, its Pro-week figure 0 and
/// its plan dollars 0 on a model the harness's price list names, as it names every model these cases pin. Its cache
/// stays warm until its refresh five milliseconds back plus its TTL, and it was written ten milliseconds back. It rides
/// the standard lane, and the harness's router prices no rewrite, so a line reads `fast` false and `move_cost` null.
/// The pin holds no thinking sum, so its move rewrites its whole last prompt, which `move_size` reads with no pricer,
/// and the two drop members read null.
fn line(agent: &str, model: &str, last_used_at: i64, lifecycle: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "agent_id": agent,
        "model": model,
        "account_id": "business",
        "effort": null,
        "last_input_tokens": 100_000,
        "cache_ttl_ms": 300_000,
        "last_used_at": last_used_at,
        "warm": true,
        "warm_until": common::NOW + 299_995,
        "burn": null,
        "burn_unit": "window_share",
        "burn_window": "five_hour",
        "burn_equivalent": 0.0,
        "burn_usd": 0.0,
        "burn_at": common::NOW,
        "pinned_at": common::NOW - 10,
        "ended": false,
        "fast": false,
        "account_bound": false,
        "move_cost": null,
        "move_size": 100_000.0,
        "move_drop_size": null,
        "move_drop_cost": null,
        "lifecycle": lifecycle,
    })
}

/// An auxiliary request (a title, a summary, a classifier call) names a model of its own, and the pin it places is
/// one only auxiliary requests use, which draws no line, so main's line reads its conversation pin.
#[tokio::test]
async fn a_main_pins_line_reads_its_conversation_pin_beside_a_newer_auxiliary_pin() {
    let now = common::NOW;
    let parts = lines_harness(|g| {
        vec![
            phoebis_store::PinRow {
                last_used_at: now - 50,
                ..line_pin("s1", "main", "claude-opus-5", g)
            },
            phoebis_store::PinRow {
                auxiliary_only: true,
                ..line_pin("s1", "main", "claude-haiku-4-5", g)
            },
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let g = common::generation(&parts.store, "desktop");
    for (id, model, class, at) in [
        ("r1", "claude-opus-5[1m]", "main", now - 60_000),
        ("r2", "claude-haiku-4-5", "auxiliary", now - 30_000),
    ] {
        parts
            .ledger
            .record(classed_request(id, "s1", "main", model, Some(class), at, g));
    }
    register_with_the_engine(&parts, "s1", &[]).await;
    assert_eq!(
        lines(&parts, &cookie, &[("s1", 2)]).await,
        [serde_json::json!([line("main", "claude-opus-5", now - 50, Some("busy"))])],
        "main's line is its conversation pin, and the newer pin of its auxiliary request draws none"
    );
}

/// A pin only auxiliary requests have used draws no line, whether another agent of the session holds a conversation
/// pin or none does.
#[tokio::test]
async fn a_pin_whose_requests_are_all_auxiliary_draws_no_line() {
    let now = common::NOW;
    let parts = lines_harness(|g| {
        vec![
            phoebis_store::PinRow {
                auxiliary_only: true,
                ..line_pin("s-borrowed", "main", "claude-haiku-4-5", g)
            },
            line_pin("s-borrowed", "worker-1", "claude-opus-5", g),
            phoebis_store::PinRow {
                auxiliary_only: true,
                ..line_pin("s-auxiliary", "main", "claude-opus-5", g)
            },
        ]
    })
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    let g = common::generation(&parts.store, "desktop");
    for (id, session, agent, model, class, at) in [
        (
            "r1",
            "s-borrowed",
            "worker-1",
            "claude-opus-5[1m]",
            "subagent",
            now - 60_000,
        ),
        (
            "r2",
            "s-borrowed",
            "main",
            "claude-haiku-4-5",
            "auxiliary",
            now - 30_000,
        ),
        (
            "r3",
            "s-auxiliary",
            "main",
            "claude-opus-5[1m]",
            "auxiliary",
            now - 30_000,
        ),
    ] {
        parts.ledger.record(classed_request(
            id,
            session,
            agent,
            model,
            Some(class),
            at,
            g,
        ));
    }
    register_with_the_engine(&parts, "s-borrowed", &["worker-1"]).await;
    register_with_the_engine(&parts, "s-auxiliary", &[]).await;
    assert_eq!(
        lines(&parts, &cookie, &[("s-borrowed", 2), ("s-auxiliary", 1)]).await,
        [
            serde_json::json!([line("worker-1", "claude-opus-5", now - 5, Some("busy"))]),
            serde_json::json!([]),
        ],
        "main sent auxiliary requests alone, so its pin draws no line beside the subagent's and none where it is the one \
         pin"
    );
}

/// A raw caller sends no `x-claude-code-request-class` and runs no companion, so its unclassed pin is a conversation
/// pin, which its row lists as main's line within the non-companion timeout.
#[tokio::test]
async fn a_raw_callers_session_with_no_companion_lists_its_pin_as_mains_line() {
    let now = common::NOW;
    let parts = lines_harness(|g| vec![line_pin("s-raw", "main", "claude-opus-5", g)]).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let g = common::generation(&parts.store, "desktop");
    for (id, model, at) in [
        ("r1", "claude-opus-5", now - 60_000),
        ("r2", "claude-opus-5[1m]", now - 30_000),
    ] {
        parts
            .ledger
            .record(classed_request(id, "s-raw", "main", model, None, at, g));
    }
    assert_eq!(
        lines(&parts, &cookie, &[("s-raw", 2)]).await,
        [serde_json::json!([line(
            "main",
            "claude-opus-5",
            now - 5,
            None
        )])],
        "the unclassed session's pin is main's line on a row no companion reports"
    );
}

/// The mod registers main's session again under its instance after the session's end, as a resume does: main is busy
/// awaiting its next request with its earlier request on record, so its recipe stands and its pin draws its line.
#[tokio::test]
async fn a_main_registered_again_after_its_sessions_end_reads_registered_on_its_row_its_line_and_its_recipe(
) {
    let parts = lines_harness(|g| vec![line_pin("s1", "main", "claude-opus-5", g)]).await;
    let cookie = login(&parts.app, "admin-secret").await;
    register_with_the_engine(&parts, "s1", &[]).await;
    let secret = parts
        .desktop_secret
        .as_deref()
        .expect("the harness created the key desktop");
    for (sequence, event) in [(2, "session_end"), (3, "session_start")] {
        common::report_as_the_mod(
            &parts.keepalive,
            secret,
            serde_json::json!({
                "protocol": 1,
                "event": event,
                "session_id": "s1",
                "instance_id": "i1",
                "sequence": sequence
            }),
        )
        .await;
    }
    let listed = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1")
        .unwrap_or_else(|| panic!("no row for s1: {listed}"));
    let recipes = json(get(&parts.app, "/admin/keepalive/recipes", Some(&cookie)).await).await;
    let recipe = recipes["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "s1" && r["agent_id"] == "main")
        .unwrap_or_else(|| panic!("no recipe for main: {recipes}"));
    assert_eq!(
        (
            row["lifecycle"].as_str(),
            row["agent_contexts"][0]["agent_id"].as_str(),
            row["agent_contexts"][0]["lifecycle"].as_str(),
            recipe["agent_state"].as_str()
        ),
        (
            Some("registered"),
            Some("main"),
            Some("registered"),
            Some("registered")
        ),
        "the row, main's line and main's recipe read one word: {row} {recipe}"
    );
}

/// A deleted key's session outlives the key in the engine until the engine's next tick ends it: the row must read
/// as historical, with no agents, companion, facts or recipe.
#[tokio::test]
async fn a_deleted_keys_session_is_historical_with_no_companion() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    let registered = common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 1
        }),
    )
    .await;
    // The session and its live agent each report a fact, so the row behind the deletion has one of each to leave out.
    for body in [
        serde_json::json!({
            "protocol": 1,
            "event": "session_facts",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 2,
            "title": "Checkout retry"
        }),
        serde_json::json!({
            "protocol": 1,
            "event": "agent_facts",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 3,
            "agent_id": "main",
            "request_generation": registered["request_generation"],
            "name": "lead"
        }),
    ] {
        common::report_as_the_mod(&parts.keepalive, &secret, body).await;
    }
    // The row reads the stored sets, so both land ahead of the read.
    parts.keepalive.flush_persisted().await;
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
        row["facts"]["title"], "Checkout retry",
        "the live row answers the session's title"
    );
    assert_eq!(
        row["agent_facts"],
        serde_json::json!([{
            "agent_id": "main",
            "agent_type": null,
            "description": null,
            "name": "lead"
        }]),
        "the live row answers main's name"
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
    assert!(
        row["facts"]["title"].is_null(),
        "the historical row answers no title: {row}"
    );
    assert_eq!(
        row["agent_facts"],
        serde_json::json!([]),
        "the historical row answers no agent's facts"
    );
    let held = |engine: &phoebis_session_engine::Engine| {
        engine.sessions().iter().any(|s| s.session_id == "s1")
    };
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

/// The console decodes a recipe's cause against the cause array of its API types, the array its `RecipeCause` union
/// derives from, so a word the engine serializes that the array lacks would read as no cause at all, and a word the
/// array keeps that the engine dropped is a label nothing can reach.
#[test]
fn every_engine_reason_is_a_cause_word_the_console_types_list() {
    use phoebis_session_engine::Reason;
    use std::collections::BTreeSet;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/api/types.ts");
    let types = std::fs::read_to_string(&path).unwrap();
    let start = types
        .find("export const RECIPE_CAUSE_VALUES =")
        .expect("types.ts declares the cause array");
    let array = &types[start..];
    let array = &array[..array.find(']').expect("the array ends with a bracket")];
    let listed: BTreeSet<&str> = array.split('\'').skip(1).step_by(2).collect();
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
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    let start = |sequence: u64, event: &str| {
        serde_json::json!({
            "protocol": 1,
            "event": event,
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": sequence
        })
    };
    common::report_as_the_mod(&parts.keepalive, &secret, start(1, "session_start")).await;
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
    assert_eq!(
        listed["sessions"][0]["lifecycle"].as_str(),
        Some("registered"),
        "main has sent no request since its registration, so the row words it registered rather than busy"
    );
    common::report_as_the_mod(&parts.keepalive, &secret, start(2, "session_end")).await;
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
    let secret = common::created_key_secret(
        &parts.app,
        &cookie,
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
    )
    .await;
    common::report_as_the_mod(
        &parts.keepalive,
        &secret,
        serde_json::json!({
            "protocol": 1,
            "event": "session_start",
            "session_id": "s1",
            "instance_id": "i1",
            "sequence": 1
        }),
    )
    .await;
    phoebis_session_engine::spawn_observer(parts.keepalive.clone());
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
        .header("user-agent", "claude-cli/2.1.269 (external, sdk-cli)")
        .header("x-claude-code-session-id", "s1")
        .header("x-claude-code-agent-id", "main")
        .header("x-claude-code-request-class", "main")
        .body(axum::body::Body::from(
            r#"{"model":"claude-sonnet-4-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}],"metadata":{"user_id":"{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}"}}"#,
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
    let main = || {
        parts
            .keepalive
            .recipes()
            .into_iter()
            .find(|line| line.session_id == "s1" && line.agent_id == "main")
    };
    let due = fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        main().and_then(|line| line.next_probe_at)
    })
    .await;
    parts
        .clock_cell
        .store(due + 1_000, std::sync::atomic::Ordering::SeqCst);
    parts.keepalive.step().await;
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (parts.keepalive.stats().warming.running_probes == 0).then_some(())
    })
    .await;
    assert!(main().unwrap().probes_today >= 1, "the probe ran");
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

#[tokio::test]
async fn the_accounts_companion_id_is_the_allowance_routes_pseudonym() {
    use tower::ServiceExt;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A label apart from the id, so a pseudonym drawn from the label reads another id.
    let created = post(
        &parts.app,
        "/admin/accounts",
        r#"{"id":"console","kind":"api_key","label":"Console","secret":"sk-ant-api03-NEW"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), 201);
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"console","priority":0}]}"#, Some(&cookie)).await;
    let key = json(
        post(
            &parts.app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    let request = axum::http::Request::get("/v0/companion/allowance?session_id=s1")
        .header(
            "authorization",
            format!("Bearer {}", key["secret"].as_str().unwrap()),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let allowance = json(
        phoebis_session_engine::companion_router(parts.keepalive.clone())
            .oneshot(request)
            .await
            .unwrap(),
    )
    .await;
    let account = json(get(&parts.app, "/admin/accounts/console", Some(&cookie)).await).await;
    assert_eq!(
        allowance["accounts"]
            .as_array()
            .map(|accounts| accounts.iter().map(|a| a["id"].clone()).collect::<Vec<_>>()),
        Some(vec![account["companion_id"].clone()]),
        "{allowance}"
    );
}
