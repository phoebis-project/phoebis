//! Events that are not requests: what the router broadcasts, and what the registry and the credential
//! store record on their own, mirrored into one table so the UI has one timeline.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::{BackoffEdge, Clock};
use phoebis_relay::CatalogChange;
use phoebis_routing::{CreditsState, RouterEvent, RouterHandle};
use phoebis_store::{AccountState, ResolvedAttempt, StateCause, Store};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::assemble::Record;
use crate::db::newest_row_at;
use crate::record::{CacheMapWrite, DecisionRow, EventRecord};
use crate::task::Ledger;
use crate::LedgerDb;

/// Rows read per mirror tick; a registry busier than this per tick is caught up on the next.
const MIRROR_BATCH: usize = 500;

/// `None` for the five variants that land as no row of their own here: a `Decision` and a `CacheMapWritten`, which
/// the mirror maps to the ledger's own records, an `AccountStateChanged`, a transition the mirror's account watch
/// reads from the registry, and a `MarkCleared` and a `ReservationExpired`, which have no row.
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
            // The round's facts serialize as the detail's fields, the pins and the marks as arrays of objects. The
            // routing task writes every round to the registry with the decision's writes, so its marks and dwell
            // outlive a restart.
            detail: serde_json::to_value(round).ok().map(|mut detail| {
                detail["persisted"] = json!(true);
                detail.to_string()
            }),
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
            restored: _,
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
            restored: _,
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
        | RouterEvent::AccountStateChanged { .. }
        | RouterEvent::MarkCleared { .. }
        | RouterEvent::ReservationExpired { .. } => return None,
    };
    Some(row)
}

/// `poll_backoff_started` or `poll_backoff_ended`, on the account the usage service backs off.
pub fn poll_backoff(edge: &BackoffEdge) -> Record {
    let (at, kind, account_id, detail) = match edge {
        BackoffEdge::Started {
            account_id,
            at,
            endpoint,
            reason,
            attempts,
            next_attempt_at,
            last_clean_at,
        } => (
            *at,
            "poll_backoff_started",
            account_id,
            json!({
                "reason": reason,
                "endpoint": endpoint,
                "attempts": attempts,
                "next_attempt_at": next_attempt_at,
                "last_clean_poll_at": last_clean_at,
            }),
        ),
        BackoffEdge::Ended {
            account_id,
            at,
            since,
            attempts,
            resumed_at,
        } => (
            *at,
            "poll_backoff_ended",
            account_id,
            json!({
                "duration_ms": at - since,
                "attempts": attempts,
                "resumed_at": resumed_at,
            }),
        ),
    };
    Record::Event(EventRecord {
        at,
        kind: kind.into(),
        account_id: Some(account_id.clone()),
        detail: Some(detail.to_string()),
        ..Default::default()
    })
}

/// `catalog_changed` on the account whose listing moved. The ids travel in the detail, since one row names many
/// models.
pub fn catalog_changed(change: &CatalogChange) -> Record {
    Record::Event(EventRecord {
        at: change.at,
        kind: "catalog_changed".into(),
        account_id: Some(change.account_id.clone()),
        detail: Some(
            json!({
                "added": change.added,
                "removed": change.removed,
                "count": change.count,
            })
            .to_string(),
        ),
        ..Default::default()
    })
}

/// An account's state as the mirror last observed it, through the registry or the routing loop's event, with the
/// row's identity at that observation: an observation at or below it is older than what the mirror already wrote.
#[derive(Clone, Copy, Debug)]
struct Observed {
    state: AccountState,
    incarnation: u64,
    revision: u64,
}

impl Observed {
    fn newer_than(&self, held: &Observed) -> bool {
        (self.incarnation, self.revision) > (held.incarnation, held.revision)
    }
}

fn record_transition(
    ledger: &Ledger,
    account_id: &str,
    from: AccountState,
    to: AccountState,
    cause: StateCause,
    detail: Option<&str>,
    at: i64,
) {
    ledger.record(Record::Event(EventRecord {
        at,
        kind: "account_state_changed".into(),
        account_id: Some(account_id.to_string()),
        detail: Some(
            json!({ "from": from.as_str(), "to": to.as_str(), "cause": cause.as_str(), "detail": detail })
                .to_string(),
        ),
        ..Default::default()
    }));
}

/// The account's last row: the registry holds no row under the identity the mirror held.
fn record_deletion(ledger: &Ledger, account_id: &str, from: AccountState, at: i64) {
    // `deleted` is the one `to` that names no account state: the row it would describe is gone.
    let deleted = StateCause::Deleted.as_str();
    ledger.record(Record::Event(EventRecord {
        at,
        kind: "account_state_changed".into(),
        account_id: Some(account_id.to_string()),
        detail: Some(
            json!({ "from": from.as_str(), "to": deleted, "cause": deleted, "detail": null })
                .to_string(),
        ),
        ..Default::default()
    }));
}

/// A refresh row's detail as fields: the committed token's expiry, or the failure's class beside its text as the
/// store holds it. The expiry is read off the account's credential at the tick, which names it only while the stored
/// bundle is the one the commit wrote: a later commit, a re-pair or a disconnect in between leaves the expiry null
/// rather than another bundle's. A failed read is logged at the first of a run of them, `read_failing` holding the run.
fn refresh_detail(
    store: &Store,
    attempt: &ResolvedAttempt,
    read_failing: &mut bool,
) -> serde_json::Value {
    if let Some(generation) = attempt.committed_generation() {
        let credential = match store.credential_metadata(&attempt.account_id) {
            Ok(credential) => {
                *read_failing = false;
                credential
            }
            Err(error) => {
                if !std::mem::replace(read_failing, true) {
                    tracing::warn!(
                        account = %attempt.account_id,
                        %error,
                        "a refresh row names no expiry: the credential could not be read"
                    );
                }
                None
            }
        }
        .filter(|c| {
            c.generation == generation && c.lifecycle_generation == attempt.lifecycle_generation
        });
        let access = credential.as_ref().map(|c| c.expires_at);
        return json!({
            "until": access,
            "access_expires_at": access,
            "refresh_expires_at": credential.and_then(|c| c.refresh_expires_at),
        });
    }
    json!({
        "cause": attempt.detail.as_deref().map(phoebis_oauth::refresh_cause),
        "error": attempt.detail,
    })
}

/// Whether the previous run announced this already: the first purge after a start walks every restored reading
/// and pool member, so a clock reset that passed, or a wind-down that opened, before that run's last row was that
/// run's to announce. A later purge announces what this run found, whenever its window opened, and a fresh file has
/// no previous run.
fn announced_before(event: &RouterEvent, down_since: Option<i64>) -> bool {
    let Some(down_since) = down_since else {
        return false;
    };
    match event {
        RouterEvent::WindowReset {
            observed: "clock",
            restored: true,
            reset_at,
            ..
        } => *reset_at < down_since,
        RouterEvent::WindDownStarted {
            restored: true,
            ends_at,
            wind_down_minutes,
            ..
        } => ends_at - i64::from(*wind_down_minutes) * 60_000 < down_since,
        _ => false,
    }
}

/// The mirror's starting point: the newest registry change and resolved refresh attempt, and each account's
/// state as the registry holds it. Read before the start's recovery, so a refresh the recovery settles and a
/// state it lands are mirrored as changes rather than taken as history.
#[derive(Clone, Debug)]
pub struct MirrorBaseline {
    last_change: u64,
    /// The pair is the cursor: a batch that ends inside one millisecond resumes at the next id in it.
    last_resolved: (i64, u64),
    states: HashMap<String, Observed>,
}

impl MirrorBaseline {
    /// Watermarks start at the newest row: history before the mirror existed is not replayed on every restart.
    pub fn read(store: &Store) -> MirrorBaseline {
        MirrorBaseline {
            last_change: store
                .newest_registry_change_id()
                .ok()
                .flatten()
                .unwrap_or(0),
            last_resolved: store
                .newest_resolved_refresh_attempt()
                .ok()
                .flatten()
                .unwrap_or((0, 0)),
            states: store
                .accounts()
                .map(|rows| {
                    rows.into_iter()
                        .map(|a| {
                            let observed = Observed {
                                state: a.state,
                                incarnation: a.incarnation,
                                revision: a.revision,
                            };
                            (a.id, observed)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

pub fn spawn_event_mirror(
    db: Arc<LedgerDb>,
    ledger: Ledger,
    store: Arc<Store>,
    router: RouterHandle,
    clock: Clock,
    poll: Duration,
    baseline: Option<MirrorBaseline>,
) -> tokio::task::JoinHandle<()> {
    // Subscribed and read here rather than inside the task: a spawned task does not run until the caller
    // yields, and everything the router broadcast or the store recorded in between would be lost or replayed.
    let mut events = router.subscribe_events();
    let MirrorBaseline {
        mut last_change,
        mut last_resolved,
        mut states,
    } = baseline.unwrap_or_else(|| MirrorBaseline::read(&store));
    // A deleted holder's incarnation, so an event about it that arrives after the tick wrote its last row is dropped
    // rather than read as a holder the mirror never saw.
    let mut gone: HashMap<String, u64> = HashMap::new();
    // Read before this run's writer lands anything, so it names the previous run's newest row.
    let down_since = match db.read_blocking(|conn| Ok(newest_row_at(conn)?)) {
        Ok(down_since) => down_since,
        Err(error) => {
            tracing::warn!(
                %error,
                "the ledger's newest row could not be read, so a reset or a wind-down the previous run announced \
                 is written again"
            );
            None
        }
    };
    tokio::spawn(async move {
        let mut credential_read_failing = false;
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
                    Ok(RouterEvent::AccountStateChanged { account_id, to, incarnation, revision, cause, detail, at }) => {
                        let observed = Observed { state: to, incarnation, revision };
                        let held = states.get(&account_id).copied();
                        let about_gone = gone.get(&account_id).is_some_and(|&buried| incarnation <= buried);
                        // At or below the held identity, the event is older than what the mirror already observed.
                        if !about_gone && held.is_none_or(|held| observed.newer_than(&held)) {
                            match held {
                                Some(held) if held.incarnation == incarnation && held.state == to => {}
                                Some(held) if held.incarnation == incarnation => {
                                    record_transition(&ledger, &account_id, held.state, to, cause, detail.as_deref(), at)
                                }
                                // The held holder is gone and another took its id before a tick saw the gap.
                                Some(held) => {
                                    record_deletion(&ledger, &account_id, held.state, at);
                                    if to == AccountState::ReauthRequired {
                                        record_transition(&ledger, &account_id, AccountState::Unpaired, AccountState::ReauthRequired, cause, detail.as_deref(), at);
                                    }
                                }
                                // A holder the mirror never saw: the registry's own row says it was created. One born
                                // marked records its move from `unpaired`, as a row born paired does, so its history
                                // opens the same way.
                                None if to == AccountState::ReauthRequired => {
                                    record_transition(&ledger, &account_id, AccountState::Unpaired, AccountState::ReauthRequired, cause, detail.as_deref(), at)
                                }
                                None => {}
                            }
                            states.insert(account_id, observed);
                        }
                    }
                    Ok(event) => {
                        if announced_before(&event, down_since) {
                            continue;
                        }
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
                            let detail = refresh_detail(&store, &attempt, &mut credential_read_failing).to_string();
                            ledger.record(Record::Event(EventRecord {
                                at: attempt.resolved_at,
                                kind: kind.into(),
                                account_id: Some(attempt.account_id),
                                detail: Some(detail),
                                ..Default::default()
                            }));
                        }
                    }
                    if let Ok(accounts) = store.accounts() {
                        let now = clock();
                        let carried: HashSet<String> = accounts.iter().map(|account| account.id.clone()).collect();
                        for account in accounts {
                            let observed = Observed {
                                state: account.state,
                                incarnation: account.incarnation,
                                revision: account.revision,
                            };
                            let held = states.get(&account.id).copied();
                            // At or below the held identity, the row was observed already, by an earlier tick or by
                            // the routing loop's event.
                            if held.is_some_and(|held| !observed.newer_than(&held)) {
                                continue;
                            }
                            match held {
                                Some(held) if held.incarnation == account.incarnation && held.state == account.state => {}
                                Some(held) if held.incarnation == account.incarnation => {
                                    record_transition(&ledger, &account.id, held.state, account.state, account.state_cause, None, now)
                                }
                                // The held holder is gone and another took its id between two ticks.
                                Some(held) => {
                                    record_deletion(&ledger, &account.id, held.state, now);
                                    if account.state == AccountState::ReauthRequired {
                                        record_transition(&ledger, &account.id, AccountState::Unpaired, AccountState::ReauthRequired, account.state_cause, None, now);
                                    }
                                }
                                // A holder the mirror never saw: the registry's own row says it was created. One born
                                // marked records its move from `unpaired`, as the row born paired below does.
                                None if account.state == AccountState::ReauthRequired => {
                                    record_transition(&ledger, &account.id, AccountState::Unpaired, AccountState::ReauthRequired, account.state_cause, None, now)
                                }
                                // A row born paired skipped `unpaired`: the transition a pairing on an existing row records is
                                // recorded for it too, so the account's history opens the same way.
                                None if account.state == AccountState::Ready && account.state_cause == StateCause::Paired => {
                                    record_transition(&ledger, &account.id, AccountState::Unpaired, AccountState::Ready, account.state_cause, None, now)
                                }
                                None => {}
                            }
                            states.insert(account.id, observed);
                        }
                        // A held account the rows no longer carry was deleted: its last row, and the id forgotten so a
                        // replacement under it is a holder the mirror never saw.
                        states.retain(|id, held| {
                            let kept = carried.contains(id);
                            if !kept {
                                record_deletion(&ledger, id, held.state, now);
                                gone.insert(id.clone(), held.incarnation);
                            }
                            kept
                        });
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
            round: Box::new(ShedRoundFacts {
                window: "seven_day".into(),
                utilization: 0.5,
                projection: 0.97,
                horizon: 19_000,
                ceiling: 0.95,
                unit: "window_share",
                burn_average_seconds: 600,
                shed_cap: 0.5,
                burn_sum: 5.0,
                excess: 3.0,
                target: 2.5,
                removed: 4.0,
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
                    rewrite: Some(0.01),
                    planned: Some("b".into()),
                    runway_ms: Some(7_200_000),
                    skip: None,
                    note: Some("current"),
                }],
                marks: vec![key.clone()],
                outcome: "marked 1".into(),
                reason: None,
                dwell_seconds: 900,
                dwell_until: 901_000,
            }),
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
            ("projection", json!(0.97)),
            ("horizon", json!(19_000)),
            ("ceiling", json!(0.95)),
            ("unit", json!("window_share")),
            ("burn_average_seconds", json!(600)),
            ("shed_cap", json!(0.5)),
            ("burn_sum", json!(5.0)),
            ("excess", json!(3.0)),
            ("target", json!(2.5)),
            ("removed", json!(4.0)),
            ("warm_sessions", json!("when_needed")),
            ("outcome", json!("marked 1")),
            ("reason", json!(null)),
            ("dwell_seconds", json!(900)),
            ("dwell_until", json!(901_000)),
            ("persisted", json!(true)),
        ] {
            assert_eq!(detail[field], value, "{field}");
        }
        for absent in ["ahead", "elapsed_share", "threshold", "trigger"] {
            assert!(
                detail.get(absent).is_none(),
                "{absent} is no member of the round: {detail}"
            );
        }
        let mut members: Vec<&str> = detail
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        members.sort_unstable();
        assert_eq!(
            members,
            [
                "burn_average_seconds",
                "burn_sum",
                "ceiling",
                "dwell_seconds",
                "dwell_until",
                "excess",
                "horizon",
                "marks",
                "outcome",
                "persisted",
                "pins",
                "projection",
                "reason",
                "removed",
                "shed_cap",
                "target",
                "unit",
                "utilization",
                "warm_sessions",
                "window",
            ],
            "the round's members and no other"
        );
        assert_eq!(
            detail["pins"],
            json!([{
                "key_id": "k", "session_id": "s1", "agent_id": "main", "model": "m", "cold": true,
                "burn": 4.0, "last_input_tokens": 1_000, "per_rewrite": 0.004, "marked": true,
                "rewrite": 0.01, "planned": "b", "runway_ms": 7_200_000, "skip": null,
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
            restored: false,
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
            restored: false,
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
                cause: StateCause::AuthFailed,
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
                parent_agent_id: None,
                session_id: None,
                request_class: None,
                canonical_model: "m".into(),
                lane: Default::default(),
                flags: Default::default(),
                chain: Default::default(),
                attempted: Vec::new(),
                holds: Vec::new(),
                client: ClientClass::ClaudeCode,
                client_version: None,
            },
            &snapshot,
            &RefusalFacts {
                kind: "client",
                code: "drift",
                reason: "hash".into(),
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
