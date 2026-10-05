//! Events that are not requests: what the router broadcasts, and what the registry and the credential
//! store record on their own, mirrored into one table so the UI has one timeline.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::Clock;
use phoebis_routing::{RouterEvent, RouterHandle};
use phoebis_store::{AccountState, Store};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::assemble::Record;
use crate::record::EventRecord;
use crate::task::Ledger;
use crate::LedgerDb;

/// Rows read per mirror tick; a registry busier than this per tick is caught up on the next.
const MIRROR_BATCH: usize = 500;

/// `None` for a variant the mirror consumes itself rather than as an event row: a `Decision` or a
/// `CacheMapWritten`, the ledger's own records.
pub fn event_of(event: &RouterEvent) -> Option<EventRecord> {
    let model_of = |scope: &str| scope.strip_prefix("model:").map(str::to_string);
    let row = match event {
        RouterEvent::CooldownCleared {
            account_id,
            scope,
            cause,
            at,
        } => EventRecord {
            at: *at,
            kind: "cooldown_cleared".into(),
            account_id: Some(account_id.clone()),
            model: model_of(scope),
            detail: Some(json!({ "cause": cause, "scope": scope }).to_string()),
            ..Default::default()
        },
        RouterEvent::CooldownSet {
            account_id,
            scope,
            until,
            reason,
            window,
            source_request_id,
            at,
        } => EventRecord {
            at: *at,
            kind: "cooldown_set".into(),
            account_id: Some(account_id.clone()),
            model: model_of(scope),
            detail: Some(
                json!({ "scope": scope, "until": until, "reason": reason, "window": window, "source_request_id": source_request_id })
                    .to_string(),
            ),
            ..Default::default()
        },
        RouterEvent::OverageEnded {
            account_id,
            since,
            at,
        } => EventRecord {
            at: *at,
            kind: "overage_ended".into(),
            account_id: Some(account_id.clone()),
            detail: Some(json!({ "since": since }).to_string()),
            ..Default::default()
        },
        RouterEvent::OverageStarted { account_id, at } => EventRecord {
            at: *at,
            kind: "overage_started".into(),
            account_id: Some(account_id.clone()),
            ..Default::default()
        },
        RouterEvent::PinCreated {
            key_id,
            session_id,
            agent_id,
            model,
            account_id,
            epoch,
            at,
        } => EventRecord {
            at: *at,
            kind: "pin_created".into(),
            account_id: Some(account_id.clone()),
            key_id: Some(key_id.clone()),
            session_id: Some(session_id.clone()),
            model: Some(model.clone()),
            detail: Some(json!({ "agent_id": agent_id, "epoch": epoch }).to_string()),
            ..Default::default()
        },
        RouterEvent::PinExpired {
            key_id,
            session_id,
            agent_id,
            model,
            account_id,
            reasons,
            at,
        } => EventRecord {
            at: *at,
            kind: "pin_expired".into(),
            account_id: Some(account_id.clone()),
            key_id: Some(key_id.clone()),
            session_id: Some(session_id.clone()),
            model: Some(model.clone()),
            detail: Some(json!({ "agent_id": agent_id, "reasons": reasons }).to_string()),
            ..Default::default()
        },
        RouterEvent::PinMigrated {
            key_id,
            session_id,
            agent_id,
            model,
            from_account,
            to_account,
            epoch,
            reason,
            at,
        } => EventRecord {
            at: *at,
            kind: "pin_migrated".into(),
            account_id: Some(to_account.clone()),
            key_id: Some(key_id.clone()),
            session_id: Some(session_id.clone()),
            model: Some(model.clone()),
            detail: Some(
                json!({ "agent_id": agent_id, "from_account": from_account, "epoch": epoch, "reason": reason })
                    .to_string(),
            ),
            ..Default::default()
        },
        RouterEvent::PinReleased {
            key_id,
            session_id,
            agent_id,
            model,
            account_id,
            at,
        } => EventRecord {
            at: *at,
            kind: "pin_released".into(),
            account_id: Some(account_id.clone()),
            key_id: Some(key_id.clone()),
            session_id: Some(session_id.clone()),
            model: Some(model.clone()),
            detail: Some(json!({ "agent_id": agent_id }).to_string()),
            ..Default::default()
        },
        RouterEvent::Decision { .. } | RouterEvent::CacheMapWritten { .. } => return None,
    };
    Some(row)
}

pub fn spawn_event_mirror(
    _db: Arc<LedgerDb>,
    ledger: Ledger,
    store: Arc<Store>,
    router: RouterHandle,
    clock: Clock,
    poll: Duration,
) -> tokio::task::JoinHandle<()> {
    // Subscribed and read here rather than inside the task: a spawned task does not run until the caller
    // yields, and everything the router broadcast or the store recorded in between would be lost or replayed.
    let mut events = router.subscribe_events();
    // Watermarks start at the newest row: history before the mirror existed is not replayed on every restart.
    let mut last_change = store
        .newest_registry_change_id()
        .ok()
        .flatten()
        .unwrap_or(0);
    // The pair is the cursor: a batch that ends inside one millisecond resumes at the next id in it.
    let mut last_resolved: (i64, u64) = store
        .newest_resolved_refresh_attempt()
        .ok()
        .flatten()
        .unwrap_or((0, 0));
    let mut states: HashMap<String, AccountState> = store
        .accounts()
        .map(|rows| rows.into_iter().map(|a| (a.id, a.state)).collect())
        .unwrap_or_default();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => {
                        if let Some(row) = event_of(&event) {
                            ledger.record(Record::Event(row));
                        }
                    }
                    Err(RecvError::Lagged(n)) => tracing::warn!(lost = n, "the ledger fell behind the router's events"),
                    Err(RecvError::Closed) => break,
                },
                _ = tick.tick() => {
                    if let Ok(changes) = store.registry_changes_after(last_change, MIRROR_BATCH) {
                        for change in changes {
                            last_change = change.id;
                            ledger.record(Record::Event(EventRecord {
                                at: change.at,
                                kind: "registry_changed".into(),
                                actor: Some(change.actor.clone()),
                                detail: Some(json!({
                                    "kind": change.kind,
                                    "target": change.target,
                                    // Old rows carry `diff_json` re-encoded as a JSON string; a row already parsed
                                    // this task falls back to that string so both shapes read as a value here.
                                    "diff": serde_json::from_str::<serde_json::Value>(&change.diff_json)
                                        .unwrap_or(serde_json::Value::String(change.diff_json.clone())),
                                }).to_string()),
                                ..Default::default()
                            }));
                        }
                    }
                    if let Ok(attempts) = store.refresh_attempts_resolved_after(last_resolved.0, last_resolved.1, MIRROR_BATCH) {
                        for attempt in attempts {
                            last_resolved = (attempt.resolved_at, attempt.id);
                            let kind = match attempt.outcome.as_str() {
                                "committed" => "refresh_succeeded",
                                "uncertain" => "refresh_uncertain",
                                _ => "refresh_failed",
                            };
                            ledger.record(Record::Event(EventRecord {
                                at: attempt.resolved_at,
                                kind: kind.into(),
                                account_id: Some(attempt.account_id),
                                detail: attempt.detail,
                                ..Default::default()
                            }));
                        }
                    }
                    if let Ok(accounts) = store.accounts() {
                        let now = clock();
                        for account in accounts {
                            let before = states.insert(account.id.clone(), account.state);
                            if account.state == AccountState::ReauthRequired && before != Some(AccountState::ReauthRequired) {
                                ledger.record(Record::Event(EventRecord {
                                    at: now,
                                    kind: "reauth_required".into(),
                                    account_id: Some(account.id),
                                    ..Default::default()
                                }));
                            }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_routing::RouterEvent;

    #[test]
    fn router_events_map_to_rows_with_the_ids_in_their_columns_and_the_rest_in_detail() {
        let e = event_of(&RouterEvent::PinMigrated {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            from_account: "a".into(),
            to_account: "b".into(),
            epoch: 7,
            reason: "cooldown".into(),
            at: 5,
        })
        .expect("a pin row");
        assert_eq!(
            (
                e.at,
                e.kind.as_str(),
                e.account_id.as_deref(),
                e.key_id.as_deref(),
                e.session_id.as_deref(),
                e.model.as_deref()
            ),
            (
                5,
                "pin_migrated",
                Some("b"),
                Some("k"),
                Some("s"),
                Some("m")
            )
        );
        let detail: serde_json::Value = serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            (
                detail["agent_id"].as_str(),
                detail["from_account"].as_str(),
                detail["epoch"].as_u64(),
                detail["reason"].as_str()
            ),
            (Some("main"), Some("a"), Some(7), Some("cooldown"))
        );
        let e = event_of(&RouterEvent::CooldownSet {
            account_id: "a".into(),
            scope: "model:m".into(),
            until: 9,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            at: 1,
        })
        .expect("a cooldown row");
        assert_eq!(
            (e.kind.as_str(), e.account_id.as_deref(), e.model.as_deref()),
            ("cooldown_set", Some("a"), Some("m"))
        );
        let e = event_of(&RouterEvent::CooldownCleared {
            account_id: "a".into(),
            scope: "model:m".into(),
            cause: "recovered",
            at: 1,
        })
        .expect("a cooldown row");
        assert_eq!(
            (e.kind.as_str(), e.account_id.as_deref(), e.model.as_deref()),
            ("cooldown_cleared", Some("a"), Some("m"))
        );
        let detail: serde_json::Value = serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            (detail["cause"].as_str(), detail["scope"].as_str()),
            (Some("recovered"), Some("model:m")),
            "what released the row travels beside the scope it was held under"
        );
        for (event, kind) in [
            (
                RouterEvent::CooldownCleared {
                    account_id: "a".into(),
                    scope: "account".into(),
                    cause: "expired",
                    at: 1,
                },
                "cooldown_cleared",
            ),
            (
                RouterEvent::OverageStarted {
                    account_id: "a".into(),
                    at: 1,
                },
                "overage_started",
            ),
            (
                RouterEvent::OverageEnded {
                    account_id: "a".into(),
                    since: Some(0),
                    at: 1,
                },
                "overage_ended",
            ),
            (
                RouterEvent::PinCreated {
                    key_id: "k".into(),
                    session_id: "s".into(),
                    agent_id: "main".into(),
                    model: "m".into(),
                    account_id: "a".into(),
                    epoch: 1,
                    at: 1,
                },
                "pin_created",
            ),
            (
                RouterEvent::PinExpired {
                    key_id: "k".into(),
                    session_id: "s".into(),
                    agent_id: "main".into(),
                    model: "m".into(),
                    account_id: "a".into(),
                    reasons: vec!["expired"],
                    at: 1,
                },
                "pin_expired",
            ),
            (
                RouterEvent::PinReleased {
                    key_id: "k".into(),
                    session_id: "s".into(),
                    agent_id: "main".into(),
                    model: "m".into(),
                    account_id: "a".into(),
                    at: 1,
                },
                "pin_released",
            ),
        ] {
            assert_eq!(event_of(&event).map(|row| row.kind), Some(kind.to_string()));
        }
        let row = event_of(&RouterEvent::PinExpired {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            account_id: "a".into(),
            reasons: vec!["cooldown", "spent"],
            at: 1,
        })
        .expect("a pin row");
        assert_eq!(
            row.detail.as_deref(),
            Some(r#"{"agent_id":"main","reasons":["cooldown","spent"]}"#),
            "the facts travel in the selection's order"
        );
    }

    #[test]
    fn a_decision_and_a_cache_map_map_to_no_event_row() {
        use phoebis_routing::{
            CacheMapEntry, ClientClass, DecisionRecord, RefusalFacts, RefusedRequest,
            RegistrySnapshot,
        };
        let snapshot =
            RegistrySnapshot::from_load(phoebis_store::RegistryLoad::default(), [0; 32], 0, 1);
        let (record, summary) = DecisionRecord::refused(
            &RefusedRequest {
                request_id: "r".into(),
                attempt: 1,
                seq: 1,
                at: 1,
                key_id: "k".into(),
                key_generation: 0,
                agent_id: "main".into(),
                session_id: None,
                canonical_model: "m".into(),
                client: ClientClass::ClaudeCode,
                client_version: None,
            },
            &snapshot,
            &RefusalFacts {
                kind: "client",
                reason: "hash; client unknown".into(),
            },
        );
        assert_eq!(
            event_of(&RouterEvent::Decision {
                summary: Box::new(summary),
                record: Box::new(record),
                at: 1,
            }),
            None
        );
        assert_eq!(
            event_of(&RouterEvent::CacheMapWritten {
                request_id: "r".into(),
                seq: 1,
                attempt: 1,
                entries: vec![CacheMapEntry {
                    tier: "message",
                    depth: 1,
                    prefix_tokens: 1_200,
                    ttl_ms: 300_000,
                    account_id: "a".into(),
                }],
                at: 1,
            }),
            None
        );
    }
}
