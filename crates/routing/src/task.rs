//! The router task: one loop that owns the routing state, answers selections over a channel, reloads the
//! registry snapshot on request, pulls utilization samples in the background, and writes state through a
//! single writer so the store sees every change in the order the loop made it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use phoebis_oauth::{Clock, UsageService, UsageSnapshot};
use phoebis_store::{AccountState, CooldownRow, PinRow, Store, StoreError};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::select::{select, Exhaustion, MigrateReason, SelectRequest, SelectResult};
use crate::state::{
    EvidenceSource, OverageEvidence, PinRef, RoutingState, StateWrite, UtilizationSample,
};
use crate::{resolve, RegistrySnapshot};

const COMMANDS: usize = 4_096;

const EVENTS: usize = 1_024;

/// How long after an invalidation the next pin for the same triple counts as a migration rather than a new
/// session. A failover re-selects within one request, well inside this.
const MIGRATION_WINDOW_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq)]
pub enum RouterEvent {
    CooldownCleared {
        account_id: String,
        scope: String,
        /// What released the row: `admin`, `expired`, or `recovered`.
        cause: &'static str,
        at: i64,
    },
    CooldownSet {
        account_id: String,
        scope: String,
        until: i64,
        reason: String,
        window: String,
        source_request_id: Option<String>,
        at: i64,
    },
    OverageEnded {
        account_id: String,
        since: Option<i64>,
        at: i64,
    },
    OverageStarted {
        account_id: String,
        at: i64,
    },
    PinCreated {
        key_id: String,
        session_id: String,
        model: String,
        account_id: String,
        epoch: u64,
        at: i64,
    },
    PinExpired {
        key_id: String,
        session_id: String,
        model: String,
        account_id: String,
        at: i64,
    },
    PinMigrated {
        key_id: String,
        session_id: String,
        model: String,
        from_account: String,
        to_account: String,
        epoch: u64,
        reason: String,
        at: i64,
    },
    PinReleased {
        key_id: String,
        session_id: String,
        model: String,
        account_id: String,
        at: i64,
    },
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub purge_interval: Duration,
    pub sample_ttl: Duration,
    pub touch_interval: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            purge_interval: Duration::from_secs(60),
            sample_ttl: Duration::from_secs(60),
            touch_interval: Duration::from_secs(60),
        }
    }
}

pub struct SelectAnswer {
    pub result: SelectResult,
    pub cancel: CancellationToken,
}

#[derive(Clone, Debug)]
pub enum Report {
    AccountState {
        account_id: String,
        state: AccountState,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    Cooldown {
        account_id: String,
        row: CooldownRow,
        pin: Option<PinRef>,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    Overage {
        account_id: String,
        evidence: OverageEvidence,
    },
    PinInvalidated {
        account_id: String,
        pin: PinRef,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    PinRenewed {
        pin: PinRef,
        now: i64,
    },
    Sample {
        account_id: String,
        sample: UtilizationSample,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("the router task has stopped")]
pub struct RouterGone;

#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    #[error(transparent)]
    Gone(#[from] RouterGone),
    #[error("the registry could not be loaded: {0}")]
    Load(String),
}

enum Command {
    ClearCooldown {
        account_id: String,
        scope: String,
        reply: oneshot::Sender<bool>,
    },
    Cooldowns {
        reply: oneshot::Sender<Vec<CooldownRow>>,
    },
    Overage {
        account_id: String,
        reply: oneshot::Sender<Option<OverageEvidence>>,
    },
    Overages {
        reply: oneshot::Sender<Vec<(String, OverageEvidence)>>,
    },
    Pins {
        reply: oneshot::Sender<Vec<PinRow>>,
    },
    ReleaseSession {
        key_id: String,
        session_id: String,
        reply: oneshot::Sender<usize>,
    },
    Reload {
        reply: oneshot::Sender<Result<(), String>>,
    },
    Report(Report),
    Samples {
        reply: oneshot::Sender<Vec<(String, UtilizationSample)>>,
    },
    Select {
        request: SelectRequest,
        reply: oneshot::Sender<SelectAnswer>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    Touch {
        key_id: String,
    },
}

#[derive(Clone)]
pub struct RouterHandle {
    events: broadcast::Sender<RouterEvent>,
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    tx: mpsc::Sender<Command>,
}

impl RouterHandle {
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone()
    }

    /// What changed in the routing state, as it changes. A receiver that falls behind loses the oldest
    /// events and is told so; the task never waits on a receiver.
    pub fn subscribe_events(&self) -> broadcast::Receiver<RouterEvent> {
        self.events.subscribe()
    }

    pub async fn select(&self, request: SelectRequest) -> Result<SelectAnswer, RouterGone> {
        self.ask(|reply| Command::Select { request, reply }).await
    }

    /// Never waits: a report is a fact about the past, and a relay holding a client stream must not block on
    /// the task's queue. A full queue is logged and the report dropped, which delays a state change by one
    /// request rather than stalling one.
    pub fn report(&self, report: Report) {
        match self.tx.try_send(Command::Report(report)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("the router task's queue is full, a report was dropped")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("the router task has stopped, a report was dropped")
            }
        }
    }

    pub async fn reload(&self) -> Result<(), ReloadError> {
        self.ask(|reply| Command::Reload { reply })
            .await?
            .map_err(ReloadError::Load)
    }
    /// The first call drains the queue and the writer, then answers `Ok(())`. A later one answers
    /// `Err(RouterGone)`, because the task it would ask has already stopped.
    pub async fn shutdown(&self) -> Result<(), RouterGone> {
        self.ask(|reply| Command::Shutdown { reply }).await
    }
    pub async fn release_session(
        &self,
        key_id: &str,
        session_id: &str,
    ) -> Result<usize, RouterGone> {
        let (k, s) = (key_id.to_string(), session_id.to_string());
        self.ask(|reply| Command::ReleaseSession {
            key_id: k,
            session_id: s,
            reply,
        })
        .await
    }
    pub async fn clear_cooldown(&self, account_id: &str, scope: &str) -> Result<bool, RouterGone> {
        let (a, s) = (account_id.to_string(), scope.to_string());
        self.ask(|reply| Command::ClearCooldown {
            account_id: a,
            scope: s,
            reply,
        })
        .await
    }
    pub async fn pins(&self) -> Result<Vec<PinRow>, RouterGone> {
        self.ask(|reply| Command::Pins { reply }).await
    }
    pub async fn cooldowns(&self) -> Result<Vec<CooldownRow>, RouterGone> {
        self.ask(|reply| Command::Cooldowns { reply }).await
    }
    /// One ask for the whole map: a scrape must not queue one command per account in front of selection.
    pub async fn overages(&self) -> Result<Vec<(String, OverageEvidence)>, RouterGone> {
        self.ask(|reply| Command::Overages { reply }).await
    }
    pub async fn overage(&self, account_id: &str) -> Result<Option<OverageEvidence>, RouterGone> {
        let a = account_id.to_string();
        self.ask(|reply| Command::Overage {
            account_id: a,
            reply,
        })
        .await
    }
    /// One ask for every account's latest utilization sample, so a rule sweep never queues one command per
    /// account in front of selection.
    pub async fn samples(&self) -> Result<Vec<(String, UtilizationSample)>, RouterGone> {
        self.ask(|reply| Command::Samples { reply }).await
    }
    pub fn touch_key(&self, key_id: &str) {
        let _ = self.tx.try_send(Command::Touch {
            key_id: key_id.to_string(),
        });
    }

    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, RouterGone> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(make(reply)).await.map_err(|_| RouterGone)?;
        rx.await.map_err(|_| RouterGone)
    }
}

struct Task {
    cancel: HashMap<String, CancellationToken>,
    clock: Clock,
    config: RouterConfig,
    events: broadcast::Sender<RouterEvent>,
    /// The account a triple's pin was invalidated from, why, and when. The next select for that triple
    /// within `MIGRATION_WINDOW_MS` reports a migration rather than a fresh pin; older entries go on purge.
    invalidated: HashMap<(String, String, String), (String, &'static str, i64)>,
    revision: u64,
    sampled_at: HashMap<String, i64>,
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    state: RoutingState,
    store: Arc<Store>,
    touched_at: HashMap<String, i64>,
    /// Weak on purpose: the loop ends when the last handle drops, so a test that reopens an on-disk store
    /// gets the process lock back; the task's own follow-ups upgrade it and give up when nobody is left.
    tx: mpsc::WeakSender<Command>,
    usage: Option<Arc<UsageService>>,
    writer: mpsc::UnboundedSender<Vec<StateWrite>>,
    /// Resolves once the writer has landed everything it was sent and exited; `shutdown` waits on it.
    writer_done: Option<oneshot::Receiver<()>>,
}

pub fn spawn(
    store: Arc<Store>,
    usage: Option<Arc<UsageService>>,
    clock: Clock,
    config: RouterConfig,
) -> Result<RouterHandle, StoreError> {
    let now = clock();
    store.purge_cooldowns_before(now)?;
    let snapshot = Arc::new(RwLock::new(Arc::new(RegistrySnapshot::from_load(
        store.load_registry()?,
        store.digest_key(),
        now,
        1,
    ))));
    let mut state = RoutingState::from_load(store.load_routing()?);
    // Selection ages a full-window reading that dates no refund on the same interval the pulls below run on.
    state.set_sample_ttl_ms(config.sample_ttl.as_millis() as i64);
    let (tx, mut rx) = mpsc::channel(COMMANDS);
    // Unbounded so the loop never blocks on SQLite: a stalled store grows this queue in memory rather than
    // stalling selection, and bounding the process's memory is the operator's call, not this channel's.
    let (writer, mut writes) = mpsc::unbounded_channel::<Vec<StateWrite>>();
    let (done_tx, done_rx) = oneshot::channel();
    let (events, _) = broadcast::channel(EVENTS);
    let handle = RouterHandle {
        events: events.clone(),
        snapshot: snapshot.clone(),
        tx: tx.clone(),
    };
    let writer_store = store.clone();
    // Weak for the same reason the task's own sender is: a strong one here would keep the loop running after
    // the last handle is gone, and with it the process lock a reopened on-disk store needs back.
    let writer_tx = tx.downgrade();
    tokio::spawn(async move {
        // One batch at a time, in order: two blocking writes racing on separate threads could land a delete
        // before the upsert it was meant to follow.
        while let Some(batch) = writes.recv().await {
            let store = writer_store.clone();
            let size = batch.len();
            match tokio::task::spawn_blocking(move || apply(&store, batch)).await {
                // An account-state mark that moved the row changes what the snapshot has to say, so the
                // loop is asked for a reload once the write is on disk rather than before.
                Ok(Ok(moved)) => {
                    if moved {
                        if let Some(tx) = writer_tx.upgrade() {
                            let (reply, _rx) = oneshot::channel();
                            let _ = tx.send(Command::Reload { reply }).await;
                        }
                    }
                }
                Ok(Err(error)) => {
                    tracing::error!(%error, batch = size, "a routing state write failed")
                }
                Err(error) => {
                    tracing::error!(%error, batch = size, "a routing state write panicked, its batch is lost")
                }
            }
        }
        let _ = done_tx.send(());
    });
    let mut task = Task {
        cancel: HashMap::new(),
        clock,
        config,
        events,
        invalidated: HashMap::new(),
        revision: 1,
        sampled_at: HashMap::new(),
        snapshot,
        state,
        store,
        touched_at: HashMap::new(),
        tx: tx.downgrade(),
        usage,
        writer,
        writer_done: Some(done_rx),
    };
    drop(tx);
    tokio::spawn(async move {
        let mut purge = tokio::time::interval(task.config.purge_interval);
        purge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = rx.recv() => match command {
                    Some(Command::Shutdown { reply }) => {
                        // Nothing queued behind the shutdown is served: the handles that sent it are done.
                        rx.close();
                        while let Some(command) = rx.recv().await { if let Command::Report(report) = command { task.report(report); } }
                        task.drain_writer().await;
                        let _ = reply.send(());
                        break;
                    }
                    Some(command) => task.handle(command).await,
                    None => { task.drain_writer().await; break; }
                },
                _ = purge.tick() => task.purge(),
            }
        }
    });
    Ok(handle)
}

/// Reports whether any account-state mark in the batch actually moved its row, which is the one write here
/// that the registry snapshot has to be rebuilt from.
fn apply(store: &Store, batch: Vec<StateWrite>) -> Result<bool, StoreError> {
    let mut moved = false;
    for write in batch {
        match write {
            StateWrite::AccountState {
                account_id,
                state,
                lifecycle_generation,
                credential_generation,
            } => {
                moved |= store.set_state_if_generations(
                    &account_id,
                    state,
                    lifecycle_generation,
                    credential_generation,
                )?;
            }
            StateWrite::DeleteCooldown(account_id, scope) => {
                store.delete_cooldown(&account_id, &scope)?;
            }
            StateWrite::DeletePin(key_id, session_id, model) => {
                store.delete_pin(&key_id, &session_id, &model)?;
            }
            StateWrite::UpsertCooldown(row) => store.upsert_cooldown(&row)?,
            StateWrite::UpsertOverage(row) => {
                store.upsert_overage(&row)?;
            }
            StateWrite::UpsertPin(row) => store.upsert_pin(&row)?,
        }
    }
    Ok(moved)
}

impl Task {
    /// A send with no receiver is an error and means nothing: the loop must never depend on anyone listening.
    fn emit(&self, event: RouterEvent) {
        let _ = self.events.send(event);
    }

    /// Dropping the sender ends the writer's loop; waiting on `writer_done` is what makes an acknowledged pin or
    /// cooldown durable before the process exits.
    async fn drain_writer(&mut self) {
        let (tx, _) = mpsc::unbounded_channel::<Vec<StateWrite>>();
        let writer = std::mem::replace(&mut self.writer, tx);
        drop(writer);
        if let Some(done) = self.writer_done.take() {
            let _ = done.await;
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Select { mut request, reply } => {
                let snapshot = self
                    .snapshot
                    .read()
                    .expect("snapshot lock poisoned")
                    .clone();
                if request.revision < snapshot.revision {
                    // The registry moved after admission: the key, its policy, the pool, and the candidates are
                    // re-derived from what is current, and a request the current registry would refuse is refused.
                    let now = (self.clock)();
                    let Ok(key) = snapshot.authenticate_id(&request.key_id, now) else {
                        let _ = reply.send(SelectAnswer {
                            result: SelectResult::Exhausted(Exhaustion::Untimed),
                            cancel: CancellationToken::new(),
                        });
                        return;
                    };
                    match resolve(&snapshot, key, &request.canonical_model) {
                        Ok(candidates) => request.candidates = candidates,
                        Err(refusal) => {
                            let _ = reply.send(SelectAnswer {
                                result: SelectResult::Refused(refusal),
                                cancel: CancellationToken::new(),
                            });
                            return;
                        }
                    }
                    request.revision = snapshot.revision;
                }
                self.pull_samples(&request);
                let triple = request.session_id.as_ref().map(|s| {
                    (
                        request.key_id.clone(),
                        s.clone(),
                        request.canonical_model.clone(),
                    )
                });
                let before = triple
                    .as_ref()
                    .and_then(|(k, s, m)| self.state.pin(k, s, m).cloned());
                let (result, writes) =
                    select(&mut self.state, &snapshot, &request, &mut rand::rng());
                self.write(writes);
                if let Some((k, s, m)) = &triple {
                    let after = self.state.pin(k, s, m).cloned();
                    let now = request.now;
                    match (before, after, &result) {
                        (None, Some(new), SelectResult::Selected(_)) => {
                            match self
                                .invalidated
                                .remove(&(k.clone(), s.clone(), m.clone()))
                                .filter(|(_, _, at)| now - at <= MIGRATION_WINDOW_MS)
                            {
                                Some((from, reason, _)) => {
                                    self.emit(RouterEvent::PinMigrated {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        model: m.clone(),
                                        from_account: from,
                                        to_account: new.account_id.clone(),
                                        epoch: new.epoch,
                                        reason: reason.into(),
                                        at: now,
                                    });
                                }
                                None => self.emit(RouterEvent::PinCreated {
                                    key_id: k.clone(),
                                    session_id: s.clone(),
                                    model: m.clone(),
                                    account_id: new.account_id.clone(),
                                    epoch: new.epoch,
                                    at: now,
                                }),
                            }
                        }
                        (Some(old), Some(new), SelectResult::Selected(selection))
                            if old.epoch != new.epoch =>
                        {
                            let reason = match selection.migrated {
                                Some(MigrateReason::Overage) => "overage",
                                Some(MigrateReason::Spent) => "spent",
                                Some(MigrateReason::Utilization) => "utilization",
                                None => "expired",
                            };
                            if reason == "expired" {
                                self.emit(RouterEvent::PinExpired {
                                    key_id: k.clone(),
                                    session_id: s.clone(),
                                    model: m.clone(),
                                    account_id: old.account_id.clone(),
                                    at: now,
                                });
                                self.emit(RouterEvent::PinCreated {
                                    key_id: k.clone(),
                                    session_id: s.clone(),
                                    model: m.clone(),
                                    account_id: new.account_id.clone(),
                                    epoch: new.epoch,
                                    at: now,
                                });
                            } else {
                                self.emit(RouterEvent::PinMigrated {
                                    key_id: k.clone(),
                                    session_id: s.clone(),
                                    model: m.clone(),
                                    from_account: old.account_id.clone(),
                                    to_account: new.account_id.clone(),
                                    epoch: new.epoch,
                                    reason: reason.into(),
                                    at: now,
                                });
                            }
                        }
                        (Some(old), None, _) => self.emit(RouterEvent::PinExpired {
                            key_id: k.clone(),
                            session_id: s.clone(),
                            model: m.clone(),
                            account_id: old.account_id.clone(),
                            at: now,
                        }),
                        _ => {}
                    }
                }
                let cancel = match &result {
                    SelectResult::Selected(selection) => {
                        self.token(&selection.account.id).child_token()
                    }
                    SelectResult::Exhausted(_) | SelectResult::Refused(_) => {
                        CancellationToken::new()
                    }
                };
                let _ = reply.send(SelectAnswer { result, cancel });
            }
            Command::Report(report) => self.report(report),
            Command::Reload { reply } => {
                let _ = reply.send(self.reload().await);
            }
            Command::Shutdown { .. } => unreachable!("the loop takes shutdown before handle"),
            Command::ReleaseSession {
                key_id,
                session_id,
                reply,
            } => {
                // The account a pin names is only readable while it is still held, so the rows come first.
                let held: HashMap<(String, String, String), String> = self
                    .state
                    .pins()
                    .into_iter()
                    .filter(|p| p.key_id == key_id && p.session_id == session_id)
                    .map(|p| ((p.key_id, p.session_id, p.model), p.account_id))
                    .collect();
                let gone = self.state.release_session(&key_id, &session_id);
                let count = gone.len();
                let now = (self.clock)();
                for triple in &gone {
                    if let Some(account_id) = held.get(triple) {
                        self.emit(RouterEvent::PinReleased {
                            key_id: triple.0.clone(),
                            session_id: triple.1.clone(),
                            model: triple.2.clone(),
                            account_id: account_id.clone(),
                            at: now,
                        });
                    }
                }
                self.write(
                    gone.into_iter()
                        .map(|(k, s, m)| StateWrite::DeletePin(k, s, m))
                        .collect(),
                );
                let _ = reply.send(count);
            }
            Command::ClearCooldown {
                account_id,
                scope,
                reply,
            } => {
                let cleared = self.release_cooldown(account_id, scope, "admin");
                let _ = reply.send(cleared);
            }
            Command::Pins { reply } => {
                let _ = reply.send(self.state.pins());
            }
            Command::Cooldowns { reply } => {
                let _ = reply.send(self.state.cooldowns());
            }
            Command::Overages { reply } => {
                let _ = reply.send(self.state.overages());
            }
            Command::Overage { account_id, reply } => {
                let _ = reply.send(self.state.overage(&account_id).cloned());
            }
            Command::Samples { reply } => {
                let _ = reply.send(self.state.samples());
            }
            Command::Touch { key_id } => {
                let now = (self.clock)();
                let due = self
                    .touched_at
                    .get(&key_id)
                    .is_none_or(|at| now - at >= self.config.touch_interval.as_millis() as i64);
                if due {
                    self.touched_at.insert(key_id.clone(), now);
                    let store = self.store.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = store.touch_api_key(&key_id, now);
                    });
                }
            }
        }
    }

    fn report(&mut self, report: Report) {
        match report {
            Report::Cooldown {
                account_id,
                mut row,
                pin,
                lifecycle_generation,
                credential_generation,
            } => {
                if self.superseded(&account_id, lifecycle_generation, credential_generation) {
                    return;
                }
                // The router's own clock dates the row, so a sample is compared against the loop's timeline
                // rather than the relay's.
                row.set_at = (self.clock)();
                let mut writes = Vec::new();
                if self.state.record_cooldown(row.clone()) {
                    self.emit(RouterEvent::CooldownSet {
                        account_id: account_id.clone(),
                        scope: row.scope.clone(),
                        until: row.until,
                        reason: row.reason.clone(),
                        window: row.window.clone(),
                        source_request_id: row.source_request_id.clone(),
                        at: (self.clock)(),
                    });
                    writes.push(StateWrite::UpsertCooldown(row));
                }
                if let Some(pin) = pin {
                    if self.state.invalidate_pin(&pin, &account_id) {
                        self.invalidated.insert(
                            (
                                pin.key_id.clone(),
                                pin.session_id.clone(),
                                pin.model.clone(),
                            ),
                            (account_id.clone(), "cooldown", (self.clock)()),
                        );
                        writes.push(StateWrite::DeletePin(pin.key_id, pin.session_id, pin.model));
                    }
                }
                self.write(writes);
            }
            Report::Overage {
                account_id,
                evidence,
            } => {
                let at = evidence.observed_at;
                let (was, since) = match self.state.overage(&account_id) {
                    Some(current) => (current.in_use, current.since),
                    None => (false, None),
                };
                if self.state.record_overage(&account_id, evidence) {
                    match (was, self.state.in_overage(&account_id)) {
                        (false, true) => self.emit(RouterEvent::OverageStarted {
                            account_id: account_id.clone(),
                            at,
                        }),
                        (true, false) => self.emit(RouterEvent::OverageEnded {
                            account_id: account_id.clone(),
                            since,
                            at,
                        }),
                        _ => {}
                    }
                    // The row is built from the normalized state, not the incoming evidence: `since` is what the
                    // state kept across consecutive in-use heads.
                    if let Some(current) = self.state.overage(&account_id) {
                        self.write(vec![StateWrite::UpsertOverage(phoebis_store::OverageRow {
                            account_id: account_id.clone(),
                            in_use: current.in_use,
                            since: current.since,
                            reset: current.reset,
                            observed_at: current.observed_at,
                            source: current.source.as_str().into(),
                        })]);
                    }
                }
            }
            Report::PinInvalidated {
                account_id,
                pin,
                lifecycle_generation,
                credential_generation,
            } => {
                if self.superseded(&account_id, lifecycle_generation, credential_generation) {
                    return;
                }
                if self.state.invalidate_pin(&pin, &account_id) {
                    self.invalidated.insert(
                        (
                            pin.key_id.clone(),
                            pin.session_id.clone(),
                            pin.model.clone(),
                        ),
                        (account_id.clone(), "mark", (self.clock)()),
                    );
                    self.write(vec![StateWrite::DeletePin(
                        pin.key_id,
                        pin.session_id,
                        pin.model,
                    )]);
                }
            }
            Report::PinRenewed { pin, now } => {
                if let Some(current) = self
                    .state
                    .pin(&pin.key_id, &pin.session_id, &pin.model)
                    .cloned()
                {
                    if current.epoch == pin.epoch {
                        let renewed = PinRow {
                            last_used_at: now,
                            ..current
                        };
                        self.state.put_pin(renewed.clone());
                        self.write(vec![StateWrite::UpsertPin(renewed)]);
                    }
                }
            }
            Report::Sample { account_id, sample } => {
                // A cooled account sends no requests, so a window the upstream reset ahead of the deadline it
                // announced reaches the router here and nowhere else. Only a sample newer than the stored one
                // answers for the account now: two pulls can be in flight, and the older one's reading is one
                // the window has already moved past.
                if self.state.record_sample(&account_id, sample.clone()) {
                    let now = (self.clock)();
                    for row in self.state.recovered_cooldowns(&account_id, &sample, now) {
                        self.release_cooldown(row.account_id, row.scope, "recovered");
                    }
                }
            }
            Report::AccountState {
                account_id,
                state,
                lifecycle_generation,
                credential_generation,
            } => {
                // The relay decided; the store is the source the snapshot is rebuilt from, so the write comes
                // first and the reload follows once it has landed. The write is fenced by both generations:
                // a verdict about a credential that was replaced, or a slot that was disconnected, lands
                // nowhere. It goes through the writer every other routing write uses, so shutdown drains it
                // rather than leaving a mark in a detached task the runtime may drop unpolled.
                self.write(vec![StateWrite::AccountState {
                    account_id,
                    state,
                    lifecycle_generation,
                    credential_generation,
                }]);
            }
        }
    }

    /// Whether a report is about a slot or a credential the registry has moved past. A delayed rejection
    /// carries the pair its own attempt went out with, and acting on it would cool an account, or end a
    /// session's affinity, over entitlements that have since been replaced.
    ///
    /// The snapshot's credential generation lags a rotation by one reload, so a report naming a generation it
    /// has not seen yet is applied: a false drop loses a cooldown the registry wanted, while a false keep
    /// records one a moment before the reload would have made it obviously right.
    fn superseded(
        &self,
        account_id: &str,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> bool {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let Some(account) = snapshot.account(account_id) else {
            tracing::debug!(account = %account_id, "a report named an account the registry no longer holds");
            return true;
        };
        if account.lifecycle_generation != lifecycle_generation {
            tracing::debug!(account = %account_id, "a report named a lifecycle the account has moved past");
            return true;
        }
        if snapshot
            .credential_generation(account_id)
            .is_some_and(|current| current > credential_generation)
        {
            tracing::debug!(account = %account_id, "a report named a credential the account has replaced");
            return true;
        }
        false
    }

    async fn reload(&mut self) -> Result<(), String> {
        let store = self.store.clone();
        let now = (self.clock)();
        let loaded = tokio::task::spawn_blocking(move || {
            store.load_registry().map(|load| (load, store.digest_key()))
        })
        .await;
        let (load, digest_key) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                tracing::error!(%error, "the registry could not be reloaded; the previous snapshot stays in force");
                return Err(error.to_string());
            }
            Err(error) => {
                tracing::error!(%error, "the registry load panicked; the previous snapshot stays in force");
                return Err(error.to_string());
            }
        };
        self.revision += 1;
        let next = Arc::new(RegistrySnapshot::from_load(
            load,
            digest_key,
            now,
            self.revision,
        ));
        let previous = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        // A pause is not a reason to cancel: `enabled` governs routing and probes only, and an admitted request
        // finishes on its account. A disconnect or a deletion takes the credential away, so its streams end.
        for account in previous.accounts() {
            let present = next.account(&account.id).is_some_and(|a| {
                !matches!(
                    a.state,
                    AccountState::Disconnected | AccountState::Pairing | AccountState::Unpaired
                )
            });
            if !present {
                if let Some(token) = self.cancel.remove(&account.id) {
                    token.cancel();
                }
            }
        }
        *self.snapshot.write().expect("snapshot lock poisoned") = next;
        // Pins of a deleted key or pool, everything held against an account the registry no longer has, and
        // a cooldown past its deadline go now rather than at the next tick. An account that left its pool is
        // not one of them: `select` step 1 decides that per request, since a non-portable request keeps such
        // a pin by design.
        self.purge();
        Ok(())
    }

    fn token(&mut self, account_id: &str) -> &CancellationToken {
        self.cancel.entry(account_id.to_string()).or_default()
    }

    /// The three steps a release takes wherever it starts: drop the row, say what released it, and delete it.
    /// The purge tick has its own, because it clears a batch the state already dropped.
    fn release_cooldown(&mut self, account_id: String, scope: String, cause: &'static str) -> bool {
        let cleared = self.state.clear_cooldown(&account_id, &scope);
        if cleared {
            self.emit(RouterEvent::CooldownCleared {
                account_id: account_id.clone(),
                scope: scope.clone(),
                cause,
                at: (self.clock)(),
            });
            self.write(vec![StateWrite::DeleteCooldown(account_id, scope)]);
        }
        cleared
    }

    fn purge(&mut self) {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let now = (self.clock)();
        // The account a pin names is only readable while it is still held, so the rows come first.
        let held: HashMap<(String, String, String), String> = self
            .state
            .pins()
            .into_iter()
            .map(|p| ((p.key_id, p.session_id, p.model), p.account_id))
            .collect();
        let purged = self.state.purge(&snapshot, now);
        tracing::debug!(
            cooldowns = purged.cooldowns.len(),
            pins = purged.pins.len(),
            "purged expired and orphaned routing state"
        );
        self.invalidated
            .retain(|_, (_, _, at)| now - *at <= MIGRATION_WINDOW_MS);
        for (account_id, scope) in &purged.cooldowns {
            self.emit(RouterEvent::CooldownCleared {
                account_id: account_id.clone(),
                scope: scope.clone(),
                cause: "expired",
                at: now,
            });
        }
        for triple in &purged.pins {
            if let Some(account_id) = held.get(triple) {
                self.emit(RouterEvent::PinExpired {
                    key_id: triple.0.clone(),
                    session_id: triple.1.clone(),
                    model: triple.2.clone(),
                    account_id: account_id.clone(),
                    at: now,
                });
            }
        }
        let mut writes: Vec<StateWrite> = purged
            .cooldowns
            .into_iter()
            .map(|(a, s)| StateWrite::DeleteCooldown(a, s))
            .collect();
        writes.extend(
            purged
                .pins
                .into_iter()
                .map(|(k, s, m)| StateWrite::DeletePin(k, s, m)),
        );
        self.write(writes);
    }

    /// Samples arrive through `Report::Sample` from a background fetch, so the loop never waits on the
    /// network; a selection uses whatever samples exist when it runs.
    fn pull_samples(&mut self, request: &SelectRequest) {
        let Some(usage) = self.usage.clone() else {
            return;
        };
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let Some(pool) = snapshot
            .key(&request.key_id)
            .and_then(|k| snapshot.pool(&k.pool_id))
        else {
            return;
        };
        let now = (self.clock)();
        let ttl = self.config.sample_ttl.as_millis() as i64;
        for candidate in &request.candidates {
            let oauth = snapshot
                .account(&candidate.account_id)
                .is_some_and(|a| a.kind == phoebis_store::AccountKind::Oauth);
            let stale = self
                .sampled_at
                .get(&candidate.account_id)
                .is_none_or(|at| now - at >= ttl);
            let needs = needs_sample(
                pool,
                &self.state,
                &candidate.account_id,
                &request.canonical_model,
                now,
            );
            if !oauth || !stale || !needs {
                continue;
            }
            self.sampled_at.insert(candidate.account_id.clone(), now);
            let (usage, tx, account_id) =
                (usage.clone(), self.tx.clone(), candidate.account_id.clone());
            tokio::spawn(async move {
                let Some(tx) = tx.upgrade() else { return };
                if let Ok(snapshot) = usage.snapshot(&account_id).await {
                    let (sample, overage) = sample_from_usage(&snapshot);
                    let _ = tx
                        .send(Command::Report(Report::Sample {
                            account_id: account_id.clone(),
                            sample,
                        }))
                        .await;
                    if let Some(evidence) = overage {
                        let _ = tx
                            .send(Command::Report(Report::Overage {
                                account_id,
                                evidence,
                            }))
                            .await;
                    }
                }
            });
        }
    }

    fn write(&self, writes: Vec<StateWrite>) {
        if !writes.is_empty() {
            let _ = self.writer.send(writes);
        }
    }
}

/// Whether a selection has to read a candidate's usage. The pool's own reasons stand, and a candidate under a
/// live rate-limit cooldown is read whatever the strategy: while the account is cooled no response head of its
/// own can show the window the upstream reset ahead of the deadline it announced. A candidate whose latest
/// sample spent its model-scoped window is read for the same reason: selection excludes it on that reading,
/// so no request of its own will ever bring a fresher one. The kind order is a third: it reads overage
/// evidence to put an account in the second group, and nothing but a sample retires that evidence, so without
/// one a recovered account would sit below its plan-funded siblings until the process restarts.
fn needs_sample(
    pool: &phoebis_store::PoolRow,
    state: &RoutingState,
    account_id: &str,
    canonical_model: &str,
    now: i64,
) -> bool {
    pool.kind_ordered()
        || pool.migrate_above_utilization.is_some()
        || pool.select_below_utilization.is_some()
        || pool.overage_policy != phoebis_store::OveragePolicy::Allow
        || state
            .cooldown(account_id, canonical_model, now)
            .is_some_and(|row| row.reason == "rate_limit")
        || state
            .sample(account_id)
            .is_some_and(|s| s.fable.unwrap_or(0.0) >= 1.0)
}

pub fn sample_from_usage(snapshot: &UsageSnapshot) -> (UtilizationSample, Option<OverageEvidence>) {
    let window = |key: &str| snapshot.windows.iter().find(|w| w.key == key);
    let fraction = |key: &str| window(key).and_then(|w| w.utilization).map(|u| u / 100.0);
    let reset = |key: &str| window(key).and_then(|w| w.resets_at);
    let fable = snapshot.fable_window.as_ref();
    let sample = UtilizationSample {
        fable: fable.and_then(|w| w.utilization).map(|u| u / 100.0),
        fable_reset: fable.and_then(|w| w.resets_at),
        five_hour: fraction("five_hour"),
        five_hour_reset: reset("five_hour"),
        seven_day: fraction("seven_day"),
        seven_day_reset: reset("seven_day"),
        observed_at: snapshot.fetched_at,
    };
    let overage = match (sample.five_hour, sample.seven_day) {
        (None, None) => None,
        (five, seven) => {
            let enabled = snapshot.extra_usage.as_ref().is_some_and(|e| e.is_enabled);
            let full = five.unwrap_or(0.0) >= 1.0 || seven.unwrap_or(0.0) >= 1.0;
            // The latest reset among the exhausted windows: under `never` the account stays out until every full
            // window has reset, not until the first one has.
            let reset = [("five_hour", five), ("seven_day", seven)]
                .into_iter()
                .filter(|(_, u)| u.unwrap_or(0.0) >= 1.0)
                .filter_map(|(key, _)| window(key).and_then(|w| w.resets_at))
                .max();
            Some(OverageEvidence {
                in_use: enabled && full,
                since: None,
                reset,
                observed_at: snapshot.fetched_at,
                source: EvidenceSource::Sample,
            })
        }
    };
    (sample, overage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::{OveragePolicy, PoolRow, RoutingLoad, Strategy, SubStrategy};

    /// A pool that reads no utilization of its own: no threshold, no strategy that orders by a window, and an
    /// overage policy that never keeps an account out.
    fn plain_pool() -> PoolRow {
        PoolRow {
            id: "p".into(),
            label: "p".into(),
            enabled: true,
            strategy: Strategy::PriorityRandom,
            affinity_ttl_seconds: 3_600,
            overage_policy: OveragePolicy::Allow,
            migrate_above_utilization: None,
            select_below_utilization: None,
            default_allowed_models: None,
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: false,
            api_key_sub_strategy: SubStrategy::PriorityRandom,
            cache_ttl_json: "{}".into(),
            cache_diagnostics: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn cooled(until: i64, reason: &str) -> RoutingState {
        RoutingState::from_load(RoutingLoad {
            cooldowns: vec![CooldownRow {
                account_id: "a".into(),
                scope: "account".into(),
                until,
                set_at: 0,
                reason: reason.into(),
                window: "five_hour".into(),
                source_request_id: None,
            }],
            ..RoutingLoad::default()
        })
    }

    #[test]
    fn a_candidate_whose_latest_sample_spent_its_fable_window_is_sampled_again() {
        let mut spent = RoutingState::from_load(RoutingLoad::default());
        spent.record_sample(
            "a",
            UtilizationSample {
                fable: Some(1.0),
                fable_reset: None,
                five_hour: Some(0.1),
                five_hour_reset: None,
                seven_day: Some(0.1),
                seven_day_reset: None,
                observed_at: 1_000,
            },
        );
        assert!(
            needs_sample(&plain_pool(), &spent, "a", "claude-sonnet-5", 2_000),
            "selection excludes on this reading, so nothing but another sample can undo it"
        );
    }

    #[test]
    fn a_candidate_under_a_live_rate_limit_cooldown_is_sampled_whatever_the_strategy() {
        let pool = plain_pool();
        let quiet = RoutingState::from_load(RoutingLoad::default());
        let needs = |pool: &PoolRow, state: &RoutingState| {
            needs_sample(pool, state, "a", "claude-sonnet-5", 1_000)
        };
        assert!(!needs(&pool, &quiet), "nothing in this pool reads usage");
        assert!(
            needs(&pool, &cooled(2_000, "rate_limit")),
            "a cooled candidate is re-checked under priority_random with no threshold set"
        );
        assert!(
            !needs(&pool, &cooled(500, "rate_limit")),
            "a row already past its deadline has nothing left to release"
        );
        assert!(
            !needs(&pool, &cooled(2_000, "permission")),
            "and no amount of allowance restores an entitlement"
        );
        let migrating = PoolRow {
            migrate_above_utilization: Some(0.8),
            ..plain_pool()
        };
        assert!(
            needs(&migrating, &quiet),
            "the pool's own reasons to sample still stand"
        );
    }

    #[test]
    fn a_kind_ordered_pool_is_sampled_even_when_nothing_else_reads_usage() {
        let quiet = RoutingState::from_load(RoutingLoad::default());
        let kind_ordered = PoolRow {
            prefer_subscription: true,
            ..plain_pool()
        };
        assert!(
            !needs_sample(&plain_pool(), &quiet, "a", "claude-sonnet-5", 1_000),
            "the same pool without the preference reads no usage at all"
        );
        assert!(
            needs_sample(&kind_ordered, &quiet, "a", "claude-sonnet-5", 1_000),
            "the kind order reads overage evidence, and only a sample retires it"
        );
        let mut recovered = RoutingState::from_load(RoutingLoad::default());
        recovered.record_overage(
            "a",
            OverageEvidence {
                in_use: true,
                since: Some(1_000),
                reset: None,
                observed_at: 1_000,
                source: EvidenceSource::Sample,
            },
        );
        assert!(
            needs_sample(&kind_ordered, &recovered, "a", "claude-sonnet-5", 1_000),
            "an account already in the second group is exactly the one whose evidence has to be refreshed"
        );
    }
}
