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
                detail: Some(r#"{"reason":"overage","from_account":"a"}"#.into()),
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
    assert_eq!(to.migrations.get("overage"), Some(&1));
    let from = rows.iter().find(|r| r.account_id == "a").unwrap();
    assert_eq!(
        from.migrations.get("overage"),
        None,
        "the source row counts pins_migrated_out, not a reason of its own"
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

/// The account rows the mirror wrote, in order: each kind with its detail.
async fn account_rows(h: &Harness) -> Vec<(String, serde_json::Value)> {
    h.db.events(0, i64::MAX, 100)
        .await
        .unwrap()
        .into_iter()
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
