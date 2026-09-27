//! Rollups for a synthetic day equal a direct aggregation of the raw rows for every group-by, the unattributed
//! bucket included, and merged-histogram percentiles lie within one bucket of the exact ones.
use std::collections::{BTreeMap, HashMap};

use phoebis_ledger::{
    apply_filter, breakdown, bucket_index, burn_rows, burn_series, burn_series_over, delta_of,
    fold_account, fold_event, fold_keepalive, key_request_totals, member_spends, moves_by_reason,
    placements, read_account_rollups, read_rollups, rewrite_series, run_retention,
    session_identities, AttemptRecord, EventRecord, Histogram, KeepaliveDelta, LedgerConfig,
    LedgerDb, LiveKeys, ModelPrice, PlanFactors, PlanWindows, PriceTable, RequestRecord,
    RollupFilter, RollupRow, SeriesFilter, NO_CLASS, UNATTRIBUTED,
};
use phoebis_routing::{RegistrySnapshot, RoutingState, FIVE_HOUR};
use phoebis_store::{AccountKind, BudgetPeriod, PoolMember, RegistryLoad, RoutingLoad};
use rand::{RngExt, SeedableRng};

const DAY_MS: i64 = 86_400_000;
/// Monday 2026-09-21 at 14:00 UTC, inside the peak band.
const WEEKDAY_PEAK: i64 = 1_789_999_200_000;
/// Monday 2026-09-21 at 20:00 UTC, past the peak band on the same weekday.
const WEEKDAY_OFF_PEAK: i64 = 1_790_020_800_000;

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
        }
        db.insert_request(request, &earlier, None).unwrap();
    }
    for (request, attempts) in &rows {
        db.insert_request(request, attempts, None).unwrap();
    }
    let rollups = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
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
                .any(|r| r.user_agent == "claude-cli/2.1.300 (external, cli)"),
        "a caller that sent no agent folds under the reserved id, beside the ones that did"
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
    let live = LiveKeys(BTreeMap::from([
        ("k1".to_string(), 2_000),
        ("k2".to_string(), 0),
    ]));
    let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
    let groups = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default(), &live),
        "key",
        true,
        &live,
        &[],
    );
    assert_eq!(
        groups
            .iter()
            .map(|g| (g.key.as_str(), g.label.as_str(), g.requests))
            .collect::<Vec<_>>(),
        vec![
            ("k1", "k1", 1),
            ("k1@1000", "k1 (deleted)", 2),
            ("k2", "k2", 2),
            ("-", "unattributed", 1)
        ],
        "groups sort by label; the reserved id keeps its label and no suffix"
    );
    let agents = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default(), &live),
        "user_agent",
        true,
        &live,
        &[],
    );
    let ops = agents
        .iter()
        .find(|g| g.key == "my-client/1.0 (ops@example.invalid)")
        .expect("the agent is its own group");
    assert_eq!(
        ops.label, "my-client/1.0 (ops@example.invalid)",
        "an agent carrying `@` is labelled verbatim: only a key group is parsed for a generation"
    );
    let clients = breakdown(
        &apply_filter(rows.clone(), &RollupFilter::default(), &live),
        "client",
        true,
        &live,
        &[],
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
    let sum = |keys: &[&str]| -> u64 {
        apply_filter(
            rows.clone(),
            &RollupFilter {
                keys: keys.iter().map(|k| k.to_string()).collect(),
                ..Default::default()
            },
            &live,
        )
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
    let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default(), &live);
    let default_groups = breakdown(&filtered, "key", false, &live, &[]);
    assert!(
        !default_groups.iter().any(|g| g.key == "k1"),
        "a group whose rows are all probes stays out of the default breakdown: {default_groups:?}"
    );
    let with_probes = breakdown(&filtered, "key", true, &live, &[]);
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
    let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default(), &live);
    // A kept-warm credit never touches `requests`, so a group made only of credit rows never sees a probe
    // row either; it belongs in the default breakdown, not just the probe-counted one.
    let default_groups = breakdown(&filtered, "user_agent", false, &live, &[]);
    let group = default_groups
        .iter()
        .find(|g| g.key == UNATTRIBUTED)
        .expect("a group of keepalive credits alone stays in the default breakdown");
    assert_eq!(group.kept_warm.requests, 1);
    assert!(group.savings.gross_usd > 0.0);
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
    let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
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
    let rollups = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
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
    let rows = db.read_blocking(|c| read_rollups(c, 0, i64::MAX)).unwrap();
    let filtered = apply_filter(rows, &RollupFilter::default(), &live);
    let groups = breakdown(&filtered, "request_class", false, &live, &[]);
    let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
    assert!(
        keys.contains(&"main") && keys.contains(&"compaction") && keys.contains(&NO_CLASS),
        "the named classes and the classless overhead land in their own groups: {keys:?}"
    );
}

#[test]
fn the_burn_series_prices_a_subscription_row_in_meter_tokens_and_an_api_key_row_in_dollars() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, account: &str, kind: &str, usd: f64, cache_read: Option<u64>| {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: 3_600_000,
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
    // Both rows carry a nonzero `estimated_usd`, so a burn that priced every row both ways instead of by kind
    // would still pass the dollar assertion; only pricing by kind can pass both.
    plant("sub", "oauth-a", "oauth", 9.99, None);
    plant("key", "api-b", "api_key", 1.25, None);
    // Absent from `kinds` below: its tokens must reach neither total, its cache reads included, the one row
    // carrying any so the cached half of the assertion below has something to leave out.
    plant("unknown", "unknown-c", "oauth", 4.0, Some(500));
    let factors = PlanFactors::shipped();
    let windows = PlanWindows::shipped();
    let kinds = HashMap::from([
        ("oauth-a".to_string(), AccountKind::Oauth),
        ("api-b".to_string(), AccountKind::ApiKey),
    ]);
    let filter = SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let points = db
        .read_blocking(|c| burn_series(c, &factors, &windows, &kinds, &filter))
        .unwrap();
    let point = points
        .iter()
        .find(|p| p.meter_tokens > 0.0 || p.usd > 0.0)
        .expect("the bucket every tracked row landed in");
    let hour = phoebis_ledger::hour_of(3_600_000);
    let expected_meter = factors
        .meter_tokens("claude-sonnet-5", 1_000, 0, 0, 0, 100)
        .unwrap()
        * windows.band_factor(FIVE_HOUR, hour * 1_000);
    assert!(
        (point.meter_tokens - expected_meter).abs() < 1e-9,
        "the subscription row alone prices in meter tokens: {} vs {expected_meter}",
        point.meter_tokens
    );
    assert!(
        (point.usd - 1.25).abs() < 1e-9,
        "the api-key row alone prices in dollars from estimated_usd"
    );
    assert_eq!(
        (point.cached_tokens, point.uncached_tokens),
        (0, 2_000),
        "the row absent from kinds contributes to neither total"
    );
}

#[test]
fn an_hour_inside_the_peak_band_weighs_a_subscription_rows_meter_tokens_by_the_band_factor() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, at) in [("peak", WEEKDAY_PEAK), ("off_peak", WEEKDAY_OFF_PEAK)] {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: at,
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
    let factors = PlanFactors::shipped();
    let windows = PlanWindows::shipped();
    let kinds = HashMap::from([("a".to_string(), AccountKind::Oauth)]);
    let filter = SeriesFilter {
        from: WEEKDAY_PEAK - 3_600_000,
        to: WEEKDAY_OFF_PEAK + 3_600_000,
        bucket_seconds: 3_600,
        ..SeriesFilter::default()
    };
    let points = db
        .read_blocking(|c| burn_series(c, &factors, &windows, &kinds, &filter))
        .unwrap();
    let peak = points
        .iter()
        .find(|p| p.at == phoebis_ledger::hour_of(WEEKDAY_PEAK) * 1_000)
        .unwrap();
    let off_peak = points
        .iter()
        .find(|p| p.at == phoebis_ledger::hour_of(WEEKDAY_OFF_PEAK) * 1_000)
        .unwrap();
    assert!(
        off_peak.meter_tokens > 0.0,
        "the off-peak row still prices, unweighed"
    );
    assert!(
        (peak.meter_tokens - off_peak.meter_tokens * 1.45).abs() < 1e-6,
        "the peak hour weighs the same tokens 1.45 times the off-peak hour: {} vs {}",
        peak.meter_tokens,
        off_peak.meter_tokens
    );
    // A daily bucket holds both rows; each still reads its own hour's band rather than the bucket's midnight
    // start, or the peak row would lose its weight entirely once the bucket outgrows the hour.
    let daily_filter = SeriesFilter {
        bucket_seconds: 86_400,
        ..filter.clone()
    };
    let daily_points = db
        .read_blocking(|c| burn_series(c, &factors, &windows, &kinds, &daily_filter))
        .unwrap();
    let day = daily_points
        .iter()
        .find(|p| p.meter_tokens > 0.0)
        .expect("the day both rows landed in");
    let base = factors
        .meter_tokens("claude-sonnet-5", 1_000, 0, 0, 0, 100)
        .unwrap();
    assert!(
        (day.meter_tokens - (base * 1.45 + base)).abs() < 1e-6,
        "the peak row weighs 1.45 and the off-peak row 1.0 in the one daily bucket they share: {} vs {}",
        day.meter_tokens,
        base * 1.45 + base
    );
}

/// A caller that folds several scopes reads the range once: the series over the rows `burn_rows` read answers,
/// point for point, what `burn_series` answers when it reads the range itself for each scope.
#[test]
fn the_burn_series_over_one_read_matches_a_read_per_scope() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (request, attempts) in &synthetic_day(5) {
        db.insert_request(request, attempts, None).unwrap();
    }
    let factors = PlanFactors::shipped();
    let windows = PlanWindows::shipped();
    let kinds = HashMap::from([
        ("a".to_string(), AccountKind::Oauth),
        ("b".to_string(), AccountKind::ApiKey),
        ("c".to_string(), AccountKind::Oauth),
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
                .read_blocking(|c| burn_series(c, &factors, &windows, &kinds, &filter))
                .unwrap();
            let over = burn_series_over(&rows, &factors, &windows, &kinds, &filter).unwrap();
            assert!(
                read.iter().any(|p| p.meter_tokens > 0.0 || p.usd > 0.0),
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
    let factors = PlanFactors::shipped();
    let windows = PlanWindows::shipped();
    assert!(db
        .read_blocking(|c| burn_series(c, &factors, &windows, &HashMap::new(), &filter))
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
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        model.to_string(),
        ModelPrice {
            input: rate,
            cache_write_5m: rate,
            cache_write_1h: rate,
            cache_read: rate,
            output: rate,
        },
    );
    PriceTable {
        version: "test".into(),
        effective: "test".into(),
        source: "test".into(),
        models,
    }
}

/// A request and its one attempt, for a pool member's spend: `estimated_usd` is planted at an obviously wrong
/// figure, so a test that reads the priced sum back at the seeded token count (not the planted figure) proves
/// `member_spends` recomputes at read time rather than trusting what the attempt was booked at.
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

    // The day bound: a row from the day before and a probe inside the day are both left out.
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
            member_spends(
                c,
                &[("p".into(), "a".into(), BudgetPeriod::Day)],
                day_now,
                &table,
            )
        })
        .unwrap();
    assert_eq!(
        (day[0].spent_usd, day[0].period_start, day[0].period_end),
        (50.0, ms(2026, 1, 15, 0, 0), ms(2026, 1, 16, 0, 0)),
        "the row before the day and the probe are left out; the price is the table's, not the row's \
         planted estimated_usd of 999"
    );

    // The month bound over the same rows: both non-probe requests of January count.
    let month_now = ms(2026, 1, 20, 0, 0);
    let month = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[("p".into(), "a".into(), BudgetPeriod::Month)],
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
        (150.0, ms(2026, 1, 1, 0, 0), ms(2026, 2, 1, 0, 0)),
        "the month sums every day inside it, the probe still left out"
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
                &[("p".into(), "b".into(), BudgetPeriod::Month)],
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
        .read_blocking(|c| {
            member_spends(
                c,
                &[("p".into(), "a".into(), BudgetPeriod::Day)],
                now,
                &table,
            )
        })
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
            member_spends(
                c,
                &[("p".into(), "a".into(), BudgetPeriod::Month)],
                now,
                &table,
            )
        })
        .unwrap();
    assert_eq!(before[0].spent_usd, 50.0);

    // A retention window shorter than the month purges the raw request and its attempt; the rollup that
    // folded them keeps its default 400-day life, so the query still answers the same spend.
    let purged = run_retention(
        &db,
        &LedgerConfig {
            retention_days: 5,
            ..LedgerConfig::default()
        },
        now,
    )
    .unwrap();
    assert_eq!((purged.requests, purged.attempts), (1, 1));
    let after = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[("p".into(), "a".into(), BudgetPeriod::Month)],
                now,
                &table,
            )
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
                budget_usd: Some(10.0),
                budget_period: BudgetPeriod::Month,
            }],
            ..Default::default()
        },
        [0u8; 32],
        now,
        1,
    );
    state.record_member_spend(&snapshot, after);
    let spend = state
        .member_spend("p", "a")
        .expect("the row still matches the member's configured period");
    assert!(
        spend.spent_usd >= 10.0,
        "the member stays excluded: its spend is still at or above the budget"
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
