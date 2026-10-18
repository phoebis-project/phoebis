//! Rollups for a synthetic day equal a direct aggregation of the raw rows for every group-by, the unattributed
//! bucket included, and merged-histogram percentiles lie within one bucket of the exact ones.
use std::collections::BTreeMap;
use std::sync::LazyLock;

use phoebis_ledger::{
    apply_filter, breakdown, breakdown_paged, bucket_index, bucket_seconds, burn_groups_paged,
    burn_rows, burn_series, burn_series_over, deltas_of, fold_account, fold_event, fold_keepalive,
    held_series, hour_ceil, hour_of, key_request_totals, list_requests, member_spends,
    moves_by_reason, placements, price_equivalent, price_shares, read_account_rollups,
    read_rollups, request_totals, rewrite_series, run_retention_blocking, series, series_paged,
    session_identities, AttemptRecord, BudgetedMember, CostRevisions, EventRecord, FundingFilter,
    GroupRow, Histogram, KeepaliveDelta, LedgerConfig, LedgerDb, LiveKeys, ModelPrice, ModelRow,
    PlanTables, PriceRevisions, PriceTable, RequestFilter, RequestRecord, RollupFilter, RollupRow,
    RollupSelect, SeriesFilter, SharePricing, Speed, WindowShares, BILLED_FUNDING, FAST_FUNDING,
    NO_CLASS, OVERAGE_FUNDING, PAGE_HOURS, PLAN_FUNDING, UNATTRIBUTED,
};
use phoebis_routing::{
    PinUsage, RegistrySnapshot, RoutingState, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{BudgetPeriod, PoolMember, RegistryLoad, RoutingLoad};
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

/// The lists the readers price at, the ones `insert_request` folds with: a reader prices a row's counts at read
/// time, so a case whose figure comes from the shipped revision reads the same list its rows were folded under.
static PRICES: LazyLock<PriceRevisions> = LazyLock::new(PriceRevisions::shipped);
static COSTS: LazyLock<PlanTables> = LazyLock::new(PlanTables::shipped);

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
        .share_at(window, None, tier, model, usage, at)
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

/// What a reader prices one attempt at: its counts at the shipped revision in force at `at`.
fn shipped_usd(at: i64, model: &str, speed: Speed, counts: PinUsage) -> f64 {
    PriceRevisions::shipped()
        .in_force(at)
        .estimate(
            model,
            speed,
            counts.input,
            counts.cache_read,
            counts.write_5m,
            counts.write_1h,
            counts.output,
        )
        .expect("the shipped list carries the model")
        .0
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

/// A direct aggregation of the raw rows under one group-by, built from the same delta rule but summed in memory, the
/// bands read at the shipped list `insert_request` folds with.
fn aggregate(
    rows: &[(RequestRecord, Vec<AttemptRecord>)],
    group: impl Fn(&RollupRow) -> String,
) -> BTreeMap<String, RollupRow> {
    let mut out: BTreeMap<String, RollupRow> = BTreeMap::new();
    for (request, attempts) in rows {
        for delta in deltas_of(request, attempts, &PRICES) {
            let key = group(&delta);
            out.entry(key.clone())
                .and_modify(|row| row.add(&delta))
                .or_insert_with(|| RollupRow {
                    hour: 0,
                    key_id: key.clone(),
                    ..delta.clone()
                });
        }
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
        &PRICES,
        &COSTS,
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
        &PRICES,
        &COSTS,
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
        &PRICES,
        &COSTS,
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
        .read_blocking(|c| key_request_totals(c, i64::MIN, i64::MAX, &live, &PRICES, &COSTS))
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
                &PRICES,
                &COSTS,
            )
            .unwrap();
            let paged = series_paged(
                &db, &filter, &live, from, to, bucket, group_by, None, &PRICES, &COSTS,
            )
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
                &PRICES,
                &COSTS,
            );
            let paged = breakdown_paged(
                &db, &filter, &live, from, to, group_by, None, &PRICES, &COSTS,
            )
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
    let default_groups = breakdown(&filtered, "key", false, &live, &[], None, &PRICES, &COSTS);
    assert!(
        !default_groups.iter().any(|g| g.key == "k1"),
        "a group whose rows are all probes stays out of the default breakdown: {default_groups:?}"
    );
    let with_probes = breakdown(&filtered, "key", true, &live, &[], None, &PRICES, &COSTS);
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
                kept_warm_5m_tokens: 2_000,
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
    let default_groups = breakdown(
        &filtered,
        "user_agent",
        false,
        &live,
        &[],
        None,
        &PRICES,
        &COSTS,
    );
    let group = default_groups
        .iter()
        .find(|g| g.key == UNATTRIBUTED)
        .expect("a group of keepalive credits alone stays in the default breakdown");
    assert_eq!(group.kept_warm.requests, 1);
    // The group a route answers carries the saving's three members beside its probe cost.
    let answered = serde_json::to_value(group).unwrap();
    assert!(
        ["saving_equivalent", "saving_shares", "saving_usd"]
            .iter()
            .all(|member| answered.get(member).is_some()),
        "{answered}"
    );
}

/// A credit's saving prices at read time from its two counts, at the revision in force at the row's hour: each count
/// written at its own TTL's rate less the read the two paid instead. A credit on a model the revision lacks adds
/// nothing.
#[test]
fn the_saving_prices_each_kept_warm_count_at_its_ttls_write_rate_less_the_read() {
    const LISTED: &str = "claude-saving-1";
    let per_m = |rate_per_token: f64| rate_per_token * 1_000_000.0;
    let prices = one_entry(
        LISTED,
        ModelRow {
            standard: ModelPrice {
                input: per_m(1.0),
                cache_read: per_m(0.5),
                cache_write_5m: per_m(3.0),
                cache_write_1h: per_m(5.0),
                output: per_m(7.0),
            },
            fast: None,
            long_context: None,
        },
    );
    let at = ms(2026, 1, 15, 10, 0);
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        for (model, five_minutes, hour) in [
            (LISTED, 2_000, 0),
            (LISTED, 0, 300),
            ("claude-unlisted-1", 1_000, 1_000),
        ] {
            fold_keepalive(
                &tx,
                &KeepaliveDelta {
                    at,
                    key_id: "desktop".into(),
                    pool_id: "main".into(),
                    account_id: "a".into(),
                    canonical_model: model.into(),
                    kind: "inference".into(),
                    kept_warm_requests: 1,
                    kept_warm_5m_tokens: five_minutes,
                    kept_warm_1h_tokens: hour,
                    ..Default::default()
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let saving = |group_by: &str, key: &str| {
        breakdown(
            &rows,
            group_by,
            false,
            &LiveKeys::default(),
            &[],
            None,
            &prices,
            &COSTS,
        )
        .iter()
        .find(|g| g.key == key)
        .unwrap_or_else(|| panic!("a {key} group"))
        .saving_usd
    };
    let listed = 2_000.0 * 3.0 + 300.0 * 5.0 - 2_300.0 * 0.5;
    assert!(
        (saving("model", LISTED) - listed).abs() < 1e-9,
        "the five-minute write and the hour's write less the read of both: {}",
        saving("model", LISTED)
    );
    assert_eq!(
        saving("model", "claude-unlisted-1"),
        0.0,
        "a model the revision lacks prices no saving"
    );
    assert!(
        (saving("all", "all") - listed).abs() < 1e-9,
        "and adds nothing beside a priced one"
    );
}

/// A credit's row carries no request token, so its saving alone prices it: on a model the cost model leaves unweighted
/// (the test table's Opus 5 beside Opus 5.5 at 2), the saving prices at the model tier's latest weight and marks its
/// group estimated, as a request row on the model does.
#[test]
fn a_credit_whose_saving_prices_at_a_fallback_weight_marks_its_group_estimated() {
    let prices = phoebis_ledger::test_tables::api_prices();
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                at: phoebis_ledger::test_tables::JANUARY_2026,
                key_id: "desktop".into(),
                pool_id: "main".into(),
                account_id: "a".into(),
                canonical_model: "claude-opus-5".into(),
                kind: "inference".into(),
                kept_warm_requests: 1,
                kept_warm_5m_tokens: 2_000,
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
    let groups = breakdown(
        &rows,
        "all",
        false,
        &LiveKeys::default(),
        &[],
        None,
        &prices,
        &costs,
    );
    let group = groups.first().expect("the credit's group");
    assert!(
        group.saving_equivalent.contains_key(FIVE_HOUR),
        "the saving priced on the window: {:?}",
        group.saving_equivalent
    );
    assert!(
        group.estimated,
        "a saving priced at the fallback weight marks the group estimated"
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
                kept_warm_5m_tokens: 2_000,
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
    let groups = breakdown(
        &filtered,
        "request_class",
        false,
        &live,
        &[],
        None,
        &PRICES,
        &COSTS,
    );
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
    let speeds = |r: &(RequestRecord, Vec<AttemptRecord>)| -> Vec<String> {
        deltas_of(&r.0, &r.1, &PRICES)
            .into_iter()
            .map(|d| d.speed)
            .collect()
    };
    assert_eq!(speeds(&fast), ["fast"]);
    assert_eq!(
        speeds(&silent),
        ["standard"],
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
    let groups = breakdown(&filtered, "speed", false, &live, &[], None, &PRICES, &COSTS);
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
/// overage flag and its answer's speed as named, the account tier it recorded, a client where a case needs the row
/// keyed apart from a credit, and the classifier standing where a case marks the client's own classifier call.
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
    tier: Option<&'a str>,
    classifier: Option<&'a str>,
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
            tier: Some("test_tier"),
            classifier: None,
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
            classifier: row.classifier.map(String::from),
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
            overage_in_use: row.overage,
            speed: row.speed.map(String::from),
            // The landing records no tier for an api_key account, whose rows price in dollars alone.
            tier: row
                .tier
                .filter(|_| row.account_kind != "api_key")
                .map(String::from),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
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
        &PRICES,
        &COSTS,
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
    let at = hour_range(WEEKDAY_OFF_PEAK).0;
    price_shares(tables, None, "test_tier", "claude-fable-5-1", usage, at, at).shares
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
    let pricing = SharePricing { tables: &tables };
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
        "the probe row's share apart from the request side, and no waste while no wasted probe is recorded: \
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
            tier: Some("narrow_tier"),
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let pricing = SharePricing { tables: &tables };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let cost = group(&groups, "a")
        .cost_shares
        .as_ref()
        .expect("the narrow tier still prices the 5-hour window");
    let want = price_shares(
        &tables,
        None,
        "narrow_tier",
        "claude-fable-5-1",
        usage(1_000, 100),
        from,
        from,
    )
    .shares[FIVE_HOUR]
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
    let pricing = SharePricing { tables: &tables };
    let (from, _) = hour_range(WEEKDAY_OFF_PEAK);
    let mut rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(rows.len(), 2, "one rollup row per model");
    let fable = fable_shares(&tables, usage(1_000, 100));
    let haiku = price_shares(
        &tables,
        None,
        "test_tier",
        "claude-haiku-4-5",
        usage(1_000, 100),
        from,
        from,
    )
    .shares;
    for order in ["as read", "reversed"] {
        let groups = breakdown(
            &rows,
            "account",
            false,
            &LiveKeys::default(),
            &[],
            Some(pricing),
            &PRICES,
            &COSTS,
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
    let pricing = SharePricing { tables: &tables };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let a = group(&groups, "a");
    assert!(
        a.cost_shares.as_ref().is_some_and(|m| m.is_empty()),
        "the weightless row prices on no window, so the week and the shared windows are absent alike: {a:?}"
    );
    assert!(
        a.cost_equivalent.is_empty() && a.equivalent_split.is_empty(),
        "and the equivalent latches the same windows rather than reading the priced row alone: {a:?}"
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
    let pricing = SharePricing { tables: &tables };
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

/// A settlement carries the wasted probe's own counts, so the wasted part of a window is those counts priced on it:
/// a quarter of the probe's cache read is a quarter of the probe's share.
#[tokio::test]
async fn the_wasted_probe_share_prices_the_settlements_own_counts_on_the_window() {
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
    // The engine writes a settlement's waste as a row of its own, here in the next hour.
    let next_hour = WEEKDAY_OFF_PEAK + 3_600_000;
    let credit = |at: i64| KeepaliveDelta {
        at,
        key_id: "k".into(),
        pool_id: "p".into(),
        account_id: "a".into(),
        canonical_model: "claude-fable-5-1".into(),
        kind: "probe".into(),
        tier: Some("test_tier".into()),
        ..Default::default()
    };
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                wasted_probes: 1,
                wasted_cache_read_tokens: probe.cache_read / 4,
                ..credit(next_hour)
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let tables = share_tables();
    let pricing = SharePricing { tables: &tables };
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
        "the wasted part is the settlement's counts priced on the window: {probes:?} vs {cost}"
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
    let pricing = SharePricing { tables: &tables };
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
        k.cost_shares.is_none() && k.cache_shares.is_none() && k.probe_shares.is_none(),
        "an api_key group answers no share: {k:?}"
    );
    assert_eq!(
        (k.plan_requests, k.plan_output_tokens),
        (Some(0), Some(0)),
        "and counts no plan-funded request, the counts answering under every group: {k:?}"
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
    let pricing = SharePricing { tables: &tables };
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
        &PRICES,
        &COSTS,
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

/// A row files under the funding its serving attempt had and keeps the account tier it recorded, so an id that comes
/// back as another kind of account moves none of it: an api_key account's row stays billed and priced in dollars once
/// its id serves as an oauth account with a tier, whose own row prices its share alone.
#[tokio::test]
async fn an_api_key_account_s_rows_stay_billed_and_tierless_when_its_id_comes_back_as_an_oauth_account(
) {
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
    plant_share(
        &db,
        ShareRow {
            id: "r-oauth",
            account: "k",
            ..ShareRow::default()
        },
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let tables = share_tables();
    let pricing = SharePricing { tables: &tables };
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
    let counts = |groups: &[GroupRow]| {
        groups
            .iter()
            .map(|g| (g.key.clone(), g.requests))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        (counts(&billed), counts(&plan)),
        (vec![("k".to_string(), 1)], vec![("k".to_string(), 1)]),
        "each request stays on the side its own answer was funded from"
    );
    assert!(
        group(&billed, "k").cost_shares.is_none(),
        "the api_key row recorded no tier and prices no share: {billed:?}"
    );
    let own = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    assert!(
        close(
            group(&plan, "k")
                .cost_shares
                .as_ref()
                .and_then(|m| m.get(FIVE_HOUR)),
            own
        ),
        "the oauth row prices its own share and nothing of the api_key row's: {plan:?} vs {own}"
    );
}

/// The mirror: an oauth account's plan row stays on the plan side, priced under its own tier, once its id serves as
/// an api_key account.
#[tokio::test]
async fn an_oauth_account_s_plan_row_stays_plan_under_its_tier_when_its_id_comes_back_as_an_api_key_account(
) {
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
            id: "r-api",
            account_kind: "api_key",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let pricing = SharePricing { tables: &tables };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let read = |funding: FundingFilter| {
        let db = db.clone();
        async move {
            let filter = RollupFilter {
                funding: Some(funding),
                ..RollupFilter::default()
            };
            account_groups(&db, &filter, from, to, Some(pricing)).await
        }
    };
    let plan = read(FundingFilter::Plan).await;
    let own = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    let a = group(&plan, "a");
    assert!(
        a.requests == 1 && close(a.cost_shares.as_ref().and_then(|m| m.get(FIVE_HOUR)), own),
        "the plan row stays on the plan side at its own share: {a:?} vs {own}"
    );
    let billed = read(FundingFilter::Billed).await;
    let a = group(&billed, "a");
    assert!(
        a.requests == 1 && a.cost_shares.is_none(),
        "and the api_key row beside it bills its dollars with no share: {a:?}"
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
                probe_migrated: 1,
                ..delta()
            },
        )?;
        // A settlement carries no funding of its own and files under the plan.
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                wasted_probes: 1,
                wasted_cache_read_tokens: 500,
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
            funding_of(|r| r.probe_migrated > 0),
            funding_of(|r| r.wasted_probes > 0)
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
    let probe_usd = shipped_usd(
        WEEKDAY_OFF_PEAK,
        "claude-fable-5-1",
        Speed::Standard,
        PinUsage {
            cache_read: 2_000,
            ..PinUsage::default()
        },
    );
    assert!(
        probe_usd > 0.0
            && (billed.0 - probe_usd).abs() < 1e-12
            && (billed.1 - probe_usd).abs() < 1e-12
            && billed.2 == 0.0,
        "billed keeps the probe's request dollars as its cost, and no waste: {billed:?}"
    );
    let wasted_usd = shipped_usd(
        WEEKDAY_OFF_PEAK,
        "claude-fable-5-1",
        Speed::Standard,
        PinUsage {
            cache_read: 500,
            ..PinUsage::default()
        },
    );
    let plan = read(FundingFilter::Plan).await;
    assert!(
        wasted_usd > 0.0 && plan.0 == 0.0 && plan.1 == 0.0 && (plan.2 - wasted_usd).abs() < 1e-12,
        "plan keeps the settlement's waste alone, its counts priced when read: {plan:?}"
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
                probe_migrated: 1,
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

/// The share reads the tier a row recorded and nothing the registry holds now: a group whose rows recorded none (a
/// profile not yet read, or a row folded before the tier was) answers no share and still counts its plan rows, while
/// an account the registry no longer holds prices its rows under the tier they recorded.
#[tokio::test]
async fn a_group_whose_rows_recorded_no_tier_answers_no_share_and_a_dropped_account_prices_under_its_rows_tier(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-tierless",
            account: "t",
            tier: None,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-dropped",
            account: "u",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let pricing = SharePricing { tables: &tables };
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, Some(pricing)).await;
    let t = group(&groups, "t");
    assert!(
        t.cost_shares.is_none() && t.cache_shares.is_none() && t.probe_shares.is_none(),
        "the tierless group answers the three shares null: {t:?}"
    );
    assert_eq!(
        (t.plan_requests, t.plan_output_tokens),
        (Some(1), Some(100)),
        "and still counts the plan row its equivalent is summed over: {t:?}"
    );
    let equivalent = equivalent_of("claude-fable-5-1", usage(1_000, 100), WEEKDAY_OFF_PEAK);
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert!(
            close(t.cost_equivalent.get(window), equivalent[window].unwrap()),
            "the equivalent reads no account tier, so the tierless row prices on {window}: {t:?}"
        );
    }
    let u = group(&groups, "u");
    let own = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    assert!(
        close(u.cost_shares.as_ref().and_then(|m| m.get(FIVE_HOUR)), own),
        "the dropped account's row prices under the tier it recorded: {u:?} vs {own}"
    );
}

/// The groups of `[from, to)` under `group_by` and `filter`, the shares priced on `tables` where the read allows.
async fn groups_by(
    db: &std::sync::Arc<LedgerDb>,
    filter: &RollupFilter,
    (from, to): (i64, i64),
    group_by: &str,
    tables: &PlanTables,
) -> Vec<GroupRow> {
    breakdown_paged(
        db,
        filter,
        &LiveKeys::default(),
        from,
        to,
        group_by,
        Some(SharePricing { tables }),
        &PRICES,
        &COSTS,
    )
    .await
    .unwrap()
}

/// The 20x equivalent `usage` of `model` reads in the hour of `at`, from the shipped cost model the readers price at.
fn equivalent_of(model: &str, usage: PinUsage, at: i64) -> WindowShares {
    let hour = hour_of(at) * 1_000;
    price_equivalent(&COSTS, None, model, usage, hour, hour).shares
}

/// The owner's reading: one request of a Pro account sits next to one of a Max 20x account in the one unit, so the
/// equivalent adds across accounts and account tiers under a grouping that spans them, where the shares cannot.
#[tokio::test]
async fn the_equivalent_under_the_model_grouping_adds_two_accounts_of_two_tiers_into_one_figure() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let (pro, max) = (usage(1_000, 100), usage(3_000, 50));
    plant_share(
        &db,
        ShareRow {
            id: "r-pro",
            account: "pro",
            usage: pro,
            tier: Some(PRO),
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-max",
            account: "max",
            usage: max,
            tier: Some(MAX_20X),
            ..ShareRow::default()
        },
    );
    let tables = PlanTables::shipped();
    let groups = groups_by(
        &db,
        &RollupFilter::default(),
        hour_range(WEEKDAY_OFF_PEAK),
        "model",
        &tables,
    )
    .await;
    let fable = group(&groups, "claude-fable-5-1");
    let (pro, max) = (
        equivalent_of("claude-fable-5-1", pro, WEEKDAY_OFF_PEAK),
        equivalent_of("claude-fable-5-1", max, WEEKDAY_OFF_PEAK),
    );
    assert_eq!(
        fable.cost_equivalent.keys().copied().collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE],
        "the three columns, the Fable week among them: {fable:?}"
    );
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        let want = pro[window].unwrap() + max[window].unwrap();
        assert!(
            close(fable.cost_equivalent.get(window), want),
            "the two accounts' rows add on {window}: {fable:?} vs {want}"
        );
    }
    assert!(
        fable.cost_shares.is_none() && fable.cache_shares.is_none() && fable.probe_shares.is_none(),
        "two accounts' own windows do not add, so the group answers no share: {fable:?}"
    );
    assert_eq!(
        (
            fable.plan_requests,
            fable.plan_output_tokens,
            fable.estimated
        ),
        (Some(2), Some(150), false),
        "the counts answer under every grouping, and a weighted model is no estimate: {fable:?}"
    );
}

/// The billed rows' dollars sit apart from the plan rows', so the native figure of a group reads in one request: the
/// two add to `cost_usd`, and the billed rows add nothing to the equivalent.
#[tokio::test]
async fn the_billed_dollars_and_the_plan_rows_dollars_add_to_the_group_s_cost() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let plan = usage(1_000, 100);
    plant_share(
        &db,
        ShareRow {
            id: "r-plan",
            usage: plan,
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
            id: "r-fast",
            usage: usage(500, 50),
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
            usage: usage(4_000, 400),
            ..ShareRow::default()
        },
    );
    let groups = groups_by(
        &db,
        &RollupFilter::default(),
        hour_range(WEEKDAY_OFF_PEAK),
        "all",
        &share_tables(),
    )
    .await;
    let all = group(&groups, "all");
    let plan_usd = shipped_usd(WEEKDAY_OFF_PEAK, "claude-fable-5-1", Speed::Standard, plan);
    assert!(
        all.billed_usd > 0.0 && (all.billed_usd + plan_usd - all.cost_usd).abs() < 1e-12,
        "the billed rows' dollars and the plan row's add to the group's: {all:?} vs {plan_usd}"
    );
    let billed = &all.billed;
    assert!(
        (billed.cache_read + billed.cache_write + billed.input + billed.output - all.billed_usd)
            .abs()
            < 1e-12,
        "the billed split sums to the billed dollars: {all:?}"
    );
    let want = equivalent_of("claude-fable-5-1", plan, WEEKDAY_OFF_PEAK)[SEVEN_DAY].unwrap();
    assert!(
        close(all.cost_equivalent.get(SEVEN_DAY), want),
        "the plan row alone reads in the equivalent: {all:?} vs {want}"
    );
}

/// A cached plan row's equivalent splits by token kind as the console's breakdown reads it, and its cache halves read
/// under the model grouping as under the account grouping.
#[tokio::test]
async fn the_equivalent_split_s_four_kinds_sum_to_the_seven_day_equivalent_and_the_cache_halves_read_by_model(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let cached = PinUsage {
        input: 500,
        cache_read: 2_000,
        write_5m: 300,
        write_1h: 100,
        output: 50,
    };
    plant_share(
        &db,
        ShareRow {
            id: "r-cached",
            usage: cached,
            ..ShareRow::default()
        },
    );
    let groups = groups_by(
        &db,
        &RollupFilter::default(),
        hour_range(WEEKDAY_OFF_PEAK),
        "model",
        &share_tables(),
    )
    .await;
    let fable = group(&groups, "claude-fable-5-1");
    let seven_day = fable.cost_equivalent[SEVEN_DAY];
    assert!(seven_day > 0.0, "{fable:?}");
    assert_eq!(
        fable.equivalent_split.keys().copied().collect::<Vec<_>>(),
        ["input", "cache_read", "cache_write", "output"],
        "{fable:?}"
    );
    assert!(
        fable.equivalent_split.values().all(|part| *part > 0.0)
            && (fable.equivalent_split.values().sum::<f64>() - seven_day).abs() < 1e-12,
        "the four kinds sum to the 7-day equivalent: {fable:?}"
    );
    let half = |usage: PinUsage, window: &str| {
        equivalent_of("claude-fable-5-1", usage, WEEKDAY_OFF_PEAK)[window].unwrap()
    };
    let read = PinUsage {
        cache_read: 2_000,
        ..PinUsage::default()
    };
    let write = PinUsage {
        write_5m: 300,
        write_1h: 100,
        ..PinUsage::default()
    };
    for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
        assert!(
            fable.cache_equivalent.get(window).is_some_and(|c| {
                (c.read - half(read, window)).abs() < 1e-12
                    && (c.write - half(write, window)).abs() < 1e-12
            }),
            "the cache halves on {window}: {fable:?}"
        );
    }
    assert!(
        (fable.equivalent_split["cache_read"] - half(read, SEVEN_DAY)).abs() < 1e-12,
        "the split's cache read is the cache half of the 7-day window: {fable:?}"
    );
}

/// The probe rows and the wasted probes' own counts read in the equivalent under every grouping, each priced at its
/// own row's hour.
#[tokio::test]
async fn the_probe_equivalent_prices_the_probe_rows_and_the_wasted_counts() {
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
            ..ShareRow::default()
        },
    );
    let next_hour = WEEKDAY_OFF_PEAK + 3_600_000;
    let wasted = PinUsage {
        cache_read: 500,
        ..PinUsage::default()
    };
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                at: next_hour,
                key_id: "k".into(),
                pool_id: "p".into(),
                account_id: "a".into(),
                canonical_model: "claude-fable-5-1".into(),
                kind: "probe".into(),
                wasted_probes: 1,
                wasted_cache_read_tokens: wasted.cache_read,
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let (from, _) = hour_range(WEEKDAY_OFF_PEAK);
    let (_, to) = hour_range(next_hour);
    let groups = groups_by(&db, &with_probes(), (from, to), "model", &share_tables()).await;
    let fable = group(&groups, "claude-fable-5-1");
    let cost = equivalent_of("claude-fable-5-1", probe, WEEKDAY_OFF_PEAK)[FIVE_HOUR].unwrap();
    let waste = equivalent_of("claude-fable-5-1", wasted, next_hour)[FIVE_HOUR].unwrap();
    assert!(
        waste > 0.0
            && fable.probe_equivalent.get(FIVE_HOUR).is_some_and(|p| {
                (p.cost - cost).abs() < 1e-12 && (p.wasted - waste).abs() < 1e-12
            }),
        "the probe row's equivalent and the wasted counts priced at their own hour: {fable:?} vs {cost} and {waste}"
    );
}

/// The own-window shares add within one account, so a read answers them under any grouping once its filter names
/// exactly one account, and under the account grouping, and nowhere else.
#[tokio::test]
async fn the_shares_answer_under_any_grouping_where_the_filter_names_one_account() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-a",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-b",
            account: "b",
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let range = hour_range(WEEKDAY_OFF_PEAK);
    let own = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    let five_hour = |g: &GroupRow| {
        g.cost_shares
            .as_ref()
            .and_then(|m| m.get(FIVE_HOUR))
            .copied()
    };
    let spanning = groups_by(&db, &RollupFilter::default(), range, "model", &tables).await;
    assert_eq!(
        five_hour(group(&spanning, "claude-fable-5-1")),
        None,
        "a model group spanning two accounts answers no share"
    );
    let named = RollupFilter {
        accounts: vec!["a".into()],
        ..RollupFilter::default()
    };
    let one = groups_by(&db, &named, range, "model", &tables).await;
    assert!(
        close(five_hour(group(&one, "claude-fable-5-1")).as_ref(), own),
        "the same group under a filter naming account a reads a's share: {one:?} vs {own}"
    );
    let by_account = groups_by(&db, &RollupFilter::default(), range, "account", &tables).await;
    for key in ["a", "b"] {
        assert!(
            close(five_hour(group(&by_account, key)).as_ref(), own),
            "each account group is one account: {by_account:?}"
        );
    }
}

/// A model the cost model does not weight prices at its model tier's latest weight, and its group says so.
#[tokio::test]
async fn a_group_on_a_model_without_a_weight_reads_its_tiers_equivalent_marked_estimated() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    for (id, model) in [
        ("r-opus-5", "claude-opus-5"),
        ("r-opus-5-5", "claude-opus-5-5"),
    ] {
        plant_share(
            &db,
            ShareRow {
                id,
                model,
                ..ShareRow::default()
            },
        );
    }
    let groups = groups_by(
        &db,
        &RollupFilter::default(),
        hour_range(WEEKDAY_OFF_PEAK),
        "model",
        &share_tables(),
    )
    .await;
    let (unweighted, weighted) = (
        group(&groups, "claude-opus-5"),
        group(&groups, "claude-opus-5-5"),
    );
    assert!(
        unweighted.estimated && !weighted.estimated,
        "{unweighted:?} and {weighted:?}"
    );
    assert!(
        unweighted
            .cost_equivalent
            .get(FIVE_HOUR)
            .is_some_and(|e| *e > 0.0)
            && unweighted.cost_equivalent == weighted.cost_equivalent,
        "Opus 5 reads Opus 5.5's figure for the same tokens: {unweighted:?} vs {weighted:?}"
    );
}

/// A classifier call's plan row reads zero in the equivalent, and its cache halves and its split by token kind read
/// zero beside it, while a plain row's figures stay priced from its counts.
#[tokio::test]
async fn a_classifier_rows_cache_equivalents_and_split_read_zero_beside_its_zero_equivalent() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let cached = PinUsage {
        input: 500,
        cache_read: 2_000,
        write_5m: 300,
        write_1h: 0,
        output: 50,
    };
    plant_share(
        &db,
        ShareRow {
            id: "r-classifier",
            model: "claude-opus-5-5",
            usage: cached,
            classifier: Some("client"),
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-plain",
            model: "claude-fable-5-1",
            usage: cached,
            ..ShareRow::default()
        },
    );
    let groups = groups_by(
        &db,
        &RollupFilter::default(),
        hour_range(WEEKDAY_OFF_PEAK),
        "model",
        &share_tables(),
    )
    .await;
    let (marked, plain) = (
        group(&groups, "claude-opus-5-5"),
        group(&groups, "claude-fable-5-1"),
    );
    for window in [FIVE_HOUR, SEVEN_DAY] {
        assert_eq!(
            marked.cost_equivalent.get(window),
            Some(&0.0),
            "the marked group's {window}: {marked:?}"
        );
        assert!(
            marked
                .cache_equivalent
                .get(window)
                .is_some_and(|c| c.read == 0.0 && c.write == 0.0),
            "the marked group's cache halves on {window}: {marked:?}"
        );
        assert!(
            plain.cost_equivalent.get(window).is_some_and(|e| *e > 0.0)
                && plain
                    .cache_equivalent
                    .get(window)
                    .is_some_and(|c| c.read > 0.0 && c.write > 0.0),
            "the plain group's {window}: {plain:?}"
        );
    }
    assert_eq!(
        marked.equivalent_split.keys().copied().collect::<Vec<_>>(),
        ["input", "cache_read", "cache_write", "output"],
        "the marked group's split is priced, not empty: {marked:?}"
    );
    assert!(
        marked.equivalent_split.values().all(|part| *part == 0.0),
        "the marked group's split: {marked:?}"
    );
    assert!(
        plain.equivalent_split.len() == 4
            && plain.equivalent_split.values().all(|part| *part > 0.0),
        "the plain group's split: {plain:?}"
    );
}

/// The burn prices each row by its funding: a subscription row in the share of its own windows where the read names
/// its one account, an api_key row in dollars, and every plan row in the 20x equivalent under every scope, so the
/// equivalents of two accounts add into one figure while their shares do not.
#[test]
fn the_burn_series_prices_a_subscription_row_in_window_share_and_an_api_key_row_in_dollars() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant =
        |id: &str, account: &str, kind: &str, tier: Option<&str>, cache_read: Option<u64>| {
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
                    tier: tier.map(String::from),
                    ..Default::default()
                }],
                None,
            )
            .unwrap();
        };
    // Every row folds a nonzero dollar figure from its counts, so a burn that priced every row in dollars instead of
    // by its funding would fail the dollar assertion, and only pricing by funding passes it and the shares.
    plant("sub", "oauth-a", "oauth", Some(MAX_20X), None);
    plant("key", "api-b", "api_key", None, None);
    // A plan row that recorded no tier prices no share and no dollar, while its tokens and its equivalent still count,
    // its cache reads included, the one row carrying any so the cached half of the assertion below reads them.
    plant("tierless", "tierless-c", "oauth", None, Some(500));
    let tables = PlanTables::shipped();
    let point_of = |accounts: &[&str]| {
        let filter = SeriesFilter {
            from: 0,
            to: 7_200_000,
            bucket_seconds: 3_600,
            accounts: accounts.iter().map(|a| a.to_string()).collect(),
            ..SeriesFilter::default()
        };
        db.read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .expect("the bucket every tracked row landed in")
    };
    let hour = phoebis_ledger::hour_of(3_600_000);
    let band = tables.band_factor(FIVE_HOUR, None, hour * 1_000);
    let tokens = usage(1_000, 100);
    let cached = PinUsage {
        cache_read: 500,
        ..tokens
    };
    let every = point_of(&[]);
    assert_eq!(
        (every.five_hour_share, every.seven_day_share),
        (None, None),
        "three accounts' windows do not add, so a read naming none answers no share: {every:?}"
    );
    let equivalent = |window: &str, usage: PinUsage| {
        tables
            .equivalent_at(window, None, "claude-sonnet-5-5", usage, hour * 1_000)
            .expect("the shipped model weighs the model")
    };
    for (window, got) in [
        (FIVE_HOUR, every.five_hour_equivalent),
        (SEVEN_DAY, every.seven_day_equivalent),
    ] {
        let want = equivalent(window, tokens) + equivalent(window, cached);
        assert!(
            want > 0.0 && got.is_some_and(|got| (got - want).abs() < 1e-12),
            "both plan rows add their {window} equivalent, the tierless one included, and the api_key row a zero: \
             {got:?} vs {want}"
        );
    }
    let key_usd = shipped_usd(
        3_600_000,
        "claude-sonnet-5-5",
        Speed::Standard,
        usage(1_000, 100),
    );
    assert!(
        (every.usd - key_usd).abs() < 1e-9,
        "the api-key row alone prices in dollars from the dollars its counts folded: {} vs {key_usd}",
        every.usd
    );
    assert_eq!(
        (every.cached_tokens, every.uncached_tokens),
        (500, 3_000),
        "the tierless row counts its tokens and prices no dollar"
    );
    let sub = point_of(&["oauth-a"]);
    let five_hour = share_of(
        &tables,
        FIVE_HOUR,
        MAX_20X,
        "claude-sonnet-5-5",
        tokens,
        WEEKDAY_OFF_PEAK,
    ) * band;
    assert!(
        sub.five_hour_share
            .is_some_and(|share| (share - five_hour).abs() < 1e-6),
        "the subscription row prices its 5-hour share under the tier it recorded: {sub:?} vs {five_hour}"
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
        sub.seven_day_share
            .is_some_and(|share| (share - seven_day).abs() < 1e-6),
        "and its 7-day share: {sub:?} vs {seven_day}"
    );
    assert_eq!(sub.usd, 0.0, "a subscription spends no dollars");
}

/// The burn prices a row by the funding its key carries, so a tier on the key moves no dollar into a share: a billed
/// row reads its dollars, no share and a zero equivalent, under `-` as under a sized tier.
#[test]
fn a_billed_burn_row_reads_in_dollars_and_a_zero_equivalent_whatever_tier_its_key_carries() {
    let tables = PlanTables::shipped();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["k".into()],
        ..SeriesFilter::default()
    };
    let reading = |tier: &str| {
        let row = RollupRow {
            input_tokens: 1_000,
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
                "standard",
                tier,
                "-",
            )
        };
        let point = burn_series_over(&[row], &tables, &PRICES, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap();
        (
            point.usd,
            point.five_hour_share,
            point.seven_day_share,
            point.five_hour_equivalent,
            point.seven_day_equivalent,
            point.tokens,
        )
    };
    let recorded = reading(UNATTRIBUTED);
    let key_usd = shipped_usd(
        3_600_000,
        "claude-sonnet-5-5",
        Speed::Standard,
        usage(1_000, 0),
    );
    assert_eq!(
        recorded,
        (key_usd, None, None, Some(0.0), Some(0.0), 1_000),
        "an api_key account's row prices in dollars from its counts and adds a zero equivalent"
    );
    assert_eq!(
        reading(MAX_20X),
        recorded,
        "and reads the same under a sized tier on its key"
    );
}

#[test]
fn a_fast_row_never_lifts_a_window_an_unpriced_row_of_its_bucket_nulled() {
    let tables = PlanTables::shipped();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    // The shipped model places Mystery 2 in no tier, so its standard row leaves the bucket unpriced on both windows.
    let row = |model: &str, speed: &str| RollupRow {
        input_tokens: 1_000,
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
            "standard",
            MAX_20X,
            "-",
        )
    };
    let unpriced = row("claude-mystery-2", "standard");
    let fast = row("claude-opus-5-5", "fast");
    let fast_usd = shipped_usd(3_600_000, "claude-opus-5-5", Speed::Fast, usage(1_000, 0));
    for rows in [[unpriced.clone(), fast.clone()], [fast, unpriced]] {
        let point = burn_series_over(&rows, &tables, &PRICES, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap();
        assert_eq!(
            (
                point.usd,
                point.five_hour_share,
                point.seven_day_share,
                point.five_hour_equivalent,
                point.seven_day_equivalent
            ),
            (fast_usd, None, None, None, None),
            "the fast row's zero never stands for a bucket holding a row the table cannot price, whichever came first"
        );
    }
}

#[test]
fn a_fast_row_adds_dollars_and_no_share_to_a_subscription_burn_point() {
    let tables = PlanTables::shipped();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let point = |speed: &str, tier: &str| {
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
                if speed == "fast" {
                    FAST_FUNDING
                } else {
                    PLAN_FUNDING
                },
                "standard",
                tier,
                "-",
            )
        };
        burn_series_over(&[row], &tables, &PRICES, &filter)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap()
    };
    let standard = point("standard", MAX_20X);
    assert!(
        standard.five_hour_share.is_some()
            && standard.five_hour_equivalent.is_some_and(|e| e > 0.0)
            && standard.usd == 0.0,
        "a standard row on a subscription reads a share, an equivalent and no dollars: {standard:?}"
    );
    let fast_usd = shipped_usd(3_600_000, "claude-opus-5-5", Speed::Fast, usage(1_000, 100));
    let fast = point("fast", MAX_20X);
    assert_eq!(
        (
            fast.usd,
            fast.five_hour_share,
            fast.seven_day_share,
            fast.five_hour_equivalent,
            fast.seven_day_equivalent,
            fast.uncached_tokens
        ),
        (fast_usd, Some(0.0), Some(0.0), Some(0.0), Some(0.0), 1_000),
        "a fast row draws usage credits: its dollars and its tokens, a zero share of either window that still \
         says the tier is sized, and a zero equivalent"
    );
    let tierless = point("fast", UNATTRIBUTED);
    assert_eq!(
        (
            tierless.usd,
            tierless.five_hour_share,
            tierless.seven_day_share
        ),
        (fast_usd, None, None),
        "without a tier the tables size, a fast row reads its dollars and no share"
    );
}

/// A request that asked for fast mode and was answered without it files under the fast speed while its funding reads
/// the served answer: the burn prices it on the plan it drew from, as a standard row of the same usage.
#[test]
fn a_row_filed_under_the_asked_fast_speed_with_plan_funding_prices_as_a_standard_row() {
    let tables = PlanTables::shipped();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
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
                "standard",
                MAX_20X,
                "-",
            )
        };
        burn_series_over(&[row], &tables, &PRICES, &filter)
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
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let row = RollupRow {
        input_tokens: 1_000,
        output_tokens: 100,
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
            "standard",
            MAX_20X,
            "-",
        )
    };
    let share = |model: &str| {
        tables.share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            model,
            usage(1_000, 100),
            3_600_000,
        )
    };
    assert!(
        share("claude-opus-4-8").is_some() && share("claude-opus-4-8") == share("claude-opus-5-5"),
        "the model has no weight of its own and prices at Opus 5.5's, the latest weighted model of its tier"
    );
    let point = burn_series_over(&[row], &tables, &PRICES, &filter)
        .unwrap()
        .into_iter()
        .find(|p| p.at == 3_600_000)
        .unwrap();
    let fast_usd = shipped_usd(3_600_000, "claude-opus-4-8", Speed::Fast, usage(1_000, 100));
    assert_eq!(
        (point.usd, point.five_hour_share, point.seven_day_share),
        (fast_usd, Some(0.0), Some(0.0)),
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
    let filter = SeriesFilter {
        from: WEEKDAY_PEAK - 3_600_000,
        to: WEEKDAY_OFF_PEAK + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
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
                "standard",
                MAX_20X,
                "-",
            )
        };
        burn_series_over(&[row], &tables, &PRICES, &filter)
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
                tier: Some(MAX_20X.into()),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    }
    let tables = PlanTables::shipped();
    let filter = SeriesFilter {
        from: WEEKDAY_PEAK - 3_600_000,
        to: WEEKDAY_OFF_PEAK + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let points = db
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
    let factor = tables.band_factor(FIVE_HOUR, None, WEEKDAY_PEAK);
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
    // Max 20x is the anchor tier, so the 20x equivalent of its own rows is their share, band and all.
    for point in [peak, off_peak] {
        assert_eq!(
            (point.five_hour_equivalent, point.seven_day_equivalent),
            (point.five_hour_share, point.seven_day_share),
            "{point:?}"
        );
    }
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
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &daily_filter))
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
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let point = db
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
    let plant = |account: &str, kind: &str, tier: Option<&str>| {
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
                tier: tier.map(String::from),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    };
    // Every row folds dollars from its counts, so only pricing by kind leaves them off the subscription rows.
    plant("max", "oauth", Some(MAX_20X));
    plant("pro", "oauth", Some(PRO));
    plant("tierless", "oauth", None);
    plant("key", "api_key", None);
    let tables = PlanTables::shipped();
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let point_of = |account: &str| {
        let filter = SeriesFilter {
            from: hour,
            to: hour + 3_600_000,
            bucket_seconds: 3_600,
            accounts: vec![account.into()],
            ..SeriesFilter::default()
        };
        db.read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
        (
            None,
            None,
            shipped_usd(
                WEEKDAY_OFF_PEAK,
                "claude-opus-5-5",
                Speed::Standard,
                usage(1_000_000, 0)
            ),
            1_000_000
        ),
        "an api_key row keeps its estimated dollars and reads no window"
    );
    // The 20x equivalent reads no account tier: one Opus 5.5 request on Pro reads the same figure as on Max 20x or
    // with no tier at all, so a Pro account's request compares to a Max 20x account's.
    let equivalent = |window: &str| {
        tables
            .equivalent_at(window, None, "claude-opus-5-5", million, hour)
            .unwrap()
    };
    for point in [&max, &pro, &tierless] {
        assert_eq!(
            (point.five_hour_equivalent, point.seven_day_equivalent),
            (Some(equivalent(FIVE_HOUR)), Some(equivalent(SEVEN_DAY))),
            "{point:?}"
        );
    }
    assert_eq!(
        (key.five_hour_equivalent, key.seven_day_equivalent),
        (Some(0.0), Some(0.0)),
        "the api_key row adds a zero equivalent beside its dollars"
    );
}

/// One request in the off-band weekday hour on `account`, a Max 20x subscription, its one attempt carrying `input`
/// uncached tokens. `None` books a failed request, whose rollup row carries no token.
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
            tier: Some(MAX_20X.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// The off-band hour's burn point of `account`, a Max 20x subscription.
fn off_band_point_on_max_20x(db: &LedgerDb, account: &str) -> phoebis_ledger::BurnPoint {
    let tables = PlanTables::shipped();
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec![account.into()],
        ..SeriesFilter::default()
    };
    db.read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
                point.five_hour_equivalent,
                point.seven_day_equivalent,
                point.tokens,
                point.usd
            ),
            (None, None, None, None, 2_000_000, 0.0),
            "{priced} beside Mystery 2, the equivalent latched as the share is"
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
fn a_row_on_a_model_without_a_weight_prices_an_estimated_share_at_its_tiers_latest_weight() {
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
            tier: Some(MAX_20X.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let tables = PlanTables::shipped();
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["max".into()],
        ..SeriesFilter::default()
    };
    let point = db
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
    assert!(
        point.estimated,
        "the bucket is marked as priced through the fallback: {point:?}"
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
            tier: Some(MAX_20X.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let tables = PlanTables::shipped();
    let factor = tables.band_factor(FIVE_HOUR, None, finished);
    assert!(
        tables.band_factor(FIVE_HOUR, None, started) == 1.0 && factor > 1.0,
        "the attempt starts before the band and completes inside it"
    );
    let filter = SeriesFilter {
        from: WEEKDAY_BAND_OPENS - 3_600_000,
        to: WEEKDAY_BAND_OPENS + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| {
            phoebis_ledger::session_burn_series(c, &tables, &PriceRevisions::shipped(), &filter)
        })
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
    // The attempt's own tier is Max 20x, the anchor, so its equivalent reads its share, banded at its completion.
    assert_eq!(
        (point.five_hour_equivalent, point.seven_day_equivalent),
        (point.five_hour_share, point.seven_day_share),
        "{point:?}"
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
                speed: speed.map(String::from),
                tier: Some(MAX_20X.into()),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    }
    let tables = PlanTables::shipped();
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| {
            phoebis_ledger::session_burn_series(c, &tables, &PriceRevisions::shipped(), &filter)
        })
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
        (
            shipped_usd(
                WEEKDAY_OFF_PEAK,
                "claude-opus-5-5",
                Speed::Fast,
                usage(1_000_000, 0)
            ),
            Some(0.0),
            Some(0.0),
            1_000_000
        ),
        "the attempt that reported speed fast reads its dollars at the fast cells and a zero share"
    );
}

/// A rollup row of a million input tokens on account `a` under `tier`, in the hour starting at `hour` seconds.
fn burn_row(hour: i64, model: &str, speed: &str, funding: &str, tier: &str) -> RollupRow {
    RollupRow {
        input_tokens: 1_000_000,
        ..RollupRow::key(
            hour,
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
            funding,
            "standard",
            tier,
            "-",
        )
    }
}

/// The point at `hour` seconds of a series.
fn point_at(points: &[phoebis_ledger::BurnPoint], hour: i64) -> phoebis_ledger::BurnPoint {
    points
        .iter()
        .find(|p| p.at == hour * 1_000)
        .cloned()
        .unwrap_or_else(|| panic!("a point at {hour}: {points:?}"))
}

/// A billed row draws no plan window, so a bucket with tokens and no plan row reads 0 on every equivalent column,
/// and `None` stays for an idle bucket alone.
#[test]
fn a_burn_bucket_with_tokens_and_no_plan_row_reads_a_zero_equivalent_and_an_idle_bucket_none() {
    let tables = PlanTables::shipped();
    let rows = [
        burn_row(
            3_600,
            "claude-opus-5-5",
            "standard",
            OVERAGE_FUNDING,
            MAX_20X,
        ),
        burn_row(7_200, "claude-opus-5-5", "fast", FAST_FUNDING, MAX_20X),
        burn_row(
            10_800,
            "claude-opus-5-5",
            "standard",
            BILLED_FUNDING,
            UNATTRIBUTED,
        ),
    ];
    let filter = SeriesFilter {
        from: 0,
        to: 4 * 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let points = burn_series_over(&rows, &tables, &PRICES, &filter).unwrap();
    for hour in [3_600, 7_200, 10_800] {
        let point = point_at(&points, hour);
        assert!(point.usd > 0.0, "{point:?}");
        assert_eq!(
            (
                point.five_hour_equivalent,
                point.seven_day_equivalent,
                point.model_week_equivalent
            ),
            (Some(0.0), Some(0.0), None),
            "a billed row on a model without a week of its own adds a zero to each shared window: {point:?}"
        );
    }
    let idle = point_at(&points, 0);
    assert_eq!(
        (idle.five_hour_equivalent, idle.seven_day_equivalent),
        (None, None),
        "an idle bucket"
    );
}

/// The burn carries the model week beside the two shared windows: a Fable row's week share and equivalent, which a
/// row on a model without a week neither adds to nor nulls, and nothing for a bucket whose rows carry no week.
#[test]
fn the_burn_answers_the_model_week_beside_the_shared_windows() {
    let tables = PlanTables::shipped();
    let rows = [
        burn_row(3_600, "claude-fable-5-1", "standard", PLAN_FUNDING, MAX_20X),
        burn_row(3_600, "claude-opus-5-5", "standard", PLAN_FUNDING, MAX_20X),
        burn_row(7_200, "claude-opus-5-5", "standard", PLAN_FUNDING, MAX_20X),
        burn_row(10_800, "claude-fable-5-1", "fast", FAST_FUNDING, MAX_20X),
    ];
    let read = |accounts: Vec<String>| {
        let filter = SeriesFilter {
            from: 0,
            to: 4 * 3_600_000,
            bucket_seconds: 3_600,
            accounts,
            ..SeriesFilter::default()
        };
        burn_series_over(&rows, &tables, &PRICES, &filter).unwrap()
    };
    let million = usage(1_000_000, 0);
    let week_share = tables
        .share_at(
            SEVEN_DAY_FABLE,
            None,
            MAX_20X,
            "claude-fable-5-1",
            million,
            3_600_000,
        )
        .unwrap();
    let week_equivalent = tables
        .equivalent_at(
            SEVEN_DAY_FABLE,
            None,
            "claude-fable-5-1",
            million,
            3_600_000,
        )
        .unwrap();
    let named = read(vec!["a".into()]);
    let fable = point_at(&named, 3_600);
    assert_eq!(
        (fable.model_week_share, fable.model_week_equivalent),
        (Some(week_share), Some(week_equivalent)),
        "the Fable row's week, the Opus row beside it adding nothing to it: {fable:?}"
    );
    let opus = point_at(&named, 7_200);
    assert_eq!(
        (opus.model_week_share, opus.model_week_equivalent),
        (None, None),
        "a bucket whose rows carry no week of their own: {opus:?}"
    );
    let fast = point_at(&named, 10_800);
    assert_eq!(
        (fast.model_week_share, fast.model_week_equivalent),
        (Some(0.0), Some(0.0)),
        "a fast Fable row adds a zero to its week: {fast:?}"
    );
    let every = point_at(&read(Vec::new()), 3_600);
    assert_eq!(
        (every.model_week_share, every.model_week_equivalent),
        (None, Some(week_equivalent)),
        "a read naming no account answers the week's equivalent alone: {every:?}"
    );
}

/// One request of a million Opus-tier input tokens on `account` under `tier` in the off-band hour, `session` its
/// session.
fn plant_burn(db: &LedgerDb, id: &str, account: &str, tier: &str, model: &str, session: &str) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 500),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            session_id: Some(session.into()),
            canonical_model: model.into(),
            model: model.into(),
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
            account_kind: "oauth".into(),
            started_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 500),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            tier: Some(tier.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// The burn groups by every dimension the breakdown knows, each group's 20x equivalent adding across accounts and
/// account tiers, and answers the own-window shares only where each group holds one account's rows.
#[tokio::test]
async fn the_burn_groups_by_every_breakdown_dimension_and_answers_the_shares_where_one_account_is_named(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_burn(&db, "r-a-opus", "a", MAX_20X, "claude-opus-5-5", "s");
    plant_burn(&db, "r-a-sonnet", "a", MAX_20X, "claude-sonnet-5-5", "s");
    plant_burn(&db, "r-b-opus", "b", PRO, "claude-opus-5-5", "s");
    let tables = PlanTables::shipped();
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let read = |group_by: &'static str, accounts: Vec<String>| {
        let db = db.clone();
        let tables = tables.clone();
        async move {
            let filter = SeriesFilter {
                from: hour * 1_000,
                to: (hour + 3_600) * 1_000,
                bucket_seconds: 3_600,
                accounts,
                ..SeriesFilter::default()
            };
            burn_groups_paged(
                &db,
                &tables,
                &PRICES,
                &filter,
                group_by,
                &LiveKeys::default(),
                &[],
            )
            .await
            .unwrap()
        }
    };
    let point = |groups: &[(String, Vec<phoebis_ledger::BurnPoint>)], key: &str| {
        let (_, points) = groups
            .iter()
            .find(|(group, _)| group == key)
            .unwrap_or_else(|| panic!("a group {key}: {groups:?}"));
        point_at(points, hour)
    };
    let million = usage(1_000_000, 0);
    let by_model = read("model", Vec::new()).await;
    assert_eq!(
        by_model
            .iter()
            .map(|(group, _)| group.as_str())
            .collect::<Vec<_>>(),
        ["claude-opus-5-5", "claude-sonnet-5-5"]
    );
    let opus = point(&by_model, "claude-opus-5-5");
    let equivalent = tables
        .equivalent_at(FIVE_HOUR, None, "claude-opus-5-5", million, hour * 1_000)
        .unwrap();
    assert!(
        opus.five_hour_equivalent
            .is_some_and(|e| (e - 2.0 * equivalent).abs() < 1e-12)
            && opus.five_hour_share.is_none(),
        "the two accounts' rows add in the equivalent and not in the shares: {opus:?} vs {equivalent}"
    );
    let named = read("model", vec!["a".into()]).await;
    let opus = point(&named, "claude-opus-5-5");
    let share = |tier: &str| {
        share_of(
            &tables,
            FIVE_HOUR,
            tier,
            "claude-opus-5-5",
            million,
            hour * 1_000,
        )
    };
    assert!(
        opus.five_hour_share
            .is_some_and(|s| (s - share(MAX_20X)).abs() < 1e-12),
        "a filter naming account a reads its share under any grouping: {opus:?}"
    );
    let by_account = read("account", Vec::new()).await;
    assert!(
        point(&by_account, "b")
            .five_hour_share
            .is_some_and(|s| (s - share(PRO)).abs() < 1e-12),
        "each account group is one account, priced under the tier its rows recorded: {by_account:?}"
    );
    let all = read("all", Vec::new()).await;
    assert_eq!(
        all.iter()
            .map(|(group, _)| group.as_str())
            .collect::<Vec<_>>(),
        ["all"]
    );
}

/// A session can move between accounts: its series adds every attempt's equivalent, and reads a share once the read
/// names one account, whose attempts alone it then holds.
#[test]
fn a_session_series_answers_its_shares_where_the_read_names_one_account() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_burn(&db, "r-a", "a", MAX_20X, "claude-opus-5-5", "s");
    plant_burn(&db, "r-b", "b", PRO, "claude-opus-5-5", "s");
    let tables = PlanTables::shipped();
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let read = |accounts: Vec<String>| {
        let filter = SeriesFilter {
            from: hour * 1_000,
            to: (hour + 3_600) * 1_000,
            bucket_seconds: 3_600,
            accounts,
            ..SeriesFilter::default()
        };
        let sessions = db
            .read_blocking(|c| phoebis_ledger::session_burn_series(c, &tables, &PRICES, &filter))
            .unwrap();
        let (_, points) = sessions
            .iter()
            .find(|(id, _)| id == "s")
            .unwrap_or_else(|| panic!("the session is answered: {sessions:?}"));
        point_at(points, hour)
    };
    let million = usage(1_000_000, 0);
    let equivalent = tables
        .equivalent_at(
            FIVE_HOUR,
            None,
            "claude-opus-5-5",
            million,
            WEEKDAY_OFF_PEAK + 500,
        )
        .unwrap();
    let every = read(Vec::new());
    assert!(
        every.five_hour_share.is_none()
            && every
                .five_hour_equivalent
                .is_some_and(|e| (e - 2.0 * equivalent).abs() < 1e-12)
            && every.uncached_tokens == 2_000_000,
        "both attempts add in the equivalent and not in the shares: {every:?} vs {equivalent}"
    );
    let a = read(vec!["a".into()]);
    let share = share_of(
        &tables,
        FIVE_HOUR,
        MAX_20X,
        "claude-opus-5-5",
        million,
        WEEKDAY_OFF_PEAK + 500,
    );
    assert!(
        a.five_hour_share.is_some_and(|s| (s - share).abs() < 1e-12)
            && a.uncached_tokens == 1_000_000,
        "account a's attempt alone, under the tier it recorded: {a:?} vs {share}"
    );
}

/// A read naming one account ranks the sessions by that account's attempts alone, so the series lists the account's
/// own busiest sessions even where twenty sessions of another account hold more tokens over the range.
#[test]
fn a_session_series_naming_one_account_ranks_that_account_s_sessions_alone() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Each session carries a million tokens, and a tie ranks by session id, so b's twenty sort ahead of a's one.
    for n in 0..20 {
        plant_burn(
            &db,
            &format!("r-b-{n:02}"),
            "b",
            PRO,
            "claude-opus-5-5",
            &format!("s-b-{n:02}"),
        );
    }
    plant_burn(&db, "r-a", "a", MAX_20X, "claude-opus-5-5", "s-z");
    let tables = PlanTables::shipped();
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let filter = SeriesFilter {
        from: hour * 1_000,
        to: (hour + 3_600) * 1_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| phoebis_ledger::session_burn_series(c, &tables, &PRICES, &filter))
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["s-z"],
        "account a's one session, and none of b's: {sessions:?}"
    );
    let point = point_at(&sessions[0].1, hour);
    assert!(
        point.uncached_tokens == 1_000_000 && point.five_hour_share.is_some(),
        "its attempt priced under a's tier: {point:?}"
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

/// One Sonnet request of `session` on the oauth account `a` on `test_tier` in the off-band hour, `overage` as its head
/// reported it.
fn plant_funded(db: &LedgerDb, id: &str, session: &str, overage: Option<bool>) {
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
            overage_in_use: overage,
            tier: Some("test_tier".into()),
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
        None,
        "test_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
        WEEKDAY_OFF_PEAK,
    )
    .shares;
    assert_eq!(keys(&fable), [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]);
    assert!(
        close(fable[FIVE_HOUR], 0.1)
            && close(fable[SEVEN_DAY], 0.01)
            && close(fable[SEVEN_DAY_FABLE], 0.02),
        "a Fable model prices every window it carries: {fable:?}"
    );
    let haiku = price_shares(
        &tables,
        None,
        "test_tier",
        "claude-haiku-4-5",
        one,
        WEEKDAY_OFF_PEAK,
        WEEKDAY_OFF_PEAK,
    )
    .shares;
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
        None,
        "narrow_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
        WEEKDAY_OFF_PEAK,
    )
    .shares;
    assert_eq!(keys(&narrow), [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]);
    assert!(
        close(narrow[FIVE_HOUR], 0.1) && narrow[SEVEN_DAY].is_none() && narrow[SEVEN_DAY_FABLE].is_none(),
        "a tier sized on the 5-hour window alone prices the week and the model week null: {narrow:?}"
    );
    let unweighted = price_shares(
        &tables,
        None,
        "test_tier",
        "claude-opus-5-5",
        one,
        WEEKDAY_OFF_PEAK,
        WEEKDAY_OFF_PEAK,
    )
    .shares;
    assert_eq!(
        unweighted.into_iter().collect::<Vec<_>>(),
        [(FIVE_HOUR, None), (SEVEN_DAY, None)],
        "a model with no weight keeps its two shared keys, each null"
    );
    let no_tier = price_shares(
        &tables,
        None,
        "missing_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
        WEEKDAY_OFF_PEAK,
    )
    .shares;
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
    plant_funded(&db, "r-plan", "s", None);
    plant_funded(&db, "r-overage", "s", Some(true));
    let tables = share_tables();
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
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
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
    // The two rows carry the same counts, so the bucket holding one row's dollars rather than both proves the plan
    // row adds none.
    let one_row = shipped_usd(
        WEEKDAY_OFF_PEAK,
        "claude-sonnet-5-5",
        Speed::Standard,
        usage(1_000, 100),
    );
    assert!(
        (point.usd - one_row).abs() < 1e-9,
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
    plant_funded(&db, "r-plan", "s-mixed", None);
    plant_funded(&db, "r-overage", "s-mixed", Some(true));
    let tables = share_tables();
    let hour = phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000;
    let filter = SeriesFilter {
        from: hour,
        to: hour + 3_600_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let sessions = db
        .read_blocking(|c| {
            phoebis_ledger::session_burn_series(c, &tables, &PriceRevisions::shipped(), &filter)
        })
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
    // The two attempts carry the same counts, so the bucket holding one attempt's dollars rather than both proves the
    // plan attempt adds none.
    let one_attempt = shipped_usd(
        WEEKDAY_OFF_PEAK,
        "claude-sonnet-5-5",
        Speed::Standard,
        usage(1_000, 100),
    );
    assert!(
        (point.usd - one_attempt).abs() < 1e-9,
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
        // Account a is a Max 20x subscription and c a Pro one, recorded on each attempt as the landing would.
        for attempt in &mut attempts {
            attempt.tier = match attempt.account_id.as_str() {
                "a" => Some(MAX_20X.into()),
                "c" => Some(PRO.into()),
                _ => None,
            };
        }
        db.insert_request(&request, &attempts, None).unwrap();
    }
    let tables = PlanTables::shipped();
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
                .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
                .unwrap();
            let over = burn_series_over(&rows, &tables, &PRICES, &filter).unwrap();
            // The unweighted GPT rows of b and c latch their buckets' equivalent, so a scope holding them reads the
            // fast rows' dollars alone.
            assert!(
                read.iter()
                    .any(|p| p.five_hour_equivalent.is_some_and(|e| e > 0.0) || p.usd > 0.0),
                "the scope reads something: {filter:?}"
            );
            assert_eq!(
                read.iter().any(|p| p.five_hour_share.is_some()),
                filter.accounts.len() == 1,
                "the shares answer where the scope names one account alone: {filter:?}"
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
    let by_fit = db
        .read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
        .unwrap();
    assert_eq!(
        by_fit,
        BTreeMap::from([(
            "shed".to_string(),
            BTreeMap::from([("unknown".to_string(), 1)])
        )]),
        "a move without a fit reads under the fit unknown"
    );
}

#[test]
fn the_held_series_reads_the_slow_down_columns_per_bucket() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Two rows of `a` in the second hour, apart by their key, and one of `b`, which the filter leaves out.
    let held = |id: &str, key_id: &str, account_id: &str, held_ms: u64| {
        let request = RequestRecord {
            id: id.into(),
            received_at: 3_600_500,
            key_id: Some(key_id.into()),
            pool_id: Some("p".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            held: Some(format!(
                r#"{{"waited_on":["{account_id}"],"cohort":null,"held_ms":{held_ms},"waits":[{{"reason":"slow_down","held_ms":{held_ms}}}]}}"#
            )),
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: account_id.into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            output_tokens: Some(5),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    };
    held("r1", "desktop", "a", 2_000);
    held("r2", "laptop", "a", 3_000);
    held("r3", "desktop", "b", 7_000);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let series = db.read_blocking(|c| held_series(c, &filter)).unwrap();
    assert_eq!(
        series,
        vec![(0, 0, 0), (3_600_000, 5_000, 2)],
        "one point per bucket, at its millisecond edge: the two rows' waits summed and their requests counted"
    );
    let pools = SeriesFilter {
        pools: vec!["q".into()],
        accounts: Vec::new(),
        ..filter
    };
    assert_eq!(
        db.read_blocking(|c| held_series(c, &pools)).unwrap(),
        vec![(0, 0, 0), (3_600_000, 0, 0)],
        "a pool the rows are not under reads nothing"
    );
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
    let by_fit = db
        .read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
        .unwrap();
    assert_eq!(
        by_fit,
        BTreeMap::from([
            (
                "future_reason".to_string(),
                BTreeMap::from([("unknown".to_string(), 1)])
            ),
            (
                "unknown".to_string(),
                BTreeMap::from([("unknown".to_string(), 1)])
            ),
        ]),
        "the compound key splits into the reason and the fit, each unknown where the event names none"
    );
}

#[test]
fn a_zero_bucket_seconds_answers_the_range_error_rather_than_panicking() {
    let db = LedgerDb::open_in_memory().unwrap();
    // `SeriesFilter::default()` carries `bucket_seconds: 0`; the guard has to run before the division that
    // would otherwise panic on it.
    let filter = SeriesFilter::default();
    assert!(db.read_blocking(|c| moves_by_reason(c, &filter)).is_err());
    assert!(db
        .read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
        .is_err());
    assert!(db.read_blocking(|c| rewrite_series(c, &filter)).is_err());
    let tables = PlanTables::shipped();
    assert!(db
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter))
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
fn priced_table(model: &str, rate_per_token: f64) -> PriceRevisions {
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
    one_entry(model, row)
}

/// A list of one entry, dated before every instant the spend cases read, holding `row` under `model`.
fn one_entry(model: &str, row: ModelRow) -> PriceRevisions {
    PriceRevisions::new(
        vec![PriceTable {
            effective: "2026-01-01".into(),
            models: [(model.to_string(), row)].into_iter().collect(),
        }],
        "test",
    )
    .expect("one entry")
}

/// One canonical model with a distinct rate per token kind, so a transposition of the five sums `member_spends`
/// passes to `estimate_banded` (the flat-rate `priced_table` above prices every kind alike and cannot catch one)
/// shows up as a wrong total rather than the same number in the wrong place.
fn priced_table_distinct(model: &str) -> PriceRevisions {
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
    one_entry(model, row)
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
                &PriceRevisions::shipped(),
            )
        })
        .unwrap();
    let shipped = PriceRevisions::shipped();
    let table = shipped.in_force(ms(2026, 1, 15, 1, 0));
    let input = |speed| {
        table
            .price_at("claude-opus-5", speed, 0)
            .expect("the shipped list prices Opus 5")
            .input
    };
    assert!(
        (rows[0].spent_usd - (input(Speed::Standard) + input(Speed::Fast))).abs() < 1e-9,
        "a million Opus 5 input tokens at the shipped standard cell and a million at the fast cell: {}",
        rows[0].spent_usd
    );
}

/// A request and its one attempt, for a pool member's spend: the rollup row carries the counts alone, so a test that
/// reads the sum back at its own table's price for the seeded token count proves `member_spends` prices at read
/// time.
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
        "the row before the day is left out, the probe counts like any other write, and the price is the \
         table's the read passes, not the shipped list's"
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

/// A lifetime total passes `i64::MIN` as its first hour: every seeded row is priced, and the totals equal the same
/// read from the earliest seeded hour, so the sentinel bounds the SQL and nothing multiplies it.
#[test]
fn a_lifetime_total_prices_every_row_as_a_read_from_the_earliest_hour_does() {
    let db = LedgerDb::open_in_memory().unwrap();
    let (first, second) = (ms(2026, 1, 10, 3, 0), ms(2026, 1, 12, 7, 0));
    plant_spend(
        &db,
        "r1",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        first,
        1_000,
    );
    plant_spend(
        &db,
        "r2",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        second,
        2_000,
    );
    let to = hour_ceil(ms(2026, 1, 15, 0, 0));
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let (lifetime, since, key_lifetime, key_since) = db
        .read_blocking(|c| {
            Ok((
                request_totals(c, "account_id", i64::MIN, to, &PRICES, &COSTS)?,
                request_totals(c, "account_id", hour_of(first), to, &PRICES, &COSTS)?,
                key_request_totals(c, i64::MIN, to, &live, &PRICES, &COSTS)?,
                key_request_totals(c, hour_of(first), to, &live, &PRICES, &COSTS)?,
            ))
        })
        .unwrap();
    let want = shipped_usd(first, "claude-sonnet-5", Speed::Standard, usage(1_000, 0))
        + shipped_usd(second, "claude-sonnet-5", Speed::Standard, usage(2_000, 0));
    assert!(
        lifetime
            .get("a")
            .is_some_and(|t| t.requests == 2 && (t.cost_usd - want).abs() < 1e-12),
        "{lifetime:?} against {want}"
    );
    assert_eq!(lifetime, since);
    assert_eq!(key_lifetime, key_since);
    assert_eq!(key_lifetime["desktop"], lifetime["a"]);
}

/// The totals price each group at its own hour, so a cost revision that takes effect inside one price revision's
/// span moves the equivalent at its own date: the hour before 2026-01-15 reads the first entry's weights and the hour
/// after reads them doubled.
#[test]
fn a_cost_revision_inside_a_price_span_moves_the_totals_equivalent_at_its_own_boundary() {
    let db = LedgerDb::open_in_memory().unwrap();
    let costs = PlanTables {
        revisions: CostRevisions::from_yaml(
            &phoebis_ledger::test_tables::SUBSCRIPTION_COSTS.replacen(
                "effective: 2026-02-01",
                "effective: 2026-01-15",
                1,
            ),
        )
        .expect("the cost list with its second entry on 2026-01-15 parses"),
        path: None,
        source: "test".to_string(),
    };
    let prices = phoebis_ledger::test_tables::api_prices();
    let boundary = ms(2026, 1, 15, 0, 0);
    let (before, after) = (boundary - 1_800_000, boundary + 1_800_000);
    plant_spend(
        &db,
        "r-before",
        "p",
        "a",
        "claude-opus-5-5",
        "inference",
        before,
        1_000,
    );
    plant_spend(
        &db,
        "r-after",
        "p",
        "a",
        "claude-opus-5-5",
        "inference",
        after,
        1_000,
    );
    let (from_hour, to_hour) = (hour_of(before), hour_of(after) + 3_600);
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let (accounts, keys) = db
        .read_blocking(|c| {
            Ok((
                request_totals(c, "account_id", from_hour, to_hour, &prices, &costs)?,
                key_request_totals(c, from_hour, to_hour, &live, &prices, &costs)?,
            ))
        })
        .unwrap();
    let at = |instant: i64| {
        let hour = hour_of(instant) * 1_000;
        price_equivalent(&costs, None, "claude-opus-5-5", usage(1_000, 0), hour, hour).shares
    };
    let (first, second) = (at(before), at(after));
    assert!(
        second[FIVE_HOUR].unwrap() > first[FIVE_HOUR].unwrap(),
        "the second cost entry weighs the model more: {first:?} vs {second:?}"
    );
    for totals in [&accounts["a"], &keys["desktop"]] {
        for (window, got) in [
            (FIVE_HOUR, totals.equivalent.five_hour),
            (SEVEN_DAY, totals.equivalent.seven_day),
        ] {
            let want = first[window].unwrap() + second[window].unwrap();
            assert!(
                (got - want).abs() < 1e-12,
                "each row at its own cost entry on {window}: {totals:?} vs {want}"
            );
        }
    }
}

/// The totals read each group's peak factor at its own hour: one Opus 5.5 input token weighs 2 units in the test
/// cost model's first entry, 0.02 of a Max 20x 5-hour window's 100 out of the band and 0.04 inside it, where the
/// window holds half its capacity.
#[test]
fn a_peak_hour_beside_a_quiet_one_prices_the_totals_equivalent_at_each_hours_factor() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Thursday 2026-01-15: 12:00 UTC is 04:00 in Los Angeles, before the band, and 13:00 UTC its first hour.
    let (quiet, peak) = (ms(2026, 1, 15, 12, 30), ms(2026, 1, 15, 13, 30));
    plant_spend(
        &db,
        "r-quiet",
        "p",
        "a",
        "claude-opus-5-5",
        "inference",
        quiet,
        1,
    );
    plant_spend(
        &db,
        "r-peak",
        "p",
        "a",
        "claude-opus-5-5",
        "inference",
        peak,
        1,
    );
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let prices = phoebis_ledger::test_tables::api_prices();
    let totals = db
        .read_blocking(|c| {
            request_totals(
                c,
                "account_id",
                hour_of(quiet),
                hour_of(peak) + 3_600,
                &prices,
                &costs,
            )
        })
        .unwrap();
    let five_hour = totals["a"].equivalent.five_hour;
    assert!(
        (five_hour - (0.02 + 0.04)).abs() < 1e-12,
        "the quiet hour's 0.02 and the peak hour's 0.04: {five_hour}"
    );
}

/// Two rows of one hour, account tier and model, one Opus 5.5 input token each, one from an entrypoint the test regime
/// lists and one from an entrypoint it leaves out: each prices under its own regime, 2 / 200 and 2 / 100 of the Max 20x
/// 5-hour window, where one price over the group would read the base 0.04. A session attempt from the listed
/// entrypoint reads the regime's figure. 04:00 in Los Angeles, before the band, in the first cost entry.
#[test]
fn a_group_mixing_two_clients_prices_each_rows_regime() {
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let prices = phoebis_ledger::test_tables::api_prices();
    let at = phoebis_ledger::test_tables::JANUARY_2026;
    let row = |id, client| ShareRow {
        id,
        model: "claude-opus-5-5",
        at,
        usage: usage(1, 0),
        client: Some(client),
        tier: Some(MAX_20X),
        ..ShareRow::default()
    };
    let (from_hour, to_hour) = (hour_of(at), hour_of(at) + 3_600);
    let filter = SeriesFilter {
        from: from_hour * 1_000,
        to: to_hour * 1_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let close = |got: f64, want: f64| (got - want).abs() < 1e-12;
    let db = LedgerDb::open_in_memory().unwrap();
    plant_share(&db, row("r-desk", "claude-code/desk"));
    plant_share(&db, row("r-sdk", "claude-code/sdk-cli"));
    let (totals, points) = db
        .read_blocking(|c| {
            Ok((
                request_totals(c, "account_id", from_hour, to_hour, &prices, &costs)?,
                burn_series(c, &costs, &prices, &filter)?,
            ))
        })
        .unwrap();
    let equivalent = totals["a"].equivalent.five_hour;
    assert!(
        close(equivalent, 0.01 + 0.02),
        "each row under its own regime: {equivalent}"
    );
    let share = points
        .iter()
        .find_map(|p| p.five_hour_share)
        .unwrap_or_else(|| panic!("the hour carries its share: {points:?}"));
    assert!(
        close(share, 0.01 + 0.02),
        "each row under its own regime: {share}"
    );
    let db = LedgerDb::open_in_memory().unwrap();
    plant_share(&db, row("r-desk", "claude-code/desk"));
    let sessions = db
        .read_blocking(|c| phoebis_ledger::session_burn_series(c, &costs, &prices, &filter))
        .unwrap();
    let share = sessions
        .iter()
        .find(|(id, _)| id == "s")
        .and_then(|(_, points)| points.iter().find_map(|p| p.five_hour_share))
        .unwrap_or_else(|| panic!("the session's hour carries its share: {sessions:?}"));
    assert!(close(share, 0.01), "the attempt under the regime: {share}");
}

/// The dashboard's day, a pool's traffic and the totals read the billed rows' dollars apart and the plan rows in the
/// equivalent, marked where a plan row priced through the fallback.
#[tokio::test]
async fn the_dashboard_the_traffic_and_the_totals_split_the_billed_dollars_from_the_plan_rows_equivalent(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let plan = usage(1_000, 0);
    plant_share(
        &db,
        ShareRow {
            id: "r-plan",
            model: "claude-opus-5-5",
            usage: plan,
            ..ShareRow::default()
        },
    );
    // Opus 5 has no weight of its own in the shipped model and prices at its model tier's latest weight.
    plant_share(
        &db,
        ShareRow {
            id: "r-unweighted",
            model: "claude-opus-5",
            usage: plan,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-overage",
            model: "claude-opus-5-5",
            usage: usage(2_000, 0),
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
            model: "claude-opus-5-5",
            usage: usage(3_000, 0),
            ..ShareRow::default()
        },
    );
    let billed = shipped_usd(
        WEEKDAY_OFF_PEAK,
        "claude-opus-5-5",
        Speed::Standard,
        usage(2_000, 0),
    ) + shipped_usd(
        WEEKDAY_OFF_PEAK,
        "claude-opus-5-5",
        Speed::Standard,
        usage(3_000, 0),
    );
    let window = |w: &str| {
        equivalent_of("claude-opus-5-5", plan, WEEKDAY_OFF_PEAK)[w].unwrap()
            + equivalent_of("claude-opus-5", plan, WEEKDAY_OFF_PEAK)[w].unwrap()
    };
    let read =
        |got_billed: f64, got: &phoebis_ledger::SharedEquivalent, estimated: bool, what: &str| {
            assert!(
                (got_billed - billed).abs() < 1e-12
                    && (got.five_hour - window(FIVE_HOUR)).abs() < 1e-12
                    && (got.seven_day - window(SEVEN_DAY)).abs() < 1e-12
                    && estimated,
                "{what}: {got_billed} and {got:?} vs {billed}, {} and {}",
                window(FIVE_HOUR),
                window(SEVEN_DAY)
            );
        };
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let day = phoebis_ledger::dashboard(&db, WEEKDAY_OFF_PEAK, &PRICES, &COSTS)
        .await
        .unwrap();
    read(
        day.billed_24h,
        &day.equivalent_24h,
        day.estimated_24h,
        "the day",
    );
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    read(point.billed, &point.equivalent, true, "the hour");
    let traffic =
        phoebis_ledger::pool_traffic(&db, "p", hour, hour + 3_600, 3_600, &PRICES, &COSTS)
            .await
            .unwrap();
    read(
        traffic[0].billed_usd,
        &traffic[0].equivalent,
        traffic[0].estimated,
        "the pool's bucket",
    );
    let totals = db
        .read_blocking(|c| request_totals(c, "pool_id", hour, hour + 3_600, &PRICES, &COSTS))
        .unwrap();
    read(
        totals["p"].billed_usd,
        &totals["p"].equivalent,
        totals["p"].estimated,
        "the pool's totals",
    );
}

/// A guardrail's block is a refusal the operator asked for: the day's error rate and its most common code leave the
/// blocked requests out, numerator and denominator and the hour's errors alike, and the day's requests keep them.
#[tokio::test]
async fn the_dashboard_leaves_the_blocked_requests_out_of_the_rate_and_the_top_code_and_keeps_them_in_the_requests(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let row = |n: usize, outcome: &str, status: u16, code: Option<&str>| RequestRecord {
        id: format!("r{n}"),
        received_at: WEEKDAY_OFF_PEAK - 60_000 + n as i64,
        key_id: Some("k".into()),
        pool_id: Some("p".into()),
        model: "claude-opus-5-5".into(),
        canonical_model: "claude-opus-5-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(status),
        outcome: outcome.into(),
        error_code: code.map(String::from),
        ..RequestRecord::default()
    };
    for n in 0..10 {
        let request = match n {
            0..6 => row(n, "succeeded", 200, None),
            6..9 => row(n, "rejected", 400, Some("guardrail_blocked")),
            _ => row(n, "failed", 502, Some("upstream_error")),
        };
        db.insert_request(&request, &[], None).unwrap();
    }
    let hour = hour_of(WEEKDAY_OFF_PEAK - 60_000);
    let day = phoebis_ledger::dashboard(&db, WEEKDAY_OFF_PEAK, &PRICES, &COSTS)
        .await
        .unwrap();
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    assert_eq!(
        (day.requests_24h, point.requests, point.errors),
        (10, 10, 1),
        "the blocked requests count among the requests and not among the hour's errors"
    );
    assert_eq!(day.error_rate_24h, Some(1.0 / 7.0));
    assert_eq!(day.top_error_code.as_deref(), Some("upstream_error"));
}

/// A probe on a model the revision does not list consumed quota all the same, so its row prices its counts and its
/// wasted counts at the dearest listed cell of each type of the entry in force, while an inference row on the same
/// model stays unpriced.
#[test]
fn a_probe_row_on_a_model_the_revision_lacks_prices_at_the_dearest_listed_cells() {
    let prices = phoebis_ledger::test_tables::api_prices();
    let hour = hour_of(phoebis_ledger::test_tables::JANUARY_2026);
    let row = |kind: &str| RollupRow {
        requests: 1,
        input_tokens: 1_000,
        output_tokens: 100,
        wasted_probes: u64::from(kind == "probe"),
        wasted_input_tokens: if kind == "probe" { 500 } else { 0 },
        ..RollupRow::key(
            hour,
            "k",
            0,
            "p",
            "a",
            "claude-mystery-9",
            kind,
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            "standard",
            PLAN_FUNDING,
            "standard",
            UNATTRIBUTED,
            "-",
        )
    };
    let groups = breakdown(
        &[row("inference"), row("probe")],
        "kind",
        true,
        &LiveKeys::default(),
        &[],
        None,
        &prices,
        &COSTS,
    );
    let rates = prices.fallback_rates(hour * 1_000);
    let (input, output) = (rates.input.unwrap(), rates.output.unwrap());
    assert!(input > 0.0 && output > 0.0);
    let probe = groups.iter().find(|g| g.key == "probe").unwrap();
    assert!(
        (probe.probe_cost_usd - (1_000.0 * input + 100.0 * output)).abs() < 1e-12
            && (probe.wasted_probe_usd - 500.0 * input).abs() < 1e-12,
        "{probe:?}"
    );
    let inference = groups.iter().find(|g| g.key == "inference").unwrap();
    assert_eq!(
        inference.cost_usd, 0.0,
        "an inference row on the model stays unpriced"
    );
}

/// A member's rows carry the band their attempts priced at, so a group on a model priced by prompt length prices its
/// counts at the cells its band names, exactly what its attempts drew, and a revision in force that moves those cells
/// moves its dollars as it moves every other group's.
#[test]
fn a_members_spend_prices_every_group_from_its_band_rows_counts_exactly() {
    let db = LedgerDb::open_in_memory().unwrap();
    let now = ms(2026, 1, 15, 10, 0);
    // The rows land at the test list, so the Haiku 5.5 row over the first entry's threshold files under `long` and
    // the Sonnet 5.5 row under `standard`.
    let prices = phoebis_ledger::test_tables::api_prices();
    for (id, model, input) in [
        ("long", "claude-haiku-5-5", 1_001),
        ("plain", "claude-sonnet-5-5", 1_000_000),
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
            ..Default::default()
        };
        db.insert_request_at(&request, &[attempt], None, &prices)
            .unwrap();
    }
    let spend = |prices: &PriceRevisions| {
        db.read_blocking(|c| {
            member_spends(c, &[budgeted("p", "a", BudgetPeriod::Day)], now, prices)
        })
        .unwrap()[0]
            .spent_usd
    };
    let input_usd = |prices: &PriceRevisions, model: &str, input: u64| {
        prices
            .in_force(phoebis_ledger::test_tables::JANUARY_2026)
            .estimate(model, Speed::Standard, input, 0, 0, 0, 0)
            .expect("the test list carries the model")
            .0
    };
    let table = prices.in_force(phoebis_ledger::test_tables::JANUARY_2026);
    let long = input_usd(&prices, "claude-haiku-5-5", 1_001);
    let long_cells = &table.models["claude-haiku-5-5"]
        .long_context
        .as_ref()
        .expect("the test list prices Haiku 5.5 by prompt length")
        .cells;
    assert!(
        (long - 1_001.0 * long_cells.input / 1e6).abs() < 1e-12,
        "the attempt drew the long-context cells: {long}"
    );
    assert!(
        (spend(&prices) - (long + input_usd(&prices, "claude-sonnet-5-5", 1_000_000))).abs() < 1e-9,
        "{}",
        spend(&prices)
    );
    // A list in force that doubles Sonnet 5.5's input and Haiku 5.5's standard and long-context input moves both
    // groups.
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
    let doubled = PriceRevisions::from_yaml(&text, "test").unwrap();
    assert!(
        input_usd(&doubled, "claude-haiku-5-5", 1_001) > long,
        "the long-context cells moved"
    );
    assert!(
        (spend(&doubled)
            - (input_usd(&doubled, "claude-haiku-5-5", 1_001)
                + input_usd(&doubled, "claude-sonnet-5-5", 1_000_000)))
        .abs()
            < 1e-9,
        "the long-context group moves with the plain one: {}",
        spend(&doubled)
    );
}

/// A month whose middle carries a revision's date sums as two spans, each priced at the entry in force over it: a
/// million Sonnet 5.5 input tokens before the date at the first entry's cell and a million after it at the second's.
#[test]
fn a_members_spend_across_a_revision_inside_its_period_prices_each_span_at_its_own_entry() {
    let db = LedgerDb::open_in_memory().unwrap();
    // The test list with its second entry, which doubles the cells, moved into the middle of January.
    let text = phoebis_ledger::test_tables::API_PRICES.replacen(
        "effective: 2026-02-01",
        "effective: 2026-01-20",
        1,
    );
    let prices = PriceRevisions::from_yaml(&text, "test").unwrap();
    for (id, day) in [("before", 10), ("after", 25)] {
        let request = RequestRecord {
            id: id.into(),
            received_at: ms(2026, 1, day, 1, 0),
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            canonical_model: "claude-sonnet-5-5".into(),
            model: "claude-sonnet-5-5".into(),
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
            ..Default::default()
        };
        db.insert_request_at(&request, &[attempt], None, &prices)
            .unwrap();
    }
    let cell = |day: i8| {
        prices.in_force(ms(2026, 1, day, 1, 0)).models["claude-sonnet-5-5"]
            .standard
            .input
    };
    assert_eq!(
        cell(25),
        2.0 * cell(10),
        "the second entry doubles the cell"
    );
    let spent = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[budgeted("p", "a", BudgetPeriod::Month)],
                ms(2026, 1, 28, 0, 0),
                &prices,
            )
        })
        .unwrap()[0]
        .spent_usd;
    assert!(
        (spent - (cell(10) + cell(25))).abs() < 1e-9,
        "a million at each entry's cell: {spent}"
    );
}

#[test]
fn the_reconciliations_read_answers_the_named_accounts_newest_first_inside_the_trailing_thirty_days(
) {
    let now = WEEKDAY_OFF_PEAK;
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        for (account, window, window_end) in [
            ("a", "five_hour", now - DAY_MS),
            ("a", "seven_day", now - 3 * DAY_MS),
            ("b", "five_hour", now - 2 * DAY_MS),
            ("c", "seven_day_fable", now - 4 * DAY_MS),
            ("a", "five_hour", now - 30 * DAY_MS - 1),
        ] {
            c.execute(
                "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
                 last_reading_at, observed_rise, predicted_share, ratio, requests, estimated, computed_at) \
                 VALUES (?1, ?2, ?3, ?3, ?3, 0.2, 0.2, 1.0, 1, 0, ?3)",
                rusqlite::params![account, window, window_end],
            )?;
        }
        Ok(())
    })
    .unwrap();
    let read = |accounts: &[&str]| {
        let accounts: Vec<String> = accounts.iter().map(|a| a.to_string()).collect();
        db.read_blocking(|c| phoebis_ledger::read_reconciliations(c, &accounts, now))
            .unwrap()
            .into_iter()
            .map(|r| (r.account_id, r.window, r.window_end))
            .collect::<Vec<_>>()
    };
    let row = |account: &str, window: &str, window_end: i64| {
        (account.to_string(), window.to_string(), window_end)
    };
    assert_eq!(
        read(&["a", "c"]),
        [
            row("a", "five_hour", now - DAY_MS),
            row("a", "seven_day", now - 3 * DAY_MS),
            row("c", "seven_day_fable", now - 4 * DAY_MS),
        ],
        "the named accounts' windows, newest first, the one that ended before the trailing 30 days left out"
    );
    assert_eq!(
        read(&[]).len(),
        4,
        "an empty filter answers every account's windows of the 30 days"
    );
}
