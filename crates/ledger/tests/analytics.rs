//! Rollups for a synthetic day equal a direct aggregation of the raw rows for every group-by, the unattributed
//! bucket included, and merged-histogram percentiles lie within one bucket of the exact ones.
use std::collections::{BTreeMap, HashMap};

use phoebis_ledger::{
    apply_filter, breakdown, breakdown_paged, bucket_index, bucket_seconds, burn_rows, burn_series,
    burn_series_over, delta_of, fold_account, fold_event, fold_keepalive, hour_ceil, hour_of,
    key_request_totals, list_requests, member_spends, moves_by_reason, placements, price_shares,
    read_account_rollups, read_rollups, rewrite_series, run_retention_blocking, series,
    series_paged, session_identities, AttemptRecord, BudgetedMember, CostRevisions, EventRecord,
    FundingFilter, GroupRow, Histogram, KeepaliveDelta, LedgerConfig, LedgerDb, LiveKeys,
    ModelPrice, ModelRow, PlanTables, PriceRevisions, PriceTable, RequestFilter, RequestRecord,
    RollupFilter, RollupRow, RollupSelect, SeriesFilter, SharePricing, Speed, Tiers, WindowShares,
    BILLED_FUNDING, FAST_FUNDING, NO_CLASS, OVERAGE_FUNDING, PAGE_HOURS, PLAN_FUNDING,
    UNATTRIBUTED,
};
use phoebis_routing::{
    PinUsage, RegistrySnapshot, RoutingState, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{AccountKind, BudgetPeriod, PoolMember, RegistryLoad, RoutingLoad};
use rand::{RngExt, SeedableRng};

const DAY_MS: i64 = 86_400_000;
/// Monday 2026-09-21 at 14:00 UTC, inside the peak band.
const WEEKDAY_PEAK: i64 = 1_789_999_200_000;
/// Monday 2026-09-21 at 20:00 UTC, past the peak band on the same weekday.
const WEEKDAY_OFF_PEAK: i64 = 1_790_020_800_000;
/// Monday 2026-09-21 at 12:00 UTC, the peak band's first minute (05:00 in Los Angeles).
const WEEKDAY_BAND_OPENS: i64 = 1_789_992_000_000;
const MAX_20X: &str = "default_claude_max_20x";
const PRO: &str = "default_claude_ai";

/// The share of `window` on `tier` that `usage` of `model` reads for a request completing at `at`, from the tables
/// the test loaded.
fn share_of(
    tables: &PlanTables,
    window: &str,
    tier: &str,
    model: &str,
    usage: PinUsage,
    at: i64,
) -> f64 {
    tables
        .share_at(window, tier, model, usage, at)
        .expect("the tables price the model on the tier")
}

/// `input` uncached and `output` tokens.
fn usage(input: u64, output: u64) -> PinUsage {
    PinUsage {
        input,
        output,
        ..PinUsage::default()
    }
}

fn synthetic_day(seed: u64) -> Vec<(RequestRecord, Vec<AttemptRecord>)> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let keys = ["k1", "k2", "k3"];
    let pools = ["p1", "p2"];
    let accounts = ["a", "b", "c"];
    let models = ["claude-sonnet-5", "claude-opus-5", "gpt-5"];
    // The last one stands for a caller that sent no agent at all, which folds under the reserved id.
    let agents = [
        Some("claude-cli/2.1.269 (external, sdk-cli)"),
        Some("claude-cli/2.1.300 (external, cli)"),
        None,
    ];
    // The last one stands for a request the relay never classified, which folds under the reserved id.
    let clients = [Some("claude-code/cli"), Some("sdk/python"), None];
    let efforts = [None, None, Some("low"), Some("high"), Some("max")];
    let kinds = [
        "inference",
        "inference",
        "inference",
        "count_tokens",
        "probe",
    ];
    let mut out = Vec::new();
    for i in 0..4_000u32 {
        let received_at = rng.random_range(0..DAY_MS);
        let unattributed = rng.random_range(0..20) == 0;
        let attempts_n = if unattributed {
            0
        } else {
            rng.random_range(0..=3)
        };
        let outcome = match (unattributed, attempts_n) {
            (true, _) => "rejected",
            (false, 0) => "exhausted",
            _ => ["succeeded", "succeeded", "succeeded", "failed", "canceled"]
                [rng.random_range(0..5)],
        };
        let status = match outcome {
            "rejected" => 401,
            "exhausted" => 429,
            "succeeded" => 200,
            "failed" => [429, 500, 529][rng.random_range(0..3)],
            _ => 503,
        };
        let ttfb = (outcome != "rejected").then(|| rng.random_range(20..3_000));
        let ttfc = ttfb
            .filter(|_| rng.random_bool(0.7))
            .map(|t| t + rng.random_range(0..2_000));
        let duration = ttfb.map(|t| t + rng.random_range(500..20_000));
        let id = format!("r{i}");
        let mut attempts = Vec::new();
        for n in 1..=attempts_n {
            let last = n == attempts_n;
            attempts.push(AttemptRecord {
                request_id: id.clone(),
                attempt: n,
                account_id: accounts[rng.random_range(0..3)].into(),
                account_kind: "oauth".into(),
                started_at: received_at + n as i64,
                outcome: if last { "succeeded" } else { "rate_limited" }.into(),
                input_tokens: last.then(|| rng.random_range(100..5_000)),
                output_tokens: last.then(|| rng.random_range(1..3_000)),
                cache_read_tokens: last
                    .then(|| [0, 0, rng.random_range(1..4_000)][rng.random_range(0..3)]),
                cache_write_5m_tokens: last
                    .then(|| [0, rng.random_range(1..2_000)][rng.random_range(0..2)]),
                cache_write_1h_tokens: last.then_some(0),
                thinking_tokens: last.then(|| rng.random_range(0..500)),
                web_search_requests: last.then(|| rng.random_range(0..3)),
                web_fetch_requests: last.then(|| rng.random_range(0..3)),
                cache_miss_reason: last.then(|| {
                    ["none", "tools_changed", "messages_changed"][rng.random_range(0..3)].into()
                }),
                estimated_usd: last.then(|| rng.random_range(0.0..0.5)),
                ratelimit_5h_utilization: Some(rng.random_range(0.0..1.0)),
                speed: (last && rng.random_bool(0.5)).then(|| "fast".into()),
                ..Default::default()
            });
        }
        let request = RequestRecord {
            id,
            received_at,
            finished_at: Some(received_at + duration.unwrap_or(5)),
            key_id: (!unattributed).then(|| keys[rng.random_range(0..3)].into()),
            key_generation: i64::from(i % 4 == 0),
            pool_id: (!unattributed).then(|| pools[rng.random_range(0..2)].into()),
            model: models[i as usize % 3].into(),
            canonical_model: models[i as usize % 3].into(),
            kind: kinds[rng.random_range(0..5)].into(),
            origin: "client".into(),
            status: Some(status),
            outcome: outcome.into(),
            attempts: attempts_n,
            ttfb_ms: ttfb,
            ttfc_ms: ttfc,
            duration_ms: duration,
            error_code: (outcome != "succeeded").then(|| "e".into()),
            user_agent: agents[rng.random_range(0..3)].map(String::from),
            client: clients[rng.random_range(0..3)].map(String::from),
            effort: efforts[rng.random_range(0..5)].map(String::from),
            tool_count: rng.random_range(0..30),
            ..Default::default()
        };
        out.push((request, attempts));
    }
    out
}

/// A direct aggregation of the raw rows under one group-by, built from the same delta rule but summed in memory.
fn aggregate(
    rows: &[(RequestRecord, Vec<AttemptRecord>)],
    group: impl Fn(&RollupRow) -> String,
) -> BTreeMap<String, RollupRow> {
    let mut out: BTreeMap<String, RollupRow> = BTreeMap::new();
    for (request, attempts) in rows {
        let delta = delta_of(request, attempts);
        let key = group(&delta);
        out.entry(key.clone())
            .and_modify(|row| row.add(&delta))
            .or_insert_with(|| RollupRow {
                hour: 0,
                key_id: key.clone(),
                ..delta.clone()
            });
    }
    out
}

fn sum_by(
    rollups: &[RollupRow],
    group: impl Fn(&RollupRow) -> String,
) -> BTreeMap<String, RollupRow> {
    let mut out: BTreeMap<String, RollupRow> = BTreeMap::new();
    for row in rollups {
        let key = group(row);
        out.entry(key.clone())
            .and_modify(|r| r.add(row))
            .or_insert_with(|| RollupRow {
                hour: 0,
                key_id: key.clone(),
                ..row.clone()
            });
    }
    out
}

#[test]
fn rollups_for_a_synthetic_day_equal_a_direct_aggregation_for_every_group_by() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut rows = synthetic_day(11);
    // Every twentieth request is written twice, the first time with other tokens and another serving account:
    // a retried record must leave exactly one contribution, under the rows the ledger holds now.
    for (request, attempts) in rows.iter_mut().step_by(20) {
        let mut earlier = attempts.clone();
        if let Some(last) = earlier.last_mut() {
            last.output_tokens = last.output_tokens.map(|n| n + 999);
            last.account_id = "z".into();
            last.speed = match last.speed {
                Some(_) => None,
                None => Some("fast".into()),
            };
        }
        db.insert_request(request, &earlier, None).unwrap();
    }
    for (request, attempts) in &rows {
        db.insert_request(request, attempts, None).unwrap();
    }
    let rollups = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert!(
        rollups.iter().all(|r| r.account_id != "z"),
        "a replace that moves a request to another account leaves no row at all for the vacated key"
    );
    assert!(
        rollups
            .iter()
            .any(|r| r.key_id == UNATTRIBUTED && r.account_id == UNATTRIBUTED),
        "the unattributed bucket exists"
    );
    assert!(
        rollups.iter().any(|r| r.user_agent == UNATTRIBUTED)
            && rollups
                .iter()
                .any(|r| r.user_agent == "claude-cli/2.1.300"),
        "a caller that sent no agent folds under the reserved id, beside the product tokens of the ones that did"
    );
    let raw_total: u64 = rollups.iter().map(|r| r.requests).sum();
    assert_eq!(raw_total, rows.len() as u64);
    #[allow(clippy::type_complexity)]
    let groups: Vec<(&str, Box<dyn Fn(&RollupRow) -> String>)> = vec![
        ("key", Box::new(|r| r.key_id.clone())),
        (
            "key_generation",
            Box::new(|r| format!("{}@{}", r.key_id, r.key_generation)),
        ),
        ("pool", Box::new(|r| r.pool_id.clone())),
        ("account", Box::new(|r| r.account_id.clone())),
        ("model", Box::new(|r| r.canonical_model.clone())),
        ("kind", Box::new(|r| r.kind.clone())),
        ("user_agent", Box::new(|r| r.user_agent.clone())),
        ("client", Box::new(|r| r.client.clone())),
        ("effort", Box::new(|r| r.effort.clone())),
        ("speed", Box::new(|r| r.speed.clone())),
        ("hour", Box::new(|r| r.hour.to_string())),
    ];
    for (name, group) in &groups {
        let expected = aggregate(&rows, group);
        let actual = sum_by(&rollups, group);
        assert_eq!(
            actual.keys().collect::<Vec<_>>(),
            expected.keys().collect::<Vec<_>>(),
            "group-by {name}: the groups"
        );
        for (key, want) in &expected {
            let got = &actual[key];
            assert_eq!(
                (
                    got.requests,
                    got.succeeded,
                    got.rate_limited,
                    got.failed,
                    got.exhausted,
                    got.rejected
                ),
                (
                    want.requests,
                    want.succeeded,
                    want.rate_limited,
                    want.failed,
                    want.exhausted,
                    want.rejected
                ),
                "group-by {name} = {key}: outcomes"
            );
            assert_eq!(
                (
                    got.attempts,
                    got.failovers,
                    got.cache_hits,
                    got.cache_misses,
                    got.uncached_requests
                ),
                (
                    want.attempts,
                    want.failovers,
                    want.cache_hits,
                    want.cache_misses,
                    want.uncached_requests
                ),
                "group-by {name} = {key}: attempts and cache"
            );
            assert_eq!(
                (
                    got.input_tokens,
                    got.cache_read_tokens,
                    got.cache_write_5m_tokens,
                    got.cache_write_1h_tokens,
                    got.output_tokens
                ),
                (
                    want.input_tokens,
                    want.cache_read_tokens,
                    want.cache_write_5m_tokens,
                    want.cache_write_1h_tokens,
                    want.output_tokens
                ),
                "group-by {name} = {key}: tokens"
            );
            assert_eq!(
                (
                    got.thinking_tokens,
                    got.web_search_requests,
                    got.web_fetch_requests
                ),
                (
                    want.thinking_tokens,
                    want.web_search_requests,
                    want.web_fetch_requests
                ),
                "group-by {name} = {key}: the thinking share and the server-tool calls"
            );
            assert!(
                (got.estimated_usd - want.estimated_usd).abs() < 1e-6,
                "group-by {name} = {key}: usd"
            );
            assert!(
                (got.input_usd - want.input_usd).abs() < 1e-6
                    && (got.cache_read_usd - want.cache_read_usd).abs() < 1e-6
                    && (got.cache_write_usd - want.cache_write_usd).abs() < 1e-6
                    && (got.output_usd - want.output_usd).abs() < 1e-6,
                "group-by {name} = {key}: split usd"
            );
            assert_eq!(
                (
                    &got.ttfb_hist,
                    &got.ttfc_hist,
                    &got.duration_hist,
                    &got.output_tps_hist
                ),
                (
                    &want.ttfb_hist,
                    &want.ttfc_hist,
                    &want.duration_hist,
                    &want.output_tps_hist
                ),
                "group-by {name} = {key}: histograms"
            );
            assert_eq!(
                (&got.errors, &got.cache_miss_reasons),
                (&want.errors, &want.cache_miss_reasons),
                "group-by {name} = {key}: json counts"
            );
        }
    }
}

#[test]
fn a_reused_id_is_two_key_groups_and_a_bare_selector_names_the_live_generation() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str,
                 key: Option<&str>,
                 generation: i64,
                 agent: Option<&str>,
                 client: Option<&str>| {
        let (mut request, mut attempts) = synthetic_day(1).swap_remove(0);
        request.id = id.into();
        request.key_id = key.map(String::from);
        request.key_generation = generation;
        request.received_at = 1_000;
        request.kind = "inference".into();
        request.user_agent = agent.map(String::from);
        request.client = client.map(String::from);
        for attempt in &mut attempts {
            attempt.request_id = id.into();
        }
        db.insert_request(&request, &attempts, None).unwrap();
    };
    plant("r1", Some("k1"), 1_000, None, Some("claude-code/cli"));
    plant("r2", Some("k1"), 1_000, None, Some("claude-code/cli"));
    plant("r3", Some("k1"), 2_000, None, Some("sdk/python"));
    plant("r4", Some("k2"), 0, None, Some("sdk/python"));
    // A client whose agent carries `@`, and a request the router never attributed.
    plant(
        "r5",
        Some("k2"),
        0,
        Some("my-client/1.0 (ops@example.invalid)"),
        Some("sdk/python"),
    );
    plant("r6", None, 0, None, None);
    // An earlier holder's rows filed before the generation column stay at 0, which is no creation time; and a
    // generation at 09:31:45 pins the hour and the minute.
    plant("r7", Some("k1"), 0, None, Some("claude-code/cli"));
    plant("r8", Some("k1"), 34_305_000, None, Some("claude-code/cli"));
    let live = LiveKeys(BTreeMap::from([
        ("k1".to_string(), 2_000),
        ("k2".to_string(), 0),
    ]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let groups = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        "key",
        true,
        &live,
        &[],
        None,
    );
    assert_eq!(
        groups
            .iter()
            .map(|g| (
                g.key.as_str(),
                g.label.as_str(),
                g.key_id.as_deref(),
                g.key_created_at,
                g.requests
            ))
            .collect::<Vec<_>>(),
        vec![
            ("k1", "k1", Some("k1"), None, 1),
            ("k1@0", "k1 (deleted)", Some("k1"), Some(0), 1),
            (
                "k1@1000",
                "k1 (deleted, created 1970-01-01 00:00 UTC)",
                Some("k1"),
                Some(1_000),
                2
            ),
            (
                "k1@34305000",
                "k1 (deleted, created 1970-01-01 09:31 UTC)",
                Some("k1"),
                Some(34_305_000),
                1
            ),
            ("k2", "k2", Some("k2"), None, 2),
            ("-", "unattributed", Some("-"), None, 1)
        ],
        "groups sort by label; the reserved id keeps its label and no suffix, and a key group names its holder beside the label, with the creation instant for an earlier holder alone"
    );
    let agents = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        "user_agent",
        true,
        &live,
        &[],
        None,
    );
    let ops = agents
        .iter()
        .find(|g| g.key == "my-client/1.0")
        .expect("the agent's product token is its own group");
    assert_eq!(
        ops.label, "my-client/1.0",
        "an agent is labeled by its token verbatim: only a key group is parsed for a generation, and the \
         comment carrying `@` stays on the request row"
    );
    let clients = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        "client",
        true,
        &live,
        &[],
        None,
    );
    assert_eq!(
        clients
            .iter()
            .map(|g| (g.key.as_str(), g.label.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("claude-code/cli", "claude-code/cli"),
            ("sdk/python", "sdk/python"),
            ("-", "unattributed"),
        ],
        "the class groups the rows; a row folded before the column existed labels as every reserved id does"
    );
    // The selectors resolve against the live keys and bind in the read itself, as the analytics routes read them.
    let sum = |keys: &[&str]| -> u64 {
        let filter = RollupFilter {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        };
        let Some(select) = filter.rollup_select(&live) else {
            return 0;
        };
        db.read_blocking(|c| read_rollups(c, 0, i64::MAX, &select))
            .unwrap()
            .iter()
            .map(|r| r.requests)
            .sum()
    };
    assert_eq!(sum(&["k1"]), 1, "a bare id is the live generation");
    assert_eq!(sum(&["k1@1000"]), 2, "the earlier holder is named in full");
    assert_eq!(
        sum(&["k3"]),
        0,
        "an id no live key holds selects nothing bare"
    );
    assert_eq!(
        sum(&["desktop@"]),
        0,
        "a selector with nothing after the `@` names no generation and selects nothing"
    );
    assert_eq!(
        sum(&["k1@abc"]),
        0,
        "a generation that is not a number selects nothing, the live rows included"
    );
    assert_eq!(
        sum(&["-"]),
        1,
        "the reserved id selects the unattributed rows whatever their generation"
    );
    assert_eq!(
        sum(&["-@0"]),
        0,
        "the reserved id has no generation of its own; `-` alone names its rows"
    );
    let totals = db
        .read_blocking(|c| key_request_totals(c, i64::MIN, &live))
        .unwrap();
    assert_eq!(
        totals.keys().collect::<Vec<_>>(),
        vec!["k1", "k2"],
        "the keys list never carries an earlier holder's row"
    );
    assert_eq!(totals["k1"].requests, 1);
}

/// The fixed filters as the analytics applied them in memory before the read took them into SQL: a key selector
/// through `LiveKeys::selects`, and the models compared lowercased.
fn selected_in_memory(row: &RollupRow, f: &RollupFilter, live: &LiveKeys) -> bool {
    let models: Vec<String> = f.models.iter().map(|m| m.to_ascii_lowercase()).collect();
    (f.keys.is_empty() || f.keys.iter().any(|k| live.selects(k, row)))
        && (f.pools.is_empty() || f.pools.contains(&row.pool_id))
        && (f.accounts.is_empty() || f.accounts.contains(&row.account_id))
        && (models.is_empty() || models.contains(&row.canonical_model))
        && (f.kinds.is_empty() || f.kinds.contains(&row.kind))
}

/// A long range is read a day at a time and folded as each page arrives; the answer is the one the whole range
/// read at once and folded in memory gives, under the default filter and under every fixed filter at once. The
/// pages start at the range's first hour and the daily buckets at the epoch's, so a bucket's rows arrive in two
/// pages: two requests of one day bucket sit either side of the first page's edge.
#[tokio::test]
async fn a_four_hundred_day_range_read_in_pages_answers_the_unpaged_series_and_breakdown() {
    const HOUR_MS: i64 = 3_600_000;
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let from = 6 * HOUR_MS;
    let to = from + 400 * DAY_MS;
    let mut planted = synthetic_day(17);
    for (i, (request, attempts)) in planted.iter_mut().enumerate() {
        let shift = from + (i as i64 % 400) * DAY_MS;
        request.received_at += shift;
        for attempt in attempts.iter_mut() {
            attempt.started_at += shift;
        }
    }
    for (n, at) in [("edge-before", 28 * HOUR_MS), ("edge-after", 30 * HOUR_MS)] {
        let (mut request, mut attempts) = planted[0].clone();
        request.id = n.into();
        request.received_at = at;
        for attempt in &mut attempts {
            attempt.request_id = n.into();
            attempt.started_at = at;
        }
        planted.push((request, attempts));
    }
    for (request, attempts) in &planted {
        db.insert_request(request, attempts, None).unwrap();
    }
    assert_eq!(
        (from / 1_000 + PAGE_HOURS * 3_600) * 1_000,
        30 * HOUR_MS,
        "the first page ends between the two requests of the second day's bucket"
    );
    let live = LiveKeys(BTreeMap::from([
        ("k1".to_string(), 0),
        ("k2".to_string(), 1),
    ]));
    let narrowed = RollupFilter {
        keys: vec!["k1".into(), "k2".into(), UNATTRIBUTED.into(), "k3@1".into()],
        pools: vec!["p1".into(), UNATTRIBUTED.into()],
        accounts: vec!["a".into(), "b".into(), UNATTRIBUTED.into()],
        models: vec!["CLAUDE-SONNET-5".into(), "gpt-5".into()],
        kinds: vec!["inference".into(), "probe".into()],
        user_agents: vec!["claude-cli/*".into()],
        ..RollupFilter::default()
    };
    let whole = db
        .read_blocking(|c| read_rollups(c, hour_of(from), hour_ceil(to), &RollupSelect::default()))
        .unwrap();
    let bucket = bucket_seconds(from, to);
    assert_eq!(bucket, 86_400);
    for filter in [RollupFilter::default(), narrowed] {
        let rows: Vec<RollupRow> = apply_filter(
            whole
                .iter()
                .filter(|r| selected_in_memory(r, &filter, &live))
                .cloned()
                .collect(),
            &filter,
        );
        assert!(!rows.is_empty(), "the filter selects rows: {filter:?}");
        for group_by in ["all", "effort", "key", "user_agent"] {
            let unpaged = series(
                &rows,
                from,
                to,
                bucket,
                group_by,
                filter.probes_counted(),
                &live,
                &filter.user_agent_groups,
                None,
            )
            .unwrap();
            let paged = series_paged(&db, &filter, &live, from, to, bucket, group_by, None)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(&paged).unwrap(),
                serde_json::to_value(&unpaged).unwrap(),
                "series by {group_by} under {filter:?}"
            );
            let unpaged = breakdown(
                &rows,
                group_by,
                filter.probes_counted(),
                &live,
                &filter.user_agent_groups,
                None,
            );
            let paged = breakdown_paged(&db, &filter, &live, from, to, group_by, None)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(&paged).unwrap(),
                serde_json::to_value(&unpaged).unwrap(),
                "breakdown by {group_by} under {filter:?}"
            );
        }
    }
}

#[test]
fn a_group_of_probes_alone_stays_out_of_the_default_breakdown() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, key: &str, kind: &str| {
        let (mut request, mut attempts) = synthetic_day(3).swap_remove(0);
        request.id = id.into();
        request.key_id = Some(key.into());
        request.key_generation = 0;
        request.received_at = 1_000;
        request.kind = kind.into();
        for attempt in &mut attempts {
            attempt.request_id = id.into();
        }
        db.insert_request(&request, &attempts, None).unwrap();
    };
    plant("r1", "k1", "probe");
    plant("r2", "k2", "inference");
    let live = LiveKeys(BTreeMap::from([
        ("k1".to_string(), 0),
        ("k2".to_string(), 0),
    ]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default());
    let default_groups = breakdown(&filtered, "key", false, &live, &[], None);
    assert!(
        !default_groups.iter().any(|g| g.key == "k1"),
        "a group whose rows are all probes stays out of the default breakdown: {default_groups:?}"
    );
    let with_probes = breakdown(&filtered, "key", true, &live, &[], None);
    assert!(
        with_probes.iter().any(|g| g.key == "k1"),
        "the same group appears once probes count as requests: {with_probes:?}"
    );
}

#[test]
fn a_group_of_keepalive_credits_alone_stays_in_the_default_breakdown() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                at: 1_000,
                key_id: "desktop".into(),
                pool_id: "main".into(),
                account_id: "a".into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                kept_warm_request_id: Some("r1".into()),
                kept_warm_requests: 1,
                kept_warm_tokens: 2_000,
                saved_usd_gross: 0.0036,
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let live = LiveKeys::default();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default());
    // A kept-warm credit never touches `requests`, so a group made only of credit rows never sees a probe
    // row either; it belongs in the default breakdown, not just the probe-counted one.
    let default_groups = breakdown(&filtered, "user_agent", false, &live, &[], None);
    let group = default_groups
        .iter()
        .find(|g| g.key == UNATTRIBUTED)
        .expect("a group of keepalive credits alone stays in the default breakdown");
    assert_eq!(group.kept_warm.requests, 1);
    // The row's saving stays in the rollups, unread: the group a route answers carries no savings figure.
    let answered = serde_json::to_value(group).unwrap();
    assert!(
        answered.get("savings").is_none() && !answered.to_string().contains("gross"),
        "{answered}"
    );
}

#[test]
fn a_keepalive_credits_row_files_under_the_reserved_class() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                at: 1_000,
                key_id: "desktop".into(),
                pool_id: "main".into(),
                account_id: "a".into(),
                canonical_model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                kept_warm_requests: 1,
                kept_warm_tokens: 2_000,
                saved_usd_gross: 0.0036,
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_class, NO_CLASS);
}

/// One request the replace path rewrites, always under the same id.
fn replaced(id: &str) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: 3_700_000,
        finished_at: Some(3_700_900),
        key_id: Some("k".into()),
        pool_id: Some("p".into()),
        model: "m".into(),
        canonical_model: "m".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    }
}

/// Its one attempt, carrying the 5h utilization the account rollup takes a maximum of.
fn reading(id: &str, account: &str, utilization: f64) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: account.into(),
        account_kind: "oauth".into(),
        started_at: 3_700_000,
        outcome: "succeeded".into(),
        ratelimit_5h_utilization: Some(utilization),
        ..Default::default()
    }
}

fn peak(db: &LedgerDb, account: &str) -> Option<f64> {
    db.read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
        .unwrap()
        .into_iter()
        .find(|r| r.account_id == account)
        .and_then(|r| r.utilization_5h_max)
}

#[test]
fn a_replace_that_lowers_a_reading_lowers_the_accounts_hourly_peak() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(&replaced("r1"), &[reading("r1", "a", 0.9)], None)
        .unwrap();
    assert_eq!(peak(&db, "a"), Some(0.9));
    db.insert_request(&replaced("r1"), &[reading("r1", "a", 0.2)], None)
        .unwrap();
    assert_eq!(
        peak(&db, "a"),
        Some(0.2),
        "the peak is a maximum over the attempts the ledger holds, not over every one it ever held"
    );
}

#[test]
fn a_replace_that_moves_the_serving_attempt_empties_the_old_accounts_reading() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(&replaced("r1"), &[reading("r1", "a", 0.9)], None)
        .unwrap();
    db.insert_request(&replaced("r1"), &[reading("r1", "b", 0.4)], None)
        .unwrap();
    assert_eq!(
        peak(&db, "a"),
        None,
        "nothing is left in the hour to read the vacated account's utilization from"
    );
    assert_eq!(peak(&db, "b"), Some(0.4));
}

#[test]
fn merged_histogram_percentiles_lie_within_one_bucket_of_the_exact_percentiles() {
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = synthetic_day(23);
    for (request, attempts) in &rows {
        db.insert_request(request, attempts, None).unwrap();
    }
    let rollups = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let exact = |values: &mut Vec<f64>, p: f64| {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        values[(((p * values.len() as f64).ceil() as usize).max(1)) - 1]
    };
    let within_one_bucket = |histogram: &Histogram, values: &mut Vec<f64>| {
        for p in [0.5, 0.95, 0.99] {
            let from_hist = histogram.percentile(p).unwrap();
            let exact_value = exact(values, p);
            let (a, b) = (
                bucket_index(from_hist) as i64,
                bucket_index(exact_value) as i64,
            );
            assert!((a - b).abs() <= 1, "p{} from the merged histogram {from_hist} sits {} buckets from the exact {exact_value}", (p * 100.0) as u32, (a - b).abs());
        }
    };
    // Over the whole day.
    let mut merged = Histogram::new();
    for row in &rollups {
        merged.merge(&row.ttfb_hist);
    }
    let mut values: Vec<f64> = rows
        .iter()
        .filter_map(|(r, _)| r.ttfb_ms.map(|t| t as f64))
        .collect();
    within_one_bucket(&merged, &mut values);
    // Over one group-by: per account.
    for account in ["a", "b", "c"] {
        let mut merged = Histogram::new();
        for row in rollups.iter().filter(|r| r.account_id == account) {
            merged.merge(&row.duration_hist);
        }
        let mut values: Vec<f64> = rows
            .iter()
            .filter(|(_, attempts)| attempts.last().is_some_and(|a| a.account_id == account))
            .filter_map(|(r, _)| r.duration_ms.map(|t| t as f64))
            .collect();
        within_one_bucket(&merged, &mut values);
    }
}

#[test]
fn a_breakdown_by_request_class_splits_conversation_from_overhead() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, class: Option<&str>| {
        let (mut request, mut attempts) = synthetic_day(5).swap_remove(0);
        request.id = id.into();
        request.key_id = Some("k1".into());
        request.key_generation = 0;
        request.received_at = 1_000;
        request.kind = "inference".into();
        request.request_class = class.map(String::from);
        for attempt in &mut attempts {
            attempt.request_id = id.into();
        }
        db.insert_request(&request, &attempts, None).unwrap();
    };
    plant("r1", Some("main"));
    plant("r2", Some("compaction"));
    plant("r3", None);
    let live = LiveKeys(BTreeMap::from([("k1".to_string(), 0)]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default());
    let groups = breakdown(&filtered, "request_class", false, &live, &[], None);
    let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
    assert!(
        keys.contains(&"main") && keys.contains(&"compaction") && keys.contains(&NO_CLASS),
        "the named classes and the classless overhead land in their own groups: {keys:?}"
    );
}

/// One inference request of key `k` whose attempts answered with the given `usage.speed`s, in order; the last
/// is the serving attempt, and an earlier one is a 429.
fn served_at(id: &str, speeds: &[Option<&str>]) -> (RequestRecord, Vec<AttemptRecord>) {
    let request = RequestRecord {
        id: id.into(),
        received_at: 3_600_000,
        key_id: Some("k".into()),
        pool_id: Some("p".into()),
        canonical_model: "claude-opus-5".into(),
        model: "claude-opus-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: speeds.len() as u32,
        ..Default::default()
    };
    let attempts = speeds
        .iter()
        .enumerate()
        .map(|(i, speed)| {
            let serving = i + 1 == speeds.len();
            AttemptRecord {
                request_id: id.into(),
                attempt: i as u32 + 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                started_at: 3_600_000 + i as i64,
                upstream_status: Some(if serving { 200 } else { 429 }),
                outcome: if serving { "succeeded" } else { "rate_limited" }.into(),
                output_tokens: serving.then_some(10),
                speed: speed.map(String::from),
                ..Default::default()
            }
        })
        .collect();
    (request, attempts)
}

#[test]
fn a_fast_answer_files_its_request_under_speed_fast_and_a_silent_answer_under_standard() {
    let fast = served_at("r-fast", &[Some("fast")]);
    let silent = served_at("r-silent", &[None]);
    assert_eq!(delta_of(&fast.0, &fast.1).speed, "fast");
    assert_eq!(
        delta_of(&silent.0, &silent.1).speed,
        "standard",
        "an answer that named no speed files under standard"
    );
    let db = LedgerDb::open_in_memory().unwrap();
    for (request, attempts) in [&fast, &silent] {
        db.insert_request(request, attempts, None).unwrap();
    }
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.speed.as_str(), r.requests))
            .collect::<Vec<_>>(),
        vec![("fast", 1), ("standard", 1)],
        "every other key part is equal, so the speed alone keeps the two requests on rows of their own"
    );
}

#[test]
fn group_by_speed_splits_the_rows_by_served_speed() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, speed) in [
        ("r1", Some("fast")),
        ("r2", Some("fast")),
        ("r3", Some("standard")),
        ("r4", None),
    ] {
        let (request, attempts) = served_at(id, &[speed]);
        db.insert_request(&request, &attempts, None).unwrap();
    }
    let live = LiveKeys(BTreeMap::from([("k".to_string(), 0)]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default());
    let groups = breakdown(&filtered, "speed", false, &live, &[], None);
    assert_eq!(
        groups
            .iter()
            .map(|g| (g.key.as_str(), g.requests))
            .collect::<Vec<_>>(),
        vec![("fast", 2), ("standard", 2)],
        "a named standard and a silent answer share one group"
    );
}

#[test]
fn the_speed_filter_reads_the_serving_attempts_speed() {
    let db = LedgerDb::open_in_memory().unwrap();
    let (request, attempts) = served_at("r-moved", &[None, Some("fast")]);
    db.insert_request(&request, &attempts, None).unwrap();
    let (request, attempts) = served_at("r-plain", &[None]);
    db.insert_request(&request, &attempts, None).unwrap();
    let listed = |speeds: &[&str]| {
        let filter = RequestFilter {
            speeds: speeds.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        db.read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), None, 10))
            .unwrap()
            .0
            .into_iter()
            .map(|(r, _)| r.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        listed(&["fast"]),
        vec!["r-moved"],
        "the serving attempt answered fast"
    );
    assert_eq!(
        listed(&["standard"]),
        vec!["r-plain"],
        "the 429 before it named no speed, which must not read as standard"
    );
}

/// One request for the share cases, through `insert_request`: the attempt's five token counts as given, its head's
/// overage flag and its answer's speed as named, and a client where a case needs the row keyed apart from a credit.
struct ShareRow<'a> {
    id: &'a str,
    account: &'a str,
    account_kind: &'a str,
    kind: &'a str,
    model: &'a str,
    at: i64,
    usage: PinUsage,
    overage: Option<bool>,
    speed: Option<&'a str>,
    client: Option<&'a str>,
    usd: f64,
}

impl Default for ShareRow<'_> {
    fn default() -> Self {
        ShareRow {
            id: "r",
            account: "a",
            account_kind: "oauth",
            kind: "inference",
            model: "claude-fable-5-1",
            at: WEEKDAY_OFF_PEAK,
            usage: usage(1_000, 100),
            overage: None,
            speed: None,
            client: None,
            usd: 0.0,
        }
    }
}

fn plant_share(db: &LedgerDb, row: ShareRow<'_>) {
    db.insert_request(
        &RequestRecord {
            id: row.id.into(),
            received_at: row.at,
            finished_at: Some(row.at + 500),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            session_id: Some("s".into()),
            canonical_model: row.model.into(),
            model: row.model.into(),
            kind: row.kind.into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            client: row.client.map(String::from),
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: row.id.into(),
            attempt: 1,
            account_id: row.account.into(),
            account_kind: row.account_kind.into(),
            started_at: row.at,
            finished_at: Some(row.at + 500),
            outcome: "succeeded".into(),
            input_tokens: Some(row.usage.input),
            cache_read_tokens: Some(row.usage.cache_read),
            cache_write_5m_tokens: Some(row.usage.write_5m),
            cache_write_1h_tokens: Some(row.usage.write_1h),
            output_tokens: Some(row.usage.output),
            estimated_usd: Some(row.usd),
            overage_in_use: row.overage,
            speed: row.speed.map(String::from),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// The oauth account `a` and the api_key account `k` of the share cases.
fn share_kinds() -> HashMap<String, AccountKind> {
    HashMap::from([
        ("a".to_string(), AccountKind::Oauth),
        ("k".to_string(), AccountKind::ApiKey),
    ])
}

fn share_tiers(tier: &str) -> Tiers {
    HashMap::from([("a".to_string(), Some(tier.to_string()))])
}

/// The hour of `at` as `[from, to)` in milliseconds, `from` the instant every row of the hour is priced at.
fn hour_range(at: i64) -> (i64, i64) {
    let from = hour_of(at) * 1_000;
    (from, from + 3_600_000)
}

/// The account-grouped breakdown of `[from, to)` under `filter`, priced where `pricing` is given.
async fn account_groups(
    db: &std::sync::Arc<LedgerDb>,
    filter: &RollupFilter,
    from: i64,
    to: i64,
    pricing: Option<SharePricing<'_>>,
) -> Vec<GroupRow> {
    breakdown_paged(
        db,
        filter,
        &LiveKeys::default(),
        from,
        to,
        "account",
        pricing,
    )
    .await
    .unwrap()
}

fn group<'a>(groups: &'a [GroupRow], key: &str) -> &'a GroupRow {
    groups
        .iter()
        .find(|g| g.key == key)
        .unwrap_or_else(|| panic!("the group {key} is answered: {groups:?}"))
}

fn close(got: Option<&f64>, want: f64) -> bool {
    got.is_some_and(|g| (g - want).abs() < 1e-12)
}

fn with_probes() -> RollupFilter {
    RollupFilter {
        include_probes: true,
        ..RollupFilter::default()
    }
}

/// A Fable share on `test_tier` at the off-band hour, the share cases' instant.
fn fable_shares(tables: &PlanTables, usage: PinUsage) -> WindowShares {
    price_shares(
        tables,
        "test_tier",
        "claude-fable-5-1",
        usage,
        hour_range(WEEKDAY_OFF_PEAK).0,
    )
}

#[tokio::test]
async fn a_breakdown_row_under_the_account_grouping_sums_its_plan_rows_shares_and_a_cached_row_s_read_and_write(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let plain = usage(1_000, 100);
    let cached = PinUsage {
        input: 500,
        cache_read: 2_000,
        write_5m: 300,
        output: 50,
        ..PinUsage::default()
    };
    let probe = PinUsage {
        cache_read: 2_000,
        ..PinUsage::default()
    };
    plant_share(
        &db,
        ShareRow {
            id: "r-plain",
            usage: plain,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-cached",
            usage: cached,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            kind: "probe",
            usage: probe,
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let a = group(&groups, "a");
    let cost = a
        .cost_shares
        .as_ref()
        .expect("an oauth group with a tier prices its shares");
    assert_eq!(
        cost.keys().copied().collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE],
        "a Fable account prices every window it carries: {cost:?}"
    );
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        let want = fable_shares(&tables, plain)[window].unwrap()
            + fable_shares(&tables, cached)[window].unwrap();
        assert!(
            close(cost.get(window), want),
            "the two plan rows' shares sum on {window}: {cost:?} vs {want}"
        );
    }
    let cache = a.cache_shares.as_ref().expect("the cache shares");
    let read = fable_shares(
        &tables,
        PinUsage {
            cache_read: 2_000,
            ..PinUsage::default()
        },
    )[SEVEN_DAY]
        .unwrap();
    let write = fable_shares(
        &tables,
        PinUsage {
            write_5m: 300,
            ..PinUsage::default()
        },
    )[SEVEN_DAY]
        .unwrap();
    assert!(
        read > 0.0
            && cache.get(SEVEN_DAY).is_some_and(|s| {
                (s.read - read).abs() < 1e-12 && (s.write - write).abs() < 1e-12
            }),
        "the cached row's read and write shares, the plain row adding nothing: {cache:?} vs {read} and {write}"
    );
    let probes = a.probe_shares.as_ref().expect("the probe shares");
    let probe_cost = fable_shares(&tables, probe)[FIVE_HOUR].unwrap();
    assert!(
        probes
            .get(FIVE_HOUR)
            .is_some_and(|s| (s.cost - probe_cost).abs() < 1e-12 && s.wasted.is_none()),
        "the probe row's share apart from the request side, and no probe dollars to read a waste from: \
         {probes:?} vs {probe_cost}"
    );
    assert_eq!(
        (a.plan_requests, a.plan_output_tokens),
        (Some(2), Some(150)),
        "the two plan rows and their output tokens: {a:?}"
    );
}

#[tokio::test]
async fn an_unpriced_row_nulls_the_window_it_could_not_price_and_leaves_the_other() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-narrow",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("narrow_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let cost = group(&groups, "a")
        .cost_shares
        .as_ref()
        .expect("the narrow tier still prices the 5-hour window");
    let want = price_shares(
        &tables,
        "narrow_tier",
        "claude-fable-5-1",
        usage(1_000, 100),
        from,
    )[FIVE_HOUR]
        .unwrap();
    assert_eq!(
        cost.keys().copied().collect::<Vec<_>>(),
        [FIVE_HOUR],
        "a tier sized on the 5-hour window alone answers neither week: {cost:?}"
    );
    assert!(close(cost.get(FIVE_HOUR), want), "{cost:?} vs {want}");
}

#[test]
fn a_haiku_row_adds_nothing_to_the_fable_week_and_never_nulls_it() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_share(
        &db,
        ShareRow {
            id: "r-fable",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-haiku",
            model: "claude-haiku-4-5",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, _) = hour_range(WEEKDAY_OFF_PEAK);
    let mut rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(rows.len(), 2, "one rollup row per model");
    let fable = fable_shares(&tables, usage(1_000, 100));
    let haiku = price_shares(
        &tables,
        "test_tier",
        "claude-haiku-4-5",
        usage(1_000, 100),
        from,
    );
    for order in ["as read", "reversed"] {
        let groups = breakdown(
            &rows,
            "account",
            false,
            &LiveKeys::default(),
            &[],
            Some(pricing),
        );
        let cost = group(&groups, "a")
            .cost_shares
            .as_ref()
            .expect("the shares are priced");
        assert!(
            close(cost.get(SEVEN_DAY_FABLE), fable[SEVEN_DAY_FABLE].unwrap()),
            "the Fable week is the Fable row's alone, rows {order}: {cost:?} vs {fable:?}"
        );
        assert!(
            close(
                cost.get(FIVE_HOUR),
                fable[FIVE_HOUR].unwrap() + haiku[FIVE_HOUR].unwrap()
            ) && close(
                cost.get(SEVEN_DAY),
                fable[SEVEN_DAY].unwrap() + haiku[SEVEN_DAY].unwrap()
            ),
            "the shared windows sum both rows, rows {order}: {cost:?}"
        );
        rows.reverse();
    }
}

#[tokio::test]
async fn a_genuinely_unpriced_fable_row_nulls_the_week() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-fable",
            ..ShareRow::default()
        },
    );
    // An id whose tier the cost model does not place, so nothing prices it, and which names the Fable week, so the
    // week applies to it.
    plant_share(
        &db,
        ShareRow {
            id: "r-fable-unweighted",
            model: "claude-mystery-fable-1",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let a = group(&groups, "a");
    assert!(
        a.cost_shares.as_ref().is_some_and(|m| m.is_empty()),
        "the weightless row prices on no window, so the week and the shared windows are absent alike: {a:?}"
    );
    assert_eq!(
        a.plan_requests,
        Some(2),
        "both plan rows were summed over: {a:?}"
    );
}

#[tokio::test]
async fn the_probe_inclusion_rule_governs_the_request_shares() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let inference = usage(1_000, 100);
    let probe = PinUsage {
        cache_read: 2_000,
        ..PinUsage::default()
    };
    plant_share(
        &db,
        ShareRow {
            id: "r-inference",
            usage: inference,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            kind: "probe",
            usage: probe,
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let inference_share = fable_shares(&tables, inference)[FIVE_HOUR].unwrap();
    let probe_share = fable_shares(&tables, probe)[FIVE_HOUR].unwrap();
    let read = |filter: RollupFilter| {
        let db = db.clone();
        async move {
            let groups = account_groups(&db, &filter, from, to, Some(pricing)).await;
            let a = group(&groups, "a");
            (
                a.cost_shares
                    .as_ref()
                    .and_then(|m| m.get(FIVE_HOUR))
                    .copied(),
                a.plan_requests,
                a.probe_shares
                    .as_ref()
                    .and_then(|m| m.get(FIVE_HOUR))
                    .map(|s| s.cost),
            )
        }
    };
    let (cost, requests, probe_cost) = read(RollupFilter::default()).await;
    assert!(
        close(cost.as_ref(), inference_share) && requests == Some(1),
        "with probes left out the request shares read the inference row alone: {cost:?}, {requests:?}"
    );
    assert!(
        close(probe_cost.as_ref(), probe_share),
        "while the probe share reads the probe row: {probe_cost:?} vs {probe_share}"
    );
    let (cost, requests, probe_cost) = read(with_probes()).await;
    assert!(
        close(cost.as_ref(), inference_share + probe_share) && requests == Some(2),
        "with probes counted the probe row joins the request shares: {cost:?}, {requests:?}"
    );
    assert!(
        close(probe_cost.as_ref(), probe_share),
        "and the probe share stays the probe row's: {probe_cost:?}"
    );
    let (cost, requests, _) = read(RollupFilter {
        kinds: vec!["probe".into()],
        ..RollupFilter::default()
    })
    .await;
    assert!(
        close(cost.as_ref(), probe_share) && requests == Some(1),
        "under a probe kind filter the request shares read the probe row: {cost:?}, {requests:?}"
    );
}

#[tokio::test]
async fn the_wasted_probe_share_reads_the_group_s_dollar_ratio_over_its_separate_rows() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let probe = PinUsage {
        cache_read: 2_000,
        ..PinUsage::default()
    };
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            kind: "probe",
            usage: probe,
            client: Some("claude-code/cli"),
            ..ShareRow::default()
        },
    );
    // The engine writes the probe's dollars and a settlement's waste as rows of their own, here in the next hour.
    let next_hour = WEEKDAY_OFF_PEAK + 3_600_000;
    let credit = |at: i64| KeepaliveDelta {
        at,
        key_id: "k".into(),
        pool_id: "p".into(),
        account_id: "a".into(),
        canonical_model: "claude-fable-5-1".into(),
        kind: "probe".into(),
        ..Default::default()
    };
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                probe_cost_usd: 0.04,
                ..credit(next_hour)
            },
        )?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                wasted_probes: 1,
                wasted_probe_usd: 0.01,
                ..credit(next_hour)
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, _) = hour_range(WEEKDAY_OFF_PEAK);
    let (credits_from, credits_to) = hour_range(next_hour);
    let groups = account_groups(&db, &with_probes(), from, credits_to, Some(pricing)).await;
    let probes = group(&groups, "a")
        .probe_shares
        .as_ref()
        .expect("the probe shares");
    let cost = fable_shares(&tables, probe)[FIVE_HOUR].unwrap();
    assert!(
        probes.get(FIVE_HOUR).is_some_and(|s| {
            (s.cost - cost).abs() < 1e-12
                && s.wasted
                    .is_some_and(|w| w > 0.0 && (w - cost / 4.0).abs() < 1e-12)
        }),
        "the wasted part is the probe share times a cent over four cents: {probes:?} vs {cost}"
    );
    let credits =
        account_groups(&db, &with_probes(), credits_from, credits_to, Some(pricing)).await;
    let probes = group(&credits, "a")
        .probe_shares
        .as_ref()
        .expect("the credit rows' group still prices in shares");
    assert!(
        !probes.contains_key(FIVE_HOUR),
        "no probe request row priced into the hour, so there is no share to read a waste off: {probes:?}"
    );
}

#[tokio::test]
async fn an_overage_row_and_an_api_key_group_add_to_no_share() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-plan",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-overage",
            usage: usage(2_000, 200),
            overage: Some(true),
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-api",
            account: "k",
            account_kind: "api_key",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let a = group(&groups, "a");
    let plan = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    assert!(
        close(a.cost_shares.as_ref().and_then(|m| m.get(FIVE_HOUR)), plan)
            && (a.plan_requests, a.plan_output_tokens) == (Some(1), Some(100)),
        "the overage row's tokens are absent from the sums and from the counts: {a:?} vs {plan}"
    );
    let k = group(&groups, "k");
    assert!(
        k.cost_shares.is_none()
            && k.cache_shares.is_none()
            && k.probe_shares.is_none()
            && k.plan_requests.is_none()
            && k.plan_output_tokens.is_none(),
        "an api_key group answers no share: {k:?}"
    );
}

#[tokio::test]
async fn a_series_bucket_prices_as_the_breakdown_does() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-plain",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-cached",
            usage: PinUsage {
                input: 500,
                cache_read: 2_000,
                write_1h: 300,
                output: 50,
                ..PinUsage::default()
            },
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            kind: "probe",
            usage: PinUsage {
                cache_read: 2_000,
                ..PinUsage::default()
            },
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (kinds, tiers) = (share_kinds(), share_tiers("test_tier"));
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let filter = RollupFilter::default();
    let breakdown = account_groups(&db, &filter, from, to, Some(pricing)).await;
    let points = series_paged(
        &db,
        &filter,
        &LiveKeys::default(),
        from,
        to,
        3_600,
        "account",
        Some(pricing),
    )
    .await
    .unwrap();
    let point = points
        .iter()
        .find(|p| p.at == from)
        .expect("the planted hour's bucket");
    let row = group(&breakdown, "a");
    assert!(
        row.cost_shares.as_ref().is_some_and(|m| !m.is_empty()),
        "the breakdown prices the account: {row:?}"
    );
    assert_eq!(
        serde_json::to_value(group(&point.groups, "a")).unwrap(),
        serde_json::to_value(row).unwrap(),
        "the bucket's group carries the breakdown row's shares"
    );
}

#[tokio::test]
async fn the_funding_filter_partitions_the_rows() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-overage",
            overage: Some(true),
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-fast",
            speed: Some("fast"),
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-api",
            account: "k",
            account_kind: "api_key",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-plan",
            ..ShareRow::default()
        },
    );
    // An account the registry no longer holds: its rows file under `plan` and no kind is on file for it.
    plant_share(
        &db,
        ShareRow {
            id: "r-gone",
            account: "gone",
            ..ShareRow::default()
        },
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let read = |funding: Option<FundingFilter>| {
        let db = db.clone();
        async move {
            let filter = RollupFilter {
                funding,
                ..RollupFilter::default()
            };
            account_groups(&db, &filter, from, to, None)
                .await
                .iter()
                .map(|g| (g.key.clone(), g.requests))
                .collect::<Vec<_>>()
        }
    };
    let billed = read(Some(FundingFilter::Billed)).await;
    let plan = read(Some(FundingFilter::Plan)).await;
    let every = read(None).await;
    assert_eq!(
        billed,
        [("a".to_string(), 2), ("k".to_string(), 1)],
        "billed keeps the overage row, the fast row and the api_key row"
    );
    assert_eq!(
        plan,
        [("a".to_string(), 1), ("gone".to_string(), 1)],
        "plan keeps the plan row and the row of the account the registry no longer holds"
    );
    let sum = |rows: &[(String, u64)]| rows.iter().map(|(_, n)| n).sum::<u64>();
    assert_eq!(
        (sum(&billed), sum(&plan), sum(&every)),
        (3, 2, 5),
        "the two subsets partition the rows"
    );
}

/// A row files under the funding its serving attempt had, so the registry's later state moves none of it: an api_key
/// account's rows stay billed once the account is deleted and once its id comes back as an oauth account with a tier
/// on file, and that recreation prices none of them as a share.
#[tokio::test]
async fn an_api_key_account_s_rows_stay_billed_when_the_registry_drops_or_recreates_the_account() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-api",
            account: "k",
            account_kind: "api_key",
            ..ShareRow::default()
        },
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let tables = share_tables();
    let counts = |groups: &[GroupRow]| {
        groups
            .iter()
            .map(|g| (g.key.clone(), g.requests))
            .collect::<Vec<_>>()
    };
    let deleted: HashMap<String, AccountKind> = HashMap::new();
    let recreated = HashMap::from([("k".to_string(), AccountKind::Oauth)]);
    let tiers: Tiers = HashMap::from([("k".to_string(), Some("test_tier".to_string()))]);
    for (registry, kinds) in [("deleted", &deleted), ("recreated as oauth", &recreated)] {
        let pricing = SharePricing {
            tables: &tables,
            kinds,
            tiers: &tiers,
        };
        let read = |funding: FundingFilter| {
            let filter = RollupFilter {
                funding: Some(funding),
                ..RollupFilter::default()
            };
            let db = db.clone();
            async move { account_groups(&db, &filter, from, to, Some(pricing)).await }
        };
        let billed = read(FundingFilter::Billed).await;
        let plan = read(FundingFilter::Plan).await;
        assert_eq!(
            counts(&billed),
            [("k".to_string(), 1)],
            "the api_key row stays billed with the account {registry}"
        );
        assert!(
            plan.is_empty(),
            "and leaves the plan side empty with the account {registry}: {plan:?}"
        );
        assert!(
            group(&billed, "k")
                .cost_shares
                .as_ref()
                .is_none_or(|m| m.is_empty()),
            "and prices no share with the account {registry}: {billed:?}"
        );
    }
}

/// The mirror: an oauth account's plan row stays on the plan side once the registry says its id is an api_key account.
#[tokio::test]
async fn an_oauth_account_s_plan_row_stays_plan_when_its_id_is_recreated_as_an_api_key_account() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-plan",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let kinds = HashMap::from([("a".to_string(), AccountKind::ApiKey)]);
    let tiers = Tiers::new();
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let read = |funding: FundingFilter| {
        let db = db.clone();
        async move {
            let filter = RollupFilter {
                funding: Some(funding),
                ..RollupFilter::default()
            };
            account_groups(&db, &filter, from, to, Some(pricing))
                .await
                .iter()
                .map(|g| (g.key.clone(), g.requests))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        read(FundingFilter::Plan).await,
        [("a".to_string(), 1)],
        "the plan row stays on the plan side"
    );
    assert!(
        read(FundingFilter::Billed).await.is_empty(),
        "and never moves to the billed side"
    );
}

#[tokio::test]
async fn an_overage_probe_s_cost_files_under_billed_and_its_settlement_rides_under_plan() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            kind: "probe",
            usage: PinUsage {
                cache_read: 2_000,
                ..PinUsage::default()
            },
            overage: Some(true),
            usd: 0.05,
            ..ShareRow::default()
        },
    );
    let delta = || KeepaliveDelta {
        at: WEEKDAY_OFF_PEAK,
        key_id: "k".into(),
        pool_id: "p".into(),
        account_id: "a".into(),
        canonical_model: "claude-fable-5-1".into(),
        kind: "probe".into(),
        ..Default::default()
    };
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                overage_in_use: Some(true),
                probe_cost_usd: 0.04,
                ..delta()
            },
        )?;
        // A settlement carries no funding of its own and files under the plan.
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                wasted_probes: 1,
                wasted_probe_usd: 0.01,
                ..delta()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let funding_of = |pick: fn(&RollupRow) -> bool| {
        rows.iter()
            .find(|r| pick(r))
            .map(|r| r.funding.as_str())
            .expect("the row is folded")
    };
    assert_eq!(
        (
            funding_of(|r| r.probe_cost_usd > 0.0),
            funding_of(|r| r.wasted_probe_usd > 0.0)
        ),
        (OVERAGE_FUNDING, PLAN_FUNDING),
        "the credit files under the probe's overage and the settlement under the plan: {rows:?}"
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let read = |funding: FundingFilter| {
        let db = db.clone();
        async move {
            let filter = RollupFilter {
                funding: Some(funding),
                ..with_probes()
            };
            let groups = account_groups(&db, &filter, from, to, None).await;
            let a = group(&groups, "a");
            (a.cost_usd, a.probe_cost_usd, a.wasted_probe_usd)
        }
    };
    let billed = read(FundingFilter::Billed).await;
    assert!(
        (billed.0 - 0.05).abs() < 1e-12 && (billed.1 - 0.04).abs() < 1e-12 && billed.2 == 0.0,
        "billed keeps the probe's request dollars and its credit, and no waste: {billed:?}"
    );
    let plan = read(FundingFilter::Plan).await;
    assert!(
        plan.0 == 0.0 && plan.1 == 0.0 && (plan.2 - 0.01).abs() < 1e-12,
        "plan keeps the settlement's waste alone: {plan:?}"
    );
}

#[test]
fn a_fast_recipe_s_credit_files_under_fast_whatever_its_overage_flag() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                at: WEEKDAY_OFF_PEAK,
                key_id: "k".into(),
                pool_id: "p".into(),
                account_id: "a".into(),
                canonical_model: "claude-fable-5-1".into(),
                kind: "probe".into(),
                probe_cost_usd: 0.04,
                overage_in_use: Some(true),
                speed: Some(Speed::Fast),
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.speed.as_str(), r.funding.as_str()))
            .collect::<Vec<_>>(),
        vec![("fast", FAST_FUNDING)],
        "the recipe's speed keys the credit before the overage flag: {rows:?}"
    );
}

#[tokio::test]
async fn a_tierless_oauth_group_and_an_account_the_registry_lacks_answer_no_share() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-tierless",
            account: "t",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-unknown",
            account: "u",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let mut kinds = share_kinds();
    kinds.insert("t".to_string(), AccountKind::Oauth);
    let tiers = share_tiers("test_tier");
    let pricing = SharePricing {
        tables: &tables,
        kinds: &kinds,
        tiers: &tiers,
    };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    for key in ["t", "u"] {
        let g = group(&groups, key);
        assert!(
            g.cost_shares.is_none()
                && g.cache_shares.is_none()
                && g.probe_shares.is_none()
                && g.plan_requests.is_none()
                && g.plan_output_tokens.is_none(),
            "{key} answers the five members null: {g:?}"
        );
    }
}

#[test]
fn the_burn_series_prices_a_subscription_row_in_window_share_and_an_api_key_row_in_dollars() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, account: &str, kind: &str, usd: f64, cache_read: Option<u64>| {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: 3_600_000,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                canonical_model: "claude-sonnet-5-5".into(),
                model: "claude-sonnet-5-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            &[AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: account.into(),
                account_kind: kind.into(),
                outcome: "succeeded".into(),
                input_tokens: Some(1_000),
                cache_read_tokens: cache_read,
                output_tokens: Some(100),
                estimated_usd: Some(usd),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    };
    // Both rows carry a nonzero `estimated_usd`, so a burn that priced every row both ways instead of by its
    // funding would still pass the dollar assertion; only pricing by funding can pass both.
    plant("sub", "oauth-a", "oauth", 9.99, None);
    plant("key", "api-b", "api_key", 1.25, None);
    // Absent from the registry below: a plan row of an account the registry no longer holds prices no share without
    // a tier and no dollar under the plan, while its tokens still count, its cache reads included, the one row
    // carrying any so the cached half of the assertion below reads them.
    plant("unknown", "unknown-c", "oauth", 4.0, Some(500));
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("oauth-a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let points = db
        .read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
        .unwrap();
    let point = points
        .iter()
        .find(|p| p.five_hour_share.is_some() || p.usd > 0.0)
        .expect("the bucket every tracked row landed in");
    let hour = phoebis_ledger::hour_of(3_600_000);
    let band = tables.band_factor(FIVE_HOUR, hour * 1_000);
    let tokens = usage(1_000, 100);
    let five_hour = share_of(
        &tables,
        FIVE_HOUR,
        MAX_20X,
        "claude-sonnet-5-5",
        tokens,
        WEEKDAY_OFF_PEAK,
    ) * band;
    assert!(
        (point.five_hour_share.unwrap() - five_hour).abs() < 1e-6,
        "the subscription row alone prices its 5-hour share: {:?} vs {five_hour}",
        point.five_hour_share
    );
    let seven_day = share_of(
        &tables,
        SEVEN_DAY,
        MAX_20X,
        "claude-sonnet-5-5",
        tokens,
        hour * 1_000,
    );
    assert!(
        (point.seven_day_share.unwrap() - seven_day).abs() < 1e-6,
        "the subscription row alone prices its 7-day share: {:?} vs {seven_day}",
        point.seven_day_share
    );
    assert!(
        (point.usd - 1.25).abs() < 1e-9,
        "the api-key row alone prices in dollars from estimated_usd"
    );
    assert_eq!(
        (point.cached_tokens, point.uncached_tokens),
        (500, 3_000),
        "the row of the account the registry no longer holds counts its tokens and prices nothing"
    );
}

/// The burn prices a row by the funding its key carries, so the registry's later state moves no dollar into a share:
/// an api_key account's row reads the same dollars and no share once a sized tier is on file for its id, as it is
/// once an oauth account takes the id over.
#[test]
fn an_api_key_account_s_burn_row_reads_in_dollars_once_its_id_is_recreated_as_oauth() {
    let tables = PlanTables::shipped();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let row = RollupRow {
        input_tokens: 1_000,
        estimated_usd: 1.25,
        ..RollupRow::key(
            3_600,
            "k",
            0,
            "p",
            "k",
            "claude-sonnet-5-5",
            "inference",
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            "standard",
            BILLED_FUNDING,
        )
    };
    let reading = |tiers: Tiers| {
        let point = burn_series_over(std::slice::from_ref(&row), &tables, &tiers, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap();
        (
            point.usd,
            point.five_hour_share,
            point.seven_day_share,
            point.tokens,
        )
    };
    let recorded = reading(Tiers::new());
    assert_eq!(
        recorded,
        (1.25, None, None, 1_000),
        "an api_key account's row prices in dollars from estimated_usd"
    );
    let recreated = reading(HashMap::from([(
        "k".to_string(),
        Some(MAX_20X.to_string()),
    )]));
    assert_eq!(
        recreated, recorded,
        "and reads the same once a sized tier is on file for the id"
    );
}

#[test]
fn a_fast_row_never_lifts_a_window_an_unpriced_row_of_its_bucket_nulled() {
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    // The shipped model places Mystery 2 in no tier, so its standard row leaves the bucket unpriced on both windows.
    let row = |model: &str, speed: &str, estimated_usd: f64| RollupRow {
        input_tokens: 1_000,
        estimated_usd,
        ..RollupRow::key(
            3_600,
            "k",
            0,
            "p",
            "a",
            model,
            "inference",
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            speed,
            if speed == "fast" {
                FAST_FUNDING
            } else {
                PLAN_FUNDING
            },
        )
    };
    let unpriced = row("claude-mystery-2", "standard", 0.0);
    let fast = row("claude-opus-5-5", "fast", 0.25);
    for rows in [[unpriced.clone(), fast.clone()], [fast, unpriced]] {
        let point = burn_series_over(&rows, &tables, &tiers, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap();
        assert_eq!(
            (point.usd, point.five_hour_share, point.seven_day_share),
            (0.25, None, None),
            "the fast row's zero never stands for a bucket holding a row the table cannot price, whichever came first"
        );
    }
}

#[test]
fn a_fast_row_adds_dollars_and_no_share_to_a_subscription_burn_point() {
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let point = |speed: &str, tiers: &Tiers| {
        let row = RollupRow {
            input_tokens: 1_000,
            output_tokens: 100,
            estimated_usd: 0.25,
            ..RollupRow::key(
                3_600,
                "k",
                0,
                "p",
                "a",
                "claude-opus-5-5",
                "inference",
                UNATTRIBUTED,
                UNATTRIBUTED,
                "none",
                NO_CLASS,
                speed,
                if speed == "fast" {
                    FAST_FUNDING
                } else {
                    PLAN_FUNDING
                },
            )
        };
        burn_series_over(&[row], &tables, tiers, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap()
    };
    let standard = point("standard", &tiers);
    assert!(
        standard.five_hour_share.is_some() && standard.usd == 0.0,
        "a standard row on a subscription reads a share and no dollars: {standard:?}"
    );
    let fast = point("fast", &tiers);
    assert_eq!(
        (
            fast.usd,
            fast.five_hour_share,
            fast.seven_day_share,
            fast.uncached_tokens
        ),
        (0.25, Some(0.0), Some(0.0), 1_000),
        "a fast row draws usage credits: its dollars and its tokens, and a zero share of either window that \
         still says the tier is sized"
    );
    let tierless = point("fast", &Tiers::new());
    assert_eq!(
        (
            tierless.usd,
            tierless.five_hour_share,
            tierless.seven_day_share
        ),
        (0.25, None, None),
        "without a tier the tables size, a fast row reads its dollars and no share"
    );
}

/// A request that asked for fast mode and was answered without it files under the fast speed while its funding reads
/// the served answer: the burn prices it on the plan it drew from, as a standard row of the same usage.
#[test]
fn a_row_filed_under_the_asked_fast_speed_with_plan_funding_prices_as_a_standard_row() {
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let point = |speed: &str| {
        let row = RollupRow {
            input_tokens: 1_000,
            output_tokens: 100,
            ..RollupRow::key(
                3_600,
                "k",
                0,
                "p",
                "a",
                "claude-opus-5-5",
                "inference",
                UNATTRIBUTED,
                UNATTRIBUTED,
                "none",
                NO_CLASS,
                speed,
                PLAN_FUNDING,
            )
        };
        burn_series_over(&[row], &tables, &tiers, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap()
    };
    let standard = point("standard");
    assert!(
        standard.five_hour_share.is_some_and(|share| share > 0.0),
        "{standard:?}"
    );
    assert_eq!(
        point("fast"),
        standard,
        "an asked speed never prices as a served one"
    );
}

#[test]
fn fast_traffic_alone_on_a_model_without_a_weight_reads_a_zero_share_on_a_sized_tier() {
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let row = RollupRow {
        input_tokens: 1_000,
        output_tokens: 100,
        estimated_usd: 0.25,
        ..RollupRow::key(
            3_600,
            "k",
            0,
            "p",
            "a",
            "claude-opus-4-8",
            "inference",
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            "fast",
            FAST_FUNDING,
        )
    };
    let share =
        |model: &str| tables.share_at(FIVE_HOUR, MAX_20X, model, usage(1_000, 100), 3_600_000);
    assert!(
        share("claude-opus-4-8").is_some() && share("claude-opus-4-8") == share("claude-opus-5-5"),
        "the model has no weight of its own and prices at Opus 5.5's, the latest weighted model of its tier"
    );
    let point = burn_series_over(&[row], &tables, &tiers, &filter)
        .unwrap()
        .into_iter()
        .find(|p| p.at == 3_600_000)
        .unwrap();
    assert_eq!(
        (point.usd, point.five_hour_share, point.seven_day_share),
        (0.25, Some(0.0), Some(0.0)),
        "the tier alone sizes the account, so its fast dollars sit beside a zero share of either window"
    );
}

/// The week's band factor is 1 in both the shipped and the test model; a file that bands the week weighs the 7-day
/// share by the band at the row's hour, as it weighs the 5-hour share, with no change to the analytics.
#[test]
fn a_banded_week_weighs_the_seven_day_share_by_the_band_at_the_rows_hour() {
    let banded_week = phoebis_ledger::test_tables::first_entry_costs_text().replacen(
        "capacity_factor: {five_hour: 0.5, seven_day: 1}",
        "capacity_factor: {five_hour: 0.5, seven_day: 0.5}",
        1,
    );
    let tables = PlanTables {
        revisions: CostRevisions::from_yaml(&banded_week).expect("the banded week parses"),
        path: None,
        source: "test".to_string(),
    };
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: WEEKDAY_PEAK - 3_600_000,
        to: WEEKDAY_OFF_PEAK + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let week_at = |at: i64| {
        let row = RollupRow {
            input_tokens: 1_000,
            ..RollupRow::key(
                hour_of(at),
                "k",
                0,
                "p",
                "a",
                "claude-opus-5-5",
                "inference",
                UNATTRIBUTED,
                UNATTRIBUTED,
                "none",
                NO_CLASS,
                "standard",
                PLAN_FUNDING,
            )
        };
        burn_series_over(&[row], &tables, &tiers, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == hour_of(at) * 1_000)
            .unwrap()
            .seven_day_share
            .unwrap()
    };
    // The test model: Opus 5.5 weighs 2 and an input token multiplies by 1, so 1,000 input tokens are 2,000 units
    // of the Max 20x week's 1,000; inside the band the week holds the 0.5 this test sets.
    assert!(
        (week_at(WEEKDAY_OFF_PEAK) - 2.0).abs() < 1e-12,
        "{}",
        week_at(WEEKDAY_OFF_PEAK)
    );
    assert!(
        (week_at(WEEKDAY_PEAK) - 2.0 / 0.5).abs() < 1e-12,
        "{}",
        week_at(WEEKDAY_PEAK)
    );
}

#[test]
fn an_hour_inside_the_peak_band_weighs_the_five_hour_share_and_prices_the_seven_day_share_at_its_hour(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, at) in [("peak", WEEKDAY_PEAK), ("off_peak", WEEKDAY_OFF_PEAK)] {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: at,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                canonical_model: "claude-sonnet-5-5".into(),
                model: "claude-sonnet-5-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            &[AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                input_tokens: Some(1_000),
                output_tokens: Some(100),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    }
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: WEEKDAY_PEAK - 3_600_000,
        to: WEEKDAY_OFF_PEAK + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let points = db
        .read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
        .unwrap();
    let peak = points
        .iter()
        .find(|p| p.at == phoebis_ledger::hour_of(WEEKDAY_PEAK) * 1_000)
        .unwrap();
    let off_peak = points
        .iter()
        .find(|p| p.at == phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000)
        .unwrap();
    let tokens = usage(1_000, 100);
    let base = share_of(
        &tables,
        FIVE_HOUR,
        MAX_20X,
        "claude-sonnet-5-5",
        tokens,
        WEEKDAY_OFF_PEAK,
    );
    let factor = tables.band_factor(FIVE_HOUR, WEEKDAY_PEAK);
    assert!(factor > 1.0, "the peak hour lies inside the band");
    assert!(
        (off_peak.five_hour_share.unwrap() - base).abs() < 1e-6,
        "the off-peak row still prices, unweighed: {:?} vs {base}",
        off_peak.five_hour_share
    );
    assert!(
        (peak.five_hour_share.unwrap() - base * factor).abs() < 1e-6,
        "the peak hour weighs the same tokens by the band: {:?} vs {}",
        peak.five_hour_share,
        base * factor
    );
    let week = |at: i64| share_of(&tables, SEVEN_DAY, MAX_20X, "claude-sonnet-5-5", tokens, at);
    for (point, at) in [(peak, WEEKDAY_PEAK), (off_peak, WEEKDAY_OFF_PEAK)] {
        assert!(
            (point.seven_day_share.unwrap() - week(at)).abs() < 1e-6,
            "the 7-day share reads the week's own factor at the row's hour: {:?} vs {}",
            point.seven_day_share,
            week(at)
        );
    }
    // A daily bucket holds both rows; each still reads its own hour's band rather than the bucket's midnight
    // start, or the peak row would lose its weight entirely once the bucket outgrows the hour.
    let daily_filter = SeriesFilter {
        bucket_seconds: 86_400,
        ..filter.clone()
    };
    let daily_points = db
        .read_blocking(|c| burn_series(c, &tables, &tiers, &daily_filter))
        .unwrap();
    let day = daily_points
        .iter()
        .find(|p| p.five_hour_share.is_some())
        .expect("the day both rows landed in");
    assert!(
        (day.five_hour_share.unwrap() - (base * factor + base)).abs() < 1e-6,
        "the peak row weighs the band and the off-peak row 1.0 in the one daily bucket they share: {:?} vs {}",
        day.five_hour_share,
        base * factor + base
    );
    let both = week(WEEKDAY_PEAK) + week(WEEKDAY_OFF_PEAK);
    assert!(
        (day.seven_day_share.unwrap() - both).abs() < 1e-6,
        "the day's 7-day share sums both rows: {:?} vs {both}",
        day.seven_day_share
    );
}

#[test]
fn a_bucket_counts_every_token_type_its_rows_carry() {
    let db = LedgerDb::open_in_memory().unwrap();
    // A cold-cache request: cache writes and output, no uncached input and no cache read.
    db.insert_request(
        &RequestRecord {
            id: "cold".into(),
            received_at: WEEKDAY_OFF_PEAK,
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "cold".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(0),
            cache_write_5m_tokens: Some(3_000),
            cache_write_1h_tokens: Some(1_000),
            output_tokens: Some(200),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), None)]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let point = db
        .read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
        .unwrap()
        .into_iter()
        .find(|p| p.at == hour)
        .unwrap();
    assert_eq!(
        (point.cached_tokens, point.uncached_tokens, point.tokens),
        (0, 0, 4_200),
        "every type counts"
    );
}

#[test]
fn a_row_of_an_account_without_a_tier_prices_no_share_and_an_api_key_row_keeps_its_dollars() {
    let db = LedgerDb::open_in_memory().unwrap();
    // A million Opus 5.5 input tokens per account in one off-band hour.
    let plant = |account: &str, kind: &str, usd: f64| {
        let id = format!("r-{account}");
        db.insert_request(
            &RequestRecord {
                id: id.clone(),
                received_at: WEEKDAY_OFF_PEAK,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                canonical_model: "claude-opus-5-5".into(),
                model: "claude-opus-5-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            &[AttemptRecord {
                request_id: id,
                attempt: 1,
                account_id: account.into(),
                account_kind: kind.into(),
                outcome: "succeeded".into(),
                input_tokens: Some(1_000_000),
                estimated_usd: Some(usd),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    };
    // Every row carries dollars, so only pricing by kind leaves them off the subscription rows.
    plant("max", "oauth", 5.0);
    plant("pro", "oauth", 5.0);
    plant("tierless", "oauth", 5.0);
    plant("key", "api_key", 5.0);
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([
        ("max".to_string(), Some(MAX_20X.to_string())),
        ("pro".to_string(), Some(PRO.to_string())),
        ("tierless".to_string(), None),
    ]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let point_of = |account: &str| {
        let filter = SeriesFilter {
            from: hour,
            to: hour + 3_600_000,
            bucket_seconds: 3_600,
            accounts: vec![account.into()],
            ..SeriesFilter::default()
        };
        db.read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
            .unwrap()
            .into_iter()
            .find(|p| p.at == hour)
            .expect("the planted hour")
    };
    let million = usage(1_000_000, 0);
    let share = |window: &str, tier: &str| {
        share_of(&tables, window, tier, "claude-opus-5-5", million, hour)
    };
    let max = point_of("max");
    assert!(
        (max.five_hour_share.unwrap() - share(FIVE_HOUR, MAX_20X)).abs() < 1e-9
            && (max.seven_day_share.unwrap() - share(SEVEN_DAY, MAX_20X)).abs() < 1e-9,
        "a million Opus 5.5 input tokens read their share of each Max 20x window off the band: {max:?}"
    );
    assert_eq!(max.usd, 0.0, "a subscription spends no dollars");
    let pro = point_of("pro");
    assert!(
        (pro.five_hour_share.unwrap() - share(FIVE_HOUR, PRO)).abs() < 1e-9
            && (pro.seven_day_share.unwrap() - share(SEVEN_DAY, PRO)).abs() < 1e-9,
        "Pro reads the same tokens over its own capacity on each window: {pro:?}"
    );
    let tierless = point_of("tierless");
    assert_eq!(
        (
            tierless.five_hour_share,
            tierless.seven_day_share,
            tierless.uncached_tokens,
            tierless.usd,
        ),
        (None, None, 1_000_000, 0.0),
        "no tier sizes a window, so the tokens count and no share is priced"
    );
    let key = point_of("key");
    assert_eq!(
        (
            key.five_hour_share,
            key.seven_day_share,
            key.usd,
            key.uncached_tokens
        ),
        (None, None, 5.0, 1_000_000),
        "an api_key row keeps its estimated dollars and reads no window"
    );
}

/// One request in the off-band weekday hour on `account`, its one attempt carrying `input` uncached tokens; `None`
/// books a failed request, whose rollup row carries no token.
fn plant_in_the_off_band_hour(
    db: &LedgerDb,
    id: &str,
    account: &str,
    model: &str,
    input: Option<u64>,
) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: WEEKDAY_OFF_PEAK,
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            canonical_model: model.into(),
            model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: if input.is_some() {
                "succeeded"
            } else {
                "failed"
            }
            .into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: "oauth".into(),
            outcome: if input.is_some() {
                "succeeded"
            } else {
                "upstream_error"
            }
            .into(),
            input_tokens: input,
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// The off-band hour's burn point of `account`, a Max 20x subscription.
fn off_band_point_on_max_20x(db: &LedgerDb, account: &str) -> phoebis_ledger::BurnPoint {
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([(account.to_string(), Some(MAX_20X.to_string()))]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec![account.into()],
        ..SeriesFilter::default()
    };
    db.read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
        .unwrap()
        .into_iter()
        .find(|p| p.at == hour)
        .expect("the planted hour")
}

#[test]
fn a_bucket_mixing_a_priced_and_an_unpriced_row_prices_no_share_on_that_window_and_counts_every_token(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    // The same hour on Max 20x: a million input tokens of a model the table prices and a million Mystery 2 tokens of
    // a tier it does not place, and a partial sum would read as the hour's share. The rollups read in model order, so
    // Mystery 2 comes before Opus 5.5 and after Fable 5.1, and each order is taken once.
    for (account, priced) in [("a", "claude-opus-5-5"), ("b", "claude-fable-5-1")] {
        plant_in_the_off_band_hour(
            &db,
            &format!("{account}-priced"),
            account,
            priced,
            Some(1_000_000),
        );
        plant_in_the_off_band_hour(
            &db,
            &format!("{account}-unpriced"),
            account,
            "claude-mystery-2",
            Some(1_000_000),
        );
        let point = off_band_point_on_max_20x(&db, account);
        assert_eq!(
            (
                point.five_hour_share,
                point.seven_day_share,
                point.tokens,
                point.usd
            ),
            (None, None, 2_000_000, 0.0),
            "{priced} beside Mystery 2"
        );
    }
    // Opus 5 has no weight of its own and prices at Opus 5.5's, so a bucket of the two is priced whole: the share is
    // the sum of two Opus 5.5 rows.
    plant_in_the_off_band_hour(&db, "c-opus-5-5", "c", "claude-opus-5-5", Some(1_000_000));
    plant_in_the_off_band_hour(&db, "c-opus-5", "c", "claude-opus-5", Some(1_000_000));
    let point = off_band_point_on_max_20x(&db, "c");
    let opus = |window| {
        share_of(
            &PlanTables::shipped(),
            window,
            MAX_20X,
            "claude-opus-5-5",
            usage(1_000_000, 0),
            WEEKDAY_OFF_PEAK,
        )
    };
    assert!(
        point
            .five_hour_share
            .is_some_and(|share| (share - 2.0 * opus(FIVE_HOUR)).abs() < 1e-12)
            && point
                .seven_day_share
                .is_some_and(|share| (share - 2.0 * opus(SEVEN_DAY)).abs() < 1e-12),
        "Opus 5.5 beside Opus 5: {point:?}"
    );
    assert_eq!(point.tokens, 2_000_000);
}

#[test]
fn a_row_with_no_token_never_unprices_a_bucket_and_a_bucket_of_them_alone_reads_no_share() {
    let db = LedgerDb::open_in_memory().unwrap();
    // A failed request on Sonnet 4.5, a model the cost model has no weight for, books a row with no token beside a
    // priced hour of Opus 5.5: it has nothing to price, so the hour keeps its share.
    plant_in_the_off_band_hour(&db, "priced", "a", "claude-opus-5-5", Some(1_000_000));
    plant_in_the_off_band_hour(&db, "failed", "a", "claude-sonnet-4-5", None);
    // A failed request alone on a model the table lists, which would price at zero: an idle hour, not a share of zero.
    plant_in_the_off_band_hour(&db, "listed", "b", "claude-opus-5-5", None);
    let point = off_band_point_on_max_20x(&db, "a");
    let opus = share_of(
        &PlanTables::shipped(),
        FIVE_HOUR,
        MAX_20X,
        "claude-opus-5-5",
        usage(1_000_000, 0),
        WEEKDAY_OFF_PEAK,
    );
    assert!(
        point
            .five_hour_share
            .is_some_and(|share| (share - opus).abs() < 1e-9),
        "the Opus 5.5 share stands: {point:?} vs {opus}"
    );
    assert_eq!(point.tokens, 1_000_000);
    let idle = off_band_point_on_max_20x(&db, "b");
    assert_eq!(
        (idle.five_hour_share, idle.seven_day_share, idle.tokens),
        (None, None, 0)
    );
}

#[test]
fn a_row_on_a_model_without_a_weight_prices_a_share_at_its_tiers_latest_weight_and_its_tokens_count(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    // The shipped model has no weight for Opus 5, so a million of its input tokens price on both windows at the
    // weight of the latest weighted model of its tier.
    db.insert_request(
        &RequestRecord {
            id: "r-opus-5".into(),
            received_at: WEEKDAY_OFF_PEAK,
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            canonical_model: "claude-opus-5".into(),
            model: "claude-opus-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r-opus-5".into(),
            attempt: 1,
            account_id: "max".into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            estimated_usd: Some(5.0),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("max".to_string(), Some(MAX_20X.to_string()))]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let point = db
        .read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
        .unwrap()
        .into_iter()
        .find(|p| p.at == hour)
        .expect("the planted hour");
    let costs = tables.revisions.in_force(WEEKDAY_OFF_PEAK);
    let (weight, fallback) = costs
        .weight_or_fallback("claude-opus-5", FIVE_HOUR)
        .expect("Opus 5 prices through its tier");
    assert!(fallback, "the weight is its tier's, not its own");
    let capacity = &costs.capacity[MAX_20X];
    let share = |window_capacity: Option<f64>| {
        1_000_000.0 * weight * costs.type_multiplier.input / window_capacity.unwrap()
    };
    assert!(
        point
            .five_hour_share
            .is_some_and(|got| (got - share(capacity.five_hour)).abs() < 1e-12)
            && point
                .seven_day_share
                .is_some_and(|got| (got - share(capacity.seven_day)).abs() < 1e-12),
        "a known tier prices the model at its tier's weight: {point:?}"
    );
    assert_eq!(
        (point.uncached_tokens, point.usd),
        (1_000_000, 0.0),
        "and the tokens still count"
    );
}

#[test]
fn a_session_attempt_weighs_its_five_hour_share_by_the_band_at_its_completion() {
    let db = LedgerDb::open_in_memory().unwrap();
    let (started, finished) = (WEEKDAY_BAND_OPENS - 60_000, WEEKDAY_BAND_OPENS + 60_000);
    db.insert_request(
        &RequestRecord {
            id: "r-edge".into(),
            received_at: started,
            finished_at: Some(finished),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            session_id: Some("s-edge".into()),
            canonical_model: "claude-sonnet-5-5".into(),
            model: "claude-sonnet-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r-edge".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            started_at: started,
            finished_at: Some(finished),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let tables = PlanTables::shipped();
    let factor = tables.band_factor(FIVE_HOUR, finished);
    assert!(
        tables.band_factor(FIVE_HOUR, started) == 1.0 && factor > 1.0,
        "the attempt starts before the band and completes inside it"
    );
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let filter = SeriesFilter {
        from: WEEKDAY_BAND_OPENS - 3_600_000,
        to: WEEKDAY_BAND_OPENS + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| phoebis_ledger::session_burn_series(c, &tables, &tiers, &filter))
        .unwrap();
    let point = sessions
        .iter()
        .find(|(id, _)| id == "s-edge")
        .and_then(|(_, points)| points.iter().find(|p| p.five_hour_share.is_some()))
        .unwrap_or_else(|| panic!("the session's bucket carries its share: {sessions:?}"));
    let tokens = usage(1_000, 100);
    let base = share_of(
        &tables,
        FIVE_HOUR,
        MAX_20X,
        "claude-sonnet-5-5",
        tokens,
        started,
    );
    assert!(
        (point.five_hour_share.unwrap() - base * factor).abs() < 1e-6,
        "priced at the band its completion fell in, not its start's: {point:?} vs {}",
        base * factor
    );
    let week = share_of(
        &tables,
        SEVEN_DAY,
        MAX_20X,
        "claude-sonnet-5-5",
        tokens,
        finished,
    );
    assert!(
        (point.seven_day_share.unwrap() - week).abs() < 1e-6,
        "the 7-day share is priced too: {point:?} vs {week}"
    );
}

#[test]
fn a_sessions_fast_attempt_reads_its_dollars_at_a_zero_share() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (session, speed) in [("s-standard", None), ("s-fast", Some("fast"))] {
        let id = format!("r-{session}");
        db.insert_request(
            &RequestRecord {
                id: id.clone(),
                received_at: WEEKDAY_OFF_PEAK,
                finished_at: Some(WEEKDAY_OFF_PEAK + 500),
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                session_id: Some(session.into()),
                canonical_model: "claude-opus-5-5".into(),
                model: "claude-opus-5-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            &[AttemptRecord {
                request_id: id,
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                started_at: WEEKDAY_OFF_PEAK,
                finished_at: Some(WEEKDAY_OFF_PEAK + 500),
                outcome: "succeeded".into(),
                input_tokens: Some(1_000_000),
                estimated_usd: Some(10.0),
                speed: speed.map(String::from),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    }
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some(MAX_20X.to_string()))]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| phoebis_ledger::session_burn_series(c, &tables, &tiers, &filter))
        .unwrap();
    let point = |session: &str| {
        sessions
            .iter()
            .find(|(id, _)| id == session)
            .and_then(|(_, points)| points.iter().find(|p| p.at == hour).cloned())
            .unwrap_or_else(|| panic!("{session} has the planted hour: {sessions:?}"))
    };
    let standard = point("s-standard");
    assert!(
        standard.five_hour_share.is_some_and(|s| s > 0.0) && standard.usd == 0.0,
        "the standard attempt reads a share and no dollars: {standard:?}"
    );
    let fast = point("s-fast");
    assert_eq!(
        (
            fast.usd,
            fast.five_hour_share,
            fast.seven_day_share,
            fast.uncached_tokens
        ),
        (10.0, Some(0.0), Some(0.0), 1_000_000),
        "the attempt that reported speed fast reads its dollars and a zero share"
    );
}

/// A cost model in round numbers for the share cases: one Fable input token weighs 10 units, `test_tier` holds 100
/// units on the 5-hour window and 1000 on the week, `narrow_tier` sizes the 5-hour window alone, and the Fable week
/// is half the 7-day window.
const SHARE_COSTS: &str = "\
revisions:
  - effective: 2026-01-01
    unit:
      defined_by: {tier: test_tier, window: five_hour, band: out_of_band}
      units_per_window: 100
    capacity:
      test_tier:   {five_hour: 100, seven_day: 1000}
      narrow_tier: {five_hour: 100, seven_day: null}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-fable-5-1: 10
      claude-sonnet-5-5: 1
      claude-haiku-4-5: 0.5
    model_windows:
      seven_day_fable: {of: seven_day, capacity_factor: 0.5}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-fable-5-1: fable
      claude-sonnet-5-5: sonnet
      claude-haiku-4-5: haiku
";

fn share_tables() -> PlanTables {
    PlanTables {
        revisions: CostRevisions::from_yaml(SHARE_COSTS).expect("the share cost model parses"),
        path: None,
        source: "test".to_string(),
    }
}

/// One Sonnet request of `session` on the oauth account `a` in the off-band hour, `overage` as its head reported it.
fn plant_funded(db: &LedgerDb, id: &str, session: &str, overage: Option<bool>, usd: f64) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 500),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            session_id: Some(session.into()),
            canonical_model: "claude-sonnet-5-5".into(),
            model: "claude-sonnet-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            started_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 500),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            estimated_usd: Some(usd),
            overage_in_use: overage,
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

#[test]
fn price_shares_answers_the_two_shared_windows_and_the_model_week_the_model_carries() {
    let tables = share_tables();
    let one = usage(1, 0);
    let keys = |shares: &WindowShares| shares.keys().copied().collect::<Vec<_>>();
    let close = |got: Option<f64>, want: f64| got.is_some_and(|g| (g - want).abs() < 1e-12);
    // One Fable input token weighs 10 units off the band: over 100 on the 5-hour window, over 1000 on the week, and
    // over the model week's half of it.
    let fable = price_shares(
        &tables,
        "test_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
    );
    assert_eq!(keys(&fable), [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]);
    assert!(
        close(fable[FIVE_HOUR], 0.1)
            && close(fable[SEVEN_DAY], 0.01)
            && close(fable[SEVEN_DAY_FABLE], 0.02),
        "a Fable model prices every window it carries: {fable:?}"
    );
    let haiku = price_shares(
        &tables,
        "test_tier",
        "claude-haiku-4-5",
        one,
        WEEKDAY_OFF_PEAK,
    );
    assert_eq!(
        keys(&haiku),
        [FIVE_HOUR, SEVEN_DAY],
        "a Haiku model has no week of its own"
    );
    assert!(
        close(haiku[FIVE_HOUR], 0.005) && close(haiku[SEVEN_DAY], 0.0005),
        "{haiku:?}"
    );
    let narrow = price_shares(
        &tables,
        "narrow_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
    );
    assert_eq!(keys(&narrow), [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]);
    assert!(
        close(narrow[FIVE_HOUR], 0.1) && narrow[SEVEN_DAY].is_none() && narrow[SEVEN_DAY_FABLE].is_none(),
        "a tier sized on the 5-hour window alone prices the week and the model week null: {narrow:?}"
    );
    let unweighted = price_shares(
        &tables,
        "test_tier",
        "claude-opus-5-5",
        one,
        WEEKDAY_OFF_PEAK,
    );
    assert_eq!(
        unweighted.into_iter().collect::<Vec<_>>(),
        [(FIVE_HOUR, None), (SEVEN_DAY, None)],
        "a model with no weight keeps its two shared keys, each null"
    );
    let no_tier = price_shares(
        &tables,
        "missing_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
    );
    assert_eq!(
        no_tier.into_iter().collect::<Vec<_>>(),
        [
            (FIVE_HOUR, None),
            (SEVEN_DAY, None),
            (SEVEN_DAY_FABLE, None)
        ],
        "a tier the file lacks keeps every applicable key, each null"
    );
}

#[test]
fn an_overage_row_adds_to_no_share_and_a_plan_row_to_both() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_funded(&db, "r-plan", "s", None, 3.0);
    plant_funded(&db, "r-overage", "s", Some(true), 7.0);
    let tables = share_tables();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some("test_tier".to_string()))]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let mut fundings: Vec<String> = db
        .read_blocking(|c| burn_rows(c, &filter))
        .unwrap()
        .into_iter()
        .map(|row| row.funding)
        .collect();
    fundings.sort();
    assert_eq!(
        fundings,
        [OVERAGE_FUNDING, PLAN_FUNDING],
        "the fold files the two requests under their fundings"
    );
    let point = db
        .read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
        .unwrap()
        .into_iter()
        .find(|p| p.at == hour)
        .expect("the planted hour");
    let plan = usage(1_000, 100);
    let five_hour = share_of(
        &tables,
        FIVE_HOUR,
        "test_tier",
        "claude-sonnet-5-5",
        plan,
        hour,
    );
    let seven_day = share_of(
        &tables,
        SEVEN_DAY,
        "test_tier",
        "claude-sonnet-5-5",
        plan,
        hour,
    );
    assert!(
        (point.usd - 7.0).abs() < 1e-9,
        "the overage row's dollars alone reach the bucket: {point:?}"
    );
    assert!(
        point
            .five_hour_share
            .is_some_and(|s| (s - five_hour).abs() < 1e-9),
        "the plan row alone prices the 5-hour share: {point:?} vs {five_hour}"
    );
    assert!(
        point
            .seven_day_share
            .is_some_and(|s| (s - seven_day).abs() < 1e-9),
        "the plan row alone prices the 7-day share: {point:?} vs {seven_day}"
    );
    assert_eq!(point.uncached_tokens, 2_000, "both rows' tokens count");
}

#[test]
fn a_session_s_overage_attempt_adds_dollars_and_no_share_while_its_plan_attempt_adds_shares() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_funded(&db, "r-plan", "s-mixed", None, 3.0);
    plant_funded(&db, "r-overage", "s-mixed", Some(true), 7.0);
    let tables = share_tables();
    let tiers: Tiers = HashMap::from([("a".to_string(), Some("test_tier".to_string()))]);
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| phoebis_ledger::session_burn_series(c, &tables, &tiers, &filter))
        .unwrap();
    let point = sessions
        .iter()
        .find(|(id, _)| id == "s-mixed")
        .and_then(|(_, points)| points.iter().find(|p| p.at == hour).cloned())
        .unwrap_or_else(|| panic!("the session has the planted hour: {sessions:?}"));
    let plan = usage(1_000, 100);
    let at = WEEKDAY_OFF_PEAK + 500;
    let five_hour = share_of(
        &tables,
        FIVE_HOUR,
        "test_tier",
        "claude-sonnet-5-5",
        plan,
        at,
    );
    let seven_day = share_of(
        &tables,
        SEVEN_DAY,
        "test_tier",
        "claude-sonnet-5-5",
        plan,
        at,
    );
    assert!(
        (point.usd - 7.0).abs() < 1e-9,
        "the overage attempt's dollars alone reach the bucket: {point:?}"
    );
    assert!(
        point
            .five_hour_share
            .is_some_and(|s| (s - five_hour).abs() < 1e-9),
        "the plan attempt alone prices the 5-hour share: {point:?} vs {five_hour}"
    );
    assert!(
        point
            .seven_day_share
            .is_some_and(|s| (s - seven_day).abs() < 1e-9),
        "the plan attempt alone prices the 7-day share: {point:?} vs {seven_day}"
    );
    assert_eq!(point.uncached_tokens, 2_000, "both attempts' tokens count");
}

/// A caller that folds several scopes reads the range once: the series over the rows `burn_rows` read answers,
/// point for point, what `burn_series` answers when it reads the range itself for each scope.
#[test]
fn the_burn_series_over_one_read_matches_a_read_per_scope() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (mut request, mut attempts) in synthetic_day(5) {
        // The day's Claude models all lack a weight in the cost model; its Opus rows move to Opus 5.5, which the
        // model weighs. Account a's other rows move to what the model prices (Sonnet 5.5, and the API key for its
        // GPT rows), so its scope reads priced shares, while c keeps its unweighed Sonnet 5 rows beside Opus 5.5 and
        // its buckets mix priced shares with unpriced rows.
        if request.canonical_model == "claude-opus-5" {
            request.canonical_model = "claude-opus-5-5".into();
        }
        if let Some(last) = attempts.last_mut().filter(|a| a.account_id == "a") {
            match request.canonical_model.as_str() {
                "claude-sonnet-5" => request.canonical_model = "claude-sonnet-5-5".into(),
                "gpt-5" => last.account_id = "b".into(),
                _ => {}
            }
        }
        db.insert_request(&request, &attempts, None).unwrap();
    }
    let tables = PlanTables::shipped();
    let tiers: Tiers = HashMap::from([
        ("a".to_string(), Some(MAX_20X.to_string())),
        ("c".to_string(), Some(PRO.to_string())),
    ]);
    for bucket_seconds in [3_600, 6 * 3_600] {
        // A range that starts mid-bucket, so the read reaches back to the bucket's edge.
        let base = SeriesFilter {
            from: 90 * 60_000,
            to: DAY_MS,
            bucket_seconds,
            ..SeriesFilter::default()
        };
        let rows = db.read_blocking(|c| burn_rows(c, &base)).unwrap();
        let scopes = [
            SeriesFilter::default(),
            SeriesFilter {
                accounts: vec!["a".into()],
                ..SeriesFilter::default()
            },
            SeriesFilter {
                keys: vec!["k2".into()],
                ..SeriesFilter::default()
            },
            SeriesFilter {
                pools: vec!["p1".into()],
                request_classes: vec![NO_CLASS.into()],
                ..SeriesFilter::default()
            },
        ];
        for scope in scopes {
            let filter = SeriesFilter {
                from: base.from,
                to: base.to,
                bucket_seconds,
                ..scope
            };
            let read = db
                .read_blocking(|c| burn_series(c, &tables, &tiers, &filter))
                .unwrap();
            let over = burn_series_over(&rows, &tables, &tiers, &filter).unwrap();
            assert!(
                read.iter()
                    .any(|p| p.five_hour_share.is_some() || p.usd > 0.0),
                "the scope reads something: {filter:?}"
            );
            assert_eq!(over, read, "{filter:?}");
        }
    }
}

#[test]
fn the_rewrite_series_and_the_moves_by_reason_read_the_account_rollups() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_account(
            &tx,
            &phoebis_ledger::AccountRollupRow {
                hour: 3_600,
                account_id: "a".into(),
                rewrite_tokens: 40,
                ..Default::default()
            },
        )?;
        fold_event(
            &tx,
            &EventRecord {
                at: 3_600_500,
                kind: "pin_migrated".into(),
                account_id: Some("a".into()),
                detail: Some(r#"{"reason":"shed"}"#.into()),
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let series = db.read_blocking(|c| rewrite_series(c, &filter)).unwrap();
    assert_eq!(series.iter().map(|(_, n)| n).sum::<u64>(), 40);
    let moves = db.read_blocking(|c| moves_by_reason(c, &filter)).unwrap();
    assert_eq!(moves.get("shed"), Some(&1));
}

#[test]
fn moves_by_reason_invents_no_vocabulary_for_a_reason_no_code_names_or_none_at_all() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_event(
            &tx,
            &EventRecord {
                at: 3_600_000,
                kind: "pin_migrated".into(),
                account_id: Some("a".into()),
                detail: Some(r#"{"reason":"future_reason"}"#.into()),
                ..Default::default()
            },
        )?;
        fold_event(
            &tx,
            &EventRecord {
                at: 3_600_100,
                kind: "pin_migrated".into(),
                account_id: Some("b".into()),
                detail: Some("{}".into()),
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let moves = db.read_blocking(|c| moves_by_reason(c, &filter)).unwrap();
    assert_eq!(
        moves.get("future_reason"),
        Some(&1),
        "a reason no code names reads back as itself"
    );
    assert_eq!(
        moves.get("unknown"),
        Some(&1),
        "no reason at all falls back to unknown"
    );
}

#[test]
fn a_zero_bucket_seconds_answers_the_range_error_rather_than_panicking() {
    let db = LedgerDb::open_in_memory().unwrap();
    // `SeriesFilter::default()` carries `bucket_seconds: 0`; the guard has to run before the division that
    // would otherwise panic on it.
    let filter = SeriesFilter::default();
    assert!(db.read_blocking(|c| moves_by_reason(c, &filter)).is_err());
    assert!(db.read_blocking(|c| rewrite_series(c, &filter)).is_err());
    let tables = PlanTables::shipped();
    assert!(db
        .read_blocking(|c| burn_series(c, &tables, &HashMap::new(), &filter))
        .is_err());
}

/// A UTC instant, for a period bound the test states independently of `member_spends`'s own arithmetic.
fn ms(y: i16, m: i8, d: i8, h: i8, mi: i8) -> i64 {
    jiff::civil::date(y, m, d)
        .at(h, mi, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap()
        .timestamp()
        .as_millisecond()
}

/// One canonical model at a flat rate per token of every kind, so a token count and a dollar figure read the
/// same number and a test needs no real-world price to check the read-time estimate landed, not a stale one.
fn priced_table(model: &str, rate_per_token: f64) -> PriceTable {
    let rate = rate_per_token * 1_000_000.0;
    let row = ModelRow {
        standard: ModelPrice {
            input: rate,
            cache_write_5m: rate,
            cache_write_1h: rate,
            cache_read: rate,
            output: rate,
        },
        fast: None,
        long_context: None,
    };
    PriceTable {
        effective: "2026-01-01".into(),
        models: [(model.to_string(), row)].into_iter().collect(),
    }
}

/// One canonical model with a distinct rate per token kind, so a transposition of the five sums `member_spends`
/// passes to `estimate` (the flat-rate `priced_table` above prices every kind alike and cannot catch one) shows
/// up as a wrong total rather than the same number in the wrong place.
fn priced_table_distinct(model: &str) -> PriceTable {
    let per_m = |rate_per_token: f64| rate_per_token * 1_000_000.0;
    let row = ModelRow {
        standard: ModelPrice {
            input: per_m(1.0),
            cache_read: per_m(2.0),
            cache_write_5m: per_m(3.0),
            cache_write_1h: per_m(4.0),
            output: per_m(5.0),
        },
        fast: None,
        long_context: None,
    };
    PriceTable {
        effective: "2026-01-01".into(),
        models: [(model.to_string(), row)].into_iter().collect(),
    }
}

/// A request and its one attempt, carrying every token kind at a distinct count, for the distinct-rate test.
#[allow(clippy::too_many_arguments)]
fn plant_spend_every_kind(
    db: &LedgerDb,
    id: &str,
    pool: &str,
    account: &str,
    model: &str,
    received_at: i64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        key_id: Some("desktop".into()),
        pool_id: Some(pool.into()),
        canonical_model: model.into(),
        model: model.into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: account.into(),
        account_kind: "oauth".into(),
        outcome: "succeeded".into(),
        input_tokens: Some(input_tokens),
        cache_read_tokens: Some(cache_read_tokens),
        cache_write_5m_tokens: Some(cache_write_5m_tokens),
        cache_write_1h_tokens: Some(cache_write_1h_tokens),
        output_tokens: Some(output_tokens),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

/// One budgeted period of `account` on `pool`, summed over every row whatever its funding.
fn budgeted(pool: &str, account: &str, period: BudgetPeriod) -> BudgetedMember {
    BudgetedMember {
        pool_id: pool.into(),
        account_id: account.into(),
        period,
        metered_only: false,
    }
}

#[test]
fn a_members_spend_prices_every_token_kind_at_its_own_rate() {
    let db = LedgerDb::open_in_memory().unwrap();
    let table = priced_table_distinct("claude-sonnet-5");
    let now = ms(2026, 1, 15, 10, 0);
    plant_spend_every_kind(
        &db,
        "every-kind",
        "p",
        "a",
        "claude-sonnet-5",
        ms(2026, 1, 15, 1, 0),
        10,
        20,
        30,
        40,
        50,
    );
    let rows = db
        .read_blocking(|c| member_spends(c, &[budgeted("p", "a", BudgetPeriod::Day)], now, &table))
        .unwrap();
    assert_eq!(
        rows[0].spent_usd,
        10.0 * 1.0 + 20.0 * 2.0 + 30.0 * 3.0 + 40.0 * 4.0 + 50.0 * 5.0,
        "each token kind is priced at its own rate; a transposition of the five sums would answer a different total"
    );
}

#[test]
fn a_members_spend_prices_a_fast_row_at_the_fast_cells() {
    let db = LedgerDb::open_in_memory().unwrap();
    let now = ms(2026, 1, 15, 10, 0);
    for (id, speed) in [("standard", None), ("fast", Some("fast"))] {
        let request = RequestRecord {
            id: id.into(),
            received_at: ms(2026, 1, 15, 1, 0),
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            canonical_model: "claude-opus-5".into(),
            model: "claude-opus-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "api_key".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            speed: speed.map(String::from),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    }
    let rows = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[budgeted("p", "a", BudgetPeriod::Day)],
                now,
                PriceRevisions::shipped().in_force(now),
            )
        })
        .unwrap();
    assert_eq!(
        rows[0].spent_usd,
        5.0 + 10.0,
        "a million Opus 5 input tokens at the $5 standard cell and a million at the $10 fast cell"
    );
}

/// A request and its one attempt, for a pool member's spend: `estimated_usd` is planted at an obviously wrong
/// figure, so a test that reads the priced sum back at the seeded token count (not the planted figure) proves
/// `member_spends` recomputes at read time for a model priced one way rather than trusting what the attempt was
/// booked at.
#[allow(clippy::too_many_arguments)]
fn plant_spend(
    db: &LedgerDb,
    id: &str,
    pool: &str,
    account: &str,
    model: &str,
    kind: &str,
    received_at: i64,
    input_tokens: u64,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        key_id: Some("desktop".into()),
        pool_id: Some(pool.into()),
        canonical_model: model.into(),
        model: model.into(),
        kind: kind.into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: account.into(),
        account_kind: "oauth".into(),
        outcome: "succeeded".into(),
        input_tokens: Some(input_tokens),
        estimated_usd: Some(999.0),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

#[test]
fn a_members_spend_is_the_priced_rollups_of_its_utc_period_alone() {
    let db = LedgerDb::open_in_memory().unwrap();
    let table = priced_table("claude-sonnet-5", 1.0);

    // The day bound: a row from the day before is left out; a probe inside the day counts like any other write.
    plant_spend(
        &db,
        "d0",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        ms(2026, 1, 14, 23, 0),
        100,
    );
    plant_spend(
        &db,
        "d1",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        ms(2026, 1, 15, 9, 0),
        50,
    );
    plant_spend(
        &db,
        "d2",
        "p",
        "a",
        "claude-sonnet-5",
        "probe",
        ms(2026, 1, 15, 9, 30),
        200,
    );
    let day_now = ms(2026, 1, 15, 10, 0);
    let day = db
        .read_blocking(|c| {
            member_spends(c, &[budgeted("p", "a", BudgetPeriod::Day)], day_now, &table)
        })
        .unwrap();
    assert_eq!(
        (day[0].spent_usd, day[0].period_start, day[0].period_end),
        (250.0, ms(2026, 1, 15, 0, 0), ms(2026, 1, 16, 0, 0)),
        "the row before the day is left out; the probe counts like any other write; the price is the \
         table's, not the row's planted estimated_usd of 999"
    );

    // The month bound over the same rows: every request of January counts, the probe included.
    let month_now = ms(2026, 1, 20, 0, 0);
    let month = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[budgeted("p", "a", BudgetPeriod::Month)],
                month_now,
                &table,
            )
        })
        .unwrap();
    assert_eq!(
        (
            month[0].spent_usd,
            month[0].period_start,
            month[0].period_end
        ),
        (350.0, ms(2026, 1, 1, 0, 0), ms(2026, 2, 1, 0, 0)),
        "the month sums every day inside it, the probe included"
    );

    // A month that turns over at a UTC midnight: a row from the last day of January is out of February's month,
    // a row minutes into February is in it.
    plant_spend(
        &db,
        "t0",
        "p",
        "b",
        "claude-sonnet-5",
        "inference",
        ms(2026, 1, 31, 22, 0),
        999,
    );
    plant_spend(
        &db,
        "t1",
        "p",
        "b",
        "claude-sonnet-5",
        "inference",
        ms(2026, 2, 1, 0, 5),
        7,
    );
    let turnover_now = ms(2026, 2, 1, 0, 10);
    let turnover = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[budgeted("p", "b", BudgetPeriod::Month)],
                turnover_now,
                &table,
            )
        })
        .unwrap();
    assert_eq!(
        (
            turnover[0].spent_usd,
            turnover[0].period_start,
            turnover[0].period_end
        ),
        (7.0, ms(2026, 2, 1, 0, 0), ms(2026, 3, 1, 0, 0)),
        "January's row is the previous month's; only February's counts, from the turnover on"
    );
}

#[test]
fn an_unpriced_model_contributes_nothing_to_a_members_spend() {
    let db = LedgerDb::open_in_memory().unwrap();
    let table = priced_table("claude-sonnet-5", 1.0);
    let now = ms(2026, 1, 15, 10, 0);
    plant_spend(
        &db,
        "priced",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        ms(2026, 1, 15, 1, 0),
        40,
    );
    plant_spend(
        &db,
        "unpriced",
        "p",
        "a",
        "an-unlisted-model",
        "inference",
        ms(2026, 1, 15, 2, 0),
        1_000,
    );
    let rows = db
        .read_blocking(|c| member_spends(c, &[budgeted("p", "a", BudgetPeriod::Day)], now, &table))
        .unwrap();
    assert_eq!(
        rows[0].spent_usd, 40.0,
        "a model the table cannot price skips its row; the sum is the priced model's alone"
    );
}

#[test]
fn a_purge_of_the_periods_raw_requests_leaves_the_members_spend_in_force() {
    let db = LedgerDb::open_in_memory().unwrap();
    let table = priced_table("claude-sonnet-5", 1.0);
    let now = ms(2026, 1, 20, 12, 0);
    plant_spend(
        &db,
        "r1",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        ms(2026, 1, 2, 0, 0),
        50,
    );
    let before = db
        .read_blocking(|c| {
            member_spends(c, &[budgeted("p", "a", BudgetPeriod::Month)], now, &table)
        })
        .unwrap();
    assert_eq!(before[0].spent_usd, 50.0);

    // A retention window shorter than the month purges the raw request and its attempt; the rollup that
    // folded them keeps its default 400-day life, so the query still answers the same spend.
    let purged = run_retention_blocking(
        &db,
        &LedgerConfig {
            retention_days: 5,
            ..LedgerConfig::default()
        },
        now,
        &[],
    )
    .unwrap();
    assert_eq!((purged.requests, purged.attempts), (1, 1));
    let after = db
        .read_blocking(|c| {
            member_spends(c, &[budgeted("p", "a", BudgetPeriod::Month)], now, &table)
        })
        .unwrap();
    assert_eq!(
        after[0].spent_usd, 50.0,
        "the hourly rollup outlives the purged raw rows"
    );

    // A router seeded with this row keeps the member excluded: its spend is still at or above the budget the
    // operator configured, under the period the row was summed over.
    let mut state = RoutingState::from_load(RoutingLoad::default());
    let snapshot = RegistrySnapshot::from_load(
        RegistryLoad {
            members: vec![PoolMember {
                pool_id: "p".into(),
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(10.0),
            }],
            ..Default::default()
        },
        [0u8; 32],
        now,
        1,
    );
    state.record_member_spend(&snapshot, after);
    let spend = state
        .member_spends_of("p", "a")
        .next()
        .expect("the row still matches the member's configured period");
    assert!(
        spend.spent_usd >= 10.0,
        "the member stays excluded: its spend is still at or above the budget"
    );
}

#[test]
fn the_rollup_retention_keeps_the_rows_of_a_budgeted_members_active_period() {
    let db = LedgerDb::open_in_memory().unwrap();
    let table = priced_table("claude-sonnet-5", 1.0);
    let now = ms(2026, 1, 20, 12, 0);
    let ten_days_ago = ms(2026, 1, 10, 12, 0);
    // The same age on three accounts: `a` budgets by the month, `b` carries no budget, `c` budgets by the day.
    for (id, account) in [("monthly", "a"), ("unbudgeted", "b"), ("daily", "c")] {
        plant_spend(
            &db,
            id,
            "p",
            account,
            "claude-sonnet-5",
            "inference",
            ten_days_ago,
            50,
        );
    }
    let budgets = vec![
        budgeted("p", "a", BudgetPeriod::Month),
        budgeted("p", "c", BudgetPeriod::Day),
    ];
    run_retention_blocking(
        &db,
        &LedgerConfig {
            retention_days: 5,
            rollup_retention_days: 5,
            ..LedgerConfig::default()
        },
        now,
        &budgets,
    )
    .unwrap();
    let spent = |account: &str, period: BudgetPeriod| {
        db.read_blocking(|c| member_spends(c, &[budgeted("p", account, period)], now, &table))
            .unwrap()[0]
            .spent_usd
    };
    assert_eq!(
        spent("a", BudgetPeriod::Month),
        50.0,
        "the monthly member's row of this month outlives the five-day retention"
    );
    assert_eq!(
        spent("b", BudgetPeriod::Month),
        0.0,
        "an unbudgeted account's row of the same age goes"
    );
    assert_eq!(
        spent("c", BudgetPeriod::Month),
        0.0,
        "a daily member's row from before its day goes"
    );
    let accounts = db
        .read_blocking(|c| read_account_rollups(c, 0, i64::MAX))
        .unwrap();
    assert!(
        accounts.is_empty(),
        "the account rollups keep the retention as configured: {accounts:?}"
    );
}

fn plant_placement(
    db: &LedgerDb,
    id: &str,
    kind: &str,
    received_at: i64,
    rung: &str,
    prefix_tokens: u64,
    cache_read_tokens: u64,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: kind.into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        outcome: "succeeded".into(),
        cache_read_tokens: Some(cache_read_tokens),
        selection: Some(format!(
            r#"{{"placement":{{"rung":"{rung}","depth":1,"prefix_tokens":{prefix_tokens}}}}}"#
        )),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

#[test]
fn placements_by_rung_count_hits_from_the_trace_and_the_response() {
    let db = LedgerDb::open_in_memory().unwrap();
    // content: a hit reads at least the promised prefix.
    plant_placement(&db, "c1", "inference", 1_000, "content", 500, 500);
    plant_placement(&db, "c2", "inference", 2_000, "content", 500, 500);
    plant_placement(&db, "c3", "inference", 3_000, "content", 500, 100);
    // static: a hit reads anything at all.
    plant_placement(&db, "s1", "inference", 4_000, "static", 200, 50);
    plant_placement(&db, "s2", "inference", 5_000, "static", 200, 0);
    // Left out: a probe's attempt, and one past the range.
    plant_placement(&db, "pr", "probe", 6_000, "content", 500, 500);
    plant_placement(&db, "out", "inference", 100_000, "content", 500, 500);

    let rows = db
        .read_blocking(|c| placements(c, &RollupFilter::default(), &LiveKeys::default(), 0, 10_000))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.rung.as_str(),
                r.placements,
                r.hits,
                r.cache_read_tokens,
                r.promised_tokens
            ))
            .collect::<Vec<_>>(),
        vec![("content", 3, 2, 1_100, 1_500), ("static", 2, 1, 50, 400)]
    );
}

/// A leg with two passes on one trace: the first carries no usage (a retry in place, a strip retry, or a 401
/// replay before the serving attempt), the second is the one that answered.
fn plant_retried_placement(
    db: &LedgerDb,
    id: &str,
    received_at: i64,
    rung: &str,
    prefix_tokens: u64,
    cache_read_tokens: u64,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 2,
        ..Default::default()
    };
    let trace =
        format!(r#"{{"placement":{{"rung":"{rung}","depth":1,"prefix_tokens":{prefix_tokens}}}}}"#);
    let attempts = vec![
        AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            outcome: "rate_limited".into(),
            selection: Some(trace.clone()),
            ..Default::default()
        },
        AttemptRecord {
            request_id: id.into(),
            attempt: 2,
            account_id: "b".into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            cache_read_tokens: Some(cache_read_tokens),
            selection: Some(trace),
            ..Default::default()
        },
    ];
    db.insert_request(&request, &attempts, None).unwrap();
}

#[test]
fn placements_count_the_serving_attempt_alone_not_every_retried_pass() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_retried_placement(&db, "r1", 1_000, "content", 500, 500);
    let rows = db
        .read_blocking(|c| placements(c, &RollupFilter::default(), &LiveKeys::default(), 0, 10_000))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.rung.as_str(), r.placements, r.hits))
            .collect::<Vec<_>>(),
        vec![("content", 1, 1)],
        "the first pass carried no usage; only the serving attempt counts, as one placement and one hit"
    );
}

#[allow(clippy::too_many_arguments)]
fn plant_placement_full(
    db: &LedgerDb,
    id: &str,
    pool: &str,
    key: &str,
    account: &str,
    model: &str,
    client: Option<&str>,
    received_at: i64,
    rung: &str,
    prefix_tokens: u64,
    cache_read_tokens: u64,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        key_id: Some(key.into()),
        pool_id: Some(pool.into()),
        canonical_model: model.into(),
        model: model.into(),
        client: client.map(String::from),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: account.into(),
        account_kind: "oauth".into(),
        outcome: "succeeded".into(),
        cache_read_tokens: Some(cache_read_tokens),
        selection: Some(format!(
            r#"{{"placement":{{"rung":"{rung}","depth":1,"prefix_tokens":{prefix_tokens}}}}}"#
        )),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

#[test]
fn placements_apply_the_model_and_client_filter() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_placement_full(
        &db,
        "m1",
        "main",
        "desktop",
        "a",
        "claude-sonnet-5",
        Some("claude-code/cli"),
        1_000,
        "content",
        500,
        500,
    );
    plant_placement_full(
        &db,
        "m2",
        "main",
        "desktop",
        "a",
        "claude-opus-5",
        Some("sdk/python"),
        2_000,
        "content",
        500,
        500,
    );

    let by_model = db
        .read_blocking(|c| {
            placements(
                c,
                &RollupFilter {
                    models: vec!["claude-sonnet-5".into()],
                    ..Default::default()
                },
                &LiveKeys::default(),
                0,
                10_000,
            )
        })
        .unwrap();
    assert_eq!(
        by_model.iter().map(|r| r.placements).sum::<u64>(),
        1,
        "a model filter narrows placements to that model's own attempts"
    );

    let by_client = db
        .read_blocking(|c| {
            placements(
                c,
                &RollupFilter {
                    clients: vec!["sdk/*".into()],
                    ..Default::default()
                },
                &LiveKeys::default(),
                0,
                10_000,
            )
        })
        .unwrap();
    assert_eq!(
        by_client.iter().map(|r| r.placements).sum::<u64>(),
        1,
        "a client filter narrows placements to that client's own attempts"
    );
}

#[test]
fn placements_apply_the_keys_and_pools_filter() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_placement_full(
        &db,
        "kp1",
        "p1",
        "desktop",
        "a",
        "claude-sonnet-5",
        None,
        1_000,
        "content",
        500,
        500,
    );
    plant_placement_full(
        &db,
        "kp2",
        "p2",
        "mobile",
        "a",
        "claude-sonnet-5",
        None,
        2_000,
        "content",
        500,
        500,
    );

    let by_pool = db
        .read_blocking(|c| {
            placements(
                c,
                &RollupFilter {
                    pools: vec!["p1".into()],
                    ..Default::default()
                },
                &LiveKeys::default(),
                0,
                10_000,
            )
        })
        .unwrap();
    assert_eq!(
        by_pool.iter().map(|r| r.placements).sum::<u64>(),
        1,
        "a pool filter narrows placements to that pool's own attempts"
    );

    let live = LiveKeys([("desktop".to_string(), 0)].into_iter().collect());
    let by_key = db
        .read_blocking(|c| {
            placements(
                c,
                &RollupFilter {
                    keys: vec!["desktop".into()],
                    ..Default::default()
                },
                &live,
                0,
                10_000,
            )
        })
        .unwrap();
    assert_eq!(
        by_key.iter().map(|r| r.placements).sum::<u64>(),
        1,
        "a key filter narrows placements to that key's own attempts"
    );
}

#[test]
fn placements_score_a_with_main_row_like_static() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_placement(&db, "wm1", "inference", 1_000, "with_main", 0, 50);
    plant_placement(&db, "wm2", "inference", 2_000, "with_main", 0, 0);
    let rows = db
        .read_blocking(|c| placements(c, &RollupFilter::default(), &LiveKeys::default(), 0, 10_000))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.rung.as_str(), r.placements, r.hits))
            .collect::<Vec<_>>(),
        vec![("with_main", 2, 1)],
        "with_main scores like static: a hit is any read at all, since it names no entry of its own"
    );
}

#[allow(clippy::too_many_arguments)]
fn plant_identity(
    db: &LedgerDb,
    id: &str,
    key: &str,
    generation: i64,
    session: &str,
    kind: &str,
    received_at: i64,
    forked_from: Option<&str>,
    fork_depth: Option<u32>,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        key_id: Some(key.into()),
        key_generation: generation,
        pool_id: Some("main".into()),
        session_id: Some(session.into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: kind.into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 0,
        forked_from: forked_from.map(String::from),
        fork_depth,
        ..Default::default()
    };
    db.insert_request(&request, &[], None).unwrap();
}

#[test]
fn session_identities_answer_a_session_whose_requests_are_older_than_the_activity_cutoff() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Old enough that `session_activity`'s own `since` bound would drop it; `session_identities` reads no
    // such bound, since a pin-only row (no request of its own) still needs the identity of what it holds.
    plant_identity(
        &db,
        "r1",
        "k1",
        0,
        "s1",
        "inference",
        0,
        None,
        None,
    );
    assert!(
        db.read_blocking(|c| phoebis_ledger::session_activity(c, 500))
            .unwrap()
            .is_empty(),
        "the same session is outside session_activity's own since bound"
    );
    let rows = db
        .read_blocking(|c| session_identities(c, &[("k1".into(), 0, "s1".into())]))
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn a_forked_sessions_identity_names_its_base_from_its_first_request() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_identity(
        &db,
        "r1",
        "k1",
        0,
        "s2",
        "inference",
        1_000,
        Some("s1"),
        Some(3),
    );
    plant_identity(
        &db,
        "r2",
        "k1",
        0,
        "s2",
        "inference",
        2_000,
        None,
        None,
    );
    let rows = db
        .read_blocking(|c| session_identities(c, &[("k1".into(), 0, "s2".into())]))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].forked_from.as_deref(), rows[0].fork_depth),
        (Some("s1"), Some(3)),
        "the fork facts come from the session's first request, kept even though the later one carries none"
    );
}

#[test]
fn a_forked_sessions_identity_skips_an_auxiliary_request_ahead_of_its_first_main_request() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str,
                 session: &str,
                 received_at: i64,
                 class: &str,
                 forked_from: Option<&str>,
                 fork_depth: Option<u32>| {
        let request = RequestRecord {
            id: id.into(),
            received_at,
            finished_at: Some(received_at + 500),
            key_id: Some("k1".into()),
            key_generation: 0,
            pool_id: Some("main".into()),
            session_id: Some(session.into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 0,
            forked_from: forked_from.map(String::from),
            fork_depth,
            request_class: Some(class.into()),
            ..Default::default()
        };
        db.insert_request(&request, &[], None).unwrap();
    };
    plant("r0", "s2", 500, "auxiliary", None, None);
    plant("r1", "s2", 1_000, "main", Some("s1"), Some(3));
    plant("r2", "s2", 2_000, "main", None, None);
    let rows = db
        .read_blocking(|c| session_identities(c, &[("k1".into(), 0, "s2".into())]))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].forked_from.as_deref(), rows[0].fork_depth),
        (Some("s1"), Some(3)),
        "the fork facts come from the first request that is not auxiliary, the probe ahead of it skipped"
    );

    // A marker an auxiliary row recorded before the rule stays in that row and never names the session a fork.
    plant("r3", "s3", 500, "auxiliary", Some("s1"), Some(1));
    plant("r4", "s3", 1_000, "main", None, None);
    let rows = db
        .read_blocking(|c| session_identities(c, &[("k1".into(), 0, "s3".into())]))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].forked_from.as_deref(), rows[0].fork_depth),
        (None, None),
        "an auxiliary row's marker is not the session's"
    );
}

#[test]
fn a_session_the_ledger_holds_no_client_request_for_is_absent_from_the_answer() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_identity(
        &db,
        "r1",
        "k1",
        0,
        "s3",
        "probe",
        1_000,
        None,
        None,
    );
    let rows = db
        .read_blocking(|c| session_identities(c, &[("k1".into(), 0, "s3".into())]))
        .unwrap();
    assert!(
        rows.is_empty(),
        "a session with no client request is not recorded"
    );
}

#[test]
fn session_identities_chunks_a_call_over_many_sessions_and_merges_the_answer() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut named = Vec::new();
    for i in 0..1_000 {
        let session = format!("s{i}");
        plant_identity(
            &db,
            &format!("r{i}"),
            "k1",
            0,
            &session,
            "inference",
            1_000 + i as i64,
            None,
            None,
        );
        named.push(("k1".to_string(), 0_i64, session));
    }
    let rows = db.read_blocking(|c| session_identities(c, &named)).unwrap();
    assert_eq!(
        rows.len(),
        1_000,
        "every session across more than one chunk answers, not only the first chunk's"
    );
}

/// An hour's sums carry no per-attempt prompt, so a member's rows on a model priced by prompt length sum at the
/// dollars they were booked at, while every other group follows the table in force.
#[test]
fn a_members_spend_reads_a_long_context_groups_booked_dollars_and_reprices_the_rest() {
    let db = LedgerDb::open_in_memory().unwrap();
    let now = ms(2026, 1, 15, 10, 0);
    // The booked figure is deliberately not what the table prices, so a re-pricing would show.
    for (id, model, input, booked) in [
        ("long", "claude-haiku-5-5", 1_001, 7.0),
        ("plain", "claude-sonnet-5-5", 1_000_000, 999.0),
    ] {
        let request = RequestRecord {
            id: id.into(),
            received_at: ms(2026, 1, 15, 1, 0),
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            canonical_model: model.into(),
            model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "api_key".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            estimated_usd: Some(booked),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    }
    let spend = |table: &PriceTable| {
        db.read_blocking(|c| member_spends(c, &[budgeted("p", "a", BudgetPeriod::Day)], now, table))
            .unwrap()[0]
            .spent_usd
    };
    // The test table's Sonnet 5.5 input is 3 per million: a million tokens re-price at 3, the Haiku 5.5 row at 7
    // as booked.
    let revisions = phoebis_ledger::test_tables::api_prices();
    let table = revisions.in_force(phoebis_ledger::test_tables::JANUARY_2026);
    assert!(
        (spend(table) - (7.0 + 3.0)).abs() < 1e-9,
        "{}",
        spend(table)
    );
    // A table in force that doubles Sonnet 5.5's input and Haiku 5.5's standard and long-context input moves the
    // plain group and leaves the one priced by prompt length at its booked dollars.
    let mut text = phoebis_ledger::test_tables::API_PRICES.to_string();
    for (cell, doubled) in [
        (
            "claude-sonnet-5-5: {input: 3,",
            "claude-sonnet-5-5: {input: 6,",
        ),
        ("    input: 1\n", "    input: 2\n"),
        ("{over: 1000, input: 5,", "{over: 1000, input: 10,"),
    ] {
        assert_eq!(text.matches(cell).count(), 1, "{cell}");
        text = text.replacen(cell, doubled, 1);
    }
    let revisions = PriceRevisions::from_yaml(&text, "test").unwrap();
    let doubled = revisions.in_force(phoebis_ledger::test_tables::JANUARY_2026);
    assert!(
        (spend(doubled) - (7.0 + 6.0)).abs() < 1e-9,
        "the booked 7 stays beside the plain group's doubled 6: {}",
        spend(doubled)
    );
}
