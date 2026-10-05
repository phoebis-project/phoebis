//! Router and registry events reach the events table, and the account rollup counts what they say.
mod common;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_ledger::{event_of, recent_events_of_kind, EventRecord, LedgerConfig, LedgerDb};
use phoebis_routing::RouterEvent;
use phoebis_store::{AccountState, StorageKey, Store};
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
async fn registry_changes_and_resolved_refreshes_are_mirrored_and_a_reauth_mark_is_an_event() {
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
    h.store
        .update_account(
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
        .set_state("a", phoebis_store::AccountState::ReauthRequired)
        .unwrap();
    let kinds = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "reauth_required")
            .then(|| rows.iter().map(|e| e.kind.clone()).collect::<Vec<_>>())
    })
    .await;
    assert!(
        kinds.contains(&"refresh_failed".to_string())
            && kinds.contains(&"reauth_required".to_string())
    );
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
        .filter(|e| e.kind == "account_state_changed" || e.kind == "reauth_required")
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

fn reauth() -> (String, serde_json::Value) {
    ("reauth_required".to_string(), serde_json::Value::Null)
}

fn polled_every_50_ms() -> LedgerConfig {
    LedgerConfig {
        event_poll: Duration::from_millis(50),
        ..LedgerConfig::default()
    }
}

#[tokio::test]
async fn every_account_transition_writes_its_row_and_a_re_auth_keeps_its_own() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), polled_every_50_ms()).await;
    h.store
        .set_state("a", AccountState::FundingRequired)
        .unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await.len() == 1).then_some(())
    })
    .await;
    h.store
        .set_state("a", AccountState::ReauthRequired)
        .unwrap();
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = account_rows(&h).await;
        (rows.len() == 3).then_some(rows)
    })
    .await;
    assert_eq!(
        rows,
        [
            transition("ready", "funding_required", "registry", None),
            transition("funding_required", "reauth_required", "registry", None),
            reauth(),
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
        cause: "auth_failed",
        detail: Some("invalid x-api-key".into()),
        at: NOW,
    });
    let expected = [
        transition(
            "ready",
            "reauth_required",
            "auth_failed",
            Some("invalid x-api-key"),
        ),
        reauth(),
    ];
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
        .set_state("a", AccountState::ReauthRequired)
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
        .set_state("a", AccountState::FundingRequired)
        .unwrap();
    h.store
        .set_state("a", AccountState::ReauthRequired)
        .unwrap();
    let expected = [
        transition("ready", "reauth_required", "registry", None),
        reauth(),
    ];
    wait_for_async(Duration::from_secs(5), || async {
        (account_rows(&h).await == expected).then_some(())
    })
    .await;
    for (to, revision, cause) in [
        (
            AccountState::FundingRequired,
            row.revision + 1,
            "upstream_error",
        ),
        (
            AccountState::ReauthRequired,
            row.revision + 2,
            "auth_failed",
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
        None,
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
    let ledger = phoebis_ledger::spawn(
        db.clone(),
        Arc::new(phoebis_ledger::Pricing::new(
            phoebis_ledger::PriceTable::shipped(),
        )),
        store,
        &phoebis_bus::Bus::new(),
        router.clone(),
        clock,
        Arc::new(phoebis_ledger::Metrics::new()),
        polled_every_50_ms(),
    );
    ledger.record(phoebis_ledger::Record::RouterStarted {
        version: "0.1.0".into(),
        counts: phoebis_routing::LoadCounts::default(),
        at: NOW,
    });
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
