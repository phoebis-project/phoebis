//! Rollups for a synthetic day equal a direct aggregation of the raw rows for every group-by, the unattributed
//! bucket included, and merged-histogram percentiles lie within one bucket of the exact ones.
use std::collections::BTreeMap;
use std::sync::LazyLock;

use phoebis_ledger::{
    account_request_counts, apply_filter, breakdown, breakdown_paged, bucket_index, bucket_seconds,
    burn_groups_paged, burn_rows, burn_series, burn_series_over, deltas_of, fold_account,
    fold_event, fold_keepalive, held_series, hour_ceil, hour_of, key_request_totals, list_requests,
    member_spends, moves_by_reason, placements, pool_traffic, pools_traffic, price_shares,
    read_account_rollups, read_rollups, refusal_categories, rewrite_series, run_retention_blocking,
    series, series_paged, session_identities, AttemptRecord, BudgetedMember, CostRevisions,
    EventRecord, FundingFilter, GroupBy, GroupRow, Histogram, InputTransformation, KeepaliveDelta,
    LedgerConfig, LedgerDb, LiveKeys, ModelPrice, ModelRow, Patterns, PlacementRow, PlanTables,
    PriceRevisions, PriceTable, RequestFilter, RequestRecord, RollupFilter, RollupRow,
    RollupSelect, SeriesFilter, SessionSpendKey, Speed, WindowShares, BILLED_FUNDING, FAST_FUNDING,
    NO_CLASS, OTHER_DIMENSION, OVERAGE_FUNDING, PAGE_HOURS, PLAN_FUNDING, UNATTRIBUTED,
    USAGE_BASED_FUNDING,
};
use phoebis_ledger::{RefusalCount, Refusals};
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
/// 2027-01-01 at 00:00 UTC, the read's instant, after every row these cases plant, so the anchor row in force then
/// divides the Pro-week figure.
const NOW: i64 = 1_798_761_600_000;

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
        .estimate(model, speed, counts)
        .expect("the shipped list carries the model")
        .total()
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
                account_kind: AccountKind::Oauth,
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
    // An earlier holder's rows filed before the generation column stay at 0, which is no creation time, and a
    // generation at 09:31:45 carries one the label leaves to `key_created_at`.
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
        GroupBy::Key,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
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
            ("k1@1000", "k1 (deleted)", Some("k1"), Some(1_000), 2),
            (
                "k1@34305000",
                "k1 (deleted)",
                Some("k1"),
                Some(34_305_000),
                1
            ),
            ("k2", "k2", Some("k2"), None, 2),
            ("-", "unattributed", Some("-"), None, 1)
        ],
        "groups sort by label, then by key where earlier holders share one. The reserved id keeps its label and no \
         suffix, every earlier holder reads deleted whatever its generation, and a key group names its holder beside \
         the label, with the creation instant for an earlier holder alone"
    );
    let agents = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        GroupBy::UserAgent,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
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
        GroupBy::Client,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
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
        .read_blocking(|c| key_request_totals(c, i64::MIN, i64::MAX, &live, &PRICES, &COSTS, NOW))
        .unwrap();
    assert_eq!(
        totals.keys().collect::<Vec<_>>(),
        vec!["k1", "k2"],
        "the keys list never carries an earlier holder's row"
    );
    assert_eq!(totals["k1"].requests, 1);
}

#[test]
fn the_model_grouping_labels_its_empty_key_no_model_on_the_breakdown_and_the_series() {
    const HOUR_MS: i64 = 3_600_000;
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, model: &str, kind: &str, outcome: &str| {
        let request = RequestRecord {
            id: id.into(),
            received_at: 1_000,
            model: model.into(),
            canonical_model: model.into(),
            kind: kind.into(),
            origin: "client".into(),
            outcome: outcome.into(),
            ..Default::default()
        };
        db.insert_request(&request, &[], None).unwrap();
    };
    plant("served", "claude-sonnet-5-5", "inference", "succeeded");
    // A request the router rejected before routing names no model.
    plant("rejected", "", "inference", "rejected");
    // An empty key under another grouping, which keeps it as its label.
    plant("unkinded", "claude-sonnet-5-5", "", "succeeded");
    let live = LiveKeys::default();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let labels = |groups: &[GroupRow]| -> Vec<(String, String)> {
        groups
            .iter()
            .map(|g| (g.key.clone(), g.label.clone()))
            .collect()
    };
    let want = vec![
        (
            "claude-sonnet-5-5".to_string(),
            "claude-sonnet-5-5".to_string(),
        ),
        (String::new(), "no model".to_string()),
    ];
    let groups = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        GroupBy::Model,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    assert_eq!(
        labels(&groups),
        want,
        "the empty key keeps its key, so a filter on it works, and reads no model, while a model reads its id"
    );
    let points = series(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        0,
        HOUR_MS,
        3_600,
        GroupBy::Model,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    )
    .unwrap();
    assert_eq!(
        points.iter().map(|p| labels(&p.groups)).collect::<Vec<_>>(),
        vec![want],
        "the series labels its groups by the same function"
    );
    let kinds = breakdown(
        &apply_filter(rows, &RollupFilter::default()),
        GroupBy::Kind,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    assert_eq!(
        labels(&kinds),
        vec![
            (String::new(), String::new()),
            ("inference".to_string(), "inference".to_string())
        ],
        "only the model grouping reads its empty key as no model"
    );
}

#[test]
fn the_classifier_grouping_labels_its_reserved_id_none_on_the_breakdown_and_the_series_while_the_key_grouping_keeps_unattributed(
) {
    const HOUR_MS: i64 = 3_600_000;
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, key: Option<&str>, classifier: Option<&str>| {
        let request = RequestRecord {
            id: id.into(),
            received_at: 1_000,
            key_id: key.map(String::from),
            model: "claude-sonnet-5-5".into(),
            canonical_model: "claude-sonnet-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            classifier: classifier.map(String::from),
            ..Default::default()
        };
        db.insert_request(&request, &[], None).unwrap();
    };
    plant("classified", Some("k"), Some("client"));
    // No classifier call, and no key the router attributed the request to.
    plant("plain", None, None);
    let live = LiveKeys::default();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let labels = |groups: &[GroupRow]| -> Vec<(String, String)> {
        groups
            .iter()
            .map(|g| (g.key.clone(), g.label.clone()))
            .collect()
    };
    let grouped = |group_by: GroupBy| {
        breakdown(
            &apply_filter(rows.clone(), &RollupFilter::default()),
            group_by,
            true,
            &live,
            &[],
            &PRICES,
            &COSTS,
            NOW,
        )
    };
    let want = vec![
        ("client".to_string(), "client".to_string()),
        (UNATTRIBUTED.to_string(), "none".to_string()),
    ];
    assert_eq!(
        labels(&grouped(GroupBy::Classifier)),
        want,
        "the reserved id keeps its key, so a filter on it works, and reads none, as the Requests facet names it"
    );
    let points = series(
        &apply_filter(rows.clone(), &RollupFilter::default()),
        0,
        HOUR_MS,
        3_600,
        GroupBy::Classifier,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    )
    .unwrap();
    assert_eq!(
        points.iter().map(|p| labels(&p.groups)).collect::<Vec<_>>(),
        vec![want],
        "the series labels its groups by the same function"
    );
    for group_by in [GroupBy::Key, GroupBy::Account] {
        let reserved = labels(&grouped(group_by))
            .into_iter()
            .find(|(key, _)| key == UNATTRIBUTED)
            .map(|(_, label)| label);
        assert_eq!(
            reserved.as_deref(),
            Some("unattributed"),
            "the {group_by:?} grouping keeps its label for the reserved id"
        );
    }
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
        user_agents: Patterns::new(vec!["claude-cli/*".into()]).unwrap(),
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
        for group_by in [
            GroupBy::All,
            GroupBy::Effort,
            GroupBy::Key,
            GroupBy::UserAgent,
        ] {
            let unpaged = series(
                &rows,
                from,
                to,
                bucket,
                group_by,
                filter.probes_counted(),
                &live,
                &filter.user_agent_groups,
                &PRICES,
                &COSTS,
                NOW,
            )
            .unwrap();
            let paged = series_paged(
                &db, &filter, &live, from, to, bucket, group_by, &PRICES, &COSTS, NOW,
            )
            .await
            .unwrap();
            assert_eq!(
                serde_json::to_value(&paged).unwrap(),
                serde_json::to_value(&unpaged).unwrap(),
                "series by {group_by:?} under {filter:?}"
            );
            let unpaged = breakdown(
                &rows,
                group_by,
                filter.probes_counted(),
                &live,
                &filter.user_agent_groups,
                &PRICES,
                &COSTS,
                NOW,
            );
            let paged = breakdown_paged(
                &db, &filter, &live, from, to, group_by, &PRICES, &COSTS, NOW,
            )
            .await
            .unwrap();
            assert_eq!(
                serde_json::to_value(&paged).unwrap(),
                serde_json::to_value(&unpaged).unwrap(),
                "breakdown by {group_by:?} under {filter:?}"
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
            // The seed's row is a fast answer, a billed probe whose dollars would keep its group in, so the planted
            // rows run on the plan.
            attempt.speed = None;
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
    let default_groups = breakdown(
        &filtered,
        GroupBy::Key,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    assert!(
        !default_groups.iter().any(|g| g.key == "k1"),
        "a group whose rows are all probes stays out of the default breakdown: {default_groups:?}"
    );
    let with_probes = breakdown(
        &filtered,
        GroupBy::Key,
        true,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    assert!(
        with_probes.iter().any(|g| g.key == "k1"),
        "the same group appears once probes count as requests: {with_probes:?}"
    );
}

/// A probe on an api_key account draws real dollars, so a breakdown that counts no probe as a request still answers
/// the group its probe alone holds, with the probe's dollars and no request. One dollar a token, so its 57 tokens cost
/// $57.
#[tokio::test]
async fn a_probes_off_breakdown_answers_a_billed_probe_s_group_with_its_dollars_and_no_request() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let model = "claude-sonnet-5-5";
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            account: "api",
            account_kind: AccountKind::ApiKey,
            kind: "probe",
            model,
            usage: usage(50, 7),
            ..ShareRow::default()
        },
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = breakdown_paged(
        &db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        from,
        to,
        GroupBy::Account,
        &priced_table(model, 1.0),
        &phoebis_ledger::test_tables::subscription_costs(),
        NOW,
    )
    .await
    .unwrap();
    let api = group(&groups, "api");
    assert_eq!(
        (api.requests, api.cost_usd, api.billed_usd),
        (0, 57.0, 57.0),
        "{api:?}"
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
        GroupBy::UserAgent,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    let group = default_groups
        .iter()
        .find(|g| g.key == UNATTRIBUTED)
        .expect("a group of keepalive credits alone stays in the default breakdown");
    assert_eq!(group.kept_warm.requests, 1);
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
    let saving = |group_by: GroupBy, key: &str| {
        breakdown(
            &rows,
            group_by,
            false,
            &LiveKeys::default(),
            &[],
            &prices,
            &COSTS,
            NOW,
        )
        .iter()
        .find(|g| g.key == key)
        .unwrap_or_else(|| panic!("a {key} group"))
        .saving_usd
    };
    let listed = 2_000.0 * 3.0 + 300.0 * 5.0 - 2_300.0 * 0.5;
    assert!(
        (saving(GroupBy::Model, LISTED) - listed).abs() < 1e-9,
        "the five-minute write and the hour's write less the read of both: {}",
        saving(GroupBy::Model, LISTED)
    );
    assert_eq!(
        saving(GroupBy::Model, "claude-unlisted-1"),
        0.0,
        "a model the revision lacks prices no saving"
    );
    assert!(
        (saving(GroupBy::All, "all") - listed).abs() < 1e-9,
        "and adds nothing beside a priced one"
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
        account_kind: AccountKind::Oauth,
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

/// A request the router refused under `key` before it asked any account, flagged as the landing flags it.
fn refusal(id: &str, key: &str) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: 3_700_000,
        finished_at: Some(3_700_004),
        key_id: Some(key.into()),
        pool_id: Some("p".into()),
        model: "m".into(),
        canonical_model: "m".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(403),
        outcome: "rejected".into(),
        attempts: 0,
        duration_ms: Some(4),
        error_code: Some("guardrail_blocked".into()),
        refused: true,
        ..Default::default()
    }
}

fn all_rows(db: &LedgerDb) -> Vec<RollupRow> {
    db.read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap()
}

/// A row's key with the figures a reader sums for requests: requests, rejected, uncached requests, the errors map,
/// the duration histogram's count and the refusals map.
type Figures = (
    String,
    u64,
    u64,
    u64,
    BTreeMap<String, u64>,
    u64,
    BTreeMap<String, u64>,
);

fn request_figures(row: &RollupRow) -> Figures {
    (
        row.key_id.clone(),
        row.requests,
        row.rejected,
        row.uncached_requests,
        row.errors.clone(),
        row.duration_hist.count(),
        row.refusals.clone(),
    )
}

fn blocked(n: u64) -> BTreeMap<String, u64> {
    BTreeMap::from([("guardrail_blocked".to_string(), n)])
}

#[test]
fn a_flagged_request_folds_into_its_hours_refusals_map_and_under_no_other_figure() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(&refusal("r1", "k"), &[], None).unwrap();
    let rows = all_rows(&db);
    assert_eq!(rows.len(), 1, "one row for the refused request's hour");
    assert_eq!(rows[0].hour, hour_of(3_700_000));
    assert_eq!(
        request_figures(&rows[0]),
        ("k".to_string(), 0, 0, 0, BTreeMap::new(), 0, blocked(1)),
        "the refusal sits in the map alone: no request, no rejected, no uncached request, no error, no duration"
    );
}

/// A request on `claude-sonnet-5` served on `account`, its one attempt answering `stop_reason` with `category`.
fn stop_rows(
    id: &str,
    received_at: i64,
    account: &str,
    stop_reason: &str,
    category: Option<&str>,
) -> (RequestRecord, AttemptRecord) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        finished_at: Some(received_at + 500),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: account.into(),
        account_kind: AccountKind::Oauth,
        upstream_status: Some(200),
        outcome: "succeeded".into(),
        stop_reason: Some(stop_reason.into()),
        refusal_category: category.map(String::from),
        ..Default::default()
    };
    (request, attempt)
}

fn plant_stop(
    db: &LedgerDb,
    id: &str,
    received_at: i64,
    account: &str,
    stop_reason: &str,
    category: Option<&str>,
) {
    let (request, attempt) = stop_rows(id, received_at, account, stop_reason, category);
    db.insert_request(&request, &[attempt], None).unwrap();
}

/// A made-up SHA-256 in lowercase hex, the form the digest columns hold.
const FALLBACK_DIGEST: &str = "3b7e1f9a5c2d8e4b6a0f7c3d9e1b5a8f2c6d0e4a7b9f1c3e5d8a2b6f0c4e7a9d";

/// A request served on `account` with `end_turn` that presented the fallback credit `FALLBACK_DIGEST` names, its
/// client label and `User-Agent` header kept as sent.
fn plant_fallback(
    db: &LedgerDb,
    id: &str,
    received_at: i64,
    account: &str,
    client: Option<&str>,
    user_agent: Option<&str>,
) {
    let (request, attempt) = stop_rows(id, received_at, account, "end_turn", None);
    let request = RequestRecord {
        client: client.map(String::from),
        user_agent: user_agent.map(String::from),
        fallback_digest: Some(FALLBACK_DIGEST.into()),
        ..request
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

/// Refusal rows from `(model, category, requests)` triples.
fn refusal_rows(rows: &[(&str, &str, u64)]) -> Vec<RefusalCount> {
    rows.iter()
        .map(|(model, category, requests)| RefusalCount {
            model: model.to_string(),
            category: category.to_string(),
            requests: *requests,
        })
        .collect()
}

#[test]
fn the_refusal_count_reads_each_served_refusal_in_range_by_model_and_category_and_the_fallbacks_in_range(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_stop(&db, "h1", 1_000, "a", "refusal", Some("harmful_request"));
    plant_stop(&db, "h2", 2_000, "a", "refusal", Some("harmful_request"));
    plant_stop(&db, "t1", 3_000, "a", "refusal", Some("restricted_topic"));
    plant_stop(&db, "n1", 4_000, "a", "refusal", None);
    // Left out: two answers of another stop reason, and a refusal past the range.
    plant_stop(&db, "e1", 5_000, "a", "end_turn", None);
    plant_stop(&db, "u1", 6_000, "a", "tool_use", None);
    plant_stop(
        &db,
        "late",
        3_620_000,
        "a",
        "refusal",
        Some("harmful_request"),
    );
    // Another account's refusal, which the account filter leaves out.
    plant_stop(&db, "b1", 7_000, "b", "refusal", Some("restricted_topic"));
    // A refusal on a second model under the category of the first model's two.
    let (request, attempt) = stop_rows("o1", 8_000, "a", "refusal", Some("harmful_request"));
    let request = RequestRecord {
        canonical_model: "claude-opus-5".into(),
        model: "claude-opus-5".into(),
        ..request
    };
    db.insert_request(&request, &[attempt], None).unwrap();
    // Requests that presented a fallback credit: one in range, one past it, and one in range on the other account.
    plant_fallback(&db, "f1", 9_000, "a", None, None);
    plant_fallback(&db, "f-late", 3_621_000, "a", None, None);
    plant_fallback(&db, "f-b", 9_500, "b", None, None);
    let count = |accounts: &[&str]| {
        let filter = RequestFilter {
            accounts: accounts.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };
        db.read_blocking(|c| refusal_categories(c, &filter, &LiveKeys::default(), 0, 10_000))
            .unwrap()
    };
    assert_eq!(
        count(&["a"]),
        Refusals {
            rows: refusal_rows(&[
                ("claude-sonnet-5", "harmful_request", 2),
                ("claude-opus-5", "harmful_request", 1),
                ("claude-sonnet-5", "-", 1),
                ("claude-sonnet-5", "restricted_topic", 1),
            ]),
            fallbacks: 1,
        },
        "one row per model and category, the largest count first, then the model and the category in byte order, \
         `-` for the refusal that named none, and the account's one fallback in range"
    );
    assert_eq!(
        count(&[]),
        Refusals {
            rows: refusal_rows(&[
                ("claude-sonnet-5", "harmful_request", 2),
                ("claude-sonnet-5", "restricted_topic", 2),
                ("claude-opus-5", "harmful_request", 1),
                ("claude-sonnet-5", "-", 1),
            ]),
            fallbacks: 2,
        },
        "without the account filter the other account's refusal joins its model and category, and its fallback \
         joins the count"
    );
}

/// A range from 21:31 to 22:10 on the epoch's first day, which touches the hours 21:00 and 22:00.
const WINDOW_FROM: i64 = 77_460_000;
const WINDOW_TO: i64 = 79_800_000;
/// 20:59, in the hour before the one that holds the window's start.
const BEFORE_THE_FIRST_HOUR: i64 = 75_540_000;
/// 21:10, before the start in the hour that holds it.
const IN_THE_FIRST_HOUR: i64 = 76_200_000;
/// 22:40, past the end in the hour that holds it.
const IN_THE_LAST_HOUR: i64 = 81_600_000;
/// 23:00, the hour ceiling of the window's end.
const AT_THE_HOUR_CEILING: i64 = 82_800_000;

/// The refusals of the whole hours a range touches count, from the hour that holds its start up to and not at the hour
/// ceiling of its end.
#[test]
fn the_refusal_count_reads_the_whole_hours_a_range_touches_up_to_the_hour_ceiling_of_its_end() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_stop(
        &db,
        "before",
        BEFORE_THE_FIRST_HOUR,
        "a",
        "refusal",
        Some("harmful_request"),
    );
    plant_stop(
        &db,
        "first-hour",
        IN_THE_FIRST_HOUR,
        "a",
        "refusal",
        Some("harmful_request"),
    );
    plant_stop(
        &db,
        "last-hour",
        IN_THE_LAST_HOUR,
        "a",
        "refusal",
        Some("harmful_request"),
    );
    plant_stop(
        &db,
        "ceiling",
        AT_THE_HOUR_CEILING,
        "a",
        "refusal",
        Some("harmful_request"),
    );
    let refusals = db
        .read_blocking(|c| {
            refusal_categories(
                c,
                &RequestFilter::default(),
                &LiveKeys::default(),
                WINDOW_FROM,
                WINDOW_TO,
            )
        })
        .unwrap();
    assert_eq!(
        refusals.rows,
        refusal_rows(&[("claude-sonnet-5", "harmful_request", 2)]),
        "the refusals at 21:10 and 22:40 count, the ones at 20:59 and 23:00 do not"
    );
}

const CLI_310: &str = "claude-cli/2.1.310 (external, cli)";
const SDK_310: &str = "claude-cli/2.1.310 (external, sdk-cli)";
const CLI_311: &str = "claude-cli/2.1.311 (external, cli)";

/// A refusal under `category`, its request row keeping the caller's client label and `User-Agent` header as sent,
/// `None` where the caller sent none.
fn plant_caller_refusal(
    db: &LedgerDb,
    id: &str,
    client: Option<&str>,
    user_agent: Option<&str>,
    category: Option<&str>,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at: 1_000,
        finished_at: Some(1_500),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        client: client.map(String::from),
        user_agent: user_agent.map(String::from),
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        upstream_status: Some(200),
        outcome: "succeeded".into(),
        stop_reason: Some("refusal".into()),
        refusal_category: category.map(String::from),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

/// Four refusals: version 2.1.310 under the `cli` and the `sdk-cli` entrypoint, 2.1.311 under `cli`, and a caller
/// that sent neither a client label nor a user agent.
fn plant_callers(db: &LedgerDb) {
    plant_caller_refusal(
        db,
        "cli-310",
        Some("claude-code/cli"),
        Some(CLI_310),
        Some("harmful_request"),
    );
    plant_caller_refusal(
        db,
        "sdk-310",
        Some("claude-code/sdk-cli"),
        Some(SDK_310),
        Some("harmful_request"),
    );
    plant_caller_refusal(
        db,
        "cli-311",
        Some("claude-code/cli"),
        Some(CLI_311),
        Some("restricted_topic"),
    );
    plant_caller_refusal(db, "bare", None, None, None);
}

/// The count over the planted range under the two pattern lists.
fn count_under(db: &LedgerDb, clients: &[&str], user_agents: &[&str]) -> Refusals {
    let words =
        |list: &[&str]| Patterns::new(list.iter().map(|w| w.to_string()).collect()).unwrap();
    let filter = RequestFilter {
        clients: words(clients),
        user_agents: words(user_agents),
        ..Default::default()
    };
    db.read_blocking(|c| refusal_categories(c, &filter, &LiveKeys::default(), 0, 10_000))
        .unwrap()
}

/// The planted callers' rows, every refusal on `claude-sonnet-5`.
fn category_counts(rows: &[(&str, u64)]) -> Vec<RefusalCount> {
    rows.iter()
        .map(|(category, requests)| RefusalCount {
            model: "claude-sonnet-5".into(),
            category: category.to_string(),
            requests: *requests,
        })
        .collect()
}

#[test]
fn the_refusal_count_narrows_by_a_client_pattern_on_the_label_the_rollups_fold() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_callers(&db);
    assert_eq!(
        count_under(&db, &["claude-code/cli"], &[]).rows,
        category_counts(&[("harmful_request", 1), ("restricted_topic", 1)]),
        "a label spelled in full counts that label's refusals alone"
    );
    assert_eq!(
        count_under(&db, &["claude-code/*"], &[]).rows,
        category_counts(&[("harmful_request", 2), ("restricted_topic", 1)]),
        "a wildcard counts every label it matches, the caller with none left out"
    );
    assert_eq!(
        count_under(&db, &["-"], &[]).rows,
        category_counts(&[("-", 1)]),
        "a row with no client reads `-`, as the rollups file it"
    );
    plant_fallback(
        &db,
        "sdk-fallback",
        1_000,
        "a",
        Some("claude-code/sdk-cli"),
        Some(SDK_310),
    );
    assert_eq!(
        (
            count_under(&db, &["claude-code/cli"], &[]).fallbacks,
            count_under(&db, &["claude-code/*"], &[]).fallbacks,
        ),
        (0, 1),
        "the fallbacks narrow by the client pattern as the refusals do"
    );
}

#[test]
fn the_refusal_count_matches_a_user_agent_pattern_against_the_agents_first_product_token() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_callers(&db);
    assert_eq!(
        count_under(&db, &[], &["claude-cli/2.1.310"]).rows,
        category_counts(&[("harmful_request", 2)]),
        "the token spelled in full counts the version's refusals under both entrypoints and none of 2.1.311's"
    );
    assert_eq!(
        count_under(&db, &[], &["claude-cli/2.1.*"]).rows,
        category_counts(&[("harmful_request", 2), ("restricted_topic", 1)]),
        "a wildcard over the token counts every version it matches"
    );
    assert_eq!(
        count_under(&db, &[], &[CLI_310]).rows,
        Vec::new(),
        "the header with its comment is no token, so it counts nothing, as it names no group of the series"
    );
}

#[test]
fn the_refusal_count_counts_the_rows_either_pattern_of_a_list_matches() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_callers(&db);
    assert_eq!(
        count_under(&db, &[], &["claude-cli/2.1.311", "-"]).rows,
        category_counts(&[("-", 1), ("restricted_topic", 1)]),
        "two user agent patterns count the rows of either, the 2.1.310 rows left out"
    );
    assert_eq!(
        count_under(&db, &["claude-code/sdk-cli", "-"], &[]).rows,
        category_counts(&[("-", 1), ("harmful_request", 1)]),
        "two client patterns count the rows of either, the `cli` rows left out"
    );
}

#[test]
fn the_refusal_count_counts_a_row_only_where_the_client_and_the_user_agent_lists_both_match() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_callers(&db);
    assert_eq!(
        count_under(&db, &["claude-code/cli"], &["claude-cli/2.1.310"]).rows,
        category_counts(&[("harmful_request", 1)]),
        "one row holds both: the sdk-cli row fails the client list and the 2.1.311 row the user agent list"
    );
}

#[test]
fn a_rejection_after_an_attempt_folds_as_a_rejected_request_as_before() {
    let db = LedgerDb::open_in_memory().unwrap();
    let request = RequestRecord {
        status: Some(403),
        outcome: "rejected".into(),
        attempts: 1,
        duration_ms: Some(4),
        error_code: Some("client_not_allowed".into()),
        refused: false,
        ..replaced("r1")
    };
    let attempt = AttemptRecord {
        request_id: "r1".into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        started_at: 3_700_000,
        outcome: "failed".into(),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
    let rows = all_rows(&db);
    assert_eq!(rows.len(), 1, "one row for the attempt's account");
    assert_eq!(
        (
            rows[0].requests,
            rows[0].rejected,
            rows[0].errors.clone(),
            rows[0].refusals.clone()
        ),
        (
            1,
            1,
            BTreeMap::from([("client_not_allowed".to_string(), 1)]),
            BTreeMap::new()
        ),
        "an account was asked, so the rejection counts as a request and an error"
    );
}

#[test]
fn a_replaced_flagged_request_leaves_its_old_row_and_a_row_of_refusals_alone_is_kept() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(&refusal("r1", "k"), &[], None).unwrap();
    db.insert_request(&refusal("r1", "k2"), &[], None).unwrap();
    assert_eq!(
        all_rows(&db)
            .iter()
            .map(request_figures)
            .collect::<Vec<_>>(),
        [("k2".to_string(), 0, 0, 0, BTreeMap::new(), 0, blocked(1))],
        "the old row held the refusal alone and goes, the new row holds the map alone and stays"
    );

    let db = LedgerDb::open_in_memory().unwrap();
    // Landed without an attempt row, the served request files under the unattributed account the refusal's row names:
    // a request with an attempt files under that attempt's account, a row of its own.
    db.insert_request(&replaced("s1"), &[], None).unwrap();
    db.insert_request(&refusal("r1", "k"), &[], None).unwrap();
    assert_eq!(
        all_rows(&db)
            .iter()
            .map(|r| (r.key_id.as_str(), r.requests, r.refusals.clone()))
            .collect::<Vec<_>>(),
        [("k", 1, blocked(1))],
        "the served request and the refusal share the row before the replace"
    );
    db.insert_request(&refusal("r1", "k2"), &[], None).unwrap();
    let rows = all_rows(&db);
    let row = |key: &str| {
        rows.iter()
            .find(|r| r.key_id == key)
            .unwrap_or_else(|| panic!("a row under {key}: {rows:?}"))
    };
    assert_eq!(
        (
            row("k").requests,
            row("k").succeeded,
            row("k").refusals.clone()
        ),
        (1, 1, BTreeMap::new()),
        "the row keeps the served request and gives the refusal up"
    );
    assert_eq!(row("k2").refusals, blocked(1));
    assert_eq!(rows.len(), 2);
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
        GroupBy::RequestClass,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
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
                account_kind: AccountKind::Oauth,
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
    let groups = breakdown(
        &filtered,
        GroupBy::Speed,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    assert_eq!(
        groups
            .iter()
            .map(|g| (g.key.as_str(), g.requests))
            .collect::<Vec<_>>(),
        vec![("fast", 2), ("standard", 2)],
        "a named standard and a silent answer share one group"
    );
}

/// The rows carry the canonical model, the router's fold of a request's `model`, so a filter folds its value the same
/// way: the `[1m]` spelling names the model it stands for.
#[test]
fn a_model_filter_in_the_long_context_spelling_lists_its_canonical_models_requests() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, model) in [("r-sonnet", "claude-sonnet-5"), ("r-opus", "claude-opus-5")] {
        let request = RequestRecord {
            id: id.into(),
            received_at: 1_000,
            finished_at: Some(1_500),
            canonical_model: model.into(),
            model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            upstream_status: Some(200),
            outcome: "succeeded".into(),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    }
    let filter = RequestFilter {
        models: vec!["claude-sonnet-5[1m]".into()],
        ..Default::default()
    };
    let listed: Vec<String> = db
        .read_blocking(|c| {
            list_requests(
                c,
                &filter,
                &LiveKeys::default(),
                phoebis_ledger::RequestOrder::ReceivedAt,
                phoebis_ledger::Direction::Desc,
                None,
                10,
            )
        })
        .unwrap()
        .0
        .into_iter()
        .map(|(r, _)| r.id)
        .collect();
    assert_eq!(listed, vec!["r-sonnet"]);
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
        db.read_blocking(|c| {
            list_requests(
                c,
                &filter,
                &LiveKeys::default(),
                phoebis_ledger::RequestOrder::ReceivedAt,
                phoebis_ledger::Direction::Desc,
                None,
                10,
            )
        })
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
    account_kind: AccountKind,
    kind: &'a str,
    model: &'a str,
    at: i64,
    usage: PinUsage,
    overage: Option<bool>,
    speed: Option<&'a str>,
    client: Option<&'a str>,
    tier: Option<&'a str>,
    classifier: Option<&'a str>,
    usage_based: bool,
}

impl Default for ShareRow<'_> {
    fn default() -> Self {
        ShareRow {
            id: "r",
            account: "a",
            account_kind: AccountKind::Oauth,
            kind: "inference",
            model: "claude-fable-5-1",
            at: WEEKDAY_OFF_PEAK,
            usage: usage(1_000, 100),
            overage: None,
            speed: None,
            client: None,
            tier: Some("test_tier"),
            classifier: None,
            usage_based: false,
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
            account_kind: row.account_kind,
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
                .filter(|_| row.account_kind != AccountKind::ApiKey)
                .map(String::from),
            usage_based: row.usage_based,
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

/// The account-grouped breakdown of `[from, to)` under `filter`, its shares and figures priced on `tables`.
async fn account_groups(
    db: &std::sync::Arc<LedgerDb>,
    filter: &RollupFilter,
    from: i64,
    to: i64,
    tables: &PlanTables,
) -> Vec<GroupRow> {
    breakdown_paged(
        db,
        filter,
        &LiveKeys::default(),
        from,
        to,
        GroupBy::Account,
        &PRICES,
        tables,
        NOW,
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
    price_shares(tables, None, "test_tier", "claude-fable-5-1", usage, at, at)
}

#[tokio::test]
async fn a_breakdown_row_under_the_account_grouping_sums_its_plan_rows_shares() {
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
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, &tables).await;
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
    assert_eq!(
        (a.plan_requests, a.plan_output_tokens),
        (2, 150),
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
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, &tables).await;
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
    )[FIVE_HOUR]
        .unwrap();
    assert_eq!(
        cost.keys().copied().collect::<Vec<_>>(),
        [FIVE_HOUR],
        "a tier sized on the 5-hour window alone answers neither week: {cost:?}"
    );
    assert!(close(cost.get(FIVE_HOUR), want), "{cost:?} vs {want}");
}

/// One account's rows in one hour under two tiers, its profile read again between them: a Fable row on `test_tier`,
/// which the share cost model sizes, and a Sonnet row on a tier the model does not list.
#[tokio::test]
async fn a_group_holding_a_sized_and_an_unsized_row_of_one_account_answers_the_unsized_rows_windows_unpriced(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-sized",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-unsized",
            model: "claude-sonnet-5-5",
            tier: Some("unlisted_tier"),
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, &tables).await;
    let cost = group(&groups, "a")
        .cost_shares
        .as_ref()
        .expect("an oauth group with a tier answers its shares");
    assert_eq!(
        cost.keys().copied().collect::<Vec<_>>(),
        [SEVEN_DAY_FABLE],
        "the Sonnet row's two windows read unpriced, the model week only the Fable row keys stays priced: {cost:?}"
    );
}

/// A weighted plan row of an account whose tier the test cost model sizes, an unweighted plan row of a second account
/// and a billed row of a third: the day, the pool's traffic and the breakdown answer the same dollars and Pro-week
/// figure whether or not the read answers the shares, each figure the test tables' own to the bit. The breakdown of
/// every row as one group answers no shares, and the account breakdown, whose groups each hold one account's rows,
/// answers them.
#[tokio::test]
async fn a_dollar_read_and_a_shares_read_of_the_same_rows_answer_one_set_of_dollars_and_figures() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let prices = phoebis_ledger::test_tables::api_prices();
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let at = phoebis_ledger::test_tables::JANUARY_2026;
    for (id, account, account_kind, model, counts) in [
        (
            "r-plan",
            "a",
            AccountKind::Oauth,
            "claude-opus-5-5",
            usage(10, 1),
        ),
        // Opus 5 has no weight in the test cost model, so it prices nothing whatever its model tier weighs.
        (
            "r-unweighted",
            "c",
            AccountKind::Oauth,
            "claude-opus-5",
            usage(10, 0),
        ),
        (
            "r-billed",
            "b",
            AccountKind::ApiKey,
            "claude-opus-5-5",
            usage(20, 0),
        ),
    ] {
        plant_share(
            &db,
            ShareRow {
                id,
                account,
                account_kind,
                model,
                at,
                usage: counts,
                tier: Some(PRO),
                ..ShareRow::default()
            },
        );
    }
    let hour = hour_of(at);
    let (from, to) = hour_range(at);
    let day = {
        let (prices, costs) = (prices.clone(), costs.clone());
        db.read(move |c| phoebis_ledger::dashboard_on(c, at, &prices, &costs))
            .await
            .unwrap()
    };
    let traffic = phoebis_ledger::pool_traffic(
        &db,
        "p",
        hour,
        hour + 3_600,
        3_600,
        std::sync::Arc::new(prices.clone()),
        std::sync::Arc::new(costs.clone()),
        NOW,
    )
    .await
    .unwrap();
    let dollar_read = breakdown_paged(
        &db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        from,
        to,
        GroupBy::All,
        &prices,
        &costs,
        NOW,
    )
    .await
    .unwrap();
    let shares_read = breakdown_paged(
        &db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        from,
        to,
        GroupBy::Account,
        &prices,
        &costs,
        NOW,
    )
    .await
    .unwrap();
    let of_group = |groups: &[GroupRow], key: &str| {
        let g = group(groups, key);
        (g.cost_usd, g.billed_usd, g.cost_equivalent)
    };
    let figures = [
        (
            "the day",
            (day.cost_24h, day.billed_24h, day.equivalent_24h),
        ),
        (
            "the pool's bucket",
            (
                traffic[0].cost_usd,
                traffic[0].billed_usd,
                traffic[0].equivalent,
            ),
        ),
        (
            "every row without the shares",
            of_group(&dollar_read, "all"),
        ),
        ("a with the shares", of_group(&shares_read, "a")),
        ("b with the shares", of_group(&shares_read, "b")),
        ("c with the shares", of_group(&shares_read, "c")),
    ];
    // Account a's Opus 5.5 row weighs 40 units, 10 input tokens and 1 output token at weight 2, 0.8 of the Pro week's
    // 50, and $0.00015, its two cells' sum one bit above. Account b's 20 input tokens of Opus 5.5 are billed, $0.0002,
    // and its figure reads the zero of a group with no plan row. Account c's Opus 5 row carries tokens and no weight,
    // so c's figure and the day's read none beside their dollars, $0.00005 for c and $0.0004 for the day, one bit
    // above. The pool's bucket adds the three as its grouped sum orders them, c's model first and the billed row
    // before the plan row, which lands on $0.0004 itself.
    let all = (0.0004000000000000001, 0.0002, None);
    let pool = (0.0004, 0.0002, None);
    let a = (0.00015000000000000001, 0.0, Some(0.8));
    let b = (0.0002, 0.0002, Some(0.0));
    let c = (0.00005, 0.0, None);
    assert_eq!(
        figures,
        [
            ("the day", all),
            ("the pool's bucket", pool),
            ("every row without the shares", all),
            ("a with the shares", a),
            ("b with the shares", b),
            ("c with the shares", c),
        ]
    );
    assert_eq!(group(&dollar_read, "all").cost_shares, None);
    let shares = |key: &str| -> Vec<(&str, f64)> {
        group(&shares_read, key)
            .cost_shares
            .as_ref()
            .expect("the shares read prices the sized tier")
            .iter()
            .map(|(window, share)| (*window, *share))
            .collect()
    };
    assert_eq!(
        shares("a"),
        [(FIVE_HOUR, 8.0), (SEVEN_DAY, 0.8)],
        "the 40 units over Pro's windows of 5 and 50 units"
    );
    assert_eq!(
        shares("c"),
        [],
        "the unweighted row leaves both of Pro's windows unpriced"
    );
}

/// The ledger's test cost model: `claude-mystery-2` sits in a tier that weighs no model, `claude-unlisted-1` in no tier
/// and `claude-opus-5` carries a null weight, so none of them prices on any window, and `claude-sonnet-5-5` prices at
/// its own weight.
fn test_costs() -> PlanTables {
    phoebis_ledger::test_tables::subscription_costs()
}

/// Every planted row's group under `group_by`, priced in the equivalent on `test_costs`, the probe rows joining the
/// request side where `probes_counted`.
fn test_cost_groups(db: &LedgerDb, group_by: GroupBy, probes_counted: bool) -> Vec<GroupRow> {
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    breakdown(
        &rows,
        group_by,
        probes_counted,
        &LiveKeys::default(),
        &[],
        &PRICES,
        &test_costs(),
        NOW,
    )
}

#[test]
fn a_plan_row_on_a_model_the_cost_model_cannot_weigh_nulls_its_groups_figure() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, account, model) in [
        ("r-priced", "a", "claude-sonnet-5-5"),
        ("r-tierless", "a", "claude-unlisted-1"),
        ("r-unweighted-tier", "b", "claude-mystery-2"),
    ] {
        plant_share(
            &db,
            ShareRow {
                id,
                account,
                model,
                ..ShareRow::default()
            },
        );
    }
    let groups = test_cost_groups(&db, GroupBy::Account, false);
    for (key, why) in [
        ("a", "a model with no tier"),
        ("b", "a tier with no weighted model"),
    ] {
        let g = group(&groups, key);
        assert_eq!(
            (g.cost_equivalent, &g.cache_equivalent),
            (None, &None),
            "{why} leaves the group's figure unpriced: {g:?}"
        );
    }
}

#[test]
fn a_plan_probe_the_cost_model_cannot_weigh_nulls_the_probe_figure_and_not_the_request_figure() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_share(
        &db,
        ShareRow {
            id: "r-request",
            model: "claude-sonnet-5-5",
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-probe",
            kind: "probe",
            model: "claude-mystery-2",
            ..ShareRow::default()
        },
    );
    let groups = test_cost_groups(&db, GroupBy::Account, false);
    let a = group(&groups, "a");
    assert!(
        a.cost_equivalent.is_some() && a.probe_equivalent.is_none(),
        "the request row priced and the probe row did not: {a:?}"
    );
    let counted = test_cost_groups(&db, GroupBy::Account, true);
    assert_eq!(
        group(&counted, "a").cost_equivalent,
        None,
        "a probe row the request side counts nulls the figure there too"
    );
}

#[test]
fn a_row_on_a_model_without_a_weight_nulls_the_figure_it_joins_alone() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Opus 5 carries a null weight: on `a` the probe runs on it, on `b` the request does, and on `c` a stopped probe's
    // wasted counts do.
    for (id, account, kind, model) in [
        ("r-a-request", "a", "inference", "claude-sonnet-5-5"),
        ("r-a-probe", "a", "probe", "claude-opus-5"),
        ("r-b-request", "b", "inference", "claude-opus-5"),
        ("r-b-probe", "b", "probe", "claude-sonnet-5-5"),
        ("r-c-request", "c", "inference", "claude-sonnet-5-5"),
        ("r-d-request", "d", "inference", "claude-sonnet-5-5"),
    ] {
        plant_share(
            &db,
            ShareRow {
                id,
                account,
                kind,
                model,
                ..ShareRow::default()
            },
        );
    }
    // On `d` a credit runs on it: a credit files under its request's kind, never a probe's.
    db.write(|c| {
        let tx = c.transaction()?;
        for (account, kind, wasted, kept) in [("c", "probe", 500, 0), ("d", "inference", 0, 2_000)]
        {
            fold_keepalive(
                &tx,
                &KeepaliveDelta {
                    at: WEEKDAY_OFF_PEAK,
                    key_id: "k".into(),
                    pool_id: "p".into(),
                    account_id: account.into(),
                    canonical_model: "claude-opus-5".into(),
                    kind: kind.into(),
                    wasted_probes: u64::from(wasted > 0),
                    wasted_cache_read_tokens: wasted,
                    kept_warm_requests: u64::from(kept > 0),
                    kept_warm_5m_tokens: kept,
                    ..Default::default()
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let groups = test_cost_groups(&db, GroupBy::Account, false);
    let priced = |key: &str| {
        let g = group(&groups, key);
        (
            g.cost_equivalent.is_some(),
            g.probe_equivalent.is_some(),
            g.saving_equivalent.is_some(),
        )
    };
    assert_eq!(
        priced("a"),
        (true, false, true),
        "the probe nulls the probe figure alone"
    );
    assert_eq!(
        priced("b"),
        (false, true, true),
        "the request nulls the group's figure alone"
    );
    assert_eq!(
        priced("c"),
        (true, false, true),
        "the wasted counts are the probe row's own"
    );
    assert_eq!(
        priced("d"),
        (true, true, false),
        "the credit nulls the saving alone"
    );
}

#[test]
fn the_kept_warm_and_probe_counts_count_what_a_plan_account_served_apart() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Two plan probes and a request on `a`, and a probe through the API key account `k`.
    for (id, account, account_kind, kind) in [
        ("r-request", "a", AccountKind::Oauth, "inference"),
        ("r-probe-1", "a", AccountKind::Oauth, "probe"),
        ("r-probe-2", "a", AccountKind::Oauth, "probe"),
        ("r-probe-billed", "k", AccountKind::ApiKey, "probe"),
    ] {
        plant_share(
            &db,
            ShareRow {
                id,
                account,
                account_kind,
                kind,
                model: "claude-sonnet-5-5",
                ..ShareRow::default()
            },
        );
    }
    // Two plan credits at either TTL, one in overage and one through the API key account.
    db.write(|c| {
        let tx = c.transaction()?;
        for (account, account_kind, overage, five_minutes, hour) in [
            ("a", None, false, 1_000, 0),
            ("a", None, false, 0, 400),
            ("a", None, true, 200, 0),
            ("k", Some(phoebis_store::AccountKind::ApiKey), false, 300, 0),
        ] {
            fold_keepalive(
                &tx,
                &KeepaliveDelta {
                    at: WEEKDAY_OFF_PEAK,
                    key_id: "k".into(),
                    pool_id: "p".into(),
                    account_id: account.into(),
                    canonical_model: "claude-sonnet-5-5".into(),
                    kind: "inference".into(),
                    kept_warm_requests: 1,
                    kept_warm_5m_tokens: five_minutes,
                    kept_warm_1h_tokens: hour,
                    account_kind,
                    overage_in_use: overage,
                    ..Default::default()
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let groups = test_cost_groups(&db, GroupBy::Model, false);
    let g = group(&groups, "claude-sonnet-5-5");
    let kept = &g.kept_warm;
    assert_eq!(
        (kept.requests, kept.tokens_5m, kept.tokens_1h),
        (4, 1_500, 400),
        "{kept:?}"
    );
    assert_eq!(
        (kept.plan_requests, kept.plan_tokens_5m, kept.plan_tokens_1h),
        (2, 1_000, 400),
        "the plan-served credits alone: {kept:?}"
    );
    assert_eq!(
        (g.probes.ok, g.probes.plan_ok),
        (3, 2),
        "the answered probes and the plan-served ones: {:?}",
        g.probes
    );
}

/// The two shared windows of every reader that sums them over the hour of `WEEKDAY_OFF_PEAK`, priced on `costs`.
async fn shared_sums(
    db: &std::sync::Arc<LedgerDb>,
    costs: &PlanTables,
) -> Vec<(&'static str, Option<f64>)> {
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let live = LiveKeys(BTreeMap::from([("k".to_string(), 0)]));
    let day_costs = costs.clone();
    let day = db
        .read(move |c| phoebis_ledger::dashboard_on(c, WEEKDAY_OFF_PEAK, &PRICES, &day_costs))
        .await
        .unwrap();
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour")
        .equivalent;
    let traffic = phoebis_ledger::pool_traffic(
        db,
        "p",
        hour,
        hour + 3_600,
        3_600,
        std::sync::Arc::new(PRICES.clone()),
        std::sync::Arc::new(costs.clone()),
        NOW,
    )
    .await
    .unwrap();
    let keys = db
        .read_blocking(|c| key_request_totals(c, hour, hour + 3_600, &live, &PRICES, costs, NOW))
        .unwrap();
    vec![
        ("the day", day.equivalent_24h),
        ("the hour", point),
        ("the pool's bucket", traffic[0].equivalent),
        ("the key's totals", keys["k"].equivalent),
    ]
}

/// A billed row's figure is `None` as an unpriced plan row's is, since dollars fund it, so a sum that nulled on any
/// `None` would null a mixed day. A billed row with tokens beside a priced plan row reads the plan row's figure,
/// billed rows alone read zero, and a plan row on Opus 5, a null weight, reads none.
#[tokio::test]
async fn a_billed_row_with_tokens_adds_nothing_to_the_figure_and_a_billed_only_group_reads_zero() {
    let costs = test_costs();
    let at = hour_range(WEEKDAY_OFF_PEAK).0;
    let plan_figure = costs
        .equivalent_at(
            SEVEN_DAY,
            None,
            "claude-sonnet-5-5",
            usage(1_000, 100),
            at,
            at,
            NOW,
        )
        .expect("Sonnet 5.5 is weighted");
    let billed = || ShareRow {
        id: "r-billed",
        account: "k-account",
        account_kind: AccountKind::ApiKey,
        model: "claude-sonnet-5-5",
        ..ShareRow::default()
    };
    for (rows, want, what) in [
        (
            vec![
                billed(),
                ShareRow {
                    id: "r-plan",
                    model: "claude-sonnet-5-5",
                    ..ShareRow::default()
                },
            ],
            Some(plan_figure),
            "a billed row beside a priced plan row",
        ),
        (vec![billed()], Some(0.0), "a billed row alone"),
        (
            vec![
                billed(),
                ShareRow {
                    id: "r-unweighted",
                    model: "claude-opus-5",
                    ..ShareRow::default()
                },
            ],
            None,
            "a billed row beside a plan row on a model without a weight",
        ),
    ] {
        let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
        for row in rows {
            plant_share(&db, row);
        }
        for (sum_of, sum) in shared_sums(&db, &costs).await {
            assert_eq!(sum, want, "{sum_of} over {what}");
        }
    }
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
    );
    for order in ["as read", "reversed"] {
        let groups = breakdown(
            &rows,
            GroupBy::Account,
            false,
            &LiveKeys::default(),
            &[],
            &PRICES,
            &tables,
            NOW,
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
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, &tables).await;
    let a = group(&groups, "a");
    assert!(
        a.cost_shares.as_ref().is_some_and(|m| m.is_empty()),
        "the weightless row prices on no window, so the week and the shared windows are absent alike: {a:?}"
    );
    assert!(
        a.cost_equivalent.is_none() && a.equivalent_split.is_none(),
        "and the figure latches as the shares do rather than reading the priced row alone: {a:?}"
    );
    assert_eq!(a.plan_requests, 2, "both plan rows were summed over: {a:?}");
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
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let inference_share = fable_shares(&tables, inference)[FIVE_HOUR].unwrap();
    let probe_share = fable_shares(&tables, probe)[FIVE_HOUR].unwrap();
    let read = |filter: RollupFilter| {
        let (db, tables) = (db.clone(), &tables);
        async move {
            let groups = account_groups(&db, &filter, from, to, tables).await;
            let a = group(&groups, "a");
            (
                a.cost_shares
                    .as_ref()
                    .and_then(|m| m.get(FIVE_HOUR))
                    .copied(),
                a.plan_requests,
            )
        }
    };
    let (cost, requests) = read(RollupFilter::default()).await;
    assert!(
        close(cost.as_ref(), inference_share) && requests == 1,
        "with probes left out the request shares read the inference row alone: {cost:?}, {requests:?}"
    );
    let (cost, requests) = read(with_probes()).await;
    assert!(
        close(cost.as_ref(), inference_share + probe_share) && requests == 2,
        "with probes counted the probe row joins the request shares: {cost:?}, {requests:?}"
    );
    let (cost, requests) = read(RollupFilter {
        kinds: vec!["probe".into()],
        ..RollupFilter::default()
    })
    .await;
    assert!(
        close(cost.as_ref(), probe_share) && requests == 1,
        "under a probe kind filter the request shares read the probe row: {cost:?}, {requests:?}"
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
            account_kind: AccountKind::ApiKey,
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, &tables).await;
    let a = group(&groups, "a");
    let plan = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    assert!(
        close(a.cost_shares.as_ref().and_then(|m| m.get(FIVE_HOUR)), plan)
            && (a.plan_requests, a.plan_output_tokens) == (1, 100),
        "the overage row's tokens are absent from the sums and from the counts: {a:?} vs {plan}"
    );
    let k = group(&groups, "k");
    assert!(
        k.cost_shares.is_none(),
        "an api_key group answers no share: {k:?}"
    );
    assert_eq!(
        (k.plan_requests, k.plan_output_tokens),
        (0, 0),
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
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let filter = RollupFilter::default();
    let breakdown = account_groups(&db, &filter, from, to, &tables).await;
    let points = series_paged(
        &db,
        &filter,
        &LiveKeys::default(),
        from,
        to,
        3_600,
        GroupBy::Account,
        &PRICES,
        &tables,
        NOW,
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
            account_kind: AccountKind::ApiKey,
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
            account_groups(&db, &filter, from, to, &COSTS)
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

/// A usage-based seat's attempt files its rollup row under the seat's own funding word, which the `billed` filter keeps
/// and the `plan` filter drops, and the budget's metered sum counts its dollars as it counts an overage or a fast row.
#[tokio::test]
async fn a_usage_based_seats_row_files_under_usage_based_stays_billed_and_counts_in_the_metered_sum(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            id: "r-seat",
            account: "seat",
            usage_based: true,
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
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let word_of = |account: &str| {
        rows.iter()
            .find(|r| r.account_id == account)
            .map(|r| r.funding.as_str())
    };
    assert_eq!(
        (word_of("seat"), word_of("a")),
        (Some(USAGE_BASED_FUNDING), Some(PLAN_FUNDING)),
        "the seat's row carries its own word in the key beside the plan row's: {rows:?}"
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let read = |funding: FundingFilter| {
        let db = db.clone();
        async move {
            let filter = RollupFilter {
                funding: Some(funding),
                ..RollupFilter::default()
            };
            account_groups(&db, &filter, from, to, &COSTS)
                .await
                .iter()
                .map(|g| (g.key.clone(), g.requests))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        (
            read(FundingFilter::Billed).await,
            read(FundingFilter::Plan).await
        ),
        (vec![("seat".to_string(), 1)], vec![("a".to_string(), 1)]),
        "billed keeps the seat's row and plan drops it"
    );
    // A flat dollar a token, so the sum is the row's token count.
    let prices = priced_table("claude-fable-5-1", 1.0);
    let metered = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[BudgetedMember {
                    pool_id: "p".into(),
                    account_id: "seat".into(),
                    period: BudgetPeriod::Day,
                    metered_only: true,
                }],
                WEEKDAY_OFF_PEAK + 3_600_000,
                &prices,
            )
        })
        .unwrap();
    assert_eq!(
        metered.iter().map(|m| m.spent_usd).collect::<Vec<_>>(),
        [1_100.0],
        "the metered sum counts the seat's thousand input and hundred output tokens"
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
            account_kind: AccountKind::ApiKey,
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
    let read = |funding: FundingFilter| {
        let filter = RollupFilter {
            funding: Some(funding),
            ..RollupFilter::default()
        };
        let (db, tables) = (db.clone(), &tables);
        async move { account_groups(&db, &filter, from, to, tables).await }
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
            account_kind: AccountKind::ApiKey,
            ..ShareRow::default()
        },
    );
    let tables = share_tables();
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let read = |funding: FundingFilter| {
        let (db, tables) = (db.clone(), &tables);
        async move {
            let filter = RollupFilter {
                funding: Some(funding),
                ..RollupFilter::default()
            };
            account_groups(&db, &filter, from, to, tables).await
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
                overage_in_use: true,
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
            let groups = account_groups(&db, &filter, from, to, &COSTS).await;
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
                overage_in_use: true,
                speed: Speed::Fast,
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
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let groups = account_groups(&db, &RollupFilter::default(), from, to, &tables).await;
    let t = group(&groups, "t");
    assert!(
        t.cost_shares.is_none(),
        "the tierless group answers its shares null: {t:?}"
    );
    assert_eq!(
        (t.plan_requests, t.plan_output_tokens),
        (1, 100),
        "and still counts the plan row its figure is summed over: {t:?}"
    );
    let equivalent = equivalent_of(
        &tables,
        "claude-fable-5-1",
        usage(1_000, 100),
        WEEKDAY_OFF_PEAK,
    );
    assert!(
        close(t.cost_equivalent.as_ref(), equivalent),
        "the figure reads no account tier, so the tierless row prices: {t:?}"
    );
    let u = group(&groups, "u");
    let own = fable_shares(&tables, usage(1_000, 100))[FIVE_HOUR].unwrap();
    assert!(
        close(u.cost_shares.as_ref().and_then(|m| m.get(FIVE_HOUR)), own),
        "the dropped account's row prices under the tier it recorded: {u:?} vs {own}"
    );
}

/// The groups of `[from, to)` under `group_by` and `filter`, the shares, where the read allows, and the figures priced
/// on `tables`.
async fn groups_by(
    db: &std::sync::Arc<LedgerDb>,
    filter: &RollupFilter,
    (from, to): (i64, i64),
    group_by: GroupBy,
    tables: &PlanTables,
) -> Vec<GroupRow> {
    breakdown_paged(
        db,
        filter,
        &LiveKeys::default(),
        from,
        to,
        group_by,
        &PRICES,
        tables,
        NOW,
    )
    .await
    .unwrap()
}

/// The Pro-week figure `usage` of `model` reads in the hour of `at` on `tables`, the cost model the read priced at.
fn equivalent_of(tables: &PlanTables, model: &str, usage: PinUsage, at: i64) -> f64 {
    let hour = hour_of(at) * 1_000;
    tables
        .equivalent_at(SEVEN_DAY, None, model, usage, hour, hour, NOW)
        .expect("the cost model weighs the model")
}

/// The owner's reading: one request of a Pro account sits next to one of an Ultra account in the one unit, so the
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
        GroupBy::Model,
        &tables,
    )
    .await;
    let fable = group(&groups, "claude-fable-5-1");
    let (pro, max) = (
        equivalent_of(&tables, "claude-fable-5-1", pro, WEEKDAY_OFF_PEAK),
        equivalent_of(&tables, "claude-fable-5-1", max, WEEKDAY_OFF_PEAK),
    );
    let want = pro + max;
    assert!(
        close(fable.cost_equivalent.as_ref(), want),
        "the two accounts' rows add into one figure: {fable:?} vs {want}"
    );
    assert!(
        fable.cost_shares.is_none(),
        "two accounts' own windows do not add, so the group answers no share: {fable:?}"
    );
    assert_eq!(
        (fable.plan_requests, fable.plan_output_tokens),
        (2, 150),
        "the counts answer under every grouping: {fable:?}"
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
            account_kind: AccountKind::ApiKey,
            usage: usage(4_000, 400),
            ..ShareRow::default()
        },
    );
    let groups = groups_by(
        &db,
        &RollupFilter::default(),
        hour_range(WEEKDAY_OFF_PEAK),
        GroupBy::All,
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
    let want = equivalent_of(&share_tables(), "claude-fable-5-1", plan, WEEKDAY_OFF_PEAK);
    assert!(
        close(all.cost_equivalent.as_ref(), want),
        "the plan row alone reads in the figure: {all:?} vs {want}"
    );
}

/// A cached plan row's figure splits by token kind as the console's breakdown reads it, and its cache halves read
/// under the model grouping as under the account grouping.
#[tokio::test]
async fn the_equivalent_split_s_four_kinds_sum_to_the_figure_and_the_cache_halves_read_by_model() {
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
        GroupBy::Model,
        &share_tables(),
    )
    .await;
    let fable = group(&groups, "claude-fable-5-1");
    let figure = fable.cost_equivalent.expect("the row priced");
    assert!(figure > 0.0, "{fable:?}");
    let split = fable.equivalent_split.as_ref().expect("the row priced");
    assert_eq!(
        split.keys().copied().collect::<Vec<_>>(),
        ["input", "cache_read", "cache_write", "output"],
        "{fable:?}"
    );
    assert!(
        split.values().all(|part| *part > 0.0)
            && (split.values().sum::<f64>() - figure).abs() < 1e-12,
        "the four kinds sum to the figure: {fable:?}"
    );
    let half = |usage: PinUsage| {
        equivalent_of(&share_tables(), "claude-fable-5-1", usage, WEEKDAY_OFF_PEAK)
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
    assert!(
        fable.cache_equivalent.as_ref().is_some_and(|c| {
            (c.read - half(read)).abs() < 1e-12 && (c.write - half(write)).abs() < 1e-12
        }),
        "the cache halves: {fable:?}"
    );
    assert!(
        (split["cache_read"] - half(read)).abs() < 1e-12,
        "the split's cache read is the figure's cache half: {fable:?}"
    );
}

/// The probe rows and the wasted probes' own counts read in the Pro-week figure under every grouping, each priced at
/// its own row's hour.
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
    let groups = groups_by(
        &db,
        &with_probes(),
        (from, to),
        GroupBy::Model,
        &share_tables(),
    )
    .await;
    let fable = group(&groups, "claude-fable-5-1");
    let cost = equivalent_of(&share_tables(), "claude-fable-5-1", probe, WEEKDAY_OFF_PEAK);
    let waste = equivalent_of(&share_tables(), "claude-fable-5-1", wasted, next_hour);
    assert!(
        waste > 0.0
            && fable.probe_equivalent.as_ref().is_some_and(|p| {
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
    let spanning = groups_by(
        &db,
        &RollupFilter::default(),
        range,
        GroupBy::Model,
        &tables,
    )
    .await;
    assert_eq!(
        five_hour(group(&spanning, "claude-fable-5-1")),
        None,
        "a model group spanning two accounts answers no share"
    );
    let named = RollupFilter {
        accounts: vec!["a".into()],
        ..RollupFilter::default()
    };
    let one = groups_by(&db, &named, range, GroupBy::Model, &tables).await;
    assert!(
        close(five_hour(group(&one, "claude-fable-5-1")).as_ref(), own),
        "the same group under a filter naming account a reads a's share: {one:?} vs {own}"
    );
    let by_account = groups_by(
        &db,
        &RollupFilter::default(),
        range,
        GroupBy::Account,
        &tables,
    )
    .await;
    for key in ["a", "b"] {
        assert!(
            close(five_hour(group(&by_account, key)).as_ref(), own),
            "each account group is one account: {by_account:?}"
        );
    }
}

/// A classifier call's plan row reads zero in its shares and in the equivalent, and its cache halves and its split by
/// token kind read zero beside them, while a plain row's figures stay priced from its counts. The read names the one
/// account both rows landed on, so the groups answer their shares.
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
        &RollupFilter {
            accounts: vec!["a".into()],
            ..RollupFilter::default()
        },
        hour_range(WEEKDAY_OFF_PEAK),
        GroupBy::Model,
        &share_tables(),
    )
    .await;
    let (marked, plain) = (
        group(&groups, "claude-opus-5-5"),
        group(&groups, "claude-fable-5-1"),
    );
    for window in [FIVE_HOUR, SEVEN_DAY] {
        assert_eq!(
            marked.cost_shares.as_ref().and_then(|m| m.get(window)),
            Some(&0.0),
            "the marked group's share of {window}: {marked:?}"
        );
        assert!(
            plain
                .cost_shares
                .as_ref()
                .and_then(|m| m.get(window))
                .is_some_and(|share| *share > 0.0),
            "the plain group's share of {window}: {plain:?}"
        );
    }
    assert_eq!(
        (marked.cost_equivalent, &marked.cache_equivalent),
        (
            Some(0.0),
            &Some(phoebis_ledger::CacheEquivalent {
                read: 0.0,
                write: 0.0
            })
        ),
        "the marked group's figure and cache halves: {marked:?}"
    );
    assert!(
        plain.cost_equivalent.is_some_and(|e| e > 0.0)
            && plain
                .cache_equivalent
                .as_ref()
                .is_some_and(|c| c.read > 0.0 && c.write > 0.0),
        "the plain group's figure: {plain:?}"
    );
    let marked_split = marked
        .equivalent_split
        .as_ref()
        .expect("the marked group's split is priced, not empty");
    assert_eq!(
        marked_split.keys().copied().collect::<Vec<_>>(),
        ["input", "cache_read", "cache_write", "output"],
        "{marked:?}"
    );
    assert!(
        marked_split.values().all(|part| *part == 0.0),
        "the marked group's split: {marked:?}"
    );
    assert!(
        plain
            .equivalent_split
            .as_ref()
            .is_some_and(|split| split.len() == 4 && split.values().all(|part| *part > 0.0)),
        "the plain group's split: {plain:?}"
    );
}

/// A rollup row keyed as the client's classifier call reads a zero share on both windows and a zero Pro-week figure in
/// its burn bucket, while a plain row of the same counts in the next bucket reads its figures from its counts.
#[test]
fn a_classifier_rows_burn_bucket_reads_zero_shares_and_figure_beside_a_plain_rows_figures() {
    let tables = share_tables();
    let filter = SeriesFilter {
        from: 0,
        to: 10_800_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let row = |hour: i64, classifier: &str| RollupRow {
        input_tokens: 1_000,
        output_tokens: 100,
        ..RollupRow::key(
            hour,
            "k",
            0,
            "p",
            "a",
            "claude-sonnet-5-5",
            "inference",
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            "standard",
            PLAN_FUNDING,
            "standard",
            "test_tier",
            classifier,
        )
    };
    let points = burn_series_over(
        &[row(3_600, "client"), row(7_200, NO_CLASS)],
        &tables,
        &PRICES,
        &filter,
        &LiveKeys::default(),
        NOW,
    )
    .unwrap();
    let figures = |at: i64| {
        let point = points
            .iter()
            .find(|p| p.at == at)
            .expect("the bucket is in the range");
        [
            point.five_hour_share,
            point.seven_day_share,
            point.equivalent_by_tier["test_tier"],
        ]
    };
    assert_eq!(
        figures(3_600_000),
        [Some(0.0); 3],
        "the classifier row's shares on both windows and its figure"
    );
    let plain = figures(7_200_000);
    assert!(
        plain.iter().all(|figure| figure.is_some_and(|f| f > 0.0)),
        "the plain row's shares on both windows and its figure: {plain:?}"
    );
}

/// A free classifier call spends no window: beside a priced row, one on a tier the cost model does not size and one on
/// a tier sized on the 5-hour window alone leave the bucket's shares and its 5-hour figure as the priced row set them.
#[test]
fn a_free_call_on_an_unsized_or_one_window_tier_marks_no_window_and_adds_no_five_hour_figure() {
    let tables = share_tables();
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    let row = |tier: &str, classifier: &str| RollupRow {
        input_tokens: 1_000,
        output_tokens: 100,
        ..RollupRow::key(
            3_600,
            "k",
            0,
            "p",
            "a",
            "claude-sonnet-5-5",
            "inference",
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            "standard",
            PLAN_FUNDING,
            "standard",
            tier,
            classifier,
        )
    };
    let points = burn_series_over(
        &[
            row("test_tier", NO_CLASS),
            row("unlisted_tier", "client"),
            row("narrow_tier", "client"),
        ],
        &tables,
        &PRICES,
        &filter,
        &LiveKeys::default(),
        NOW,
    )
    .unwrap();
    let point = point_at(&points, 3_600);
    // 1,000 input and 100 output tokens at weight 1 are 2,000 units, over test_tier's 100 on the 5-hour window and
    // 1,000 on the week, off the band at 01:00 UTC, and the 5-hour figure divides by the same 100 of the anchor tier.
    assert_eq!(
        (
            point.five_hour_share,
            point.seven_day_share,
            point.five_hour_equivalent_by_tier["test_tier"]
        ),
        (Some(20.0), Some(2.0), Some(20.0)),
        "the priced row's shares and 5-hour figure alone: {point:?}"
    );
}

/// A classifier call's plan row reads zero shares and a zero figure through its session's series, while a plain row of
/// the same counts on another account reads its figures there.
#[test]
fn a_classifier_row_reads_zero_in_the_session_series_beside_a_plain_rows_figures() {
    let db = LedgerDb::open_in_memory().unwrap();
    let tables = share_tables();
    plant_share(
        &db,
        ShareRow {
            id: "r-classifier",
            classifier: Some("client"),
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-plain",
            account: "b",
            ..ShareRow::default()
        },
    );
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    // A read naming one account answers the session's shares from that account's attempts alone.
    let figures = |account: &str| {
        let filter = SeriesFilter {
            from: hour * 1_000,
            to: (hour + 3_600) * 1_000,
            bucket_seconds: 3_600,
            accounts: vec![account.into()],
            ..SeriesFilter::default()
        };
        let sessions = db
            .read_blocking(|c| {
                phoebis_ledger::session_burn_series(
                    c,
                    &tables,
                    &PRICES,
                    &filter,
                    &LiveKeys::default(),
                    NOW,
                )
            })
            .unwrap();
        let (_, _, points) = sessions
            .iter()
            .find(|(id, _, _)| id == "s")
            .unwrap_or_else(|| panic!("the session is answered: {sessions:?}"));
        let point = point_at(points, hour);
        [
            point.five_hour_share,
            point.seven_day_share,
            point.equivalent_by_tier["test_tier"],
        ]
    };
    assert_eq!(
        figures("a"),
        [Some(0.0); 3],
        "the classifier row's shares and figure in the session series"
    );
    let plain = figures("b");
    assert!(
        plain.iter().all(|figure| figure.is_some_and(|f| f > 0.0)),
        "the plain row's shares and figure in the session series: {plain:?}"
    );
}

/// The burn prices each row by its funding: a subscription row in the share of its own windows where the read names
/// its one account, an api_key row in dollars, and every plan row in the Pro-week figure under every scope, so the
/// figures of two accounts add into one while their shares do not.
#[test]
fn the_burn_series_prices_a_subscription_row_in_window_share_and_an_api_key_row_in_dollars() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str,
                 account: &str,
                 kind: AccountKind,
                 tier: Option<&str>,
                 cache_read: Option<u64>| {
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
                account_kind: kind,
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
    plant("sub", "oauth-a", AccountKind::Oauth, Some(MAX_20X), None);
    plant("key", "api-b", AccountKind::ApiKey, None, None);
    // A plan row that recorded no tier prices no share and no dollar, while its tokens and its figure still count,
    // its cache reads included, the one row carrying any so the cached half of the assertion below reads them.
    plant(
        "tierless",
        "tierless-c",
        AccountKind::Oauth,
        None,
        Some(500),
    );
    let tables = PlanTables::shipped();
    let point_of = |accounts: &[&str]| {
        let filter = SeriesFilter {
            from: 0,
            to: 7_200_000,
            bucket_seconds: 3_600,
            accounts: accounts.iter().map(|a| a.to_string()).collect(),
            ..SeriesFilter::default()
        };
        db.read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &LiveKeys::default(), NOW))
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
    let figure = |usage: PinUsage| {
        tables
            .equivalent_at(
                SEVEN_DAY,
                None,
                "claude-sonnet-5-5",
                usage,
                hour * 1_000,
                hour * 1_000,
                NOW,
            )
            .expect("the shipped model weighs the model")
    };
    let want = figure(tokens) + figure(cached);
    assert!(
        want > 0.0 && every.equivalent_by_tier[PRO].is_some_and(|got| (got - want).abs() < 1e-12),
        "both plan rows add their figure, the tierless one included, and the api_key row a zero: {:?} vs {want}",
        every.equivalent_by_tier
    );
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
/// row reads its dollars, no share and a zero figure, under `-` as under a sized tier.
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
        let point = burn_series_over(&[row], &tables, &PRICES, &filter, &LiveKeys::default(), NOW)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap();
        (
            point.usd,
            point.five_hour_share,
            point.seven_day_share,
            point.equivalent_by_tier[PRO],
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
        (key_usd, None, None, Some(0.0), 1_000),
        "an api_key account's row prices in dollars from its counts and adds a zero figure"
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
        let point = burn_series_over(&rows, &tables, &PRICES, &filter, &LiveKeys::default(), NOW)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap();
        assert_eq!(
            (
                point.usd,
                point.five_hour_share,
                point.seven_day_share,
                point.equivalent_by_tier[PRO]
            ),
            (fast_usd, None, None, None),
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
        burn_series_over(&[row], &tables, &PRICES, &filter, &LiveKeys::default(), NOW)
            .unwrap()
            .into_iter()
            .find(|p| p.at == 3_600_000)
            .unwrap()
    };
    let standard = point("standard", MAX_20X);
    assert!(
        standard.five_hour_share.is_some()
            && standard.equivalent_by_tier[PRO].is_some_and(|e| e > 0.0)
            && standard.usd == 0.0,
        "a standard row on a subscription reads a share, a figure and no dollars: {standard:?}"
    );
    let fast_usd = shipped_usd(3_600_000, "claude-opus-5-5", Speed::Fast, usage(1_000, 100));
    let fast = point("fast", MAX_20X);
    assert_eq!(
        (
            fast.usd,
            fast.five_hour_share,
            fast.seven_day_share,
            fast.equivalent_by_tier[PRO],
            fast.uncached_tokens
        ),
        (fast_usd, Some(0.0), Some(0.0), Some(0.0), 1_000),
        "a fast row draws usage credits: its dollars and its tokens, a zero share of either window that still \
         says the tier is sized, and a zero figure"
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

/// The fold files an answered attempt under the speed its answer reported, so no row it folds today holds tokens under
/// the fast speed with plan funding. A ledger can still hold such rows from a fold before that rule, since rollups stay
/// for 365 days. The burn prices such a row on the plan it drew from, its window shares and figures as a standard row
/// of the same usage reads them, while its list dollars read the cells of the speed the row is filed under, as the
/// breakdown's cost does.
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
        burn_series_over(&[row], &tables, &PRICES, &filter, &LiveKeys::default(), NOW)
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
    // The plan's list dollars read the cells the row files under, as the breakdown's dollars price a row, so they are
    // left out of the plan's shares and figures this case reads.
    assert_eq!(
        phoebis_ledger::BurnPoint {
            plan_usd: standard.plan_usd,
            ..point("fast")
        },
        standard,
        "an asked speed never prices as a served one"
    );
}

#[test]
fn fast_traffic_alone_on_a_model_without_a_weight_reads_a_zero_share_and_a_zero_equivalent_on_a_sized_tier(
) {
    let tables = phoebis_ledger::test_tables::subscription_costs();
    // Haiku 5.5 carries no weight in the test cost model, and fast cells in this list alone: the standard cells at $1
    // and $5 a million input and output tokens, the fast ones at $6 and $30.
    let prices = PriceRevisions::from_yaml(
        "revisions:\n  - effective: 2026-01-01\n    models:\n      claude-haiku-5-5: {input: 1, cache_write_5m: 1.25, \
         cache_write_1h: 2, cache_read: 0.1, output: 5, \
         fast: {input: 6, cache_write_5m: 7.5, cache_write_1h: 12, cache_read: 0.6, output: 30}}\n",
        "test",
    )
    .expect("the case's price list parses");
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
            "claude-haiku-5-5",
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
    assert_eq!(
        tables.share_at(
            FIVE_HOUR,
            None,
            MAX_20X,
            "claude-haiku-5-5",
            usage(1_000, 100),
            3_600_000,
        ),
        None,
        "the model has no weight, so it prices nothing on the plan window"
    );
    let point = burn_series_over(&[row], &tables, &prices, &filter, &LiveKeys::default(), NOW)
        .unwrap()
        .into_iter()
        .find(|p| p.at == 3_600_000)
        .unwrap();
    // 1,000 input tokens at $6 and 100 output tokens at $30 a million, where the standard cells would read $0.0015.
    assert!(
        (point.usd - 0.009).abs() < 1e-12
            && (point.five_hour_share, point.seven_day_share) == (Some(0.0), Some(0.0)),
        "the tier alone sizes the account, so its dollars at the fast cells sit beside a zero share of either \
         window: {point:?}"
    );
    assert_eq!(
        (point.equivalent_by_tier[PRO], point.unpriced),
        (Some(0.0), false),
        "the billed row alone reads the zero figure of a bucket with no plan row"
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
        burn_series_over(&[row], &tables, &PRICES, &filter, &LiveKeys::default(), NOW)
            .unwrap()
            .into_iter()
            .find(|p| p.at == hour_of(at) * 1_000)
            .unwrap()
            .seven_day_share
            .unwrap()
    };
    // The test model: Opus 5.5 weighs 2 and an input token multiplies by 1, so 1,000 input tokens are 2,000 units
    // of the Ultra week's 1,000, and inside the band the week holds the 0.5 this test sets.
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
                account_kind: AccountKind::Oauth,
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
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &LiveKeys::default(), NOW))
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
        .read_blocking(|c| {
            burn_series(
                c,
                &tables,
                &PRICES,
                &daily_filter,
                &LiveKeys::default(),
                NOW,
            )
        })
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
            account_kind: AccountKind::Oauth,
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
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &LiveKeys::default(), NOW))
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
    let plant = |account: &str, kind: AccountKind, tier: Option<&str>| {
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
                account_kind: kind,
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
    plant("max", AccountKind::Oauth, Some(MAX_20X));
    plant("pro", AccountKind::Oauth, Some(PRO));
    plant("tierless", AccountKind::Oauth, None);
    plant("key", AccountKind::ApiKey, None);
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
        db.read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &LiveKeys::default(), NOW))
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
        "a million Opus 5.5 input tokens read their share of each Ultra window off the band: {max:?}"
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
    // The Pro-week figure reads no account tier: one Opus 5.5 request on Pro reads the same figure as on Ultra or
    // with no tier at all, so a Pro account's request compares to an Ultra account's.
    let figure = tables.equivalent_at(SEVEN_DAY, None, "claude-opus-5-5", million, hour, hour, NOW);
    assert!(figure.is_some(), "the shipped model weighs Opus 5.5");
    for point in [&max, &pro, &tierless] {
        assert_eq!(point.equivalent_by_tier[PRO], figure, "{point:?}");
    }
    assert_eq!(
        key.equivalent_by_tier[PRO],
        Some(0.0),
        "the api_key row adds a zero figure beside its dollars"
    );
}

/// One request in the off-band weekday hour on `account`, an Ultra subscription, its one attempt carrying `input`
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
            account_kind: AccountKind::Oauth,
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

/// The off-band hour's burn point of `account`, an Ultra subscription.
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
    db.read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &LiveKeys::default(), NOW))
        .unwrap()
        .into_iter()
        .find(|p| p.at == hour)
        .expect("the planted hour")
}

#[test]
fn a_bucket_mixing_a_priced_and_an_unpriced_row_prices_no_share_on_that_window_and_counts_every_token(
) {
    let db = LedgerDb::open_in_memory().unwrap();
    // The same hour on Ultra: a million input tokens of a model the table prices and a million Mystery 2 tokens of
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
                point.equivalent_by_tier[PRO],
                point.unpriced,
                point.tokens,
                point.usd
            ),
            (None, None, None, true, 2_000_000, 0.0),
            "{priced} beside Mystery 2, the figure latched as the share is"
        );
    }
    // Opus 4.7 has no weight of its own though Opus 5.5 weighs its model tier, so a bucket of the two latches as one
    // beside Mystery 2 does.
    plant_in_the_off_band_hour(&db, "c-opus-5-5", "c", "claude-opus-5-5", Some(1_000_000));
    plant_in_the_off_band_hour(&db, "c-opus-4-7", "c", "claude-opus-4-7", Some(1_000_000));
    let point = off_band_point_on_max_20x(&db, "c");
    assert_eq!(
        (
            point.five_hour_share,
            point.seven_day_share,
            point.equivalent_by_tier[PRO],
            point.tokens
        ),
        (None, None, None, 2_000_000),
        "Opus 5.5 beside Opus 4.7: {point:?}"
    );
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
            account_kind: AccountKind::Oauth,
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
            phoebis_ledger::session_burn_series(
                c,
                &tables,
                &PriceRevisions::shipped(),
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap();
    let point = sessions
        .iter()
        .find(|(id, _, _)| id == "s-edge")
        .and_then(|(_, _, points)| points.iter().find(|p| p.five_hour_share.is_some()))
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
                account_kind: AccountKind::Oauth,
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
            phoebis_ledger::session_burn_series(
                c,
                &tables,
                &PriceRevisions::shipped(),
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap();
    let point = |session: &str| {
        sessions
            .iter()
            .find(|(id, _, _)| id == session)
            .and_then(|(_, _, points)| points.iter().find(|p| p.at == hour).cloned())
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
    let points =
        burn_series_over(&rows, &tables, &PRICES, &filter, &LiveKeys::default(), NOW).unwrap();
    for hour in [3_600, 7_200, 10_800] {
        let point = point_at(&points, hour);
        assert!(point.usd > 0.0, "{point:?}");
        assert_eq!(
            (point.equivalent_by_tier[PRO], point.unpriced),
            (Some(0.0), false),
            "a billed row adds a zero Pro-week figure: {point:?}"
        );
    }
    let idle = point_at(&points, 0);
    assert_eq!(
        (idle.equivalent_by_tier[PRO], idle.unpriced),
        (None, false),
        "an idle bucket"
    );
}

/// The burn carries the model week's share beside the two shared windows: a Fable row's week share, which a row on a
/// model without a week neither adds to nor nulls, and nothing for a bucket whose rows carry no week. The Pro-week
/// figure sums both rows with no week of its own.
#[test]
fn the_burn_answers_the_model_week_share_beside_the_shared_windows() {
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
        burn_series_over(&rows, &tables, &PRICES, &filter, &LiveKeys::default(), NOW).unwrap()
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
    let figure = |model: &str| {
        tables
            .equivalent_at(SEVEN_DAY, None, model, million, 3_600_000, 3_600_000, NOW)
            .unwrap()
    };
    let named = read(vec!["a".into()]);
    let fable = point_at(&named, 3_600);
    assert_eq!(
        fable.model_week_share,
        Some(week_share),
        "the Fable row's week, the Opus row beside it adding nothing to it: {fable:?}"
    );
    let opus = point_at(&named, 7_200);
    assert_eq!(
        opus.model_week_share, None,
        "a bucket whose rows carry no week of their own: {opus:?}"
    );
    let fast = point_at(&named, 10_800);
    assert_eq!(
        fast.model_week_share,
        Some(0.0),
        "a fast Fable row adds a zero to its week: {fast:?}"
    );
    let every = point_at(&read(Vec::new()), 3_600);
    let both = figure("claude-fable-5-1") + figure("claude-opus-5-5");
    assert!(
        every.model_week_share.is_none()
            && every.equivalent_by_tier[PRO].is_some_and(|got| (got - both).abs() <= 1e-12 * both),
        "a read naming no account answers no week share and the two rows' figure: {every:?} vs {both}"
    );
}

/// A plan row of 1,000 Sonnet 5.5 input tokens on account `a` under `tier` in the hour starting at `hour` seconds, its
/// `client` the label whose entrypoint names its regime.
fn regime_row(hour: i64, client: &str, tier: &str) -> RollupRow {
    RollupRow {
        input_tokens: 1_000,
        ..RollupRow::key(
            hour,
            "k",
            0,
            "p",
            "a",
            "claude-sonnet-5-5",
            "inference",
            UNATTRIBUTED,
            client,
            "none",
            NO_CLASS,
            "standard",
            PLAN_FUNDING,
            "standard",
            tier,
            "-",
        )
    }
}

/// The 5-hour figure prices each row under its own regime and the band of its hour, over the anchor tier whatever tier
/// served it: inside the band the base regime's factor of 0.5 doubles a base row's figure, while a desk row divides by
/// the desk regime's own Pro capacity, twice the base one, at its factor of 1, and reads a quarter of it.
#[test]
fn the_five_hour_figure_prices_each_row_under_its_own_regime_and_the_band_of_its_hour() {
    let tables = phoebis_ledger::test_tables::subscription_costs();
    let hour = WEEKDAY_PEAK / 1_000;
    let filter = SeriesFilter {
        from: WEEKDAY_PEAK,
        to: WEEKDAY_PEAK + 2 * 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let rows = [
        regime_row(hour, UNATTRIBUTED, MAX_20X),
        regime_row(hour + 3_600, "claude-code/desk", MAX_20X),
    ];
    let points =
        burn_series_over(&rows, &tables, &PRICES, &filter, &LiveKeys::default(), NOW).unwrap();
    // 1,000 input tokens at Sonnet 5.5's weight 2 are 2,000 units. The base row divides by Pro's 5 and the band's
    // 0.5, the desk row by the desk regime's Pro 10 and its factor 1, both rows at 07:00 and 08:00 in Los Angeles.
    assert_eq!(
        (
            point_at(&points, hour).five_hour_equivalent_by_tier[PRO],
            point_at(&points, hour + 3_600).five_hour_equivalent_by_tier[PRO]
        ),
        (Some(800.0), Some(200.0)),
        "each row's 5-hour figure under its own regime: {points:?}"
    );
}

/// A bucket's plan rows answer their list dollars in `plan_usd` and its billed rows theirs in `usd`, neither side
/// counting the other's.
#[test]
fn a_burn_bucket_answers_its_plan_rows_list_dollars_in_plan_usd_and_its_billed_rows_in_usd() {
    let prices = priced_table("claude-sonnet-5-5", 1e-6);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let row = |funding: &str, tier: &str, input: u64, output: u64| RollupRow {
        input_tokens: input,
        output_tokens: output,
        ..RollupRow::key(
            3_600,
            "k",
            0,
            "p",
            "a",
            "claude-sonnet-5-5",
            "inference",
            UNATTRIBUTED,
            UNATTRIBUTED,
            "none",
            NO_CLASS,
            "standard",
            funding,
            "standard",
            tier,
            "-",
        )
    };
    let rows = [
        row(PLAN_FUNDING, "test_tier", 1_000, 100),
        row(BILLED_FUNDING, UNATTRIBUTED, 2_000, 0),
    ];
    let point = point_at(
        &burn_series_over(
            &rows,
            &share_tables(),
            &prices,
            &filter,
            &LiveKeys::default(),
            NOW,
        )
        .unwrap(),
        3_600,
    );
    // A dollar per million tokens of every type: the plan row's 1,100 tokens and the billed row's 2,000.
    assert!(
        (point.plan_usd - 0.0011).abs() < 1e-12 && (point.usd - 0.002).abs() < 1e-12,
        "the plan row's dollars in plan_usd and the billed row's in usd: {point:?}"
    );
}

/// The Fable week's figure counts the plan rows on a model that draws the Fable week and a zero for every other row
/// with tokens, while the Pro-week figure keeps both: a busy bucket without a Fable row reads 0, not idle.
#[test]
fn the_fable_week_figure_counts_the_fable_rows_alone_and_a_busy_bucket_without_one_reads_zero() {
    let filter = SeriesFilter {
        from: 0,
        to: 3 * 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let row = |hour: i64, model: &str| RollupRow {
        input_tokens: 1_000,
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
            "standard",
            PLAN_FUNDING,
            "standard",
            "test_tier",
            "-",
        )
    };
    let rows = [
        row(3_600, "claude-fable-5-1"),
        row(3_600, "claude-sonnet-5-5"),
        row(7_200, "claude-sonnet-5-5"),
    ];
    let points = burn_series_over(
        &rows,
        &share_tables(),
        &PRICES,
        &filter,
        &LiveKeys::default(),
        NOW,
    )
    .unwrap();
    let figures = |hour: i64| {
        let point = point_at(&points, hour);
        (
            point.fable_equivalent_by_tier["test_tier"],
            point.equivalent_by_tier["test_tier"],
        )
    };
    // 1,000 input tokens at Fable 5.1's weight 10 are 10,000 units and at Sonnet 5.5's weight 1 are 1,000, each over
    // test_tier's 1,000 on the week. The Fable week holds half the week, so the Fable row reads 20 of it.
    assert_eq!(
        (figures(3_600), figures(7_200)),
        ((Some(20.0), Some(11.0)), (Some(0.0), Some(1.0))),
        "the Fable row's figure alone beside both rows' figure, and a zero beside a Sonnet row's: {points:?}"
    );
}

/// The points of the one group the burn route's reader answers for every row of `[from, to)` in `bucket_seconds`
/// buckets.
async fn burn_points(
    db: &std::sync::Arc<LedgerDb>,
    tables: &PlanTables,
    prices: &PriceRevisions,
    from: i64,
    to: i64,
    bucket_seconds: i64,
) -> Vec<phoebis_ledger::BurnPoint> {
    let filter = SeriesFilter {
        from,
        to,
        bucket_seconds,
        ..SeriesFilter::default()
    };
    let mut groups = burn_groups_paged(
        db,
        tables,
        prices,
        &filter,
        GroupBy::All,
        &LiveKeys::default(),
        &[],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(groups.len(), 1, "one group of every row: {groups:?}");
    groups.remove(0).2
}

/// A cost model whose second entry doubles `test_tier`'s capacity on both windows from 18:30 UTC on 2026-01-15, an
/// instant inside the 18:00 hour, with Sonnet 5.5 at weight 1 in both entries.
const DOUBLED_CAPACITY_IN_ONE_HOUR: &str = "\
unit: {tier: test_tier, window: seven_day}
revisions:
  - effective: 2026-01-01
    capacity:
      test_tier: {five_hour: 50, seven_day: 500}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5-5: sonnet
  - effective: 2026-01-15T18:30:00Z
    capacity:
      test_tier: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5-5: sonnet
";

/// Each plan request scales to a tier under the cost revision in force when it arrived: two requests of 50 input and
/// 5 output tokens, 100 units, at 18:10 and at 18:45 read 0.2 of the anchor's 500 on the week before the entry of 18:30
/// doubles it and 0.1 of its 1,000 after, so the day's bucket reads 0.3.
#[tokio::test]
async fn a_tiers_burn_figure_scales_each_plan_request_at_the_cost_revision_in_force_when_it_arrived(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    for (id, minute) in [("early", 10), ("late", 45)] {
        plant_share(
            &db,
            ShareRow {
                id,
                model: "claude-sonnet-5-5",
                at: ms(2026, 1, 15, 18, minute),
                usage: usage(50, 5),
                ..ShareRow::default()
            },
        );
    }
    let tables = phoebis_ledger::test_tables::tables(
        CostRevisions::from_yaml(DOUBLED_CAPACITY_IN_ONE_HOUR).unwrap(),
    );
    let day = ms(2026, 1, 15, 0, 0);
    let points = burn_points(
        &db,
        &tables,
        &priced_table("claude-sonnet-5-5", 1.0),
        day,
        day + DAY_MS,
        86_400,
    )
    .await;
    let point = point_at(&points, day / 1_000);
    assert!(
        close(point.equivalent_by_tier["test_tier"].as_ref(), 0.3),
        "{point:?}"
    );
}

/// One cost revision of three tiers: the anchor `test_tier` at 100 units on the 5-hour window and 1,000 on the week,
/// `wide_tier` at 200 and 2,000, and `narrow_tier` sized on the 5-hour window alone at 50. Sonnet 5.5 weighs 1 and
/// Fable 5.1 weighs 10 on every window, its Fable week half the week.
const THREE_TIERS: &str = "\
unit: {tier: test_tier, window: seven_day}
revisions:
  - effective: 2026-01-01
    capacity:
      test_tier:   {five_hour: 100, seven_day: 1000}
      wide_tier:   {five_hour: 200, seven_day: 2000}
      narrow_tier: {five_hour: 50, seven_day: null}
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
    model_windows:
      seven_day_fable: {of: seven_day, capacity_factor: 0.5}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-fable-5-1: fable
      claude-sonnet-5-5: sonnet
";

fn three_tiers() -> PlanTables {
    phoebis_ledger::test_tables::tables(CostRevisions::from_yaml(THREE_TIERS).unwrap())
}

/// One of a burn point's maps over the tiers of `THREE_TIERS`.
fn three_tier_map(
    narrow: Option<f64>,
    test: Option<f64>,
    wide: Option<f64>,
) -> BTreeMap<String, Option<f64>> {
    BTreeMap::from([
        ("narrow_tier".to_string(), narrow),
        ("test_tier".to_string(), test),
        ("wide_tier".to_string(), wide),
    ])
}

/// A tier whose revision does not size a window reads null on that window alone, and the other tiers keep their
/// figures: one plan row of 100 Sonnet units off the band reads `narrow_tier` null on the week and 2 of its 50 on the
/// 5-hour window, beside its share of each window of the two tiers that size both.
#[tokio::test]
async fn a_tier_its_revision_does_not_size_on_a_window_reads_null_there_beside_the_other_tiers_figures(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_share(
        &db,
        ShareRow {
            model: "claude-sonnet-5-5",
            usage: usage(50, 5),
            ..ShareRow::default()
        },
    );
    let (from, to) = hour_range(WEEKDAY_OFF_PEAK);
    let points = burn_points(&db, &three_tiers(), &PRICES, from, to, 3_600).await;
    let point = point_at(&points, from / 1_000);
    assert_eq!(
        (point.equivalent_by_tier, point.five_hour_equivalent_by_tier),
        (
            three_tier_map(None, Some(0.1), Some(0.05)),
            three_tier_map(Some(2.0), Some(1.0), Some(0.5))
        )
    );
}

/// A billed row and a free classifier call spend no plan window, so each adds a zero to every tier's entry of the three
/// maps ahead of the tier's size: on the three tiers' tables an hour of a billed Fable row and an hour of a free Fable
/// classifier call read zero on every entry, `narrow_tier`'s week and Fable week among them.
#[tokio::test]
async fn a_billed_row_and_a_free_classifier_call_add_a_zero_to_every_tiers_burn_figure_an_unsized_tiers_too(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let (from, _) = hour_range(WEEKDAY_OFF_PEAK);
    plant_share(
        &db,
        ShareRow {
            id: "billed",
            account: "direct",
            account_kind: AccountKind::ApiKey,
            at: from,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "free",
            classifier: Some("client"),
            at: from + 3_600_000,
            ..ShareRow::default()
        },
    );
    let points = burn_points(
        &db,
        &three_tiers(),
        &PRICES,
        from,
        from + 2 * 3_600_000,
        3_600,
    )
    .await;
    let maps = |hour: i64| {
        let point = point_at(&points, hour);
        (
            point.equivalent_by_tier,
            point.five_hour_equivalent_by_tier,
            point.fable_equivalent_by_tier,
        )
    };
    let zeros = three_tier_map(Some(0.0), Some(0.0), Some(0.0));
    let all_zero = (zeros.clone(), zeros.clone(), zeros);
    assert_eq!(
        [maps(from / 1_000), maps(from / 1_000 + 3_600)],
        [all_zero.clone(), all_zero],
        "the billed row's hour, then the free call's"
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
            account_kind: AccountKind::Oauth,
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

/// The burn groups by every dimension the breakdown knows, each group's Pro-week figure adding across accounts and
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
    let read = |group_by: GroupBy, accounts: Vec<String>| {
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
                NOW,
            )
            .await
            .unwrap()
        }
    };
    let point = |groups: &[(
        String,
        phoebis_routing::BurnUnit,
        Vec<phoebis_ledger::BurnPoint>,
    )],
                 key: &str| {
        let (_, _, points) = groups
            .iter()
            .find(|(group, _, _)| group == key)
            .unwrap_or_else(|| panic!("a group {key}: {groups:?}"));
        point_at(points, hour)
    };
    let million = usage(1_000_000, 0);
    let by_model = read(GroupBy::Model, Vec::new()).await;
    assert_eq!(
        by_model
            .iter()
            .map(|(group, _, _)| group.as_str())
            .collect::<Vec<_>>(),
        ["claude-opus-5-5", "claude-sonnet-5-5"]
    );
    let opus = point(&by_model, "claude-opus-5-5");
    let equivalent = tables
        .equivalent_at(
            SEVEN_DAY,
            None,
            "claude-opus-5-5",
            million,
            hour * 1_000,
            hour * 1_000,
            NOW,
        )
        .unwrap();
    assert!(
        opus.equivalent_by_tier[PRO].is_some_and(|e| (e - 2.0 * equivalent).abs() < 1e-12)
            && opus.five_hour_share.is_none(),
        "the two accounts' rows add in the figure and not in the shares: {opus:?} vs {equivalent}"
    );
    let named = read(GroupBy::Model, vec!["a".into()]).await;
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
    let by_account = read(GroupBy::Account, Vec::new()).await;
    assert!(
        point(&by_account, "b")
            .five_hour_share
            .is_some_and(|s| (s - share(PRO)).abs() < 1e-12),
        "each account group is one account, priced under the tier its rows recorded: {by_account:?}"
    );
    let all = read(GroupBy::All, Vec::new()).await;
    assert_eq!(
        all.iter()
            .map(|(group, _, _)| group.as_str())
            .collect::<Vec<_>>(),
        ["all"]
    );
}

/// A burn group whose rows in the range hold refusals alone is left out beside a group whose one token falls on a model
/// the cost model has no weight for, which stays and reads unpriced, so a gate written on a priced figure fails here.
#[tokio::test]
async fn a_burn_group_of_refusals_alone_is_left_out_beside_an_unpriced_group_with_one_token() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_request(
        &RequestRecord {
            id: "r-refused".into(),
            received_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 4),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            model: "claude-unweighted-1".into(),
            canonical_model: "claude-unweighted-1".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(403),
            outcome: "rejected".into(),
            attempts: 0,
            error_code: Some("guardrail_blocked".into()),
            refused: true,
            ..Default::default()
        },
        &[],
        None,
    )
    .unwrap();
    db.insert_request(
        &RequestRecord {
            id: "r-token".into(),
            received_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 500),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            model: "claude-unweighted-1".into(),
            canonical_model: "claude-unweighted-1".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r-token".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            started_at: WEEKDAY_OFF_PEAK,
            finished_at: Some(WEEKDAY_OFF_PEAK + 500),
            outcome: "succeeded".into(),
            input_tokens: Some(1),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let filter = SeriesFilter {
        from: hour * 1_000,
        to: (hour + 3_600) * 1_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let groups = burn_groups_paged(
        &db,
        &share_tables(),
        &PRICES,
        &filter,
        GroupBy::Account,
        &LiveKeys::default(),
        &[],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        groups
            .iter()
            .map(|(group, _, points)| (group.as_str(), point_at(points, hour).unpriced))
            .collect::<Vec<_>>(),
        [("a", true)],
        "the refused request's group leaves and the unweighted token's stays: {groups:?}"
    );
}

/// A session can move between accounts: its series adds every attempt's figure, and reads a share once the read names
/// one account, whose attempts alone it then holds.
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
            .read_blocking(|c| {
                phoebis_ledger::session_burn_series(
                    c,
                    &tables,
                    &PRICES,
                    &filter,
                    &LiveKeys::default(),
                    NOW,
                )
            })
            .unwrap();
        let (_, _, points) = sessions
            .iter()
            .find(|(id, _, _)| id == "s")
            .unwrap_or_else(|| panic!("the session is answered: {sessions:?}"));
        point_at(points, hour)
    };
    let million = usage(1_000_000, 0);
    let equivalent = tables
        .equivalent_at(
            SEVEN_DAY,
            None,
            "claude-opus-5-5",
            million,
            WEEKDAY_OFF_PEAK,
            WEEKDAY_OFF_PEAK + 500,
            NOW,
        )
        .unwrap();
    let every = read(Vec::new());
    assert!(
        every.five_hour_share.is_none()
            && every.equivalent_by_tier[PRO].is_some_and(|e| (e - 2.0 * equivalent).abs() < 1e-12)
            && every.uncached_tokens == 2_000_000,
        "both attempts add in the figure and not in the shares: {every:?} vs {equivalent}"
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
        .read_blocking(|c| {
            phoebis_ledger::session_burn_series(
                c,
                &tables,
                &PRICES,
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["s-z"],
        "account a's one session, and none of b's: {sessions:?}"
    );
    let point = point_at(&sessions[0].2, hour);
    assert!(
        point.uncached_tokens == 1_000_000 && point.five_hour_share.is_some(),
        "its attempt priced under a's tier: {point:?}"
    );
}

/// A session read under a key or a pool ranks and prices the named key's or pool's requests alone, and leaves out a
/// session whose requests there hold no token. Twenty sessions of another key in another pool each hold more tokens
/// than session one, so a ranking that binds neither crowds it out of the twenty.
#[test]
fn a_session_series_under_a_key_or_a_pool_ranks_and_prices_its_requests_alone() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, session: &str, key: &str, pool: &str, input: Option<u64>| {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: WEEKDAY_OFF_PEAK,
                finished_at: Some(WEEKDAY_OFF_PEAK + 500),
                key_id: Some(key.into()),
                key_generation: 1,
                pool_id: Some(pool.into()),
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
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: AccountKind::Oauth,
                started_at: WEEKDAY_OFF_PEAK,
                finished_at: Some(WEEKDAY_OFF_PEAK + 500),
                outcome: "succeeded".into(),
                input_tokens: input,
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    };
    plant("r-one-a", "s-one", "ka", "p1", Some(1_000));
    plant("r-one-b", "s-one", "kb", "p2", Some(2_000));
    for n in 0..20 {
        plant(
            &format!("r-b-{n:02}"),
            &format!("s-b-{n:02}"),
            "kb",
            "p2",
            Some(10_000),
        );
    }
    plant("r-empty", "s-empty", "ka", "p1", None);
    let live = LiveKeys(BTreeMap::from([
        ("ka".to_string(), 1),
        ("kb".to_string(), 1),
    ]));
    let tables = PlanTables::shipped();
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let read = |keys: Vec<String>, pools: Vec<String>| {
        let filter = SeriesFilter {
            from: hour * 1_000,
            to: (hour + 3_600) * 1_000,
            bucket_seconds: 3_600,
            keys,
            pools,
            ..SeriesFilter::default()
        };
        db.read_blocking(|c| {
            phoebis_ledger::session_burn_series(c, &tables, &PRICES, &filter, &live, NOW)
        })
        .unwrap()
        .into_iter()
        .map(|(id, _, points)| (id, point_at(&points, hour).tokens))
        .collect::<Vec<_>>()
    };
    assert_eq!(
        read(vec!["ka".into()], Vec::new()),
        [("s-one".to_string(), 1_000)],
        "under key ka, session one alone with its ka request's tokens"
    );
    assert_eq!(
        read(Vec::new(), vec!["p1".into()]),
        [("s-one".to_string(), 1_000)],
        "under pool p1, session one alone with its p1 request's tokens"
    );
}

/// On six-hour buckets a range that starts inside a bucket reads back to the bucket's edge, and a session whose tokens
/// lie there alone leaves the answer, so it takes none of the twenty slots: the twenty sessions of the range itself
/// stay.
#[test]
fn a_session_with_tokens_before_the_ranges_first_hour_alone_takes_no_slot_of_the_twenty() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Each of the twenty carries a million tokens at 20:00, inside the range.
    for n in 0..20 {
        plant_burn(
            &db,
            &format!("r-b-{n:02}"),
            "a",
            MAX_20X,
            "claude-opus-5-5",
            &format!("s-b-{n:02}"),
        );
    }
    // Two million tokens at 18:30, after the six-hour edge at 18:00 and before the range's first hour, 19:00.
    let early = WEEKDAY_OFF_PEAK - 90 * 60_000;
    db.insert_request(
        &RequestRecord {
            id: "r-early".into(),
            received_at: early,
            finished_at: Some(early + 500),
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            session_id: Some("s-early".into()),
            canonical_model: "claude-opus-5-5".into(),
            model: "claude-opus-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r-early".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            started_at: early,
            finished_at: Some(early + 500),
            outcome: "succeeded".into(),
            input_tokens: Some(2_000_000),
            tier: Some(MAX_20X.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let filter = SeriesFilter {
        from: WEEKDAY_OFF_PEAK - 30 * 60_000,
        to: WEEKDAY_OFF_PEAK + 3_600_000,
        bucket_seconds: 6 * 3_600,
        ..SeriesFilter::default()
    };
    let ids: Vec<String> = db
        .read_blocking(|c| {
            phoebis_ledger::session_burn_series(
                c,
                &PlanTables::shipped(),
                &PRICES,
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(
        (ids.len(), ids.contains(&"s-early".to_string())),
        (20, false),
        "the twenty sessions of the range, and not the early one: {ids:?}"
    );
}

/// The reserved key id and the reserved pool name the requests that store no key and no pool, which the requests table
/// keeps as NULL, and a request that stores both stays out. Under the account grouping the reserved key's rollup rows
/// match at the generation they carry.
#[tokio::test]
async fn the_reserved_key_and_pool_read_the_requests_that_store_neither() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let plant = |id: &str, account: &str, key: Option<&str>, pool: Option<&str>| {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: WEEKDAY_OFF_PEAK,
                finished_at: Some(WEEKDAY_OFF_PEAK + 500),
                key_id: key.map(Into::into),
                pool_id: pool.map(Into::into),
                session_id: Some(format!("s-{id}")),
                canonical_model: "claude-opus-5-5".into(),
                model: "claude-opus-5-5".into(),
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
                account_kind: AccountKind::Oauth,
                started_at: WEEKDAY_OFF_PEAK,
                finished_at: Some(WEEKDAY_OFF_PEAK + 500),
                outcome: "succeeded".into(),
                input_tokens: Some(1_000),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    };
    plant("neither", "a", None, None);
    plant("both", "b", Some("k"), Some("p"));
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let filter = |keys: Vec<String>, pools: Vec<String>| SeriesFilter {
        from: hour * 1_000,
        to: (hour + 3_600) * 1_000,
        bucket_seconds: 3_600,
        keys,
        pools,
        ..SeriesFilter::default()
    };
    let tables = PlanTables::shipped();
    let sessions = |filter: SeriesFilter| -> Vec<String> {
        db.read_blocking(|c| {
            phoebis_ledger::session_burn_series(
                c,
                &tables,
                &PRICES,
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect()
    };
    assert_eq!(
        sessions(filter(vec![UNATTRIBUTED.into()], Vec::new())),
        ["s-neither"],
        "the reserved key reads the request that stores no key"
    );
    assert_eq!(
        sessions(filter(Vec::new(), vec![UNATTRIBUTED.into()])),
        ["s-neither"],
        "the reserved pool reads the request that stores no pool"
    );
    let accounts: Vec<String> = burn_groups_paged(
        &db,
        &tables,
        &PRICES,
        &filter(vec![UNATTRIBUTED.into()], Vec::new()),
        GroupBy::Account,
        &LiveKeys::default(),
        &[],
        NOW,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|(group, _, _)| group)
    .collect();
    assert_eq!(
        accounts,
        ["a"],
        "the reserved key's rollup rows under the account grouping"
    );
}

/// The share cases' cost model.
fn share_tables() -> PlanTables {
    phoebis_ledger::test_tables::tables(
        CostRevisions::from_yaml(phoebis_ledger::test_tables::SHARE_COSTS)
            .expect("the share cost model parses"),
    )
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
            account_kind: AccountKind::Oauth,
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
        None,
        "test_tier",
        "claude-haiku-4-5",
        one,
        WEEKDAY_OFF_PEAK,
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
        None,
        "narrow_tier",
        "claude-fable-5-1",
        one,
        WEEKDAY_OFF_PEAK,
        WEEKDAY_OFF_PEAK,
    );
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
    );
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
        .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &LiveKeys::default(), NOW))
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
            phoebis_ledger::session_burn_series(
                c,
                &tables,
                &PriceRevisions::shipped(),
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap();
    let point = sessions
        .iter()
        .find(|(id, _, _)| id == "s-mixed")
        .and_then(|(_, _, points)| points.iter().find(|p| p.at == hour).cloned())
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
        // Account a is an Ultra subscription and c a Pro one, recorded on each attempt as the landing would.
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
    // The key scope's bare id reads the generation a live key holds it at, one of the two the day's rows carry.
    let live = LiveKeys(BTreeMap::from([("k2".to_string(), 0)]));
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
                .read_blocking(|c| burn_series(c, &tables, &PRICES, &filter, &live, NOW))
                .unwrap();
            let over = burn_series_over(&rows, &tables, &PRICES, &filter, &live, NOW).unwrap();
            // The unweighted GPT rows of b and c latch their buckets' figure, so a scope holding them reads the fast
            // rows' dollars alone.
            assert!(
                read.iter()
                    .any(|p| p.equivalent_by_tier[PRO].is_some_and(|e| e > 0.0) || p.usd > 0.0),
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
    assert_eq!(
        series,
        vec![(0, 0), (3_600_000, 40)],
        "one point per bucket, at its millisecond edge: the row's rewrite tokens in the hour it fell in"
    );
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
fn the_rewrite_series_sums_the_named_accounts_alone() {
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
        fold_account(
            &tx,
            &phoebis_ledger::AccountRollupRow {
                hour: 3_600,
                account_id: "b".into(),
                rewrite_tokens: 700,
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
        accounts: vec!["a".into()],
        ..SeriesFilter::default()
    };
    assert_eq!(
        db.read_blocking(|c| rewrite_series(c, &filter)).unwrap(),
        vec![(0, 0), (3_600_000, 40)],
        "the named account's rewrite tokens count, and the other account's in the same hour do not"
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
            account_kind: AccountKind::Oauth,
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

/// A 7-day range from 21:31 on the epoch's first day reads six-hour buckets, whose first edge, 18:00, lies before the
/// hour that holds the start. The account reads sum the rollups from 21:00 alone, each through its own call, while the
/// points keep the bucket grid from 18:00: the rows of 20:00 join no point and no move, and the rows of the second
/// day's 00:00 fill the second point, the one at that edge.
#[test]
fn the_account_reads_of_a_range_sum_from_the_hour_that_holds_its_start_on_the_bucket_grid() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_account(
            &tx,
            &phoebis_ledger::AccountRollupRow {
                hour: 72_000,
                account_id: "a".into(),
                rewrite_tokens: 700,
                migrations: BTreeMap::from([("spent/fits".to_string(), 1)]),
                ..Default::default()
            },
        )?;
        fold_account(
            &tx,
            &phoebis_ledger::AccountRollupRow {
                hour: 75_600,
                account_id: "a".into(),
                rewrite_tokens: 40,
                migrations: BTreeMap::from([("shed/fits".to_string(), 1)]),
                ..Default::default()
            },
        )?;
        fold_account(
            &tx,
            &phoebis_ledger::AccountRollupRow {
                hour: 86_400,
                account_id: "a".into(),
                rewrite_tokens: 300,
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let held = |id: &str, received_at: i64, held_ms: u64| {
        let request = RequestRecord {
            id: id.into(),
            received_at,
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            held: Some(format!(
                r#"{{"waited_on":["a"],"cohort":null,"held_ms":{held_ms},"waits":[{{"reason":"slow_down","held_ms":{held_ms}}}]}}"#
            )),
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            outcome: "succeeded".into(),
            output_tokens: Some(5),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    };
    // 20:10, 21:10 and the second day's 00:10, a slow-down wait in each of the three hours.
    held("before", 72_600_000, 7_000);
    held("first-hour", 76_200_000, 2_000);
    held("second-point", 87_000_000, 3_000);
    let filter = SeriesFilter {
        from: 77_460_000,
        to: 77_460_000 + 7 * DAY_MS,
        bucket_seconds: SIX_HOURS,
        ..SeriesFilter::default()
    };
    assert_eq!(
        db.read_blocking(|c| rewrite_series(c, &filter)).unwrap(),
        vec![
            (64_800_000, 40),
            (86_400_000, 300),
            (108_000_000, 0),
            (129_600_000, 0),
            (151_200_000, 0),
            (172_800_000, 0),
            (194_400_000, 0),
            (216_000_000, 0),
            (237_600_000, 0),
            (259_200_000, 0),
            (280_800_000, 0),
            (302_400_000, 0),
            (324_000_000, 0),
            (345_600_000, 0),
            (367_200_000, 0),
            (388_800_000, 0),
            (410_400_000, 0),
            (432_000_000, 0),
            (453_600_000, 0),
            (475_200_000, 0),
            (496_800_000, 0),
            (518_400_000, 0),
            (540_000_000, 0),
            (561_600_000, 0),
            (583_200_000, 0),
            (604_800_000, 0),
            (626_400_000, 0),
            (648_000_000, 0),
            (669_600_000, 0),
        ],
        "the first point at the 18:00 edge holds the 21:00 row's tokens alone, the second the 00:00 row's, the last the \
         bucket that holds the end"
    );
    assert_eq!(
        db.read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
            .unwrap(),
        BTreeMap::from([(
            "shed".to_string(),
            BTreeMap::from([("fits".to_string(), 1)])
        )]),
        "the 21:00 row's move alone"
    );
    assert_eq!(
        db.read_blocking(|c| held_series(c, &filter)).unwrap()[..2],
        [(64_800_000, 2_000, 1), (86_400_000, 3_000, 1)],
        "the first point at the 18:00 edge holds the 21:10 wait alone, the second the 00:10 wait"
    );
}

/// The six-hour bucket of the series readers' range cases.
const SIX_HOURS: i64 = 21_600;

/// What each series reader answers over the empty ledger `db` from the epoch to `to_hour`, in six-hour buckets, with
/// its points dropped.
async fn series_readers_over(
    db: &std::sync::Arc<LedgerDb>,
    to_hour: i64,
) -> Vec<(&'static str, Result<(), phoebis_ledger::LedgerError>)> {
    let from_hour = 0;
    let filter = SeriesFilter {
        from: from_hour * 1_000,
        to: to_hour * 1_000,
        bucket_seconds: SIX_HOURS,
        ..SeriesFilter::default()
    };
    let live = LiveKeys::default();
    vec![
        (
            "series",
            series(
                &[],
                filter.from,
                filter.to,
                SIX_HOURS,
                GroupBy::All,
                false,
                &live,
                &[],
                &PRICES,
                &COSTS,
                NOW,
            )
            .map(drop),
        ),
        (
            "series_paged",
            series_paged(
                db,
                &RollupFilter::default(),
                &live,
                filter.from,
                filter.to,
                SIX_HOURS,
                GroupBy::All,
                &PRICES,
                &COSTS,
                NOW,
            )
            .await
            .map(drop),
        ),
        (
            "pool_traffic",
            pool_traffic(
                db,
                "p",
                from_hour,
                to_hour,
                SIX_HOURS,
                std::sync::Arc::new(PRICES.clone()),
                std::sync::Arc::new(COSTS.clone()),
                NOW,
            )
            .await
            .map(drop),
        ),
        (
            "pools_traffic",
            db.read_blocking(|c| {
                pools_traffic(
                    c,
                    &["p".to_string()],
                    from_hour,
                    to_hour,
                    SIX_HOURS,
                    &PRICES,
                    &COSTS,
                    NOW,
                )
            })
            .map(drop),
        ),
        (
            "burn_rows",
            db.read_blocking(|c| burn_rows(c, &filter)).map(drop),
        ),
        (
            "burn_series_over",
            burn_series_over(&[], &COSTS, &PRICES, &filter, &LiveKeys::default(), NOW).map(drop),
        ),
        (
            "burn_groups_paged",
            burn_groups_paged(db, &COSTS, &PRICES, &filter, GroupBy::All, &live, &[], NOW)
                .await
                .map(drop),
        ),
        (
            "rewrite_series",
            db.read_blocking(|c| rewrite_series(c, &filter)).map(drop),
        ),
        (
            "held_series",
            db.read_blocking(|c| held_series(c, &filter)).map(drop),
        ),
        (
            "moves_by_reason_and_fit",
            db.read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
                .map(drop),
        ),
        (
            "session_burn_series",
            db.read_blocking(|c| {
                phoebis_ledger::session_burn_series(
                    c,
                    &COSTS,
                    &PRICES,
                    &filter,
                    &LiveKeys::default(),
                    NOW,
                )
            })
            .map(drop),
        ),
    ]
}

/// 10,000 whole six-hour buckets and one hour more: 10,001 points once the trailing partial bucket counts as the
/// point the readers answer for it, one past `MAX_POINTS`, where a whole-bucket count reads 10,000 and lets it pass.
#[tokio::test]
async fn every_series_reader_refuses_a_range_whose_trailing_partial_bucket_is_one_point_too_many() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let accepted: Vec<&str> = series_readers_over(&db, 10_000 * SIX_HOURS + 3_600)
        .await
        .into_iter()
        .filter(|(_, answer)| !matches!(answer, Err(phoebis_ledger::LedgerError::Range(_))))
        .map(|(reader, _)| reader)
        .collect();
    assert_eq!(
        accepted,
        Vec::<&str>::new(),
        "each reader answers the range refusal"
    );
}

/// Exactly 10,000 whole six-hour buckets: `MAX_POINTS` points with no trailing partial bucket, the last range the
/// guard lets through, where a count that rounds a whole bucket up reads 10,001 and refuses it.
#[tokio::test]
async fn a_range_of_exactly_the_most_points_answers_from_every_series_reader() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let refused: Vec<(&str, String)> = series_readers_over(&db, 10_000 * SIX_HOURS)
        .await
        .into_iter()
        .filter_map(|(reader, answer)| answer.err().map(|e| (reader, e.to_string())))
        .collect();
    assert_eq!(
        refused,
        Vec::<(&str, String)>::new(),
        "each reader answers the range"
    );
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
        account_kind: AccountKind::Oauth,
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
        account_kind: AccountKind::Oauth,
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

    // A retention window shorter than the month purges the raw request and its attempt. The rollup that
    // folded them keeps its default 365-day life, so the query still answers the same spend.
    let purged = run_retention_blocking(
        &db,
        &LedgerConfig {
            retention_days: 5,
            ..LedgerConfig::default()
        },
        now,
        &[],
        &PriceRevisions::shipped(),
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
        &PriceRevisions::shipped(),
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
        account_kind: AccountKind::Oauth,
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
    plant_placement(&db, "out", "inference", 3_700_000, "content", 500, 500);

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

/// The placements of the whole hours a range touches count, from the hour that holds its start up to and not at the
/// hour ceiling of its end.
#[test]
fn placements_count_the_whole_hours_a_range_touches_up_to_the_hour_ceiling_of_its_end() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_placement(
        &db,
        "before",
        "inference",
        BEFORE_THE_FIRST_HOUR,
        "content",
        500,
        500,
    );
    plant_placement(
        &db,
        "first-hour",
        "inference",
        IN_THE_FIRST_HOUR,
        "content",
        500,
        500,
    );
    plant_placement(
        &db,
        "last-hour",
        "inference",
        IN_THE_LAST_HOUR,
        "content",
        500,
        500,
    );
    plant_placement(
        &db,
        "ceiling",
        "inference",
        AT_THE_HOUR_CEILING,
        "content",
        500,
        500,
    );
    let rows = db
        .read_blocking(|c| {
            placements(
                c,
                &RollupFilter::default(),
                &LiveKeys::default(),
                WINDOW_FROM,
                WINDOW_TO,
            )
        })
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.rung.as_str(), r.placements, r.hits))
            .collect::<Vec<_>>(),
        vec![("content", 2, 2)],
        "the placements at 21:10 and 22:40 count, the ones at 20:59 and 23:00 do not"
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
            account_kind: AccountKind::Oauth,
            outcome: "rate_limited".into(),
            selection: Some(trace.clone()),
            ..Default::default()
        },
        AttemptRecord {
            request_id: id.into(),
            attempt: 2,
            account_id: "b".into(),
            account_kind: AccountKind::Oauth,
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
        account_kind: AccountKind::Oauth,
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
                    clients: Patterns::new(vec!["sdk/*".into()]).unwrap(),
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

/// A `content` placement at 1,000 ms that promises 500 tokens, its request row keeping the caller's client label and
/// `User-Agent` header as sent, `None` where the caller sent none.
fn plant_caller_placement(
    db: &LedgerDb,
    id: &str,
    client: Option<&str>,
    user_agent: Option<&str>,
    cache_read_tokens: u64,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at: 1_000,
        finished_at: Some(1_500),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        client: client.map(String::from),
        user_agent: user_agent.map(String::from),
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
        account_kind: AccountKind::Oauth,
        outcome: "succeeded".into(),
        cache_read_tokens: Some(cache_read_tokens),
        selection: Some(r#"{"placement":{"rung":"content","depth":1,"prefix_tokens":500}}"#.into()),
        ..Default::default()
    };
    db.insert_request(&request, &[attempt], None).unwrap();
}

/// The placements over the planted range under the two pattern lists.
fn placements_under(db: &LedgerDb, clients: &[&str], user_agents: &[&str]) -> Vec<PlacementRow> {
    let words =
        |list: &[&str]| Patterns::new(list.iter().map(|w| w.to_string()).collect()).unwrap();
    let filter = RollupFilter {
        clients: words(clients),
        user_agents: words(user_agents),
        ..Default::default()
    };
    db.read_blocking(|c| placements(c, &filter, &LiveKeys::default(), 0, 10_000))
        .unwrap()
}

/// Each read's rungs with their placement counts.
fn placed(rows: &[PlacementRow]) -> Vec<(&str, u64)> {
    rows.iter()
        .map(|r| (r.rung.as_str(), r.placements))
        .collect()
}

#[test]
fn placements_match_a_user_agent_pattern_against_the_agents_first_product_token() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_caller_placement(&db, "cli-310", Some("claude-code/cli"), Some(CLI_310), 500);
    plant_caller_placement(
        &db,
        "sdk-310",
        Some("claude-code/sdk-cli"),
        Some(SDK_310),
        100,
    );
    plant_caller_placement(&db, "cli-311", Some("claude-code/cli"), Some(CLI_311), 500);
    plant_caller_placement(&db, "bare", None, None, 500);
    assert_eq!(
        placements_under(&db, &[], &["claude-cli/2.1.310"]),
        vec![PlacementRow {
            rung: "content".into(),
            placements: 2,
            hits: 1,
            cache_read_tokens: 600,
            promised_tokens: 1_000,
        }],
        "the token spelled in full counts the version's placements under both entrypoints in one row, and none of \
         2.1.311's"
    );
    assert_eq!(
        placements_under(&db, &[], &[CLI_310]),
        Vec::new(),
        "the header with its comment is no token, so it counts nothing, as it names no group of the series"
    );
    assert_eq!(
        placed(&placements_under(&db, &[], &["-"])),
        vec![("content", 1)],
        "a row with no user agent reads `-`, as the rollups file it"
    );
}

#[test]
fn placements_match_a_client_pattern_against_the_label_the_rollups_fold() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_caller_placement(&db, "cli", Some("claude-code/cli"), None, 500);
    // The relay writes an entrypoint of 60 letters into the label whole, past the 64 bytes the rollups keep.
    plant_caller_placement(
        &db,
        "long",
        Some("claude-code/entrypointentrypointentrypointentrypointentrypointentrypoint"),
        None,
        500,
    );
    plant_caller_placement(&db, "bare", None, None, 500);
    assert_eq!(
        placed(&placements_under(&db, &["claude-code/*"], &[])),
        vec![("content", 1)],
        "the wildcard counts `claude-code/cli`, and the label past the bound folds to `other`, which it does not match"
    );
    assert_eq!(
        placed(&placements_under(&db, &["-"], &[])),
        vec![("content", 1)],
        "a row with no client reads `-`, as the rollups file it"
    );
    assert_eq!(
        placed(&placements_under(&db, &[OTHER_DIMENSION], &[])),
        vec![("content", 1)],
        "the label past the bound reads `other`, as the rollups file it"
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
    plant_identity(&db, "r1", "k1", 0, "s1", "inference", 0, None, None);
    assert!(
        db.read_blocking(|c| phoebis_ledger::session_activity(c, 500))
            .unwrap()
            .is_empty(),
        "the same session is outside session_activity's own since bound"
    );
    let rows = db
        .read_blocking(|c| {
            session_identities(
                c,
                &[SessionSpendKey {
                    key_id: "k1".into(),
                    key_generation: 0,
                    session_id: "s1".into(),
                }],
            )
        })
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
    plant_identity(&db, "r2", "k1", 0, "s2", "inference", 2_000, None, None);
    let rows = db
        .read_blocking(|c| {
            session_identities(
                c,
                &[SessionSpendKey {
                    key_id: "k1".into(),
                    key_generation: 0,
                    session_id: "s2".into(),
                }],
            )
        })
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
        .read_blocking(|c| {
            session_identities(
                c,
                &[SessionSpendKey {
                    key_id: "k1".into(),
                    key_generation: 0,
                    session_id: "s2".into(),
                }],
            )
        })
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
        .read_blocking(|c| {
            session_identities(
                c,
                &[SessionSpendKey {
                    key_id: "k1".into(),
                    key_generation: 0,
                    session_id: "s3".into(),
                }],
            )
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].forked_from.as_deref(), rows[0].fork_depth),
        (None, None),
        "an auxiliary row's marker is not the session's"
    );
}

#[test]
fn a_forked_sessions_identity_skips_a_probe_and_another_keys_session_of_the_same_id() {
    let db = LedgerDb::open_in_memory().unwrap();
    // The probe is the earliest row of the id and the other key's row lands before the session's own first request,
    // so a lookup that drops either filter answers that row's facts.
    plant_identity(&db, "r1", "k1", 0, "s2", "probe", 500, None, None);
    plant_identity(
        &db,
        "r2",
        "k2",
        0,
        "s2",
        "inference",
        700,
        Some("s9"),
        Some(1),
    );
    plant_identity(
        &db,
        "r3",
        "k1",
        0,
        "s2",
        "inference",
        1_000,
        Some("s1"),
        Some(3),
    );
    let rows = db
        .read_blocking(|c| {
            session_identities(
                c,
                &[SessionSpendKey {
                    key_id: "k1".into(),
                    key_generation: 0,
                    session_id: "s2".into(),
                }],
            )
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].forked_from.as_deref(), rows[0].fork_depth),
        (Some("s1"), Some(3)),
        "the fork facts come from the session's own first client request under its key"
    );
}

#[test]
fn a_session_the_ledger_holds_no_client_request_for_is_absent_from_the_answer() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_identity(&db, "r1", "k1", 0, "s3", "probe", 1_000, None, None);
    let rows = db
        .read_blocking(|c| {
            session_identities(
                c,
                &[SessionSpendKey {
                    key_id: "k1".into(),
                    key_generation: 0,
                    session_id: "s3".into(),
                }],
            )
        })
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
        named.push(SessionSpendKey {
            key_id: "k1".to_string(),
            key_generation: 0,
            session_id: session,
        });
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
    let (key_lifetime, key_since) = db
        .read_blocking(|c| {
            Ok((
                key_request_totals(c, i64::MIN, to, &live, &PRICES, &COSTS, NOW)?,
                key_request_totals(c, hour_of(first), to, &live, &PRICES, &COSTS, NOW)?,
            ))
        })
        .unwrap();
    let key = &key_lifetime["desktop"];
    assert_eq!(key.requests, 2, "{key:?}");
    assert_eq!(key_lifetime, key_since);
}

/// The account list reads its request counts apart from the priced totals, so the counts keep the totals' rule on
/// probes themselves: a probe served beside two requests of one account counts the two requests alone. With two
/// requests the count differs from the probe's row alone and from all three rows.
#[test]
fn the_account_request_counts_leave_the_probe_rows_out() {
    let db = LedgerDb::open_in_memory().unwrap();
    let at = ms(2026, 1, 10, 3, 0);
    plant_spend(
        &db,
        "r1",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        at,
        1_000,
    );
    plant_spend(&db, "r2", "p", "a", "claude-sonnet-5", "probe", at, 1_000);
    plant_spend(
        &db,
        "r3",
        "p",
        "a",
        "claude-sonnet-5",
        "inference",
        at,
        1_000,
    );
    let counts = db
        .read_blocking(|c| account_request_counts(c, i64::MIN, hour_of(at) + 3_600))
        .unwrap();
    assert_eq!(counts, BTreeMap::from([("a".to_string(), 2)]));
}

/// The totals price each group at its own hour, so a cost revision that takes effect inside one price revision's
/// span moves the figure at its own date: the hour before 2026-01-15 reads the first entry's weights and the hour
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
    let keys = db
        .read_blocking(|c| key_request_totals(c, from_hour, to_hour, &live, &prices, &costs, NOW))
        .unwrap();
    let at = |instant: i64| {
        let hour = hour_of(instant) * 1_000;
        costs
            .equivalent_at(
                SEVEN_DAY,
                None,
                "claude-opus-5-5",
                usage(1_000, 0),
                hour,
                hour,
                NOW,
            )
            .expect("the test tables weigh Opus 5.5")
    };
    let (first, second) = (at(before), at(after));
    assert!(
        second > first,
        "the second cost entry weighs the model more: {first} vs {second}"
    );
    let totals = &keys["desktop"];
    assert!(
        totals
            .equivalent
            .is_some_and(|got| (got - (first + second)).abs() < 1e-12),
        "each row at its own cost entry: {totals:?} vs {}",
        first + second
    );
}

/// Two rows of one hour, account tier and model, one Opus 5.5 input token each, one from an entrypoint the test regime
/// lists and one from an entrypoint it leaves out: each prices under its own regime, 2 / 200 and 2 / 100 of the Ultra
/// 5-hour window, where one price over the group would read the base 0.04. A session attempt from the listed
/// entrypoint reads the regime's share. 04:00 in Los Angeles, before the band, in the first cost entry.
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
    let points = db
        .read_blocking(|c| burn_series(c, &costs, &prices, &filter, &LiveKeys::default(), NOW))
        .unwrap();
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
        .read_blocking(|c| {
            phoebis_ledger::session_burn_series(
                c,
                &costs,
                &prices,
                &filter,
                &LiveKeys::default(),
                NOW,
            )
        })
        .unwrap();
    let share = sessions
        .iter()
        .find(|(id, _, _)| id == "s")
        .and_then(|(_, _, points)| points.iter().find_map(|p| p.five_hour_share))
        .unwrap_or_else(|| panic!("the session's hour carries its share: {sessions:?}"));
    assert!(close(share, 0.01), "the attempt under the regime: {share}");
}

/// The dashboard's day and its hour and a pool's traffic bucket read the billed rows' dollars apart and the plan rows
/// in the Pro-week figure.
#[tokio::test]
async fn the_dashboard_s_day_and_hour_and_the_pool_s_bucket_split_the_billed_dollars_from_the_plan_rows_equivalent(
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
            account_kind: AccountKind::ApiKey,
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
    let figure = equivalent_of(&COSTS, "claude-opus-5-5", plan, WEEKDAY_OFF_PEAK);
    let read = |got_billed: f64, got: Option<f64>, what: &str| {
        assert!(
            (got_billed - billed).abs() < 1e-12
                && got.is_some_and(|got| (got - figure).abs() < 1e-12),
            "{what}: {got_billed} and {got:?} vs {billed} and {figure}"
        );
    };
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let day = db
        .read(move |c| phoebis_ledger::dashboard_on(c, WEEKDAY_OFF_PEAK, &PRICES, &COSTS))
        .await
        .unwrap();
    read(day.billed_24h, day.equivalent_24h, "the day");
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    read(point.billed, point.equivalent, "the hour");
    let traffic = phoebis_ledger::pool_traffic(
        &db,
        "p",
        hour,
        hour + 3_600,
        3_600,
        std::sync::Arc::new(PRICES.clone()),
        std::sync::Arc::new(COSTS.clone()),
        NOW,
    )
    .await
    .unwrap();
    read(
        traffic[0].billed_usd,
        traffic[0].equivalent,
        "the pool's bucket",
    );
}

/// A row the landing did not flag is a rejected request like any other: the day's requests, the hour's errors, the
/// error rate and the most common code count its guardrail blocks with the other rejections.
#[tokio::test]
async fn the_dashboard_counts_an_unflagged_block_in_the_rate_and_the_top_code_like_any_other_rejection(
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
    let day = db
        .read(move |c| phoebis_ledger::dashboard_on(c, WEEKDAY_OFF_PEAK, &PRICES, &COSTS))
        .await
        .unwrap();
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    assert_eq!(
        (day.requests_24h, point.requests, point.errors),
        (10, 10, 4),
        "the unflagged blocks count among the requests and among the hour's errors"
    );
    assert_eq!(day.error_rate_24h, Some(4.0 / 10.0));
    assert_eq!(day.top_error_code.as_deref(), Some("guardrail_blocked"));
}

/// A request canceled under `code` on the pool `p` in the minute before `WEEKDAY_OFF_PEAK`, with no attempt.
fn canceled_at(n: i64, code: &str) -> RequestRecord {
    RequestRecord {
        id: format!("r{n}"),
        received_at: WEEKDAY_OFF_PEAK - 60_000 + n,
        key_id: Some("k".into()),
        pool_id: Some("p".into()),
        model: "claude-opus-5-5".into(),
        canonical_model: "claude-opus-5-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "canceled".into(),
        error_code: Some(code.into()),
        ..RequestRecord::default()
    }
}

/// A client that left is a request and no error: the day's errors, its rate and its most common code read the
/// account's cancel alone, whose code sorts first, so a tie would name the client's.
#[tokio::test]
async fn the_dashboard_counts_a_client_that_left_among_the_requests_and_not_the_errors() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_request(&canceled_at(0, "account_canceled"), &[], None)
        .unwrap();
    db.insert_request(&canceled_at(1, "client_gone"), &[], None)
        .unwrap();
    let hour = hour_of(WEEKDAY_OFF_PEAK - 60_000);
    let day = db
        .read(move |c| phoebis_ledger::dashboard_on(c, WEEKDAY_OFF_PEAK, &PRICES, &COSTS))
        .await
        .unwrap();
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    assert_eq!(
        (day.requests_24h, point.errors, day.error_rate_24h),
        (2, 1, Some(0.5)),
        "the account's cancel is the one error among the two requests"
    );
    assert_eq!(day.top_error_code.as_deref(), Some("account_canceled"));
}

/// Six requests of the key `desktop` in the minute before `WEEKDAY_OFF_PEAK` on the pool `p`: two guardrail blocks
/// and a contract refusal, each flagged with no attempt as the landing flags a refusal, a `client_not_allowed`
/// rejection after one attempt, a succeeded request and a 529. The succeeded request and the two blocks share the
/// session `s1`, the contract refusal is alone on `s2`, and the rejection and the 529 carry no session.
fn plant_asked_and_refused(db: &LedgerDb) {
    let row = |n: i64, session: Option<&str>, outcome: &str, status: u16, code: Option<&str>| {
        RequestRecord {
            id: format!("r{n}"),
            received_at: WEEKDAY_OFF_PEAK - 60_000 + n,
            finished_at: Some(WEEKDAY_OFF_PEAK - 60_000 + n + 4),
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            session_id: session.map(String::from),
            model: "claude-opus-5-5".into(),
            canonical_model: "claude-opus-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(status),
            outcome: outcome.into(),
            duration_ms: Some(4),
            error_code: code.map(String::from),
            ..RequestRecord::default()
        }
    };
    let refused = |n: i64, session: &str, status: u16, code: &str| RequestRecord {
        attempts: 0,
        refused: true,
        ..row(n, Some(session), "rejected", status, Some(code))
    };
    let asked = |n: i64, outcome: &str, status: u16, code: Option<&str>, session: Option<&str>| {
        let attempt = AttemptRecord {
            request_id: format!("r{n}"),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            started_at: WEEKDAY_OFF_PEAK - 60_000 + n,
            upstream_status: Some(status),
            outcome: if status == 200 { "succeeded" } else { "failed" }.into(),
            ..Default::default()
        };
        let request = RequestRecord {
            attempts: 1,
            ..row(n, session, outcome, status, code)
        };
        (request, vec![attempt])
    };
    for request in [
        refused(0, "s1", 403, "guardrail_blocked"),
        refused(1, "s1", 403, "guardrail_blocked"),
        refused(2, "s2", 403, "client_contract"),
    ] {
        db.insert_request(&request, &[], None).unwrap();
    }
    for (request, attempts) in [
        asked(3, "rejected", 403, Some("client_not_allowed"), None),
        asked(4, "succeeded", 200, None, Some("s1")),
        asked(5, "failed", 529, Some("overloaded_error"), None),
    ] {
        db.insert_request(&request, &attempts, None).unwrap();
    }
}

/// The pool's traffic and the key's totals read the folded rows, which hold the asked requests alone, so the refused
/// requests stay out of the requests and the failed count without a rule of the readers' own.
#[tokio::test]
async fn the_pool_traffic_and_the_key_totals_count_the_asked_requests_alone() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_asked_and_refused(&db);
    let hour = hour_of(WEEKDAY_OFF_PEAK - 60_000);
    let traffic = phoebis_ledger::pool_traffic(
        &db,
        "p",
        hour,
        hour + 3_600,
        3_600,
        std::sync::Arc::new(PRICES.clone()),
        std::sync::Arc::new(COSTS.clone()),
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        (traffic[0].requests, traffic[0].failed),
        (3, 2),
        "the rejection and the 529 among the three asked requests"
    );
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let totals = db
        .read_blocking(|c| key_request_totals(c, hour, hour + 3_600, &live, &PRICES, &COSTS, NOW))
        .unwrap();
    assert_eq!(totals["desktop"].requests, 3, "the three asked requests");
}

#[tokio::test]
async fn pools_traffic_answers_every_asked_pool_and_adds_each_rows_figures_into_its_bucket() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let last = WEEKDAY_OFF_PEAK - 60_000;
    let hour = hour_of(last);
    for (n, pool, account, at) in [
        (1, "p", "a", last + 1),
        (2, "p", "gone", last + 2),
        (3, "q", "b", last + 3),
    ] {
        let request = RequestRecord {
            id: format!("r{n}"),
            received_at: at,
            finished_at: Some(at + 4),
            key_id: Some("k".into()),
            pool_id: Some(pool.into()),
            model: "claude-opus-5-5".into(),
            canonical_model: "claude-opus-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..RequestRecord::default()
        };
        let attempt = AttemptRecord {
            request_id: format!("r{n}"),
            attempt: 1,
            account_id: account.into(),
            account_kind: AccountKind::Oauth,
            started_at: at,
            upstream_status: Some(200),
            outcome: "succeeded".into(),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    }
    let traffic = db
        .read_blocking(|c| {
            pools_traffic(
                c,
                &["p".into(), "q".into()],
                hour,
                hour + 3_600,
                3_600,
                &PRICES,
                &COSTS,
                NOW,
            )
        })
        .unwrap();
    let requests = |pool: &str, at: i64| {
        traffic[pool]
            .iter()
            .find(|bucket| bucket.at == at * 1_000)
            .map(|bucket| bucket.requests)
    };
    assert_eq!(traffic.keys().collect::<Vec<_>>(), ["p", "q"]);
    assert_eq!(
        requests("p", hour),
        Some(2),
        "a former member's row counts for the hour it was a member"
    );
    assert_eq!(requests("q", hour), Some(1));
}

/// The pools' statement takes the client's cancels off its own complement, apart from the dashboard's fold.
#[test]
fn pools_traffic_reads_a_client_that_left_as_a_request_and_no_failed_request() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(&canceled_at(0, "client_gone"), &[], None)
        .unwrap();
    let hour = hour_of(WEEKDAY_OFF_PEAK - 60_000);
    let traffic = db
        .read_blocking(|c| {
            pools_traffic(
                c,
                &["p".into()],
                hour,
                hour + 3_600,
                3_600,
                &PRICES,
                &COSTS,
                NOW,
            )
        })
        .unwrap();
    assert_eq!((traffic["p"][0].requests, traffic["p"][0].failed), (1, 0));
}

/// A key's totals read the dollars of a usage-based seat's probe in their cost and billed dollars and count none of its
/// requests or tokens, while a plan probe of the same key adds nothing. One dollar a token, so the plan request's 1,100
/// tokens cost $1,100, the seat's probe $23 and the plan probe $400.
#[test]
fn a_key_s_totals_add_a_billed_probe_s_dollars_and_no_probe_s_request_or_token() {
    let db = LedgerDb::open_in_memory().unwrap();
    let model = "claude-sonnet-5-5";
    plant_share(
        &db,
        ShareRow {
            id: "r-plan",
            model,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-seat-probe",
            account: "seat",
            kind: "probe",
            model,
            usage: usage(20, 3),
            usage_based: true,
            ..ShareRow::default()
        },
    );
    plant_share(
        &db,
        ShareRow {
            id: "r-plan-probe",
            kind: "probe",
            model,
            usage: usage(400, 0),
            ..ShareRow::default()
        },
    );
    let (prices, costs) = (
        priced_table(model, 1.0),
        phoebis_ledger::test_tables::subscription_costs(),
    );
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let live = LiveKeys(BTreeMap::from([("k".to_string(), 0)]));
    let totals = db
        .read_blocking(|c| key_request_totals(c, hour, hour + 3_600, &live, &prices, &costs, NOW))
        .unwrap();
    let k = &totals["k"];
    assert_eq!(
        (k.requests, k.tokens, k.cost_usd, k.billed_usd),
        (1, 1_100, 1_123.0, 23.0),
        "{k:?}"
    );
}

/// On the pool `p` at `WEEKDAY_OFF_PEAK`, a plan request of 1,000 input and 100 output tokens and an overage probe that
/// failed with a code after its answer read 300 cached tokens and 20 input tokens, both on Sonnet 5.5.
fn plant_a_request_and_a_failed_billed_probe(db: &LedgerDb) {
    let model = "claude-sonnet-5-5";
    plant_share(
        db,
        ShareRow {
            id: "r-plan",
            model,
            ..ShareRow::default()
        },
    );
    let probe = RequestRecord {
        id: "r-probe".into(),
        received_at: WEEKDAY_OFF_PEAK,
        finished_at: Some(WEEKDAY_OFF_PEAK + 500),
        key_id: Some("k".into()),
        pool_id: Some("p".into()),
        canonical_model: model.into(),
        model: model.into(),
        kind: "probe".into(),
        origin: "client".into(),
        status: Some(500),
        outcome: "failed".into(),
        error_code: Some("upstream_api_error".into()),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: "r-probe".into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        started_at: WEEKDAY_OFF_PEAK,
        finished_at: Some(WEEKDAY_OFF_PEAK + 500),
        upstream_status: Some(500),
        outcome: "failed".into(),
        input_tokens: Some(20),
        cache_read_tokens: Some(300),
        cache_write_5m_tokens: Some(0),
        cache_write_1h_tokens: Some(0),
        output_tokens: Some(0),
        overage_in_use: Some(true),
        ..Default::default()
    };
    db.insert_request(&probe, &[attempt], None).unwrap();
}

/// The pool's statement holds a clause of its own, so its bucket adds the failed overage probe's dollars and reads no
/// request, no token and no failed request from it. One dollar a token, so the request costs $1,100 and the probe $320.
#[tokio::test]
async fn the_pool_s_bucket_adds_a_billed_probe_s_dollars_and_no_request_or_failure_of_it() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_a_request_and_a_failed_billed_probe(&db);
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let traffic = phoebis_ledger::pool_traffic(
        &db,
        "p",
        hour,
        hour + 3_600,
        3_600,
        std::sync::Arc::new(priced_table("claude-sonnet-5-5", 1.0)),
        std::sync::Arc::new(phoebis_ledger::test_tables::subscription_costs()),
        NOW,
    )
    .await
    .unwrap();
    let bucket = &traffic[0];
    assert_eq!(
        (
            bucket.requests,
            bucket.failed,
            bucket.tokens,
            bucket.cost_usd,
            bucket.billed_usd
        ),
        (1, 0, 1_100, 1_420.0, 320.0),
        "{bucket:?}"
    );
}

/// The Dashboard's day adds the failed overage probe's dollars and no other figure of it: the requests, the errors, the
/// refused count, the most common code and the cached tokens read the plan request alone. One dollar a token, so the
/// request costs $1,100 and the probe $320.
#[tokio::test]
async fn the_dashboard_s_day_adds_a_billed_probe_s_dollars_and_no_other_figure_of_it() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_a_request_and_a_failed_billed_probe(&db);
    let (prices, costs) = (
        priced_table("claude-sonnet-5-5", 1.0),
        phoebis_ledger::test_tables::subscription_costs(),
    );
    let day = db
        .read(move |c| phoebis_ledger::dashboard_on(c, WEEKDAY_OFF_PEAK, &prices, &costs))
        .await
        .unwrap();
    let hour = hour_of(WEEKDAY_OFF_PEAK);
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    assert_eq!(
        (day.cost_24h, day.billed_24h, point.cost, point.billed),
        (1_420.0, 320.0, 1_420.0, 320.0),
        "the day and its hour hold the probe's dollars"
    );
    assert_eq!(
        (
            day.requests_24h,
            point.errors,
            day.refused_24h,
            day.top_error_code.as_deref(),
            day.tokens_24h,
            day.tokens_cached_24h
        ),
        (1, 0, 0, None, 1_100, 0),
        "every other figure reads the plan request alone"
    );
}

/// The day's requests, its error rate and its most common error code read the asked requests alone, and the refused
/// requests read in the day's refused count, its most common refusal code and the hour's refused count.
#[tokio::test]
async fn the_dashboard_leaves_the_refused_requests_out_of_its_counts_and_reads_them_in_its_own_members(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    plant_asked_and_refused(&db);
    let hour = hour_of(WEEKDAY_OFF_PEAK - 60_000);
    let day = db
        .read(move |c| phoebis_ledger::dashboard_on(c, WEEKDAY_OFF_PEAK, &PRICES, &COSTS))
        .await
        .unwrap();
    let point = day
        .hourly
        .iter()
        .find(|h| h.at == hour * 1_000)
        .expect("the planted hour");
    assert_eq!(
        (day.requests_24h, point.requests, point.refused),
        (3, 3, 3),
        "the rejection and the two served requests are the requests, the three refusals the refused"
    );
    assert_eq!(
        day.error_rate_24h,
        Some(2.0 / 3.0),
        "the failed and the rejected over the three asked requests"
    );
    assert!(
        matches!(
            day.top_error_code.as_deref(),
            Some("client_not_allowed" | "overloaded_error")
        ),
        "an asked request's code, never a refusal's: {:?}",
        day.top_error_code
    );
    assert_eq!(
        (day.refused_24h, day.top_refusal_code.as_deref()),
        (3, Some("guardrail_blocked")),
        "two blocks over one contract refusal"
    );
}

/// A group reads its refused requests in its refused count and its refusals by code, apart from its requests.
#[test]
fn the_breakdown_answers_refused_and_refusals_per_group() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_asked_and_refused(&db);
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default());
    let groups = breakdown(
        &filtered,
        GroupBy::Key,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    let desktop = group(&groups, "desktop");
    assert_eq!(
        (desktop.requests, desktop.refused, desktop.refusals.clone()),
        (
            3,
            3,
            BTreeMap::from([
                ("client_contract".to_string(), 1),
                ("guardrail_blocked".to_string(), 2),
            ])
        ),
        "the refused requests beside the three asked ones and not among them"
    );
}

/// A group answers the store's reserved counts as members of their own, so its map holds the answers' keys alone.
#[test]
fn the_breakdown_answers_the_dropped_blocks_keys_apart_from_the_moved_count() {
    let db = LedgerDb::open_in_memory().unwrap();
    let request = RequestRecord {
        id: "r1".into(),
        received_at: WEEKDAY_OFF_PEAK,
        key_id: Some("desktop".into()),
        pool_id: Some("p".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let moved = AttemptRecord {
        request_id: "r1".into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        upstream_status: Some(200),
        outcome: "succeeded".into(),
        input_tokens: Some(100),
        output_tokens: Some(10),
        selection: Some(
            r#"{"strategy":"pace","tier":[0,1,0],"renewed":false,"moved":"shed","skipped":[]}"#
                .into(),
        ),
        input_transformations: Some(BTreeMap::from([(
            "thinking.end_user_binding_mismatch".to_string(),
            InputTransformation {
                blocks: 4,
                first_message: 1,
            },
        )])),
        ..Default::default()
    };
    db.insert_request(&request, &[moved], None).unwrap();
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let groups = breakdown(
        &apply_filter(rows, &RollupFilter::default()),
        GroupBy::Key,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    );
    let desktop = group(&groups, "desktop");
    assert_eq!(
        (
            desktop.input_transformations.clone(),
            desktop.input_transformations_moved
        ),
        (
            BTreeMap::from([("thinking.end_user_binding_mismatch".to_string(), 1)]),
            1
        ),
        "the answer's one key in the map, the move beside it"
    );
}

/// A request of `kind` on `model`, its one attempt answered with `input` uncached tokens and `read` read back under the
/// miss reason `reason`, and the marker fact `cache_marker`.
fn cached(
    id: &str,
    model: &str,
    kind: &str,
    (input, read): (u64, u64),
    reason: Option<&str>,
    cache_marker: Option<bool>,
) -> (RequestRecord, Vec<AttemptRecord>) {
    (
        RequestRecord {
            id: id.into(),
            received_at: WEEKDAY_OFF_PEAK,
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            canonical_model: model.into(),
            model: model.into(),
            kind: kind.into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            cache_marker,
            ..Default::default()
        },
        vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            upstream_status: Some(200),
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            cache_read_tokens: Some(read),
            cache_miss_reason: reason.map(String::from),
            ..Default::default()
        }],
    )
}

/// The groups of every row the ledger holds, by `by`.
fn groups_of(db: &LedgerDb, by: GroupBy) -> Vec<GroupRow> {
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    breakdown(
        &apply_filter(rows, &RollupFilter::default()),
        by,
        false,
        &live,
        &[],
        &PRICES,
        &COSTS,
        NOW,
    )
}

/// One of the reason's two requests read 950 of its 1,000 prompt tokens back, the other none.
#[test]
fn a_reasons_hits_answer_apart_from_the_reason_map() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, read) in [("hit", 950), ("miss", 0)] {
        let (request, attempts) = cached(
            id,
            "claude-sonnet-5",
            "inference",
            (1_000 - read, read),
            Some("messages_changed"),
            None,
        );
        db.insert_request(&request, &attempts, None).unwrap();
    }
    let groups = groups_of(&db, GroupBy::Key);
    let desktop = group(&groups, "desktop");
    assert_eq!(
        (
            desktop.cache_miss_reasons.clone(),
            desktop.cache_miss_reason_hits.clone()
        ),
        (
            BTreeMap::from([("messages_changed".to_string(), 2)]),
            BTreeMap::from([("messages_changed".to_string(), 1)])
        )
    );
}

/// The token count carries no marker, and its row's stored count reads 0 as an hour folded before the column does, so
/// the 1 comes from the read alone.
#[test]
fn a_token_count_answers_as_uncacheable_beside_a_hit() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, kind, read) in [("hit", "inference", 900), ("count", "count_tokens", 0)] {
        let (request, attempts) =
            cached(id, "claude-sonnet-5", kind, (100, read), None, Some(false));
        db.insert_request(&request, &attempts, None).unwrap();
    }
    let groups = groups_of(&db, GroupBy::Model);
    assert_eq!(group(&groups, "claude-sonnet-5").uncacheable, 1);
}

/// The refused rows stay in the window that picks a session's newest request, so a session whose every request was
/// refused is still listed.
#[test]
fn a_session_of_refusals_alone_is_still_listed() {
    let db = LedgerDb::open_in_memory().unwrap();
    plant_asked_and_refused(&db);
    let sessions = db
        .read_blocking(|c| phoebis_ledger::session_activity(c, WEEKDAY_OFF_PEAK - 3_600_000))
        .unwrap();
    let session = |id: &str| {
        sessions
            .iter()
            .find(|s| s.session_id == id)
            .unwrap_or_else(|| panic!("the session {id} is listed: {sessions:?}"))
    };
    assert_eq!(
        sessions.len(),
        2,
        "the rejection and the 529 carry no session: {sessions:?}"
    );
    assert_eq!(
        session("s2").last_request_at,
        WEEKDAY_OFF_PEAK - 60_000 + 2,
        "the contract refusal alone, listed with its newest request"
    );
}

/// A probe on a model the revision does not list reads no dollars for its counts or its wasted counts, as an inference
/// row on the same model reads none.
#[test]
fn a_probe_row_on_a_model_the_revision_lacks_reads_no_dollars() {
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
        GroupBy::Kind,
        true,
        &LiveKeys::default(),
        &[],
        &prices,
        &COSTS,
        NOW,
    );
    let probe = groups.iter().find(|g| g.key == "probe").unwrap();
    assert_eq!(
        (probe.probe_cost_usd, probe.wasted_probe_usd),
        (0.0, 0.0),
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
            account_kind: AccountKind::ApiKey,
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
            .estimate(
                model,
                Speed::Standard,
                PinUsage {
                    input,
                    cache_read: 0,
                    write_5m: 0,
                    write_1h: 0,
                    output: 0,
                },
            )
            .expect("the test list carries the model")
            .total()
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
            account_kind: AccountKind::ApiKey,
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

/// A price list whose second entry takes effect at 18:30 UTC on 2026-01-15, inside an hour, and adds Opus 5.5 at $10
/// per million input tokens.
const AN_ENTRY_INSIDE_AN_HOUR: &str = "\
revisions:
  - effective: 2026-01-01
    models:
      claude-sonnet-5-5: {input: 3, cache_write_5m: 3.75, cache_write_1h: 6, cache_read: 0.3, output: 15}
  - effective: 2026-01-15T18:30:00Z
    models:
      claude-opus-5-5: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 1, output: 50}
      claude-sonnet-5-5: {input: 3, cache_write_5m: 3.75, cache_write_1h: 6, cache_read: 0.3, output: 15}
";

/// A million Opus 5.5 input tokens on the API-key member `a` of pool `p` at 17:45 and at 18:45 UTC on 2026-01-15, so
/// one row lands in the hour before the entry's and one in the hour the entry starts in, folded on
/// `AN_ENTRY_INSIDE_AN_HOUR`, which it answers.
fn plant_around_an_entry_inside_an_hour(db: &LedgerDb) -> PriceRevisions {
    let prices = PriceRevisions::from_yaml(AN_ENTRY_INSIDE_AN_HOUR, "test").unwrap();
    for (id, hour) in [("before", 17), ("inside", 18)] {
        let request = RequestRecord {
            id: id.into(),
            received_at: ms(2026, 1, 15, hour, 45),
            key_id: Some("desktop".into()),
            pool_id: Some("p".into()),
            canonical_model: "claude-opus-5-5".into(),
            model: "claude-opus-5-5".into(),
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
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            ..Default::default()
        };
        db.insert_request_at(&request, &[attempt], None, &prices)
            .unwrap();
    }
    prices
}

/// The dollars of the hour starting at `hour`:00 UTC on 2026-01-15, every row of it as one group.
async fn dollars_of_hour(
    db: &std::sync::Arc<LedgerDb>,
    prices: &PriceRevisions,
    hour: i8,
) -> Vec<f64> {
    let from = ms(2026, 1, 15, hour, 0);
    breakdown_paged(
        db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        from,
        from + 3_600_000,
        GroupBy::All,
        prices,
        &phoebis_ledger::test_tables::subscription_costs(),
        NOW,
    )
    .await
    .unwrap()
    .iter()
    .map(|group| group.cost_usd)
    .collect()
}

/// A row whose hour holds no request before the entry's instant has no earlier side and prices whole at the entry in
/// force at the hour's end: the row of 18:00 reads the entry of 18:30 that adds Opus 5.5, a million input tokens at
/// $10, while the row of 17:00 ends before the entry and reads no dollars on a model the entry before it lacks.
#[tokio::test]
async fn the_breakdown_prices_the_hour_an_entry_starts_in_at_that_entry() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let prices = plant_around_an_entry_inside_an_hour(&db);
    assert_eq!(
        (
            dollars_of_hour(&db, &prices, 17).await,
            dollars_of_hour(&db, &prices, 18).await
        ),
        (vec![0.0], vec![10.0])
    );
}

/// A member's spend splits its period at the start of the hour an entry takes effect in, and the hour of 18:00 holds no
/// request before the entry's instant, so its row prices whole at the entry of 18:30 that adds Opus 5.5 and the row of
/// 17:00 at the entry before it: the day counts the $10 of the 18:45 request alone.
#[test]
fn a_members_spend_prices_the_hour_an_entry_starts_in_at_that_entry() {
    let db = LedgerDb::open_in_memory().unwrap();
    let prices = plant_around_an_entry_inside_an_hour(&db);
    let spent = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[budgeted("p", "a", BudgetPeriod::Day)],
                ms(2026, 1, 15, 23, 0),
                &prices,
            )
        })
        .unwrap()[0]
        .spent_usd;
    assert_eq!(spent, 10.0);
}

/// Opus 5.5 at $5 per million input tokens, and at $10 from 18:30 UTC on 2026-01-15, an instant inside the 18:00 hour.
const TWO_RATES_IN_ONE_HOUR: &str = "\
revisions:
  - effective: 2026-01-01
    models:
      claude-opus-5-5: {input: 5, cache_write_5m: 6.25, cache_write_1h: 10, cache_read: 0.5, output: 25}
  - effective: 2026-01-15T18:30:00Z
    models:
      claude-opus-5-5: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 1, output: 50}
";

/// The key, the pool and the API-key account one request of a split hour runs under.
type Scope<'a> = (&'a str, &'a str, &'a str);

/// The first scope of `plant_a_split_hour`, whose hour reads $15 once each request prices at its own entry.
const FIRST_SCOPE: Scope<'static> = ("desktop", "p1", "a1");

/// One request of `input` Opus 5.5 input tokens under `scope`, received at `at` and folded on `prices`.
fn plant_billed(
    db: &LedgerDb,
    id: &str,
    (key, pool, account): Scope<'_>,
    at: i64,
    input: u64,
    prices: &PriceRevisions,
) {
    let request = RequestRecord {
        id: id.into(),
        received_at: at,
        key_id: Some(key.into()),
        pool_id: Some(pool.into()),
        canonical_model: "claude-opus-5-5".into(),
        model: "claude-opus-5-5".into(),
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
        account_kind: AccountKind::ApiKey,
        outcome: "succeeded".into(),
        input_tokens: Some(input),
        ..Default::default()
    };
    db.insert_request_at(&request, &[attempt], None, prices)
        .unwrap();
}

/// The 18:00 hour of 2026-01-15, which `TWO_RATES_IN_ONE_HOUR` splits at 18:30.
fn split_hour() -> i64 {
    ms(2026, 1, 15, 18, 0)
}

/// `FIRST_SCOPE` takes a million input tokens on the last millisecond before the entry of 18:30 and a million on its
/// first, so its hour reads $5 and $10. A second scope (key `laptop`, pool `p2`, account `a2`) takes two million at
/// 18:10, $10. Priced whole at the hour's end, the two read $20 each. Answers the list the rows were folded on.
fn plant_a_split_hour(db: &LedgerDb) -> PriceRevisions {
    let prices = PriceRevisions::from_yaml(TWO_RATES_IN_ONE_HOUR, "test").unwrap();
    let entry = ms(2026, 1, 15, 18, 30);
    plant_billed(db, "before", FIRST_SCOPE, entry - 1, 1_000_000, &prices);
    plant_billed(db, "on", FIRST_SCOPE, entry, 1_000_000, &prices);
    let second = ("laptop", "p2", "a2");
    plant_billed(
        db,
        "other",
        second,
        ms(2026, 1, 15, 18, 10),
        2_000_000,
        &prices,
    );
    prices
}

/// The first scope's pool alone.
fn first_pool() -> RollupFilter {
    RollupFilter {
        pools: vec![FIRST_SCOPE.1.into()],
        ..RollupFilter::default()
    }
}

/// Each request of a split hour prices at the entry in force when it arrived, and the stored row's request count stays
/// the row's own: the side of the request before 18:30 carries its tokens alone.
#[tokio::test]
async fn the_breakdown_prices_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let prices = plant_a_split_hour(&db);
    let groups = breakdown_paged(
        &db,
        &first_pool(),
        &LiveKeys::default(),
        split_hour(),
        split_hour() + 3_600_000,
        GroupBy::All,
        &prices,
        &phoebis_ledger::test_tables::subscription_costs(),
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        groups
            .iter()
            .map(|g| (g.cost_usd, g.requests))
            .collect::<Vec<_>>(),
        [(15.0, 2)]
    );
}

/// The series loads the sides of its own range, so its point of the split hour reads the first scope's $15.
#[tokio::test]
async fn the_series_prices_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let prices = plant_a_split_hour(&db);
    let points = series_paged(
        &db,
        &first_pool(),
        &LiveKeys::default(),
        split_hour(),
        split_hour() + 3_600_000,
        3_600,
        GroupBy::All,
        &prices,
        &phoebis_ledger::test_tables::subscription_costs(),
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        points
            .iter()
            .map(|p| (
                p.at,
                p.groups.iter().map(|g| g.cost_usd).collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        [(split_hour(), vec![15.0])]
    );
}

/// The Dashboard's hour of the split reads $25: the first scope's $15 and the second scope's two million at the $5 of
/// 18:10.
#[test]
fn the_dashboards_day_prices_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = LedgerDb::open_in_memory().unwrap();
    let prices = plant_a_split_hour(&db);
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let day = db
        .read_blocking(|c| phoebis_ledger::dashboard_on(c, ms(2026, 1, 15, 19, 0), &prices, &costs))
        .unwrap();
    let hour = day
        .hourly
        .iter()
        .find(|h| h.at == split_hour())
        .expect("the trailing day holds the split hour");
    assert_eq!(hour.cost, 25.0);
}

/// The burn route's read loads the sides ahead of its pages, so the first pool's point of the split hour reads $15.
#[tokio::test]
async fn the_burn_prices_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let prices = plant_a_split_hour(&db);
    let filter = SeriesFilter {
        from: split_hour(),
        to: split_hour() + 3_600_000,
        bucket_seconds: 3_600,
        pools: vec![FIRST_SCOPE.1.into()],
        ..SeriesFilter::default()
    };
    let groups = burn_groups_paged(
        &db,
        &phoebis_ledger::test_tables::subscription_costs(),
        &prices,
        &filter,
        GroupBy::All,
        &LiveKeys::default(),
        &[],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        groups
            .iter()
            .flat_map(|(_, _, points)| points.iter().map(|p| (p.at, p.usd)))
            .collect::<Vec<_>>(),
        [(split_hour(), 15.0)]
    );
}

/// A key's own usage prices the split hour by request, so the first scope's key reads $15 over the day.
#[test]
fn a_keys_usage_prices_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = LedgerDb::open_in_memory().unwrap();
    let prices = plant_a_split_hour(&db);
    let (day, _) = db
        .read_blocking(|c| {
            phoebis_ledger::key_usage_windows(c, FIRST_SCOPE.0, 0, ms(2026, 1, 15, 19, 0), &prices)
        })
        .unwrap();
    assert_eq!(day.cost_usd, 15.0);
}

/// The keys' totals price the split hour from its stored rows, each priced by request, while the hour's request count
/// stays the rows' own.
#[test]
fn the_key_totals_price_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = LedgerDb::open_in_memory().unwrap();
    let prices = plant_a_split_hour(&db);
    let live = LiveKeys(BTreeMap::from([
        ("desktop".to_string(), 0),
        ("laptop".to_string(), 0),
    ]));
    let hour = split_hour() / 1_000;
    let keys = db
        .read_blocking(|c| {
            key_request_totals(
                c,
                hour,
                hour + 3_600,
                &live,
                &prices,
                &phoebis_ledger::test_tables::subscription_costs(),
                NOW,
            )
        })
        .unwrap();
    let first = &keys[FIRST_SCOPE.0];
    assert_eq!((first.cost_usd, first.requests), (15.0, 2));
}

/// A pool's traffic prices the split hour from its stored rows, so the first pool's bucket reads $15.
#[test]
fn a_pools_traffic_prices_each_request_of_a_split_hour_at_the_entry_in_force_when_it_arrived() {
    let db = LedgerDb::open_in_memory().unwrap();
    let prices = plant_a_split_hour(&db);
    let hour = split_hour() / 1_000;
    let traffic = db
        .read_blocking(|c| {
            pools_traffic(
                c,
                &[FIRST_SCOPE.1.to_string()],
                hour,
                hour + 3_600,
                3_600,
                &prices,
                &phoebis_ledger::test_tables::subscription_costs(),
                NOW,
            )
        })
        .unwrap();
    assert_eq!(
        traffic[FIRST_SCOPE.1]
            .iter()
            .map(|b| b.cost_usd)
            .collect::<Vec<_>>(),
        [15.0]
    );
}

/// A member's day and month in one call, a day after the split hour, with one more million of the first scope at 10:00
/// on 2026-01-16: the day reads that request's $10 alone, though the month's split hour is bound out of the same call,
/// and the month reads the split hour's $15 and the hours behind it, $25.
#[test]
fn a_members_day_and_month_each_read_their_own_hours_around_a_split_hour() {
    let db = LedgerDb::open_in_memory().unwrap();
    let prices = plant_a_split_hour(&db);
    let next_day = ms(2026, 1, 16, 10, 0);
    plant_billed(&db, "next-day", FIRST_SCOPE, next_day, 1_000_000, &prices);
    let (pool, account) = (FIRST_SCOPE.1, FIRST_SCOPE.2);
    let spends = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[
                    budgeted(pool, account, BudgetPeriod::Day),
                    budgeted(pool, account, BudgetPeriod::Month),
                ],
                ms(2026, 1, 16, 12, 0),
                &prices,
            )
        })
        .unwrap();
    assert_eq!(
        spends
            .iter()
            .map(|s| (s.period, s.spent_usd))
            .collect::<Vec<_>>(),
        [(BudgetPeriod::Day, 10.0), (BudgetPeriod::Month, 25.0)]
    );
}

/// Opus 5.5 at $5 per million input tokens, at $10 from 18:20 UTC on 2026-01-15 and at $20 from 18:40, two instants
/// inside one hour.
const THREE_RATES_IN_ONE_HOUR: &str = "\
revisions:
  - effective: 2026-01-01
    models:
      claude-opus-5-5: {input: 5, cache_write_5m: 6.25, cache_write_1h: 10, cache_read: 0.5, output: 25}
  - effective: 2026-01-15T18:20:00Z
    models:
      claude-opus-5-5: {input: 10, cache_write_5m: 12.5, cache_write_1h: 20, cache_read: 1, output: 50}
  - effective: 2026-01-15T18:40:00Z
    models:
      claude-opus-5-5: {input: 20, cache_write_5m: 25, cache_write_1h: 40, cache_read: 2, output: 100}
";

/// A side holds the requests since the instant before its own, so the side of 18:40 leaves out the request of 18:10
/// that the side of 18:20 priced: a million input tokens at 18:10, 18:30 and 18:50 read $5, $10 and $20.
#[tokio::test]
async fn each_side_of_an_hour_split_twice_prices_only_the_requests_since_the_instant_before_it() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let prices = PriceRevisions::from_yaml(THREE_RATES_IN_ONE_HOUR, "test").unwrap();
    for (id, minute) in [("first", 10), ("second", 30), ("third", 50)] {
        plant_billed(
            &db,
            id,
            FIRST_SCOPE,
            ms(2026, 1, 15, 18, minute),
            1_000_000,
            &prices,
        );
    }
    assert_eq!(dollars_of_hour(&db, &prices, 18).await, vec![35.0]);
}

/// A cost model whose second entry doubles Sonnet 5.5's weight from 18:30 UTC on 2026-01-15, an instant inside the
/// 18:00 hour. `test_tier` anchors the figure at 1,000 units of the 7-day window in both entries.
const DOUBLED_WEIGHT_IN_ONE_HOUR: &str = "\
unit: {tier: test_tier, window: seven_day}
revisions:
  - effective: 2026-01-01
    capacity:
      test_tier: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5-5: sonnet
  - effective: 2026-01-15T18:30:00Z
    capacity:
      test_tier: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 0.5, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5-5: 2
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5-5: sonnet
";

/// Two equal plan requests of 50 input and 5 output tokens, 100 units, at 18:10 and at 18:45 read a Pro-week figure of
/// 0.1 and 0.2 on either side of the cost entry of 18:30, where the price list splits nothing, and the plan's counts
/// stay the two requests'.
#[tokio::test]
async fn a_cost_entry_inside_an_hour_weighs_each_plan_request_at_the_entry_in_force_when_it_arrived(
) {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    for (id, minute) in [("early", 10), ("late", 45)] {
        plant_share(
            &db,
            ShareRow {
                id,
                model: "claude-sonnet-5-5",
                at: ms(2026, 1, 15, 18, minute),
                usage: usage(50, 5),
                ..ShareRow::default()
            },
        );
    }
    let tables = phoebis_ledger::test_tables::tables(
        CostRevisions::from_yaml(DOUBLED_WEIGHT_IN_ONE_HOUR).unwrap(),
    );
    let groups = breakdown_paged(
        &db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        split_hour(),
        split_hour() + 3_600_000,
        GroupBy::All,
        &priced_table("claude-sonnet-5-5", 1.0),
        &tables,
        NOW,
    )
    .await
    .unwrap();
    let all = &groups[0];
    assert!(close(all.cost_equivalent.as_ref(), 0.3), "{all:?}");
    assert_eq!(
        (all.plan_requests, all.plan_output_tokens),
        (2, 10),
        "{all:?}"
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
                 last_reading_at, observed_rise, predicted_share, ratio, requests, computed_at) \
                 VALUES (?1, ?2, ?3, ?3, ?3, 0.2, 0.2, 1.0, 1, ?3)",
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
