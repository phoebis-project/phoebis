//! Router and registry events reach the events table, and the account rollup counts what they say.
mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_ledger::{event_of, recent_events_of_kind, EventRecord, LedgerConfig, LedgerDb};
use phoebis_routing::RouterEvent;
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
