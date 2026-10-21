//! The keepalive record: folded into the hourly rollups on the delta's key, the kept-warm mark on the request,
//! the derived probe counts, and the migration that adds the skipped column.
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
use phoebis_routing::FIVE_HOUR;

/// The dollars of `counts` on Sonnet 5 at the shipped revision in force at `at`, the list `insert_request` folds with
/// and the readers here price at.
fn sonnet_usd(at: i64, input: u64, cache_read: u64, output: u64) -> f64 {
    PriceRevisions::shipped()
        .in_force(at)
        .estimate(
            "claude-sonnet-5",
            Speed::Standard,
            input,
            cache_read,
            0,
            0,
            output,
        )
        .expect("the shipped list carries the model")
        .0
}

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
            account_kind: "api_key".into(),
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
            account_kind: "api_key".into(),
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
    // The probe's own request row's counts and the settlement's, each priced when read.
    let (probe_usd, wasted_usd) = (sonnet_usd(at, 0, 2_000, 0), sonnet_usd(at, 100, 0, 20));
    assert!(probe_usd > 0.0 && wasted_usd > 0.0);
    assert_eq!(
        (
            desktop.probe_cost_usd,
            desktop.wasted_probes,
            desktop.wasted_probe_usd
        ),
        (probe_usd, 3, wasted_usd)
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
/// client's regime as the request's own row does. The test cost model doubles the 20x tier's 5-hour capacity under
/// `desk`, and `JANUARY_2026` falls at 04:00 in Los Angeles, before the base regime's peak: ten tokens read on Opus 5.5
/// (weight 2) save ten writes at the five-minute multiplier 2 less ten reads at 0.1, 38 units, 0.19 of the desk's 200
/// and 0.38 of the base's 100.
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
    )
    .into_iter()
    .map(|group| {
        let five_hour = group.saving_equivalent.get(FIVE_HOUR).copied();
        (group.key, five_hour.unwrap_or(f64::NAN))
    })
    .collect();
    let want = [("claude-code/desk", 0.19), ("claude-code/sdk-cli", 0.38)];
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
async fn migration_five_adds_the_skipped_column_and_the_marks_table_to_a_version_four_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = LedgerDb::open(&path).unwrap();
        // Planted while every column still exists, so the roll-back below has a row to drop columns out from
        // under: proves a row written before migration 15 keeps reading after it, not just an empty table.
        db.insert_request(
            &RequestRecord {
                id: "r0".into(),
                // An hour clear of the range the final rollup assertion below reads, so this row's own
                // fold does not add a second hour-0 row that the assertion's `[0]` could pick up instead.
                received_at: -3_600_000,
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
                request_id: "r0".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "api_key".into(),
                outcome: "succeeded".into(),
                input_tokens: Some(500),
                output_tokens: Some(50),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
        // The replay runs every migration above four over a file the current schema already built, so each
        // column a later migration adds has to go first. The rollup table keeps the generation column it
        // cannot drop (it is in the primary key); the rebuild that replays over it names its own columns and
        // never reads that one.
        // Migration 34 drops the rollup's eight dollar columns and keys it by band and tier, so the table goes back to
        // the version-33 shape before the drops below, rebuilt since a primary-key column cannot be dropped in place.
        // Migrations 35 and 38 create the conditions' and the reconciliations' tables, so both leave before the replay
        // creates them again, the second before migration 20 creates a table of its name. Migration 39's column and
        // table leave for the same reason. Migration 40's two request columns leave for the same reason. Migration 41's
        // index leaves before its column, which SQLite refuses to drop while indexed, and its wall column leaves with
        // them. Migration 42's flag and refusals map leave too, both ahead of the rebuild, which lays the rollup table
        // out from its own columns and leaves no map column to drop. Migration 43's attempt class leaves as well, while
        // the samples table keeps its shape, since that step's rebuild names the columns it copies.
        let mut sql = format!(
            "ALTER TABLE attempts DROP COLUMN usage_based; \
             ALTER TABLE requests DROP COLUMN refused; ALTER TABLE rollups_hourly DROP COLUMN refusals_json; \
             DROP INDEX requests_landed_at; ALTER TABLE requests DROP COLUMN landed_at; \
             ALTER TABLE account_rollups_hourly DROP COLUMN wall_early_ms; \
             DROP TABLE routing_conditions; DROP TABLE window_reconciliations; {VERSION_33_ROLLUPS} "
        );
        sql.push_str(
            "ALTER TABLE requests DROP COLUMN classifier; ALTER TABLE requests DROP COLUMN client_contract; \
             DROP TABLE request_guardrails; ALTER TABLE requests DROP COLUMN guardrail_id; ",
        );
        sql.push_str("ALTER TABLE attempts DROP COLUMN written_at; ALTER TABLE requests DROP COLUMN diagnostics_source; ALTER TABLE requests DROP COLUMN stream_end_at; ALTER TABLE rollups_hourly DROP COLUMN probes_held; ALTER TABLE requests DROP COLUMN key_generation; ALTER TABLE rollups_hourly DROP COLUMN probes_skipped; DROP TABLE kept_warm_marks; ALTER TABLE bodies DROP COLUMN request_len_seen; ALTER TABLE bodies DROP COLUMN response_len_seen; ALTER TABLE account_rollups_hourly DROP COLUMN rewrite_tokens; ALTER TABLE account_rollups_hourly DROP COLUMN migrations_json; ");
        for column in [
            "cleared_input_tokens",
            "inference_geo",
            "refusal_category",
            "refusal_recommended_model",
            "selection",
            "served_model",
            "speed",
            "stop_reason",
            "stop_sequence",
            "thinking_tokens",
            "upstream_message_id",
            "web_fetch_requests",
            "web_search_requests",
        ] {
            sql.push_str(&format!("ALTER TABLE attempts DROP COLUMN {column}; "));
        }
        for column in [
            "agent_type",
            "auto_mode",
            "client",
            "client_drift",
            "compaction",
            "context_compacted",
            "context_edits",
            "effort",
            "fork_depth",
            "forked_from",
            "held",
            "identified_by",
            "identity_depth",
            "identity_root",
            "max_tokens",
            "message_count",
            "presented_retired",
            "prev_tool_ms",
            "request_class",
            "request_truncated",
            "requested_service_tier",
            "requested_speed",
            "response_truncated",
            "rotation_at",
            "system_blocks",
            "temperature",
            "thinking_budget",
            "thinking_type",
            "tool_count",
            "turn_origin",
        ] {
            sql.push_str(&format!("ALTER TABLE requests DROP COLUMN {column}; "));
        }
        // Migration 33 drops the attempt's five dollar columns and its stamp and adds the tier, so the attempts table
        // goes back to the shape a version-four file has before the replay runs it.
        sql.push_str("ALTER TABLE attempts DROP COLUMN tier; ");
        for column in [
            "cache_read_usd REAL",
            "cache_write_usd REAL",
            "estimated_usd REAL",
            "input_usd REAL",
            "output_usd REAL",
            "pricing_version TEXT",
        ] {
            sql.push_str(&format!("ALTER TABLE attempts ADD COLUMN {column}; "));
        }
        sql.push_str("ALTER TABLE quota_samples DROP COLUMN observed_at; ");
        sql.push_str("DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; ");
        sql.push_str("DROP TABLE decisions; DROP INDEX IF EXISTS events_account; ");
        sql.push_str("DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; ");
        sql.push_str("ALTER TABLE events DROP COLUMN request_id; ");
        sql.push_str(
            "ALTER TABLE events DROP COLUMN agent_id; ALTER TABLE events DROP COLUMN pool_id; ",
        );
        sql.push_str("UPDATE meta SET value = '4' WHERE key = 'schema_version';");
        db.write(|c| {
            c.execute_batch(&sql)?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 43);
    let (_, attempts) = db.request("r0").await.unwrap().unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "every attempt row written before migration 15 reads after it"
    );
    assert_eq!(attempts[0].account_id, "a");
    assert_eq!(attempts[0].account_kind, "api_key");
    assert_eq!(attempts[0].outcome, "succeeded");
    assert_eq!(attempts[0].input_tokens, Some(500));
    assert_eq!(attempts[0].output_tokens, Some(50));
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
        db.read_blocking(|c| read_rollups(c, 0, 3_600, &RollupSelect::default()))
            .unwrap()[0]
            .probes_skipped,
        1
    );
}

#[tokio::test]
async fn migration_thirty_seven_keeps_a_rollup_rows_kept_warm_tokens_under_five_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = LedgerDb::open(&path).unwrap();
        db.insert_request(
            &RequestRecord {
                id: "r0".into(),
                received_at: 0,
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
                request_id: "r0".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "api_key".into(),
                outcome: "succeeded".into(),
                input_tokens: Some(500),
                output_tokens: Some(50),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
        // The file as version 36 left it: the one count, holding what a credit booked before the split.
        // Migration 38 creates the reconciliations' table and migration 39 the guardrails' column and table, so each
        // leaves before the replay adds it again. Migration 40's two request columns, migration 41's index and two
        // columns, the index first, migration 42's flag and refusals map and migration 43's attempt class leave for the
        // same reason.
        db.write(|c| {
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN usage_based; \
                 ALTER TABLE requests DROP COLUMN refused; ALTER TABLE rollups_hourly DROP COLUMN refusals_json; \
                 DROP INDEX requests_landed_at; ALTER TABLE requests DROP COLUMN landed_at; \
                 ALTER TABLE account_rollups_hourly DROP COLUMN wall_early_ms; \
                 ALTER TABLE requests DROP COLUMN classifier; ALTER TABLE requests DROP COLUMN client_contract; \
                 DROP TABLE window_reconciliations; \
                 DROP TABLE request_guardrails; ALTER TABLE requests DROP COLUMN guardrail_id; \
                 ALTER TABLE rollups_hourly DROP COLUMN kept_warm_1h_tokens; \
                 ALTER TABLE rollups_hourly RENAME COLUMN kept_warm_5m_tokens TO kept_warm_tokens; \
                 UPDATE rollups_hourly SET kept_warm_requests = 3, kept_warm_tokens = 2000; \
                 UPDATE meta SET value = '36' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 43);
    let counts: (i64, i64, i64) = db
        .read_blocking(|c| {
            Ok(c.query_row(
                "SELECT kept_warm_requests, kept_warm_5m_tokens, kept_warm_1h_tokens FROM rollups_hourly",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(
        counts,
        (3, 2_000, 0),
        "a past row's count stays whole under five minutes, since its split was never recorded"
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
            account_kind: "oauth".into(),
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
