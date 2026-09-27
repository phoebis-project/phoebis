//! The router task: one loop that owns the routing state, answers selections over a channel, reloads the
//! registry snapshot on request, pulls utilization samples in the background, and writes state through a
//! single writer so the store sees every change in the order the loop made it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use phoebis_oauth::{Clock, UsageService, UsageSnapshot};
use phoebis_store::{AccountState, CooldownRow, FitRow, PinKey, PinRow, RekeyWrite, Store, StoreError};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::affinity::{
    AffinityMap, AffinityReport, ConversationLookup, ConversationMatch, ConversationQuery,
};
use crate::select::{
    release_reasons, select, Exhaustion, MigrateReason, SelectRequest, SelectResult,
};
use crate::state::{
    burn_folded, AccountTicket, EvidenceSource, InFlight, MemberSpend, OverageEvidence, PinRef,
    PinUsage, RateTable, Rates, RoutingState, StateWrite, UtilizationSample, WindowReading,
    FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use crate::{resolve, Presented, RegistrySnapshot};

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
        agent_id: String,
        model: String,
        account_id: String,
        epoch: u64,
        at: i64,
    },
    PinExpired {
        key_id: String,
        session_id: String,
        agent_id: String,
        model: String,
        account_id: String,
        /// What the selection found against the account, in its own order (see `release_reasons`), or
        /// `client_error` alone for a pin the relay released after the request that created it failed.
        reasons: Vec<&'static str>,
        at: i64,
    },
    PinMigrated {
        key_id: String,
        session_id: String,
        agent_id: String,
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
        agent_id: String,
        model: String,
        account_id: String,
        at: i64,
    },
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    /// The meter's peak band at a renewal's instant: the meter burn folds its price times it, the dollar burn
    /// does not.
    pub band: BandWeight,
    pub purge_interval: Duration,
    /// The burn's prices. The task holds them because neither this crate nor the relay can see the ledger's
    /// tables.
    pub rates: BurnRates,
    pub sample_ttl: Duration,
    pub touch_interval: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            band: BandWeight::default(),
            purge_interval: Duration::from_secs(60),
            rates: BurnRates::default(),
            sample_ttl: Duration::from_secs(60),
            touch_interval: Duration::from_secs(60),
        }
    }
}

/// The two tables a renewal's usage is priced through: meter tokens for `PinRow::burn`, dollars for
/// `PinRow::burn_usd`. Both default to `RateTable::unit`, so a burn is a token count until the ledger's tables
/// are wired in.
#[derive(Clone, Debug, PartialEq)]
pub struct BurnRates {
    pub meter: RateTable,
    pub dollars: RateTable,
}

impl Default for BurnRates {
    fn default() -> Self {
        BurnRates {
            meter: RateTable::unit(),
            dollars: RateTable::unit(),
        }
    }
}

/// The weight the meter puts on a charge landing on a window at an instant. The ledger's peak band decides it,
/// and this crate cannot name the ledger's table, so the band arrives as a function; the default weighs every
/// instant 1.
#[derive(Clone)]
pub struct BandWeight(Arc<Weigh>);

type Weigh = dyn Fn(&str, i64) -> f64 + Send + Sync;

impl BandWeight {
    pub fn new(weigh: impl Fn(&str, i64) -> f64 + Send + Sync + 'static) -> BandWeight {
        BandWeight(Arc::new(weigh))
    }

    pub fn at(&self, window: &str, at: i64) -> f64 {
        (self.0)(window, at)
    }
}

impl Default for BandWeight {
    fn default() -> BandWeight {
        BandWeight::new(|_, _| 1.0)
    }
}

impl std::fmt::Debug for BandWeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BandWeight")
    }
}

pub struct SelectAnswer {
    pub result: SelectResult,
    pub cancel: CancellationToken,
    /// The selected account's in-flight ticket, taken where the selection read the count, for the request's
    /// first wire attempt on it; `None` for any other answer.
    pub ticket: Option<AccountTicket>,
}

/// Every report that names an account names its holder too: an id is reusable, and a report whose
/// incarnation is not the snapshot account's changes neither memory nor the store.
#[derive(Clone, Debug)]
pub enum Report {
    AccountState {
        account_id: String,
        state: AccountState,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    Affinity(AffinityReport),
    Cooldown {
        account_id: String,
        row: CooldownRow,
        pin: Option<PinRef>,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    Fit {
        row: FitRow,
        /// The account holder's incarnation, the guard every report carries. `row.incarnation` holds the same
        /// value as the ledger saw it; the report's is the one checked here, and the row's is the one `purge`
        /// checks later.
        incarnation: u64,
    },
    /// The ledger's latest read of a pool member's spend, replacing every row it names.
    MemberSpend(Vec<MemberSpend>),
    Overage {
        account_id: String,
        evidence: OverageEvidence,
        incarnation: u64,
    },
    PinInvalidated {
        account_id: String,
        pin: PinRef,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    /// A pin the relay releases because the request that created it ended in a client error; the next request of
    /// the session is a fresh placement, not a move.
    PinReleased {
        pin: PinRef,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    PinRenewed {
        pin: PinRef,
        now: i64,
        /// The attempt's usage, `None` for an attempt that answered none; never folded for a probe.
        usage: Option<PinUsage>,
        /// `client` or `keepalive`.
        origin: &'static str,
        canonical_model: String,
        /// The cache TTL the relay resolved for the attempt: the policy's rewrite, else the client's marker.
        cache_ttl_ms: i64,
    },
    Sample {
        account_id: String,
        sample: UtilizationSample,
        incarnation: u64,
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
    Canonicals {
        models: Vec<String>,
        reply: oneshot::Sender<Vec<String>>,
    },
    ClearCooldown {
        account_id: String,
        scope: String,
        reply: oneshot::Sender<bool>,
    },
    Cooldowns {
        reply: oneshot::Sender<Vec<CooldownRow>>,
    },
    Fits {
        reply: oneshot::Sender<Vec<FitRow>>,
    },
    MemberSpends {
        reply: oneshot::Sender<Vec<MemberSpend>>,
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
    /// The dollar rates a confirmed pricing import reaches the burn with, without a restart.
    ReplaceRates {
        dollars: RateTable,
        reply: oneshot::Sender<()>,
    },
    /// The dollar rates in force for one model, so a caller can check a `ReplaceRates` landed without serving a
    /// request through it.
    Rates {
        canonical_model: String,
        reply: oneshot::Sender<Rates>,
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
    Resolve {
        spelling: String,
        dated_id: String,
        at: i64,
        reply: oneshot::Sender<()>,
    },
    /// A usage fetch ended: `at` is when its sample landed, `None` for a fetch that failed.
    SampleFinished {
        account_id: String,
        incarnation: u64,
        at: Option<i64>,
    },
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
        presented: Presented,
        rotated_at: Option<i64>,
    },
}

#[derive(Clone)]
pub struct RouterHandle {
    affinity: Arc<RwLock<AffinityMap>>,
    events: broadcast::Sender<RouterEvent>,
    in_flight: InFlight,
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

    /// The shared in-flight table: a key permit reserved after authentication and before selection, held across
    /// every attempt; an account ticket per wire attempt, a leg's first handed out with its selection.
    pub fn in_flight(&self) -> &InFlight {
        &self.in_flight
    }

    /// The shared affinity map: the relay's conversation climb reads it under the same lock the report handler
    /// writes, without a command round trip.
    pub fn affinity(&self) -> Arc<RwLock<AffinityMap>> {
        self.affinity.clone()
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

    /// A spelling the catalog cache resolved to the dated id the API lists. Waits for room in the queue
    /// rather than dropping, and answers once the state keys the spelling's pins and cooldowns on the dated
    /// id: a caller that names requests by the dated id only after this answers never runs ahead of the rows.
    pub async fn resolve(&self, spelling: &str, dated_id: &str, at: i64) -> Result<(), RouterGone> {
        let (spelling, dated_id) = (spelling.to_string(), dated_id.to_string());
        self.ask(|reply| Command::Resolve {
            spelling,
            dated_id,
            at,
            reply,
        })
        .await
    }

    /// The name the state keys each model's rows on, in the order asked: one ask for every model a caller
    /// retained, so a sweep never queues one command per model in front of selection.
    pub async fn canonicals(&self, models: Vec<String>) -> Result<Vec<String>, RouterGone> {
        self.ask(|reply| Command::Canonicals { models, reply })
            .await
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
    /// Swaps the dollar side of the burn's rates, so a confirmed pricing import reaches it without a restart.
    pub async fn replace_dollar_rates(&self, dollars: RateTable) -> Result<(), RouterGone> {
        self.ask(|reply| Command::ReplaceRates { dollars, reply })
            .await
    }
    /// The dollar rates in force for `canonical_model`, for a caller that has to check a `replace_dollar_rates`
    /// landed without serving a request through it.
    pub async fn rates(&self, canonical_model: &str) -> Result<Rates, RouterGone> {
        let canonical_model = canonical_model.to_string();
        self.ask(|reply| Command::Rates {
            canonical_model,
            reply,
        })
        .await
    }
    pub async fn cooldowns(&self) -> Result<Vec<CooldownRow>, RouterGone> {
        self.ask(|reply| Command::Cooldowns { reply }).await
    }
    /// One ask for every account's fit rows, so a reader never queues one command per account in front of
    /// selection.
    pub async fn fits(&self) -> Result<Vec<FitRow>, RouterGone> {
        self.ask(|reply| Command::Fits { reply }).await
    }
    /// Every budgeted member's spend as the ledger last reported it, from the state's table alone: the
    /// analytics route reads this rather than querying the ledger a second time.
    pub async fn member_spends(&self) -> Result<Vec<MemberSpend>, RouterGone> {
        self.ask(|reply| Command::MemberSpends { reply }).await
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
    /// `rotated_at` is the key's rotation the caller authenticated under, so a touch that lands after a later
    /// rotation stamps nothing on the new grace.
    pub fn touch_key(&self, key_id: &str, presented: Presented, rotated_at: Option<i64>) {
        let _ = self.tx.try_send(Command::Touch {
            key_id: key_id.to_string(),
            presented,
            rotated_at,
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

impl ConversationLookup for RouterHandle {
    /// The snapshot's own incarnation reading, so a stale conversation never outlives the account it named.
    fn conversation(&self, query: &ConversationQuery<'_>) -> Option<ConversationMatch> {
        let snapshot = self.snapshot();
        let incarnation_of = |account_id: &str| snapshot.account(account_id).map(|a| a.incarnation);
        let query = ConversationQuery {
            canonical_model: query.canonical_model,
            key_id: query.key_id,
            key_generation: query.key_generation,
            parent_ids: query.parent_ids,
            message_boundaries: query.message_boundaries,
            incarnation_of: &incarnation_of,
        };
        self.affinity
            .read()
            .expect("affinity lock poisoned")
            .conversation(&query)
    }
}

struct Task {
    cancel: HashMap<String, CancellationToken>,
    clock: Clock,
    config: RouterConfig,
    events: broadcast::Sender<RouterEvent>,
    /// The account a pin was invalidated from, why, and when. The next select for that same key
    /// within `MIGRATION_WINDOW_MS` reports a migration rather than a fresh pin; older entries go on purge.
    invalidated: HashMap<PinKey, (String, &'static str, i64)>,
    revision: u64,
    sampling: Sampling,
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    state: RoutingState,
    store: Arc<Store>,
    /// The rotation each slot's last touch was stamped under, beside its timestamp: a touch under a different
    /// rotation is a first sight of that presentation, not a repeat the grace period should swallow.
    touched_at: HashMap<(String, Presented), (i64, Option<i64>)>,
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
    // One table, shared by the handle (the relay's synchronous permits and tickets) and the state (the
    // selector's cap partition), so a count is exact wherever it is read.
    let in_flight = InFlight::default();
    state.set_in_flight(in_flight.clone());
    // Shared the same way, so the relay's conversation climb reads the map under the same lock the report
    // handler writes, without a command round trip.
    let affinity = state.affinity();
    let (tx, mut rx) = mpsc::channel(COMMANDS);
    // Unbounded so the loop never blocks on SQLite: a stalled store grows this queue in memory rather than
    // stalling selection, and bounding the process's memory is the operator's call, not this channel's.
    let (writer, mut writes) = mpsc::unbounded_channel::<Vec<StateWrite>>();
    let (done_tx, done_rx) = oneshot::channel();
    let (events, _) = broadcast::channel(EVENTS);
    let handle = RouterHandle {
        affinity,
        events: events.clone(),
        in_flight,
        snapshot: snapshot.clone(),
        tx: tx.clone(),
    };
    let writer_store = store.clone();
    // Weak for the same reason the task's own sender is: a strong one here would keep the loop running after
    // the last handle is gone, and with it the process lock a reopened on-disk store needs back.
    let writer_tx = tx.downgrade();
    tokio::spawn(async move {
        // The memory has already moved when a rekey fails, and only the next landing of the same spelling
        // would write it again; one more try at the next batch covers a passing failure without letting a
        // write the store keeps refusing hold up every later one.
        let mut retry: Option<Box<RekeyWrite>> = None;
        // One batch at a time, in order: two blocking writes racing on separate threads could land a delete
        // before the upsert it was meant to follow.
        while let Some(batch) = writes.recv().await {
            if let Some(rekey) = retry.take() {
                retry_rekey(&writer_store, rekey).await;
            }
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
                Ok(Err(failed)) => {
                    tracing::error!(error = %failed.error, batch = size, "a routing state write failed");
                    retry = failed.rekey;
                }
                Err(error) => {
                    tracing::error!(%error, batch = size, "a routing state write panicked, its batch is lost")
                }
            }
        }
        if let Some(rekey) = retry.take() {
            retry_rekey(&writer_store, rekey).await;
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
        sampling: Sampling::default(),
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
                        while let Some(command) = rx.recv().await {
                            match command {
                                Command::Report(report) => task.report(report),
                                // A resolution is a report with an answer: it lands with the drain rather than
                                // leaving the catalog cache to publish the spelling again after a restart.
                                Command::Resolve { spelling, dated_id, at, reply } => {
                                    task.resolve(&spelling, &dated_id, at);
                                    let _ = reply.send(());
                                }
                                _ => {}
                            }
                        }
                        // The map's own dirty rows are never written until this point, so a clean shutdown is
                        // the one moment they have to reach the store; a crash loses them, which the design
                        // accepts as best effort.
                        let drained = task
                            .state
                            .affinity()
                            .write()
                            .expect("affinity lock poisoned")
                            .drain();
                        if !drained.is_empty() {
                            task.write(vec![StateWrite::UpsertAffinity(drained)]);
                        }
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

/// A batch that stopped at a failed write, carrying that write when it was a rekey: the one write the writer
/// tries again.
struct Failed {
    error: StoreError,
    rekey: Option<Box<RekeyWrite>>,
}

impl From<StoreError> for Failed {
    fn from(error: StoreError) -> Failed {
        Failed { error, rekey: None }
    }
}

async fn retry_rekey(store: &Arc<Store>, rekey: Box<RekeyWrite>) {
    let store = store.clone();
    let spelling = rekey.resolution.spelling.clone();
    match tokio::task::spawn_blocking(move || store.rekey_resolution(&rekey)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(%error, %spelling, "a model resolution's rekey failed again and is dropped")
        }
        Err(error) => {
            tracing::error!(%error, %spelling, "a model resolution's rekey panicked and is dropped")
        }
    }
}

/// Reports whether any account-state mark in the batch actually moved its row, which is the one write here
/// that the registry snapshot has to be rebuilt from.
fn apply(store: &Store, batch: Vec<StateWrite>) -> Result<bool, Failed> {
    let mut moved = false;
    for write in batch {
        match write {
            StateWrite::AccountState {
                account_id,
                state,
                incarnation,
                lifecycle_generation,
                credential_generation,
            } => {
                moved |= store.set_state_if_generations(
                    &account_id,
                    state,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                )?;
            }
            StateWrite::DeleteAffinity(keys) => {
                store.delete_affinity(&keys)?;
            }
            StateWrite::DeleteCooldown(account_id, scope) => {
                store.delete_cooldown(&account_id, &scope)?;
            }
            StateWrite::DeletePin(key) => {
                store.delete_pin(&key)?;
            }
            StateWrite::Rekey(rekey) => {
                if let Err(error) = store.rekey_resolution(&rekey) {
                    return Err(Failed {
                        error,
                        rekey: Some(Box::new(rekey)),
                    });
                }
            }
            StateWrite::UpsertAffinity(rows) => store.upsert_affinity(&rows)?,
            StateWrite::UpsertCooldown(row) => store.upsert_cooldown(&row)?,
            StateWrite::UpsertFit(row) => store.upsert_fit(&row)?,
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
                // Keyed on the dated id from the first request after a resolution lands, so a fresh pin and a
                // fresh cooldown never start a second life under the spelling the resolution just retired.
                request.canonical_model =
                    self.state.canonical(&request.canonical_model).to_string();
                if request.revision < snapshot.revision {
                    // The registry moved after admission: the key, its policy, the pool, and the candidates are
                    // re-derived from what is current, and a request the current registry would refuse is refused.
                    let now = (self.clock)();
                    let Ok(key) = snapshot.authenticate_id(&request.key_id, now) else {
                        let _ = reply.send(SelectAnswer {
                            result: SelectResult::Exhausted(Exhaustion::Untimed),
                            cancel: CancellationToken::new(),
                            ticket: None,
                        });
                        return;
                    };
                    // The id is reusable, so the lifetime is the identity: a key created again under the id
                    // since admission is another key, and the request is refused as a refused id is.
                    if key.created_at != request.key_generation {
                        let _ = reply.send(SelectAnswer {
                            result: SelectResult::Exhausted(Exhaustion::Untimed),
                            cancel: CancellationToken::new(),
                            ticket: None,
                        });
                        return;
                    }
                    // The relay's `SelectRequest` carries no folded spelling of its own; the first admitted
                    // candidate's is the closest thing on hand, since every candidate carries the same one.
                    let folded = request
                        .candidates
                        .first()
                        .map(|c| c.upstream_model.clone())
                        .unwrap_or_else(|| request.canonical_model.clone());
                    match resolve(
                        &snapshot,
                        key,
                        &request.canonical_model,
                        &folded,
                    ) {
                        Ok(candidates) => request.candidates = candidates,
                        Err(refusal) => {
                            let _ = reply.send(SelectAnswer {
                                result: SelectResult::Refused(refusal),
                                cancel: CancellationToken::new(),
                                ticket: None,
                            });
                            return;
                        }
                    }
                    request.revision = snapshot.revision;
                }
                self.pull_samples(&request);
                let pin_key = request.session_id.as_ref().map(|s| PinKey {
                    key_id: request.key_id.clone(),
                    session_id: s.clone(),
                    agent_id: request.agent_id.clone(),
                    model: request.canonical_model.clone(),
                });
                let before = pin_key
                    .as_ref()
                    .and_then(|key| self.state.pin(key).cloned());
                // The facts are read against the state the selection runs on, before it moves anything.
                let released = before
                    .as_ref()
                    .map(|old| release_reasons(&self.state, &snapshot, old, &request))
                    .unwrap_or_default();
                let (mut result, writes) =
                    select(&mut self.state, &snapshot, &request, &mut rand::rng());
                self.write(writes);
                if let Some(key) = &pin_key {
                    let (k, s, ag, m) = (&key.key_id, &key.session_id, &key.agent_id, &key.model);
                    let after = self.state.pin(key).cloned();
                    let now = request.now;
                    let selected = matches!(result, SelectResult::Selected(_));
                    match (before, after) {
                        (None, Some(new)) if selected => {
                            match self
                                .invalidated
                                .remove(key)
                                .filter(|(_, _, at)| now - at <= MIGRATION_WINDOW_MS)
                            {
                                Some((from, reason, _)) => {
                                    // A hard fail-over's move is named on the attempt row the same way a
                                    // selector's move is.
                                    if let SelectResult::Selected(selection) = &mut result {
                                        selection.trace.moved = Some(reason);
                                    }
                                    self.emit(RouterEvent::PinMigrated {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        agent_id: ag.clone(),
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
                                    agent_id: ag.clone(),
                                    model: m.clone(),
                                    account_id: new.account_id.clone(),
                                    epoch: new.epoch,
                                    at: now,
                                }),
                            }
                        }
                        (Some(old), Some(new)) if old.epoch != new.epoch => {
                            if let SelectResult::Selected(selection) = &result {
                                let reason = selection
                                    .migrated
                                    .map(MigrateReason::as_str)
                                    .unwrap_or("expired");
                                if reason == "expired" {
                                    self.emit(RouterEvent::PinExpired {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        agent_id: ag.clone(),
                                        model: m.clone(),
                                        account_id: old.account_id.clone(),
                                        reasons: released.clone(),
                                        at: now,
                                    });
                                    self.emit(RouterEvent::PinCreated {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        agent_id: ag.clone(),
                                        model: m.clone(),
                                        account_id: new.account_id.clone(),
                                        epoch: new.epoch,
                                        at: now,
                                    });
                                } else {
                                    self.emit(RouterEvent::PinMigrated {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        agent_id: ag.clone(),
                                        model: m.clone(),
                                        from_account: old.account_id.clone(),
                                        to_account: new.account_id.clone(),
                                        epoch: new.epoch,
                                        reason: reason.into(),
                                        at: now,
                                    });
                                }
                            }
                        }
                        (Some(old), None) => self.emit(RouterEvent::PinExpired {
                            key_id: k.clone(),
                            session_id: s.clone(),
                            agent_id: ag.clone(),
                            model: m.clone(),
                            account_id: old.account_id.clone(),
                            reasons: released,
                            at: now,
                        }),
                        _ => {}
                    }
                }
                let (cancel, ticket) = match &result {
                    // Charged here, in the one place selections are serialized, so the count the next selection
                    // reads already holds this one: a ticket taken at the dial, awaits later, would let every
                    // selection answered in between read the same count and land over the cap.
                    SelectResult::Selected(selection) => (
                        self.token(&selection.account.id).child_token(),
                        Some(
                            self.state
                                .in_flight()
                                .dispatched(&selection.account.id, request.now),
                        ),
                    ),
                    SelectResult::Exhausted(_)
                    | SelectResult::Held { .. }
                    | SelectResult::Refused(_) => (CancellationToken::new(), None),
                };
                let _ = reply.send(SelectAnswer {
                    result,
                    cancel,
                    ticket,
                });
            }
            Command::Report(report) => self.report(report),
            Command::Resolve {
                spelling,
                dated_id,
                at,
                reply,
            } => {
                self.resolve(&spelling, &dated_id, at);
                let _ = reply.send(());
            }
            Command::Canonicals { models, reply } => {
                let _ = reply.send(
                    models
                        .iter()
                        .map(|model| self.state.canonical(model).to_string())
                        .collect(),
                );
            }
            Command::SampleFinished {
                account_id,
                incarnation,
                at,
            } => {
                // The reload already dropped the marker of a holder the id moved past, and the one standing
                // now may be the next holder's own fetch.
                if !self.moved_past(&account_id, incarnation) {
                    self.sampling.finish(&account_id, at);
                }
            }
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
                let held: HashMap<PinKey, String> = self
                    .state
                    .pins()
                    .into_iter()
                    .filter(|p| p.key_id == key_id && p.session_id == session_id)
                    .map(|p| (p.key(), p.account_id))
                    .collect();
                // Every agent's pin of the session leaves together: the session's next request starts fresh
                // wherever each of its agents lands, not only its main turn's.
                let gone = self.state.release_session(&key_id, &session_id);
                let count = gone.len();
                let now = (self.clock)();
                for key in &gone {
                    if let Some(account_id) = held.get(key) {
                        self.emit(RouterEvent::PinReleased {
                            key_id: key.key_id.clone(),
                            session_id: key.session_id.clone(),
                            agent_id: key.agent_id.clone(),
                            model: key.model.clone(),
                            account_id: account_id.clone(),
                            at: now,
                        });
                    }
                }
                self.write(gone.into_iter().map(StateWrite::DeletePin).collect());
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
            Command::ReplaceRates { dollars, reply } => {
                self.config.rates.dollars = dollars;
                let _ = reply.send(());
            }
            Command::Rates {
                canonical_model,
                reply,
            } => {
                let _ = reply.send(self.config.rates.dollars.rates_for(&canonical_model));
            }
            Command::Cooldowns { reply } => {
                let _ = reply.send(self.state.cooldowns());
            }
            Command::Fits { reply } => {
                let _ = reply.send(self.state.fits());
            }
            Command::MemberSpends { reply } => {
                let snapshot = self
                    .snapshot
                    .read()
                    .expect("snapshot lock poisoned")
                    .clone();
                let rows = snapshot
                    .pools()
                    .flat_map(|pool| snapshot.members(&pool.id))
                    .filter(|m| m.budget_usd.is_some())
                    .filter_map(|m| self.state.member_spend(&m.pool_id, &m.account_id).cloned())
                    .collect();
                let _ = reply.send(rows);
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
            Command::Touch {
                key_id,
                presented,
                rotated_at,
            } => {
                let now = (self.clock)();
                // Debounced per presentation, so a retired-secret touch is never swallowed by a live one that
                // landed inside the same minute: the retired stamp is what the console's count leans on. A
                // stamp from before a key rotated again never debounces the newly retired secret's first
                // touch, or that secret's use would stay unstamped until the earlier grace expired on its own.
                let slot = (key_id.clone(), presented);
                let due = match self.touched_at.get(&slot) {
                    Some((_, stamped_rotation)) if *stamped_rotation != rotated_at => true,
                    Some((at, _)) => now - at >= self.config.touch_interval.as_millis() as i64,
                    None => true,
                };
                if due {
                    self.touched_at.insert(slot, (now, rotated_at));
                    let store = self.store.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = store.touch_api_key(
                            &key_id,
                            now,
                            presented == Presented::Retired,
                            rotated_at,
                        );
                    });
                }
            }
        }
    }

    fn report(&mut self, report: Report) {
        match report {
            Report::Affinity(report) => {
                if self.moved_past(&report.account_id, report.incarnation) {
                    return;
                }
                let rows = self
                    .state
                    .affinity()
                    .write()
                    .expect("affinity lock poisoned")
                    .record(&report);
                if !rows.is_empty() {
                    self.write(vec![StateWrite::UpsertAffinity(rows)]);
                }
            }
            Report::MemberSpend(rows) => {
                let snapshot = self
                    .snapshot
                    .read()
                    .expect("snapshot lock poisoned")
                    .clone();
                self.state.record_member_spend(&snapshot, rows);
            }
            Report::Cooldown {
                account_id,
                mut row,
                pin,
                incarnation,
                lifecycle_generation,
                credential_generation,
            } => {
                if self.superseded(
                    &account_id,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                ) {
                    return;
                }
                // The router's own clock dates the row, so a sample is compared against the loop's timeline
                // rather than the relay's. The scope is canonicalized here too, not only inside
                // `record_cooldown`, so the write this pushes and the event it emits carry the same key the
                // state now holds the row under, whichever spelling the report named.
                row.set_at = (self.clock)();
                row.incarnation = incarnation;
                row.scope = self.state.canonical_scope(&row.scope);
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
                if let Some(mut pin) = pin {
                    pin.model = self.state.canonical(&pin.model).to_string();
                    if self.state.invalidate_pin(&pin, &account_id) {
                        let key = PinKey {
                            key_id: pin.key_id,
                            session_id: pin.session_id,
                            agent_id: pin.agent_id,
                            model: pin.model,
                        };
                        self.invalidated.insert(
                            key.clone(),
                            (account_id.clone(), "cooldown", (self.clock)()),
                        );
                        writes.push(StateWrite::DeletePin(key));
                    }
                }
                self.write(writes);
            }
            Report::Overage {
                account_id,
                mut evidence,
                incarnation,
            } => {
                if self.moved_past(&account_id, incarnation) {
                    return;
                }
                evidence.incarnation = incarnation;
                let at = evidence.observed_at;
                let (was, since) = match self.state.overage(&account_id) {
                    Some(current) => (current.in_use, current.since),
                    None => (false, None),
                };
                if self.state.record_overage(&account_id, evidence) {
                    // The flag as it now stands on the record just folded, not the predicate: this detects the
                    // report's own edge, which a reset passing quietly between reports must not retrigger.
                    let now_in_use = self.state.overage(&account_id).is_some_and(|e| e.in_use);
                    match (was, now_in_use) {
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
                            incarnation: current.incarnation,
                        })]);
                    }
                }
            }
            Report::PinInvalidated {
                account_id,
                mut pin,
                incarnation,
                lifecycle_generation,
                credential_generation,
            } => {
                if self.superseded(
                    &account_id,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                ) {
                    return;
                }
                pin.model = self.state.canonical(&pin.model).to_string();
                if self.state.invalidate_pin(&pin, &account_id) {
                    let key = PinKey {
                        key_id: pin.key_id,
                        session_id: pin.session_id,
                        agent_id: pin.agent_id,
                        model: pin.model,
                    };
                    self.invalidated
                        .insert(key.clone(), (account_id.clone(), "mark", (self.clock)()));
                    self.write(vec![StateWrite::DeletePin(key)]);
                }
            }
            Report::PinReleased {
                mut pin,
                incarnation,
                lifecycle_generation,
                credential_generation,
            } => {
                pin.model = self.state.canonical(&pin.model).to_string();
                let key = PinKey {
                    key_id: pin.key_id.clone(),
                    session_id: pin.session_id.clone(),
                    agent_id: pin.agent_id.clone(),
                    model: pin.model.clone(),
                };
                // The epoch names the one pin the request created, and with it the account the generations are
                // checked against; a pin replaced since is another placement's to keep. A pin some selection
                // renewed after its placement is serving another turn of the session, so the error that ended
                // the first turn no longer speaks for it.
                if self.state.renewed_since_placement(&key) {
                    return;
                }
                let Some(account_id) = self
                    .state
                    .pin(&key)
                    .filter(|current| current.epoch == pin.epoch)
                    .map(|current| current.account_id.clone())
                else {
                    return;
                };
                if self.superseded(
                    &account_id,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                ) {
                    return;
                }
                // Nothing enters `invalidated`: the pin never served, so the session's next request is a fresh
                // placement rather than a move away from it.
                self.state.remove_pin(&key);
                self.emit(RouterEvent::PinExpired {
                    key_id: key.key_id.clone(),
                    session_id: key.session_id.clone(),
                    agent_id: key.agent_id.clone(),
                    model: key.model.clone(),
                    account_id,
                    reasons: vec!["client_error"],
                    at: (self.clock)(),
                });
                self.write(vec![StateWrite::DeletePin(key)]);
            }
            Report::PinRenewed {
                mut pin,
                now,
                usage,
                origin,
                canonical_model,
                cache_ttl_ms,
            } => {
                pin.model = self.state.canonical(&pin.model).to_string();
                let key = PinKey {
                    key_id: pin.key_id.clone(),
                    session_id: pin.session_id.clone(),
                    agent_id: pin.agent_id.clone(),
                    model: pin.model.clone(),
                };
                if let Some(current) = self.state.pin(&key).cloned() {
                    if current.epoch == pin.epoch {
                        let mut renewed = PinRow {
                            last_used_at: now,
                            ..current
                        };
                        renewed.cache_ttl_ms = cache_ttl_ms;
                        if origin == "keepalive" {
                            // A probe's selection judged nothing, since it replays another request's facts; the
                            // renewal is the refresh it wrote. Its usage is the keepalive's cost, never the
                            // session's burn.
                            renewed.last_refresh_at = now;
                        } else if let Some(usage) = usage {
                            // Both averages fold whatever the account's kind: the reader picks the unit its
                            // funding selects at read time, so leaving overage needs no reset.
                            let window_ms = self.burn_window_ms(&pin.key_id);
                            // The meter charges a request inside the peak band more, so the meter burn weighs it
                            // by the band at the renewal; dollars cost the same at every hour.
                            let meter = self.config.rates.meter.price(&canonical_model, usage)
                                * self.config.band.at(FIVE_HOUR, now);
                            let dollars = self.config.rates.dollars.price(&canonical_model, usage);
                            renewed.burn =
                                burn_folded(renewed.burn, renewed.burn_at, now, window_ms, meter);
                            renewed.burn_usd = burn_folded(
                                renewed.burn_usd,
                                renewed.burn_at,
                                now,
                                window_ms,
                                dollars,
                            );
                            renewed.burn_at = Some(now);
                            renewed.last_input_tokens =
                                usage.input + usage.cache_read + usage.write_5m + usage.write_1h;
                        }
                        self.state.put_pin(renewed.clone());
                        self.state.mark_renewed(&key);
                        self.write(vec![StateWrite::UpsertPin(renewed)]);
                    }
                }
            }
            Report::Sample {
                account_id,
                sample,
                incarnation,
            } => {
                if self.moved_past(&account_id, incarnation) {
                    return;
                }
                // A head that read both shared windows and every window the account's sample already holds says
                // what a usage fetch would, so it holds the poll off for a TTL as a fetch does. One that leaves a
                // known window unread leaves the poll due: a head may not carry the Fable reading, and only the
                // poll would re-read it while the account stays busy.
                let covers = sample.windows.contains_key(FIVE_HOUR)
                    && sample.windows.contains_key(SEVEN_DAY)
                    && self.state.sample(&account_id).is_none_or(|stored| {
                        stored
                            .windows
                            .keys()
                            .all(|window| sample.windows.contains_key(window))
                    });
                if covers {
                    self.sampling.stamp(&account_id, sample.observed_at);
                }
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
            Report::Fit { row, incarnation } => {
                if self.moved_past(&row.account_id, incarnation) {
                    return;
                }
                if self.state.record_fit(row.clone()) {
                    self.write(vec![StateWrite::UpsertFit(row)]);
                }
            }
            Report::AccountState {
                account_id,
                state,
                incarnation,
                lifecycle_generation,
                credential_generation,
            } => {
                // The relay decided; the store is the source the snapshot is rebuilt from, so the write comes
                // first and the reload follows once it has landed. The write is fenced by the incarnation and
                // both generations: a verdict about a credential that was replaced, a slot that was
                // disconnected, or a holder the id no longer has lands nowhere. It goes through the writer
                // every other routing write uses, so shutdown drains it rather than leaving a mark in a
                // detached task the runtime may drop unpolled.
                self.write(vec![StateWrite::AccountState {
                    account_id,
                    state,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                }]);
            }
        }
    }

    fn resolve(&mut self, spelling: &str, dated_id: &str, at: i64) {
        let rekey = self.state.apply_resolution(spelling, dated_id, at);
        // A late invalidation from a request admitted under the retiring spelling must still land beside the
        // moved pin rather than starting a fresh migration window under a key nothing selects on any more.
        let keys: Vec<PinKey> = self
            .invalidated
            .keys()
            .filter(|key| key.model == spelling)
            .cloned()
            .collect();
        for key in keys {
            if let Some(value) = self.invalidated.remove(&key) {
                self.invalidated.insert(
                    PinKey {
                        model: dated_id.to_string(),
                        ..key
                    },
                    value,
                );
            }
        }
        self.write(vec![StateWrite::Rekey(rekey)]);
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
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    ) -> bool {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let Some(account) = holder(&snapshot, account_id, incarnation) else {
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

    fn moved_past(&self, account_id: &str, incarnation: u64) -> bool {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        holder(&snapshot, account_id, incarnation).is_none()
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
        // finishes on its account. A disconnect or a deletion takes the credential away, so its streams end,
        // and so do those of a holder deleted and created again under its id since the previous snapshot.
        // Samples and the sampler's marks are keyed by id alone, so a moved holder's go here, before the purge.
        for account in previous.accounts() {
            let moved = next
                .account(&account.id)
                .is_none_or(|a| a.incarnation != account.incarnation);
            let present = !moved
                && next.account(&account.id).is_some_and(|a| {
                    !matches!(a.state, AccountState::Disconnected | AccountState::Unpaired)
                });
            if !present {
                if let Some(token) = self.cancel.remove(&account.id) {
                    token.cancel();
                }
            }
            if moved {
                self.state.forget_sample(&account.id);
                self.sampling.forget(&account.id);
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

    /// The burn window of the pool the key routes through; the default preset's for a key the snapshot no
    /// longer holds, whose pin the next purge drops anyway.
    fn burn_window_ms(&self, key_id: &str) -> i64 {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let seconds = snapshot
            .key(key_id)
            .and_then(|k| snapshot.pool(&k.pool_id))
            .map_or(phoebis_store::Shedding::default().burn_window_seconds, |pool| {
                pool.shedding.burn_window_seconds
            });
        i64::try_from(seconds)
            .ok()
            .and_then(|s| s.checked_mul(1_000))
            .unwrap_or(i64::MAX)
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
        // A permit or a ticket whose guard's drop never reached the table: a panic, or a stream past the relay's
        // idle bound with no bound of its own.
        self.state.in_flight().expire(now);
        // The account a pin names is only readable while it is still held, so the rows come first.
        let held: HashMap<PinKey, String> = self
            .state
            .pins()
            .into_iter()
            .map(|p| (p.key(), p.account_id))
            .collect();
        let purged = self.state.purge(&snapshot, now);
        tracing::debug!(
            affinity = purged.affinity.len(),
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
        for (key, reason) in &purged.pins {
            if let Some(account_id) = held.get(key) {
                self.emit(RouterEvent::PinExpired {
                    key_id: key.key_id.clone(),
                    session_id: key.session_id.clone(),
                    agent_id: key.agent_id.clone(),
                    model: key.model.clone(),
                    account_id: account_id.clone(),
                    reasons: vec![*reason],
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
                .map(|(key, _)| StateWrite::DeletePin(key)),
        );
        if !purged.affinity.is_empty() {
            writes.push(StateWrite::DeleteAffinity(purged.affinity));
        }
        self.write(writes);
    }

    /// Samples arrive through `Report::Sample` from a background fetch, so the loop never waits on the
    /// network; a selection uses whatever samples exist when it runs. The fetch names the holder it was
    /// started for, so an answer that lands after a delete, a create and a reload under the same id is
    /// refused rather than recorded against the next holder.
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
            let Some(account) = snapshot.account(&candidate.account_id) else {
                continue;
            };
            let oauth = account.kind == phoebis_store::AccountKind::Oauth;
            let due = self.sampling.due(&candidate.account_id, now, ttl);
            let needs = needs_sample(pool, &self.state, &candidate.account_id, now);
            if !oauth || !due || !needs {
                continue;
            }
            self.sampling.begin(&candidate.account_id);
            let (usage, tx, clock, account_id, incarnation) = (
                usage.clone(),
                self.tx.clone(),
                self.clock.clone(),
                candidate.account_id.clone(),
                account.incarnation,
            );
            tokio::spawn(async move {
                let Some(tx) = tx.upgrade() else { return };
                let mut guard = SampleGuard::new(&account_id, incarnation, tx.clone());
                let at = match usage.snapshot(&account_id).await {
                    Ok(snapshot) => {
                        let (sample, overage) = sample_from_usage(&snapshot, incarnation);
                        let _ = tx
                            .send(Command::Report(Report::Sample {
                                account_id: account_id.clone(),
                                sample,
                                incarnation,
                            }))
                            .await;
                        if let Some(evidence) = overage {
                            let _ = tx
                                .send(Command::Report(Report::Overage {
                                    account_id: account_id.clone(),
                                    evidence,
                                    incarnation,
                                }))
                                .await;
                        }
                        Some(clock())
                    }
                    Err(_) => None,
                };
                guard.finish();
                let _ = tx
                    .send(Command::SampleFinished {
                        account_id,
                        incarnation,
                        at,
                    })
                    .await;
            });
        }
    }

    fn write(&self, writes: Vec<StateWrite>) {
        if !writes.is_empty() {
            let _ = self.writer.send(writes);
        }
    }
}

/// The snapshot's account under the id while it is still the holder a report was formed about, `None` once the
/// account is gone or was deleted and created again, which resets both generations to the values the old holder
/// started at.
fn holder<'a>(
    snapshot: &'a RegistrySnapshot,
    account_id: &str,
    incarnation: u64,
) -> Option<&'a phoebis_store::AccountRow> {
    let account = snapshot
        .account(account_id)
        .filter(|account| account.incarnation == incarnation);
    if account.is_none() {
        tracing::debug!(account = %account_id, "a report named a holder the id has moved past");
    }
    account
}

/// The sampler's bookkeeping per account id. An account is sampled again once its last sample is a TTL old,
/// dated when the sample landed rather than when the fetch left, so a failed fetch leaves it due at the next
/// selection instead of unsampled for a whole TTL. The in-flight mark is what keeps two selections from
/// fetching the same account twice meanwhile.
#[derive(Default)]
struct Sampling {
    in_flight: HashSet<String>,
    sampled_at: HashMap<String, i64>,
}

impl Sampling {
    fn due(&self, account_id: &str, now: i64, ttl: i64) -> bool {
        !self.in_flight.contains(account_id)
            && self
                .sampled_at
                .get(account_id)
                .is_none_or(|at| now - at >= ttl)
    }

    fn begin(&mut self, account_id: &str) {
        self.in_flight.insert(account_id.to_string());
    }

    fn finish(&mut self, account_id: &str, at: Option<i64>) {
        self.in_flight.remove(account_id);
        if let Some(at) = at {
            self.sampled_at.insert(account_id.to_string(), at);
        }
    }

    /// Dates a sample that arrived without a fetch, leaving a fetch that is out to finish on its own. An older
    /// head landing late never moves the date back.
    fn stamp(&mut self, account_id: &str, at: i64) {
        let sampled_at = self.sampled_at.entry(account_id.to_string()).or_insert(at);
        *sampled_at = (*sampled_at).max(at);
    }

    fn forget(&mut self, account_id: &str) {
        self.in_flight.remove(account_id);
        self.sampled_at.remove(account_id);
    }
}

/// Sends a failed `SampleFinished` if the sample task it guards ends without sending its own: a panic inside
/// `sample_from_usage`, the one synchronous step of that task, unwinds through this guard first, and without
/// it the account's in-flight mark would outlive the panicked fetch until a reload or restart cleared it.
struct SampleGuard {
    account_id: String,
    incarnation: u64,
    tx: mpsc::Sender<Command>,
    done: bool,
}

impl SampleGuard {
    fn new(account_id: &str, incarnation: u64, tx: mpsc::Sender<Command>) -> SampleGuard {
        SampleGuard {
            account_id: account_id.to_string(),
            incarnation,
            tx,
            done: false,
        }
    }

    /// Marks the task's own `SampleFinished` as sent, so the drop below sends nothing more.
    fn finish(&mut self) {
        self.done = true;
    }
}

impl Drop for SampleGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let _ = self.tx.try_send(Command::SampleFinished {
            account_id: std::mem::take(&mut self.account_id),
            incarnation: self.incarnation,
            at: None,
        });
    }
}

/// Whether a selection has to read a candidate's usage. The pool's own reasons stand, and a candidate with a
/// live rate-limit cooldown row of any scope is read whatever the strategy: while the account is cooled no
/// response head of its own can show the window the upstream reset ahead of the deadline it announced, and the
/// one row `cooldown` answers for the model can be a permission row standing over a rate-limit row, or miss a
/// rate-limit row on another model's scope. A candidate whose latest sample spent its model-scoped window is
/// read for the same reason: selection excludes it on that reading, so no request of its own will ever bring a
/// fresher one. The kind order is a third: it reads overage evidence to put an account in the second group, and
/// nothing but a sample retires that evidence, so without one a recovered account would sit below its
/// plan-funded siblings until the process restarts. The spent move is a fourth: it moves a pin only onto a
/// sibling whose sample is younger than the TTL. Pace is a fifth: it ranks by the readings themselves, and a quiet
/// sibling brings no head of its own to refresh them.
fn needs_sample(
    pool: &phoebis_store::PoolRow,
    state: &RoutingState,
    account_id: &str,
    now: i64,
) -> bool {
    pool.strategy == phoebis_store::Strategy::Pace
        || pool.kind_ordered()
        || pool.shedding.migrate_when_spent
        || pool.migrate_above_utilization.is_some()
        || pool.select_below_utilization.is_some()
        || pool.overage_policy != phoebis_store::OveragePolicy::Allow
        || state
            .cooldowns_of(account_id, now)
            .any(|row| row.reason == "rate_limit")
        || state
            .sample(account_id)
            .and_then(|s| s.reading(SEVEN_DAY_FABLE))
            .is_some_and(|r| r.utilization >= 1.0)
}

/// `incarnation` is the holder of the account id the snapshot was fetched for, which the evidence names.
pub fn sample_from_usage(
    snapshot: &UsageSnapshot,
    incarnation: u64,
) -> (UtilizationSample, Option<OverageEvidence>) {
    let window = |key: &str| snapshot.windows.iter().find(|w| w.key == key);
    let mut sample = UtilizationSample {
        observed_at: snapshot.fetched_at,
        ..Default::default()
    };
    for (name, utilization, reset) in [
        (
            FIVE_HOUR,
            window("five_hour").and_then(|w| w.utilization),
            window("five_hour").and_then(|w| w.resets_at),
        ),
        (
            SEVEN_DAY,
            window("seven_day").and_then(|w| w.utilization),
            window("seven_day").and_then(|w| w.resets_at),
        ),
        (
            SEVEN_DAY_FABLE,
            snapshot.fable_window.as_ref().and_then(|w| w.utilization),
            snapshot.fable_window.as_ref().and_then(|w| w.resets_at),
        ),
    ] {
        if let Some(utilization) = utilization {
            sample = sample.with(
                name,
                WindowReading {
                    utilization: utilization / 100.0,
                    reset,
                    observed_at: snapshot.fetched_at,
                },
            );
        }
    }
    let overage = match (
        sample.reading(FIVE_HOUR).map(|r| r.utilization),
        sample.reading(SEVEN_DAY).map(|r| r.utilization),
    ) {
        (None, None) => None,
        (five, seven) => {
            let enabled = snapshot.extra_usage.as_ref().is_some_and(|e| e.is_enabled);
            let full = five.unwrap_or(0.0) >= 1.0 || seven.unwrap_or(0.0) >= 1.0;
            // The latest reset among the exhausted windows: under `never` the account stays out until every full
            // window has reset, not until the first one has.
            let reset = [FIVE_HOUR, SEVEN_DAY]
                .into_iter()
                .filter_map(|name| sample.reading(name))
                .filter(|r| r.utilization >= 1.0)
                .filter_map(|r| r.reset)
                .max();
            Some(OverageEvidence {
                in_use: enabled && full,
                since: None,
                reset,
                observed_at: snapshot.fetched_at,
                source: EvidenceSource::Sample,
                incarnation,
            })
        }
    };
    (sample, overage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::{
        Affinity, Failover, OveragePolicy, PoolRow, QuotaHeaders, RoutingLoad, Shedding, Strategy,
    };

    /// A pool that reads no utilization of its own: no threshold, no strategy that orders by a window, an
    /// overage policy that never keeps an account out, and no spent move.
    fn plain_pool() -> PoolRow {
        PoolRow {
            id: "p".into(),
            label: "p".into(),
            enabled: true,
            strategy: Strategy::Priority,
            affinity_ttl_seconds: 3_600,
            overage_policy: OveragePolicy::Allow,
            migrate_above_utilization: None,
            select_below_utilization: None,
            model_rules: Vec::new(),
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: false,
            shedding: Shedding {
                migrate_when_spent: false,
                ..Shedding::default()
            },
            failover: Failover::default(),
            cache_ttl_json: "{}".into(),
            cache_diagnostics: None,
            affinity: Affinity::default(),
            quota_headers: QuotaHeaders::Pass,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_pool_that_moves_a_spent_pin_is_sampled_for_the_destinations_fresh_reading() {
        let quiet = RoutingState::from_load(RoutingLoad::default());
        let spent_moves = PoolRow {
            shedding: Shedding::default(),
            ..plain_pool()
        };
        assert!(
            !needs_sample(&plain_pool(), &quiet, "a", 1_000),
            "the same pool without the spent move reads no usage at all"
        );
        assert!(
            needs_sample(&spent_moves, &quiet, "a", 1_000),
            "a spent pin moves only onto a sibling with a fresh sample, and only sampling brings one"
        );
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
                incarnation: 1,
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
                observed_at: 1_000,
                ..Default::default()
            }
            .with(
                SEVEN_DAY_FABLE,
                WindowReading {
                    utilization: 1.0,
                    reset: None,
                    observed_at: 1_000,
                },
            )
            .with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.1,
                    reset: None,
                    observed_at: 1_000,
                },
            )
            .with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.1,
                    reset: None,
                    observed_at: 1_000,
                },
            ),
        );
        assert!(
            needs_sample(&plain_pool(), &spent, "a", 2_000),
            "selection excludes on this reading, so nothing but another sample can undo it"
        );
    }

    #[test]
    fn a_candidate_under_a_live_rate_limit_cooldown_is_sampled_whatever_the_strategy() {
        let pool = plain_pool();
        let quiet = RoutingState::from_load(RoutingLoad::default());
        let needs = |pool: &PoolRow, state: &RoutingState| needs_sample(pool, state, "a", 1_000);
        assert!(!needs(&pool, &quiet), "nothing in this pool reads usage");
        assert!(
            needs(&pool, &cooled(2_000, "rate_limit")),
            "a cooled candidate is re-checked under priority with no threshold set"
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
    fn a_live_rate_limit_row_of_any_scope_is_read_past_the_row_the_selector_answers_for_the_model()
    {
        let pool = plain_pool();
        let rows = |rows: &[(&str, i64, &str)]| {
            RoutingState::from_load(RoutingLoad {
                cooldowns: rows
                    .iter()
                    .map(|(scope, until, reason)| CooldownRow {
                        account_id: "a".into(),
                        scope: (*scope).into(),
                        until: *until,
                        set_at: 0,
                        reason: (*reason).into(),
                        window: "five_hour".into(),
                        source_request_id: None,
                        incarnation: 1,
                    })
                    .collect(),
                ..RoutingLoad::default()
            })
        };
        let needs = |state: &RoutingState| needs_sample(&pool, state, "a", 1_000);
        // The model row is a permission row with the later deadline, the one `cooldown` answers; the
        // account-wide rate-limit row under it is the one a sample can release.
        assert!(
            needs(&rows(&[
                ("account", 2_000, "rate_limit"),
                ("model:claude-sonnet-5", 3_000, "permission"),
            ])),
            "a later permission row on the model does not hide a live rate-limit row on the account"
        );
        assert!(
            needs(&rows(&[("model:claude-fable-5-1", 2_000, "rate_limit")])),
            "a rate-limit row on another model's scope is still this account's window to re-read"
        );
        assert!(
            !needs(&rows(&[
                ("model:claude-fable-5-1", 500, "rate_limit"),
                ("account", 3_000, "permission"),
            ])),
            "an expired rate-limit row and a live permission row leave nothing a sample could release"
        );
    }

    #[test]
    fn a_failed_usage_fetch_leaves_the_account_due_for_the_next_sample() {
        let pool = PoolRow {
            migrate_above_utilization: Some(0.8),
            ..plain_pool()
        };
        let state = RoutingState::from_load(RoutingLoad::default());
        let ttl = 60_000;
        let mut sampling = Sampling::default();
        let due = |sampling: &Sampling, now: i64| {
            sampling.due("a", now, ttl) && needs_sample(&pool, &state, "a", now)
        };
        assert!(due(&sampling, 1_000));
        sampling.begin("a");
        assert!(
            !due(&sampling, 1_000),
            "a second selection while the fetch is out asks nothing"
        );
        sampling.finish("a", None);
        assert!(
            due(&sampling, 2_000),
            "the fetch failed, so the next selection a second later asks again"
        );
        sampling.begin("a");
        sampling.finish("a", Some(2_500));
        assert!(
            !due(&sampling, 3_000),
            "a sample that landed stands for the TTL"
        );
        assert!(due(&sampling, 2_500 + ttl));
    }

    #[tokio::test]
    async fn a_sample_guard_dropped_without_finishing_sends_a_failed_sample_finished() {
        let (tx, mut rx) = mpsc::channel(1);
        {
            let _guard = SampleGuard::new("a", 3, tx);
        }
        let Some(Command::SampleFinished {
            account_id,
            incarnation,
            at,
        }) = rx.recv().await
        else {
            panic!("the drop must send SampleFinished on its own");
        };
        assert_eq!((account_id.as_str(), incarnation, at), ("a", 3, None));
    }

    #[tokio::test]
    async fn a_sample_guard_that_finished_sends_nothing_on_drop() {
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut guard = SampleGuard::new("a", 3, tx);
            guard.finish();
        }
        assert!(
            rx.try_recv().is_err(),
            "the task's own send is the only one, once it finished"
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
            !needs_sample(&plain_pool(), &quiet, "a", 1_000),
            "the same pool without the preference reads no usage at all"
        );
        assert!(
            needs_sample(&kind_ordered, &quiet, "a", 1_000),
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
                incarnation: 1,
            },
        );
        assert!(
            needs_sample(&kind_ordered, &recovered, "a", 1_000),
            "an account already in the second group is exactly the one whose evidence has to be refreshed"
        );
    }
}
