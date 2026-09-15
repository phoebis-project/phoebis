//! The router task: one loop that owns the routing state, answers selections over a channel, reloads the
//! registry snapshot on request, pulls utilization samples in the background, and writes state through a
//! single writer so the store sees every change in the order the loop made it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use phoebis_oauth::{Clock, UsageService, UsageSnapshot};
use phoebis_store::{AccountState, CooldownRow, PinRow, Store, StoreError};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::select::{select, SelectRequest, SelectResult};
use crate::state::{
    EvidenceSource, OverageEvidence, PinRef, RoutingState, StateWrite, UtilizationSample,
};
use crate::{resolve, RegistrySnapshot};

const COMMANDS: usize = 4_096;

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
    },
    Overage {
        account_id: String,
        evidence: OverageEvidence,
    },
    PinInvalidated {
        account_id: String,
        pin: PinRef,
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

    pub async fn select(&self, request: SelectRequest) -> Result<SelectAnswer, RouterGone> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Select { request, reply })
            .await
            .map_err(|_| RouterGone)?;
        rx.await.map_err(|_| RouterGone)
    }

    /// Never waits: a report is a fact about the past, and a relay holding a client stream must not block on
    /// the task's queue. A full queue is logged and the report dropped, which delays a state change by one
    /// request rather than stalling one.
    pub fn report(&self, report: Report) {
        if let Err(error) = self.tx.try_send(Command::Report(report)) {
            tracing::warn!(%error, "the router task's queue is full, a report was dropped");
        }
    }

    pub async fn reload(&self) -> Result<(), ReloadError> {
        self.ask(|reply| Command::Reload { reply })
            .await?
            .map_err(ReloadError::Load)
    }
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
    pub async fn overage(&self, account_id: &str) -> Result<Option<OverageEvidence>, RouterGone> {
        let a = account_id.to_string();
        self.ask(|reply| Command::Overage {
            account_id: a,
            reply,
        })
        .await
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
    let state = RoutingState::from_load(store.load_routing()?);
    let (tx, mut rx) = mpsc::channel(COMMANDS);
    let (writer, mut writes) = mpsc::unbounded_channel::<Vec<StateWrite>>();
    let (done_tx, done_rx) = oneshot::channel();
    let handle = RouterHandle {
        snapshot: snapshot.clone(),
        tx: tx.clone(),
    };
    let writer_store = store.clone();
    tokio::spawn(async move {
        // One batch at a time, in order: two blocking writes racing on separate threads could land a delete
        // before the upsert it was meant to follow.
        while let Some(batch) = writes.recv().await {
            let store = writer_store.clone();
            let result = tokio::task::spawn_blocking(move || apply(&store, batch)).await;
            if let Ok(Err(error)) = result {
                tracing::error!(%error, "a routing state write failed");
            }
        }
        let _ = done_tx.send(());
    });
    let mut task = Task {
        cancel: HashMap::new(),
        clock,
        config,
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

fn apply(store: &Store, batch: Vec<StateWrite>) -> Result<(), StoreError> {
    for write in batch {
        match write {
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
    Ok(())
}

impl Task {
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
                            result: SelectResult::Exhausted(crate::Exhaustion::Untimed),
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
                let (result, writes) =
                    select(&mut self.state, &snapshot, &request, &mut rand::rng());
                self.write(writes);
                let cancel = match &result {
                    SelectResult::Selected(selection) => {
                        self.token(&selection.account_id).child_token()
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
                let gone = self.state.release_session(&key_id, &session_id);
                let count = gone.len();
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
                let cleared = self.state.clear_cooldown(&account_id, &scope);
                self.write(vec![StateWrite::DeleteCooldown(account_id, scope)]);
                let _ = reply.send(cleared);
            }
            Command::Pins { reply } => {
                let _ = reply.send(self.state.pins());
            }
            Command::Cooldowns { reply } => {
                let _ = reply.send(self.state.cooldowns());
            }
            Command::Overage { account_id, reply } => {
                let _ = reply.send(self.state.overage(&account_id).cloned());
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
                row,
                pin,
            } => {
                let mut writes = Vec::new();
                if self.state.record_cooldown(row.clone()) {
                    writes.push(StateWrite::UpsertCooldown(row));
                }
                if let Some(pin) = pin {
                    if self.state.invalidate_pin(&pin, &account_id) {
                        writes.push(StateWrite::DeletePin(pin.key_id, pin.session_id, pin.model));
                    }
                }
                self.write(writes);
            }
            Report::Overage {
                account_id,
                evidence,
            } => {
                if self.state.record_overage(&account_id, evidence) {
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
            Report::PinInvalidated { account_id, pin } => {
                if self.state.invalidate_pin(&pin, &account_id) {
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
                self.state.record_sample(&account_id, sample);
            }
            Report::AccountState {
                account_id,
                state,
                lifecycle_generation,
                credential_generation,
            } => {
                // The relay decided; the store is the source the snapshot is rebuilt from, so the write comes
                // first and the reload follows on the loop's next turn. The write is fenced by both generations:
                // a verdict about a credential that was replaced, or a slot that was disconnected, lands nowhere.
                let store = self.store.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let id = account_id.clone();
                    let moved = tokio::task::spawn_blocking(move || {
                        store.set_state_if_generations(
                            &id,
                            state,
                            lifecycle_generation,
                            credential_generation,
                        )
                    })
                    .await;
                    if !matches!(moved, Ok(Ok(true))) {
                        return;
                    }
                    let Some(tx) = tx.upgrade() else { return };
                    let (reply, _rx) = oneshot::channel();
                    let _ = tx.send(Command::Reload { reply }).await;
                });
            }
        }
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
        // Pins of a deleted key or pool, or of an account that left its pool, go now rather than at the next tick.
        self.purge();
        Ok(())
    }

    fn token(&mut self, account_id: &str) -> &CancellationToken {
        self.cancel.entry(account_id.to_string()).or_default()
    }

    fn purge(&mut self) {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let purged = self.state.purge(&snapshot, (self.clock)());
        tracing::debug!(
            cooldowns = purged.cooldowns.len(),
            pins = purged.pins.len(),
            "purged expired routing state"
        );
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
        let needs = pool.strategy == phoebis_store::Strategy::LeastUtilized
            || pool.migrate_above_utilization.is_some()
            || pool.overage_policy != phoebis_store::OveragePolicy::Allow;
        if !needs {
            return;
        }
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
            if !oauth || !stale {
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

pub fn sample_from_usage(snapshot: &UsageSnapshot) -> (UtilizationSample, Option<OverageEvidence>) {
    let window = |key: &str| snapshot.windows.iter().find(|w| w.key == key);
    let fraction = |key: &str| window(key).and_then(|w| w.utilization).map(|u| u / 100.0);
    let sample = UtilizationSample {
        five_hour: fraction("five_hour"),
        seven_day: fraction("seven_day"),
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
