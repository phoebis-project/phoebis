//! Label cardinality stays bounded across a run with 10,000 distinct session ids and 10,000 distinct model
//! strings, and the text format carries what the spec's table names.
mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Rule};
use phoebis_bus::SubscriberStats;
use phoebis_ledger::{
    AttemptRecord, AttentionGaugeSource, AttentionGauges, LedgerConfig, Metrics, PriceTable,
    Pricing, QuotaSampleRecord, RequestRecord, RoutingGauges,
};
use tower::ServiceExt;

fn series_of(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect()
}

#[test]
fn ten_thousand_sessions_and_models_leave_the_label_set_bounded() {
    let metrics = Metrics::new();
    let table = PriceTable::shipped();
    for i in 0..10_000u32 {
        let model = format!("vendor/model-{i}");
        let request = RequestRecord {
            id: format!("r{i}"),
            received_at: i as i64,
            key_id: Some(format!("k{}", i % 3)),
            pool_id: Some("p".into()),
            session_id: Some(format!("session-{i}")),
            model: model.clone(),
            canonical_model: model.clone(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ttfb_ms: Some(50 + (i % 900) as i64),
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: request.id.clone(),
            attempt: 1,
            account_id: format!("a{}", i % 2),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(10),
            output_tokens: Some(5),
            ttfb_ms: request.ttfb_ms,
            ..Default::default()
        };
        metrics.observe_request(&table, &request, &[attempt]);
    }
    let priced = RequestRecord {
        id: "p".into(),
        received_at: 0,
        key_id: Some("k0".into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    metrics.observe_request(
        &table,
        &priced,
        &[AttemptRecord {
            request_id: "p".into(),
            attempt: 1,
            account_id: "a0".into(),
            outcome: "succeeded".into(),
            output_tokens: Some(1),
            ttfb_ms: Some(20),
            ..Default::default()
        }],
    );
    let text = metrics.render(
        &RoutingGauges::default(),
        &[("ledger", Arc::new(SubscriberStats::default()))],
        3,
    );
    let series = series_of(&text);
    assert!(
        series.len() < 400,
        "{} series: cardinality is bounded by accounts, keys, outcomes, and the priced models",
        series.len()
    );
    assert!(!text.contains("session-"), "no session id in any label");
    assert!(
        !text.contains("vendor/model"),
        "no caller-supplied model string in any label"
    );
    assert!(text.contains(
        r#"phoebis_attempts_total{account="a0",model="other",outcome="succeeded"} 5000"#
    ));
    assert!(text.contains(
        r#"phoebis_attempts_total{account="a0",model="claude-sonnet-5",outcome="succeeded"} 1"#
    ));
    assert!(text
        .contains(r#"phoebis_requests_total{key="k1",kind="inference",outcome="succeeded"} 3333"#));
    assert!(text.contains(r#"phoebis_tokens_total{account="a1",kind="input",model="other"} 50000"#));
    assert!(text.contains("# TYPE phoebis_upstream_ttfb_seconds histogram"));
    assert!(text.contains(r#"phoebis_upstream_ttfb_seconds_bucket{model="other",le="+Inf"} 10000"#));
    assert!(text.contains(r#"phoebis_upstream_ttfb_seconds_sum{model="claude-sonnet-5"} "#));
    assert!(text.contains(r#"phoebis_upstream_ttfb_seconds_count{model="claude-sonnet-5"} 1"#));
    assert!(text.contains("phoebis_pricing_unknown_model_total 3"));
    assert!(text.contains(r#"phoebis_bus_dropped_total{subscriber="ledger"} 0"#));
}

/// A response slower than the last finite bucket's bound has no bucket that tells the truth about it, and
/// counting it in the last one reads a three-minute response as a hundred-second one in every quantile.
#[test]
fn a_response_past_the_last_finite_bucket_is_exported_as_inf_only() {
    let metrics = Metrics::new();
    let table = PriceTable::shipped();
    let observe = |ttfb_ms: i64| {
        let request = RequestRecord {
            id: "r".into(),
            received_at: 0,
            key_id: Some("k".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ttfb_ms: Some(ttfb_ms),
            ..Default::default()
        };
        metrics.observe_request(
            &table,
            &request,
            &[AttemptRecord {
                request_id: "r".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                ttfb_ms: Some(ttfb_ms),
                ..Default::default()
            }],
        );
    };
    // The bucket lines in the order they are exported: the 64 finite ones, then +Inf.
    let buckets = || -> Vec<u64> {
        metrics
            .render(&RoutingGauges::default(), &[], 0)
            .lines()
            .filter(|l| {
                l.starts_with(r#"phoebis_upstream_ttfb_seconds_bucket{model="claude-sonnet-5""#)
            })
            .map(|l| l.rsplit_once(' ').unwrap().1.parse().unwrap())
            .collect()
    };
    observe(50);
    let before = buckets();
    assert_eq!(before.len(), 65, "64 finite buckets and +Inf");
    assert_eq!((before[63], before[64]), (1, 1));
    // 200 seconds, well past the last finite edge of about 100.
    observe(200_000);
    let after = buckets();
    assert_eq!(
        after[63], before[63],
        "the last finite bucket does not claim an observation past its bound"
    );
    assert_eq!(after[64], before[64] + 1, "+Inf carries it");
}

#[test]
fn gauges_come_from_the_routing_state_and_drops_are_counted_per_table() {
    let metrics = Metrics::new();
    metrics.observe_drop("requests");
    metrics.observe_drop("requests");
    metrics.observe_drop("events");
    metrics.observe_samples(&[phoebis_ledger::QuotaSampleRecord {
        account_id: "a".into(),
        bucket: 0,
        window: "5h".into(),
        utilization: Some(0.42),
        reset_at: None,
        source: "header".into(),
        observed_at: None,
    }]);
    let gauges = RoutingGauges {
        cooldowns: vec![
            ("a".into(), "account".into(), 120),
            ("a".into(), "model:other".into(), 5),
        ],
        overage: vec![("a".into(), true), ("b".into(), false)],
        pins_by_pool: vec![("main".into(), 7)],
        live_accounts: BTreeSet::from(["a".to_string()]),
        ..RoutingGauges::default()
    };
    let text = metrics.render(&gauges, &[], 0);
    for line in [
        r#"phoebis_ledger_dropped_total{table="events"} 1"#,
        r#"phoebis_ledger_dropped_total{table="requests"} 2"#,
        r#"phoebis_account_cooldown_seconds{account="a",scope="account"} 120"#,
        r#"phoebis_account_cooldown_seconds{account="a",scope="model:other"} 5"#,
        r#"phoebis_account_overage_in_use{account="a"} 1"#,
        r#"phoebis_account_overage_in_use{account="b"} 0"#,
        r#"phoebis_account_ratelimit_utilization{account="a",window="5h"} 0.42"#,
        r#"phoebis_pins_active{pool="main"} 7"#,
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
    assert!(
        text.contains("# TYPE phoebis_pins_active gauge")
            && text.contains("# TYPE phoebis_requests_total counter")
    );
}

#[test]
fn the_routers_drop_counters_render_one_series_per_kind() {
    let gauges = RoutingGauges {
        dropped_reports: vec![("cooldown", 2), ("usage", 0)],
        ..RoutingGauges::default()
    };
    let text = Metrics::new().render(&gauges, &[], 0);
    assert!(
        text.contains("# TYPE phoebis_router_dropped_reports_total counter"),
        "{text}"
    );
    for line in [
        r#"phoebis_router_dropped_reports_total{kind="cooldown"} 2"#,
        r#"phoebis_router_dropped_reports_total{kind="usage"} 0"#,
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
    assert_eq!(
        text.matches("phoebis_router_dropped_reports_total{")
            .count(),
        2,
        "one series per kind the table carries: {text}"
    );
}

#[test]
fn the_routing_state_writers_depth_and_degraded_state_render_as_their_own_families() {
    let gauges = RoutingGauges {
        writer: phoebis_routing::WriterStatus {
            depth: 3,
            degraded: true,
            ..phoebis_routing::WriterStatus::default()
        },
        ..RoutingGauges::default()
    };
    let text = Metrics::new().render(&gauges, &[], 0);
    for line in [
        "# TYPE phoebis_router_writer_degraded gauge",
        "# TYPE phoebis_router_writer_depth gauge",
        "phoebis_router_writer_degraded 1\n",
        "phoebis_router_writer_depth 3\n",
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
    let clear = Metrics::new().render(&RoutingGauges::default(), &[], 0);
    assert!(
        clear.contains("phoebis_router_writer_degraded 0\n")
            && clear.contains("phoebis_router_writer_depth 0\n"),
        "a writer that keeps up reads 0 on both: {clear}"
    );
}

#[test]
fn the_ledger_writers_backlog_and_its_admission_refusals_render_as_their_own_families() {
    let gauges = RoutingGauges {
        ledger_queued: 2_048,
        ledger_behind_refusals: 3,
        ..RoutingGauges::default()
    };
    let text = Metrics::new().render(&gauges, &[], 0);
    for line in [
        "# TYPE phoebis_ledger_behind_refusals_total counter",
        "# TYPE phoebis_ledger_queued gauge",
        "phoebis_ledger_behind_refusals_total 3",
        "phoebis_ledger_queued 2048",
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
}

#[test]
fn the_ledgers_degraded_state_and_its_parked_priced_records_render_as_their_own_families() {
    let gauges = RoutingGauges {
        ledger_degraded: true,
        ledger_parked_priced: 2,
        ..RoutingGauges::default()
    };
    let text = Metrics::new().render(&gauges, &[], 0);
    for line in [
        "# TYPE phoebis_ledger_degraded gauge",
        "# TYPE phoebis_ledger_parked_priced_total counter",
        "phoebis_ledger_degraded 1",
        "phoebis_ledger_parked_priced_total 2",
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
    let clear = Metrics::new().render(&RoutingGauges::default(), &[], 0);
    assert!(
        clear.contains("phoebis_ledger_degraded 0\n"),
        "a ledger that writes every batch reads 0: {clear}"
    );
}

#[tokio::test]
async fn a_scrape_reads_the_routers_drop_counters_at_gather() {
    use phoebis_store::{StorageKey, Store};
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let router = phoebis_routing::spawn(
        store,
        Arc::new(|| 1_000),
        phoebis_routing::RouterConfig::default(),
    )
    .unwrap();
    router.shutdown().await.unwrap();
    router
        .report(phoebis_routing::Report::MemberSpend(Vec::new()))
        .await;
    let gauges = phoebis_ledger::gather(
        &router,
        &PriceTable::shipped(),
        1_000,
        60_000,
        None,
        AttentionGauges::default(),
        None,
    )
    .await;
    assert_eq!(
        gauges
            .dropped_reports
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>(),
        phoebis_routing::Report::KINDS,
        "every kind of the router's table is copied"
    );
    let text = Metrics::new().render(&gauges, &[], 0);
    for line in [
        r#"phoebis_router_dropped_reports_total{kind="member_spend"} 1"#,
        r#"phoebis_router_dropped_reports_total{kind="usage"} 0"#,
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
}

#[tokio::test]
async fn cooldowns_on_two_unpriced_models_render_as_one_other_sample_with_the_longer_wait() {
    use phoebis_store::{AccountKind, AccountRow, AccountState, CooldownRow, StorageKey, Store};
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::ApiKey,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    for (model, until) in [("vendor-x", 61_000), ("vendor-y", 121_000)] {
        store
            .upsert_cooldown(&CooldownRow {
                account_id: "a".into(),
                scope: format!("model:{model}"),
                until,
                set_at: 0,
                reason: "rate_limit".into(),
                window: "w".into(),
                source_request_id: None,
                incarnation: 1,
            })
            .unwrap();
    }
    let router = phoebis_routing::spawn(
        store,
        Arc::new(|| 1_000),
        phoebis_routing::RouterConfig::default(),
    )
    .unwrap();
    let gauges = phoebis_ledger::gather(
        &router,
        &PriceTable::shipped(),
        1_000,
        60_000,
        None,
        AttentionGauges::default(),
        None,
    )
    .await;
    assert_eq!(
        gauges.cooldowns,
        vec![("a".into(), "model:other".into(), 120)],
        "one sample per normalised label set, the maximum remaining"
    );
    let text = Metrics::new().render(&gauges, &[], 0);
    assert_eq!(
        text.matches("phoebis_account_cooldown_seconds{account=\"a\",scope=\"model:other\"}")
            .count(),
        1
    );
}

#[tokio::test]
async fn an_overage_observations_reset_decides_the_gauge_rather_than_the_raw_flag() {
    use phoebis_store::{AccountKind, AccountRow, AccountState, StorageKey, Store};
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    for id in ["past", "ahead"] {
        store
            .insert_account(&AccountRow {
                id: id.into(),
                kind: AccountKind::Oauth,
                label: id.into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
    }
    let router = phoebis_routing::spawn(
        store,
        Arc::new(|| 2_000),
        phoebis_routing::RouterConfig::default(),
    )
    .unwrap();
    let evidence = |reset: Option<i64>| phoebis_routing::OverageEvidence {
        in_use: true,
        since: Some(1_000),
        reset,
        observed_at: 1_000,
        source: phoebis_routing::EvidenceSource::Head,
        incarnation: 1,
    };
    router
        .report(phoebis_routing::Report::Overage {
            account_id: "past".into(),
            evidence: evidence(Some(1_500)),
            incarnation: 1,
        })
        .await;
    router
        .report(phoebis_routing::Report::Overage {
            account_id: "ahead".into(),
            evidence: evidence(Some(9_000)),
            incarnation: 1,
        })
        .await;
    let gauges = phoebis_ledger::gather(
        &router,
        &PriceTable::shipped(),
        2_000,
        60_000,
        None,
        AttentionGauges::default(),
        None,
    )
    .await;
    let text = Metrics::new().render(&gauges, &[], 0);
    assert!(
        text.contains(r#"phoebis_account_overage_in_use{account="past"} 0"#),
        "a reset already past now reads 0: {text}"
    );
    assert!(
        text.contains(r#"phoebis_account_overage_in_use{account="ahead"} 1"#),
        "a reset still ahead of now reads 1: {text}"
    );
}

#[tokio::test]
async fn a_landed_request_reaches_the_request_and_attempt_series() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5[1m]", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // The observation follows the commit, so the row can be readable a moment before the series moves.
    let text = wait_for(Duration::from_secs(5), || {
        let text = h.metrics.render(&RoutingGauges::default(), &[], 0);
        text.contains("phoebis_requests_total{").then_some(text)
    })
    .await;
    assert!(
        text.contains(
            r#"phoebis_requests_total{key="desktop",kind="inference",outcome="succeeded"} 1"#
        ),
        "{text}"
    );
    assert!(
        text.contains(r#"phoebis_attempts_total{account="a",model="other",outcome="succeeded"} 1"#),
        "the shipped table does not price claude-sonnet-4-5, so the label is other:\n{text}"
    );
    up.shutdown().await;
}

/// The renderer's family list and its registry cannot drift apart: every family in `FAMILIES` renders its TYPE
/// line, and every TYPE line rendered is in `FAMILIES`.
#[test]
fn every_registered_family_renders_and_nothing_else_does() {
    let metrics = Metrics::new();
    metrics.observe_probe("a", "ok");
    metrics.observe_probe("a", "migrated");
    metrics.observe_probe("b", "failed");
    metrics.observe_saving("main", 0.25);
    metrics.observe_saving("main", 0.5);
    let gauges = RoutingGauges {
        keepalive_agents: phoebis_ledger::KeepaliveAgents {
            busy: 2,
            idle: 1,
            parked: 0,
        },
        ..RoutingGauges::default()
    };
    let text = metrics.render(
        &gauges,
        &[("ledger", Arc::new(SubscriberStats::default()))],
        0,
    );
    let typed: BTreeSet<(String, String)> = text
        .lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .map(|l| {
            let (name, kind) = l.split_once(' ').unwrap();
            (name.to_string(), kind.to_string())
        })
        .collect();
    let registered: BTreeSet<(String, String)> = phoebis_ledger::FAMILIES
        .iter()
        .map(|(n, k)| (n.to_string(), k.to_string()))
        .collect();
    assert_eq!(typed, registered);
    for line in [
        r#"phoebis_keepalive_agents{state="busy"} 2"#,
        r#"phoebis_keepalive_agents{state="idle"} 1"#,
        r#"phoebis_keepalive_agents{state="parked"} 0"#,
        r#"phoebis_keepalive_probes_total{account="a",outcome="migrated"} 1"#,
        r#"phoebis_keepalive_probes_total{account="a",outcome="ok"} 1"#,
        r#"phoebis_keepalive_probes_total{account="b",outcome="failed"} 1"#,
        r#"phoebis_keepalive_saved_usd_total{pool="main"} 0.75"#,
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
    let order: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "families render in name order");
}

#[test]
fn a_deleted_accounts_utilization_series_leaves_the_gauge() {
    let metrics = Metrics::new();
    metrics.observe_samples(&[
        QuotaSampleRecord {
            account_id: "a".into(),
            window: "5h".into(),
            utilization: Some(0.4),
            ..Default::default()
        },
        QuotaSampleRecord {
            account_id: "b".into(),
            window: "5h".into(),
            utilization: Some(0.9),
            ..Default::default()
        },
    ]);
    let gauges = RoutingGauges {
        live_accounts: BTreeSet::from(["a".to_string()]),
        ..RoutingGauges::default()
    };
    let text = metrics.render(&gauges, &[], 0);
    assert!(
        text.contains(r#"phoebis_account_ratelimit_utilization{account="a",window="5h"} 0.4"#),
        "{text}"
    );
    assert!(
        !text.contains("account=\"b\""),
        "a deleted account's last reading does not scrape forever: {text}"
    );
}

#[test]
fn the_attention_gauge_reads_one_series_per_condition_with_its_labels_escaped() {
    let gauges = RoutingGauges {
        attention: AttentionGauges {
            conditions: vec![
                ("pool_exhausted", "main".into(), true),
                ("client_drift", "2.1.274".into(), false),
                ("pool_exhausted", "odd\"pool".into(), false),
            ],
        },
        ..RoutingGauges::default()
    };
    let text = Metrics::new().render(&gauges, &[], 0);
    for line in [
        "# TYPE phoebis_attention gauge",
        r#"phoebis_attention{kind="pool_exhausted",subject="main"} 1"#,
        r#"phoebis_attention{kind="client_drift",subject="2.1.274"} 0"#,
        r#"phoebis_attention{kind="pool_exhausted",subject="odd\"pool"} 0"#,
    ] {
        assert!(text.contains(line), "missing {line} in:\n{text}");
    }
}

#[tokio::test]
async fn an_attention_read_that_fails_answers_the_scrape_500_with_no_family() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use phoebis_store::{StorageKey, Store, StoreError};
    let router = phoebis_routing::spawn(
        Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap()),
        Arc::new(|| 1_000),
        phoebis_routing::RouterConfig::default(),
    )
    .unwrap();
    let scrape = |attention: AttentionGaugeSource| {
        let app = phoebis_ledger::metrics_router(
            Arc::new(Metrics::new()),
            router.clone(),
            Arc::new(Pricing::new(PriceTable::shipped())),
            phoebis_bus::Bus::new(),
            Arc::new(|| 1_000),
            Duration::from_secs(60),
            None,
            Some(attention),
            None,
        );
        async move {
            let resp = app
                .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }
    };
    let (status, text) = scrape(Arc::new(|| {
        Ok(AttentionGauges {
            conditions: vec![("poll_backoff", "a".into(), true)],
        })
    }))
    .await;
    assert_eq!(status, 200);
    assert!(
        text.contains(r#"phoebis_attention{kind="poll_backoff",subject="a"} 1"#),
        "{text}"
    );
    let (status, text) = scrape(Arc::new(|| {
        Err(StoreError::Io(std::io::Error::other("disk gone")))
    }))
    .await;
    assert_eq!(
        status, 500,
        "a scrape that cannot read the conditions fails rather than reading 0 for a held one"
    );
    assert_eq!(text, "attention read failed");
    assert!(
        !text.contains("disk gone"),
        "the listener asks for no credential, so the store's words stay in the log"
    );
}
