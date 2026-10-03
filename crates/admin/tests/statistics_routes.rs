//! Pricing, settings, system, quota, traffic, sessions, and the live routes.
mod common;
use std::time::Duration;

use common::{delete, get, json, login, post, wait_json};
use futures::StreamExt;
use phoebis_bus::{Event, Kind, Origin, Phase, RequestObserved, RequestShape, ResponseChunk};
use phoebis_ledger::{AttemptRecord, EventRecord, Record, RequestRecord};

const HOUR: i64 = 3_600_000;

fn record(id: &str, at: i64, model: &str, session: &str) -> Record {
    record_at(id, at, model, session, 0)
}

/// `key_generation` matters only to a caller that also plants a pin: the sessions view groups a request row
/// under it, and a row left at the default of 0 never joins a pin under the key's real generation.
fn record_at(id: &str, at: i64, model: &str, session: &str, key_generation: i64) -> Record {
    Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            key_generation,
            pool_id: Some("main".into()),
            session_id: Some(session.into()),
            model: model.into(),
            canonical_model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: at,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..Default::default()
        }],
        bodies: None,
    }
}

#[tokio::test]
async fn settings_and_system_show_effective_values_ledger_health_and_gaps_without_secrets() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let settings = json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await;
    assert_eq!(
        (
            settings["retention"]["ledger_days"].as_u64(),
            settings["retention"]["body_capture_max_hours"].as_u64(),
            settings["retention"]
                .as_object()
                .map(|retention| retention.len())
        ),
        (Some(90), Some(72), Some(3)),
        "the samples follow the ledger's own window, so retention has three members"
    );
    assert!(
        settings["compatibility"].is_null() && settings["quota"].is_null(),
        "neither removed block is answered: {settings}"
    );
    assert_eq!(
        (
            settings["routing"].as_object().map(|routing| routing.len()),
            settings["keepalive"]
                .as_object()
                .map(|keepalive| keepalive.len()),
            settings["sessions"]["expire_after_seconds"].as_u64(),
            settings["listeners"]["metrics"].as_str()
        ),
        (Some(4), Some(3), Some(86_400), Some("127.0.0.1:9464"))
    );
    assert_eq!(settings["pricing"]["version"], "2026-10-01");
    assert_eq!(settings["pricing"]["source"], "shipped");
    let tables = &settings["plan_tables"];
    let cost = &tables["cost"];
    assert_eq!(cost["effective"], "2026-10-01");
    assert_eq!(
        cost["tiers"]["default_claude_pro"]["five_hour"].as_f64(),
        Some(0.05)
    );
    assert!(cost["tiers"]["default_claude_pro"]["seven_day"].is_null());
    assert_eq!(
        cost["windows"]["five_hour"]["claude-opus-5-5"]["input"].as_f64(),
        Some(0.01461)
    );
    assert!(cost["windows"]["five_hour"]["claude-opus-5"]["input"].is_null());
    assert!(cost["windows"]["seven_day"]["claude-opus-5-5"]["input"].is_null());
    assert_eq!(cost["bands"][0]["factor"].as_f64(), Some(1.435));
    assert_eq!(
        (
            cost.get("tier"),
            cost.get("version"),
            cost["bands"][0].get("measured"),
            tables.get("tiers")
        ),
        (None, None, None, None),
        "the one table answers its v2 shape alone: {tables}"
    );
    let text = settings.to_string();
    assert!(
        !text.contains("admin-secret") && !text.contains("secret_env"),
        "no secret and no secret's variable name"
    );
    parts
        .db
        .insert_event(&phoebis_ledger::EventRecord {
            at: common::NOW - HOUR,
            kind: "ledger_gap".into(),
            detail: Some(r#"{"dropped":7}"#.into()),
            ..Default::default()
        })
        .unwrap();
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (
            system["ledger"]["schema_version"].as_u64(),
            system["ledger"]["dropped"].as_u64(),
            system["ledger"]["channel_depth"].as_u64()
        ),
        (Some(26), Some(0), Some(4_096)),
        "the ledger schema is at the migration that counts an incomplete answer and keeps the upstream message id"
    );
    assert_eq!(system["router"]["command_depth"].as_u64(), Some(4_096));
    let dropped = system["router"]["dropped_reports"].as_object().unwrap();
    assert_eq!(
        dropped.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "account_state",
            "affinity",
            "cooldown",
            "credits",
            "member_spend",
            "overage",
            "pin_invalidated",
            "pin_released",
            "pin_renewed",
            "sample",
            "usage",
        ],
        "every kind of routing report, in name order"
    );
    assert!(
        dropped.values().all(|n| n.as_u64() == Some(0)),
        "nothing refused by a router that kept up: {dropped:?}"
    );
    assert_eq!(system["gaps"][0]["detail"]["dropped"].as_u64(), Some(7));
    assert!(
        system["bus"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["subscriber"] == "ledger" && b["dropped"].as_u64() == Some(0))
            && system["bus"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["subscriber"] == "live")
    );
    assert_eq!(system["live"]["clients"].as_u64(), Some(0));
    assert_eq!(system["storage_ok"], true);
    assert_eq!(
        (&system["ready"], &system["dead"]),
        (&serde_json::json!(true), &serde_json::json!([])),
        "the harness's router task, ledger consumer and store are all alive"
    );
}

#[tokio::test]
async fn a_stopped_ledger_reads_not_ready_on_the_system_document() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.clone().shutdown().await.unwrap();
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (&system["ready"], &system["dead"], &system["storage_ok"]),
        (
            &serde_json::json!(false),
            &serde_json::json!(["ledger"]),
            &serde_json::json!(true)
        ),
        "the writer closed its channel while the router task and the store still answer: {system}"
    );
}

#[tokio::test]
async fn a_store_whose_read_fails_reads_not_ready_on_the_system_document_until_it_answers_again() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.store.debug_fail_next_schema_version();
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (&system["ready"], &system["dead"], &system["storage_ok"]),
        (
            &serde_json::json!(false),
            &serde_json::json!(["store"]),
            &serde_json::json!(false)
        ),
        "the store's read failed while the ledger and the router task still answer: {system}"
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (&system["ready"], &system["dead"], &system["storage_ok"]),
        (
            &serde_json::json!(true),
            &serde_json::json!([]),
            &serde_json::json!(true)
        ),
        "each answer reads the store afresh, so the next one is ready again: {system}"
    );
}

#[tokio::test]
async fn readiness_reads_the_ledger_dead_when_its_bus_subscriber_is_absent_or_panicked() {
    let parts = common::admin_parts_full().await;
    let readiness = |bus: phoebis_bus::Bus| phoebis_admin::Readiness {
        bus,
        ledger: parts.ledger.clone(),
        router: parts.router.clone(),
        store: parts.store.clone(),
    };
    assert_eq!(
        readiness(parts.bus.clone()).dead(),
        Vec::<&str>::new(),
        "the harness's own bus carries the ledger's live subscriber"
    );
    let bus = phoebis_bus::Bus::new();
    let _live = bus.subscribe("live");
    assert_eq!(
        readiness(bus.clone()).dead(),
        vec!["ledger"],
        "a bus the ledger never subscribed to"
    );
    let assembler = bus.subscribe("ledger");
    assert_eq!(
        readiness(bus.clone()).dead(),
        Vec::<&str>::new(),
        "a `ledger` subscriber with no panic counted"
    );
    phoebis_bus::spawn_subscriber(assembler, |_| async { panic!("the assembler died") })
        .await
        .unwrap();
    assert_eq!(
        readiness(bus).dead(),
        vec!["ledger"],
        "a `ledger` subscriber whose task panicked, while the writer's channel stays open"
    );
}

#[tokio::test]
async fn the_price_routes_answer_the_fast_cells_beside_the_standard_ones() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let settings = json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await;
    let pricing = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    let costs = json(get(&parts.app, "/admin/sessions/costs", Some(&cookie)).await).await;
    for (route, models) in [
        ("settings", &settings["pricing"]["models"]),
        ("pricing", &pricing["models"]),
        ("sessions/costs", &costs["models"]),
    ] {
        let row = |model: &str| {
            models
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["model"] == model)
                .cloned()
                .unwrap_or_else(|| panic!("{route} lists {model}: {models}"))
        };
        let opus = row("claude-opus-5-5");
        assert_eq!(
            (&opus["input_per_m"], &opus["output_per_m"]),
            (&serde_json::json!(4.0), &serde_json::json!(20.0)),
            "{route}: the standard cells stay where they were"
        );
        assert_eq!(
            opus["fast"],
            serde_json::json!({
                "input_per_m": 8.0,
                "cache_write_5m_per_m": 10.0,
                "cache_write_1h_per_m": 16.0,
                "cache_read_per_m": 0.4,
                "output_per_m": 40.0,
            }),
            "{route}: a model that serves fast mode answers its five fast cells"
        );
        assert_eq!(
            row("claude-sonnet-5").get("fast"),
            Some(&serde_json::Value::Null),
            "{route}: a model without them answers null"
        );
    }
}

#[tokio::test]
async fn quota_history_pool_traffic_and_sessions_read_the_ledger() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let bucket = phoebis_ledger::bucket_of(now - HOUR);
    parts
        .db
        .insert_samples(
            &[
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "5h".into(),
                    utilization: Some(0.4),
                    reset_at: Some(now + HOUR),
                    source: "header".into(),
                    observed_at: None,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: bucket + 300,
                    window: "5h".into(),
                    utilization: Some(0.5),
                    reset_at: Some(now + HOUR),
                    source: "poll".into(),
                    observed_at: None,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "fable".into(),
                    utilization: Some(0.1),
                    reset_at: None,
                    source: "poll".into(),
                    observed_at: None,
                },
            ],
            Some(&phoebis_ledger::ExtraUsageSampleRecord {
                account_id: "business".into(),
                bucket,
                is_enabled: true,
                in_use: None,
                monthly_limit: Some(5_000),
                used_credits: Some(10),
                utilization: Some(0.002),
                currency: Some("USD".into()),
                disabled_reason: None,
                overage_reset: None,
                source: "poll".into(),
            }),
        )
        .unwrap();
    let quota = json(
        get(
            &parts.app,
            "/admin/accounts/business/quota?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let windows = quota["windows"].as_array().unwrap();
    assert_eq!(
        windows
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["5h", "fable"]
    );
    assert_eq!(windows[0]["samples"].as_array().unwrap().len(), 2);
    assert_eq!(
        (
            windows[0]["samples"][0]["at"].as_i64(),
            windows[0]["samples"][0]["utilization"].as_f64(),
            windows[0]["samples"][0]["source"].as_str()
        ),
        (Some(bucket * 1_000), Some(0.4), Some("header"))
    );
    assert_eq!(quota["extra_usage"][0]["used_credits"].as_u64(), Some(10));
    assert_eq!(
        get(&parts.app, "/admin/accounts/nope/quota", Some(&cookie))
            .await
            .status(),
        404
    );
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
    for i in 0..5 {
        parts.ledger.record(record(
            &format!("t{i}"),
            now - i * HOUR,
            "claude-sonnet-5",
            &format!("s{}", i % 2),
        ));
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("t4").await.unwrap()
    })
    .await;
    let traffic = json(
        get(
            &parts.app,
            "/admin/pools/main/traffic?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            traffic["pool_id"].as_str(),
            traffic["bucket_seconds"].as_i64(),
            traffic["buckets"].as_array().unwrap().len()
        ),
        (Some("main"), Some(3_600), 25),
        "a 24-hour range off the hour boundary touches 25 hour buckets"
    );
    let total: u64 = traffic["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["requests"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 5);
    assert!(
        traffic["buckets"].as_array().unwrap().last().unwrap()["tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        get(&parts.app, "/admin/pools/nope/traffic", Some(&cookie))
            .await
            .status(),
        404
    );
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let rows = sessions["sessions"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        2,
        "one row per (key, session) seen in the ledger, no pin needed"
    );
    assert_eq!(
        (
            rows[0]["session_id"].as_str(),
            rows[0]["requests"].as_u64(),
            rows[0]["last_request_at"].as_i64(),
            rows[0]["pinned"].as_bool(),
            rows[0]["account_id"].as_str()
        ),
        (
            Some("s0"),
            Some(3),
            Some(now),
            Some(false),
            Some("business")
        )
    );
    assert!(rows[0]["epoch"].is_null() && rows[0]["created_at"].is_null());
}

/// The request count on a session row is the ledger's. A pin the ledger has no row for has no count, and
/// saying zero would read as "this session made no requests" rather than "nothing was recorded".
#[tokio::test]
async fn a_pin_the_ledger_has_no_row_for_reports_no_request_count() {
    // The router loads the pins at spawn and purges any whose key names no pool, so the pool, the key, and
    // the pins are written through one harness and the store is handed to the one under test.
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
    let now = common::NOW;
    for session in ["served", "silent"] {
        seed.store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "desktop".into(),
                session_id: session.into(),
                agent_id: "main".into(),
                model: "claude-sonnet-5".into(),
                account_id: "business".into(),
                epoch: 1,
                created_at: now - 10,
                last_used_at: now - 5,
                key_generation: common::generation(&seed.store, "desktop"),
                incarnation: 1,
                burn: phoebis_store::PinBurn::default(),
                burn_at: None,
                burn_banded: phoebis_store::PinBurn::default(),
                last_input_tokens: 0,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
                lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            })
            .unwrap();
    }
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let generation = common::generation(&seed.store, "desktop");
    for i in 0..3 {
        parts.ledger.record(record_at(
            &format!("p{i}"),
            now - i * HOUR,
            "claude-sonnet-5",
            "served",
            generation,
        ));
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("p2").await.unwrap()
    })
    .await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = |session: &str| {
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["session_id"] == session)
            .unwrap_or_else(|| panic!("{session} is listed"))
            .clone()
    };
    assert_eq!(
        (
            row("served")["requests"].as_u64(),
            row("served")["pinned"].as_bool()
        ),
        (Some(3), Some(true)),
        "a pin the ledger saw carries the ledger's count"
    );
    assert!(
        row("silent")["requests"].is_null(),
        "a pin with no ledger row has no count to report"
    );
    assert_eq!(row("silent")["pinned"].as_bool(), Some(true));
}

#[tokio::test]
async fn the_live_route_is_gated_and_every_session_receives_the_preview() {
    let parts = common::admin_parts_full().await;
    let addr = common::serve(parts.app.clone()).await;
    let admin = login(&parts.app, "admin-secret").await;
    let connect = |cookie: Option<String>| {
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{addr}/admin/live"),
            )
            .unwrap();
        if let Some(cookie) = cookie {
            request
                .headers_mut()
                .insert("cookie", format!("phoebis_admin={cookie}").parse().unwrap());
        }
        tokio_tungstenite::connect_async(request)
    };
    assert!(connect(None).await.is_err(), "no session, no upgrade");
    // A pinned origin: an authenticated handshake from another origin is refused by the gate.
    let pinned = common::admin_parts_full_with_origin("https://phoebis.example").await;
    let pinned_addr = common::serve(pinned.app.clone()).await;
    let pinned_cookie =
        common::login_from(&pinned.app, "admin-secret", "https://phoebis.example").await;
    let mut foreign =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{pinned_addr}/admin/live"
        ))
        .unwrap();
    foreign.headers_mut().insert(
        "cookie",
        format!("phoebis_admin={pinned_cookie}").parse().unwrap(),
    );
    foreign
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    let refused = tokio_tungstenite::connect_async(foreign).await;
    assert!(
        matches!(refused, Err(tokio_tungstenite::tungstenite::Error::Http(ref r)) if r.status() == 403),
        "a foreign origin is refused before the upgrade: {refused:?}"
    );
    let mut same = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{pinned_addr}/admin/live"),
    )
    .unwrap();
    same.headers_mut().insert(
        "cookie",
        format!("phoebis_admin={pinned_cookie}").parse().unwrap(),
    );
    same.headers_mut()
        .insert("origin", "https://phoebis.example".parse().unwrap());
    let (mut same_ws, _) = tokio_tungstenite::connect_async(same).await.unwrap();
    assert_eq!(
        common::next_text(&mut same_ws).await["kind"],
        "snapshot",
        "the configured origin is admitted"
    );
    let _ = same_ws.close(None).await;
    let (mut admin_ws, _) = connect(Some(admin.clone())).await.unwrap();
    assert_eq!(common::next_text(&mut admin_ws).await["kind"], "snapshot");
    // Straight onto the bus: a capturing request and one text chunk.
    parts.bus.publish(Event::RequestObserved(RequestObserved {
        request_id: "live-1".into(),
        origin: Origin::Client,
        kind: Kind::Inference,
        key_id: Some("desktop".into()),
        key_generation: 0,
        pool_id: Some("main".into()),
        session_id: Some("s1".into()),
        agent_id: Some("main".into()),
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: true,
        headers: vec![],
        body: bytes::Bytes::from_static(b"{}"),
        body_capture_hours: 24,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: phoebis_bus::ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        received_at: common::NOW,
    }));
    parts.bus.publish(Event::ResponseChunk(ResponseChunk { request_id: "live-1".into(), attempt: 1, index: 0, body: bytes::Bytes::from_static(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"secret prompt text\"}}\n\n"), phase: Some(Phase::FirstContentDelta) }));
    let admin_seen = common::deltas_until(&mut admin_ws, "preview").await;
    assert!(admin_seen
        .iter()
        .any(|d| d["type"] == "preview" && d["chunk"] == "secret prompt text"));
    let past = json(
        get(
            &parts.app,
            &format!(
                "/admin/live/snapshot?from={}&to={}",
                common::NOW - 1,
                common::NOW + 1
            ),
            Some(&admin),
        )
        .await,
    )
    .await;
    assert_eq!(past["entries"][0]["id"], "live-1");
    assert!(
        past["entries"][0]["preview"].is_null(),
        "a past read carries no preview"
    );
    let by_instant = json(
        get(
            &parts.app,
            &format!("/admin/live/snapshot?at={}", common::NOW + 1),
            Some(&admin),
        )
        .await,
    )
    .await;
    assert_eq!(
        by_instant["entries"], past["entries"],
        "at= is the 30-minute strip ending at that instant"
    );
    assert_eq!(
        get(
            &parts.app,
            &format!("/admin/live/snapshot?at={}&to={}", common::NOW, common::NOW),
            Some(&admin)
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        get(
            &parts.app,
            &format!("/admin/live/snapshot?at={}", i64::MIN),
            Some(&admin)
        )
        .await
        .status(),
        200,
        "the window an instant at the bound asks for is clamped, not wrapped"
    );
    assert_eq!(
        get(&parts.app, "/admin/live", Some(&admin)).await.status(),
        400,
        "a plain GET is not an upgrade"
    );
    let _ = admin_ws.close(None).await;
}

#[tokio::test]
async fn the_live_socket_closes_when_its_session_ends() {
    let parts = common::admin_parts_full().await;
    let addr = common::serve(parts.app.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{addr}/admin/live"
        ))
        .unwrap();
    request
        .headers_mut()
        .insert("cookie", format!("phoebis_admin={cookie}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(common::next_text(&mut ws).await["kind"], "snapshot");
    assert_eq!(
        delete(&parts.app, "/admin/session", Some(&cookie))
            .await
            .status(),
        204
    );
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match ws.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => break frame,
                Some(Ok(_)) => continue,
                other => panic!("the socket ended without a close frame: {other:?}"),
            }
        }
    })
    .await
    .expect("the writer's tick notices the ended session within five seconds");
    let frame = closed.expect("a close frame with a reason");
    assert_eq!(u16::from(frame.code), 1008);
    assert_eq!(frame.reason, "session ended");
}

#[tokio::test]
async fn the_sixty_fifth_live_socket_is_refused_until_one_closes() {
    let parts = common::admin_parts_full().await;
    let addr = common::serve(parts.app.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let connect = || {
        let cookie = cookie.clone();
        async move {
            let mut request =
                tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                    format!("ws://{addr}/admin/live"),
                )
                .unwrap();
            request
                .headers_mut()
                .insert("cookie", format!("phoebis_admin={cookie}").parse().unwrap());
            tokio_tungstenite::connect_async(request).await
        }
    };
    let mut sockets = Vec::new();
    for _ in 0..phoebis_live::MAX_SOCKETS {
        let (mut ws, _) = connect().await.unwrap();
        assert_eq!(common::next_text(&mut ws).await["kind"], "snapshot");
        sockets.push(ws);
    }
    match connect().await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 503);
            let body = response.body().clone().unwrap_or_default();
            assert!(String::from_utf8_lossy(&body).contains("live feed at capacity"));
        }
        other => panic!("the 65th upgrade is refused before the handshake: {other:?}"),
    }
    let _ = sockets.pop().unwrap().close(None).await;
    // The slot frees once the server notices the close, which is not synchronous with sending it.
    let (mut admitted, _) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pair) = connect().await {
                return pair;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a slot frees once a socket closes");
    assert_eq!(common::next_text(&mut admitted).await["kind"], "snapshot");
}

#[tokio::test]
async fn a_non_numeric_limit_on_the_request_log_is_refused() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let refused = get(&parts.app, "/admin/requests?limit=abc", Some(&cookie)).await;
    assert_eq!(refused.status(), 400);
    assert_eq!(json(refused).await["error"], "limit is not a number");
    assert_eq!(
        get(&parts.app, "/admin/requests?limit=5", Some(&cookie))
            .await
            .status(),
        200
    );
}

/// Whether the point answers `field` as null: a key the route left out indexes to null too, which a shape without
/// the field would pass.
fn null_field(point: &serde_json::Value, field: &str) -> bool {
    point.get(field) == Some(&serde_json::Value::Null)
}

/// Stores the Max 20x tier on `business`, as a profile read does.
fn set_max_20x(store: &phoebis_store::Store) {
    store
        .set_profile(
            "business",
            r#"{"account":{"uuid":"u","email_address":"business@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_20x".into()),
                since: Some(1_000_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
}

#[tokio::test]
async fn the_burn_route_answers_each_subscription_points_share_of_both_windows() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    // An hour of a million Opus 5.5 input tokens on `business`.
    let mut opus = record("burn-1", now - HOUR, "claude-opus-5-5", "s1");
    if let Record::Request { attempts, .. } = &mut opus {
        attempts[0].input_tokens = Some(1_000_000);
        attempts[0].output_tokens = None;
    }
    parts.ledger.record(opus);
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("burn-1").await.unwrap()
    })
    .await;
    let business = |v: &serde_json::Value| {
        v["points"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["group"] == "business" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
            .cloned()
            .unwrap_or_else(|| panic!("a bucket carries the recorded attempt: {v}"))
    };
    let burn_url = "/admin/analytics/burn?range=24h&group_by=account";
    // No tier on file yet: the upstream is a subscription whose window no table sizes.
    let tierless = business(&json(get(&parts.app, burn_url, Some(&cookie)).await).await);
    assert!(
        tierless["unit"] == "window_share"
            && null_field(&tierless, "five_hour_share")
            && null_field(&tierless, "seven_day_share"),
        "a subscription with no tier prices no share: {tierless}"
    );
    set_max_20x(&parts.store);
    let burn = json(get(&parts.app, burn_url, Some(&cookie)).await).await;
    assert_eq!(burn["group_by"], "account");
    let counts_every_token = |v: &serde_json::Value| {
        v["points"].as_array().unwrap().iter().all(|p| {
            let sides = p["cached_tokens"].as_u64().unwrap_or(0)
                + p["uncached_tokens"].as_u64().unwrap_or(0);
            p["tokens"].as_u64().is_some_and(|tokens| tokens >= sides)
        })
    };
    assert!(
        counts_every_token(&burn),
        "each point counts every token type, its cached and uncached input among them: {burn}"
    );
    let point = business(&burn);
    let tables = phoebis_ledger::PlanTables::shipped();
    let hour = point["at"].as_i64().unwrap();
    let band = tables.cost.band_factor(phoebis_routing::FIVE_HOUR, hour);
    assert!(
        (point["five_hour_share"].as_f64().unwrap() - 0.01461 * band).abs() < 1e-9,
        "a million Opus 5.5 input tokens read the table's 5-hour share on Max 20x: {point}"
    );
    assert!(
        null_field(&point, "seven_day_share"),
        "every 7-day cell is unmeasured, so the 7-day share reads null: {point}"
    );
    assert_eq!(
        (&point["unit"], &point["usd"]),
        (&serde_json::json!("window_share"), &serde_json::json!(0.0)),
        "a subscription spends no dollars: {point}"
    );
    let mut keys: Vec<&str> = point
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "at",
            "cached_tokens",
            "five_hour_share",
            "group",
            "seven_day_share",
            "tokens",
            "uncached_tokens",
            "unit",
            "usd"
        ],
        "a point carries its shares with no flag beside them: {point}"
    );
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "burn-2".into(),
            received_at: now - HOUR,
            finished_at: Some(now - HOUR + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s2".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "burn-2".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            started_at: now - HOUR,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            estimated_usd: Some(0.01),
            ..Default::default()
        }],
        bodies: None,
    });
    let with_direct = wait_json(&parts.app, burn_url, &cookie, |v| {
        v["points"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["group"] == "direct" && p["usd"].as_f64().unwrap_or(0.0) > 0.0)
    })
    .await;
    assert!(
        with_direct["points"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["group"] == "direct")
            .all(|p| null_field(p, "five_hour_share")
                && null_field(p, "seven_day_share")
                && p["unit"] == "usd"),
        "an api_key account has no window to read a share of"
    );
    let by_key = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=key",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        counts_every_token(&by_key),
        "under every grouping: {by_key}"
    );
    let keyed = by_key["points"].as_array().unwrap();
    assert!(
        keyed
            .iter()
            .any(|p| p["uncached_tokens"].as_u64().unwrap_or(0) >= 1_000_000),
        "the key's bucket carries the subscription's tokens: {by_key}"
    );
    assert!(
        keyed.iter().all(|p| null_field(p, "five_hour_share")
            && null_field(p, "seven_day_share")
            && null_field(p, "unit")),
        "the shares are an account-grouped figure alone, since shares of different accounts' windows do not add"
    );
    assert_eq!(
        get(
            &parts.app,
            "/admin/analytics/burn?group_by=pool",
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "pool is not one of the burn route's own group_by values"
    );
}

#[tokio::test]
async fn a_fast_answer_moves_a_subscription_groups_dollars_and_no_share() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    set_max_20x(&parts.store);
    let mut fast = record("fast-1", common::NOW - HOUR, "claude-opus-5", "s1");
    if let Record::Request { attempts, .. } = &mut fast {
        attempts[0].input_tokens = Some(1_000_000);
        attempts[0].output_tokens = None;
        attempts[0].speed = Some("fast".into());
    }
    parts.ledger.record(fast);
    let burn_url = "/admin/analytics/burn?range=24h&group_by=account";
    let burn = wait_json(&parts.app, burn_url, &cookie, |v| {
        v["points"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["group"] == "business" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
    })
    .await;
    let point = burn["points"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["group"] == "business" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
        .cloned()
        .unwrap();
    assert!(
        (point["usd"].as_f64().unwrap() - 10.0).abs() < 1e-9,
        "a million Opus 5 input tokens served fast read the $10 fast cell in dollars: {point}"
    );
    assert_eq!(
        (
            &point["unit"],
            &point["five_hour_share"],
            &point["seven_day_share"],
        ),
        (
            &serde_json::json!("window_share"),
            &serde_json::json!(0.0),
            &serde_json::json!(0.0),
        ),
        "the account still charts in its window unit at a zero share, so it reads as priced beside its dollars: \
         {point}"
    );
}

#[tokio::test]
async fn the_burn_route_ranks_sessions_by_tokens_and_prices_each_by_its_accounts_kind() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A tier on file, so the shares the session grouping leaves null are the grouping's and not a missing tier's.
    set_max_20x(&parts.store);
    let now = common::NOW;
    let session_request = |id: &str,
                           at: i64,
                           session: &str,
                           account: &str,
                           input: u64,
                           output: u64,
                           estimated_usd: f64| Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some(session.into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: if account == "business" {
                "oauth"
            } else {
                "api_key"
            }
            .into(),
            started_at: at,
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            output_tokens: Some(output),
            estimated_usd: Some(estimated_usd),
            ..Default::default()
        }],
        bodies: None,
    };
    // The most tokens, split an hour apart so the series carries it in two buckets. Each attempt carries a
    // nonzero `estimated_usd` too, so the assertion below proves the oauth branch ignores it rather than
    // merely never having seen a value to add.
    parts.ledger.record(session_request(
        "r-big-1",
        now - HOUR - 100,
        "s-big",
        "business",
        2_000,
        200,
        5.0,
    ));
    parts.ledger.record(session_request(
        "r-big-2",
        now - 100,
        "s-big",
        "business",
        2_000,
        200,
        5.0,
    ));
    // Fewer tokens than "s-big", more than "s-small", on an api_key account.
    parts.ledger.record(session_request(
        "r-mid",
        now - 100,
        "s-mid",
        "direct",
        1_000,
        100,
        0.05,
    ));
    // The fewest tokens: last in the ranking.
    parts.ledger.record(session_request(
        "r-small",
        now - 100,
        "s-small",
        "business",
        10,
        1,
        0.0,
    ));
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-mid").await.unwrap()
    })
    .await;
    let burn = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=session",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let points = burn["points"].as_array().unwrap();
    let first_index = |group: &str| {
        points
            .iter()
            .position(|p| p["group"] == group)
            .unwrap_or_else(|| panic!("{group} is not in the ranked sessions: {points:?}"))
    };
    assert!(
        first_index("s-big") < first_index("s-mid")
            && first_index("s-mid") < first_index("s-small"),
        "ranked by tokens, most first: {points:?}"
    );
    let big_buckets: Vec<serde_json::Value> = points
        .iter()
        .filter(|p| p["group"] == "s-big" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
        .cloned()
        .collect();
    assert_eq!(
        big_buckets.len(),
        2,
        "the hour-apart attempts land in two different buckets: {points:?}"
    );
    assert!(
        big_buckets.iter().all(|p| p["usd"].as_f64() == Some(0.0)),
        "an oauth session spends no dollars: {big_buckets:?}"
    );
    let mid_point = points
        .iter()
        .find(|p| p["group"] == "s-mid" && p["usd"].as_f64().unwrap_or(0.0) > 0.0)
        .unwrap_or_else(|| panic!("s-mid's bucket carries its dollar price: {points:?}"));
    assert_eq!(mid_point["uncached_tokens"].as_u64(), Some(1_000));
    // A session can move between upstreams, and shares of different accounts' windows do not add.
    assert!(
        points.iter().all(|p| null_field(p, "five_hour_share")
            && null_field(p, "seven_day_share")
            && null_field(p, "unit")),
        "the session grouping answers no share, a tier on file or not: {points:?}"
    );
}

#[tokio::test]
async fn the_burn_route_answers_only_the_twenty_busiest_sessions() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let session_request = |id: &str, session: &str, input: u64| Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: now - 100,
            finished_at: Some(now - 50),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some(session.into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: now - 100,
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            output_tokens: Some(10),
            ..Default::default()
        }],
        bodies: None,
    };
    // Twenty-one sessions with distinct token counts, written so all twenty-one land in the writer's one
    // batch; the smallest carries the fewest tokens and is the one the twenty-wide cut must drop.
    for i in 0..21u64 {
        let id = format!("r-cut-{i:02}");
        let session = format!("s-cut-{i:02}");
        parts.ledger.record(session_request(&id, &session, 100 + i));
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-cut-20").await.unwrap()
    })
    .await;
    let burn = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=session",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let points = burn["points"].as_array().unwrap();
    let groups: std::collections::BTreeSet<&str> = points
        .iter()
        .filter_map(|p| p["group"].as_str())
        .filter(|g| g.starts_with("s-cut-"))
        .collect();
    assert_eq!(
        groups.len(),
        20,
        "the twenty busiest sessions survive the cut: {groups:?}"
    );
    assert!(
        !groups.contains("s-cut-00"),
        "the session with the fewest tokens is the one the cut drops: {groups:?}"
    );
}

#[tokio::test]
async fn the_session_series_sizes_its_buckets_like_the_account_series_on_long_ranges() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    set_max_20x(&parts.store);
    let now = common::NOW;
    // A 7-day range buckets in 6 hours; `now` always falls in the trailing partial bucket the ceiling of the
    // hour leaves, since the newest bucket ends at `now`'s own hour rather than a 6-hour edge.
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-trailing".into(),
            received_at: now,
            finished_at: Some(now + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s-trailing".into()),
            model: "claude-sonnet-5-5".into(),
            canonical_model: "claude-sonnet-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-trailing".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: now,
            outcome: "succeeded".into(),
            input_tokens: Some(500),
            output_tokens: Some(50),
            ..Default::default()
        }],
        bodies: None,
    });
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-trailing").await.unwrap()
    })
    .await;
    let by_account = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=7d&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let by_session = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=7d&group_by=session",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let account_bucket_count = by_account["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["group"] == "business")
        .count();
    let session_points = by_session["points"].as_array().unwrap();
    let session_bucket_count = session_points
        .iter()
        .filter(|p| p["group"] == "s-trailing")
        .count();
    assert_eq!(
        session_bucket_count, account_bucket_count,
        "the session series answers as many points over the range as the account series does"
    );
    let trailing = session_points
        .iter()
        .rfind(|p| p["group"] == "s-trailing")
        .unwrap();
    assert_eq!(
        trailing["uncached_tokens"].as_u64(),
        Some(500),
        "the trailing partial bucket still carries the attempt inside it: {trailing}"
    );
    let account_trailing = by_account["points"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|p| p["group"] == "business")
        .unwrap();
    assert!(
        account_trailing["five_hour_share"].as_f64().unwrap_or(0.0) > 0.0
            && account_trailing["seven_day_share"].is_null(),
        "the account series prices the same attempt's 5-hour share in its trailing bucket, and every 7-day cell \
         is unmeasured: {account_trailing}"
    );
}

#[tokio::test]
async fn the_rewrites_route_answers_the_series_and_the_moves_by_reason() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let hour_ago = now - HOUR;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-rewrite".into(),
            received_at: hour_ago,
            finished_at: Some(hour_ago + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-rewrite".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: hour_ago,
            outcome: "succeeded".into(),
            cache_write_5m_tokens: Some(500),
            selection: Some(
                r#"{"strategy":"pace","tier":null,"renewed":false,"moved":"shed","skipped":[]}"#
                    .into(),
            ),
            ..Default::default()
        }],
        bodies: None,
    });
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 100,
        kind: "pin_migrated".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(
            serde_json::json!({ "from_account": "direct", "epoch": 2, "reason": "shed" })
                .to_string(),
        ),
    }));
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-rewrite").await.unwrap()
    })
    .await;
    let rewrites = json(
        get(
            &parts.app,
            "/admin/analytics/rewrites?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let total: u64 = rewrites["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["rewrite_tokens"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 500, "the moved attempt's cache write is the rewrite");
    assert_eq!(rewrites["moves"]["shed"].as_u64(), Some(1));
}

#[tokio::test]
async fn a_pool_row_answers_moves_per_hour_by_reason() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/accounts",
        r#"{"id":"outside","kind":"oauth","label":"Outside"}"#,
        Some(&cookie),
    )
    .await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"other","label":"Other","strategy":"priority","members":[{"account_id":"outside","priority":0}]}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"empty","label":"Empty","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    let now = common::NOW;
    let event_at = |account: &str, reason: &str, at: i64| {
        Record::Event(EventRecord {
            at,
            kind: "pin_migrated".into(),
            account_id: Some(account.into()),
            key_id: Some("desktop".into()),
            session_id: Some("s1".into()),
            model: Some("claude-sonnet-5".into()),
            actor: None,
            detail: Some(serde_json::json!({ "reason": reason }).to_string()),
        })
    };
    let event = |account: &str, reason: &str| event_at(account, reason, now - 100);
    parts.ledger.record(event("business", "shed"));
    parts.ledger.record(event("direct", "shed"));
    parts.ledger.record(event("business", "spent"));
    // A shed of another pool's account, left out of "main"'s count.
    parts.ledger.record(event("outside", "shed"));
    // 61 minutes ago: outside the last hour, so this one is not counted either.
    parts
        .ledger
        .record(event_at("business", "shed", now - 61 * 60_000));
    let pools = wait_json(&parts.app, "/admin/pools", &cookie, |v| {
        v["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "main")
            .is_some_and(|p| p["moves_per_hour"]["shed"] == 2.0)
    })
    .await;
    let pool = |id: &str| {
        pools["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            pool("main")["moves_per_hour"]["shed"].as_f64(),
            pool("main")["moves_per_hour"]["spent"].as_f64()
        ),
        (Some(2.0), Some(1.0)),
        "the hour-old move and another pool's account are both left out"
    );
    assert_eq!(
        pool("empty")["moves_per_hour"],
        serde_json::json!({}),
        "a memberless pool answers an empty map without a query to attribute moves against"
    );
}

#[tokio::test]
async fn a_request_answers_its_class_and_each_attempts_trace_with_skipped_as_named_objects_on_the_wire(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-trace".into(),
            received_at: common::NOW,
            finished_at: Some(common::NOW + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            request_class: Some("main".into()),
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-trace".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: common::NOW,
            outcome: "succeeded".into(),
            selection: Some(
                r#"{"strategy":"pace","tier":null,"renewed":false,"moved":null,"skipped":[{"account_id":"direct","obstacle":"cooldown"}]}"#
                    .into(),
            ),
            ..Default::default()
        }],
        bodies: None,
    });
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-trace").await.unwrap()
    })
    .await;
    let raw = get(&parts.app, "/admin/requests/r-trace", Some(&cookie)).await;
    let body = json(raw).await;
    assert_eq!(body["request"]["request_class"], "main");
    let skipped = &body["request"]["attempts"][0]["selection"]["skipped"];
    assert!(skipped.is_array());
    assert_eq!(
        skipped[0],
        serde_json::json!({ "account_id": "direct", "obstacle": "cooldown" }),
        "a named object on the wire, never a tuple: {body}"
    );
}

#[tokio::test]
async fn budgets_route_answers_a_members_spend_against_its_budget() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // "direct" carries a budget in both pools (proving a second row sorts by pool ahead of "main") while
    // "business" rides "main" unbudgeted (proving the route excludes it).
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority",
        "members":[{"account_id":"business","priority":0},
                   {"account_id":"direct","priority":1,"budget_daily_usd":20.0,"budget_monthly_usd":50.0}]}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"aux","label":"Aux","strategy":"priority",
        "members":[{"account_id":"direct","priority":0,"budget_daily_usd":5.0}]}"#,
        Some(&cookie),
    )
    .await;
    // A real priced attempt, so the writer's own refresh reports a real spend; the route's job is reading it
    // back, not computing it.
    let mut spend = record("r1", common::NOW, "claude-sonnet-5", "s1");
    if let Record::Request { attempts, .. } = &mut spend {
        attempts[0].account_id = "direct".into();
        attempts[0].account_kind = "api_key".into();
    }
    parts.ledger.record(spend);
    let budgets = wait_json(&parts.app, "/admin/analytics/budgets", &cookie, |v| {
        v["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["spent_usd"].as_f64().is_some_and(|s| s > 0.0))
    })
    .await;
    let members = budgets["members"].as_array().unwrap();
    assert_eq!(
        members.len(),
        3,
        "business's unbudgeted membership in main is excluded; one row answers per budgeted period: {budgets}"
    );
    assert_eq!(
        (
            members[0]["pool_id"].as_str(),
            members[0]["account_id"].as_str(),
            members[0]["budget_usd"].as_f64(),
            members[0]["budget_period"].as_str(),
            members[0]["kind"].as_str(),
        ),
        (
            Some("aux"),
            Some("direct"),
            Some(5.0),
            Some("day"),
            Some("api_key")
        ),
        "sorted by pool id ahead of main"
    );
    assert_eq!(
        (
            members[1]["pool_id"].as_str(),
            members[1]["account_id"].as_str(),
            members[1]["budget_usd"].as_f64(),
            members[1]["budget_period"].as_str(),
            members[1]["kind"].as_str(),
        ),
        (
            Some("main"),
            Some("direct"),
            Some(20.0),
            Some("day"),
            Some("api_key")
        ),
        "a member with both budgets answers its day ahead of its month"
    );
    let member = &members[2];
    assert_eq!(
        (
            member["pool_id"].as_str(),
            member["account_id"].as_str(),
            member["budget_usd"].as_f64(),
            member["budget_period"].as_str(),
            member["kind"].as_str(),
        ),
        (
            Some("main"),
            Some("direct"),
            Some(50.0),
            Some("month"),
            Some("api_key")
        ),
        "a budgeted period names its pool, account, and budget"
    );
    assert_eq!(
        member["period_end"].as_i64(),
        Some(2_678_400_000),
        "the period end is the calendar month's own boundary at the fixture's clock"
    );
}

#[tokio::test]
async fn budgets_route_refuses_once_the_router_is_gone() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.router.shutdown().await.unwrap();
    let refused = get(&parts.app, "/admin/analytics/budgets", Some(&cookie)).await;
    assert_eq!(refused.status(), 503);
    assert_eq!(
        json(refused).await["error"].as_str(),
        Some("the router is not available")
    );
}

#[tokio::test]
async fn placements_route_answers_the_seeded_ledgers_rows_by_rung() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"other","label":"Other","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let generation_of = |key: &str| common::generation(&parts.store, key);
    let plant = |id: &str,
                 key: &str,
                 at: i64,
                 rung: &str,
                 prefix_tokens: u64,
                 cache_read_tokens: u64| {
        Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 500),
                key_id: Some(key.into()),
                key_generation: generation_of(key),
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
                outcome: "succeeded".into(),
                cache_read_tokens: Some(cache_read_tokens),
                selection: Some(format!(
                    r#"{{"placement":{{"rung":"{rung}","depth":1,"prefix_tokens":{prefix_tokens}}}}}"#
                )),
                ..Default::default()
            }],
            bodies: None,
        }
    };
    // A hit and a miss on the same rung, so the ratio the route answers is not a coincidence of the seed:
    // inverting or fixing `hit_share` at either extreme would still pass a seed of one hit alone.
    parts.ledger.record(plant(
        "p-content",
        "desktop",
        common::NOW - 5_000,
        "content",
        500,
        500,
    ));
    parts.ledger.record(plant(
        "p-content-miss",
        "desktop",
        common::NOW - 4_500,
        "content",
        500,
        200,
    ));
    parts.ledger.record(plant(
        "p-static",
        "desktop",
        common::NOW - 4_000,
        "static",
        200,
        50,
    ));
    // Under a different key, so the `keys` filter has something of its own to exclude.
    parts.ledger.record(plant(
        "p-other-key",
        "other",
        common::NOW - 3_000,
        "with_main",
        0,
        10,
    ));
    for id in ["p-content", "p-content-miss", "p-static", "p-other-key"] {
        fake_upstream::wait_for_async(Duration::from_secs(5), || async {
            parts.db.request(id).await.unwrap()
        })
        .await;
    }
    let placed = json(
        get(
            &parts.app,
            "/admin/analytics/placements?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let row = |placed: &serde_json::Value, rung: &str| {
        placed["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["rung"] == rung)
            .cloned()
    };
    assert_eq!(
        (
            row(&placed, "content").as_ref().unwrap()["placements"].as_u64(),
            row(&placed, "content").as_ref().unwrap()["hits"].as_u64(),
            row(&placed, "content").as_ref().unwrap()["hit_share"].as_f64(),
            row(&placed, "content").as_ref().unwrap()["cache_read_tokens"].as_u64(),
            row(&placed, "content").as_ref().unwrap()["promised_tokens"].as_u64(),
        ),
        (Some(2), Some(1), Some(0.5), Some(700), Some(1_000)),
        "one hit among two placements reads the true ratio, not the extremes a single-row seed cannot tell apart"
    );
    assert_eq!(
        (
            row(&placed, "static").as_ref().unwrap()["placements"].as_u64(),
            row(&placed, "static").as_ref().unwrap()["hits"].as_u64(),
            row(&placed, "static").as_ref().unwrap()["hit_share"].as_f64(),
        ),
        (Some(1), Some(1), Some(1.0)),
        "a static placement is a hit on any read at all"
    );
    assert!(
        row(&placed, "with_main").is_some(),
        "the other key's placement is answered without a keys filter"
    );
    let filtered = json(
        get(
            &parts.app,
            "/admin/analytics/placements?range=24h&keys=desktop",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        row(&filtered, "with_main").is_none(),
        "a keys filter naming desktop excludes the other key's placement"
    );
    assert_eq!(
        row(&filtered, "content").as_ref().unwrap()["placements"].as_u64(),
        Some(2),
        "desktop's own rows still answer under the filter"
    );
}
