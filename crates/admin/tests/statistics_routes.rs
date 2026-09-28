//! Pricing with its import, settings, system, quota, traffic, sessions, and the live routes.
mod common;
use std::time::Duration;

use phoebis_bus::{Event, Kind, Origin, Phase, RequestObserved, RequestShape, ResponseChunk};
use phoebis_ledger::{AttemptRecord, EventRecord, Record, RequestRecord};
use common::{delete, get, json, login, post, wait_json};
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

#[tokio::test]
async fn a_confirmed_import_reaches_the_routers_burn_without_a_restart() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // Named as the harness's catalog serves it (`claude-sonnet-4-5`), or the diff would leave it out as an
    // unserved model and the confirmed table would carry the shipped row's rates instead.
    let before = parts.router.rates("claude-sonnet-4-5").await.unwrap();
    assert_eq!(
        before.input, 1.0,
        "the harness's router starts on the unit table, the burn's effect otherwise only visible through a served request"
    );
    let prices = FakeUpstream::start_plain(vec![Rule { path_prefix: "/prices.json".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"claude-sonnet-4-5":{"input_cost_per_token":9e-06,"output_cost_per_token":4.5e-05}}"#.to_vec() } }]).await;
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
    let token = preview["token"].as_str().unwrap().to_string();
    assert_eq!(
        post(
            &parts.app,
            "/admin/pricing/import/confirm",
            &format!(r#"{{"token":"{token}"}}"#),
            Some(&cookie),
        )
        .await
        .status(),
        201
    );
    let after = parts.router.rates("claude-sonnet-4-5").await.unwrap();
    assert_eq!(
        (after.input, after.output),
        (9e-06, 4.5e-05),
        "the confirmed table's rates reach the router at once"
    );
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
    assert_eq!(settings["plan_factors"]["reference"], "claude-opus-5");
    assert_eq!(
        settings["plan_factors"]["models"]["claude-opus-5"]["factor"].as_f64(),
        Some(1.0)
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
        (Some(19), Some(0), Some(4_096)),
        "the ledger schema is at the migration that adds the identity columns"
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
                burn: 0.0,
                burn_at: None,
                burn_usd: 0.0,
                last_input_tokens: 0,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
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
        wire_model: "claude-sonnet-5".into(),
        stream: true,
        headers: vec![],
        body: bytes::Bytes::from_static(b"{}"),
        body_capture_hours: 24,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: phoebis_bus::ClientFacts::default(),
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

#[tokio::test]
async fn the_burn_route_answers_meter_tokens_per_account_with_plan_percent() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .store
        .set_profile(
            "business",
            r#"{"account":{"uuid":"u","email_address":"business@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_5x".into()),
                since: Some(1_000_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
    let now = common::NOW;
    parts
        .ledger
        .record(record("burn-1", now - HOUR, "claude-sonnet-5", "s1"));
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("burn-1").await.unwrap()
    })
    .await;
    let burn = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(burn["group_by"], "account");
    let point = burn["points"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["group"] == "business" && p["meter_tokens"].as_f64().unwrap_or(0.0) > 0.0)
        .unwrap_or_else(|| panic!("a bucket carries the recorded attempt: {burn}"));
    let meter_tokens = point["meter_tokens"].as_f64().unwrap();
    let expected = meter_tokens / 11_890_000.0;
    assert!(
        (point["plan_percent"].as_f64().unwrap() - expected).abs() < 1e-12,
        "the fitted 5-hour scale is absent, so the tier's predicted one divides the bucket: {point}"
    );
    // A fitted scale, other than the tier's row, takes over the division.
    parts.router.report(phoebis_routing::Report::Fit {
        row: phoebis_store::FitRow {
            account_id: "business".into(),
            window: phoebis_routing::FIVE_HOUR.into(),
            scale: 5_945_000.0,
            confidence: 0.8,
            intervals: 12,
            corrections_json: "{}".into(),
            fitted_at: now,
            incarnation: 1,
            band_factor: None,
            ..Default::default()
        },
        incarnation: 1,
    });
    let expected_fitted = meter_tokens / 5_945_000.0;
    let burn_fitted = wait_json(
        &parts.app,
        "/admin/analytics/burn?range=24h&group_by=account",
        &cookie,
        |v| {
            v["points"].as_array().unwrap().iter().any(|p| {
                p["group"] == "business"
                    && p["meter_tokens"].as_f64().unwrap_or(0.0) > 0.0
                    && (p["plan_percent"].as_f64().unwrap_or(-1.0) - expected_fitted).abs() < 1e-9
            })
        },
    )
    .await;
    let fitted_point = burn_fitted["points"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["group"] == "business" && p["meter_tokens"].as_f64().unwrap_or(0.0) > 0.0)
        .unwrap();
    assert!(
        (fitted_point["plan_percent"].as_f64().unwrap() - expected_fitted).abs() < 1e-9,
        "a fitted 5-hour scale divides the bucket instead of the tier's predicted one: {fitted_point}"
    );
    // An api_key account has no tier on file and no fit row, so its point reads plan_percent null.
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
    let with_direct = wait_json(
        &parts.app,
        "/admin/analytics/burn?range=24h&group_by=account",
        &cookie,
        |v| {
            v["points"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["group"] == "direct" && p["usd"].as_f64().unwrap_or(0.0) > 0.0)
        },
    )
    .await;
    assert!(
        with_direct["points"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["group"] == "direct")
            .all(|p| p["plan_percent"].is_null()),
        "an api_key account has no plan to read a percent of"
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
        by_key["points"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["plan_percent"].is_null()),
        "plan_percent is an account-grouped figure alone"
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
async fn the_burn_route_ranks_sessions_by_tokens_and_prices_each_by_its_accounts_kind() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
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
    // An attempt that goes out a minute before the peak band's first minute inside the range and completes a
    // minute into it: the meter charges it at its completion, so the series prices it at the band's factor.
    let windows = phoebis_ledger::PlanWindows::shipped();
    let band_edge = (1..24)
        .flat_map(|h| (0..60).map(move |m| now - 24 * HOUR + h * HOUR + m * 60_000))
        .find(|&at| {
            windows.band_factor(phoebis_routing::FIVE_HOUR, at) > 1.0
                && windows.band_factor(phoebis_routing::FIVE_HOUR, at - 60_000) == 1.0
        })
        .expect("a band edge inside the range");
    let (edge_started, edge_finished) = (band_edge - 60_000, band_edge + 60_000);
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-edge".into(),
            received_at: edge_started,
            finished_at: Some(edge_finished),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s-edge".into()),
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
            request_id: "r-edge".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: edge_started,
            finished_at: Some(edge_finished),
            outcome: "succeeded".into(),
            input_tokens: Some(100),
            output_tokens: Some(10),
            ..Default::default()
        }],
        bodies: None,
    });
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-mid").await.unwrap()
    })
    .await;
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-edge").await.unwrap()
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
        .filter(|p| p["group"] == "s-big" && p["meter_tokens"].as_f64().unwrap_or(0.0) > 0.0)
        .cloned()
        .collect();
    assert_eq!(
        big_buckets.len(),
        2,
        "the hour-apart attempts land in two different buckets: {points:?}"
    );
    assert!(
        big_buckets.iter().all(|p| p["usd"].as_f64() == Some(0.0)),
        "an oauth session prices in meter tokens alone: {big_buckets:?}"
    );
    let mid_point = points
        .iter()
        .find(|p| p["group"] == "s-mid" && p["usd"].as_f64().unwrap_or(0.0) > 0.0)
        .unwrap_or_else(|| panic!("s-mid's bucket carries its dollar price: {points:?}"));
    assert_eq!(
        mid_point["meter_tokens"].as_f64(),
        Some(0.0),
        "an api_key session prices in dollars alone"
    );
    let edge_point = points
        .iter()
        .find(|p| p["group"] == "s-edge" && p["meter_tokens"].as_f64().unwrap_or(0.0) > 0.0)
        .unwrap_or_else(|| panic!("s-edge's bucket carries its meter tokens: {points:?}"));
    let plain = phoebis_ledger::PlanFactors::shipped()
        .meter_tokens("claude-sonnet-5", 100, 0, 0, 0, 10)
        .unwrap();
    let factor = windows.band_factor(phoebis_routing::FIVE_HOUR, edge_finished);
    assert!(factor > 1.0, "the completion falls inside the band");
    assert!(
        (edge_point["meter_tokens"].as_f64().unwrap() - plain * factor).abs() < 1e-9,
        "priced at the band's factor its completion fell in, not the start's: {edge_point} vs {}",
        plain * factor
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
    assert!(
        trailing["meter_tokens"].as_f64().unwrap_or(0.0) > 0.0,
        "the trailing partial bucket still carries the attempt inside it: {trailing}"
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
async fn the_fits_route_answers_the_drift_against_the_tiers_row() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .store
        .set_profile(
            "business",
            r#"{"account":{"uuid":"u","email_address":"business@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_5x".into()),
                since: Some(1_000_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
    parts.router.report(phoebis_routing::Report::Fit {
        row: phoebis_store::FitRow {
            account_id: "business".into(),
            window: phoebis_routing::FIVE_HOUR.into(),
            scale: 5_945_000.0,
            confidence: 0.8,
            intervals: 12,
            corrections_json: "{}".into(),
            fitted_at: common::NOW,
            incarnation: 1,
            band_factor: None,
            ..Default::default()
        },
        incarnation: 1,
    });
    let fits = wait_json(&parts.app, "/admin/analytics/fits", &cookie, |v| {
        v["fits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["account_id"] == "business")
    })
    .await;
    let row = fits["fits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["account_id"] == "business")
        .unwrap();
    assert_eq!(row["predicted"].as_f64(), Some(11_890_000.0));
    assert!(
        (row["drift"].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "scale over predicted: {row}"
    );
    assert_eq!(row["band_factor_predicted"].as_f64(), Some(1.45));
}

#[tokio::test]
async fn the_fits_route_marks_a_band_factor_more_than_fifteen_percent_off_the_row() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let fit = |account: &str, window: &str, band_factor: Option<f64>| phoebis_store::FitRow {
        account_id: account.into(),
        window: window.into(),
        scale: 1.0,
        confidence: 0.5,
        intervals: 1,
        corrections_json: "{}".into(),
        fitted_at: common::NOW,
        incarnation: 1,
        band_factor,
        ..Default::default()
    };
    parts.router.report(phoebis_routing::Report::Fit {
        row: fit("business", phoebis_routing::FIVE_HOUR, Some(1.0)),
        incarnation: 1,
    });
    parts.router.report(phoebis_routing::Report::Fit {
        row: fit("direct", phoebis_routing::FIVE_HOUR, Some(1.4)),
        incarnation: 1,
    });
    parts.router.report(phoebis_routing::Report::Fit {
        row: fit("business", phoebis_routing::SEVEN_DAY, None),
        incarnation: 1,
    });
    let fits = wait_json(&parts.app, "/admin/analytics/fits", &cookie, |v| {
        v["fits"].as_array().unwrap().len() >= 3
    })
    .await;
    let row = |account: &str, window: &str| {
        fits["fits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["account_id"] == account && f["window"] == window)
            .unwrap()
            .clone()
    };
    assert_eq!(row("business", "five_hour")["band_divergent"], true);
    assert_eq!(row("direct", "five_hour")["band_divergent"], false);
    let seven = row("business", "seven_day");
    assert_eq!(
        (
            seven["band_factor"].as_f64(),
            seven["band_factor_predicted"].as_f64(),
            seven["band_divergent"].as_bool()
        ),
        (None, None, Some(false)),
        "no band lists the seven-day window"
    );
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
    // A budget applies to an API-key member alone, so "direct" carries one in both pools (proving a second row
    // sorts by pool ahead of "main") while "business" rides "main" unbudgeted (proving the route excludes it).
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority",
        "members":[{"account_id":"business","priority":0},
                   {"account_id":"direct","priority":1,"budget_usd":50.0,"budget_period":"month"}]}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"aux","label":"Aux","strategy":"priority",
        "members":[{"account_id":"direct","priority":0,"budget_usd":5.0,"budget_period":"day"}]}"#,
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
        2,
        "business's unbudgeted membership in main is excluded; only the two budgeted rows answer: {budgets}"
    );
    assert_eq!(
        (
            members[0]["pool_id"].as_str(),
            members[0]["account_id"].as_str(),
            members[0]["budget_usd"].as_f64(),
            members[0]["budget_period"].as_str(),
        ),
        (Some("aux"), Some("direct"), Some(5.0), Some("day")),
        "sorted by pool id ahead of main"
    );
    let member = &members[1];
    assert_eq!(
        (
            member["pool_id"].as_str(),
            member["account_id"].as_str(),
            member["budget_usd"].as_f64(),
            member["budget_period"].as_str(),
        ),
        (Some("main"), Some("direct"), Some(50.0), Some("month")),
        "a budgeted member names its pool, account, and budget"
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
