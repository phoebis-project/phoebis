//! Router and registry events reach the events table, and the account rollup counts what they say.
mod common;
use std::time::Duration;

use phoebis_ledger::LedgerConfig;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
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
    let events = wait_for(Duration::from_secs(5), || {
        let rows = h.db.events(0, i64::MAX, 100).unwrap();
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
            h.db.read(|c| phoebis_ledger::read_account_rollups(c, hour, hour + 3_600))
                .unwrap();
        (rows.len() == 2).then_some(rows)
    })
    .await;
    let a = rollups.iter().find(|r| r.account_id == "a").unwrap();
    let b = rollups.iter().find(|r| r.account_id == "b").unwrap();
    // The classifier rounds the deadline up to the next second: until is 4_601_000 ms, at is NOW.
    assert_eq!(
        (a.pins_created, a.pins_migrated_out, a.cooldown_seconds),
        (1, 1, 3_601)
    );
    assert_eq!((b.pins_migrated_in, b.pins_created), (1, 0));
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
    stays_for(Duration::from_millis(200), || {
        h.db.events(0, i64::MAX, 100).unwrap().is_empty()
    })
    .await;
    h.store
        .update_account("a", "renamed", "admin:abc")
        .unwrap();
    let event = wait_for(Duration::from_secs(5), || {
        h.db.events(0, i64::MAX, 100)
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
    let attempt = h.store.open_refresh_attempt("a", 1, 1).unwrap();
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
    let kinds = wait_for(Duration::from_secs(5), || {
        let rows = h.db.events(0, i64::MAX, 100).unwrap();
        rows.iter()
            .any(|e| e.kind == "reauth_required")
            .then(|| rows.iter().map(|e| e.kind.clone()).collect::<Vec<_>>())
    })
    .await;
    assert!(
        kinds.contains(&"refresh_failed".to_string())
            && kinds.contains(&"reauth_required".to_string())
    );
    h.store
        .apply_catalog_sync(
            "a",
            &[("claude-sonnet-4-5".into(), false)],
            None,
            NOW,
            "scheduler",
        )
        .unwrap();
    wait_for(Duration::from_secs(5), || {
        h.db.events(0, i64::MAX, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "catalog_synced" && e.account_id.as_deref() == Some("a"))
    })
    .await;
    up.shutdown().await;
}
