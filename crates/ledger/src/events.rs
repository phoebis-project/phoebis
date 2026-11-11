//! Events that are not requests: what the router broadcasts, and what the registry and the credential
//! store record on their own, mirrored into one table so the UI has one timeline.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::{BackoffEdge, Clock, PollFault};
use phoebis_relay::CatalogChange;
use phoebis_routing::{term, CreditsState, RouterEvent, RouterHandle};
use phoebis_store::{
    AccountRow, AccountState, CooldownScope, PinKey, ResolvedAttempt, Settled, StateCause, Store,
};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::assemble::Record;
use crate::db::newest_row_at;
use crate::query::{
    ACCOUNT_STATE_CHANGED, CATALOG_CHANGED, COOLDOWN_CLEARED, COOLDOWN_SET, FAST_CREDITS_CHANGED,
    MARK_CLEARED, OVERAGE_ENDED, OVERAGE_STARTED, PIN_CREATED, PIN_EXPIRED, PIN_MIGRATED,
    PLAN_ENDED, PLAN_RESTORED, POLL_BACKOFF_ENDED, POLL_BACKOFF_STARTED, POLL_FAILED,
    REFRESH_FAILED, REFRESH_SUCCEEDED, REFRESH_UNCERTAIN, REGISTRY_CHANGED, RESERVATION_EXPIRED,
    SHEDDING_ROUND, SLOW_DOWN_ENDED, SLOW_DOWN_STARTED, WARM_PREFIX_EXPIRED, WINDOW_CLEARED,
    WINDOW_RESET, WINDOW_SPENT, WIND_DOWN_STARTED,
};
use crate::record::{CacheMapWrite, DecisionRow, EventRecord};
use crate::task::Ledger;

/// Rows read per mirror tick; a registry busier than this per tick is caught up on the next.
const MIRROR_BATCH: usize = 500;

/// A pin event's row over the four columns its `PinKey` fills.
fn pin_row(pin: &PinKey) -> EventRecord {
    EventRecord {
        agent_id: Some(pin.agent_id.clone()),
        key_id: Some(pin.key_id.clone()),
        model: Some(pin.model.clone()),
        session_id: Some(pin.session_id.clone()),
        ..Default::default()
    }
}

/// `None` for the three variants that land as no row of their own here: a `Decision` and a `CacheMapWritten`, which
/// the mirror maps to the ledger's own records, and an `AccountStateChanged`, a transition the mirror's account watch
/// reads from the registry.
pub fn event_of(event: &RouterEvent) -> Option<EventRecord> {
    let model_of = |scope: &CooldownScope| scope.model().map(str::to_string);
    let row = match event {
        RouterEvent::CooldownCleared {
            account_id,
            scope,
            cause,
            at,
        } => EventRecord {
            at: *at,
            kind: COOLDOWN_CLEARED.into(),
            account_id: Some(account_id.clone()),
            model: model_of(scope),
            detail: Some(json!({ "cause": cause, "scope": scope.to_string() }).to_string()),
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
            kind: COOLDOWN_SET.into(),
            account_id: Some(account_id.clone()),
            model: model_of(scope),
            detail: Some(
                json!({ "scope": scope.to_string(), "until": until, "reason": reason, "window": window, "source_request_id": source_request_id })
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
            kind: OVERAGE_ENDED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(json!({ "since": since }).to_string()),
            ..Default::default()
        },
        RouterEvent::OverageStarted { account_id, at } => EventRecord {
            at: *at,
            kind: OVERAGE_STARTED.into(),
            account_id: Some(account_id.clone()),
            ..Default::default()
        },
        RouterEvent::PinCreated {
            pin,
            account_id,
            epoch,
            at,
        } => EventRecord {
            at: *at,
            kind: PIN_CREATED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(json!({ "agent_id": pin.agent_id, "epoch": epoch }).to_string()),
            ..pin_row(pin)
        },
        RouterEvent::PinExpired {
            pin,
            account_id,
            reasons,
            at,
        } => EventRecord {
            at: *at,
            kind: PIN_EXPIRED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(json!({ "agent_id": pin.agent_id, "reasons": reasons }).to_string()),
            ..pin_row(pin)
        },
        RouterEvent::PinMigrated {
            pin,
            from_account,
            to_account,
            epoch,
            reason,
            reasons,
            fit,
            runway_ms,
            binding,
            cause,
            at,
        } => {
            let mut detail = json!({
                "agent_id": pin.agent_id,
                "from_account": from_account,
                "epoch": epoch,
                "reason": reason,
                "fit": fit,
                "runway_ms": runway_ms,
                "binding": binding,
                "cause": cause,
            });
            // Only a replaced pin's move holds a list, so a rule's move and a failover's landing keep their detail.
            if !reasons.is_empty() {
                detail["reasons"] = json!(reasons);
            }
            EventRecord {
                at: *at,
                kind: PIN_MIGRATED.into(),
                account_id: Some(to_account.clone()),
                detail: Some(detail.to_string()),
                ..pin_row(pin)
            }
        }
        RouterEvent::MarkCleared {
            account_id,
            pin,
            reason,
            marked_at,
            marked_on,
            marked_window,
            projection,
            stretch_ms,
            threshold_ms,
            at,
        } => EventRecord {
            at: *at,
            kind: MARK_CLEARED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "agent_id": pin.agent_id,
                    "reason": reason,
                    "marked_at": marked_at,
                    "marked_on": marked_on,
                    "marked_window": marked_window,
                    "projection": projection,
                    "stretch_ms": stretch_ms,
                    "threshold_ms": threshold_ms,
                })
                .to_string(),
            ),
            ..pin_row(pin)
        },
        RouterEvent::ReservationExpired {
            account_id,
            request_id,
            attempt,
            cause,
            shares,
            age_ms,
            at,
        } => EventRecord {
            at: *at,
            kind: RESERVATION_EXPIRED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({ "attempt": attempt, "cause": cause, "shares": shares, "age_ms": age_ms }).to_string(),
            ),
            request_id: Some(request_id.clone()),
            ..Default::default()
        },
        RouterEvent::SheddingRound {
            account_id,
            pool_id,
            request_id,
            pin,
            round,
            at,
        } => EventRecord {
            at: *at,
            kind: SHEDDING_ROUND.into(),
            account_id: Some(account_id.clone()),
            // The round's facts serialize as the detail's fields, the pins and the marks as arrays of objects. The
            // routing task writes every round to the registry with the decision's writes, so its marks and dwell
            // outlive a restart.
            detail: serde_json::to_value(round).ok().map(|mut detail| {
                detail["persisted"] = json!(true);
                detail.to_string()
            }),
            pool_id: Some(pool_id.clone()),
            request_id: Some(request_id.clone()),
            ..pin_row(pin)
        },
        RouterEvent::SlowDownStarted {
            account_id,
            pool_id,
            factor,
            stretch_ms,
            threshold_ms,
            demand,
            reset,
            at,
        } => EventRecord {
            at: *at,
            kind: SLOW_DOWN_STARTED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "factor": factor,
                    "stretch_ms": stretch_ms,
                    "threshold_ms": threshold_ms,
                    "demand": demand,
                    "reset": reset,
                })
                .to_string(),
            ),
            pool_id: Some(pool_id.clone()),
            ..Default::default()
        },
        RouterEvent::SlowDownEnded {
            account_id,
            pool_id,
            since,
            duration_ms,
            factor_min,
            held_ms,
            at,
        } => EventRecord {
            at: *at,
            kind: SLOW_DOWN_ENDED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "since": since,
                    "duration_ms": duration_ms,
                    "factor_min": factor_min,
                    "held_ms": held_ms,
                })
                .to_string(),
            ),
            pool_id: Some(pool_id.clone()),
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
                kind: FAST_CREDITS_CHANGED.into(),
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
            kind: WINDOW_SPENT.into(),
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
        RouterEvent::WindowCleared {
            account_id,
            window,
            utilization_before,
            utilization,
            reset,
            source,
            source_request_id,
            at,
        } => EventRecord {
            at: *at,
            kind: WINDOW_CLEARED.into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "window": window,
                    "utilization_before": utilization_before,
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
            plan,
            tier,
        } => EventRecord {
            at: *at,
            kind: WINDOW_RESET.into(),
            account_id: Some(account_id.clone()),
            detail: Some(
                json!({
                    "window": window,
                    "utilization_before": utilization_before,
                    "plan": plan,
                    "tier": tier,
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
            kind: WARM_PREFIX_EXPIRED.into(),
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
            kind: WIND_DOWN_STARTED.into(),
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
            POLL_BACKOFF_STARTED,
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
            cause,
        } => (
            *at,
            POLL_BACKOFF_ENDED,
            account_id,
            json!({
                "duration_ms": at - since,
                "attempts": attempts,
                "resumed_at": resumed_at,
                "cause": cause.as_str(),
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

/// `poll_failed` on the polled account, detail {endpoint, status, error, transport}.
pub fn poll_failed(account_id: &str, at: i64, fault: &PollFault) -> Record {
    Record::Event(EventRecord {
        at,
        kind: POLL_FAILED.into(),
        account_id: Some(account_id.to_string()),
        detail: Some(
            json!({
                "endpoint": fault.endpoint,
                "status": fault.status,
                "error": fault.text,
                "transport": fault.transport(),
            })
            .to_string(),
        ),
        ..Default::default()
    })
}

/// `plan_ended` on the account whose profile reports no plan, detail {source, organization_type, rate_limit_tier,
/// subscription_status}: `source` is `profile` for a profile read and `usage_403` for one a usage 403 made due, and the
/// other three are the profile document's words.
pub fn plan_ended(account_id: &str, at: i64, source: &str, profile: &serde_json::Value) -> Record {
    plan_event(PLAN_ENDED, account_id, at, source, profile)
}

/// `plan_restored` on the account whose plan-ended flag a profile with a plan or a pairing confirm cleared, detail
/// {source, organization_type, rate_limit_tier, subscription_status}: `source` is `profile` or `pairing`.
pub fn plan_restored(
    account_id: &str,
    at: i64,
    source: &str,
    profile: &serde_json::Value,
) -> Record {
    plan_event(PLAN_RESTORED, account_id, at, source, profile)
}

fn plan_event(
    kind: &str,
    account_id: &str,
    at: i64,
    source: &str,
    profile: &serde_json::Value,
) -> Record {
    let organization = &profile["organization"];
    Record::Event(EventRecord {
        at,
        kind: kind.into(),
        account_id: Some(account_id.to_string()),
        detail: Some(
            json!({
                "source": source,
                "organization_type": organization["organization_type"],
                "rate_limit_tier": organization["rate_limit_tier"],
                "subscription_status": organization["subscription_status"],
            })
            .to_string(),
        ),
        ..Default::default()
    })
}

/// `catalog_changed` on the account whose listing moved. The ids travel in the detail, since one row names many
/// models.
pub fn catalog_changed(change: &CatalogChange) -> Record {
    Record::Event(EventRecord {
        at: change.at,
        kind: CATALOG_CHANGED.into(),
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

impl From<&AccountRow> for Observed {
    fn from(account: &AccountRow) -> Observed {
        Observed {
            state: account.state,
            incarnation: account.incarnation,
            revision: account.revision,
        }
    }
}

/// The account states the mirror holds, from the registry's rows and the routing loop's events alike, so both sources
/// write an account's transitions by one set of rules.
struct AccountWatch {
    states: HashMap<String, Observed>,
    /// A deleted holder's incarnation, so an event about it that arrives after the tick wrote its last row is dropped
    /// rather than read as a holder the mirror never saw.
    gone: HashMap<String, u64>,
}

impl AccountWatch {
    /// Writes the rows an observation of `account_id` moves it through and holds it. At or below the held identity the
    /// observation is older than what the mirror already observed, by an earlier tick or by the routing loop's event,
    /// and writes nothing.
    fn observe(
        &mut self,
        ledger: &Ledger,
        account_id: String,
        observed: Observed,
        cause: StateCause,
        detail: Option<&str>,
        at: i64,
    ) {
        let held = self.states.get(&account_id).copied();
        if held.is_some_and(|held| !observed.newer_than(&held)) {
            return;
        }
        let to = observed.state;
        let opened = || {
            record_transition(
                ledger,
                &account_id,
                AccountState::Unpaired,
                to,
                cause,
                detail,
                at,
            )
        };
        match held {
            Some(held) if held.incarnation == observed.incarnation && held.state == to => {}
            Some(held) if held.incarnation == observed.incarnation => {
                record_transition(ledger, &account_id, held.state, to, cause, detail, at)
            }
            // The held holder is gone and another took its id before the mirror saw the gap.
            Some(held) => {
                record_deletion(ledger, &account_id, held.state, at);
                if to == AccountState::ReauthRequired {
                    opened();
                }
            }
            // A holder the mirror never saw: the registry's own row says it was created. One born marked records its
            // move from `unpaired`, as the row born paired below does, so its history opens the same way.
            None if to == AccountState::ReauthRequired => opened(),
            // A row born paired skipped `unpaired`: the transition a pairing on an existing row records is recorded for
            // it too, so the account's history opens the same way.
            None if to == AccountState::Ready && cause == StateCause::Paired => opened(),
            None => {}
        }
        self.states.insert(account_id, observed);
    }

    /// `observe` for the routing loop's event, dropped when it is about a holder the tick already wrote deleted.
    fn observe_event(
        &mut self,
        ledger: &Ledger,
        account_id: String,
        observed: Observed,
        cause: StateCause,
        detail: Option<&str>,
        at: i64,
    ) {
        let about_gone = self
            .gone
            .get(&account_id)
            .is_some_and(|&buried| observed.incarnation <= buried);
        if !about_gone {
            self.observe(ledger, account_id, observed, cause, detail, at);
        }
    }

    /// A held account `carried` no longer names was deleted: its last row, and the id forgotten so a replacement under
    /// it is a holder the mirror never saw.
    fn sweep(&mut self, ledger: &Ledger, carried: &HashSet<String>, now: i64) {
        self.states.retain(|id, held| {
            let kept = carried.contains(id);
            if !kept {
                record_deletion(ledger, id, held.state, now);
                self.gone.insert(id.clone(), held.incarnation);
            }
            kept
        });
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
    state_row(
        ledger,
        account_id,
        from,
        to.as_str(),
        cause.as_str(),
        detail,
        at,
    );
}

/// The account's last row: the registry holds no row under the identity the mirror held.
fn record_deletion(ledger: &Ledger, account_id: &str, from: AccountState, at: i64) {
    // `deleted` is the one `to` that names no account state: the row it would describe is gone.
    let deleted = StateCause::Deleted.as_str();
    state_row(ledger, account_id, from, deleted, deleted, None, at);
}

fn state_row(
    ledger: &Ledger,
    account_id: &str,
    from: AccountState,
    to: &str,
    cause: &str,
    detail: Option<&str>,
    at: i64,
) {
    ledger.record(Record::Event(EventRecord {
        at,
        kind: ACCOUNT_STATE_CHANGED.into(),
        account_id: Some(account_id.to_string()),
        detail: Some(
            json!({ "from": from.as_str(), "to": to, "cause": cause, "detail": detail })
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
        "cause": attempt.cause,
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
        } => term::wind_down_start(*ends_at, *wind_down_minutes) < down_since,
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
                            let observed = Observed::from(&a);
                            (a.id, observed)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

pub fn spawn_event_mirror(
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
        states,
    } = baseline.unwrap_or_else(|| MirrorBaseline::read(&store));
    let mut watch = AccountWatch {
        states,
        gone: HashMap::new(),
    };
    // Read before this run's writer lands anything, so it names the previous run's newest row.
    let down_since = match ledger.db().read_blocking(|conn| Ok(newest_row_at(conn)?)) {
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
                    // The mirror is the broadcast's one production subscriber, so once the slot's copy drops the unwrap
                    // takes the decision without a copy.
                    Ok(RouterEvent::Decision { summary, record, at }) => {
                        let row = DecisionRow::of(Arc::unwrap_or_clone(summary), Arc::unwrap_or_clone(record), at);
                        ledger.record(Record::Decision(Box::new(row)));
                    }
                    Ok(RouterEvent::CacheMapWritten { request_id, seq, attempt, entries, at }) => {
                        ledger.record(Record::CacheMap(CacheMapWrite { request_id, seq, attempt, entries, at }));
                    }
                    Ok(RouterEvent::AccountStateChanged { account_id, to, incarnation, revision, cause, detail, at }) => {
                        let observed = Observed { state: to, incarnation, revision };
                        watch.observe_event(&ledger, account_id, observed, cause, detail.as_deref(), at);
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
                                kind: REGISTRY_CHANGED.into(),
                                actor: Some(change.actor.clone()),
                                // Every account, key and pool change names its own subject as its target.
                                account_id: change.kind.starts_with("account_").then(|| change.target.clone()),
                                key_id: change.kind.starts_with("key_").then(|| change.target.clone()),
                                pool_id: change.kind.starts_with("pool_").then(|| change.target.clone()),
                                detail: Some(json!({
                                    "kind": change.kind,
                                    "target": change.target,
                                    "diff": serde_json::from_str::<serde_json::Value>(&change.diff_json)
                                        .unwrap_or_default(),
                                }).to_string()),
                                ..Default::default()
                            }));
                        }
                    }
                    if let Ok(attempts) = store.refresh_attempts_resolved_after(last_resolved.0, last_resolved.1, MIRROR_BATCH) {
                        for attempt in attempts {
                            last_resolved = (attempt.resolved_at, attempt.id);
                            let kind = match attempt.settled() {
                                Settled::Committed | Settled::Paired => REFRESH_SUCCEEDED,
                                Settled::Uncertain => REFRESH_UNCERTAIN,
                                Settled::Failed | Settled::Superseded => REFRESH_FAILED,
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
                            let observed = Observed::from(&account);
                            watch.observe(&ledger, account.id, observed, account.state_cause, None, now);
                        }
                        watch.sweep(&ledger, &carried, now);
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
    use phoebis_store::PinKey;

    #[test]
    fn a_pin_migrated_event_maps_its_cause_into_the_detail_or_null() {
        let moved = |cause| {
            let e = event_of(&RouterEvent::PinMigrated {
                pin: PinKey::new("k", "s", "main", "m"),
                from_account: "a".into(),
                to_account: "b".into(),
                epoch: 7,
                reason: "cooldown".into(),
                reasons: Vec::new(),
                fit: Some("fits"),
                runway_ms: None,
                binding: None,
                cause,
                at: 5,
            })
            .expect("a pin row");
            serde_json::from_str::<serde_json::Value>(e.detail.as_deref().unwrap()).unwrap()
        };
        let detail = moved(Some(phoebis_routing::MoveCause::Cooldown {
            scope: "account".into(),
            reason: "rate_limit".into(),
            window: Some("five_hour".into()),
        }));
        assert_eq!(
            detail["cause"],
            json!({ "scope": "account", "reason": "rate_limit", "window": "five_hour" }),
            "a failover's cause is one object"
        );
        assert_eq!(
            detail
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "agent_id",
                "from_account",
                "epoch",
                "reason",
                "fit",
                "runway_ms",
                "binding",
                "cause"
            ],
            "the cause follows the destination's fit"
        );
        let detail = moved(None);
        assert_eq!(
            detail.get("cause"),
            Some(&serde_json::Value::Null),
            "a rule's move writes the key as null"
        );
    }

    #[test]
    fn router_events_map_to_rows_with_the_ids_in_their_columns_and_the_rest_in_detail() {
        let e = event_of(&RouterEvent::PinMigrated {
            pin: PinKey::new("k", "s", "main", "m"),
            from_account: "a".into(),
            to_account: "b".into(),
            epoch: 7,
            reason: "cooldown".into(),
            reasons: Vec::new(),
            fit: Some("rewrite_only"),
            runway_ms: Some(5_400_000),
            binding: Some("five_hour".into()),
            cause: None,
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
            (&detail["fit"], &detail["runway_ms"], &detail["binding"]),
            (
                &json!("rewrite_only"),
                &json!(5_400_000),
                &json!("five_hour")
            ),
            "the destination's fit as the move read it"
        );
        assert_eq!(
            e.agent_id.as_deref(),
            Some("main"),
            "the pin's agent has its own column beside the detail's copy"
        );
        let e = event_of(&RouterEvent::CooldownSet {
            account_id: "a".into(),
            scope: CooldownScope::Model("m".into()),
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
            scope: CooldownScope::Model("m".into()),
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
                    scope: CooldownScope::Account,
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
                    pin: PinKey::new("k", "s", "main", "m"),
                    account_id: "a".into(),
                    epoch: 1,
                    at: 1,
                },
                "pin_created",
            ),
            (
                RouterEvent::PinExpired {
                    pin: PinKey::new("k", "s", "main", "m"),
                    account_id: "a".into(),
                    reasons: vec!["expired"],
                    at: 1,
                },
                "pin_expired",
            ),
            (
                RouterEvent::SlowDownStarted {
                    account_id: "a".into(),
                    pool_id: "p".into(),
                    factor: 0.5,
                    stretch_ms: 1,
                    threshold_ms: 2,
                    demand: 0.1,
                    reset: None,
                    at: 1,
                },
                "slow_down_started",
            ),
            (
                RouterEvent::SlowDownEnded {
                    account_id: "a".into(),
                    pool_id: "p".into(),
                    since: 0,
                    duration_ms: 1,
                    factor_min: 0.5,
                    held_ms: 0,
                    at: 1,
                },
                "slow_down_ended",
            ),
        ] {
            assert_eq!(event_of(&event).map(|row| row.kind), Some(kind.to_string()));
        }
        let row = event_of(&RouterEvent::PinExpired {
            pin: PinKey::new("k", "s", "main", "m"),
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
            pin: PinKey::new("k", "s1", "main", "m"),
            round: Box::new(ShedRoundFacts {
                window: "seven_day".into(),
                utilization: 0.5,
                projection: 0.97,
                horizon: 19_000,
                ceiling: 0.95,
                trigger: "slow_down",
                stretch_ms: Some(2_520_000),
                threshold_ms: Some(1_800_000),
                factor: Some(0.3),
                unit: "window_share",
                rewrite_unit: "pro_week",
                burn_average_seconds: 600,
                shed_cap: 0.5,
                burn_sum: 5.0,
                excess: 3.0,
                target: 2.5,
                mark_share: 0.1,
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
                    rewrite_window: Some("seven_day"),
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
            ("trigger", json!("slow_down")),
            ("stretch_ms", json!(2_520_000)),
            ("threshold_ms", json!(1_800_000)),
            ("factor", json!(0.3)),
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
        for absent in ["ahead", "elapsed_share", "threshold"] {
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
                "factor",
                "horizon",
                "mark_share",
                "marks",
                "outcome",
                "persisted",
                "pins",
                "projection",
                "reason",
                "removed",
                "rewrite_unit",
                "shed_cap",
                "stretch_ms",
                "target",
                "threshold_ms",
                "trigger",
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
                "rewrite": 0.01, "rewrite_window": "seven_day", "planned": "b",
                "runway_ms": 7_200_000, "skip": null, "note": "current"
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

        let row = event_of(&RouterEvent::WindowCleared {
            account_id: "a".into(),
            window: "five_hour".into(),
            utilization_before: 1.0,
            utilization: 0.0,
            reset: Some(9_000),
            source: WindowSource::Head,
            source_request_id: Some("r3".into()),
            at: 4_000,
        })
        .expect("a cleared row");
        assert_eq!(
            columns(&row),
            (
                "window_cleared",
                Some("a"),
                None,
                None,
                None,
                None,
                None,
                Some("r3")
            ),
            "the head that read the window under its limit names the request"
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "window": "five_hour", "utilization_before": 1.0, "utilization": 0.0, "reset": 9_000,
                "source": "head", "source_request_id": "r3"
            }),
            "the readings and the reset alone, with no cause"
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
            plan: None,
            tier: None,
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
                "observed": "clock", "plan": null, "tier": null
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

        let row = event_of(&RouterEvent::MarkCleared {
            account_id: "a".into(),
            pin: PinKey::new("k", "s1", "sub-1", "m"),
            reason: "healed",
            marked_at: 1_000,
            marked_on: "a".into(),
            marked_window: "five_hour".into(),
            projection: Some(0.91),
            stretch_ms: Some(1_440_000),
            threshold_ms: Some(1_800_000),
            at: 6_000,
        })
        .expect("a mark row");
        assert_eq!(
            (row.at, columns(&row)),
            (
                6_000,
                (
                    "mark_cleared",
                    Some("a"),
                    None,
                    Some("k"),
                    Some("s1"),
                    Some("sub-1"),
                    Some("m"),
                    None
                )
            )
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "agent_id": "sub-1", "reason": "healed", "marked_at": 1_000, "marked_on": "a",
                "marked_window": "five_hour", "projection": 0.91, "stretch_ms": 1_440_000,
                "threshold_ms": 1_800_000
            })
        );

        let row = event_of(&RouterEvent::ReservationExpired {
            account_id: "a".into(),
            request_id: "r3".into(),
            attempt: 2,
            cause: "canceled",
            shares: [
                ("five_hour".to_string(), 0.104),
                ("seven_day".to_string(), 0.012),
            ]
            .into_iter()
            .collect(),
            age_ms: 4_500,
            at: 7_000,
        })
        .expect("a reservation row");
        assert_eq!(
            (row.at, columns(&row)),
            (
                7_000,
                (
                    "reservation_expired",
                    Some("a"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("r3")
                )
            ),
            "the request whose booking left names the row"
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "attempt": 2, "cause": "canceled", "shares": {"five_hour": 0.104, "seven_day": 0.012},
                "age_ms": 4_500
            })
        );

        let row = event_of(&RouterEvent::WindDownStarted {
            account_id: "a".into(),
            pool_id: "p".into(),
            ends_at: 9_000,
            wind_down_minutes: 30,
            end_source: "canceled",
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
            json!({"ends_at": 9_000, "wind_down_minutes": 30, "end_source": "canceled"})
        );

        let row = event_of(&RouterEvent::SlowDownStarted {
            account_id: "a".into(),
            pool_id: "p".into(),
            factor: 0.5,
            stretch_ms: 2_520_000,
            threshold_ms: 1_800_000,
            demand: 0.12,
            reset: Some(9_000_000),
            at: 8_000,
        })
        .expect("a slow-down row");
        assert_eq!(
            (row.at, columns(&row)),
            (
                8_000,
                (
                    "slow_down_started",
                    Some("a"),
                    Some("p"),
                    None,
                    None,
                    None,
                    None,
                    None
                )
            )
        );
        assert_eq!(
            detail_of(&row),
            json!({
                "factor": 0.5, "stretch_ms": 2_520_000, "threshold_ms": 1_800_000, "demand": 0.12,
                "reset": 9_000_000
            })
        );

        let row = event_of(&RouterEvent::SlowDownEnded {
            account_id: "a".into(),
            pool_id: "p".into(),
            since: 8_000,
            duration_ms: 2_700_000,
            factor_min: 0.35,
            held_ms: 600_000,
            at: 2_828_000,
        })
        .expect("a slow-down row");
        assert_eq!(
            (row.at, columns(&row)),
            (
                2_828_000,
                (
                    "slow_down_ended",
                    Some("a"),
                    Some("p"),
                    None,
                    None,
                    None,
                    None,
                    None
                )
            )
        );
        assert_eq!(
            detail_of(&row),
            json!({"since": 8_000, "duration_ms": 2_700_000, "factor_min": 0.35, "held_ms": 600_000})
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
                summary: Arc::new(summary),
                record: Arc::new(record),
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
