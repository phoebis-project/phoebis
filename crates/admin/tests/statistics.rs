//! The analytics, request-log, and dashboard routes over rows written straight into the ledger.
mod common;
use phoebis_ledger::{AttemptRecord, BodyRecord, RequestRecord};
use common::{get, json, login, post};

const HOUR: i64 = 3_600_000;

/// Two client builds, so a group-by over them has more than one group; `nokey` sends neither.
const SDK_CLI: &str = "claude-cli/2.1.269 (external, sdk-cli)";
const NEWER_CLI: &str = "claude-cli/2.1.300 (external, cli)";

fn request(
    id: &str,
    at: i64,
    key: Option<&str>,
    model: &str,
    outcome: &str,
    status: u16,
    kind: &str,
) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: at,
        finished_at: Some(at + 900),
        key_id: key.map(String::from),
        pool_id: key.map(|_| "main".into()),
        session_id: Some("s1".into()),
        agent_id: Some("main".into()),
        model: model.into(),
        canonical_model: model.to_ascii_lowercase(),
        stream: true,
        kind: kind.into(),
        origin: "client".into(),
        status: Some(status),
        outcome: outcome.into(),
        attempts: 1,
        ttfb_ms: Some(120),
        ttfc_ms: Some(300),
        duration_ms: Some(900),
        error_code: (outcome != "succeeded").then(|| "upstream_error".into()),
        ..Default::default()
    }
}

fn attempt(id: &str, account: &str, status: u16) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: account.into(),
        account_kind: "oauth".into(),
        started_at: 1,
        upstream_status: Some(status),
        outcome: if status == 200 {
            "succeeded"
        } else {
            "upstream_error"
        }
        .into(),
        input_tokens: Some(100),
        output_tokens: Some(10),
        cache_read_tokens: Some(40),
        estimated_usd: Some(0.01),
        pricing_version: Some("2026-09-08".into()),
        ratelimit_5h_utilization: Some(0.3),
        ratelimit_5h_reset: Some(9_000),
        ..Default::default()
    }
}

#[tokio::test]
async fn analytics_series_breakdown_and_accounts_read_the_rollups_with_percentiles() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    for i in 0..6 {
        let at = now - (5 - i) * HOUR;
        let mut row = request(
            &format!("r{i}"),
            at,
            Some("desktop"),
            "claude-sonnet-5",
            if i == 2 { "failed" } else { "succeeded" },
            if i == 2 { 500 } else { 200 },
            "inference",
        );
        row.user_agent = Some(if i % 2 == 0 { SDK_CLI } else { NEWER_CLI }.into());
        parts
            .db
            .insert_request(
                &row,
                &[attempt(
                    &format!("r{i}"),
                    if i % 2 == 0 { "business" } else { "direct" },
                    if i == 2 { 500 } else { 200 },
                )],
                None,
            )
            .unwrap();
    }
    parts
        .db
        .insert_request(
            &request(
                "probe",
                now - HOUR,
                Some("desktop"),
                "claude-sonnet-5",
                "succeeded",
                200,
                "probe",
            ),
            &[attempt("probe", "business", 200)],
            None,
        )
        .unwrap();
    parts
        .db
        .insert_request(
            &request(
                "nokey",
                now - HOUR,
                None,
                "claude-sonnet-5",
                "rejected",
                401,
                "inference",
            ),
            &[],
            None,
        )
        .unwrap();
    let cookie = login(&parts.app, "admin-secret").await;
    let series = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            series["bucket_seconds"].as_i64(),
            series["group_by"].as_str()
        ),
        (Some(3_600), Some("account"))
    );
    assert_eq!(
        series["points"].as_array().unwrap().len(),
        25,
        "a 24-hour range off the hour boundary touches 25 hour buckets, empty ones included"
    );
    let total: u64 = series["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .map(|g| g["requests"].as_u64().unwrap())
        .sum();
    assert_eq!(
        total, 7,
        "six requests and the unattributed one; the probe is out by default"
    );
    let with_probes = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=kind&include_probes=true",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let kinds: Vec<String> = with_probes["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .map(|g| g["key"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"probe".to_string()));
    let breakdown = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let rows = breakdown["rows"].as_array().unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r["label"].as_str().unwrap()).collect();
    assert_eq!(
        labels,
        ["business", "direct", "unattributed"],
        "sorted by label; the reserved id is labelled"
    );
    let business = &rows[0];
    assert_eq!(
        (
            business["requests"].as_u64(),
            business["succeeded"].as_u64(),
            business["failed"].as_u64(),
            business["cache_hits"].as_u64()
        ),
        (Some(3), Some(2), Some(1), Some(3))
    );
    assert_eq!(business["tokens"]["cache_read"].as_u64(), Some(120));
    assert!((business["cost_usd"].as_f64().unwrap() - 0.03).abs() < 1e-9);
    assert_eq!(
        business["histograms"]["ttfb_ms"].as_array().unwrap().len(),
        64
    );
    let p50 = business["percentiles"]["ttfb_ms"]["p50"].as_f64().unwrap();
    assert!(
        p50 > 100.0 && p50 < 145.0,
        "the bucket holding 120 ms: {p50}"
    );
    assert!(
        business["percentiles"]["ttft_ms"]["p99"].is_f64()
            && business["percentiles"]["output_tps"]["p50"].is_f64()
    );
    assert_eq!(business["errors"]["upstream_error"].as_u64(), Some(1));
    let unattributed = &rows[2];
    assert_eq!(
        (
            unattributed["key"].as_str(),
            unattributed["rejected"].as_u64()
        ),
        (Some("-"), Some(1))
    );
    for filter in ["keys=-", "pools=-", "accounts=-"] {
        let drilled = json(
            get(
                &parts.app,
                &format!("/admin/requests?{filter}&limit=500"),
                Some(&cookie),
            )
            .await,
        )
        .await;
        assert_eq!(
            drilled["requests"].as_array().unwrap().len(),
            1,
            "the reserved id reaches the keyless request: {filter}"
        );
        assert_eq!(drilled["requests"][0]["id"], "nokey", "{filter}");
    }
    let filtered = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=model&accounts=direct",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(filtered["rows"][0]["requests"].as_u64(), Some(3));
    let agents = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=user_agent",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        agents["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["key"].as_str().unwrap(), r["requests"].as_u64().unwrap()))
            .collect::<Vec<_>>(),
        vec![(SDK_CLI, 3), (NEWER_CLI, 3), ("-", 1)],
        "two client builds and the request that named none"
    );
    // A comma is part of every measured agent, so the filter splits on newlines rather than commas.
    let one_agent = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=user_agent\
             &user_agents=claude-cli%2F2.1.300%20(external,%20cli)",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let groups: Vec<&str> = one_agent["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        groups, [NEWER_CLI; 3],
        "the series holds that build's three requests and nothing else"
    );
    let accounts = json(
        get(
            &parts.app,
            "/admin/analytics/accounts?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let business = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account_id"] == "business")
        .unwrap();
    assert_eq!(business["peak"]["5h"].as_f64(), Some(0.3));
    assert_eq!(
        get(
            &parts.app,
            "/admin/analytics/series?range=yesterday",
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        get(
            &parts.app,
            "/admin/analytics/series?range=0,9223372036854775807",
            Some(&cookie)
        )
        .await
        .status(),
        400,
        "a span past 400 days is refused before any bucket is sized"
    );
    assert_eq!(
        get(
            &parts.app,
            &format!(
                "/admin/analytics/breakdown?range={},{}",
                now - 500 * 86_400_000,
                now
            ),
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    // An aligned custom range: series and breakdown agree on which hours are in it.
    let hour = phoebis_ledger::hour_of(now) * 1_000;
    let aligned = json(
        get(
            &parts.app,
            &format!(
                "/admin/analytics/series?range={},{}&group_by=all",
                hour - 2 * HOUR,
                hour
            ),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        aligned["points"].as_array().unwrap().len(),
        2,
        "an aligned to excludes the hour it names"
    );
    let aligned_breakdown = json(
        get(
            &parts.app,
            &format!(
                "/admin/analytics/breakdown?range={},{}&group_by=all",
                hour - 2 * HOUR,
                hour
            ),
            Some(&cookie),
        )
        .await,
    )
    .await;
    let in_series: u64 = aligned["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .map(|g| g["requests"].as_u64().unwrap())
        .sum();
    assert_eq!(
        aligned_breakdown["rows"][0]["requests"]
            .as_u64()
            .unwrap_or(0),
        in_series,
        "breakdown and series read the same hours"
    );
    assert_eq!(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=session",
            Some(&cookie)
        )
        .await
        .status(),
        400
    );
    let viewer = login(&parts.app, "viewer-secret").await;
    assert_eq!(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=7d&group_by=key",
            Some(&viewer)
        )
        .await
        .status(),
        200
    );
}

#[tokio::test]
async fn the_request_log_filters_paginates_and_shows_one_row_per_request_with_its_trail() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    for i in 0..120i64 {
        let id = format!("r{i:03}");
        let model = if i % 3 == 0 {
            "claude-opus-5"
        } else {
            "Claude-Sonnet-5[1m]"
        };
        let mut attempts = vec![attempt(
            &id,
            "business",
            if i % 10 == 0 { 429 } else { 200 },
        )];
        if i % 10 == 0 {
            attempts[0].outcome = "rate_limited".into();
            attempts[0].cooldown_scope = Some("account".into());
            attempts[0].cooldown_until = Some(now + HOUR);
            attempts.push(AttemptRecord {
                attempt: 2,
                account_id: "direct".into(),
                ..attempt(&id, "direct", 200)
            });
        }
        let mut row = request(
            &id,
            now - i * 60_000,
            Some("desktop"),
            model,
            "succeeded",
            200,
            "inference",
        );
        row.attempts = attempts.len() as u32;
        row.user_agent = Some(
            if i % 4 == 0 {
                "claude-cli/2.1.300 (external, cli)"
            } else {
                "claude-cli/2.1.269 (external, sdk-cli)"
            }
            .into(),
        );
        if i == 7 {
            row.error_code = Some("weird_code".into());
        }
        parts
            .db
            .insert_request(
                &row,
                &attempts,
                (i == 5)
                    .then(|| BodyRecord {
                        request_id: id.clone(),
                        request_body: br#"{"model":"claude-sonnet-5","messages":[]}"#.to_vec(),
                        response_body: Some(b"event: message_stop\ndata: {}\n\n".to_vec()),
                        expires_at: now + HOUR,
                    })
                    .as_ref(),
            )
            .unwrap();
    }
    let cookie = login(&parts.app, "admin-secret").await;
    let page = json(get(&parts.app, "/admin/requests?limit=50", Some(&cookie)).await).await;
    let rows = page["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 50);
    assert_eq!(rows[0]["id"], "r000", "newest first");
    let failover = &rows[10];
    assert_eq!(failover["id"], "r010");
    assert_eq!(
        (
            failover["status"].as_u64(),
            failover["account_id"].as_str(),
            failover["attempts"].as_array().unwrap().len()
        ),
        (Some(200), Some("direct"), 2),
        "one row per request, the status of the answer, the serving account"
    );
    assert_eq!(
        (
            failover["attempts"][0]["account_id"].as_str(),
            failover["attempts"][0]["upstream_status"].as_u64(),
            failover["attempts"][0]["cooldown_set"]["until"].as_i64()
        ),
        (Some("business"), Some(429), Some(now + HOUR))
    );
    assert_eq!(
        (
            failover["attempts"][0]["windows"][0]["id"].as_str(),
            failover["attempts"][0]["windows"][0]["utilization"].as_f64()
        ),
        (Some("5h"), Some(0.3))
    );
    assert_eq!(
        (
            rows[1]["model"].as_str(),
            rows[1]["canonical_model"].as_str(),
            rows[1]["transport"].as_str(),
            rows[1]["ttft_ms"].as_i64()
        ),
        (
            Some("Claude-Sonnet-5[1m]"),
            Some("claude-sonnet-5[1m]"),
            Some("sse"),
            Some(300)
        )
    );
    assert_eq!(rows[1]["usage"]["cache_read"].as_u64(), Some(40));
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    let next = json(
        get(
            &parts.app,
            &format!("/admin/requests?limit=50&cursor={cursor}"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(next["requests"][0]["id"], "r050");
    let last = json(
        get(
            &parts.app,
            &format!(
                "/admin/requests?limit=50&cursor={}",
                next["next_cursor"].as_str().unwrap()
            ),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            last["requests"].as_array().unwrap().len(),
            last["next_cursor"].is_null()
        ),
        (20, true)
    );
    let opus = json(
        get(
            &parts.app,
            "/admin/requests?models=claude-opus-5&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(opus["requests"].as_array().unwrap().len(), 40);
    let direct = json(
        get(
            &parts.app,
            "/admin/requests?accounts=direct&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        direct["requests"].as_array().unwrap().len(),
        12,
        "an account filter matches any attempt"
    );
    let text = json(
        get(
            &parts.app,
            "/admin/requests?text=weird&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(text["requests"][0]["id"], "r007");
    assert_eq!(
        rows[0]["user_agent"].as_str(),
        Some("claude-cli/2.1.300 (external, cli)"),
        "the row names the client that made the request"
    );
    let agent = json(
        get(
            &parts.app,
            "/admin/requests?user_agent=2.1.300&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        agent["requests"].as_array().unwrap().len(),
        30,
        "a substring of the agent filters the log"
    );
    let window = json(
        get(
            &parts.app,
            &format!(
                "/admin/requests?from={}&to={}&limit=500",
                now - 5 * 60_000,
                now - 3 * 60_000
            ),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(window["requests"].as_array().unwrap().len(), 3);
    let statuses = json(
        get(
            &parts.app,
            "/admin/requests?statuses=200&outcomes=succeeded&kinds=inference&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(statuses["requests"].as_array().unwrap().len(), 120);
    assert_eq!(
        get(&parts.app, "/admin/requests?cursor=nonsense", Some(&cookie))
            .await
            .status(),
        400
    );
    let detail = json(get(&parts.app, "/admin/requests/r010", Some(&cookie)).await).await;
    assert_eq!(
        (
            detail["request"]["id"].as_str(),
            detail["request"]["attempts"].as_array().unwrap().len()
        ),
        (Some("r010"), 2)
    );
    assert!(detail["events"].is_array());
    assert_eq!(
        get(&parts.app, "/admin/requests/nope", Some(&cookie))
            .await
            .status(),
        404
    );
    let bodies = json(get(&parts.app, "/admin/requests/r005/bodies", Some(&cookie)).await).await;
    assert_eq!(
        (
            bodies["request"]["model"].as_str(),
            bodies["response"]
                .as_str()
                .map(|s| s.starts_with("event: message_stop")),
            bodies["expires_at"].as_i64()
        ),
        (Some("claude-sonnet-5"), Some(true), Some(now + HOUR))
    );
    assert_eq!(
        get(&parts.app, "/admin/requests/r006/bodies", Some(&cookie))
            .await
            .status(),
        404
    );
    let viewer = login(&parts.app, "viewer-secret").await;
    assert_eq!(
        get(&parts.app, "/admin/requests/r005/bodies", Some(&viewer))
            .await
            .status(),
        403,
        "captured bodies are admin only"
    );
    assert_eq!(
        get(&parts.app, "/admin/requests/r005", Some(&viewer))
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn the_request_log_reads_a_key_selector_by_lifetime() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    // The store stamps the key from the wall clock, so the live generation is read back rather than assumed.
    let live = parts.store.api_key("desktop").unwrap().unwrap().created_at;
    let earlier = live - 60_000;
    for (id, generation) in [("r-live", live), ("r-earlier", earlier)] {
        parts
            .db
            .insert_request(
                &RequestRecord {
                    key_generation: generation,
                    ..request(
                        id,
                        now - 60_000,
                        Some("desktop"),
                        "claude-sonnet-5",
                        "succeeded",
                        200,
                        "inference",
                    )
                },
                &[attempt(id, "direct", 200)],
                None,
            )
            .unwrap();
    }
    let ids = |body: serde_json::Value| -> Vec<String> {
        body["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let bare = json(
        get(
            &parts.app,
            "/admin/requests?keys=desktop&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(bare),
        vec!["r-live"],
        "a bare id is the live generation"
    );
    let named = json(
        get(
            &parts.app,
            &format!("/admin/requests?keys=desktop@{earlier}&limit=500"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(named),
        vec!["r-earlier"],
        "the earlier holder is named in full"
    );
    let none = json(
        get(
            &parts.app,
            "/admin/requests?keys=laptop&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        ids(none).is_empty(),
        "an id no live key holds selects nothing"
    );
}

#[tokio::test]
async fn the_dashboard_reads_the_last_day_the_latest_quota_and_the_live_pins() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    for i in 0..30 {
        let id = format!("d{i}");
        parts
            .db
            .insert_request(
                &request(
                    &id,
                    now - i * HOUR,
                    Some("desktop"),
                    "claude-sonnet-5",
                    if i % 5 == 0 { "failed" } else { "succeeded" },
                    if i % 5 == 0 { 529 } else { 200 },
                    "inference",
                ),
                &[attempt(&id, "business", 200)],
                None,
            )
            .unwrap();
    }
    parts
        .db
        .insert_samples(
            &[
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - 2 * HOUR),
                    window: "5h".into(),
                    utilization: Some(0.5),
                    reset_at: Some(now + HOUR),
                    source: "header".into(),
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - HOUR),
                    window: "5h".into(),
                    utilization: Some(0.6),
                    reset_at: Some(now + HOUR),
                    source: "poll".into(),
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - HOUR),
                    window: "7d".into(),
                    utilization: Some(0.2),
                    reset_at: None,
                    source: "poll".into(),
                },
            ],
            None,
        )
        .unwrap();
    let cookie = login(&parts.app, "admin-secret").await;
    let dashboard = json(get(&parts.app, "/admin/dashboard", Some(&cookie)).await).await;
    assert_eq!(
        dashboard["requests_24h"].as_u64(),
        Some(24),
        "the 24 hourly rows ending now"
    );
    assert_eq!(
        (
            dashboard["tokens_24h"].as_u64(),
            dashboard["tokens_cached_24h"].as_u64()
        ),
        (Some(24 * 150), Some(24 * 40))
    );
    assert!((dashboard["cost_24h"].as_f64().unwrap() - 0.24).abs() < 1e-9);
    assert!((dashboard["error_rate_24h"].as_f64().unwrap() - 5.0 / 24.0).abs() < 1e-9);
    assert_eq!(dashboard["top_error_code"].as_str(), Some("upstream_error"));
    assert_eq!(dashboard["hourly"].as_array().unwrap().len(), 24);
    assert_eq!(dashboard["recent"].as_array().unwrap().len(), 20);
    assert_eq!(dashboard["recent"][0]["id"], "d0");
    let business = dashboard["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account_id"] == "business")
        .unwrap();
    let five = business["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == "5h")
        .unwrap();
    assert_eq!(
        five["utilization"].as_f64(),
        Some(0.6),
        "the newest sample per window"
    );
    assert_eq!(dashboard["sessions"]["active_pins"].as_u64(), Some(0));
}
