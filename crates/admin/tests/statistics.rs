//! The analytics, request-log, and dashboard routes over rows written straight into the ledger.
mod common;
use common::{get, json, login, patch, post, wait_json};
use phoebis_ledger::{AttemptRecord, BodyRecord, Record, RequestRecord};
use phoebis_store::AccountKind;

const HOUR: i64 = 3_600_000;

/// Two client builds, so a group-by over them has more than one group; `nokey` sends neither.
const SDK_CLI: &str = "claude-cli/2.1.269 (external, sdk-cli)";
const NEWER_CLI: &str = "claude-cli/2.1.300 (external, cli)";
/// The product tokens the hourly rollups key the two builds under.
const SDK_CLI_TOKEN: &str = "claude-cli/2.1.269";
const NEWER_CLI_TOKEN: &str = "claude-cli/2.1.300";

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
        account_kind: AccountKind::Oauth,
        started_at: common::NOW,
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
        "sorted by label; the reserved id is labeled"
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
    // The three business rows `seed_rollups` lands, each 100 input, 40 cache-read and 10 output tokens of Sonnet 5 at
    // the shipped $2, $0.20 and $10 a million: $0.000308 a row.
    assert!(
        (business["cost_usd"].as_f64().unwrap() - 0.000924).abs() < 1e-9,
        "{} vs 0.000924",
        business["cost_usd"]
    );
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
        vec![(SDK_CLI_TOKEN, 3), (NEWER_CLI_TOKEN, 3), ("-", 1)],
        "two client builds by their product tokens, and the request that named none"
    );
    // The analytics agent filter matches the product token the rollups keep, not the raw header.
    let one_agent = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=user_agent\
             &user_agents=claude-cli%2F2.1.300",
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
        groups, [NEWER_CLI_TOKEN; 3],
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
        groups, [NEWER_CLI_TOKEN; 3],
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
    assert_eq!(business["peak"]["five_hour"].as_f64(), Some(0.3));
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
}

#[tokio::test]
async fn the_accounts_route_answers_each_hour_s_last_window_sample_with_its_lead_and_null_without_one(
) {
    let parts = common::admin_parts_full().await;
    // Every instant sits at or after the epoch: the hour floor is proved on its own, and the samples are each
    // observed past their bucket's floor so the wire's instant is told apart from it.
    let five_hour_at = HOUR + 1_500_123;
    let seven_day_at = 2 * HOUR + 300_456;
    let sample = |window: &str, at: i64, utilization: Option<f64>, reset_at: i64| {
        phoebis_ledger::QuotaSampleRecord {
            account_id: "business".into(),
            bucket: phoebis_ledger::bucket_of(at),
            window: window.into(),
            utilization,
            reset_at: Some(reset_at),
            source: phoebis_ledger::SampleSource::Poll,
            observed_at: at,
        }
    };
    parts
        .db
        .insert_samples(
            &[
                sample(
                    "five_hour",
                    five_hour_at,
                    Some(0.62),
                    five_hour_at + 4 * HOUR,
                ),
                sample(
                    "seven_day",
                    seven_day_at,
                    Some(0.41),
                    seven_day_at + 6 * 24 * HOUR,
                ),
                sample(
                    "seven_day_fable",
                    seven_day_at,
                    None,
                    seven_day_at + 6 * 24 * HOUR,
                ),
            ],
            None,
        )
        .unwrap();
    // A third hour with an attempt and no sample, so its rollup row lists it with every window null.
    let attempted_at = 3 * HOUR + 60_000;
    let mut served = attempt("w1", "business", 200);
    served.started_at = attempted_at;
    parts
        .db
        .insert_request(
            &request(
                "w1",
                attempted_at,
                Some("desktop"),
                "claude-sonnet-5",
                "succeeded",
                200,
                "inference",
            ),
            &[served],
            None,
        )
        .unwrap();
    let cookie = login(&parts.app, "admin-secret").await;
    let answer = json(
        get(
            &parts.app,
            &format!("/admin/analytics/accounts?range={},{}", HOUR, 4 * HOUR),
            Some(&cookie),
        )
        .await,
    )
    .await;
    let business = answer["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account_id"] == "business")
        .unwrap();
    let hours = business["hours"].as_array().unwrap();
    assert_eq!(
        hours
            .iter()
            .map(|h| h["hour"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![HOUR, 2 * HOUR, 3 * HOUR],
        "the two sampled hours without an attempt and the attempted hour without a sample are all listed"
    );
    let first = &hours[0];
    assert!(
        first["utilization_5h_max"].is_null() && first["pins_created"] == 0,
        "an hour with samples and no attempt carries no peak and no count: {first}"
    );
    // Indexing a missing `windows` would read null and pass the null checks below, so the object is pinned first.
    assert_eq!(
        first["windows"]
            .as_object()
            .map(|w| w.keys().cloned().collect::<Vec<_>>()),
        Some(vec![
            "five_hour".to_string(),
            "seven_day".to_string(),
            "seven_day_fable".to_string()
        ]),
        "the sample-only hour's windows is an object over the three base keys: {first}"
    );
    let five = &first["windows"]["five_hour"];
    assert_eq!(five["utilization"].as_f64(), Some(0.62));
    assert_eq!(five["reset_at"].as_i64(), Some(five_hour_at + 4 * HOUR));
    assert_eq!(
        five["sampled_at"].as_i64(),
        Some(five_hour_at),
        "the sample's own instant, not its bucket's floor"
    );
    let lead = five["lead"].as_f64().unwrap();
    assert!(
        (lead - 0.42).abs() < 1e-9,
        "scored at the sample's instant, one hour of five elapsed: {lead}"
    );
    assert!(
        first["windows"]["seven_day"].is_null() && first["windows"]["seven_day_fable"].is_null(),
        "the windows the hour holds no sample of read null: {first}"
    );
    let second = &hours[1];
    assert!(second["windows"]["five_hour"].is_null());
    let seven = &second["windows"]["seven_day"];
    assert_eq!(seven["utilization"].as_f64(), Some(0.41));
    assert_eq!(seven["sampled_at"].as_i64(), Some(seven_day_at));
    let seven_lead = seven["lead"].as_f64().unwrap();
    assert!(
        (seven_lead - (0.41 - 1.0 / 7.0)).abs() < 1e-9,
        "a day of the week elapsed by the sample's instant: {seven_lead}"
    );
    let fable = &second["windows"]["seven_day_fable"];
    assert!(
        fable["utilization"].is_null() && fable["lead"].is_null(),
        "a sample that reported the reset alone has no lead: {fable}"
    );
    assert_eq!(
        fable["reset_at"].as_i64(),
        Some(seven_day_at + 6 * 24 * HOUR)
    );
    assert_eq!(fable["sampled_at"].as_i64(), Some(seven_day_at));
    let third = &hours[2];
    assert_eq!(third["utilization_5h_max"].as_f64(), Some(0.3));
    assert_eq!(
        third["windows"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), v.is_null()))
            .collect::<Vec<_>>(),
        vec![
            ("five_hour", true),
            ("seven_day", true),
            ("seven_day_fable", true)
        ],
        "an hour with no sample holds the three windows as nulls and no other id"
    );
    assert_eq!(
        business["peak"]["five_hour"].as_f64(),
        Some(0.3),
        "the peaks still come from the attempts' heads"
    );
}

#[tokio::test]
async fn the_accounts_route_runs_a_canceled_account_s_lead_to_its_end_inside_the_sampled_week() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A day of the week gone at the sample and the end a day after it: half the span to the end has elapsed,
    // where a seventh of the week has to the reset.
    let sampled_at = 2 * HOUR + 300_456;
    let end = sampled_at + 24 * HOUR;
    parts
        .db
        .insert_samples(
            &[phoebis_ledger::QuotaSampleRecord {
                account_id: "business".into(),
                bucket: phoebis_ledger::bucket_of(sampled_at),
                window: "seven_day".into(),
                utilization: Some(0.41),
                reset_at: Some(sampled_at + 6 * 24 * HOUR),
                source: phoebis_ledger::SampleSource::Poll,
                observed_at: sampled_at,
            }],
            None,
        )
        .unwrap();
    let canceled = patch(
        &parts.app,
        "/admin/accounts/business",
        &format!(r#"{{"canceled":true,"subscription_ends_at":{end}}}"#),
        Some(&cookie),
    )
    .await;
    assert_eq!(canceled.status(), 200);
    let answer = json(
        get(
            &parts.app,
            &format!("/admin/analytics/accounts?range={},{}", HOUR, 4 * HOUR),
            Some(&cookie),
        )
        .await,
    )
    .await;
    let business = answer["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account_id"] == "business")
        .unwrap();
    let hour = business["hours"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["hour"] == 2 * HOUR)
        .unwrap();
    let lead = hour["windows"]["seven_day"]["lead"].as_f64().unwrap();
    assert!(
        (lead - (0.41 - 0.5)).abs() < 1e-9,
        "the pace runs to the end, a day gone of the two to it, steeper than a seventh of the week: {lead}"
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
        "group_by is account, all, classifier, client, effort, key, kind, model, pool, request_class, speed, or user_agent"
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
    let served_standard = json(
        get(
            &parts.app,
            "/admin/requests?speeds=standard&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        served_standard["requests"].as_array().unwrap().len(),
        6,
        "an answer that named no speed lists under standard"
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

/// One plan-funded request marked as the client's classifier call and one unmarked, both served on the Ultra tier
/// of the test cost model, which sizes both shared windows and weighs the model, so the unmarked one prices above
/// zero without a figure read from the shipped files.
async fn classifier_pair() -> common::Full {
    let parts =
        common::admin_parts_full_costed(phoebis_ledger::test_tables::subscription_costs()).await;
    for (id, classifier) in [("r-classifier", Some("client")), ("r-main", None)] {
        let mut row = request(
            id,
            common::NOW - HOUR,
            Some("desktop"),
            "claude-sonnet-5-5",
            "succeeded",
            200,
            "inference",
        );
        row.request_class = classifier.map(|_| "auxiliary".into());
        row.classifier = classifier.map(String::from);
        let mut served = attempt(id, "business", 200);
        served.tier = Some("default_claude_max_20x".into());
        parts.db.insert_request(&row, &[served], None).unwrap();
    }
    parts
}

#[tokio::test]
async fn the_request_log_reads_the_classifiers_filter_and_the_show_route_prices_a_classifier_call_at_no_share(
) {
    let parts = classifier_pair().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for (filter, listed) in [("client", "r-classifier"), ("-", "r-main")] {
        let page = json(
            get(
                &parts.app,
                &format!("/admin/requests?classifiers={filter}&limit=500"),
                Some(&cookie),
            )
            .await,
        )
        .await;
        assert_eq!(
            listed_ids(&page),
            vec![listed],
            "classifiers={filter}: {page}"
        );
    }
    let marked = json(get(&parts.app, "/admin/requests/r-classifier", Some(&cookie)).await).await
        ["request"]
        .clone();
    assert_eq!(
        (
            &marked["classifier"],
            &marked["share_reason"],
            marked.get("estimated")
        ),
        (
            &serde_json::json!("client"),
            &serde_json::json!("client_classifier"),
            None
        ),
        "{marked}"
    );
    assert_eq!(
        (&marked["cost_shares"], &marked["cost_equivalent"]),
        (
            &serde_json::json!({ "five_hour": 0.0, "seven_day": 0.0 }),
            &serde_json::json!(0.0)
        ),
        "each window the tier sizes and the Pro-week figure read zero by rule: {marked}"
    );
    let plain = json(get(&parts.app, "/admin/requests/r-main", Some(&cookie)).await).await
        ["request"]
        .clone();
    assert!(
        plain["classifier"].is_null()
            && plain["share_reason"].is_null()
            && plain["cost_shares"]["five_hour"]
                .as_f64()
                .is_some_and(|share| share > 0.0),
        "an unmarked request prices from its counts and carries no reason: {plain}"
    );
}

/// The breakdown answers the shares only where a filter names one account, so the grouped read without one asserts
/// the Pro-week figure and the filtered read the shares.
#[tokio::test]
async fn the_analytics_group_on_the_classifier_dimension() {
    let parts = classifier_pair().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let group = |body: &serde_json::Value, key: &str| {
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("a {key} group: {body}"))
    };
    let breakdown = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=classifier",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let mut keys: Vec<&str> = breakdown["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["-", "client"], "{breakdown}");
    let (marked, plain) = (group(&breakdown, "client"), group(&breakdown, "-"));
    assert!(
        marked["cost_usd"].as_f64().is_some_and(|usd| usd > 0.0),
        "the marked group keeps its dollar estimate: {marked}"
    );
    assert_eq!(marked["cost_equivalent"].as_f64(), Some(0.0), "{marked}");
    assert!(
        plain["cost_equivalent"]
            .as_f64()
            .is_some_and(|equivalent| equivalent > 0.0),
        "{plain}"
    );
    let one_account = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=classifier&accounts=business",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let marked = group(&one_account, "client");
    for window in ["five_hour", "seven_day"] {
        assert_eq!(
            marked["cost_shares"][window].as_f64(),
            Some(0.0),
            "{window}: {marked}"
        );
    }
    let series = get(
        &parts.app,
        "/admin/analytics/series?range=24h&group_by=classifier",
        Some(&cookie),
    )
    .await;
    assert_eq!(series.status(), 200);
    let series = json(series).await;
    let mut keys: Vec<&str> = series["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys, ["-", "client"], "{series}");
    let burn = get(
        &parts.app,
        "/admin/analytics/burn?range=24h&group_by=classifier",
        Some(&cookie),
    )
    .await;
    assert_eq!(burn.status(), 200, "the burn groups on the dimension too");
    let burn = json(burn).await;
    let mut groups: Vec<&str> = burn["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["group"].as_str().unwrap())
        .collect();
    groups.sort_unstable();
    groups.dedup();
    assert_eq!(groups, ["-", "client"], "{burn}");
}

#[tokio::test]
async fn a_contract_refusal_lists_its_item() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let refused = RequestRecord {
        attempts: 0,
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: Some("client_contract".into()),
        client_contract: Some("class_header".into()),
        refused: true,
        ..request(
            "r-refused",
            now - HOUR,
            Some("desktop"),
            "claude-sonnet-5-5",
            "rejected",
            403,
            "inference",
        )
    };
    parts.db.insert_request(&refused, &[], None).unwrap();
    parts
        .db
        .insert_request(
            &request(
                "r-served",
                now - HOUR,
                Some("desktop"),
                "claude-sonnet-5-5",
                "succeeded",
                200,
                "inference",
            ),
            &[attempt("r-served", "business", 200)],
            None,
        )
        .unwrap();
    let page = json(get(&parts.app, "/admin/requests?limit=500", Some(&cookie)).await).await;
    let item = |id: &str| {
        page["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .map(|r| r["client_contract"].clone())
    };
    assert_eq!(
        (item("r-refused"), item("r-served")),
        (
            Some(serde_json::json!("class_header")),
            Some(serde_json::Value::Null)
        ),
        "{page}"
    );
    let shown = json(get(&parts.app, "/admin/requests/r-refused", Some(&cookie)).await).await;
    assert_eq!(
        shown["request"]["client_contract"], "class_header",
        "{shown}"
    );
}

#[tokio::test]
async fn a_request_answers_its_hold_with_its_wait_and_held_filters_the_list() {
    use phoebis_bus::{Event, Outcome, RequestComplete, ResponseFacts};
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let held = serde_json::to_string(&phoebis_routing::cohort_hold(&[
        phoebis_routing::HoldFacts {
            account_id: "business".into(),
            cohort: None,
            held_ms: 1_250,
            attempt: 1,
            seq: 1,
            ran_out: false,
            reason: "cap",
            until: None,
            waits_for: None,
        },
    ]))
    .unwrap();
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            held: Some(held.clone()),
            ..request(
                "r-held",
                now - 60_000,
                Some("desktop"),
                "claude-sonnet-5",
                "succeeded",
                200,
                "inference",
            )
        },
        attempts: vec![attempt("r-held", "business", 200)],
        bodies: None,
    });
    parts.ledger.record(Record::Request {
        request: request(
            "r-plain",
            now - 120_000,
            Some("desktop"),
            "claude-sonnet-5",
            "succeeded",
            200,
            "inference",
        ),
        attempts: vec![attempt("r-plain", "business", 200)],
        bodies: None,
    });
    // The bound ran out with every upstream still at its cap, so the relay answered without a dispatch and the
    // completion is the only place the wait is written.
    parts.bus.publish(observed("r-at-cap"));
    parts.bus.publish(Event::RequestComplete(RequestComplete {
        request_id: "r-at-cap".into(),
        attempt: 0,
        account_id: None,
        outcome: Outcome::Exhausted,
        status: 429,
        usage: None,
        response: ResponseFacts::default(),
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: Some("accounts_at_cap".into()),
        client_drift: None,
        client_contract: None,
        client_version: None,
        response_bytes: None,
        response_chunks: None,
        held: Some(held),
        diagnostics: None,
        message_id: None,
        skipped_events: 0,
        interruption: None,
        finished_at: now + 1_300,
    }));
    let all = wait_json(&parts.app, "/admin/requests?limit=500", &cookie, |v| {
        v["requests"].as_array().unwrap().len() == 3
    })
    .await;
    let row = |page: &serde_json::Value, id: &str| {
        page["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} is listed: {page}"))
    };
    assert_eq!(
        row(&all, "r-held")["held"],
        serde_json::json!({
            "waited_on": ["business"],
            "cohort": null,
            "held_ms": 1_250,
            "waits": [{ "reason": "cap", "held_ms": 1_250 }]
        }),
        "the row answers the holds it waited through with the wait"
    );
    assert_eq!(row(&all, "r-plain")["held"], serde_json::Value::Null);
    let at_cap = row(&all, "r-at-cap");
    assert_eq!(
        (
            at_cap["held"]["held_ms"].as_u64(),
            at_cap["attempts"].as_array().unwrap().len()
        ),
        (Some(1_250), 0),
        "a hold with no attempt after it still reads as held"
    );
    let held_only = json(
        get(
            &parts.app,
            "/admin/requests?held=1&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let mut ids: Vec<&str> = held_only["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        ["r-at-cap", "r-held"],
        "held keeps the requests that waited for a slot"
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
        if i == 3 || i == 4 {
            row.session_id = Some("s-pair".into());
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
        (Some("five_hour"), Some(0.3))
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
        "an account filter matches the serving attempt"
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
    let ids = |body: serde_json::Value| -> Vec<String> {
        body["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let one = json(
        get(
            &parts.app,
            "/admin/requests?request=r007&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(ids(one), ["r007"], "request answers the one row");
    let prefix = json(
        get(
            &parts.app,
            "/admin/requests?request=r0&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(prefix),
        Vec::<String>::new(),
        "request matches the id whole, never a prefix"
    );
    let session = json(
        get(
            &parts.app,
            "/admin/requests?session=s-pair&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(session),
        ["r003", "r004"],
        "session answers the session's rows"
    );
    let unknown = json(
        get(
            &parts.app,
            "/admin/requests?session=nope&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(unknown),
        Vec::<String>::new(),
        "an unknown session answers none"
    );
    let both = json(
        get(
            &parts.app,
            "/admin/requests?request=r007&text=r00&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(ids(both), ["r007"], "request stands beside text");
    let neither = json(
        get(
            &parts.app,
            "/admin/requests?request=r007&text=r01&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(neither),
        Vec::<String>::new(),
        "text still narrows a request match"
    );
    let paired = json(
        get(
            &parts.app,
            "/admin/requests?session=s-pair&text=r004&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(ids(paired), ["r004"], "session stands beside text");
    let narrowed = json(
        get(
            &parts.app,
            "/admin/requests?session=s-pair&text=r00&limit=500",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(narrowed),
        ["r003", "r004"],
        "session narrows a text that matches more rows"
    );
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
}

/// A poll passes back the newest write instant of its last answer as `landed_after` and reads the rows the ledger
/// wrote since, beside the other filters, a late row whose arrival sorts below the others included.
#[tokio::test]
async fn the_request_log_reads_the_rows_written_after_landed_after_and_answers_the_newest_stamp() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    let cookie = login(&parts.app, "admin-secret").await;
    let empty = json(get(&parts.app, "/admin/requests", Some(&cookie)).await).await;
    assert_eq!(
        empty["landed_through"].as_i64(),
        Some(0),
        "an empty ledger answers 0"
    );
    let land = |id: &str, at: i64, model: &str| {
        parts
            .db
            .insert_request(
                &request(
                    id,
                    at,
                    Some("desktop"),
                    model,
                    "succeeded",
                    200,
                    "inference",
                ),
                &[attempt(id, "business", 200)],
                None,
            )
            .unwrap();
    };
    land("r0", now, "claude-sonnet-5");
    land("r1", now - 60_000, "claude-opus-5");
    let first = json(get(&parts.app, "/admin/requests?limit=500", Some(&cookie)).await).await;
    let through = first["landed_through"].as_i64().unwrap();
    assert!(through > 0, "the answer carries the newest stamp: {first}");
    land("r2", now - 30 * 60_000, "claude-sonnet-5");
    land("r3", now + 60_000, "claude-opus-5");
    let ids = |page: &serde_json::Value| -> Vec<String> {
        page["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let since = json(
        get(
            &parts.app,
            &format!("/admin/requests?landed_after={through}&limit=500"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ids(&since),
        ["r3", "r2"],
        "the rows written after the stamp alone, in the list's order"
    );
    let newest = since["landed_through"].as_i64().unwrap();
    assert!(
        newest > through,
        "the newest stamp moved past the last answer's"
    );
    let narrowed = json(
        get(
            &parts.app,
            &format!("/admin/requests?landed_after={through}&models=claude-opus-5&limit=500"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (ids(&narrowed), narrowed["landed_through"].as_i64()),
        (vec!["r3".to_string()], Some(newest)),
        "the stamp combines with the other filters, and the newest stamp is the ledger's whatever the filters keep"
    );
    let caught_up = json(
        get(
            &parts.app,
            &format!("/admin/requests?landed_after={newest}&limit=500"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (ids(&caught_up).len(), caught_up["landed_through"].as_i64()),
        (0, Some(newest)),
        "nothing was written since the newest stamp"
    );
}

#[tokio::test]
async fn the_request_log_refuses_a_landed_after_that_is_not_a_number() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let refused = get(&parts.app, "/admin/requests?landed_after=x", Some(&cookie)).await;
    assert_eq!(refused.status(), 400);
    assert_eq!(json(refused).await["error"], "landed_after is not a number");
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
            bodies["response_truncated"].as_bool(),
        ),
        (
            Some(request_len),
            Some(request_len),
            Some(1_000),
            Some(1_010),
            Some(true)
        ),
        "bytes measured on the raw vectors, not the rendered JSON or a character count"
    );
}

/// The route reads a capture at the admin state's clock, so a body whose expiry has passed answers 404 while its row
/// waits for the purge's next pass.
#[tokio::test]
async fn the_bodies_route_serves_a_capture_before_its_expiry_by_the_clock_and_answers_404_past_it()
{
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    let cookie = login(&parts.app, "admin-secret").await;
    for (id, expires_at) in [("r-expired", now - HOUR), ("r-kept", now + HOUR)] {
        let mut row = request(
            id,
            now - 2 * HOUR,
            Some("desktop"),
            "claude-sonnet-5",
            "succeeded",
            200,
            "inference",
        );
        row.bodies_captured = true;
        parts
            .db
            .insert_request(
                &row,
                &[attempt(id, "business", 200)],
                Some(&BodyRecord {
                    request_id: id.into(),
                    request_body: b"{}".to_vec(),
                    response_body: None,
                    request_len_seen: 2,
                    response_len_seen: 0,
                    expires_at,
                }),
            )
            .unwrap();
    }
    let expired = get(
        &parts.app,
        "/admin/requests/r-expired/bodies",
        Some(&cookie),
    )
    .await;
    assert_eq!(expired.status(), 404, "the expiry lies before the clock");
    assert_eq!(
        json(expired).await["error"],
        "no captured bodies for this request"
    );
    assert_eq!(
        get(&parts.app, "/admin/requests/r-kept/bodies", Some(&cookie))
            .await
            .status(),
        200,
        "the expiry lies after the clock"
    );
}

/// A capturing request straight onto the bus: `delivered` of its response chunks reach the ledger while its
/// completion says `published` went out, so the rest were dropped at the bus.
/// The observation of an inference request under `desktop`, for a test that drives the writer through the bus.
fn observed(id: &str) -> phoebis_bus::Event {
    use phoebis_bus::{Event, Kind, Origin, RequestObserved, RequestShape};
    Event::RequestObserved(RequestObserved {
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
        guardrail_id: None,
        guardrails: Vec::new(),
        received_at: common::NOW,
    })
}

fn publish_capture(bus: &phoebis_bus::Bus, id: &str, delivered: u32, published: u32) {
    use phoebis_bus::{Event, Outcome, RequestComplete, ResponseChunk, ResponseFacts};
    bus.publish(observed(id));
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
        client_contract: None,
        client_version: None,
        response_bytes: None,
        response_chunks: Some(published),
        held: None,
        diagnostics: None,
        message_id: None,
        skipped_events: 0,
        interruption: None,
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
        assert_eq!(bodies["response_truncated"].as_bool(), Some(true), "{id}");
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
    // The key's generation is the stamp the store gave it, read back rather than assumed.
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
async fn the_dashboard_reads_the_day_and_the_pins_and_the_account_list_the_newest_sample() {
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
                    window: "five_hour".into(),
                    utilization: Some(0.5),
                    reset_at: Some(now + HOUR),
                    source: phoebis_ledger::SampleSource::Head,
                    observed_at: phoebis_ledger::bucket_of(now - 2 * HOUR) * 1_000,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - HOUR),
                    window: "five_hour".into(),
                    utilization: Some(0.6),
                    reset_at: Some(now + HOUR),
                    source: phoebis_ledger::SampleSource::Poll,
                    observed_at: phoebis_ledger::bucket_of(now - HOUR) * 1_000,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: phoebis_ledger::bucket_of(now - HOUR),
                    window: "seven_day".into(),
                    utilization: Some(0.2),
                    reset_at: None,
                    source: phoebis_ledger::SampleSource::Poll,
                    observed_at: phoebis_ledger::bucket_of(now - HOUR) * 1_000,
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
    // Each row's 100 input, 40 cache-read and 10 output tokens of Sonnet 5 at the shipped $2, $0.20 and $10 a million
    // are $0.000308, and 24 rows $0.007392.
    assert!(
        (dashboard["cost_24h"].as_f64().unwrap() - 0.007392).abs() < 1e-9,
        "the 24 rows' counts at the shipped cells: {} vs 0.007392",
        dashboard["cost_24h"]
    );
    assert!((dashboard["error_rate_24h"].as_f64().unwrap() - 5.0 / 24.0).abs() < 1e-9);
    assert_eq!(dashboard["top_error_code"].as_str(), Some("upstream_error"));
    assert_eq!(dashboard["hourly"].as_array().unwrap().len(), 24);
    assert_eq!(dashboard["sessions"]["active_pins"].as_u64(), Some(0));
    let list = json(get(&parts.app, "/admin/accounts", Some(&cookie)).await).await;
    let business = list["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "business")
        .unwrap();
    let five = business["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == "five_hour")
        .unwrap();
    assert_eq!(
        five["utilization"].as_f64(),
        Some(0.6),
        "the newest sample per window"
    );
}

/// Two served requests and two guardrail blocks of the key `desktop` an hour before `now`, each block flagged with no
/// attempt as the landing flags a refusal.
fn plant_refusals_beside_served(parts: &common::Full) {
    let now = common::NOW;
    for id in ["r-served-1", "r-served-2"] {
        parts
            .db
            .insert_request(
                &request(
                    id,
                    now - HOUR,
                    Some("desktop"),
                    "claude-sonnet-5",
                    "succeeded",
                    200,
                    "inference",
                ),
                &[attempt(id, "business", 200)],
                None,
            )
            .unwrap();
    }
    for id in ["r-refused-1", "r-refused-2"] {
        let refused = RequestRecord {
            attempts: 0,
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: Some("guardrail_blocked".into()),
            refused: true,
            ..request(
                id,
                now - HOUR,
                Some("desktop"),
                "claude-sonnet-5",
                "rejected",
                403,
                "inference",
            )
        };
        parts.db.insert_request(&refused, &[], None).unwrap();
    }
}

#[tokio::test]
async fn the_breakdown_route_carries_refused_and_refusals_per_row() {
    let parts = common::admin_parts_full().await;
    plant_refusals_beside_served(&parts);
    let cookie = login(&parts.app, "admin-secret").await;
    let breakdown = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=key",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let row = breakdown["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key_id"] == "desktop")
        .unwrap_or_else(|| panic!("no desktop row: {breakdown}"));
    assert_eq!(
        (
            row["requests"].as_u64(),
            row["refused"].as_u64(),
            &row["refusals"]
        ),
        (
            Some(2),
            Some(2),
            &serde_json::json!({ "guardrail_blocked": 2 })
        ),
        "the two blocks beside the two served requests: {row}"
    );
}

#[tokio::test]
async fn the_dashboard_route_carries_the_refused_count_and_the_top_refusal_code() {
    let parts = common::admin_parts_full().await;
    plant_refusals_beside_served(&parts);
    let cookie = login(&parts.app, "admin-secret").await;
    let dashboard = json(get(&parts.app, "/admin/dashboard", Some(&cookie)).await).await;
    assert_eq!(
        (
            dashboard["requests_24h"].as_u64(),
            dashboard["refused_24h"].as_u64(),
            dashboard["top_refusal_code"].as_str()
        ),
        (Some(2), Some(2), Some("guardrail_blocked")),
        "the two served requests, the two blocks apart"
    );
    let at = (common::NOW - HOUR).div_euclid(HOUR) * HOUR;
    let hour = dashboard["hourly"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["at"] == at)
        .unwrap_or_else(|| panic!("no hour at {at}: {}", dashboard["hourly"]));
    assert_eq!(
        (hour["requests"].as_u64(), hour["refused"].as_u64()),
        (Some(2), Some(2)),
        "the hour's refused beside its requests: {hour}"
    );
}

fn guardrail_match(id: &str, position: u32, blocked: bool) -> phoebis_ledger::GuardrailMatchRecord {
    phoebis_ledger::GuardrailMatchRecord {
        guardrail_id: id.into(),
        position,
        blocked,
    }
}

/// A request row with the given matches, the blocking one named on the row and the request refused where any blocked.
fn guarded(
    id: &str,
    at: i64,
    request_bytes: u64,
    matches: Vec<phoebis_ledger::GuardrailMatchRecord>,
) -> RequestRecord {
    let blocking = matches
        .iter()
        .find(|m| m.blocked)
        .map(|m| m.guardrail_id.clone());
    let refused = blocking.is_some();
    RequestRecord {
        request_bytes: Some(request_bytes),
        attempts: if refused { 0 } else { 1 },
        error_code: refused.then(|| "guardrail_blocked".into()),
        guardrail_id: blocking,
        guardrails: matches,
        ..request(
            id,
            at,
            Some("desktop"),
            "claude-sonnet-5",
            if refused { "rejected" } else { "succeeded" },
            if refused { 400 } else { 200 },
            "inference",
        )
    }
}

fn listed_ids(page: &serde_json::Value) -> Vec<&str> {
    page["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect()
}

/// `guardrails` keeps the requests a listed guardrail matched and `-` the ones none matched, and both the list's row
/// and the show route carry the blocking guardrail, every match in evaluation order and the row's refused flag.
#[tokio::test]
async fn the_request_log_filters_by_guardrail_and_by_none_and_the_summary_carries_the_matches() {
    let parts = common::admin_parts_full().await;
    let now = common::NOW;
    for row in [
        guarded(
            "r-blocked",
            now - HOUR,
            2_000,
            vec![
                guardrail_match("g1", 0, false),
                guardrail_match("g2", 1, true),
            ],
        ),
        guarded(
            "r-watched",
            now - 2 * HOUR,
            1_000,
            vec![guardrail_match("g1", 0, false)],
        ),
        guarded("r-none", now - 3 * HOUR, 1_000, Vec::new()),
        RequestRecord {
            refused: true,
            ..guarded(
                "r-refused",
                now - 4 * HOUR,
                1_000,
                vec![guardrail_match("g3", 0, true)],
            )
        },
    ] {
        parts.db.insert_request(&row, &[], None).unwrap();
    }
    let cookie = login(&parts.app, "admin-secret").await;
    let page = |query: &str| {
        let path = format!("/admin/requests?{query}");
        let (app, cookie) = (parts.app.clone(), cookie.clone());
        async move { json(get(&app, &path, Some(&cookie)).await).await }
    };
    let by_g1 = page("guardrails=g1").await;
    assert_eq!(listed_ids(&by_g1), ["r-blocked", "r-watched"]);
    let by_none = page("guardrails=-").await;
    assert_eq!(listed_ids(&by_none), ["r-none"]);
    assert_eq!(
        listed_ids(&page("guardrails=g2,-").await),
        ["r-blocked", "r-none"]
    );
    let expected = serde_json::json!([
        { "id": "g1", "position": 0, "blocked": false },
        { "id": "g2", "position": 1, "blocked": true },
    ]);
    let row = &by_g1["requests"][0];
    assert_eq!(row["guardrail_id"], "g2");
    assert_eq!(row["guardrails"], expected);
    assert_eq!(
        (
            row["outcome"].as_str(),
            row["status"].as_u64(),
            row["error_code"].as_str()
        ),
        (Some("rejected"), Some(400), Some("guardrail_blocked"))
    );
    assert_eq!(
        (
            &by_none["requests"][0]["guardrail_id"],
            &by_none["requests"][0]["guardrails"]
        ),
        (&serde_json::Value::Null, &serde_json::json!([]))
    );
    let shown = json(get(&parts.app, "/admin/requests/r-blocked", Some(&cookie)).await).await;
    assert_eq!(shown["request"]["guardrail_id"], "g2");
    assert_eq!(shown["request"]["guardrails"], expected);
    let listed = page("limit=500").await;
    let refused_of = |id: &str| {
        listed["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .map(|r| r["refused"].clone())
    };
    assert_eq!(
        ["r-refused", "r-blocked", "r-watched", "r-none"].map(refused_of),
        [
            Some(serde_json::json!(true)),
            Some(serde_json::json!(false)),
            Some(serde_json::json!(false)),
            Some(serde_json::json!(false))
        ],
        "the flagged refusal alone reads refused, the unflagged block and the served rows do not"
    );
    let shown = json(get(&parts.app, "/admin/requests/r-refused", Some(&cookie)).await).await;
    assert_eq!(shown["request"]["refused"], true);
}

/// The counts route answers each guardrail matched in the range with the registry's current name, null for an id the
/// registry no longer holds, and refuses a bad range with `parse_range`'s sentence.
#[tokio::test]
async fn the_guardrail_counts_route_answers_the_registry_name_and_null_for_a_deleted_guardrail() {
    let parts = common::admin_parts_full().await;
    parts
        .store
        .insert_guardrail(
            &phoebis_store::GuardrailRow {
                id: "g1".into(),
                name: "Large forks".into(),
                description: String::new(),
                enabled: true,
                mode: phoebis_guardrails::GuardrailMode::Enforce,
                action: phoebis_guardrails::GuardrailAction::Block,
                conditions: vec![phoebis_guardrails::Condition {
                    subject: phoebis_guardrails::Subject::RequestClass,
                    operator: phoebis_guardrails::Operator::Equals,
                    value: Some("auxiliary".into()),
                    name: None,
                    path: None,
                    selector: None,
                }],
                preset: None,
                incarnation: 1,
                created_at: 0,
                updated_at: 0,
            },
            &[],
            &[],
            "test",
        )
        .unwrap();
    parts.router.reload().await.unwrap();
    let now = common::NOW;
    for row in [
        guarded(
            "r1",
            now - HOUR,
            3_000,
            vec![guardrail_match("g1", 0, true)],
        ),
        guarded(
            "r2",
            now - 2 * HOUR,
            1_000,
            vec![
                guardrail_match("g1", 0, false),
                guardrail_match("ghost", 1, false),
            ],
        ),
        guarded(
            "r3",
            now - 3 * HOUR,
            500,
            vec![guardrail_match("ghost", 0, true)],
        ),
        guarded(
            "r4",
            now - 48 * HOUR,
            9_000,
            vec![guardrail_match("g1", 0, true)],
        ),
    ] {
        parts.db.insert_request(&row, &[], None).unwrap();
    }
    let cookie = login(&parts.app, "admin-secret").await;
    let counts = json(
        get(
            &parts.app,
            "/admin/analytics/guardrails?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        counts,
        serde_json::json!({
            "from": now - 24 * HOUR,
            "to": now,
            "guardrails": [
                { "id": "g1", "name": "Large forks", "observed": 1, "blocked": 1, "blocked_bytes": 3_000 },
                { "id": "ghost", "name": null, "observed": 1, "blocked": 1, "blocked_bytes": 500 },
            ],
        })
    );
    let refused = get(
        &parts.app,
        "/admin/analytics/guardrails?range=bad",
        Some(&cookie),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert_eq!(
        json(refused).await["error"],
        "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"
    );
}
