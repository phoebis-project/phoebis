//! The keepalive record: folded into the hourly rollups on the delta's key, the kept-warm mark on the request,
//! the derived probe counts, and the migration that adds the skipped column.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use phoebis_ledger::{
    fold_keepalive, hour_of, read_rollups, AttemptRecord, KeepaliveDelta, LedgerConfig, LedgerDb,
    LiveKeys, Record, RequestRecord,
};
use common::*;
use fake_upstream::FakeUpstream;

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
            account_kind: "api_key".into(),
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
                kept_warm_tokens: 2_000,
                saved_usd_gross: 0.0036,
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
                probe_cost_usd: 0.0004,
                ..delta(at + 1_000, "probe")
            },
        )?;
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
                wasted_probe_usd: 0.0012,
                wasted_probe_reasons: BTreeMap::from([("idle_limit".to_string(), 3)]),
                ..delta(at + 4_000, "probe")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, hour_of(at), hour_of(at) + 3_600))
        .unwrap();
    let inference = rows
        .iter()
        .find(|r| r.kind == "inference" && r.user_agent == "-")
        .expect("the credit lands on a user-agent-less row of the request's key");
    assert_eq!(
        (
            inference.kept_warm_requests,
            inference.kept_warm_tokens,
            inference.saved_usd_gross,
            inference.requests
        ),
        (1, 2_000, 0.0036, 0),
        "the delta touches no request counter"
    );
    let probe = rows.iter().find(|r| r.kind == "probe").unwrap();
    assert_eq!(
        (
            probe.probe_cost_usd,
            probe.probe_migrated,
            probe.probes_skipped,
            probe.wasted_probes,
            probe.wasted_probe_usd
        ),
        (0.0004, 1, 2, 3, 0.0012)
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
            account_kind: "api_key".into(),
            outcome: "succeeded".into(),
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
            account_kind: "api_key".into(),
            outcome: "rate_limited".into(),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, hour_of(at), hour_of(at) + 3_600))
        .unwrap();
    let with_probes = phoebis_ledger::RollupFilter {
        include_probes: true,
        ..Default::default()
    };
    // The rows above carry the generation a record defaults to, so that is what holds the id here.
    let live = LiveKeys(BTreeMap::from([("desktop".to_string(), 0)]));
    let groups = phoebis_ledger::breakdown(
        &phoebis_ledger::apply_filter(rows.clone(), &with_probes, &live),
        "key",
        with_probes.probes_counted(),
        &live,
        &[],
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
        &phoebis_ledger::apply_filter(rows, &default, &live),
        "key",
        default.probes_counted(),
        &live,
        &[],
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
    assert_eq!(
        (
            desktop.savings.probe_cost_usd,
            desktop.savings.wasted_probes,
            desktop.savings.wasted_probe_usd
        ),
        (0.0004, 3, 0.0012)
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
            account_kind: "api_key".into(),
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
                kept_warm_tokens: 500,
                saved_usd_gross: 0.0009,
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
                account_kind: "api_key".into(),
                outcome: "rate_limited".into(),
                ..Default::default()
            },
            AttemptRecord {
                request_id: "r3".into(),
                attempt: 2,
                account_id: "b".into(),
                account_kind: "api_key".into(),
                outcome: "succeeded".into(),
                ..Default::default()
            },
        ],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, hour_of(at), hour_of(at) + 3_600))
        .unwrap();
    let credited = rows
        .iter()
        .find(|r| r.kind == "inference" && r.account_id == "a" && r.user_agent == "-")
        .expect("the credit row on the old account survives the replace");
    assert_eq!(
        (
            credited.requests,
            credited.kept_warm_requests,
            credited.kept_warm_tokens
        ),
        (0, 3, 2_500),
        "r1, r2 and r3 were all credited on this key"
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.kind == "inference" && r.account_id == "b")
            .map(|r| r.requests),
        Some(1),
        "the request moved to its serving account"
    );
}

#[tokio::test]
async fn migration_five_adds_the_skipped_column_and_the_marks_table_to_a_version_four_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = LedgerDb::open(&path).unwrap();
        // The replay runs every migration above four over a file the current schema already built, so each
        // column a later migration adds has to go first. The rollup table keeps the generation column it
        // cannot drop (it is in the primary key); the rebuild that replays over it names its own columns and
        // never reads that one.
        let mut sql = String::from("ALTER TABLE requests DROP COLUMN key_generation; ALTER TABLE rollups_hourly DROP COLUMN probes_skipped; DROP TABLE kept_warm_marks; ");
        for column in [
            "cleared_input_tokens",
            "inference_geo",
            "refusal_category",
            "refusal_recommended_model",
            "served_model",
            "speed",
            "stop_reason",
            "stop_sequence",
            "thinking_tokens",
            "web_fetch_requests",
            "web_search_requests",
        ] {
            sql.push_str(&format!("ALTER TABLE attempts DROP COLUMN {column}; "));
        }
        for column in [
            "context_edits",
            "effort",
            "max_tokens",
            "message_count",
            "requested_service_tier",
            "requested_speed",
            "system_blocks",
            "temperature",
            "thinking_budget",
            "thinking_type",
            "tool_count",
        ] {
            sql.push_str(&format!("ALTER TABLE requests DROP COLUMN {column}; "));
        }
        sql.push_str("UPDATE meta SET value = '4' WHERE key = 'schema_version';");
        db.write(|c| {
            c.execute_batch(&sql)?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 9);
    db.write(|c| {
        let tx = c.transaction()?;
        fold_keepalive(
            &tx,
            &KeepaliveDelta {
                probes_skipped: 1,
                ..delta(0, "probe")
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        db.read_blocking(|c| read_rollups(c, 0, 3_600)).unwrap()[0].probes_skipped,
        1
    );
}

#[tokio::test]
async fn the_writer_lands_a_keepalive_record_in_its_batch() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.ledger.record(Record::Keepalive(KeepaliveDelta {
        probe_cost_usd: 0.5,
        ..delta(NOW, "probe")
    }));
    let rows = wait_for(Duration::from_secs(5), || {
        let rows =
            h.db.read_blocking(|c| read_rollups(c, 0, i64::MAX))
                .unwrap();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(rows[0].probe_cost_usd, 0.5);
    up.shutdown().await;
}
