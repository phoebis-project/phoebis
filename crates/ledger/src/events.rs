//! Events that are not requests: what the router broadcasts, and what the registry and the credential
//! store record on their own, mirrored into one table so the UI has one timeline.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::Clock;
use phoebis_routing::{CreditsState, RouterEvent, RouterHandle};
use phoebis_store::{AccountState, Store};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::assemble::Record;
use crate::record::{CacheMapWrite, DecisionRow, EventRecord};
use crate::task::Ledger;
use crate::LedgerDb;

/// Rows read per mirror tick; a registry busier than this per tick is caught up on the next.
const MIRROR_BATCH: usize = 500;

/// `None` for the three variants that land as no row of their own here: a `Decision` and a `CacheMapWritten`, which
/// the mirror maps to the ledger's own records, and an `AccountStateChanged`, a transition the mirror's account
/// watch reads from the registry.
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
            request_id: source_request_id.clone(),
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
            agent_id: Some(agent_id.clone()),
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
            agent_id: Some(agent_id.clone()),
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
            agent_id: Some(agent_id.clone()),
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
            agent_id: Some(agent_id.clone()),
            ..Default::default()
        },
        RouterEvent::SheddingRound {
            account_id,
            pool_id,
            request_id,
            key_id,
            session_id,
            agent_id,
            model,
            round,
            at,
        } => EventRecord {
            at: *at,
            kind: "shedding_round".into(),
            account_id: Some(account_id.clone()),
            key_id: Some(key_id.clone()),
            session_id: Some(session_id.clone()),
            model: Some(model.clone()),
            // The round's facts serialize as the detail's fields, the pins and the marks as arrays of objects.
            detail: serde_json::to_string(round).ok(),
            pool_id: Some(pool_id.clone()),
            agent_id: Some(agent_id.clone()),
            request_id: Some(request_id.clone()),
            ..Default::default()
        },
        RouterEvent::FastCreditsChanged {
            account_id,
            before,
            after,
            blocked_before,
            blocked_after,
            at,
        } => {
            let credits = |state: &CreditsState| {
                json!({
                    "enabled": state.enabled,
                    "disabled_reason": state.disabled_reason,
                    "utilization": state.utilization,
                })
            };
            EventRecord {
                at: *at,
                kind: "fast_credits_changed".into(),
                account_id: Some(account_id.clone()),
                detail: Some(
                    json!({
                        "before": before.as_ref().map(credits),
                        "after": credits(after),
                        "blocked_before": blocked_before,
                        "blocked_after": blocked_after,
                        "observed_at": after.observed_at,
                    })
                    .to_string(),
                ),
                ..Default::default()
            }
        }
        RouterEvent::WindowSpent {
            account_id,
            window,
            utilization,
            reset,
            source,
            source_request_id,
            at,
        } => EventRecord {
            at: *at,
            kind: "window_spent".into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "window": window,
                    "utilization": utilization,
                    "reset": reset,
                    "source": source.as_str(),
                    "source_request_id": source_request_id,
                })
                .to_string(),
            ),
            request_id: source_request_id.clone(),
            ..Default::default()
        },
        RouterEvent::WindowReset {
            account_id,
            window,
            utilization_before,
            reset_at,
            next_reset,
            observed,
            at,
        } => EventRecord {
            at: *at,
            kind: "window_reset".into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "window": window,
                    "utilization_before": utilization_before,
                    "reset_at": reset_at,
                    "next_reset": next_reset,
                    "observed": observed,
                })
                .to_string(),
            ),
            ..Default::default()
        },
        RouterEvent::WarmPrefixExpired {
            account_id,
            key_id,
            root,
            model,
            lane,
            entries,
            deepest,
            cause,
            at,
        } => EventRecord {
            at: *at,
            kind: "warm_prefix_expired".into(),
            account_id: Some(account_id.clone()),
            key_id: Some(key_id.clone()),
            // The root is the conversation's identity, which is what the session column holds.
            session_id: Some(root.clone()),
            model: Some(model.clone()),
            detail: Some(
                json!({
                    "root": root,
                    "entries": entries,
                    "deepest": deepest,
                    "lane": lane,
                    "cause": cause,
                })
                .to_string(),
            ),
            ..Default::default()
        },
        RouterEvent::WindDownStarted {
            account_id,
            pool_id,
            ends_at,
            wind_down_minutes,
            end_source,
            at,
        } => EventRecord {
            at: *at,
            kind: "wind_down_started".into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "ends_at": ends_at,
                    "wind_down_minutes": wind_down_minutes,
                    "end_source": end_source,
                })
                .to_string(),
            ),
            pool_id: Some(pool_id.clone()),
            ..Default::default()
        },
        RouterEvent::Decision { .. }
        | RouterEvent::CacheMapWritten { .. }
        | RouterEvent::AccountStateChanged { .. } => return None,
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
                    Ok(RouterEvent::Decision { summary, record, at }) => {
                        ledger.record(Record::Decision(Box::new(DecisionRow::of(*summary, *record, at))));
                    }
                    Ok(RouterEvent::CacheMapWritten { request_id, seq, attempt, entries, at }) => {
                        ledger.record(Record::CacheMap(CacheMapWrite { request_id, seq, attempt, entries, at }));
                    }
                    Ok(event) => {
                        if let Some(row) = event_of(&event) {
                            ledger.record(Record::Event(row));
                        }
                    }
                    // Counted apart from `dropped`, so that figure keeps meaning the ledger's own records.
                    Err(RecvError::Lagged(n)) => {
                        ledger.stats().mirror_lagged.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(lost = n, "the ledger fell behind the router's events");
                    }
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
                                // Every pool change names the pool as its target.
                                pool_id: change.kind.starts_with("pool_").then(|| change.target.clone()),
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
        assert_eq!(
            e.agent_id.as_deref(),
            Some("main"),
            "the pin's agent has its own column beside the detail's copy"
        );
        let e = event_of(&RouterEvent::CooldownSet {
            account_id: "a".into(),
            scope: "model:m".into(),
            until: 9,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: Some("r1".into()),
            at: 1,
        })
        .expect("a cooldown row");
        assert_eq!(
            (
                e.kind.as_str(),
                e.account_id.as_deref(),
                e.model.as_deref(),
                e.request_id.as_deref()
            ),
            ("cooldown_set", Some("a"), Some("m"), Some("r1"))
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

    fn detail_of(row: &EventRecord) -> serde_json::Value {
        serde_json::from_str(row.detail.as_deref().expect("a detail")).unwrap()
    }

    /// The row's columns: kind, account, pool, key, session, agent, model, request.
    type Columns<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );

    fn columns(row: &EventRecord) -> Columns<'_> {
        (
            row.kind.as_str(),
            row.account_id.as_deref(),
            row.pool_id.as_deref(),
            row.key_id.as_deref(),
            row.session_id.as_deref(),
            row.agent_id.as_deref(),
            row.model.as_deref(),
            row.request_id.as_deref(),
        )
    }

    #[test]
    fn the_router_s_state_changes_map_to_their_kinds_with_their_columns_and_detail() {
        use phoebis_routing::{
            CacheMapEntry, CreditsState, PinKeyFacts, ShedPinFacts, ShedRoundFacts, WindowSource,
        };
        let key = PinKeyFacts {
            key_id: "k".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            model: "m".into(),
        };
        let row = event_of(&RouterEvent::SheddingRound {
            account_id: "a".into(),
            pool_id: "p".into(),
            request_id: "r1".into(),
            key_id: "k".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            model: "m".into(),
            round: ShedRoundFacts {
                window: "seven_day".into(),
                utilization: 0.5,
                elapsed_share: 0.1,
                ahead: 0.4,
                threshold: 0.2,
                margin: 0.2,
                unit: "window_share",
                burn_window_seconds: 600,
                shed_share: 0.5,
                burn_sum: 5.0,
                target: 2.5,
                warm_sessions: "when_needed",
                pins: vec![ShedPinFacts {
                    key_id: "k".into(),
                    session_id: "s1".into(),
                    agent_id: "main".into(),
                    model: "m".into(),
                    cold: true,
                    burn: Some(4.0),
                    last_input_tokens: 1_000,
                    per_rewrite: Some(0.004),
                    marked: true,
                    note: Some("current"),
                }],
                marks: vec![key.clone()],
                dwell_seconds: 900,
                dwell_until: 901_000,
            },
            at: 1_000,
        })
        .expect("a round row");
        assert_eq!(
            (row.at, columns(&row)),
            (
                1_000,
                (
                    "shedding_round",
                    Some("a"),
                    Some("p"),
                    Some("k"),
                    Some("s1"),
                    Some("main"),
                    Some("m"),
                    Some("r1")
                )
            )
        );
        let detail = detail_of(&row);
        for (field, value) in [
            ("window", json!("seven_day")),
            ("utilization", json!(0.5)),
            ("elapsed_share", json!(0.1)),
            ("ahead", json!(0.4)),
            ("threshold", json!(0.2)),
            ("margin", json!(0.2)),
            ("unit", json!("window_share")),
            ("burn_window_seconds", json!(600)),
            ("shed_share", json!(0.5)),
            ("burn_sum", json!(5.0)),
            ("target", json!(2.5)),
            ("warm_sessions", json!("when_needed")),
            ("dwell_seconds", json!(900)),
            ("dwell_until", json!(901_000)),
        ] {
            assert_eq!(detail[field], value, "{field}");
        }
        assert_eq!(
            detail["pins"],
            json!([{
                "key_id": "k", "session_id": "s1", "agent_id": "main", "model": "m", "cold": true,
                "burn": 4.0, "last_input_tokens": 1_000, "per_rewrite": 0.004, "marked": true,
                "note": "current"
            }]),
            "each pin is an object of one-value fields"
        );
        assert_eq!(
            detail["marks"],
            json!([{"key_id": "k", "session_id": "s1", "agent_id": "main", "model": "m"}])
        );

        let credits = |enabled: bool, reason: Option<&str>, observed_at: i64| CreditsState {
            enabled,
            disabled_reason: reason.map(String::from),
            utilization: Some(0.25),
            observed_at,
            incarnation: 1,
        };
        let row = event_of(&RouterEvent::FastCreditsChanged {
            account_id: "a".into(),
            before: Some(credits(true, None, 1_000)),
            after: credits(false, Some("org_level_disabled"), 2_000),
            blocked_before: None,
            blocked_after: Some("org_level_disabled".into()),
            at: 2_500,
        })
        .expect("a credits row");
        assert_eq!(
            columns(&row),
            (
                "fast_credits_changed",
                Some("a"),
                None,
                None,
                None,
                None,
                None,
                None
            )
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "before": {"enabled": true, "disabled_reason": null, "utilization": 0.25},
                "after": {"enabled": false, "disabled_reason": "org_level_disabled", "utilization": 0.25},
                "blocked_before": null,
                "blocked_after": "org_level_disabled",
                "observed_at": 2_000,
            })
        );

        let row = event_of(&RouterEvent::WindowSpent {
            account_id: "a".into(),
            window: "five_hour".into(),
            utilization: 1.0,
            reset: Some(9_000),
            source: WindowSource::Head,
            source_request_id: Some("r2".into()),
            at: 3_000,
        })
        .expect("a window row");
        assert_eq!(
            columns(&row),
            (
                "window_spent",
                Some("a"),
                None,
                None,
                None,
                None,
                None,
                Some("r2")
            ),
            "the head that reported the reading names the request"
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "window": "five_hour", "utilization": 1.0, "reset": 9_000, "source": "head",
                "source_request_id": "r2"
            })
        );

        let row = event_of(&RouterEvent::WindowReset {
            account_id: "a".into(),
            window: "five_hour".into(),
            utilization_before: 0.7,
            reset_at: 2_000,
            next_reset: None,
            observed: "clock",
            at: 2_050,
        })
        .expect("a reset row");
        assert_eq!(
            columns(&row),
            (
                "window_reset",
                Some("a"),
                None,
                None,
                None,
                None,
                None,
                None
            )
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "window": "five_hour", "utilization_before": 0.7, "reset_at": 2_000, "next_reset": null,
                "observed": "clock"
            })
        );

        let row = event_of(&RouterEvent::WarmPrefixExpired {
            account_id: "a".into(),
            key_id: "k".into(),
            root: "s1".into(),
            model: "m".into(),
            lane: "m|standard|none|-".into(),
            entries: 2,
            deepest: CacheMapEntry {
                tier: "message",
                depth: 3,
                prefix_tokens: 1_200,
                ttl_ms: 300_000,
                account_id: "a".into(),
            },
            cause: "ttl",
            at: 4_000,
        })
        .expect("an expiry row");
        assert_eq!(
            columns(&row),
            (
                "warm_prefix_expired",
                Some("a"),
                None,
                Some("k"),
                Some("s1"),
                None,
                Some("m"),
                None
            ),
            "the root fills the session column"
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "root": "s1", "entries": 2,
                "deepest": {"tier": "message", "depth": 3, "prefix_tokens": 1_200, "ttl_ms": 300_000,
                    "account_id": "a"},
                "lane": "m|standard|none|-", "cause": "ttl"
            })
        );

        let row = event_of(&RouterEvent::WindDownStarted {
            account_id: "a".into(),
            pool_id: "p".into(),
            ends_at: 9_000,
            wind_down_minutes: 30,
            end_source: "cancelled",
            at: 5_000,
        })
        .expect("a wind-down row");
        assert_eq!(
            columns(&row),
            (
                "wind_down_started",
                Some("a"),
                Some("p"),
                None,
                None,
                None,
                None,
                None
            )
        );
        assert_eq!(
            detail_of(&row),
            json!({"ends_at": 9_000, "wind_down_minutes": 30, "end_source": "cancelled"})
        );
    }

    #[test]
    fn an_account_state_change_maps_to_no_event_row() {
        assert_eq!(
            event_of(&RouterEvent::AccountStateChanged {
                account_id: "a".into(),
                to: AccountState::ReauthRequired,
                incarnation: 1,
                revision: 4,
                cause: "auth_failed",
                detail: None,
                at: 1,
            }),
            None,
            "the transition is the account watch's row"
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
