//! The analytics, request-log, and dashboard routes over rows written straight into the ledger.
mod common;
use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
use common::{get, json, login, post, wait_json};

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

/// Six inference requests over two client builds, an agent type every third row, and a turn origin every
/// other row, so a filter or a group-by on any of the three has more than one group to answer.
fn seed_rollups(parts: &common::Full) {
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
        row.effort = (i % 3 == 0).then(|| "high".into());
        row.client = Some(
            if i % 2 == 0 {
                "claude-code/sdk-cli"
            } else {
                "claude-code/cli"
            }
            .into(),
        );
        row.agent_type = (i % 3 == 0).then(|| "Explore".into());
        row.turn_origin = (i % 2 == 0).then(|| "human".into());
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
}

#[tokio::test]
async fn analytics_series_breakdown_and_accounts_read_the_rollups_with_percentiles() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    seed_rollups(&parts);
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
    let same_three = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=user_agent\
             &user_agents=claude-cli%2F2.1.3*",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let groups: Vec<&str> = same_three["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        groups, [NEWER_CLI; 3],
        "a pattern with no exact match still selects that build's three requests"
    );
    let grouped = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=user_agent\
             &user_agent_groups=claude-cli%2F2.1.3*%0Aclaude-cli%2F*",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let by_group: std::collections::BTreeMap<&str, u64> = grouped["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["key"].as_str().unwrap(), r["requests"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        by_group,
        std::collections::BTreeMap::from([
            ("claude-cli/2.1.3*", 3),
            ("claude-cli/*", 3),
            ("other", 1)
        ]),
        "a pattern list names one group per pattern and files the rest under other"
    );
    let by_effort = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=effort",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let effort_rows = by_effort["rows"].as_array().unwrap();
    assert_eq!(
        (
            effort_rows
                .iter()
                .map(|r| r["key"].as_str().unwrap())
                .collect::<Vec<_>>(),
            effort_rows
                .iter()
                .map(|r| r["requests"].as_u64().unwrap())
                .sum::<u64>()
        ),
        (vec!["high", "none"], 7),
        "the seeded rows split by effort and sum to the total"
    );
    let too_many_patterns = get(
        &parts.app,
        &format!(
            "/admin/analytics/breakdown?range=24h&user_agents={}",
            vec!["claude-cli/*"; 33].join("%0A")
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(too_many_patterns.status(), 400);
    assert_eq!(
        json(too_many_patterns).await["error"],
        "patterns are at most 256 characters, at most 32 of them"
    );
    let one_long_pattern = get(
        &parts.app,
        &format!(
            "/admin/analytics/breakdown?range=24h&user_agents={}",
            "a".repeat(257)
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        one_long_pattern.status(),
        400,
        "a single pattern past the length bound"
    );
    assert_eq!(
        json(one_long_pattern).await["error"],
        "patterns are at most 256 characters, at most 32 of them"
    );
    let long_agent = get(
        &parts.app,
        &format!("/admin/requests?user_agent={}&limit=500", "a".repeat(257)),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        long_agent.status(),
        400,
        "the request log bounds its one pattern the same way"
    );
    assert_eq!(
        json(long_agent).await["error"],
        "patterns are at most 256 characters, at most 32 of them"
    );
    let at_the_bound = get(
        &parts.app,
        &format!(
            "/admin/analytics/breakdown?range=24h&user_agents={}",
            vec!["a".repeat(256); 32].join("%0A")
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        at_the_bound.status(),
        200,
        "32 patterns of 256 characters each is still within bounds"
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
async fn the_request_log_answers_and_filters_the_client_the_agent_type_and_the_turn_origin() {
    let parts = common::admin_parts_full().await;
    seed_rollups(&parts);
    let cookie = login(&parts.app, "admin-secret").await;
    let all = json(get(&parts.app, "/admin/requests?limit=500", Some(&cookie)).await).await;
    let rows = all["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 6);
    for row in rows {
        let row = row.as_object().unwrap();
        for key in [
            "client",
            "agent_type",
            "compaction",
            "context_compacted",
            "prev_tool_ms",
            "turn_origin",
        ] {
            assert!(row.contains_key(key), "{key} is missing from a request row");
        }
    }
    let by_pattern = json(
        get(
            &parts.app,
            "/admin/requests?client=claude-code%2Fsdk*&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let by_exact = json(
        get(
            &parts.app,
            "/admin/requests?client=claude-code%2Fsdk-cli&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        by_pattern["requests"].as_array().unwrap().len(),
        3,
        "the even rows carry the sdk-cli client alone"
    );
    assert_eq!(
        by_pattern["requests"].as_array().unwrap().len(),
        by_exact["requests"].as_array().unwrap().len(),
        "the pattern and the exact spelling answer the same rows"
    );
    let by_agent_type = json(
        get(
            &parts.app,
            "/admin/requests?agent_types=Explore&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        by_agent_type["requests"].as_array().unwrap().len(),
        2,
        "every third row drew the agent type"
    );
    let by_origin = json(
        get(
            &parts.app,
            "/admin/requests?turn_origins=human&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(by_origin["requests"].as_array().unwrap().len(), 3);
    let by_origins = json(
        get(
            &parts.app,
            "/admin/requests?turn_origins=human,scheduled&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        by_origins["requests"].as_array().unwrap().len(),
        3,
        "an origin nothing matches leaves the count unchanged"
    );
    let too_long = get(
        &parts.app,
        &format!("/admin/requests?client={}&limit=500", "a".repeat(257)),
        Some(&cookie),
    )
    .await;
    assert_eq!(too_long.status(), 400);
    assert_eq!(
        json(too_long).await["error"],
        "patterns are at most 256 characters, at most 32 of them"
    );
}

#[tokio::test]
async fn the_analytics_group_by_client_and_narrow_to_client_patterns() {
    let parts = common::admin_parts_full().await;
    seed_rollups(&parts);
    let cookie = login(&parts.app, "admin-secret").await;
    let breakdown = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=client",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let rows = breakdown["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["claude-code/cli", "claude-code/sdk-cli"],
        "the two client builds the seed drew from"
    );
    assert_eq!(
        rows.iter()
            .map(|r| r["requests"].as_u64().unwrap())
            .sum::<u64>(),
        6,
        "every seeded request carries a client"
    );
    let narrowed = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=client&clients=claude-code%2Fsdk*",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        narrowed["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["claude-code/sdk-cli"],
        "the clients pattern narrows the breakdown to the one build"
    );
    let bad_group = get(
        &parts.app,
        "/admin/analytics/breakdown?range=24h&group_by=nope",
        Some(&cookie),
    )
    .await;
    assert_eq!(bad_group.status(), 400);
    assert_eq!(
        json(bad_group).await["error"],
        "group_by is account, all, client, effort, key, kind, model, pool, request_class, speed, or user_agent"
    );
    let by_speed = get(
        &parts.app,
        "/admin/analytics/breakdown?range=24h&group_by=speed",
        Some(&cookie),
    )
    .await;
    assert_eq!(by_speed.status(), 200);
    assert_eq!(
        json(by_speed).await["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["standard"],
        "no seeded answer named a speed"
    );
    let served_fast = json(
        get(
            &parts.app,
            "/admin/requests?speeds=fast&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        served_fast["requests"].as_array().unwrap().len(),
        0,
        "the request log reads the speeds filter"
    );
    let too_many_clients = get(
        &parts.app,
        &format!(
            "/admin/analytics/breakdown?range=24h&group_by=client&clients={}",
            vec!["claude-code/cli"; 33].join("%0A")
        ),
        Some(&cookie),
    )
    .await;
    assert_eq!(too_many_clients.status(), 400);
    assert_eq!(
        json(too_many_clients).await["error"],
        "patterns are at most 256 characters, at most 32 of them"
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
                        request_len_seen: br#"{"model":"claude-sonnet-5","messages":[]}"#.len()
                            as u64,
                        response_len_seen: b"event: message_stop\ndata: {}\n\n".len() as u64,
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
            "/admin/requests?user_agent=%2A2.1.300%2A&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        agent["requests"].as_array().unwrap().len(),
        30,
        "a pattern around the version filters the log"
    );
    let exact = json(
        get(
            &parts.app,
            "/admin/requests?user_agent=claude-cli/2.1.300%20(external,%20cli)&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        exact["requests"].as_array().unwrap().len(),
        30,
        "no wildcard is an exact match"
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
async fn a_bodies_route_answers_the_bytes_each_side_stored_and_saw() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    let cookie = login(&parts.app, "admin-secret").await;
    // Two multibyte characters, 4 bytes for the pair: proves the byte counts below are not a character count.
    let request_body = r#"{"text":"éé"}"#.as_bytes().to_vec();
    let request_len = request_body.len() as u64;
    let response_body = "a".repeat(1_000).into_bytes();
    let mut row = request(
        "r-multibyte",
        now,
        Some("desktop"),
        "claude-sonnet-5",
        "succeeded",
        200,
        "inference",
    );
    row.bodies_captured = true;
    parts.ledger.record(Record::Request {
        request: row,
        attempts: vec![attempt("r-multibyte", "business", 200)],
        bodies: Some(BodyRecord {
            request_id: "r-multibyte".into(),
            request_len_seen: request_len,
            response_len_seen: 1_010,
            request_body,
            response_body: Some(response_body),
            expires_at: now + HOUR,
        }),
    });
    let bodies = wait_json(
        &parts.app,
        "/admin/requests/r-multibyte/bodies",
        &cookie,
        |v| v.get("response_bytes_seen").is_some(),
    )
    .await;
    assert_eq!(
        (
            bodies["request_bytes_stored"].as_u64(),
            bodies["request_bytes_seen"].as_u64(),
            bodies["response_bytes_stored"].as_u64(),
            bodies["response_bytes_seen"].as_u64(),
            bodies["request_truncated"].as_bool(),
            bodies["response_truncated"].as_bool(),
        ),
        (
            Some(request_len),
            Some(request_len),
            Some(1_000),
            Some(1_010),
            Some(false),
            Some(true)
        ),
        "bytes measured on the raw vectors, not the rendered JSON or a character count"
    );
}

/// A capturing request straight onto the bus: `delivered` of its response chunks reach the ledger while its
/// completion says `published` went out, so the rest were dropped at the bus.
fn publish_capture(bus: &phoebis_bus::Bus, id: &str, delivered: u32, published: u32) {
    use phoebis_bus::{
        Event, Kind, Origin, Outcome, RequestComplete, RequestObserved, RequestShape,
        ResponseChunk, ResponseFacts,
    };
    bus.publish(Event::RequestObserved(RequestObserved {
        request_id: id.into(),
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
    for index in 0..delivered {
        bus.publish(Event::ResponseChunk(ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: bytes::Bytes::from_static(b"partial"),
            phase: None,
        }));
    }
    bus.publish(Event::RequestComplete(RequestComplete {
        request_id: id.into(),
        attempt: 0,
        account_id: None,
        outcome: Outcome::Succeeded,
        status: 200,
        usage: None,
        response: ResponseFacts::default(),
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: None,
        client_drift: None,
        client_version: None,
        response_bytes: None,
        response_chunks: Some(published),
        finished_at: common::NOW + 900,
    }));
}

#[tokio::test]
async fn a_capture_the_bus_cut_short_reads_truncated_on_the_bodies_route() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    publish_capture(&parts.bus, "r-last-dropped", 1, 2);
    publish_capture(&parts.bus, "r-none-arrived", 0, 1);
    for id in ["r-last-dropped", "r-none-arrived"] {
        let bodies = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait_json(
                &parts.app,
                &format!("/admin/requests/{id}/bodies"),
                &cookie,
                |v| v.get("response_truncated").is_some(),
            ),
        )
        .await
        .expect("the capture landed");
        // Neither the stored bytes nor the seen count include a chunk the bus dropped, so only the request
        // row knows the capture is short.
        assert_eq!(
            (
                bodies["request_truncated"].as_bool(),
                bodies["response_truncated"].as_bool()
            ),
            (Some(false), Some(true)),
            "{id}"
        );
    }
}

#[tokio::test]
async fn the_request_log_reads_a_key_selector_by_lifetime() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}]}"#, Some(&cookie)).await;
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
                    observed_at: None,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - HOUR),
                    window: "5h".into(),
                    utilization: Some(0.6),
                    reset_at: Some(now + HOUR),
                    source: "poll".into(),
                    observed_at: None,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - HOUR),
                    window: "7d".into(),
                    utilization: Some(0.2),
                    reset_at: None,
                    source: "poll".into(),
                    observed_at: None,
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
