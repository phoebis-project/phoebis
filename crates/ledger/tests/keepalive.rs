//! The keepalive record: folded into the hourly rollups on the delta's key, the kept-warm mark on the request, and
//! the derived probe counts.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use fake_upstream::FakeUpstream;
use phoebis_ledger::{
    breakdown, fold_keepalive, hour_of, read_rollups, AttemptRecord, KeepaliveDelta, LedgerConfig,
    LedgerDb, LiveKeys, PlanTables, PriceRevisions, Record, RequestRecord, RollupSelect, Speed,
    UNATTRIBUTED,
};
use phoebis_store::AccountKind;

fn delta(at: i64, kind: &str) -> KeepaliveDelta {
    KeepaliveDelta {
        at,
        key_id: "desktop".into(),
        pool_id: "main".into(),
        account_id: "a".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: kind.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_keepalive_delta_lands_on_its_own_row_and_marks_the_request() {
    let db = std::sync::Arc::new(LedgerDb::open_in_memory().unwrap());
    let at = 1_700_000_000_000;
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            user_agent: Some("claude-cli/2.1.269 (external, cli)".into()),
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            cache_read_tokens: Some(2_000),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                kept_warm_request_id: Some("r1".into()),
                kept_warm_requests: 1,
                kept_warm_1h_tokens: 2_000,
                ..delta(at, "inference")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                probe_migrated: 1,
                ..delta(at + 2_000, "probe")
            },
        )?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                probes_skipped: 2,
                ..delta(at + 3_000, "probe")
            },
        )?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                wasted_probes: 3,
                wasted_input_tokens: 100,
                wasted_output_tokens: 20,
                wasted_probe_reasons: BTreeMap::from([("idle_limit".to_string(), 3)]),
                ..delta(at + 4_000, "probe")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            read_rollups(
                c,
                hour_of(at),
                hour_of(at) + 3_600,
                &RollupSelect::default(),
            )
        })
        .unwrap();
    let inference = rows
        .iter()
        .find(|r| r.kind == "inference" && r.user_agent == "-")
        .expect("the credit lands on a user-agent-less row of the request's key");
    assert_eq!(
        (
            inference.kept_warm_requests,
            inference.kept_warm_5m_tokens,
            inference.kept_warm_1h_tokens,
            inference.requests
        ),
        (1, 0, 2_000, 0),
        "an hour's credit books the hour's count and touches no request counter"
    );
    let probe = rows.iter().find(|r| r.kind == "probe").unwrap();
    assert_eq!(
        (
            probe.probe_migrated,
            probe.probes_skipped,
            probe.wasted_probes,
            (probe.wasted_input_tokens, probe.wasted_output_tokens)
        ),
        (1, 2, 3, (100, 20)),
        "the settlement's counts fold beside the probe counters"
    );
    assert_eq!(
        probe.wasted_probe_reasons.get("idle_limit").copied(),
        Some(3)
    );
    let (row, _) = db.request("r1").await.unwrap().unwrap();
    assert!(row.kept_warm, "the request row carries the mark");
    // The mark can land before the row: a request inserted later under a marked id is kept warm.
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                kept_warm_request_id: Some("r2".into()),
                kept_warm_requests: 1,
                ..delta(at, "inference")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    db.insert_request(
        &RequestRecord {
            id: "r2".into(),
            received_at: at,
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[],
        None,
    )
    .unwrap();
    assert!(db.request("r2").await.unwrap().unwrap().0.kept_warm);
    // The derived probe counts.
    db.insert_request(
        &RequestRecord {
            id: "p1".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "probe".into(),
            origin: "keepalive".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "p1".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            cache_read_tokens: Some(2_000),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    db.insert_request(
        &RequestRecord {
            id: "p2".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "probe".into(),
            origin: "keepalive".into(),
            status: Some(429),
            outcome: "failed".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "p2".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "rate_limited".into(),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            read_rollups(
                c,
                hour_of(at),
                hour_of(at) + 3_600,
                &RollupSelect::default(),
            )
        })
        .unwrap();
    let with_probes = phoebis_ledger::RollupFilter {
        include_probes: true,
        ..Default::default()
    };
    // The rows above carry the generation a record defaults to, so that is what holds the id here.
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let (prices, costs) = (PriceRevisions::shipped(), PlanTables::shipped());
    let groups = phoebis_ledger::breakdown(
        &phoebis_ledger::apply_filter(rows.clone(), &with_probes),
        "key",
        with_probes.probes_counted(),
        &live,
        &[],
        None,
        &prices,
        &costs,
        NOW,
    );
    let desktop = groups.iter().find(|g| g.key == "desktop").unwrap();
    assert_eq!(
        (
            desktop.probes.ok,
            desktop.probes.failed,
            desktop.probes.migrated,
            desktop.probes.skipped
        ),
        (1, 1, 1, 2)
    );
    assert_eq!(
        desktop.requests, 3,
        "the request counters stay the raw rows' (r1, p1, p2)"
    );
    let default = phoebis_ledger::RollupFilter::default();
    let without = phoebis_ledger::breakdown(
        &phoebis_ledger::apply_filter(rows, &default),
        "key",
        default.probes_counted(),
        &live,
        &[],
        None,
        &prices,
        &costs,
        NOW,
    );
    let desktop = without.iter().find(|g| g.key == "desktop").unwrap();
    assert_eq!(
        desktop.requests, 1,
        "excluded probe rows contribute no request, the credit row stays"
    );
    // What the cache cost to keep warm is not a request, so the probe side of the panel survives a filter
    // that only says a probe request is not one.
    assert_eq!(
        (
            desktop.probes.ok,
            desktop.probes.failed,
            desktop.probes.migrated,
            desktop.probes.skipped
        ),
        (1, 1, 1, 2)
    );
    // The probe's own request row's counts and the settlement's, each priced when read on the shipped list's first
    // entry, in force before its date: Sonnet 5 reads cache at $0.20 per million, so the probe's 2,000 read tokens
    // cost $0.0004, and the settlement's 100 input tokens at $2 and 20 output tokens at $10 per million $0.0002 each.
    assert!(
        (desktop.probe_cost_usd - 0.0004).abs() < 1e-12
            && desktop.wasted_probes == 3
            && (desktop.wasted_probe_usd - 0.0004).abs() < 1e-12,
        "{desktop:?}"
    );
    // A request without a user agent shares the reserved row with the credits of its key. Replacing it under
    // another account moves its own counters and leaves the credit where it was.
    db.insert_request(
        &RequestRecord {
            id: "r3".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            user_agent: None,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r3".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                kept_warm_request_id: Some("r3".into()),
                kept_warm_requests: 1,
                kept_warm_5m_tokens: 500,
                ..delta(at, "inference")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    db.insert_request(
        &RequestRecord {
            id: "r3".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 2,
            user_agent: None,
            ..Default::default()
        },
        &[
            AttemptRecord {
                request_id: "r3".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: AccountKind::ApiKey,
                outcome: "rate_limited".into(),
                ..Default::default()
            },
            AttemptRecord {
                request_id: "r3".into(),
                attempt: 2,
                account_id: "b".into(),
                account_kind: AccountKind::ApiKey,
                outcome: "succeeded".into(),
                ..Default::default()
            },
        ],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            read_rollups(
                c,
                hour_of(at),
                hour_of(at) + 3_600,
                &RollupSelect::default(),
            )
        })
        .unwrap();
    let credited = rows
        .iter()
        .find(|r| r.kind == "inference" && r.account_id == "a" && r.user_agent == "-")
        .expect("the credit row on the old account survives the replace");
    assert_eq!(
        (
            credited.requests,
            credited.kept_warm_requests,
            credited.kept_warm_5m_tokens,
            credited.kept_warm_1h_tokens
        ),
        (0, 3, 500, 2_000),
        "r1, r2 and r3 were all credited on this key, each count under its own TTL"
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.kind == "inference" && r.account_id == "b")
            .map(|r| r.requests),
        Some(1),
        "the request moved to its serving account"
    );
}

/// A kept-warm credit keys the client of the request it credits, so the analytics read prices its saving under that
/// client's regime as the request's own row does. The test cost model sizes the anchor Pro tier's week at 62.5 under
/// `desk` and 50 at the base, and `JANUARY_2026` falls at 04:00 in Los Angeles, before the base regime's peak: ten
/// tokens read on Opus 5.5 (weight 2) save ten writes at the five-minute multiplier 2 less ten reads at 0.1, 38 units,
/// 0.608 of the desk's Pro week and 0.76 of the base's.
#[test]
fn a_kept_warm_credit_folds_under_its_requests_client_and_prices_its_saving_under_that_clients_regime(
) {
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let prices = phoebis_ledger::test_tables::api_prices();
    let at = phoebis_ledger::test_tables::JANUARY_2026;
    let credit = |client: &str| KeepaliveDelta {
        canonical_model: "claude-opus-5-5".into(),
        client: client.into(),
        tier: Some("default_claude_max_20x".into()),
        kept_warm_requests: 1,
        kept_warm_5m_tokens: 10,
        ..delta(at, "inference")
    };
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(&tx, &credit("claude-code/desk"))?;
        fold_keepalive(&tx, &credit("claude-code/sdk-cli"))?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let savings: Vec<(String, f64)> = breakdown(
        &rows,
        "client",
        false,
        &LiveKeys::default(),
        &[],
        None,
        &prices,
        &costs,
        at,
    )
    .into_iter()
    .map(|group| (group.key, group.saving_equivalent.unwrap_or(f64::NAN)))
    .collect();
    let want = [("claude-code/desk", 0.608), ("claude-code/sdk-cli", 0.76)];
    assert!(
        savings.len() == want.len()
            && savings
                .iter()
                .zip(want)
                .all(|((key, got), (client, saving))| key == client
                    && (got - saving).abs() < 1e-12),
        "each credit under its own client and regime: {savings:?}"
    );
}

/// A settlement, a skip and a migration mark belong to the key rather than to one caller, so each folds under the
/// reserved client the savings code hands it.
#[test]
fn a_settlement_a_skip_and_a_migration_mark_fold_under_the_reserved_client() {
    let at = 1_700_000_000_000;
    let reserved = |delta: KeepaliveDelta| KeepaliveDelta {
        client: UNATTRIBUTED.into(),
        ..delta
    };
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &reserved(KeepaliveDelta {
                wasted_probes: 1,
                wasted_input_tokens: 10,
                wasted_probe_reasons: BTreeMap::from([("idle_limit".to_string(), 1)]),
                ..delta(at, "probe")
            }),
        )?;
        fold_keepalive(
            &tx,
            &reserved(KeepaliveDelta {
                probes_skipped: 1,
                ..delta(at, "probe")
            }),
        )?;
        fold_keepalive(
            &tx,
            &reserved(KeepaliveDelta {
                probe_migrated: 1,
                ..delta(at, "probe")
            }),
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows: Vec<(String, (u64, u64, u64))> = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.client.clone(),
                (row.wasted_probes, row.probes_skipped, row.probe_migrated),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [(UNATTRIBUTED.to_string(), (1, 1, 1))],
        "the three deltas share the reserved client's row"
    );
}

#[tokio::test]
async fn the_writer_lands_a_keepalive_record_in_its_batch() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.ledger.record(Record::Keepalive(KeepaliveDelta {
        wasted_probes: 1,
        wasted_output_tokens: 5,
        ..delta(NOW, "probe")
    }));
    let rows = wait_for(Duration::from_secs(5), || {
        let rows =
            h.db.read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
                .unwrap();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        (rows[0].wasted_probes, rows[0].wasted_output_tokens),
        (1, 5)
    );
    up.shutdown().await;
}

/// A usage-based seat's kept-warm credit and wasted probe file under the seat's own funding word beside the request row
/// they credit, ahead of the overage flag the answer reported, as `funding_of` reads the request's attempt.
#[test]
fn a_usage_based_seats_credit_and_wasted_probe_fold_under_usage_based_beside_its_request_row() {
    let db = LedgerDb::open_in_memory().unwrap();
    let at = 1_700_000_000_000;
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: at,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            outcome: "succeeded".into(),
            cache_read_tokens: Some(2_000),
            overage_in_use: Some(true),
            usage_based: true,
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let on_the_seat = |d: KeepaliveDelta| KeepaliveDelta {
        account_kind: Some(phoebis_store::AccountKind::Oauth),
        usage_based: true,
        overage_in_use: Some(true),
        ..d
    };
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &on_the_seat(KeepaliveDelta {
                kept_warm_request_id: Some("r1".into()),
                kept_warm_requests: 1,
                kept_warm_5m_tokens: 2_000,
                ..delta(at, "inference")
            }),
        )?;
        fold_keepalive(
            &tx,
            &on_the_seat(KeepaliveDelta {
                wasted_probes: 1,
                wasted_input_tokens: 100,
                wasted_probe_reasons: BTreeMap::from([("idle_limit".to_string(), 1)]),
                ..delta(at, "probe")
            }),
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            read_rollups(
                c,
                hour_of(at),
                hour_of(at) + 3_600,
                &RollupSelect::default(),
            )
        })
        .unwrap();
    let mut filed: Vec<(&str, &str, u64, u64, u64)> = rows
        .iter()
        .map(|r| {
            (
                r.kind.as_str(),
                r.funding.as_str(),
                r.requests,
                r.kept_warm_requests,
                r.wasted_probes,
            )
        })
        .collect();
    filed.sort();
    assert_eq!(
        filed,
        [
            ("inference", "usage_based", 1, 1, 0),
            ("probe", "usage_based", 0, 0, 1),
        ],
        "the credit lands on its request's own row and the wasted probe beside it, both under the seat's word"
    );
}

/// A fast-mode session's probes and credits are dollars at the fast cells, so they file beside the fast
/// requests they keep warm; a delta that names no speed, as every standard one does, files standard.
#[test]
fn a_fast_recipes_credit_files_under_speed_fast() {
    let db = LedgerDb::open_in_memory().unwrap();
    let at = 1_700_000_000_000;
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                kept_warm_requests: 1,
                kept_warm_5m_tokens: 5_000,
                speed: Some(Speed::Fast),
                ..delta(at, "inference")
            },
        )?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                kept_warm_requests: 1,
                kept_warm_5m_tokens: 1_000,
                ..delta(at, "inference")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            read_rollups(
                c,
                hour_of(at),
                hour_of(at) + 3_600,
                &RollupSelect::default(),
            )
        })
        .unwrap();
    let mut by_speed: Vec<(&str, u64)> = rows
        .iter()
        .map(|r| (r.speed.as_str(), r.kept_warm_5m_tokens))
        .collect();
    by_speed.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(by_speed, [("fast", 5_000), ("standard", 1_000)]);
    assert_eq!(
        rows.iter()
            .find(|r| r.speed == "fast")
            .map(|r| r.funding.as_str()),
        Some("fast"),
        "a fast credit draws usage credits, so a subscription member's budget reads it as metered"
    );
}
