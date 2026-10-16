//! Router and registry events reach the events table, and the account rollup counts what they say.
mod common;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_ledger::{event_of, recent_events_of_kind, EventRecord, LedgerConfig, LedgerDb};
use phoebis_routing::RouterEvent;
use phoebis_store::{AccountState, StateCause, StorageKey, Store};
use tower::ServiceExt;

#[tokio::test]
async fn a_failover_writes_cooldown_set_and_pin_migrated_and_the_account_rollup_counts_them() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "rejected".into(),
                ),
                ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
                ("anthropic-ratelimit-unified-reset".into(), "4600".into()),
            ],
            body: b"{}".to_vec(),
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        (0, None),
        LedgerConfig {
            event_poll: Duration::from_millis(50),
            ..LedgerConfig::default()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let events = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "pin_migrated")
            .then_some(rows)
    })
    .await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["pin_created", "cooldown_set", "pin_migrated"]);
    assert_eq!(
        (
            events[1].account_id.as_deref(),
            events[2].account_id.as_deref(),
            events[2].session_id.as_deref()
        ),
        (Some("a"), Some("b"), Some("s1"))
    );
    let hour = phoebis_ledger::hour_of(NOW);
    let rollups = wait_for(Duration::from_secs(5), || {
        let rows =
            h.db.read_blocking(|c| phoebis_ledger::read_account_rollups(c, hour, hour + 3_600))
                .unwrap();
        (rows.len() == 2).then_some(rows)
    })
    .await;
    let a = rollups.iter().find(|r| r.account_id == "a").unwrap();
    let b = rollups.iter().find(|r| r.account_id == "b").unwrap();
    // The classifier rounds the deadline up to the next second: until is 4_601_000 ms, at is NOW. Of the
    // 3,601 s elapsed, only the 2,600 that fall before this hour's end book here; the rest books to the next
    // hour, outside this query's window.
    assert_eq!(
        (a.pins_created, a.pins_migrated_out, a.cooldown_seconds),
        (1, 1, 2_600)
    );
    assert_eq!((b.pins_migrated_in, b.pins_created), (1, 0));
    up.shutdown().await;
}

#[tokio::test]
async fn a_pin_event_s_agent_and_a_cooldown_s_request_land_in_their_columns() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "rejected".into(),
                ),
                ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
                ("anthropic-ratelimit-unified-reset".into(), "4600".into()),
            ],
            body: b"{}".to_vec(),
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = resp
        .extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .unwrap()
        .request_id
        .clone();
    let _ = resp.into_body().collect().await.unwrap();
    let events = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "pin_migrated")
            .then_some(rows)
    })
    .await;
    let row = |kind: &str| events.iter().find(|e| e.kind == kind).unwrap();
    assert_eq!(
        (
            row("pin_migrated").agent_id.as_deref(),
            row("pin_migrated").request_id.as_deref()
        ),
        (Some("main"), None)
    );
    assert_eq!(
        (
            row("cooldown_set").agent_id.as_deref(),
            row("cooldown_set").request_id.as_deref()
        ),
        (None, Some(id.as_str())),
        "the cooldown names the request whose answer set it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn registry_changes_and_resolved_refreshes_are_mirrored_and_a_reauth_mark_is_one_transition_row(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            event_poll: Duration::from_millis(50),
            ..LedgerConfig::default()
        },
    )
    .await;
    // Seeded rows are before the mirror's watermark: nothing is replayed.
    stays_for_async(Duration::from_millis(200), || async {
        h.db.events(0, i64::MAX, 100).await.unwrap().is_empty()
    })
    .await;
    rename(
        &h,
        "a",
        h.store.account_revision("a").unwrap().unwrap(),
        "renamed",
        "admin:abc",
    )
    .unwrap();
    let event = wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "registry_changed")
    })
    .await;
    assert_eq!(event.actor.as_deref(), Some("admin:abc"));
    assert_eq!(event.pool_id, None, "an account's change names no pool");
    let detail: serde_json::Value = serde_json::from_str(event.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        (detail["kind"].as_str(), detail["target"].as_str()),
        (Some("account_updated"), Some("a"))
    );
    let attempt = h.store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    h.store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Failed(
                "refresh rejected with status 401 (invalid_grant)".into(),
            ),
        )
        .unwrap();
    h.store
        .set_state(
            "a",
            phoebis_store::AccountState::ReauthRequired,
            StateCause::Operator,
        )
        .unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.events(0, i64::MAX, 100).await.unwrap();
        (rows.iter().any(|e| e.kind == "refresh_failed")
            && rows.iter().any(|e| e.kind == "account_state_changed"))
        .then_some(())
    })
    .await;
    assert_eq!(
        account_rows(&h).await,
        [transition("ready", "reauth_required", "operator", None)],
        "the transition row records the move into the state"
    );
    stays_for_async(Duration::from_millis(200), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .iter()
            .all(|e| e.kind != "reauth_required")
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn an_uncertain_refresh_mirrors_as_refresh_uncertain() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        (0, None),
        LedgerConfig {
            event_poll: Duration::from_millis(50),
            ..LedgerConfig::default()
        },
    )
    .await;
    let uncertain = h.store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    h.store
        .resolve_refresh_attempt(
            uncertain,
            1,
            phoebis_store::AttemptOutcome::Uncertain("token request timed out".into()),
        )
        .unwrap();
    let failed = h.store.open_refresh_attempt("b", 1, 1, 1).unwrap();
    h.store
        .resolve_refresh_attempt(
            failed,
            1,
            phoebis_store::AttemptOutcome::Failed(
                "refresh rejected with status 401 (invalid_grant)".into(),
            ),
        )
        .unwrap();
    let kinds = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "refresh_failed")
            .then(|| rows.iter().map(|e| e.kind.clone()).collect::<Vec<_>>())
    })
    .await;
    assert!(
        kinds.contains(&"refresh_uncertain".to_string()),
        "an uncertain outcome mirrors as its own kind: {kinds:?}"
    );
    assert!(
        kinds.contains(&"refresh_failed".to_string()),
        "a plain failure still mirrors as refresh_failed: {kinds:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refresh_row_names_the_failure_s_class_and_text_or_the_committed_token_s_expiry() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        (0, None),
        polled_every_50_ms(),
    )
    .await;
    let failed = h.store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    h.store
        .resolve_refresh_attempt(
            failed,
            1,
            phoebis_store::AttemptOutcome::Failed(
                "refresh rejected with status 400 (invalid_grant)".into(),
            ),
        )
        .unwrap();
    let uncertain = h.store.open_refresh_attempt("b", 1, 1, 1).unwrap();
    h.store
        .resolve_refresh_attempt(
            uncertain,
            1,
            phoebis_store::AttemptOutcome::Uncertain(
                "refresh response lost: connection reset".into(),
            ),
        )
        .unwrap();
    let committed = h.store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    h.store
        .commit_credentials(
            "a",
            1,
            1,
            Some(1),
            Some(committed),
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new("rotated"),
                refresh_token: Some(phoebis_store::Secret::new("rotated-refresh")),
                issued_at: NOW,
                expires_at: NOW + 3_600_000,
                refresh_expires_at: Some(NOW + 86_400_000),
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    let details = wait_for_async(Duration::from_secs(5), || async {
        let rows: Vec<(String, Option<String>, serde_json::Value)> =
            h.db.events(0, i64::MAX, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.kind.starts_with("refresh_"))
                .map(|e| {
                    let detail = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok())
                        .unwrap_or(serde_json::Value::Null);
                    (e.kind, e.account_id, detail)
                })
                .collect();
        (rows.len() == 3).then_some(rows)
    })
    .await;
    let of = |kind: &str| {
        details
            .iter()
            .find(|(k, ..)| k == kind)
            .map(|(_, account, detail)| (account.clone(), detail.clone()))
            .unwrap()
    };
    assert_eq!(
        of("refresh_failed"),
        (
            Some("a".to_string()),
            serde_json::json!({
                "cause": "invalid_grant",
                "error": "refresh rejected with status 400 (invalid_grant)"
            })
        )
    );
    assert_eq!(
        of("refresh_uncertain"),
        (
            Some("b".to_string()),
            serde_json::json!({
                "cause": "refresh_response_lost",
                "error": "refresh response lost: connection reset"
            })
        )
    );
    assert_eq!(
        of("refresh_succeeded"),
        (
            Some("a".to_string()),
            serde_json::json!({
                "until": NOW + 3_600_000,
                "access_expires_at": NOW + 3_600_000,
                "refresh_expires_at": NOW + 86_400_000
            })
        )
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refresh_row_names_the_expiry_of_the_bundle_its_own_commit_wrote() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    let bundle = |expires_at: i64| phoebis_store::NewBundle {
        access_token: phoebis_store::Secret::new("rotated"),
        refresh_token: Some(phoebis_store::Secret::new("rotated-refresh")),
        issued_at: NOW,
        expires_at,
        refresh_expires_at: None,
        scopes: "user:inference".into(),
    };
    // Two commits before the mirror's next tick: the first one's bundle is gone by the time the tick reads it.
    let first = h.store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    h.store
        .commit_credentials("a", 1, 1, Some(1), Some(first), &bundle(NOW + 3_600_000))
        .unwrap();
    let second = h.store.open_refresh_attempt("a", 1, 1, 2).unwrap();
    h.store
        .commit_credentials("a", 1, 1, Some(2), Some(second), &bundle(NOW + 7_200_000))
        .unwrap();
    let untils = wait_for_async(Duration::from_secs(5), || async {
        let rows: Vec<serde_json::Value> =
            h.db.events(0, i64::MAX, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "refresh_succeeded")
                .map(|e| serde_json::from_str(e.detail.as_deref().unwrap()).unwrap())
                .collect();
        (rows.len() == 2).then(|| {
            rows.iter()
                .map(|detail| detail["until"].clone())
                .collect::<Vec<_>>()
        })
    })
    .await;
    assert_eq!(
        untils,
        [serde_json::Value::Null, serde_json::json!(NOW + 7_200_000)],
        "a commit whose bundle a later write replaced names no expiry rather than the newer bundle's"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_registry_change_lands_with_its_diff_decoded_once() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            event_poll: Duration::from_millis(50),
            ..LedgerConfig::default()
        },
    )
    .await;
    let pool = h.store.pool("main").unwrap().unwrap();
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    let row = wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| {
                e.kind == "registry_changed"
                    && e.detail
                        .as_deref()
                        .is_some_and(|d| d.contains("pool_updated"))
            })
    })
    .await;
    assert_eq!(
        row.pool_id.as_deref(),
        Some("main"),
        "a pool's change names the pool"
    );
    let detail: serde_json::Value = serde_json::from_str(row.detail.as_deref().unwrap()).unwrap();
    assert!(
        detail["diff"].is_object(),
        "the diff is a value, not a string: {detail}"
    );
    up.shutdown().await;
}

#[test]
fn recent_events_of_kind_answers_the_newest_first_and_stops_at_the_limit() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (at, account_id) in [(100, "a"), (200, "b"), (300, "c")] {
        db.insert_event(&EventRecord {
            at,
            kind: "model_rejected".into(),
            account_id: Some(account_id.into()),
            ..Default::default()
        })
        .unwrap();
    }
    let newest_two = db
        .read_blocking(|c| recent_events_of_kind(c, "model_rejected", 2))
        .unwrap();
    assert_eq!(
        newest_two
            .iter()
            .map(|e| (e.at, e.account_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![(300, Some("c")), (200, Some("b"))],
        "the newest two, newest first"
    );
}

#[test]
fn a_pin_migrated_event_counts_under_its_reason_on_the_destination() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        phoebis_ledger::fold_event(
            &tx,
            &EventRecord {
                at: 3_600_000,
                kind: "pin_migrated".into(),
                account_id: Some("b".into()),
                // The fold reads the detail by key, so the failover's cause object beside the reason changes no count.
                detail: Some(
                    r#"{"reason":"overage","from_account":"a","cause":{"scope":"account","reason":"rate_limit","window":"five_hour"}}"#
                        .into(),
                ),
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_account_rollups(c, 0, i64::MAX))
        .unwrap();
    let to = rows.iter().find(|r| r.account_id == "b").unwrap();
    assert_eq!(
        to.migrations.get("overage/unknown"),
        Some(&1),
        "a move without a fit files under the fit unknown"
    );
    let from = rows.iter().find(|r| r.account_id == "a").unwrap();
    assert_eq!(
        from.migrations.get("overage/unknown"),
        None,
        "the source row counts pins_migrated_out, not a reason of its own"
    );
}

/// Folds `events` into the account rollups of `db` in one transaction.
fn fold_events(db: &LedgerDb, events: &[EventRecord]) {
    db.write(|c| {
        let tx = c.transaction()?;
        for event in events {
            phoebis_ledger::fold_event(&tx, event)?;
        }
        tx.commit()?;
        Ok(())
    })
    .unwrap();
}

/// A move onto `to` for `reason`, its detail naming the destination's `fit` where the gate read one.
fn moved(at: i64, to: &str, reason: &str, fit: Option<&str>) -> EventRecord {
    EventRecord {
        at,
        kind: "pin_migrated".into(),
        account_id: Some(to.into()),
        // A rule's move writes its cause as null, which the fold leaves alone as it does an object.
        detail: Some(
            serde_json::json!({ "reason": reason, "from_account": "a", "fit": fit, "cause": null })
                .to_string(),
        ),
        ..Default::default()
    }
}

/// The first two hours, in one-hour buckets.
fn two_hours() -> phoebis_ledger::SeriesFilter {
    phoebis_ledger::SeriesFilter {
        from: 0,
        to: 7_200_000,
        bucket_seconds: 3_600,
        ..phoebis_ledger::SeriesFilter::default()
    }
}

#[test]
fn a_pin_migrated_event_counts_under_its_reason_and_its_fit_on_the_destination() {
    let db = LedgerDb::open_in_memory().unwrap();
    fold_events(&db, &[moved(3_600_000, "b", "overage", Some("fits"))]);
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_account_rollups(c, 0, i64::MAX))
        .unwrap();
    let to = rows.iter().find(|r| r.account_id == "b").unwrap();
    assert_eq!(
        to.migrations,
        std::collections::BTreeMap::from([("overage/fits".to_string(), 1)]),
        "the key joins the reason and the destination's fit"
    );
    let filter = two_hours();
    let by_reason = |db: &LedgerDb| {
        db.read_blocking(|c| phoebis_ledger::moves_by_reason(c, &filter))
            .unwrap()
    };
    let by_fit = |db: &LedgerDb| {
        db.read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
            .unwrap()
    };
    assert_eq!(
        by_reason(&db),
        std::collections::BTreeMap::from([("overage".to_string(), 1)])
    );
    assert_eq!(
        by_fit(&db),
        std::collections::BTreeMap::from([(
            "overage".to_string(),
            std::collections::BTreeMap::from([("fits".to_string(), 1)])
        )])
    );
    // A move outside the gate carries no fit, so it reads under the fit unknown beside the gated one.
    fold_events(&db, &[moved(3_600_100, "b", "overage", None)]);
    assert_eq!(
        by_reason(&db),
        std::collections::BTreeMap::from([("overage".to_string(), 2)])
    );
    assert_eq!(
        by_fit(&db),
        std::collections::BTreeMap::from([(
            "overage".to_string(),
            std::collections::BTreeMap::from([("fits".to_string(), 1), ("unknown".to_string(), 1)])
        )])
    );
}

#[test]
fn two_reasons_keep_their_own_fit_counts_in_the_moves_by_reason_and_fit() {
    let db = LedgerDb::open_in_memory().unwrap();
    fold_events(
        &db,
        &[
            moved(3_600_000, "b", "spent", Some("fits")),
            moved(3_600_100, "c", "spent", Some("fits")),
            moved(3_600_200, "b", "shed", Some("least_bad")),
        ],
    );
    let filter = two_hours();
    assert_eq!(
        db.read_blocking(|c| phoebis_ledger::moves_by_reason_and_fit(c, &filter))
            .unwrap(),
        std::collections::BTreeMap::from([
            (
                "shed".to_string(),
                std::collections::BTreeMap::from([("least_bad".to_string(), 1)])
            ),
            (
                "spent".to_string(),
                std::collections::BTreeMap::from([("fits".to_string(), 2)])
            ),
        ]),
        "each reason keeps its own fits, summed over the accounts"
    );
    assert_eq!(
        db.read_blocking(|c| phoebis_ledger::moves_by_reason(c, &filter))
            .unwrap(),
        std::collections::BTreeMap::from([("shed".to_string(), 1), ("spent".to_string(), 2)]),
        "the moves by reason are the sum over each reason's fits"
    );
}

/// One reconciled window as the ledger's hourly fold writes it, its readings three hours and one hour before its end.
#[allow(clippy::too_many_arguments)]
fn reconciled(
    db: &LedgerDb,
    account: &str,
    window: &str,
    window_end: i64,
    rise: f64,
    predicted: f64,
    ratio: f64,
    requests: i64,
) {
    db.write(|c| {
        c.execute(
            "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
             last_reading_at, observed_rise, predicted_share, ratio, requests, estimated, computed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?3)",
            rusqlite::params![
                account,
                window,
                window_end,
                window_end - 3 * 3_600_000,
                window_end - 3_600_000,
                rise,
                predicted,
                ratio,
                requests
            ],
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn the_three_routing_conditions_fold_from_the_days_events_and_clear_when_the_day_passes() {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    let t = 1_000 * HOUR;
    let event =
        |at: i64, kind: &str, account: &str, pool: Option<&str>, detail: serde_json::Value| {
            EventRecord {
                at,
                kind: kind.into(),
                account_id: Some(account.into()),
                pool_id: pool.map(String::from),
                detail: Some(detail.to_string()),
                ..Default::default()
            }
        };
    let round = |at: i64, account: &str, outcome: &str, reason: Option<&str>, projection: f64| {
        event(
            at,
            "shedding_round",
            account,
            Some("p"),
            serde_json::json!({
                "window": "five_hour",
                "projection": projection,
                "outcome": outcome,
                "reason": reason,
            }),
        )
    };
    let spent = |at: i64, account: &str, source: &str| {
        event(
            at,
            "window_spent",
            account,
            None,
            serde_json::json!({ "window": "five_hour", "source": source }),
        )
    };
    let db = LedgerDb::open_in_memory().unwrap();
    for row in [
        // A round on `a` that found no destination with runway, projecting 0.97 at its horizon, and a round on `c`
        // that marked a pin, which no condition reads.
        round(t, "a", "nothing_to_mark", Some("no_runway"), 0.97),
        round(t + 30 * MINUTE, "c", "marked 1", None, 0.99),
        // A move onto `b` rewrote its cache there, and b's 5-hour window was spent 20 minutes later. A move onto
        // `c` was followed by no spent window.
        moved(t + HOUR, "b", "shed", Some("rewrite_only")),
        moved(t + HOUR, "c", "spent", Some("fits")),
        spent(t + HOUR + 20 * MINUTE, "b", "head"),
        // `a` reached its wall on the router's own prediction, two hours after its round projected 0.97.
        spent(t + 2 * HOUR, "a", "predicted"),
        // Just past each bound: d's window was spent a millisecond more than an hour after the move onto it, e's a
        // millisecond before the move, and f's round projected a millisecond more than the window's length before
        // f's wall. Each account reached its wall, and none of them holds a spent destination or a projection error.
        moved(t + 30 * MINUTE, "d", "spent", Some("fits")),
        spent(t + 30 * MINUTE + HOUR + 1, "d", "head"),
        spent(t + 40 * MINUTE, "e", "head"),
        moved(t + 40 * MINUTE + 1, "e", "shed", Some("fits")),
        round(t + 2 * HOUR - 5 * HOUR - 1, "f", "marked 1", None, 0.9),
        spent(t + 2 * HOUR, "f", "poll"),
    ] {
        db.insert_event(&row).unwrap();
    }
    // `g`'s newest 5-hour window charged twice the table. `h`'s older 5-hour window drifted, and its newest read
    // inside the band, which clears it.
    reconciled(&db, "g", "five_hour", t + HOUR, 0.5, 0.25, 2.0, 3);
    reconciled(&db, "h", "five_hour", t - 4 * HOUR, 0.5, 0.25, 2.0, 3);
    reconciled(&db, "h", "five_hour", t + HOUR, 0.3, 0.3, 1.0, 2);
    let tables = phoebis_ledger::test_tables::subscription_costs();
    let fold = |now: i64| {
        db.write(|c| {
            let tx = c.transaction()?;
            phoebis_ledger::fold_routing_conditions(&tx, &tables, now)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap()
    };
    let now = t + 3 * HOUR;
    fold(now);
    let rows = db
        .read_blocking(phoebis_ledger::routing_conditions)
        .unwrap();
    let ceiling = serde_json::json!(["window_ceiling"]);
    let wall_settings = serde_json::json!(["shed_cap", "window_ceiling"]);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.kind.as_str(),
                r.subject_kind.as_str(),
                r.subject_id.as_str(),
                r.pool_id.as_deref(),
                r.since,
                r.count,
                r.computed_at,
            ))
            .collect::<Vec<_>>(),
        [
            ("destination_spent", "account", "b", None, t + HOUR, 1, now),
            ("meter_drift", "account", "g", None, t + HOUR, 1, now),
            ("shed_no_runway", "pool", "p", Some("p"), t, 1, now),
            ("wall_reached", "account", "a", None, t + 2 * HOUR, 1, now),
            (
                "wall_reached",
                "account",
                "b",
                None,
                t + HOUR + 20 * MINUTE,
                1,
                now
            ),
            (
                "wall_reached",
                "account",
                "d",
                None,
                t + 30 * MINUTE + HOUR + 1,
                1,
                now
            ),
            (
                "wall_reached",
                "account",
                "e",
                None,
                t + 40 * MINUTE,
                1,
                now
            ),
            ("wall_reached", "account", "f", None, t + 2 * HOUR, 1, now),
        ],
        "one row per subject, dated by its oldest qualifying event: {rows:?}"
    );
    let unread_wall = serde_json::json!({
        "projection_error": null,
        "setting": wall_settings,
        "walls": 1,
        "windows": ["five_hour"],
    });
    // The test tables' first entry is in force before its date, as the list's earliest.
    let drift = serde_json::json!({
        "effective": "2026-01-01",
        "source": "test",
        "windows": [{
            "observed_rise": 0.5,
            "predicted_share": 0.25,
            "ratio": 2.0,
            "requests": 3,
            "window": "five_hour",
            "window_end": t + HOUR,
        }],
    });
    assert_eq!(
        rows.iter().map(|r| r.detail.clone()).collect::<Vec<_>>(),
        [
            serde_json::json!({
                "fits": { "rewrite_only": 1 },
                "moves": 1,
                "setting": ceiling,
                "windows": ["five_hour"],
            }),
            drift.clone(),
            serde_json::json!({ "accounts": ["a"], "rounds": 1, "setting": ceiling }),
            serde_json::json!({
                "projection_error": 1.0 - 0.97,
                "setting": wall_settings,
                "walls": 1,
                "windows": ["five_hour"],
            }),
            unread_wall.clone(),
            unread_wall.clone(),
            unread_wall.clone(),
            unread_wall,
        ],
        "a wall with no round on its account inside the window's length before it has no projection error"
    );
    fold(now + 25 * HOUR);
    assert_eq!(
        db.read_blocking(phoebis_ledger::routing_conditions)
            .unwrap()
            .iter()
            .map(|r| (r.kind.as_str(), r.subject_id.as_str(), r.since, &r.detail))
            .collect::<Vec<_>>(),
        [("meter_drift", "g", t + HOUR, &drift)],
        "a day later the trailing 24 hours hold none of the events, so every condition they fed clears, while the \
         drift reads the reconciled windows"
    );
}

#[test]
fn a_weekly_wall_reads_no_projection_error_from_a_round_on_the_five_hour_window() {
    const HOUR: i64 = 3_600_000;
    let t = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    for row in [
        // A 5-hour round an hour before `a`'s 7-day window was spent: its projection reads another window's share.
        EventRecord {
            at: t,
            kind: "shedding_round".into(),
            account_id: Some("a".into()),
            pool_id: Some("p".into()),
            detail: Some(
                serde_json::json!({
                    "window": "five_hour",
                    "projection": 0.97,
                    "outcome": "marked 1",
                    "reason": null,
                })
                .to_string(),
            ),
            ..Default::default()
        },
        EventRecord {
            at: t + HOUR,
            kind: "window_spent".into(),
            account_id: Some("a".into()),
            detail: Some(
                serde_json::json!({ "window": "seven_day", "source": "poll" }).to_string(),
            ),
            ..Default::default()
        },
    ] {
        db.insert_event(&row).unwrap();
    }
    // `a`'s week charged half the table, under the band's floor.
    reconciled(&db, "a", "seven_day", t + HOUR, 0.1, 0.2, 0.5, 6);
    let tables = phoebis_ledger::test_tables::subscription_costs();
    db.write(|c| {
        let tx = c.transaction()?;
        phoebis_ledger::fold_routing_conditions(&tx, &tables, t + 2 * HOUR)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(phoebis_ledger::routing_conditions)
        .unwrap();
    let wall = rows
        .iter()
        .find(|r| r.kind == "wall_reached" && r.subject_id == "a")
        .unwrap_or_else(|| panic!("no wall on a: {rows:?}"));
    assert_eq!(
        wall.detail["projection_error"],
        serde_json::Value::Null,
        "a projection is read against its own window's wall alone: {wall:?}"
    );
    let drift = rows
        .iter()
        .find(|r| r.kind == "meter_drift" && r.subject_id == "a")
        .unwrap_or_else(|| panic!("no drift on a: {rows:?}"));
    assert_eq!(
        (
            drift.subject_kind.as_str(),
            drift.since,
            drift.count,
            &drift.detail["windows"]
        ),
        (
            "account",
            t + HOUR,
            1,
            &serde_json::json!([{
                "observed_rise": 0.1,
                "predicted_share": 0.2,
                "ratio": 0.5,
                "requests": 6,
                "window": "seven_day",
                "window_end": t + HOUR,
            }])
        ),
        "a week that charged under the band's floor drifts as one over its ceiling does: {drift:?}"
    );
}

/// A slow-down span's start as the router's mirror writes it, with the factor the read opened it at, under a pool whose
/// threshold puts the bar at the floor.
fn slow_down_started(at: i64, account: &str, pool: &str, factor: f64) -> EventRecord {
    slow_down_started_under(at, account, pool, factor, 1_800_000)
}

/// A slow-down span's start under a pool whose stretch threshold is `threshold_ms`.
fn slow_down_started_under(
    at: i64,
    account: &str,
    pool: &str,
    factor: f64,
    threshold_ms: i64,
) -> EventRecord {
    EventRecord {
        at,
        kind: "slow_down_started".into(),
        account_id: Some(account.into()),
        pool_id: Some(pool.into()),
        detail: Some(
            serde_json::json!({
                "factor": factor,
                "stretch_ms": 600_000,
                "threshold_ms": threshold_ms,
                "demand": 0.1,
                "reset": null,
            })
            .to_string(),
        ),
        ..Default::default()
    }
}

/// The end of the span opened at `since`: its row lands at `at`, after the wait that closed it, while the span itself
/// ran `duration_ms`.
fn slow_down_ended(
    at: i64,
    account: &str,
    pool: &str,
    since: i64,
    duration_ms: i64,
    factor_min: f64,
) -> EventRecord {
    EventRecord {
        at,
        kind: "slow_down_ended".into(),
        account_id: Some(account.into()),
        pool_id: Some(pool.into()),
        detail: Some(
            serde_json::json!({
                "since": since,
                "duration_ms": duration_ms,
                "factor_min": factor_min,
                "held_ms": 0,
            })
            .to_string(),
        ),
        ..Default::default()
    }
}

/// The row the ledger writes at each start, naming no account and no pool.
fn router_started(at: i64) -> EventRecord {
    EventRecord {
        at,
        kind: "router_started".into(),
        detail: Some(serde_json::json!({ "version": "test" }).to_string()),
        ..Default::default()
    }
}

/// A start row carrying `down_since`, the previous run's newest row, null where the ledger held none.
fn router_started_after(at: i64, down_since: Option<i64>) -> EventRecord {
    EventRecord {
        at,
        kind: "router_started".into(),
        detail: Some(
            serde_json::json!({ "version": "test", "down_since": down_since }).to_string(),
        ),
        ..Default::default()
    }
}

/// Inserts `rows`, folds the routing conditions at `now` and answers the `slowed_long` rows the fold wrote.
fn slowed_long_at(
    db: &LedgerDb,
    rows: &[EventRecord],
    now: i64,
) -> Vec<phoebis_ledger::RoutingCondition> {
    for row in rows {
        db.insert_event(row).unwrap();
    }
    let tables = phoebis_ledger::test_tables::subscription_costs();
    db.write(|c| {
        let tx = c.transaction()?;
        phoebis_ledger::fold_routing_conditions(&tx, &tables, now)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    db.read_blocking(phoebis_ledger::routing_conditions)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "slowed_long")
        .collect()
}

/// The condition of a slowed account folded at `now`: `spans`, `slowed_ms`, `bar_ms`, `open` and `factor_min`, in that
/// order.
fn slowed_row(
    account: &str,
    pool: &str,
    since: i64,
    now: i64,
    (spans, slowed_ms, bar_ms, open, factor_min): (u64, i64, i64, bool, f64),
) -> phoebis_ledger::RoutingCondition {
    phoebis_ledger::RoutingCondition {
        kind: "slowed_long".into(),
        subject_kind: "account".into(),
        subject_id: account.into(),
        pool_id: Some(pool.into()),
        since,
        count: spans,
        detail: serde_json::json!({
            "bar_ms": bar_ms,
            "factor_min": factor_min,
            "open": open,
            "setting": ["slow_down_stretch_seconds"],
            "slowed_ms": slowed_ms,
            "spans": spans,
        }),
        computed_at: now,
    }
}

#[test]
fn a_slow_down_that_summed_past_its_bar_of_the_day_folds_the_slowed_long_condition() {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // Two spans of 40 minutes closed inside the day, each end landing after the wait that closed it, and a
            // third open since 20 minutes before the fold: 100 minutes, the lowest factor the open span's own.
            slow_down_started(now - 5 * HOUR, "a", "p", 0.6),
            slow_down_ended(
                now - 5 * HOUR + 42 * MINUTE,
                "a",
                "p",
                now - 5 * HOUR,
                40 * MINUTE,
                0.35,
            ),
            slow_down_started(now - 3 * HOUR, "a", "p", 0.7),
            slow_down_ended(
                now - 3 * HOUR + 42 * MINUTE,
                "a",
                "p",
                now - 3 * HOUR,
                40 * MINUTE,
                0.5,
            ),
            slow_down_started(now - 20 * MINUTE, "a", "p", 0.25),
            // `b` slowed for 30 and then 20 minutes, 50 in all.
            slow_down_started(now - 4 * HOUR, "b", "p", 0.5),
            slow_down_ended(
                now - 4 * HOUR + 32 * MINUTE,
                "b",
                "p",
                now - 4 * HOUR,
                30 * MINUTE,
                0.5,
            ),
            slow_down_started(now - 2 * HOUR, "b", "p", 0.5),
            slow_down_ended(
                now - 2 * HOUR + 22 * MINUTE,
                "b",
                "p",
                now - 2 * HOUR,
                20 * MINUTE,
                0.5,
            ),
            // `d` slowed for 35 minutes under `r` and, once moved, 35 under `q`: one account, one row. `r`'s threshold
            // of 150 minutes would put the bar at five hours, `q`'s of 2000 seconds puts it at 4000 seconds, and the
            // newest span's start row is `q`'s.
            slow_down_started_under(now - 6 * HOUR, "d", "r", 0.5, 9_000_000),
            slow_down_ended(
                now - 6 * HOUR + 37 * MINUTE,
                "d",
                "r",
                now - 6 * HOUR,
                35 * MINUTE,
                0.4,
            ),
            slow_down_started_under(now - 2 * HOUR, "d", "q", 0.6, 2_000_000),
            slow_down_ended(
                now - 2 * HOUR + 37 * MINUTE,
                "d",
                "q",
                now - 2 * HOUR,
                35 * MINUTE,
                0.55,
            ),
        ],
        now,
    );
    assert_eq!(
        rows,
        [
            slowed_row(
                "a",
                "p",
                now - 5 * HOUR,
                now,
                (3, 6_000_000, 3_600_000, true, 0.25)
            ),
            slowed_row(
                "d",
                "q",
                now - 6 * HOUR,
                now,
                (2, 70 * MINUTE, 4_000_000, false, 0.4)
            ),
        ],
        "an account's spans sum across its pools past the bar of the newest span's start row, dated by the oldest \
         and named by the newest span's pool, and 50 minutes raise nothing"
    );
    // The open span ends 22 minutes after it opened, and a day later the trailing 24 hours hold none of the spans.
    assert_eq!(
        slowed_long_at(
            &db,
            &[slow_down_ended(
                now + 4 * MINUTE,
                "a",
                "p",
                now - 20 * MINUTE,
                22 * MINUTE,
                0.25,
            )],
            now + 25 * HOUR,
        ),
        Vec::new(),
        "the condition clears once the day holds no span"
    );
}

#[test]
fn a_held_accounts_zero_part_span_second_start_and_orphan_end_join_neither_its_spans_nor_its_since()
{
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    let now = 1_000 * HOUR;
    let cutoff = now - 24 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // A span standing at the cutoff whose end row lands inside the day while the span closed two minutes
            // before it, so no part of it falls in the day.
            slow_down_started(cutoff - HOUR, "a", "p", 0.1),
            slow_down_ended(cutoff + MINUTE, "a", "p", cutoff - HOUR, 58 * MINUTE, 0.1),
            // An end whose start was lost.
            slow_down_ended(now - 6 * HOUR, "a", "p", now - 7 * HOUR, 30 * MINUTE, 0.05),
            // A span open for five hours, whose second start lowers its factor and opens nothing.
            slow_down_started(now - 5 * HOUR, "a", "p", 0.6),
            slow_down_started(now - 4 * HOUR, "a", "p", 0.3),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 5 * HOUR,
            now,
            (1, 5 * HOUR, HOUR, true, 0.3)
        )],
        "one span counted, dated by its own start, at the second start's factor"
    );
}

#[test]
fn the_slowed_long_fold_closes_an_open_span_at_the_next_router_started_row() {
    const HOUR: i64 = 3_600_000;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // The restart lost the memory-only state, so no end follows the start.
            slow_down_started(now - 3 * HOUR, "a", "p", 0.4),
            router_started(now - HOUR),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 3 * HOUR,
            now,
            (1, 7_200_000, 3_600_000, false, 0.4)
        )],
        "the span runs the two hours to the restart, where reading it open to now would count three"
    );
}

#[test]
fn the_slowed_long_fold_ends_a_span_a_restart_closed_at_the_start_rows_down_since() {
    const HOUR: i64 = 3_600_000;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // The previous run wrote its newest row an hour before the restart, and its slow-down held no later.
            slow_down_started(now - 4 * HOUR, "a", "p", 0.4),
            router_started_after(now - HOUR, Some(now - 2 * HOUR)),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 4 * HOUR,
            now,
            (1, 2 * HOUR, HOUR, false, 0.4)
        )],
        "the span runs the two hours to down_since and leaves out the hour the router was down"
    );
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            slow_down_started(now - 4 * HOUR, "a", "p", 0.4),
            router_started_after(now - HOUR, None),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 4 * HOUR,
            now,
            (1, 3 * HOUR, HOUR, false, 0.4)
        )],
        "a start row without down_since keeps the restart as the span's end"
    );
}

#[test]
fn the_slowed_long_fold_counts_no_time_for_a_span_whose_restart_down_since_is_its_start() {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // A span of two hours closed earlier in the day.
            slow_down_started(now - 10 * HOUR, "a", "p", 0.5),
            slow_down_ended(
                now - 8 * HOUR + 2 * MINUTE,
                "a",
                "p",
                now - 10 * HOUR,
                2 * HOUR,
                0.5,
            ),
            // The previous run stopped right after this start row, its newest, so the span held for no time.
            slow_down_started(now - 4 * HOUR, "a", "p", 0.4),
            router_started_after(now - HOUR, Some(now - 4 * HOUR)),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 10 * HOUR,
            now,
            (1, 2 * HOUR, HOUR, false, 0.5)
        )],
        "the closed span alone counts, where reading the other to the restart would add the three hours of downtime"
    );
}

#[test]
fn the_slowed_long_fold_counts_an_open_span_from_before_the_cutoff_over_the_whole_day() {
    const HOUR: i64 = 3_600_000;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // A restart older than the start closes nothing of it.
            router_started(now - 40 * HOUR),
            slow_down_started_under(now - 30 * HOUR, "a", "p", 0.5, 9_000_000),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 30 * HOUR,
            now,
            (1, 86_400_000, 18_000_000, true, 0.5)
        )],
        "the newest row before the cutoff is the start, so the span opens at the cutoff, is dated by its own start and \
         is held to its start row's bar"
    );
}

#[test]
fn the_slowed_long_fold_clamps_a_closed_span_crossing_the_cutoff_to_the_trailing_day() {
    const HOUR: i64 = 3_600_000;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            slow_down_started_under(now - 25 * HOUR, "a", "p", 0.6, 2_400_000),
            slow_down_ended(now - 22 * HOUR, "a", "p", now - 25 * HOUR, 9_000_000, 0.45),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "a",
            "p",
            now - 25 * HOUR,
            now,
            (1, 5_400_000, 4_800_000, false, 0.45)
        )],
        "the two and a half hours count only the hour and a half after the cutoff, past the bar of twice the start \
         row's 40 minutes"
    );
}

#[test]
fn the_slowed_long_fold_counts_nothing_of_a_span_closed_before_the_cutoff() {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    let now = 1_000 * HOUR;
    let cutoff = now - 24 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // A span from 30 to 26 hours back, both of its rows before the cutoff.
            slow_down_started(now - 30 * HOUR, "a", "p", 0.5),
            slow_down_ended(now - 26 * HOUR, "a", "p", now - 30 * HOUR, 4 * HOUR, 0.5),
            // A span whose end row lands a minute after the cutoff, while the span closed two minutes before it.
            slow_down_started(cutoff - HOUR, "a", "p", 0.5),
            slow_down_ended(cutoff + MINUTE, "a", "p", cutoff - HOUR, 58 * MINUTE, 0.5),
            // 50 minutes inside the day, the only time of `a`'s the day holds.
            slow_down_started(now - 5 * HOUR, "a", "p", 0.5),
            slow_down_ended(
                now - 5 * HOUR + 52 * MINUTE,
                "a",
                "p",
                now - 5 * HOUR,
                50 * MINUTE,
                0.5,
            ),
            // `c` slowed for 61 minutes inside the day.
            slow_down_started(now - 3 * HOUR, "c", "p", 0.6),
            slow_down_ended(
                now - 3 * HOUR + 63 * MINUTE,
                "c",
                "p",
                now - 3 * HOUR,
                61 * MINUTE,
                0.55,
            ),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "c",
            "p",
            now - 3 * HOUR,
            now,
            (1, 61 * MINUTE, 3_600_000, false, 0.55)
        )],
        "counting either of a's old spans would pass the hour"
    );
}

#[test]
fn a_pool_at_the_default_holds_slowed_long_past_five_hours() {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const THRESHOLD_MS: i64 = 9_000_000;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // `a` slowed for 170 and then 120 minutes, 4 h 50 min in all.
            slow_down_started_under(now - 12 * HOUR, "a", "p", 0.5, THRESHOLD_MS),
            slow_down_ended(
                now - 12 * HOUR + 172 * MINUTE,
                "a",
                "p",
                now - 12 * HOUR,
                170 * MINUTE,
                0.4,
            ),
            slow_down_started_under(now - 6 * HOUR, "a", "p", 0.5, THRESHOLD_MS),
            slow_down_ended(
                now - 6 * HOUR + 122 * MINUTE,
                "a",
                "p",
                now - 6 * HOUR,
                120 * MINUTE,
                0.45,
            ),
            // `b` slowed for 180 minutes and has stood slowed for the last 130, 5 h 10 min in all.
            slow_down_started_under(now - 10 * HOUR, "b", "p", 0.6, THRESHOLD_MS),
            slow_down_ended(
                now - 10 * HOUR + 182 * MINUTE,
                "b",
                "p",
                now - 10 * HOUR,
                180 * MINUTE,
                0.5,
            ),
            slow_down_started_under(now - 130 * MINUTE, "b", "p", 0.3, THRESHOLD_MS),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "b",
            "p",
            now - 10 * HOUR,
            now,
            (2, 310 * MINUTE, 18_000_000, true, 0.3)
        )],
        "twice a threshold of two and a half hours puts the bar at five hours, so 4 h 50 min raise nothing"
    );
}

#[test]
fn a_pool_under_half_an_hour_holds_slowed_long_past_the_floor() {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const THRESHOLD_MS: i64 = 60_000;
    let now = 1_000 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    let rows = slowed_long_at(
        &db,
        &[
            // `a` slowed for 55 minutes, many times twice the threshold.
            slow_down_started_under(now - 4 * HOUR, "a", "p", 0.5, THRESHOLD_MS),
            slow_down_ended(
                now - 4 * HOUR + 57 * MINUTE,
                "a",
                "p",
                now - 4 * HOUR,
                55 * MINUTE,
                0.5,
            ),
            // `b` slowed for 40 and then 25 minutes, 1 h 5 min in all.
            slow_down_started_under(now - 5 * HOUR, "b", "p", 0.7, THRESHOLD_MS),
            slow_down_ended(
                now - 5 * HOUR + 42 * MINUTE,
                "b",
                "p",
                now - 5 * HOUR,
                40 * MINUTE,
                0.6,
            ),
            slow_down_started_under(now - 2 * HOUR, "b", "p", 0.7, THRESHOLD_MS),
            slow_down_ended(
                now - 2 * HOUR + 27 * MINUTE,
                "b",
                "p",
                now - 2 * HOUR,
                25 * MINUTE,
                0.65,
            ),
        ],
        now,
    );
    assert_eq!(
        rows,
        [slowed_row(
            "b",
            "p",
            now - 5 * HOUR,
            now,
            (2, 65 * MINUTE, 3_600_000, false, 0.6)
        )],
        "twice a threshold of one minute falls under the floor, so the bar is the hour and 55 minutes raise nothing"
    );
}

#[test]
fn two_agents_pin_created_rows_differ_in_their_detail() {
    let pin_created = |agent_id: &str| RouterEvent::PinCreated {
        key_id: "k".into(),
        session_id: "s".into(),
        agent_id: agent_id.into(),
        model: "m".into(),
        account_id: "a".into(),
        epoch: 1,
        at: 1,
    };
    let (main_row, sibling_row) = (
        event_of(&pin_created("main")).expect("a pin row"),
        event_of(&pin_created("worker")).expect("a pin row"),
    );
    assert_ne!(main_row.detail, sibling_row.detail);
    let main_detail: serde_json::Value =
        serde_json::from_str(main_row.detail.as_deref().unwrap()).unwrap();
    let sibling_detail: serde_json::Value =
        serde_json::from_str(sibling_row.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        (
            main_detail["agent_id"].as_str(),
            sibling_detail["agent_id"].as_str()
        ),
        (Some("main"), Some("worker"))
    );
}

#[test]
fn every_event_kind_the_router_emits_is_listed_once_in_the_kinds_alphabetically() {
    use phoebis_ledger::EVENT_KINDS;
    use phoebis_routing::{CacheMapEntry, CreditsState, PinKeyFacts, ShedRoundFacts, WindowSource};
    const KINDS: [&str; 32] = [
        "account_state_changed",
        "budget_period_ended",
        "budget_reached",
        "catalog_changed",
        "cooldown_cleared",
        "cooldown_set",
        "fast_credits_changed",
        "ledger_gap",
        "mark_cleared",
        "model_permitted",
        "model_rejected",
        "overage_ended",
        "overage_started",
        "pin_created",
        "pin_expired",
        "pin_migrated",
        "pin_released",
        "poll_backoff_ended",
        "poll_backoff_started",
        "refresh_failed",
        "refresh_succeeded",
        "refresh_uncertain",
        "registry_changed",
        "reservation_expired",
        "router_started",
        "shedding_round",
        "slow_down_ended",
        "slow_down_started",
        "warm_prefix_expired",
        "wind_down_started",
        "window_reset",
        "window_spent",
    ];
    assert_eq!(EVENT_KINDS, KINDS);
    assert!(
        EVENT_KINDS.windows(2).all(|pair| pair[0] < pair[1]),
        "alphabetical, each kind once"
    );
    let pin = |key: &str| PinKeyFacts {
        key_id: key.into(),
        session_id: "s".into(),
        agent_id: "main".into(),
        model: "m".into(),
    };
    let credits = CreditsState {
        enabled: true,
        disabled_reason: None,
        utilization: None,
        observed_at: 1,
        incarnation: 1,
    };
    // Every variant the router broadcasts but a decision, whose record the routing log reads as a decision row.
    let events = [
        RouterEvent::AccountStateChanged {
            account_id: "a".into(),
            to: AccountState::Ready,
            incarnation: 1,
            revision: 1,
            cause: StateCause::Operator,
            detail: None,
            at: 1,
        },
        RouterEvent::CacheMapWritten {
            request_id: "r".into(),
            seq: 1,
            attempt: 1,
            entries: Vec::new(),
            at: 1,
        },
        RouterEvent::CooldownCleared {
            account_id: "a".into(),
            scope: "account".into(),
            cause: "expired",
            at: 1,
        },
        RouterEvent::CooldownSet {
            account_id: "a".into(),
            scope: "account".into(),
            until: 2,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            at: 1,
        },
        RouterEvent::FastCreditsChanged {
            account_id: "a".into(),
            before: None,
            after: credits,
            blocked_before: None,
            blocked_after: None,
            at: 1,
        },
        RouterEvent::MarkCleared {
            account_id: "a".into(),
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            reason: "moved",
            marked_at: 1,
            marked_on: "a".into(),
            marked_window: "five_hour".into(),
            projection: None,
            stretch_ms: None,
            threshold_ms: None,
            at: 1,
        },
        RouterEvent::OverageEnded {
            account_id: "a".into(),
            since: None,
            at: 1,
        },
        RouterEvent::OverageStarted {
            account_id: "a".into(),
            at: 1,
        },
        RouterEvent::PinCreated {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            account_id: "a".into(),
            epoch: 1,
            at: 1,
        },
        RouterEvent::PinExpired {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            account_id: "a".into(),
            reasons: Vec::new(),
            at: 1,
        },
        RouterEvent::PinMigrated {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            from_account: "a".into(),
            to_account: "b".into(),
            epoch: 2,
            reason: "spent".into(),
            fit: Some("fits"),
            runway_ms: None,
            binding: None,
            cause: None,
            at: 1,
        },
        RouterEvent::PinReleased {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            account_id: "a".into(),
            at: 1,
        },
        RouterEvent::ReservationExpired {
            account_id: "a".into(),
            request_id: "r".into(),
            attempt: 1,
            cause: "timeout",
            shares: Default::default(),
            age_ms: 1,
            at: 1,
        },
        RouterEvent::SheddingRound {
            account_id: "a".into(),
            pool_id: "p".into(),
            request_id: "r".into(),
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            round: Box::new(ShedRoundFacts {
                window: "five_hour".into(),
                utilization: 0.5,
                projection: 0.97,
                horizon: 2,
                ceiling: 0.95,
                trigger: "slow_down",
                stretch_ms: Some(2_400_000),
                threshold_ms: Some(1_800_000),
                factor: Some(0.4),
                unit: "window_share",
                burn_average_seconds: 1_800,
                shed_cap: 0.5,
                burn_sum: 0.1,
                excess: 0.01,
                target: 0.01,
                removed: 0.02,
                warm_sessions: "when_needed",
                pins: Vec::new(),
                marks: vec![pin("k")],
                outcome: "marked 1".into(),
                reason: None,
                dwell_seconds: 900,
                dwell_until: 900_001,
            }),
            at: 1,
        },
        RouterEvent::SlowDownStarted {
            account_id: "a".into(),
            pool_id: "p".into(),
            factor: 0.5,
            stretch_ms: 2_520_000,
            threshold_ms: 1_800_000,
            demand: 0.12,
            reset: None,
            at: 1,
        },
        RouterEvent::SlowDownEnded {
            account_id: "a".into(),
            pool_id: "p".into(),
            since: 0,
            duration_ms: 1,
            factor_min: 0.5,
            held_ms: 0,
            at: 1,
        },
        RouterEvent::WarmPrefixExpired {
            account_id: "a".into(),
            key_id: "k".into(),
            root: "s".into(),
            model: "m".into(),
            lane: "m|standard|none|-".into(),
            entries: 1,
            deepest: CacheMapEntry {
                tier: "message",
                depth: 1,
                prefix_tokens: 1_000,
                ttl_ms: 300_000,
                account_id: "a".into(),
            },
            cause: "ttl",
            at: 1,
        },
        RouterEvent::WindDownStarted {
            account_id: "a".into(),
            pool_id: "p".into(),
            ends_at: 2,
            wind_down_minutes: 30,
            end_source: "ends_at",
            restored: false,
            at: 1,
        },
        RouterEvent::WindowReset {
            account_id: "a".into(),
            window: "five_hour".into(),
            utilization_before: 0.5,
            reset_at: 1,
            next_reset: None,
            observed: "clock",
            restored: false,
            at: 1,
        },
        RouterEvent::WindowSpent {
            account_id: "a".into(),
            window: "five_hour".into(),
            utilization: 1.0,
            reset: None,
            source: WindowSource::Predicted,
            source_request_id: Some("r".into()),
            at: 1,
        },
    ];
    let answered: Vec<String> = events
        .iter()
        .filter_map(event_of)
        .map(|row| row.kind)
        .collect();
    for kind in &answered {
        assert!(
            EVENT_KINDS.contains(&kind.as_str()),
            "{kind} is written but not listed"
        );
    }
    for kind in [
        "mark_cleared",
        "reservation_expired",
        "slow_down_ended",
        "slow_down_started",
    ] {
        assert!(
            answered.iter().any(|answered| answered == kind),
            "{kind} has no row: {answered:?}"
        );
    }
}

/// The account rows the mirror wrote, in order: each kind with its detail.
async fn account_rows(h: &Harness) -> Vec<(String, serde_json::Value)> {
    let rows = h.db.events(0, i64::MAX, 100).await.unwrap();
    assert!(
        rows.iter().all(|e| e.kind != "reauth_required"),
        "no event row carries reauth_required, which the mirror folds into account_state_changed"
    );
    rows.into_iter()
        .filter(|e| e.kind == "account_state_changed")
        .map(|e| {
            assert_eq!(e.account_id.as_deref(), Some("a"));
            let detail = e
                .detail
                .as_deref()
                .map(|d| serde_json::from_str(d).unwrap())
                .unwrap_or(serde_json::Value::Null);
            (e.kind, detail)
        })
        .collect()
}

fn transition(
    from: &str,
    to: &str,
    cause: &str,
    detail: Option<&str>,
) -> (String, serde_json::Value) {
    (
        "account_state_changed".to_string(),
        serde_json::json!({ "from": from, "to": to, "cause": cause, "detail": detail }),
    )
}

fn polled_every_50_ms() -> LedgerConfig {
    LedgerConfig {
        event_poll: Duration::from_millis(50),
        ..LedgerConfig::default()
    }
}

#[tokio::test]
async fn every_account_transition_writes_one_row_a_move_into_reauth_required_included() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    h.store
        .set_state("a", AccountState::FundingRequired, StateCause::Operator)
        .unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await.len() == 1).then_some(())
    })
    .await;
    h.store
        .set_state("a", AccountState::ReauthRequired, StateCause::Operator)
        .unwrap();
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = account_rows(&h).await;
        (rows.len() == 2).then_some(rows)
    })
    .await;
    assert_eq!(
        rows,
        [
            transition("ready", "funding_required", "operator", None),
            transition("funding_required", "reauth_required", "operator", None),
        ]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_routing_loop_s_cause_lands_on_the_transition_it_names() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    let row = h.store.account("a").unwrap().unwrap();
    // Sent ahead of the write it describes, with the identity the write bumps the row to, so no tick can read the
    // row before the event names its cause.
    h.router.emit(RouterEvent::AccountStateChanged {
        account_id: "a".into(),
        to: AccountState::ReauthRequired,
        incarnation: row.incarnation,
        revision: row.revision + 1,
        cause: StateCause::AuthFailed,
        detail: Some("invalid x-api-key".into()),
        at: NOW,
    });
    let expected = [transition(
        "ready",
        "reauth_required",
        "auth_failed",
        Some("invalid x-api-key"),
    )];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == expected).then_some(())
    })
    .await;
    // The ticks before the write read the row the event already moved past: no move back to `ready`.
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == expected
    })
    .await;
    h.store
        .set_state("a", AccountState::ReauthRequired, StateCause::AuthFailed)
        .unwrap();
    assert_eq!(
        h.store.account("a").unwrap().unwrap().revision,
        row.revision + 1
    );
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == expected
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_newer_watch_observation_drops_a_delayed_older_transition() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    let row = h.store.account("a").unwrap().unwrap();
    // Both writes land before the mirror runs again, so one tick reads the second.
    h.store
        .set_state("a", AccountState::FundingRequired, StateCause::Operator)
        .unwrap();
    h.store
        .set_state("a", AccountState::ReauthRequired, StateCause::Operator)
        .unwrap();
    let expected = [transition("ready", "reauth_required", "operator", None)];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == expected).then_some(())
    })
    .await;
    for (to, revision, cause) in [
        (
            AccountState::FundingRequired,
            row.revision + 1,
            StateCause::FundingRefused,
        ),
        (
            AccountState::ReauthRequired,
            row.revision + 2,
            StateCause::AuthFailed,
        ),
    ] {
        h.router.emit(RouterEvent::AccountStateChanged {
            account_id: "a".into(),
            to,
            incarnation: row.incarnation,
            revision,
            cause,
            detail: None,
            at: NOW,
        });
    }
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == expected
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn the_tick_writes_the_word_the_writer_stored() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    let rows_after = |n: usize| {
        let h = &h;
        wait_for_async(Duration::from_secs(5), move || async move {
            let rows = account_rows(h).await;
            (rows.len() == n).then_some(rows)
        })
    };
    h.store
        .set_state("a", AccountState::FundingRequired, StateCause::Operator)
        .unwrap();
    rows_after(1).await;
    h.store.bump_lifecycle("a", StateCause::Operator).unwrap();
    rows_after(2).await;
    h.store
        .set_state("a", AccountState::Ready, StateCause::Paired)
        .unwrap();
    assert_eq!(
        rows_after(3).await,
        [
            transition("ready", "funding_required", "operator", None),
            transition("funding_required", "disconnected", "operator", None),
            transition("disconnected", "ready", "paired", None),
        ]
    );
    up.shutdown().await;
}

/// The account `a` stored again under its id, past the tombstone, already marked for re-authentication.
fn replaced_and_marked(store: &Store) {
    store
        .insert_account(&phoebis_store::AccountRow {
            id: "a".into(),
            kind: phoebis_store::AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::ReauthRequired,
            state_cause: StateCause::RefreshFailed,
        })
        .unwrap();
}

#[tokio::test]
async fn a_deleted_account_s_last_row_reads_deleted() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    h.store.delete_account("a", "test").unwrap();
    let last = [transition("ready", "deleted", "deleted", None)];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == last).then_some(())
    })
    .await;
    let deletion: Vec<serde_json::Value> =
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "registry_changed")
            .map(|e| serde_json::from_str(e.detail.as_deref().unwrap()).unwrap())
            .filter(|detail: &serde_json::Value| detail["kind"] == "account_deleted")
            .collect();
    assert_eq!(
        deletion
            .iter()
            .map(|detail| (detail["target"].as_str(), detail["diff"].clone()))
            .collect::<Vec<_>>(),
        [(Some("a"), serde_json::json!({}))],
        "the registry's own row of the deletion stands beside the account's last row"
    );
    // The next tick finds no held account the rows lack: the row is written once.
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == last
    })
    .await;
    replaced_and_marked(&h.store);
    let expected = [
        last[0].clone(),
        transition("unpaired", "reauth_required", "refresh_failed", None),
    ];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == expected).then_some(())
    })
    .await;
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == expected
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn an_event_about_a_holder_the_tick_found_deleted_writes_nothing() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    let row = h.store.account("a").unwrap().unwrap();
    h.store.delete_account("a", "test").unwrap();
    let last = [transition("ready", "deleted", "deleted", None)];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == last).then_some(())
    })
    .await;
    // A mark the routing loop landed before the deletion, whose event reaches the mirror after the tick that wrote
    // the account's last row.
    h.router.emit(RouterEvent::AccountStateChanged {
        account_id: "a".into(),
        to: AccountState::ReauthRequired,
        incarnation: row.incarnation,
        revision: row.revision + 1,
        cause: StateCause::AuthFailed,
        detail: None,
        at: NOW,
    });
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == last
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn an_account_replaced_under_its_id_between_two_ticks_writes_the_old_holder_s_last_row() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    h.store.delete_account("a", "test").unwrap();
    replaced_and_marked(&h.store);
    // Whether a tick reads the gap between the two writes or not, the held holder is gone and the new one is one the
    // mirror never saw.
    let expected = [
        transition("ready", "deleted", "deleted", None),
        transition("unpaired", "reauth_required", "refresh_failed", None),
    ];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == expected).then_some(())
    })
    .await;
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == expected
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_row_born_ready_by_a_pairing_records_its_transition_from_unpaired() {
    let up = FakeUpstream::start(vec![]).await;
    // `b` is the seeded holder: `a` reaches the store after the mirror's spawn, so it is a holder the mirror never saw.
    let h = harness(&up, &[oauth("b", B, 0)], (0, None), polled_every_50_ms()).await;
    h.store
        .insert_account(&phoebis_store::AccountRow {
            id: "a".into(),
            kind: phoebis_store::AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Paired,
        })
        .unwrap();
    let expected = [transition("unpaired", "ready", "paired", None)];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == expected).then_some(())
    })
    .await;
    stays_for_async(Duration::from_millis(200), || async {
        account_rows(&h).await == expected
    })
    .await;
    up.shutdown().await;
}

/// A harness over a ledger whose newest row, the previous run's last, is at `down_since`.
async fn harness_after_a_run(up: &FakeUpstream, down_since: i64) -> Harness {
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_event(&EventRecord {
        at: down_since,
        kind: "pin_created".into(),
        account_id: Some("a".into()),
        ..Default::default()
    })
    .unwrap();
    harness_on(
        Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap()),
        db,
        up,
        &[oauth("a", A, 0)],
        (0, None),
        polled_every_50_ms(),
        false,
    )
    .await
}

#[tokio::test]
async fn a_clock_reset_the_previous_run_announced_is_not_written_again() {
    let up = FakeUpstream::start(vec![]).await;
    let down_since = NOW - 60_000;
    let h = harness_after_a_run(&up, down_since).await;
    let reset = |reset_at: i64, observed: &'static str| RouterEvent::WindowReset {
        account_id: "a".into(),
        window: "five_hour".into(),
        utilization_before: 0.9,
        reset_at,
        next_reset: None,
        observed,
        restored: observed == "clock",
        at: NOW,
    };
    // The first purge after a start walks every restored reading: a reset that passed before the previous run's
    // last row was that run's to announce, one that passed while the router was down is this run's.
    h.router.emit(reset(down_since - 1, "clock"));
    h.router.emit(reset(down_since - 1, "sample"));
    h.router.emit(reset(down_since + 1, "clock"));
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows: Vec<(i64, String)> =
            h.db.events(0, i64::MAX, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "window_reset")
                .map(|e| {
                    let detail: serde_json::Value =
                        serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
                    (
                        detail["reset_at"].as_i64().unwrap(),
                        detail["observed"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
        rows.iter()
            .any(|(reset_at, _)| *reset_at == down_since + 1)
            .then_some(rows)
    })
    .await;
    assert_eq!(
        rows,
        [
            (down_since - 1, "sample".to_string()),
            (down_since + 1, "clock".to_string())
        ],
        "a sample is a reading of this run, whatever the reset it names"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_wind_down_the_previous_run_announced_is_not_written_again() {
    let up = FakeUpstream::start(vec![]).await;
    let down_since = NOW - 60_000;
    let h = harness_after_a_run(&up, down_since).await;
    let wind_down = |opened_at: i64| RouterEvent::WindDownStarted {
        account_id: "a".into(),
        pool_id: "main".into(),
        ends_at: opened_at + 30 * 60_000,
        wind_down_minutes: 30,
        end_source: "cancelled",
        restored: true,
        at: NOW,
    };
    h.router.emit(wind_down(down_since - 1));
    h.router.emit(wind_down(down_since + 1));
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows: Vec<i64> =
            h.db.events(0, i64::MAX, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "wind_down_started")
                .map(|e| {
                    let detail: serde_json::Value =
                        serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
                    detail["ends_at"].as_i64().unwrap()
                })
                .collect();
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        rows,
        [down_since + 1 + 30 * 60_000],
        "a window that opened before the previous run's last row was that run's to announce"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_wind_down_a_cancellation_after_the_start_opens_lands_whenever_its_window_opened() {
    let up = FakeUpstream::start(vec![]).await;
    let down_since = NOW - 60_000;
    let h = harness_after_a_run(&up, down_since).await;
    // A reload runs a purge, so the start's own walk over the restored state is behind the cancellation.
    h.router.reload().await.unwrap();
    // Ten minutes out under the pool's thirty-minute wind-down: the window opened twenty minutes ago, before the
    // previous run's last row, and no run announced it.
    let ends_at = NOW + 10 * 60_000;
    h.store.set_cancelled("a", true, Some(ends_at)).unwrap();
    h.router.reload().await.unwrap();
    let wind_downs = || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "wind_down_started")
            .map(|e| {
                let detail: serde_json::Value =
                    serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
                detail["ends_at"].as_i64().unwrap()
            })
            .collect::<Vec<_>>()
    };
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = wind_downs().await;
        (!rows.is_empty()).then_some(rows)
    })
    .await;
    assert_eq!(
        rows,
        [ends_at],
        "the cancellation's wind-down is this run's"
    );
    h.router.reload().await.unwrap();
    stays_for_async(Duration::from_millis(200), || async {
        wind_downs().await.len() == 1
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_clock_reset_that_passed_while_the_router_was_down_lands_once_at_the_start() {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .insert_account(&phoebis_store::AccountRow {
            id: "a".into(),
            kind: phoebis_store::AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Operator,
        })
        .unwrap();
    let down_since = NOW - 60_000;
    let reset_at = NOW - 30_000;
    assert!(store
        .upsert_window(&phoebis_store::WindowRow {
            account_id: "a".into(),
            incarnation: 1,
            observed_at: down_since - 1_000,
            reset: Some(reset_at),
            utilization: 0.9,
            window: "five_hour".into(),
        })
        .unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_event(&EventRecord {
        at: down_since,
        kind: "pin_created".into(),
        account_id: Some("a".into()),
        ..Default::default()
    })
    .unwrap();
    // The router's start: the loop runs while the first purge waits for the ledger and the start row.
    let clock: phoebis_oauth::Clock = Arc::new(|| NOW);
    let router = phoebis_routing::spawn(
        store.clone(),
        clock.clone(),
        phoebis_routing::RouterConfig {
            hold_first_purge: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut early = router.subscribe_events();
    // A round trip through the loop: it turns, and its purge tick waits.
    router.pins().await.unwrap();
    stays_for(Duration::from_millis(200), || early.try_recv().is_err()).await;
    let _ledger = phoebis_ledger::spawn(
        db.clone(),
        Arc::new(phoebis_ledger::Pricing::new(
            phoebis_ledger::PriceRevisions::shipped(),
        )),
        Arc::new(phoebis_ledger::PlanTables::shipped()),
        store,
        &phoebis_bus::Bus::new(),
        router.clone(),
        clock,
        Arc::new(phoebis_ledger::Metrics::new()),
        LedgerConfig {
            start_version: Some("0.1.0".into()),
            ..polled_every_50_ms()
        },
    );
    router.release_purge().await;
    let rows = || async {
        db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.detail))
            .collect::<Vec<_>>()
    };
    let landed = wait_for_async(Duration::from_secs(5), || async {
        let rows = rows().await;
        rows.iter()
            .any(|(kind, _)| kind == "window_reset")
            .then_some(rows)
    })
    .await;
    assert_eq!(
        landed
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        ["pin_created", "router_started", "window_reset"],
        "the downtime's reset lands behind the start row"
    );
    let start: serde_json::Value = serde_json::from_str(landed[1].1.as_deref().unwrap()).unwrap();
    assert_eq!(start["down_since"], serde_json::json!(down_since));
    stays_for_async(Duration::from_millis(200), || async {
        rows().await.len() == 3
    })
    .await;
}

#[tokio::test]
async fn the_start_row_dates_the_gap_from_the_previous_run_whatever_the_mirror_finds_at_spawn() {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .insert_account(&phoebis_store::AccountRow {
            id: "a".into(),
            kind: phoebis_store::AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Operator,
        })
        .unwrap();
    let down_since = NOW - 60_000;
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    db.insert_event(&EventRecord {
        at: down_since,
        kind: "pin_created".into(),
        account_id: Some("a".into()),
        ..Default::default()
    })
    .unwrap();
    let clock: phoebis_oauth::Clock = Arc::new(|| NOW);
    let router = phoebis_routing::spawn(store.clone(), clock.clone(), Default::default()).unwrap();
    let _ledger = phoebis_ledger::spawn(
        db.clone(),
        Arc::new(phoebis_ledger::Pricing::new(
            phoebis_ledger::PriceRevisions::shipped(),
        )),
        Arc::new(phoebis_ledger::PlanTables::shipped()),
        store.clone(),
        &phoebis_bus::Bus::new(),
        router.clone(),
        clock,
        Arc::new(phoebis_ledger::Metrics::new()),
        LedgerConfig {
            start_version: Some("0.1.0".into()),
            ..polled_every_50_ms()
        },
    );
    // A refresh this run settled past the mirror's watermark, which its first tick lands.
    let attempt = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Failed("quarantined at start".into()),
        )
        .unwrap();
    let landed = wait_for_async(Duration::from_secs(5), || async {
        let rows: Vec<(String, Option<String>)> = db
            .events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.detail))
            .collect();
        (rows.len() == 3).then_some(rows)
    })
    .await;
    // By row id rather than by `at`: the refresh row's instant is the store's wall clock, which sorts after `NOW`
    // whichever row lands first.
    let order: Vec<String> = db
        .read_blocking(|c| {
            let mut stmt = c.prepare("SELECT kind FROM events ORDER BY rowid")?;
            let kinds = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(kinds)
        })
        .unwrap();
    assert_eq!(
        order,
        ["pin_created", "router_started", "refresh_failed"],
        "the mirror's row lands behind the start row"
    );
    let start: serde_json::Value = serde_json::from_str(landed[1].1.as_deref().unwrap()).unwrap();
    assert_eq!(
        start["down_since"],
        serde_json::json!(down_since),
        "the gap ends at the previous run's last row"
    );
}

#[tokio::test]
async fn the_start_fold_waits_for_the_start_row_so_a_span_the_previous_run_left_open_closes_at_it()
{
    const HOUR: i64 = 3_600_000;
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
    // The previous run's last row opened a span on a three hours ago, so the run stopped with the span open, and the
    // span held for no time.
    db.insert_event(&slow_down_started(NOW - 3 * HOUR, "a", "p", 0.4))
        .unwrap();
    let clock: phoebis_oauth::Clock = Arc::new(|| NOW);
    let router = phoebis_routing::spawn(store.clone(), clock.clone(), Default::default()).unwrap();
    let ledger = phoebis_ledger::spawn(
        db.clone(),
        Arc::new(phoebis_ledger::Pricing::new(
            phoebis_ledger::PriceRevisions::shipped(),
        )),
        Arc::new(phoebis_ledger::PlanTables::shipped()),
        store,
        &phoebis_bus::Bus::new(),
        router,
        clock,
        Arc::new(phoebis_ledger::Metrics::new()),
        LedgerConfig {
            start_version: Some("0.1.0".into()),
            ..polled_every_50_ms()
        },
    );
    // A read held on the reader connection before the ledger's tasks first run keeps the writer at the start's spend
    // read, short of the start row's commit, while the purge connection stays free for the fold.
    let (locked, on_lock) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let reader = std::thread::spawn({
        let db = db.clone();
        move || {
            db.read_blocking(|_| {
                locked.send(()).unwrap();
                // A case that fails first drops the sender, which ends the hold all the same.
                let _ = released.recv();
                Ok(())
            })
            .unwrap()
        }
    });
    on_lock.recv().unwrap();
    let folds = || {
        ledger
            .stats()
            .condition_folds
            .load(std::sync::atomic::Ordering::SeqCst)
    };
    stays_for(Duration::from_millis(300), || folds() == 0).await;
    release.send(()).unwrap();
    reader.join().unwrap();
    wait_for(Duration::from_secs(5), || (folds() >= 1).then_some(())).await;
    let rows: Vec<_> = db
        .read_blocking(phoebis_ledger::routing_conditions)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "slowed_long")
        .collect();
    assert_eq!(
        rows,
        Vec::new(),
        "the start's fold reads the span closed at the start row's down_since, its own start, where reading it open \
         would count the three hours to now"
    );
}

/// The router reads the mirror's baseline before the start's recovery, so the refresh the recovery settles and the
/// quarantine it lands are mirrored as changes rather than taken as history.
#[tokio::test]
async fn a_quarantine_the_start_lands_after_the_mirrors_baseline_is_mirrored() {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let account = |id: &str, cause: StateCause| phoebis_store::AccountRow {
        id: id.into(),
        kind: phoebis_store::AccountKind::Oauth,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Ready,
        state_cause: cause,
    };
    store
        .insert_account(&account("a", StateCause::Operator))
        .unwrap();
    let attempt = store.open_refresh_attempt("a", 1, 1, 1).unwrap();
    let baseline = phoebis_ledger::MirrorBaseline::read(&store);
    // The recovery's own writes on a refresh the previous run left in flight.
    store
        .set_state("a", AccountState::ReauthRequired, StateCause::RefreshFailed)
        .unwrap();
    store
        .resolve_refresh_attempt(
            attempt,
            1,
            phoebis_store::AttemptOutcome::Failed("quarantined at start".into()),
        )
        .unwrap();
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let clock: phoebis_oauth::Clock = Arc::new(|| NOW);
    let router = phoebis_routing::spawn(store.clone(), clock.clone(), Default::default()).unwrap();
    let _ledger = phoebis_ledger::spawn(
        db.clone(),
        Arc::new(phoebis_ledger::Pricing::new(
            phoebis_ledger::PriceRevisions::shipped(),
        )),
        Arc::new(phoebis_ledger::PlanTables::shipped()),
        store.clone(),
        &phoebis_bus::Bus::new(),
        router,
        clock,
        Arc::new(phoebis_ledger::Metrics::new()),
        LedgerConfig {
            mirror_baseline: Some(baseline),
            ..polled_every_50_ms()
        },
    );
    // A holder born after the spawn: the tick reads accounts last and by id, so once its row lands every row the
    // tick wrote for `a` has.
    store
        .insert_account(&account("b", StateCause::Paired))
        .unwrap();
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "account_state_changed" && e.account_id.as_deref() == Some("b"))
            .then_some(rows)
    })
    .await;
    let of_a = |kind: &str| -> Vec<serde_json::Value> {
        rows.iter()
            .filter(|e| e.kind == kind && e.account_id.as_deref() == Some("a"))
            .map(|e| serde_json::from_str(e.detail.as_deref().unwrap()).unwrap())
            .collect()
    };
    assert_eq!(
        of_a("refresh_failed")
            .iter()
            .map(|detail| detail["error"].clone())
            .collect::<Vec<_>>(),
        [serde_json::json!("quarantined at start")],
        "the attempt the recovery settled"
    );
    assert_eq!(
        of_a("account_state_changed"),
        [
            serde_json::json!({ "from": "ready", "to": "reauth_required", "cause": "refresh_failed", "detail": null })
        ],
        "the quarantine the recovery landed"
    );
}

/// A move into `reauth_required` is an `account_state_changed` row, so the routing log's kinds name no kind of that
/// state's own.
#[test]
fn the_event_kinds_hold_no_reauth_required_kind() {
    assert!(phoebis_ledger::EVENT_KINDS.contains(&"account_state_changed"));
    assert!(
        !phoebis_ledger::EVENT_KINDS.contains(&"reauth_required"),
        "{:?}",
        phoebis_ledger::EVENT_KINDS
    );
}

#[tokio::test]
async fn the_mirror_counts_the_router_events_a_full_broadcast_dropped_before_it_read_them() {
    use std::sync::atomic::Ordering;
    use tokio::sync::broadcast::error::RecvError;
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let overage = || RouterEvent::OverageStarted {
        account_id: "a".into(),
        at: NOW,
    };
    // Once this row lands, the mirror has read everything the broadcast holds.
    h.router.emit(overage());
    wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "overage_started")
            .then_some(())
    })
    .await;
    let mut witness = h.router.subscribe_events();
    // No await between the pushes: the test's runtime runs one task at a time, so the mirror reads none of them
    // before the loop ends and falls exactly as far behind as the receiver subscribed beside it.
    for _ in 0..4_096 {
        h.router.emit(overage());
    }
    let gap = match witness.recv().await {
        Err(RecvError::Lagged(gap)) => gap,
        other => panic!("the witness falls behind the broadcast, not {other:?}"),
    };
    let lagged = wait_for(Duration::from_secs(5), || {
        let lagged = h.ledger.stats().mirror_lagged.load(Ordering::Relaxed);
        (lagged > 0).then_some(lagged)
    })
    .await;
    assert_eq!(
        lagged, gap,
        "the mirror counts every event the broadcast reports it lost"
    );
    up.shutdown().await;
}
