//! Pricing with its import, settings, system, quota, traffic, sessions, and the live routes.
mod common;
use std::time::Duration;

use phoebis_bus::{Event, Kind, Origin, Phase, RequestObserved, RequestShape, ResponseChunk};
use phoebis_ledger::{AttemptRecord, Record, RequestRecord};
use common::{delete, get, json, login, post};
use fake_upstream::{FakeUpstream, Respond, Rule};
use futures::StreamExt;

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
async fn pricing_is_listed_imported_with_a_diff_and_confirmed_into_a_new_version_that_only_new_attempts_carry(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let current = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    assert_eq!(
        (
            current["version"].as_str(),
            current["source"].as_str(),
            current["models"].as_array().unwrap().len()
        ),
        (Some("2026-09-08"), Some("shipped"), 4)
    );
    assert_eq!(current["models"][3]["model"], "claude-sonnet-5");
    assert_eq!(current["models"][3]["input_per_m"].as_f64(), Some(2.0));
    // The registry serves claude-sonnet-4-5 (the harness's catalog); the file prices it and two others.
    let prices = FakeUpstream::start_plain(vec![Rule { path_prefix: "/prices.json".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"claude-sonnet-4-5":{"input_cost_per_token":3e-06,"cache_creation_input_token_cost":3.75e-06,"cache_read_input_token_cost":3e-07,"output_cost_per_token":1.5e-05},"claude-opus-5":{"input_cost_per_token":9e-06,"output_cost_per_token":4.5e-05},"gpt-5":{"input_cost_per_token":1e-06,"output_cost_per_token":8e-06}}"#.to_vec() } }, Rule { path_prefix: "/huge".into(), respond: Respond::Json { status: 200, headers: vec![], body: vec![b'x'; 17 << 20] } }]).await;
    parts
        .store
        .apply_catalog_sync(
            "business",
            &[("claude-sonnet-4-5".into(), false)],
            None,
            now,
            "test",
        )
        .unwrap();
    let label = format!("http://127.0.0.1:{}/prices.json", prices.addr().port());
    let url = format!("{label}?access_token=PLACEHOLDER-TOKEN");
    let preview = json(
        post(
            &parts.app,
            "/admin/pricing/import",
            &format!(r#"{{"url":"{url}"}}"#),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            preview["format"].as_str(),
            preview["in_file"].as_u64(),
            preview["served"].as_u64()
        ),
        (Some("litellm"), Some(3), Some(1))
    );
    assert_eq!(preview["models"].as_array().unwrap().len(), 1);
    assert_eq!(preview["models"][0]["model"], "claude-sonnet-4-5");
    assert!(
        (preview["models"][0]["cache_write_1h_per_m"]
            .as_f64()
            .unwrap()
            - 6.0)
            .abs()
            < 1e-9
    );
    assert_eq!(
        preview["changes"].as_array().unwrap().len(),
        5,
        "a model the table lacks changes on every field"
    );
    assert!(preview["changes"][0]["from"].is_null());
    let token = preview["token"].as_str().unwrap().to_string();
    assert_eq!(
        post(
            &parts.app,
            "/admin/pricing/import",
            r#"{"url":"ftp://x/y"}"#,
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        post(
            &parts.app,
            "/admin/pricing/import",
            &format!(
                r#"{{"url":"http://127.0.0.1:{}/huge"}}"#,
                prices.addr().port()
            ),
            Some(&cookie)
        )
        .await
        .status(),
        502,
        "past the 16 MiB bound"
    );
    let viewer = login(&parts.app, "viewer-secret").await;
    assert_eq!(
        post(
            &parts.app,
            "/admin/pricing/import",
            &format!(r#"{{"url":"{url}"}}"#),
            Some(&viewer)
        )
        .await
        .status(),
        403
    );
    // An attempt written before the confirmation keeps the shipped version.
    parts
        .ledger
        .record(record("before", now - HOUR, "claude-sonnet-5", "s1"));
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("before").await.unwrap()
    })
    .await;
    let confirmed = post(
        &parts.app,
        "/admin/pricing/import/confirm",
        &format!(r#"{{"token":"{token}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(confirmed.status(), 201);
    let table = json(confirmed).await;
    assert!(table["version"].as_str().unwrap().starts_with("imported-"));
    assert_eq!(
        (table["effective"].as_str(), table["source"].as_str()),
        (Some("1970-01-01"), Some(label.as_str())),
        "the import time (the frozen clock) is the effective date"
    );
    assert_eq!(
        table["models"].as_array().unwrap().len(),
        5,
        "the shipped four plus the imported one"
    );
    assert_eq!(table["versions"].as_array().unwrap().len(), 1);
    assert_eq!(
        table["versions"][0]["source"].as_str(),
        Some(label.as_str())
    );
    let pricing_view = json(get(&parts.app, "/admin/pricing", Some(&viewer)).await).await;
    assert_eq!(pricing_view["source"].as_str(), Some(label.as_str()));
    assert_eq!(
        pricing_view["versions"][0]["source"].as_str(),
        Some(label.as_str())
    );
    let settings_view = json(get(&parts.app, "/admin/settings", Some(&viewer)).await).await;
    assert_eq!(
        settings_view["pricing"]["source"].as_str(),
        Some(label.as_str())
    );
    for body in [&table, &pricing_view, &settings_view] {
        assert!(
            !body.to_string().contains("PLACEHOLDER-TOKEN"),
            "the import credential never reaches an answer"
        );
    }
    assert_eq!(
        post(
            &parts.app,
            "/admin/pricing/import/confirm",
            &format!(r#"{{"token":"{token}"}}"#),
            Some(&cookie)
        )
        .await
        .status(),
        404,
        "a token is spent by its confirmation"
    );
    parts
        .ledger
        .record(record("after", now, "claude-sonnet-4-5", "s1"));
    let (_, attempts) = fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("after").await.unwrap()
    })
    .await;
    assert!(attempts[0]
        .pricing_version
        .as_deref()
        .unwrap()
        .starts_with("imported-"));
    assert!(
        (attempts[0].estimated_usd.unwrap() - (1_000.0 * 3.0 + 100.0 * 15.0) / 1e6).abs() < 1e-12
    );
    let (_, before) = parts.db.request("before").await.unwrap().unwrap();
    assert_eq!(
        before[0].pricing_version.as_deref(),
        Some("2026-09-08"),
        "old attempts keep their version"
    );
    let costs = json(get(&parts.app, "/admin/sessions/costs", Some(&viewer)).await).await;
    assert!(costs["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["model"] == "claude-sonnet-4-5"
            && (m["reads_per_write_5m"].as_f64().unwrap() - 12.5).abs() < 1e-9));
    prices.shutdown().await;
}

/// A gzip-compressed body, the shape `flate2::write::GzEncoder` gives a stream the wire never decodes for us.
fn gzip(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(payload).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn an_import_that_cannot_connect_answers_without_the_url_or_its_token() {
    const SENTINEL: &str = "SENTINEL-IMPORT-TOKEN";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let before = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    let resp = tokio::time::timeout(
        Duration::from_secs(20),
        post(
            &parts.app,
            "/admin/pricing/import",
            &format!(r#"{{"url":"http://127.0.0.1:{port}/prices.json?access_token={SENTINEL}"}}"#),
            Some(&cookie),
        ),
    )
    .await
    .expect("a refused connection answers promptly");
    assert_eq!(resp.status(), 502);
    let body = json(resp).await;
    let text = body.to_string();
    assert!(
        !text.contains(SENTINEL) && !text.contains("127.0.0.1"),
        "the answer names neither the token nor the host: {text}"
    );
    let after = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    assert_eq!(before, after, "nothing was written for the refused fetch");
}

#[tokio::test]
async fn a_gzip_encoded_price_file_is_decoded_before_it_is_parsed() {
    let prices = FakeUpstream::start_plain(vec![Rule {
        path_prefix: "/prices.json".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            body: gzip(br#"{"claude-sonnet-4-5":{"input_cost_per_token":3e-06,"output_cost_per_token":1.5e-05}}"#),
        },
    }])
    .await;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .store
        .apply_catalog_sync(
            "business",
            &[("claude-sonnet-4-5".into(), false)],
            None,
            common::NOW,
            "test",
        )
        .unwrap();
    let url = format!("http://127.0.0.1:{}/prices.json", prices.addr().port());
    let preview = json(
        post(
            &parts.app,
            "/admin/pricing/import",
            &format!(r#"{{"url":"{url}"}}"#),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(preview["models"].as_array().unwrap().len(), 1);
    assert_eq!(preview["models"][0]["model"], "claude-sonnet-4-5");
    prices.shutdown().await;
}

#[tokio::test]
async fn a_gzip_bomb_past_the_bound_is_refused_by_the_bound() {
    let prices = FakeUpstream::start_plain(vec![Rule {
        path_prefix: "/prices.json".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            body: gzip(&vec![b'x'; 17 << 20]),
        },
    }])
    .await;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let url = format!("http://127.0.0.1:{}/prices.json", prices.addr().port());
    let resp = post(
        &parts.app,
        "/admin/pricing/import",
        &format!(r#"{{"url":"{url}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), 502);
    let body = json(resp).await;
    assert_eq!(
        body["error"].as_str(),
        Some("the price file is larger than the 16 MiB bound"),
        "{body}"
    );
    prices.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_import_that_never_answers_is_refused_at_the_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accepted once and held open, never written to: the client's read waits out the import's own timeout
    // rather than a connection refusal or a peer close.
    let _held = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(stream);
    });
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let resp = tokio::time::timeout(
        Duration::from_secs(45),
        post(
            &parts.app,
            "/admin/pricing/import",
            &format!(r#"{{"url":"http://{addr}/prices.json"}}"#),
            Some(&cookie),
        ),
    )
    .await
    .expect("the paused clock advances straight to the import's own timeout");
    assert_eq!(resp.status(), 502);
    let body = json(resp).await;
    assert_eq!(
        body["error"].as_str(),
        Some("the price file did not arrive in time"),
        "{body}"
    );
}

#[tokio::test]
async fn an_import_redirected_by_its_host_is_refused_without_following_it() {
    let prices = FakeUpstream::start_plain(vec![Rule {
        path_prefix: "/prices.json".into(),
        respond: Respond::Json {
            status: 302,
            headers: vec![("Location".into(), "http://127.0.0.1:1/prices.json".into())],
            body: Vec::new(),
        },
    }])
    .await;
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let url = format!("http://127.0.0.1:{}/prices.json", prices.addr().port());
    let resp = post(
        &parts.app,
        "/admin/pricing/import",
        &format!(r#"{{"url":"{url}"}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), 502);
    let body = json(resp).await;
    assert_eq!(
        body["error"].as_str(),
        Some("the price file answered 302"),
        "{body}"
    );
    assert_eq!(
        prices.requests().len(),
        1,
        "the redirect must never be followed"
    );
    prices.shutdown().await;
}

#[tokio::test]
async fn settings_and_system_show_effective_values_ledger_health_and_gaps_without_secrets() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let settings = json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await;
    assert_eq!(
        (
            settings["retention"]["ledger_days"].as_u64(),
            settings["retention"]["quota_samples_days"].as_u64(),
            settings["retention"]["body_capture_max_hours"].as_u64()
        ),
        (Some(90), Some(30), Some(72))
    );
    assert_eq!(
        (
            settings["quota"]["poll_interval_seconds"].as_u64(),
            settings["sessions"]["expire_after_seconds"].as_u64(),
            settings["listeners"]["metrics"].as_str()
        ),
        (Some(3_600), Some(86_400), Some("127.0.0.1:9464"))
    );
    assert_eq!(settings["pricing"]["version"], "2026-09-08");
    assert_eq!(
        settings["last_sync_at"],
        serde_json::Value::Null,
        "a registry whose catalog was never synced answers no instant at all"
    );
    parts
        .store
        .set_catalog_status("business", Some(1_700_000_000_000), None)
        .unwrap();
    parts
        .store
        .set_catalog_status("direct", Some(1_600_000_000_000), None)
        .unwrap();
    assert_eq!(
        json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await["last_sync_at"]
            .as_i64(),
        Some(1_700_000_000_000),
        "the newest sync across the accounts is the router's last sync"
    );
    let text = settings.to_string();
    assert!(
        !text.contains("admin-secret")
            && !text.contains("viewer-secret")
            && !text.contains("secret_env"),
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
        (Some(15), Some(0), Some(4_096)),
        "the ledger schema is at the migration that drops the attempt's host"
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
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: bucket + 300,
                    window: "5h".into(),
                    utilization: Some(0.5),
                    reset_at: Some(now + HOUR),
                    source: "poll".into(),
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "fable".into(),
                    utilization: Some(0.1),
                    reset_at: None,
                    source: "poll".into(),
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
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
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
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"business","priority":0}]}"#, Some(&seeding)).await;
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
                model: "claude-sonnet-5".into(),
                account_id: "business".into(),
                epoch: 1,
                created_at: now - 10,
                last_used_at: now - 5,
                migrated_at: None,
                key_generation: common::generation(&seed.store, "desktop"),
                incarnation: 1,
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
async fn the_live_route_is_gated_and_the_preview_follows_the_role() {
    let parts = common::admin_parts_full().await;
    let addr = common::serve(parts.app.clone()).await;
    let admin = login(&parts.app, "admin-secret").await;
    let viewer = login(&parts.app, "viewer-secret").await;
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
    let (mut viewer_ws, _) = connect(Some(viewer)).await.unwrap();
    assert_eq!(common::next_text(&mut admin_ws).await["kind"], "snapshot");
    assert_eq!(common::next_text(&mut viewer_ws).await["kind"], "snapshot");
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
        stream: true,
        headers: vec![],
        body: bytes::Bytes::from_static(b"{}"),
        body_capture_hours: 24,
        shape: RequestShape::default(),
        presented_retired: false,
        rotation_at: None,
        received_at: common::NOW,
    }));
    parts.bus.publish(Event::ResponseChunk(ResponseChunk { request_id: "live-1".into(), attempt: 1, index: 0, body: bytes::Bytes::from_static(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"secret prompt text\"}}\n\n"), phase: Some(Phase::FirstContentDelta) }));
    let admin_seen = common::deltas_until(&mut admin_ws, "preview").await;
    assert!(admin_seen
        .iter()
        .any(|d| d["type"] == "preview" && d["chunk"] == "secret prompt text"));
    let viewer_seen = common::deltas_for(&mut viewer_ws, Duration::from_millis(500)).await;
    assert!(
        viewer_seen.iter().any(|d| d["type"] == "started"),
        "the viewer sees the request"
    );
    assert!(
        viewer_seen.iter().all(|d| d["type"] != "preview"),
        "and never its text"
    );
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
        "a past read carries no preview for anyone"
    );
    let viewer_cookie = login(&parts.app, "viewer-secret").await;
    let past = json(
        get(
            &parts.app,
            &format!(
                "/admin/live/snapshot?from={}&to={}",
                common::NOW - 1,
                common::NOW + 1
            ),
            Some(&viewer_cookie),
        )
        .await,
    )
    .await;
    assert!(past["entries"][0]["preview"].is_null());
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
    let _ = viewer_ws.close(None).await;
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
