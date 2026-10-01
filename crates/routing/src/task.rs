//! The router task: one loop that owns the routing state, answers selections over a channel, reloads the
//! registry snapshot on request, folds the readings and the usage the relay and the poll report, and writes state
//! through a single writer so the store sees every change in the order the loop made it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use phoebis_oauth::{Clock, UsageSnapshot};
use phoebis_store::{
    AccountState, CooldownRow, PinBurn, PinKey, PinRow, RekeyWrite, Store, StoreError,
};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::affinity::{
    AffinityMap, AffinityReport, ConversationLookup, ConversationMatch, ConversationQuery,
};
use crate::select::{release_reasons, select, MigrateReason, Refusal, SelectRequest, SelectResult};
use crate::state::{
    burn_folded, AccountTicket, CreditsState, EvidenceSource, InFlight, MemberSpend,
    OverageEvidence, PinRef, PinUsage, RateTable, RoutingState, SharePricer, StateWrite,
    UtilizationSample, WindowReading, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use crate::{resolve, window_for, KeyRefusal, Presented, RegistrySnapshot};

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
    /// The meter's peak band at a renewal's instant: a pin's banded copy folds its tokens times it, the plain
    /// copy does not.
    pub band: BandWeight,
    pub purge_interval: Duration,
    /// The dollar table and the cost table's 5-hour share table the shedding round prices each pin's burn
    /// through when it reads it, and the fast table a renewal of a fast answer weighs the plain copy's fold by. The
    /// task holds them because neither this crate nor the relay can see the ledger's tables.
    pub rates: BurnRates,
    /// The share of an account's windows a completed request consumed; the default prices nothing.
    pub share: SharePricer,
    pub touch_interval: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            band: BandWeight::default(),
            purge_interval: Duration::from_secs(60),
            rates: BurnRates::default(),
            share: SharePricer::default(),
            touch_interval: Duration::from_secs(60),
        }
    }
}

/// The two tables a reader prices a pin's burn through, dollars per token and the cost table's 5-hour share on
/// its reference tier per token, and the fast cells a fast renewal is weighed by. All three default to
/// `RateTable::unit`, so a burn read through the default is a token count and a fast renewal folds unweighed.
#[derive(Clone, Debug, PartialEq)]
pub struct BurnRates {
    pub dollars: RateTable,
    pub five_hour: RateTable,
    /// Dollars per token at the fast cells; over `dollars` it is the weight a fast renewal folds with.
    pub fast: RateTable,
}

impl Default for BurnRates {
    fn default() -> Self {
        BurnRates {
            dollars: RateTable::unit(),
            five_hour: RateTable::unit(),
            fast: RateTable::unit(),
        }
    }
}

/// Per token type, the fast cell over the standard cell of `model`: 1 where the standard rate is 0, since no
/// weight can carry a charge the standard table prices at nothing, and 1 where either table has no rate for the
/// type, as for a model with no fast price.
fn fast_over_standard(rates: &BurnRates, model: &str) -> PinBurn {
    let (fast, standard) = (rates.fast.rates_for(model), rates.dollars.rates_for(model));
    let weight = |fast: Option<f64>, standard: Option<f64>| match (fast, standard) {
        (Some(fast), Some(standard)) if standard != 0.0 => fast / standard,
        _ => 1.0,
    };
    PinBurn {
        cache_read: weight(fast.cache_read, standard.cache_read),
        input: weight(fast.input, standard.input),
        output: weight(fast.output, standard.output),
        write_1h: weight(fast.write_1h, standard.write_1h),
        write_5m: weight(fast.write_5m, standard.write_5m),
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
    /// The poll's newest extra-usage state of an account, which decides fast eligibility.
    Credits {
        account_id: String,
        state: CreditsState,
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
        /// The cache TTL the relay resolved for the attempt: the policy's rewrite, else the client's marker.
        cache_ttl_ms: i64,
        /// The answer reported `usage.speed` `fast`.
        fast: bool,
    },
    Sample {
        account_id: String,
        sample: UtilizationSample,
        incarnation: u64,
    },
    /// A finished inference or probe answer's typed tokens on the account that served it, pinned or not: what the
    /// prediction adds to the account's last reading until the next one lands.
    Usage {
        account_id: String,
        at: i64,
        canonical_model: String,
        incarnation: u64,
        usage: PinUsage,
        /// The answer reported `usage.speed` `fast`.
        fast: bool,
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
    ReleaseSession {
        key_id: String,
        session_id: String,
        reply: oneshot::Sender<usize>,
    },
    Reload {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The sequence number an affinity report took at enqueue, `None` for every other report.
    Report(Report, Option<u64>),
    #[cfg(any(test, feature = "debug-hooks"))]
    HoldReports {
        reply: oneshot::Sender<()>,
    },
    #[cfg(any(test, feature = "debug-hooks"))]
    ReleaseReports {
        reply: oneshot::Sender<()>,
    },
    Resolve {
        spelling: String,
        dated_id: String,
        at: i64,
        reply: oneshot::Sender<()>,
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

/// The sequence of affinity reports: assigned at enqueue under one lock with the send, so the order of the
/// numbers is the order of the queue; `applied` is the last one the task applied or passed over, published
/// for the relay's bounded wait.
#[derive(Default)]
struct ReportSequence {
    next: Mutex<u64>,
    applied: AtomicU64,
    applied_notify: Notify,
    /// Notified when a wait registers on a sequence `applied` has not reached; read by `ReportHold::waiter`.
    #[cfg(any(test, feature = "debug-hooks"))]
    waiter_registered: Notify,
}

impl ReportSequence {
    fn publish(&self, seq: u64) {
        self.applied.store(seq, Ordering::Release);
        self.applied_notify.notify_waiters();
    }
}

#[derive(Clone)]
pub struct RouterHandle {
    affinity: Arc<RwLock<AffinityMap>>,
    events: broadcast::Sender<RouterEvent>,
    in_flight: InFlight,
    reports: Arc<ReportSequence>,
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
    /// request rather than stalling one. For `Report::Affinity` the sequence number the report took at enqueue,
    /// `None` for every other report and for a report the full or closed queue dropped.
    pub fn report(&self, report: Report) -> Option<u64> {
        if !matches!(report, Report::Affinity(_)) {
            Self::sent(self.tx.try_send(Command::Report(report, None)));
            return None;
        }
        // Held across the send, so a later number never enters the queue ahead of an earlier one and the
        // watermark the task publishes in queue order covers every number below it.
        let mut next = self
            .reports
            .next
            .lock()
            .expect("report sequence lock poisoned");
        let seq = *next + 1;
        Self::sent(self.tx.try_send(Command::Report(report, Some(seq)))).then(|| {
            *next = seq;
            seq
        })
    }

    fn sent(result: Result<(), mpsc::error::TrySendError<Command>>) -> bool {
        match result {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("the router task's queue is full, a report was dropped");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("the router task has stopped, a report was dropped");
                false
            }
        }
    }

    /// The sequence of the last affinity report the task applied.
    pub fn applied_report(&self) -> u64 {
        self.reports.applied.load(Ordering::Acquire)
    }

    /// Resolves true once the task has applied the report numbered `seq`, false at `deadline`; registered
    /// before the sequence is read, as `InFlight::wait_for_slot` registers its waiter.
    pub async fn wait_for_report(&self, seq: u64, deadline: Instant) -> bool {
        loop {
            // Registered before the watermark is read, so a report applied between the read and the wait
            // still wakes this waiter.
            let applied = self.reports.applied_notify.notified();
            tokio::pin!(applied);
            applied.as_mut().enable();
            if self.applied_report() >= seq {
                return true;
            }
            #[cfg(any(test, feature = "debug-hooks"))]
            self.reports.waiter_registered.notify_one();
            if tokio::time::timeout_at(deadline, applied).await.is_err() {
                return self.applied_report() >= seq;
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
    pub async fn cooldowns(&self) -> Result<Vec<CooldownRow>, RouterGone> {
        self.ask(|reply| Command::Cooldowns { reply }).await
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
    /// One ask for every account's latest utilization sample, each plan window predicted at the task's clock, so
    /// a rule sweep never queues one command per account in front of selection.
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

/// Compiled only under the `debug-hooks` feature, which a dependent's dev-dependency turns on, so a release build
/// carries no way to stall the task's reports.
#[cfg(any(test, feature = "debug-hooks"))]
impl RouterHandle {
    /// Answers once the task parks every affinity report it receives, unapplied and with the watermark behind
    /// it, until the hold is released; every other command is served as usual.
    pub async fn hold_reports(&self) -> ReportHold {
        self.ask(|reply| Command::HoldReports { reply })
            .await
            .expect("the router task is running");
        ReportHold {
            handle: self.clone(),
        }
    }
}

/// The task's affinity reports parked from `RouterHandle::hold_reports` until `release`.
#[cfg(any(test, feature = "debug-hooks"))]
pub struct ReportHold {
    handle: RouterHandle,
}

#[cfg(any(test, feature = "debug-hooks"))]
impl ReportHold {
    /// Resolves once a `wait_for_report` has registered on a sequence the watermark has not reached.
    pub async fn waiter(&self) {
        self.handle.reports.waiter_registered.notified().await;
    }

    /// Applies the parked reports in their order, publishes the watermark, and answers once that is done.
    pub async fn release(self) {
        self.handle
            .ask(|reply| Command::ReleaseReports { reply })
            .await
            .expect("the router task is running");
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
    /// The affinity reports a test's hold keeps unapplied, in queue order; `None` while no hold is on.
    #[cfg(any(test, feature = "debug-hooks"))]
    parked: Option<Vec<(Report, u64)>>,
    reports: Arc<ReportSequence>,
    revision: u64,
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    state: RoutingState,
    store: Arc<Store>,
    /// The rotation each slot's last touch was stamped under, beside its timestamp: a touch under a different
    /// rotation is a first sight of that presentation, not a repeat the grace period should swallow.
    touched_at: HashMap<(String, Presented), (i64, Option<i64>)>,
    writer: mpsc::UnboundedSender<Vec<StateWrite>>,
    /// Resolves once the writer has landed everything it was sent and exited; `shutdown` waits on it.
    writer_done: Option<oneshot::Receiver<()>>,
}

pub fn spawn(
    store: Arc<Store>,
    clock: Clock,
    config: RouterConfig,
) -> Result<RouterHandle, StoreError> {
    let now = clock();
    store.purge_cooldowns_before(now)?;
    store.purge_stale_windows()?;
    let snapshot = Arc::new(RwLock::new(Arc::new(RegistrySnapshot::from_load(
        store.load_registry()?,
        store.digest_key(),
        now,
        1,
    ))));
    let mut state = RoutingState::from_load(store.load_routing()?);
    // One table, shared by the handle (the relay's synchronous permits and tickets) and the state (the
    // selector's cap partition), so a count is exact wherever it is read.
    let in_flight = InFlight::default();
    state.set_in_flight(in_flight.clone());
    state.set_rates(config.rates.clone());
    // Shared the same way, so the relay's conversation climb reads the map under the same lock the report
    // handler writes, without a command round trip.
    let affinity = state.affinity();
    let (tx, mut rx) = mpsc::channel(COMMANDS);
    // Unbounded so the loop never blocks on SQLite: a stalled store grows this queue in memory rather than
    // stalling selection, and bounding the process's memory is the operator's call, not this channel's.
    let (writer, mut writes) = mpsc::unbounded_channel::<Vec<StateWrite>>();
    let (done_tx, done_rx) = oneshot::channel();
    let (events, _) = broadcast::channel(EVENTS);
    let reports = Arc::new(ReportSequence::default());
    let handle = RouterHandle {
        affinity,
        events: events.clone(),
        in_flight,
        reports: reports.clone(),
        snapshot: snapshot.clone(),
        tx: tx.clone(),
    };
    let writer_store = store.clone();
    // Weak: a strong sender would keep the loop running after the last handle is gone, and with it the process
    // lock a reopened on-disk store needs back.
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
        #[cfg(any(test, feature = "debug-hooks"))]
        parked: None,
        reports,
        revision: 1,
        snapshot,
        state,
        store,
        touched_at: HashMap::new(),
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
                        // A held report lands with the drain like any other, ahead of the ones queued after it.
                        #[cfg(any(test, feature = "debug-hooks"))]
                        task.release_reports();
                        while let Some(command) = rx.recv().await {
                            match command {
                                Command::Report(report, seq) => task.numbered_report(report, seq),
                                // A resolution is a report with an answer: it lands with the drain rather than
                                // leaving the catalog cache to publish the spelling again after a restart.
                                Command::Resolve { spelling, dated_id, at, reply } => {
                                    task.resolve(&spelling, &dated_id, at);
                                    let _ = reply.send(());
                                }
                                _ => {}
                            }
                        }
                        // A clean shutdown loses none of the refreshes since the last purge tick.
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
            StateWrite::PurgeStaleWindows => {
                store.purge_stale_windows()?;
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
            StateWrite::UpsertCredits(row) => {
                store.upsert_credits(&row)?;
            }
            StateWrite::UpsertOverage(row) => {
                store.upsert_overage(&row)?;
            }
            StateWrite::UpsertPin(row) => store.upsert_pin(&row)?,
            StateWrite::UpsertWindow(row) => {
                store.upsert_window(&row)?;
            }
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
                // The lane's model part with it: admitted before the resolution, the lane still names the spelling
                // the pin was relaned from, and would read that pin as another cache.
                request.lane = self.state.canonical_lane(&request.lane);
                if request.revision < snapshot.revision {
                    // The registry moved after admission: the key, its policy, the pool, and the candidates are
                    // re-derived from what is current, and a request the current registry would refuse is refused.
                    let now = (self.clock)();
                    let key = match snapshot.authenticate_id(&request.key_id, now) {
                        Ok(key) => key,
                        Err(refusal) => {
                            let _ = reply.send(SelectAnswer {
                                result: SelectResult::Refused(Refusal::Key(refusal)),
                                cancel: CancellationToken::new(),
                                ticket: None,
                            });
                            return;
                        }
                    };
                    // The id is reusable, so the lifetime is the identity: a key created again under the id
                    // since admission is another key, and the request is refused as a refused id is.
                    if key.created_at != request.key_generation {
                        let _ = reply.send(SelectAnswer {
                            result: SelectResult::Refused(Refusal::Key(KeyRefusal::Replaced)),
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
                                result: SelectResult::Refused(Refusal::Resolve(refusal)),
                                cancel: CancellationToken::new(),
                                ticket: None,
                            });
                            return;
                        }
                    }
                    request.revision = snapshot.revision;
                }
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
                    // selection answered in between read the same count and land over the cap. A token count
                    // takes none: it is bound to no cap.
                    SelectResult::Selected(selection) => (
                        self.token(&selection.account.id).child_token(),
                        (!request.count).then(|| {
                            self.state
                                .in_flight()
                                .dispatched(&selection.account.id, request.now)
                        }),
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
            Command::Report(report, seq) => self.numbered_report(report, seq),
            #[cfg(any(test, feature = "debug-hooks"))]
            Command::HoldReports { reply } => {
                self.parked.get_or_insert_with(Vec::new);
                let _ = reply.send(());
            }
            #[cfg(any(test, feature = "debug-hooks"))]
            Command::ReleaseReports { reply } => {
                self.release_reports();
                let _ = reply.send(());
            }
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
            Command::Cooldowns { reply } => {
                let _ = reply.send(self.state.cooldowns());
            }
            Command::MemberSpends { reply } => {
                let snapshot = self
                    .snapshot
                    .read()
                    .expect("snapshot lock poisoned")
                    .clone();
                let rows = snapshot
                    .budgeted_members()
                    .flat_map(|m| {
                        self.state
                            .member_spends_live(&snapshot, &m.pool_id, &m.account_id)
                            .into_iter()
                            .cloned()
                    })
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
                let now = (self.clock)();
                let samples = self
                    .state
                    .samples()
                    .into_iter()
                    .map(|(id, read)| {
                        let predicted = self.state.predicted_sample(&id, now).unwrap_or(read);
                        (id, predicted)
                    })
                    .collect();
                let _ = reply.send(samples);
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

    /// Applies the report, then for an affinity report publishes its number: a report `moved_past` passes over
    /// is drained the same, so a waiter on it never waits out its bound.
    fn numbered_report(&mut self, report: Report, seq: Option<u64>) {
        #[cfg(any(test, feature = "debug-hooks"))]
        if let (Some(parked), Some(seq)) = (self.parked.as_mut(), seq) {
            parked.push((report, seq));
            return;
        }
        self.report(report);
        if let Some(seq) = seq {
            self.reports.publish(seq);
        }
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    fn release_reports(&mut self) {
        for (report, seq) in self.parked.take().unwrap_or_default() {
            self.report(report);
            self.reports.publish(seq);
        }
    }

    fn report(&mut self, report: Report) {
        match report {
            Report::Affinity(mut report) => {
                if self.moved_past(&report.account_id, report.incarnation) {
                    return;
                }
                // The map files the entry under the resolved model, so its lane names that model too: an answer
                // selected before the resolution landed still reports the spelling admission read.
                report.lane = self.state.canonical_lane(&report.lane);
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
                // A fast-lane row moves the session for fast mode alone; the name tells the drawer it was no
                // rate limit on the account's standard traffic.
                let moved = if row.scope == "fast" || row.scope == "fast_credits" {
                    "fast_lane"
                } else {
                    "cooldown"
                };
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
                    let key = PinKey {
                        key_id: pin.key_id.clone(),
                        session_id: pin.session_id.clone(),
                        agent_id: pin.agent_id.clone(),
                        model: pin.model.clone(),
                    };
                    // A fast-lane row refuses the fast lane alone: once a later turn moved the pin to another
                    // lane, the session no longer asks for what the row refuses, so a late refusal leaves it.
                    let left_lane = moved == "fast_lane"
                        && self.state.pin(&key).is_some_and(|current| {
                            current.lane != self.state.canonical_lane(&pin.lane)
                        });
                    if left_lane {
                        tracing::debug!(
                            key_id = %key.key_id,
                            session_id = %key.session_id,
                            agent_id = %key.agent_id,
                            model = %key.model,
                            report_lane = %pin.lane,
                            pin_lane = %self.state.pin(&key).map_or("", |p| p.lane.as_str()),
                            "a fast refusal from a lane the pin has left leaves the pin"
                        );
                    } else if self.state.invalidate_pin(&pin, &account_id) {
                        self.invalidated
                            .insert(key.clone(), (account_id.clone(), moved, (self.clock)()));
                        writes.push(StateWrite::DeletePin(key));
                    }
                }
                self.write(writes);
            }
            Report::Credits {
                account_id,
                mut state,
                incarnation,
            } => {
                if self.moved_past(&account_id, incarnation) {
                    return;
                }
                state.incarnation = incarnation;
                let observed_at = state.observed_at;
                let room = state.blocked().is_none();
                let row = RoutingState::credits_row(&account_id, &state);
                if !self.state.record_credits(&account_id, state) {
                    return;
                }
                self.write(vec![StateWrite::UpsertCredits(row)]);
                if !room {
                    return;
                }
                // Credits with room lift a refusal only when read after it: an earlier reading says nothing about
                // the state that refused the request. They lift the credits row alone, since the fast lane's rate
                // limit beside it is a bucket the reading does not measure.
                let now = (self.clock)();
                let refused = self
                    .state
                    .cooldowns_of(&account_id, now)
                    .any(|row| row.scope == "fast_credits" && observed_at > row.set_at);
                if refused {
                    self.release_cooldown(account_id, "fast_credits".to_string(), "recovered");
                }
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
                cache_ttl_ms,
                fast,
            } => {
                pin.model = self.state.canonical(&pin.model).to_string();
                let key = PinKey {
                    key_id: pin.key_id.clone(),
                    session_id: pin.session_id.clone(),
                    agent_id: pin.agent_id.clone(),
                    model: pin.model.clone(),
                };
                if let Some(current) = self.state.pin(&key).cloned() {
                    // A probe's renewal is the refresh of the cache it read, which is another cache once the session
                    // changed lanes while it was in flight. A client's renewal folds its own burn whatever lane
                    // the pin moved to since, so it is not held to the lane.
                    let left_lane = origin == "keepalive"
                        && current.lane != self.state.canonical_lane(&pin.lane);
                    if left_lane {
                        tracing::debug!(
                            key_id = %key.key_id,
                            session_id = %key.session_id,
                            agent_id = %key.agent_id,
                            model = %key.model,
                            report_lane = %pin.lane,
                            pin_lane = %current.lane,
                            "a probe renewal from a lane the pin has left leaves the pin"
                        );
                    }
                    if current.epoch == pin.epoch && !left_lane {
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
                            // Both copies fold whatever the account's kind and nothing is priced here: the
                            // reader prices the copy its funding selects on the account the pin sits on when it
                            // reads, so leaving overage or moving between tiers needs no reset.
                            let window_ms = self.burn_window_ms(&pin.key_id);
                            // The meter charges a request inside the peak band more, so the banded copy weighs
                            // it by the band at the renewal; dollars cost the same at every hour.
                            let band = self.config.band.at(FIVE_HOUR, now);
                            let at = renewed.burn_at;
                            let fold = |rate: f64, tokens: u64, weight: f64| {
                                burn_folded(rate, at, now, window_ms, tokens as f64 * weight)
                            };
                            let folded = |copy: PinBurn, w: PinBurn| PinBurn {
                                cache_read: fold(copy.cache_read, usage.cache_read, w.cache_read),
                                input: fold(copy.input, usage.input, w.input),
                                output: fold(copy.output, usage.output, w.output),
                                write_1h: fold(copy.write_1h, usage.write_1h, w.write_1h),
                                write_5m: fold(copy.write_5m, usage.write_5m, w.write_5m),
                            };
                            let uniform = |w: f64| PinBurn {
                                cache_read: w,
                                input: w,
                                output: w,
                                write_1h: w,
                                write_5m: w,
                            };
                            // A fast answer is usage credits at the fast cells: the plain copy carries each type
                            // weighed by fast over standard, so pricing it at the standard table reads the fast
                            // dollars, and the banded copy, the plan share, only decays.
                            let plain = if fast {
                                fast_over_standard(&self.config.rates, &pin.model)
                            } else {
                                uniform(1.0)
                            };
                            renewed.burn = folded(renewed.burn, plain);
                            renewed.burn_banded =
                                folded(renewed.burn_banded, uniform(if fast { 0.0 } else { band }));
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
                // A cooled account sends no requests, so a window the upstream reset ahead of the deadline it
                // announced reaches the router here and nowhere else. Only a sample newer than the stored one
                // answers for the account now: a head and a poll can cross, and the older one's reading is one
                // the window has already moved past.
                let moved = self.state.record_sample(&account_id, sample.clone());
                if !moved.is_empty() {
                    let now = (self.clock)();
                    for row in self.state.recovered_cooldowns(&account_id, &sample, now) {
                        self.release_cooldown(row.account_id, row.scope, "recovered");
                    }
                    // An api_key bucket refills within a minute, so only the plan windows are worth a restart.
                    let windows: Vec<String> = moved
                        .into_iter()
                        .filter(|w| [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE].contains(&w.as_str()))
                        .collect();
                    let rows = self.state.window_rows(&account_id, &windows, incarnation);
                    self.write(rows.into_iter().map(StateWrite::UpsertWindow).collect());
                }
            }
            Report::Usage {
                account_id,
                at,
                canonical_model,
                incarnation,
                usage,
                fast,
            } => {
                // A fast completion draws usage credits and no plan window.
                if self.moved_past(&account_id, incarnation) || fast {
                    return;
                }
                let snapshot = self
                    .snapshot
                    .read()
                    .expect("snapshot lock poisoned")
                    .clone();
                // An api_key account bills its tokens and has no plan window to charge, and an account whose tier
                // the profile has not named yet has no factor to price through.
                if snapshot.account(&account_id).map(|a| a.kind)
                    != Some(phoebis_store::AccountKind::Oauth)
                {
                    return;
                }
                let Some(tier) = snapshot.tier(&account_id) else {
                    return;
                };
                let price = |window: &str| {
                    self.config
                        .share
                        .price(window, tier, &canonical_model, usage, at)
                };
                let fable = (window_for(&canonical_model) == SEVEN_DAY_FABLE)
                    .then(|| price(SEVEN_DAY_FABLE))
                    .flatten();
                self.state
                    .charge(&account_id, at, price(FIVE_HOUR), price(SEVEN_DAY), fable);
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
        // Samples and charges are keyed by id alone, so a moved holder's go here, before the purge.
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
            .map_or(
                phoebis_store::Shedding::default().burn_window_seconds,
                |pool| pool.shedding.burn_window_seconds,
            );
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
        // A refresh moves an entry's time in place and is written with the purge's batch, so a crash loses at
        // most one tick of them; a reload past the idle TTL would otherwise drop a live conversation's entries.
        let refreshed = self
            .state
            .affinity()
            .write()
            .expect("affinity lock poisoned")
            .drain();
        if !refreshed.is_empty() {
            writes.push(StateWrite::UpsertAffinity(refreshed));
        }
        // The fenced upsert keeps a previous holder's window rows off disk; this sweep is the backstop, so none that
        // got there anyway outlives a tick.
        writes.push(StateWrite::PurgeStaleWindows);
        self.write(writes);
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

/// The poll's extra-usage state, its utilization a fraction as the windows read; `None` when the snapshot carries
/// none. `incarnation` is the holder of the account id the snapshot was fetched for.
pub fn credits_from_usage(snapshot: &UsageSnapshot, incarnation: u64) -> Option<CreditsState> {
    snapshot.extra_usage.as_ref().map(|e| CreditsState {
        enabled: e.is_enabled,
        disabled_reason: e.disabled_reason.clone(),
        utilization: e.utilization.map(|u| u / 100.0),
        observed_at: snapshot.fetched_at,
        incarnation,
    })
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
