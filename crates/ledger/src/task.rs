//! The ledger task: one bounded channel from the assembler and every other producer, one writer that lands
//! batches through `spawn_blocking`, and the gap event that says what was lost.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use phoebis_bus::{spawn_subscriber, Bus, Delivery, Subscriber, SubscriberStats};
use phoebis_oauth::Clock;
use phoebis_routing::{MemberSpend, RegistrySnapshot, Report, RouterHandle, SEVEN_DAY};
use phoebis_store::{AccountKind, BudgetPeriod, Store};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::assemble::{Assembler, Record};
use crate::bodies::{request_cutoff, run_retention};
use crate::db::{
    begin_write, delete_parked_in, insert_decision_in, insert_event_in, insert_request_in,
    insert_samples_in, newest_row_at, update_cache_map_in,
};
use crate::events::{spawn_event_mirror, MirrorBaseline};
use crate::metrics::Metrics;
use crate::plan::{Funding, PlanTables};
use crate::price::is_free_classifier;
use crate::pricing::Pricing;
use crate::query::{
    member_spends, period_bounds, BudgetedMember, BUDGET_PERIOD_ENDED, BUDGET_REACHED, LEDGER_GAP,
    ROUTER_STARTED,
};
use crate::record::{AttemptRecord, EventRecord, ParkedRecord, ParkedRows, RequestRecord};
use crate::{LedgerDb, LedgerError, WriterRead};

pub const CHANNEL_DEPTH: usize = 4_096;

/// The writer channel's fill at which admission stops accepting money: half the channel, so the stall moves
/// before the send while the relay's facts, which the bus queue holds apart, still land behind it.
pub const LEDGER_BEHIND_DEPTH: usize = 2_048;

/// Records landed per transaction: enough to amortise the fsync, few enough that a drain on shutdown is quick.
const BATCH: usize = 64;

/// The in-place retries of a batch whose transaction failed, on the ledger's clock: four tries over fifteen
/// seconds cover a checkpoint, a backup's lock or a disk's bad moment, while the channel fills behind the held
/// batch so admission refuses before more money is spent.
pub const BATCH_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// The interval a batch the database as a whole refused, even one record at a time, is retried at, until it lands
/// or the process stops.
pub const BATCH_RETRY_HOLD: Duration = Duration::from_secs(10);

/// How often a wait on the ledger's clock reads it again: a clock a test steps is seen within a tick, and
/// production time passes as it does.
const CLOCK_TICK: Duration = Duration::from_millis(250);

/// How often the assembler ages out entries without a completion, unless the configuration says otherwise.
const EXPIRE_INTERVAL: Duration = Duration::from_secs(60);

const TASKS_POISONED: &str = "ledger background-task list poisoned by a prior panic";

const UNPRICED_POISONED: &str = "ledger unpriced-model map poisoned by a prior panic";

#[derive(Clone, Debug)]
pub struct LedgerConfig {
    pub retention_days: u32,
    pub rollup_retention_days: u32,
    pub purge_interval: Duration,
    pub event_poll: Duration,
    /// How often the assembler ages out entries whose completion never arrived.
    pub expire_interval: Duration,
    /// The binary's version for the `router_started` row `spawn` records ahead of everything it starts. `None`
    /// records none, as a test's ledger does.
    pub start_version: Option<String>,
    /// The event mirror's starting point, read by the caller before the start's recovery writes the registry.
    /// `None` reads it when the mirror spawns, as a test's ledger does.
    pub mirror_baseline: Option<MirrorBaseline>,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        LedgerConfig {
            retention_days: 365,
            rollup_retention_days: 365,
            purge_interval: Duration::from_secs(3_600),
            event_poll: Duration::from_secs(5),
            expire_interval: EXPIRE_INTERVAL,
            start_version: None,
            mirror_baseline: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct LedgerStats {
    pub dropped: AtomicU64,
    pub written: AtomicU64,
    pub write_failures: AtomicU64,
    pub gaps: AtomicU64,
    pub last_gap_at: AtomicI64,
    /// Requests answered `ledger_behind` at admission.
    pub behind_refusals: AtomicU64,
    /// Set from the first failed batch until a batch lands and the spend refresh after it reads the rows.
    pub degraded: AtomicBool,
    /// 1 while a batch is under retry.
    pub retrying: AtomicU64,
    /// Priced records parked since start.
    pub parked_priced: AtomicU64,
    /// Retention passes the purge task finished since start, whatever each one cut.
    pub purge_passes: AtomicU64,
    /// Folds of the routing conditions the purge task finished since start, the one at start among them, whatever
    /// each one found.
    pub condition_folds: AtomicU64,
    /// Cache-map writes whose decision row the ledger never stored (dropped under pressure, or purged), counted
    /// and never retried, since the decision precedes its answer's end by the upstream's whole turn.
    pub cache_map_orphans: AtomicU64,
    /// Router events the mirror's broadcast receiver missed because it lagged, each a decision or an event row
    /// never written, counted apart from `dropped`, which counts the ledger's own records.
    pub mirror_lagged: AtomicU64,
}

/// What the relay's ledger check reads: counted here, since every `behind` answer is one `ledger_behind` refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
    pub behind: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("the ledger task has stopped")]
pub struct LedgerGone;

/// A model the cost model in force gave no weight on the 7-day window when a plan-funded request on it landed, so its
/// figure was null. Kept once seen, so its gauge series reads 0 rather than vanishing after a revision weights the
/// model.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct UnpricedModel {
    /// The undated model id.
    pub model: String,
    /// When the first unpriced landing arrived, `None` once a landing found the model weighted, until the next
    /// unpriced landing dates it again.
    pub since: Option<i64>,
    /// Landed requests with at least one plan-funded attempt with usage that priced nothing since `since`, one per
    /// request, 0 once a landing found the model weighted.
    pub requests: u64,
}

/// The unpriced models by undated id, written by the landing after its commit and read by the attention reader. A
/// `std` mutex, since the reader asks synchronously from inside the runtime, where a tokio lock would have to block.
type Unpriced = Arc<std::sync::Mutex<BTreeMap<String, UnpricedModel>>>;

/// What a replay of the parked records did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Replayed {
    pub landed: u64,
    pub remaining: u64,
}

/// A replay of the parked rows the caller asks the writer to run on its own turn, between two batches.
/// `deadline` is the caller's bound on the ledger's clock, and `claimed` is who owns the replay: the writer takes it
/// (`compare_exchange(false, true)`) at its turn before it runs `land_parked`, the caller takes it at the deadline
/// instead, and whoever loses the exchange stands down. A message the caller claimed is dropped unanswered with a
/// `debug` line naming the deadline. A message the writer claims at or past its deadline is answered `WriterHeld`
/// unrun, which covers a caller that went away before its deadline and so never took the claim. A caller that gave up
/// never has its replay run behind its back, and a message the writer claimed before the deadline answers its real
/// result however long the landing takes.
struct ReplayParked {
    now: i64,
    deadline: i64,
    claimed: Arc<AtomicBool>,
    reply: oneshot::Sender<Result<Replayed, LedgerError>>,
}

#[derive(Clone)]
pub struct Ledger {
    db: Arc<LedgerDb>,
    stats: Arc<LedgerStats>,
    /// The counters of the bus subscriber that assembles the records the writer takes.
    assembler: Arc<SubscriberStats>,
    unpriced: Unpriced,
    metrics: Arc<Metrics>,
    tx: mpsc::Sender<Record>,
    /// Flipped by `shutdown`; the writer closes its receiver on it, so no clone of the handle can keep the
    /// channel open past the drain.
    stop: Arc<watch::Sender<bool>>,
    /// Taken by the first `shutdown`; the writer resolves it once the channel is drained.
    done: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    /// Background tasks the ledger owns beyond the writer, such as the quota poll. Aborted by `shutdown`
    /// before the stop signal goes out, so none of them can enqueue into a channel that is closing.
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// The writer's control channel: a replay asks for the writer's own turn through it.
    control: mpsc::Sender<ReplayParked>,
    /// The writer's clock, which a replay's deadline is read on.
    clock: Clock,
    /// The days a request row is kept, as the configuration set them at the start.
    retention_days: u32,
}

impl Ledger {
    pub fn db(&self) -> &Arc<LedgerDb> {
        &self.db
    }

    pub fn stats(&self) -> Arc<LedgerStats> {
        self.stats.clone()
    }

    /// The days a request row is kept, 0 for no limit: a window whose first reading is older finds no requests to
    /// reconcile against.
    pub fn retention_days(&self) -> u32 {
        self.retention_days
    }

    /// Every model a plan-funded landing priced nothing since the start, by undated model id, a model a later landing
    /// found weighted among them with no `since` and no requests. Empty at each start.
    pub fn unpriced_served(&self) -> Vec<UnpricedModel> {
        self.unpriced
            .lock()
            .expect(UNPRICED_POISONED)
            .values()
            .cloned()
            .collect()
    }

    pub fn dropped(&self) -> u64 {
        self.stats.dropped.load(Ordering::Relaxed)
    }

    /// Whether the ledger is degraded: from a failed batch until a batch lands and the spend refresh after it reads
    /// the rows. The writer sets it; the system page, the metrics and the attention reader read it.
    pub fn degraded(&self) -> bool {
        self.stats.degraded.load(Ordering::Relaxed)
    }

    /// Priced records parked since start, for the metrics; the system page counts the table's rows instead.
    pub fn parked_priced(&self) -> u64 {
        self.stats.parked_priced.load(Ordering::Relaxed)
    }

    /// Whether the writer still takes records and the bus subscriber that assembles them has not panicked: false once
    /// the writer closed its receiver, at a shutdown or a panic, or once the assembler panicked. The writer's channel
    /// stays open while the assembler is gone, so the subscriber's panic counter is the only sign of that half.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed() && self.assembler.panics.load(Ordering::Relaxed) == 0
    }

    /// Records waiting in the writer channel now. The batch the writer already took off the channel is not
    /// among them.
    pub fn queued(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// Whether the writer channel holds `LEDGER_BEHIND_DEPTH` records or more. The mark counts the channel, not
    /// the batch in the writer's hands, so a writer blocked on a write reads behind a batch later.
    pub fn behind(&self) -> bool {
        self.queued() >= LEDGER_BEHIND_DEPTH
    }

    /// `behind` for the relay's ledger check, which reads it once per request: every `behind` answer is one
    /// `ledger_behind` refusal and counts in `behind_refusals`. A refusal of another kind reads `behind` instead.
    pub fn admission(&self) -> Admission {
        let behind = self.behind();
        if behind {
            self.stats.behind_refusals.fetch_add(1, Ordering::Relaxed);
        }
        Admission { behind }
    }

    /// Registers a task the ledger owns beyond the writer, so `shutdown` aborts it before the channel closes:
    /// a fetch still running when the process stops must not keep a strong reference (the store, for
    /// instance) alive past that, and the quota poll must not record into a channel that is closing.
    pub fn track(&self, handle: tokio::task::JoinHandle<()>) {
        self.tasks.lock().expect(TASKS_POISONED).push(handle);
    }

    /// How many handles `shutdown` would abort now: every handle ever tracked, finished or not, the event
    /// mirror, the quota poll's scheduler and the startup step's fetches among them. A scheduled attempt is the
    /// scheduler's own, reaped when it completes, and never counts here.
    pub fn tracked(&self) -> usize {
        self.tasks.lock().expect(TASKS_POISONED).len()
    }

    /// Never waits: the relay's body task and the assembler both call this for an event or a sample, and
    /// neither may stall on disk.
    pub fn record(&self, record: Record) {
        match self.tx.try_send(record) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(record))
            | Err(mpsc::error::TrySendError::Closed(record)) => {
                count_drop(&self.stats, &self.metrics, table_of(&record));
            }
        }
    }

    /// Waits for room in the writer's channel: a request's row must land, or a completed request leaves no
    /// trace at all. A record offered after the channel closed is counted as dropped, as `record` counts one
    /// offered to a full or closed channel.
    pub async fn record_request(&self, record: Record) {
        if let Err(mpsc::error::SendError(record)) = self.tx.send(record).await {
            count_drop(&self.stats, &self.metrics, table_of(&record));
        }
    }

    /// The start-up pass on demand, run by the writer task between two of its batches: every parked row once, in
    /// parking order, each in its own transaction with its row's deletion (`land_acknowledging`), then the spend
    /// read the start runs after its own pass, so the routing table holds the landed money before this answers.
    /// The writer's turn is the serialization: no row is landed twice by two passes, or by a pass and a batch, at
    /// once. `remaining` is the table's count after the pass; `Err` when the writer has stopped or the count cannot be
    /// read.
    ///
    /// Within `bound` on the ledger's clock. One absolute deadline, `clock() + bound`, governs the `send` into the
    /// capacity-one control channel and the wait for the reply alike: `Err(LedgerError::WriterHeld)` once it passes
    /// with the message unsent or unclaimed, and nothing replayed then or later; a message the writer claimed before it
    /// answers the landing's result, past the bound included (the reply arm of the `select!` reads first, `biased`, so
    /// a ready answer is never thrown away).
    pub async fn replay_parked(&self, now: i64, bound: Duration) -> Result<Replayed, LedgerError> {
        let deadline = (self.clock)() + bound.as_millis() as i64;
        let claimed = Arc::new(AtomicBool::new(false));
        let (reply, mut answer) = oneshot::channel();
        let message = ReplayParked {
            now,
            deadline,
            claimed: claimed.clone(),
            reply,
        };
        tokio::select! {
            biased;
            sent = self.control.send(message) => sent.map_err(|_| LedgerGone)?,
            // The message is dropped with the send, so no writer turn can run it.
            () = sleep_until(Some(deadline), &self.clock) => return Err(LedgerError::WriterHeld),
        }
        tokio::select! {
            biased;
            answered = &mut answer => return answered.map_err(|_| LedgerGone)?,
            () = sleep_until(Some(deadline), &self.clock) => {}
        }
        if claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Err(LedgerError::WriterHeld);
        }
        answer.await.map_err(|_| LedgerGone)?
    }

    /// Asks the assembler to drain what the bus already delivered, then the writer to close the channel and
    /// land what it holds. A record offered after the close counts as a drop, which is what the spec's "at
    /// most the channel's contents" allows for.
    pub async fn shutdown(self) -> Result<(), LedgerGone> {
        for task in self.tasks.lock().expect(TASKS_POISONED).drain(..) {
            task.abort();
        }
        let done = self.done.lock().await.take().ok_or(LedgerGone)?;
        let _ = self.stop.send(true);
        done.await.map_err(|_| LedgerGone)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    db: Arc<LedgerDb>,
    pricing: Arc<Pricing>,
    tables: Arc<PlanTables>,
    store: Arc<Store>,
    bus: &Bus,
    router: RouterHandle,
    clock: Clock,
    metrics: Arc<Metrics>,
    mut config: LedgerConfig,
) -> Ledger {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    let (control, control_rx) = mpsc::channel::<ReplayParked>(1);
    let stats = Arc::new(LedgerStats::default());
    let unpriced = Unpriced::default();
    let (done_tx, done_rx) = oneshot::channel();
    let (stop, stopped) = watch::channel(false);
    let subscriber = bus.subscribe("ledger", Delivery::Durable);
    let ledger = Ledger {
        db: db.clone(),
        stats: stats.clone(),
        assembler: subscriber.stats.clone(),
        unpriced: unpriced.clone(),
        metrics: metrics.clone(),
        tx,
        stop: Arc::new(stop),
        done: Arc::new(Mutex::new(Some(done_rx))),
        tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        control,
        clock: clock.clone(),
        retention_days: config.retention_days,
    };
    let purge_store = store.clone();
    // The writer answers once the batch that carries the start row has left its hands. With no start row the sender
    // drops here, and the start's fold runs at once.
    let (start_landed, start_committed) = oneshot::channel::<()>();
    // Into the empty channel before the mirror exists, so the mirror's first tick lands behind it and the gap the
    // writer dates from its newest row ends at the previous run's last one.
    let start_landed = config.start_version.as_ref().map(|version| {
        ledger.record(Record::RouterStarted {
            version: version.clone(),
            counts: router.load_counts(),
            at: clock(),
        });
        start_landed
    });
    let mirror = spawn_event_mirror(
        ledger.clone(),
        store,
        router.clone(),
        clock.clone(),
        config.event_poll,
        config.mirror_baseline.take(),
    );
    ledger.track(mirror);
    let assembling = ledger.clone();
    let assembler_stop = stopped.clone();
    let expire_interval = config.expire_interval;
    let assembler = spawn_subscriber(subscriber, move |subscriber| {
        assemble(subscriber, assembling, expire_interval, assembler_stop)
    });
    let handles = Handles {
        clock,
        db: db.clone(),
        metrics,
        pricing,
        router,
        stats,
        tables,
        unpriced,
    };
    // The purge has its own task and connection so a pass never takes the writer's lock or turn: the cut runs
    // in slices, so a write waits behind one slice at most and never on a whole day's deletion.
    let purging = handles.clone();
    let purge_config = config.clone();
    let purge = tokio::spawn(async move {
        let mut tick = tokio::time::interval(purge_config.purge_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        // The conditions read the trailing day of events, so a start folds them at once rather than serving none
        // until the first tick an hour on. The start row closes the slow-down spans the previous run left open, so
        // the fold waits for its commit, at most one purge interval, since a writer held on a full disk must not keep
        // the purge that frees room waiting.
        let _ = tokio::time::timeout(purge_config.purge_interval, start_committed).await;
        loop {
            fold_conditions(
                &purging.db,
                &purging.pricing,
                &purging.tables,
                (purging.clock)(),
            )
            .await;
            purging
                .stats
                .condition_folds
                .fetch_add(1, Ordering::Relaxed);
            tick.tick().await;
            run_purge(
                &purging.db,
                &purge_store,
                &purge_config,
                &purging.router,
                (purging.clock)(),
            )
            .await;
            purging.stats.purge_passes.fetch_add(1, Ordering::Relaxed);
        }
    });
    ledger.track(purge);
    tokio::spawn(writer(
        handles,
        rx,
        control_rx,
        stopped,
        assembler,
        config.event_poll,
        done_tx,
        start_landed,
    ));
    ledger
}

/// Waits for `shutdown` to flip the flag. The borrow a `watch` hands back is not `Send`, so it must not
/// survive into a `select!` arm that awaits.
async fn stop_requested(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

/// Resolves once the assembler in `handle` has ended, and never when there is none to wait for.
async fn ended(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    match handle.as_mut() {
        Some(assembler) => {
            let _ = assembler.await;
            *handle = None;
        }
        None => std::future::pending().await,
    }
}

/// The bus side: every event, chunks included, into the assembler in publish order, completed records into
/// the channel. On stop it drains what the bus already queued before it exits, so a completion published
/// just before shutdown reaches the writer. One receiver, so one `None` ends it.
async fn assemble(
    mut subscriber: Subscriber,
    ledger: Ledger,
    expire_interval: Duration,
    mut stopped: watch::Receiver<bool>,
) {
    let mut assembler = Assembler::new(ledger.clock.clone());
    let mut expire = tokio::time::interval(expire_interval);
    expire.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = subscriber.events.recv() => match event {
                Some(event) => land_all(&ledger, assembler.observe(event)).await,
                None => break,
            },
            _ = expire.tick() => {
                let now = (ledger.clock)();
                for id in assembler.expire(now) {
                    // The ledger names it so the system page's gap list has a row for it rather than a count
                    // with nothing behind it.
                    count_drop(&ledger.stats, &ledger.metrics, "requests");
                    ledger.record(crate::assemble::gap(&id, "expired", now));
                }
            }
            _ = stop_requested(&mut stopped) => {
                while let Ok(event) = subscriber.events.try_recv() {
                    land_all(&ledger, assembler.observe(event)).await;
                }
                break;
            }
        }
    }
}

/// A request record waits for the writer's channel, since the row it names can never be written again once
/// the request that produced it is gone. An event or a sample keeps the drop-and-count `record` always used.
async fn land_all(ledger: &Ledger, records: Vec<Record>) {
    for record in records {
        match record {
            Record::Request { .. } => ledger.record_request(record).await,
            other => ledger.record(other),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn writer(
    handles: Handles,
    mut rx: mpsc::Receiver<Record>,
    mut control: mpsc::Receiver<ReplayParked>,
    mut stopped: watch::Receiver<bool>,
    assembler: tokio::task::JoinHandle<()>,
    event_poll: Duration,
    done: oneshot::Sender<()>,
    mut start_landed: Option<oneshot::Sender<()>>,
) {
    let Handles {
        clock,
        db,
        router,
        stats,
        ..
    } = &handles;
    let mut assembler = Some(assembler);
    // The assembler's handle once the stop was seen: the writer keeps reading while it waits for the
    // assembler to end, since the assembler may be waiting on this very channel for room.
    let mut ending: Option<tokio::task::JoinHandle<()>> = None;
    // A registry change wakes the writer through the mirror's own `registry_changed` record; when that record
    // lands ahead of `router.reload()` publishing the new revision, the top-of-loop check sees the old one and
    // nothing else wakes the loop again until the next record or a boundary tick days out.
    // This arm re-checks the revision at the mirror's own cadence regardless, at the cost of one extra
    // `router.snapshot()` per poll.
    let mut revision_poll = tokio::time::interval(event_poll);
    revision_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    revision_poll.tick().await;
    // Drops the writer has not yet reported: the channel's, read from the counter each round, and its own.
    let mut reported: u64 = 0;
    let mut spends = Spends {
        handles: handles.clone(),
        event_poll,
        revision: 0,
        due: None,
        budgets: Budgets::default(),
        pending: Vec::new(),
    };
    // Ahead of the start's spend read, so a parked priced record that lands now is in the first report.
    let _ = handles.land_parked(clock()).await;
    // The start's budget rows wait for the first batch and follow its records: the start row `spawn` recorded is the
    // channel's first record, and it dates the gap from the newest row before it, which must be the previous run's.
    spends.start().await;
    // A batch whose transaction failed: while it is held the loop takes no new record, so order holds and the
    // channel fills behind it, which is what moves admission's mark before more money is spent.
    let mut held: Option<Held> = None;
    loop {
        if let Some(batch) = held.take() {
            let retry = if batch.retried {
                hold(&handles, &mut stopped, batch).await
            } else {
                retry_held(&handles, &mut stopped, batch).await
            };
            match retry {
                Retry::Held(rest) => held = Some(rest),
                // Back to the turn below once the drain's one try is done, so the stop arm and `ended` still close
                // and drain the channel.
                Retry::Stopped(rest) => {
                    handles.drain_held(rest).await;
                    stats.retrying.store(0, Ordering::Relaxed);
                }
                Retry::Settled => {
                    stats.retrying.store(0, Ordering::Relaxed);
                    if let Some(landed) = start_landed.take() {
                        let _ = landed.send(());
                    }
                    spends.refresh(&[]).await;
                }
            }
            continue;
        }
        // Checked on every round, whatever woke it: a member's budget or period edited under the writer,
        // with no flush of its own, still recomputes without waiting on the next request or period boundary.
        if router.snapshot().revision != spends.revision {
            spends.refresh(&[]).await;
        }
        let first = tokio::select! {
            // `recv` on a closed channel still yields what was queued before the close, then `None`.
            record = rx.recv() => match record { Some(record) => record, None => break },
            // On the writer's own turn, between two batches, so a replay never runs beside the start-up pass, another
            // replay or a batch: two passes that read the table at once would each land the same row, and the landed
            // counts and the metrics would take it twice. The start-up pass ran on this task before the loop, under
            // the same rule.
            Some(ReplayParked { now, deadline, claimed, reply }) = control.recv() => {
                if claimed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    tracing::debug!(
                        deadline = %phoebis_store::utc_stamp(deadline),
                        "a replay whose caller gave up at its deadline is dropped unrun"
                    );
                    continue;
                }
                if clock() >= deadline {
                    let _ = reply.send(Err(LedgerError::WriterHeld));
                    continue;
                }
                let replayed = match handles.land_parked(now).await {
                    Ok(landed) => {
                        // The spend read the start runs after its own pass: the landed money reaches the routing
                        // table before the caller hears the counts, so a selection after a replay that crossed a
                        // budget is refused. The budget rows it crosses wait for the next batch or flush, as the
                        // start's own do, so a replay on the writer's first turn lands none ahead of the start row.
                        spends.read(&[]).await;
                        let remaining = db
                            .writer_read(WriterRead::ParkedCount, crate::query::parked_count)
                            .await;
                        remaining.map(|remaining| Replayed { landed, remaining })
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(replayed);
                continue;
            }
            // The flag stays set once flipped, so the arm is guarded to the one round that moves the handle.
            _ = stop_requested(&mut stopped), if assembler.is_some() => {
                ending = assembler.take();
                continue;
            }
            // The assembler drained the bus and sent its last record, which is in the channel by now: close
            // it, and the arm above lands what is left.
            _ = ended(&mut ending) => {
                rx.close();
                continue;
            }
            // Beside the top-of-loop check, the start's budget rows land here when no record came to carry them.
            _ = revision_poll.tick() => {
                spends.flush().await;
                continue;
            }
            // Re-armed after every run: the boundary a budgeted member's period ends on, so a pool whose
            // traffic never touches this writer still turns over its spend at the period's edge.
            _ = sleep_until(spends.due, clock) => {
                spends.refresh(&[]).await;
                continue;
            }
        };
        let mut records = vec![first];
        while records.len() < BATCH {
            match rx.try_recv() {
                Ok(record) => records.push(record),
                Err(_) => break,
            }
        }
        let priced = priced_attempts(&records);
        records.extend(spends.take_pending());
        stamp_tiers(&router.snapshot(), &mut records);
        let batch = Arc::new(records);
        match handles.land(&batch, 0..batch.len()).await {
            // A degraded ledger reads the spend after any batch that lands, since that read is what clears it.
            Landed::Written => {
                if !priced.is_empty() || stats.degraded.load(Ordering::Relaxed) {
                    spends.refresh(&priced).await;
                }
            }
            Landed::Refused { .. } => {
                stats.retrying.store(1, Ordering::Relaxed);
                if !stats.degraded.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "a ledger batch could not be written, so budgeted members wait until it lands"
                    );
                    router.report(Report::Accounting { degraded: true }).await;
                }
                held = Some(Held {
                    records: batch,
                    from: 0,
                    retried: false,
                });
                continue;
            }
            Landed::Lost => {}
        }
        // The start row is the channel's first record, so the first batch out of the writer's hands carries it.
        if let Some(landed) = start_landed.take() {
            let _ = landed.send(());
        }
        // The channel is empty: whatever was dropped since the last report is one gap.
        if rx.is_empty() {
            let dropped = stats.dropped.load(Ordering::Relaxed);
            if dropped > reported {
                let gap = Arc::new(vec![Record::Event(EventRecord {
                    at: clock(),
                    kind: LEDGER_GAP.into(),
                    detail: Some(format!(r#"{{"dropped":{}}}"#, dropped - reported)),
                    ..Default::default()
                })]);
                if let Landed::Written = handles.land(&gap, 0..1).await {
                    reported = dropped;
                    stats.gaps.fetch_add(1, Ordering::Relaxed);
                    stats.last_gap_at.store(clock(), Ordering::Relaxed);
                }
            }
        }
    }
    spends.flush().await;
    let _ = done.send(());
}

/// A batch the writer holds while it lands: shared with the blocking worker by reference count, so a failed batch is
/// retried and its records replayed or parked without a copy.
type Batch = Arc<Vec<Record>>;

/// Stamps each attempt of the requests in `records` with its account's priced tier as `snapshot` holds it, before the
/// blocking write so the write holds no registry type: `None` for an api_key account, for an oauth account whose
/// profile has not been read, and for a plan the router never prices (a free organization, a usage-based seat), so
/// their rows price no share. Stamped once while the writer owns the records, so a batch's retries, its replays and
/// the drain land the tiers its first try read.
fn stamp_tiers(snapshot: &RegistrySnapshot, records: &mut [Record]) {
    for record in records {
        if let Record::Request { attempts, .. } = record {
            for a in attempts {
                a.tier = snapshot.priced_tier(&a.account_id).map(String::from);
            }
        }
    }
}

/// A batch some of whose records have not landed, from `from` on.
struct Held {
    records: Batch,
    from: usize,
    /// The in-place retries are behind it, so its next try waits out `BATCH_RETRY_HOLD`.
    retried: bool,
}

/// What one `land` did with its records.
enum Landed {
    /// The transaction committed.
    Written,
    /// The transaction failed and rolled back: the records are the caller's to retry, replay or park.
    Refused { database_wide: bool, error: String },
    /// The closure panicked: a bug, so the records are dropped and counted rather than tried into it again.
    Lost,
}

/// Where a retry left a held batch.
enum Retry {
    /// Every record landed, was parked, or was dropped as a poison row.
    Settled,
    /// The database as a whole refused a record, or refused to park it: it and the records after it wait for the
    /// next try.
    Held(Held),
    /// The stop came during a wait.
    Stopped(Held),
}

/// What the writer, its spend refresh and its landing passes read and report through, built once in `spawn`.
#[derive(Clone)]
struct Handles {
    clock: Clock,
    db: Arc<LedgerDb>,
    metrics: Arc<Metrics>,
    pricing: Arc<Pricing>,
    router: RouterHandle,
    stats: Arc<LedgerStats>,
    tables: Arc<PlanTables>,
    unpriced: Unpriced,
}

#[derive(PartialEq)]
enum Wait {
    Elapsed,
    Stopped,
}

/// Waits `delay` on the ledger's clock, read every `CLOCK_TICK`, or until the stop.
async fn wait_on_clock(
    delay: Duration,
    clock: &Clock,
    stopped: &mut watch::Receiver<bool>,
) -> Wait {
    let deadline = clock() + delay.as_millis() as i64;
    // The deadline polled first, so one already passed answers `Elapsed` ahead of a stop already set.
    tokio::select! {
        biased;
        () = sleep_until(Some(deadline), clock) => Wait::Elapsed,
        () = stop_requested(stopped) => Wait::Stopped,
    }
}

/// A failed batch is retried in place, then replayed one record at a time. Nothing new is taken from the channel
/// while this runs, so order holds and admission's mark fires behind the held batch.
async fn retry_held(handles: &Handles, stopped: &mut watch::Receiver<bool>, held: Held) -> Retry {
    for delay in BATCH_RETRY_DELAYS {
        if wait_on_clock(delay, &handles.clock, stopped).await == Wait::Stopped {
            return Retry::Stopped(held);
        }
        let rest = held.from..held.records.len();
        match handles.land(&held.records, rest).await {
            Landed::Written | Landed::Lost => return Retry::Settled,
            Landed::Refused { .. } => {}
        }
    }
    handles.replay(held).await
}

/// A batch the database as a whole refused is tried again every `BATCH_RETRY_HOLD`, one record at a time, until it
/// lands or the process stops; it is never dropped.
async fn hold(handles: &Handles, stopped: &mut watch::Receiver<bool>, held: Held) -> Retry {
    if wait_on_clock(BATCH_RETRY_HOLD, &handles.clock, stopped).await == Wait::Stopped {
        return Retry::Stopped(held);
    }
    handles.replay(held).await
}

impl Handles {
    /// One record per transaction. A record the database as a whole refuses is held with everything after it. One
    /// refused on its own row is a poison row, parked when it carries money and dropped with its gap otherwise.
    async fn replay(&self, held: Held) -> Retry {
        for index in held.from..held.records.len() {
            let one = index..index + 1;
            let error = match self.land(&held.records, one).await {
                Landed::Written | Landed::Lost => continue,
                // A full disk, an I/O error or a lock held past the timeout refuses every record after this one as
                // well, so the rest of the batch waits with it. A constraint or a trigger refused this row alone.
                Landed::Refused {
                    database_wide: true,
                    ..
                } => {
                    return Retry::Held(Held {
                        records: held.records,
                        from: index,
                        retried: true,
                    })
                }
                Landed::Refused { error, .. } => error,
            };
            match &held.records[index] {
                Record::Request {
                    request,
                    attempts,
                    bodies,
                } if attempts.iter().any(AttemptRecord::has_usage) => {
                    // The parking table has no constraint a serialized record can violate, so a park the database
                    // refuses is the database's: the record stays held rather than lost.
                    if self
                        .park(request, attempts, bodies.is_some(), &error)
                        .await
                        .is_err()
                    {
                        return Retry::Held(Held {
                            records: held.records.clone(),
                            from: index,
                            retried: true,
                        });
                    }
                }
                record @ Record::Request { request, .. } => {
                    self.count_drop(table_of(record));
                    // The writer lands its own gap as a batch of one, as the gap report does: a send into its own
                    // channel would meet the queue it stopped draining.
                    let gap = Arc::new(vec![crate::assemble::gap(
                        &request.id,
                        "poison",
                        (self.clock)(),
                    )]);
                    let _ = self.land(&gap, 0..1).await;
                }
                other => self.count_drop(table_of(other)),
            }
        }
        Retry::Settled
    }

    /// Parks a priced record the database refused on its own row: one `parked_records` row with the request row and
    /// its attempts and without the captured bodies, so the capture is lost and the money is not.
    async fn park(
        &self,
        request: &RequestRecord,
        attempts: &[AttemptRecord],
        captured: bool,
        error: &str,
    ) -> Result<(), LedgerError> {
        let record = serde_json::to_string(&ParkedRows { request, attempts })?;
        let at = (self.clock)();
        let request_id = request.id.clone();
        let pool_id = request.pool_id.clone();
        let account_id = attempts.last().map(|a| a.account_id.clone());
        let refusal = error.to_string();
        let parked = blocking(&self.db, move |db| {
            db.insert_parked(
                at,
                &request_id,
                pool_id.as_deref(),
                account_id.as_deref(),
                &record,
                &refusal,
            )
        })
        .await;
        match &parked {
            Ok(()) => {
                self.stats.parked_priced.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    request_id = %request.id,
                    pool = request.pool_id.as_deref().unwrap_or(crate::rollup::UNATTRIBUTED),
                    %error,
                    capture_dropped = captured,
                    "a priced record could not be written. It is parked and tried again at the next start or on a replay"
                );
            }
            Err(park_error) => tracing::error!(
                request_id = %request.id,
                error = %park_error,
                "a priced record could not be parked, so it stays held"
            ),
        }
        parked
    }

    /// The stop's last try at a held batch: one replay, and what the database still refuses is lost and counted, the
    /// one thing a stop may lose.
    async fn drain_held(&self, held: Held) {
        let rest = match self.replay(held).await {
            Retry::Held(rest) | Retry::Stopped(rest) => rest,
            Retry::Settled => return,
        };
        let lost = &rest.records[rest.from..];
        tracing::error!(
            records = lost.len(),
            "the ledger stopped with records it could not write, and they are lost"
        );
        for record in lost {
            self.count_drop(table_of(record));
        }
    }

    /// Every parked record is tried once at start and on each replay, in the order it was parked, each in its own
    /// transaction, which deletes the parked row as it lands the record, so a replay is charged once however the
    /// process fares between the two: one that lands leaves the table and is observed as a landed record, and one that
    /// fails keeps its row with the error it met this time. Answers how many landed, or the error of a row read that
    /// failed, since a pass that never ran is not one that kept every row.
    async fn land_parked(&self, now: i64) -> Result<u64, LedgerError> {
        let snapshot = self.router.snapshot();
        let db = &self.db;
        let rows = db
        .writer_read(WriterRead::ParkedRows, crate::db::parked_rows)
        .await
        .inspect_err(|error| {
            tracing::warn!(%error, "the parked records could not be read, so they wait for the next start or replay");
        })?;
        if rows.is_empty() {
            return Ok(0);
        }
        let (mut landed, mut kept) = (0u64, 0u64);
        for row in rows {
            let outcome = match serde_json::from_str::<ParkedRecord>(&row.record) {
                Ok(parked) => {
                    // The capture was not parked, so the row claims none: the drawer would read a captured request
                    // whose bodies are gone as one whose bodies aged out.
                    let mut records = vec![Record::Request {
                        request: RequestRecord {
                            bodies_captured: false,
                            ..parked.request
                        },
                        attempts: parked.attempts,
                        bodies: None,
                    }];
                    stamp_tiers(&snapshot, &mut records);
                    let batch = Arc::new(records);
                    self.land_acknowledging(&batch, 0..1, Some(row.id)).await
                }
                Err(error) => Landed::Refused {
                    database_wide: false,
                    error: error.to_string(),
                },
            };
            let id = row.id;
            let error = match outcome {
                Landed::Written => {
                    landed += 1;
                    continue;
                }
                Landed::Refused { error, .. } => error,
                Landed::Lost => "the write panicked".to_string(),
            };
            kept += 1;
            if let Err(error) = blocking(db, move |db| db.update_parked(id, &error, now)).await {
                tracing::warn!(%error, id, "a parked record's row could not be updated");
            }
        }
        tracing::info!(landed, kept, "the parked records were tried");
        Ok(landed)
    }
}

/// Runs `f` against the ledger on a blocking worker; a worker that died answers as an I/O error.
pub(crate) async fn blocking<T: Send + 'static>(
    db: &Arc<LedgerDb>,
    f: impl FnOnce(&LedgerDb) -> Result<T, LedgerError> + Send + 'static,
) -> Result<T, LedgerError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .map_err(|join| LedgerError::Io(std::io::Error::other(join.to_string())))?
}

/// Every budgeted period of every pool member, from the snapshot. A subscription member's sum reads its metered
/// rows alone (overage and fast, and `usage_based` for a usage-based seat), since its plan-funded traffic is no spend.
fn budgeted_members(snapshot: &RegistrySnapshot) -> Vec<BudgetedMember> {
    snapshot
        .budgeted_members()
        .flat_map(|m| {
            let metered_only =
                snapshot.account(&m.account_id).map(|a| a.kind) == Some(AccountKind::Oauth);
            m.budgets().map(move |(period, _)| BudgetedMember {
                pool_id: m.pool_id.clone(),
                account_id: m.account_id.clone(),
                period,
                metered_only,
            })
        })
        .collect()
}

/// A priced attempt the batch that triggered a spend refresh landed: what a budget crossing names as its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PricedAttempt {
    pub pool_id: String,
    pub account_id: String,
    pub request_id: String,
    pub received_at: i64,
}

/// One member's spend over one period as a refresh read it, with the newest priced attempt of the refresh's batch
/// that the member served inside the row's period.
#[derive(Debug)]
struct SpentRow {
    spend: MemberSpend,
    source_request_id: Option<String>,
}

/// What a spend refresh did: the deadline to arm next, whether the read succeeded and its reports reached the
/// router, which is what lets a degraded ledger clear, and the rows it read, `None` when the read failed.
#[derive(Debug)]
struct SpendRefresh {
    due: Option<i64>,
    refreshed: bool,
    rows: Option<Vec<SpentRow>>,
}

/// A member and the period kind its budget runs over.
type SpendKey = (String, String, BudgetPeriod);

/// The budget edges the writer announces, measured from one refresh to the next.
#[derive(Default)]
struct Budgets {
    /// The crossings the ledger held at the start, by the end of the period each was in, which the first refresh
    /// takes as this run's own.
    announced: HashMap<SpendKey, i64>,
    /// The rows the last refresh read.
    last: HashMap<SpendKey, MemberSpend>,
    /// The members this run wrote `budget_reached` for in the period each is in: a period's end is announced only
    /// after its crossing was.
    reached: HashSet<SpendKey>,
}

impl Budgets {
    /// The budget rows `rows` cross against the rows the last refresh read, which they then replace: a spend at or
    /// above its member's budget where the held row of the same period was under it, or absent, or where this period
    /// wrote no crossing yet (an edit lowered the budget under the spend), is reached. A row of a later period than
    /// the held one ends the held period.
    fn edges(
        &mut self,
        snapshot: &RegistrySnapshot,
        rows: Vec<SpentRow>,
        at: i64,
    ) -> Vec<EventRecord> {
        // A crossing the previous run wrote for the period a row is in is held as if this run had read it, so a
        // restart writes no second one and the period's end is still announced.
        for row in &rows {
            let key = (
                row.spend.pool_id.clone(),
                row.spend.account_id.clone(),
                row.spend.period,
            );
            if self.announced.get(&key) == Some(&row.spend.period_end)
                && !self.last.contains_key(&key)
            {
                self.reached.insert(key.clone());
                self.last.insert(key, row.spend.clone());
            }
        }
        self.announced.clear();
        let mut out = Vec::new();
        let mut last = HashMap::with_capacity(rows.len());
        for SpentRow {
            spend,
            source_request_id,
        } in rows
        {
            let key = (
                spend.pool_id.clone(),
                spend.account_id.clone(),
                spend.period,
            );
            let held = self.last.remove(&key);
            let same_period = held
                .as_ref()
                .filter(|held| held.period_start == spend.period_start);
            if let Some(held) = held
                .as_ref()
                .filter(|h| h.period_start < spend.period_start)
            {
                if self.reached.remove(&key) {
                    out.push(budget_period_ended(held, &spend, at));
                }
            }
            let budget = snapshot
                .members(&spend.pool_id)
                .iter()
                .find(|member| member.account_id == spend.account_id)
                .and_then(|member| member.budget(spend.period));
            if let Some(budget) = budget {
                let was_under = same_period.is_none_or(|held| held.spent_usd < budget)
                    || !self.reached.contains(&key);
                if spend.spent_usd >= budget && was_under {
                    self.reached.insert(key.clone());
                    out.push(budget_reached(&spend, budget, source_request_id, at));
                }
            }
            last.insert(key, spend);
        }
        self.reached.retain(|key| last.contains_key(key));
        self.last = last;
        out
    }
}

/// The newest `budget_reached` row since `from` per pool, account and period, as the end of the period it was written
/// in.
fn announced_crossings(
    conn: &rusqlite::Connection,
    from: i64,
) -> Result<HashMap<SpendKey, i64>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT pool_id, account_id, detail FROM events WHERE kind = ?2 AND at >= ?1 \
         ORDER BY at, rowid",
    )?;
    let rows = stmt.query_map(rusqlite::params![from, BUDGET_REACHED], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (Some(pool_id), Some(account_id), Some(detail)) = row? else {
            continue;
        };
        let Ok(detail) = serde_json::from_str::<serde_json::Value>(&detail) else {
            continue;
        };
        let period = detail["period"].as_str().and_then(BudgetPeriod::parse);
        if let (Some(period), Some(period_end)) = (period, detail["period_end"].as_i64()) {
            out.insert((pool_id, account_id, period), period_end);
        }
    }
    Ok(out)
}

fn budget_reached(
    spend: &MemberSpend,
    budget: f64,
    source_request_id: Option<String>,
    at: i64,
) -> EventRecord {
    EventRecord {
        at,
        kind: BUDGET_REACHED.into(),
        account_id: Some(spend.account_id.clone()),
        detail: Some(
            json!({
                "period": spend.period.as_str(),
                "budget_usd": budget,
                "spent_usd": spend.spent_usd,
                "period_end": spend.period_end,
                "source_request_id": source_request_id,
            })
            .to_string(),
        ),
        pool_id: Some(spend.pool_id.clone()),
        request_id: source_request_id,
        ..Default::default()
    }
}

fn budget_period_ended(previous: &MemberSpend, spend: &MemberSpend, at: i64) -> EventRecord {
    EventRecord {
        at,
        kind: BUDGET_PERIOD_ENDED.into(),
        account_id: Some(spend.account_id.clone()),
        detail: Some(
            json!({
                "period": spend.period.as_str(),
                "previous_spent_usd": previous.spent_usd,
                "period_start": spend.period_start,
            })
            .to_string(),
        ),
        pool_id: Some(spend.pool_id.clone()),
        ..Default::default()
    }
}

/// The writer's spend refresh: the handles it reads and reports through, the budget edges measured across refreshes,
/// and the budget rows not yet landed.
struct Spends {
    handles: Handles,
    event_poll: Duration,
    /// The registry revision the last read priced its members under, which the writer compares the router's against
    /// to recompute after a budget or period edited under it.
    revision: u64,
    /// When the boundary tick fires next, as the last read armed it.
    due: Option<i64>,
    budgets: Budgets,
    pending: Vec<EventRecord>,
}

impl Spends {
    /// The writer's first read, after taking the crossings the ledger holds since the current month began (every
    /// period starts in it). A crossing without a row, such as one a budget set while the router was down made, is
    /// announced.
    async fn start(&mut self) {
        let (from, _) = period_bounds(BudgetPeriod::Month, (self.handles.clock)());
        match self
            .handles
            .db
            .writer_read(WriterRead::AnnouncedCrossings, move |conn| {
                announced_crossings(conn, from)
            })
            .await
        {
            Ok(announced) => self.budgets.announced = announced,
            Err(error) => tracing::warn!(
                %error,
                "the ledger's budget crossings could not be read, so a member over its budget is announced again"
            ),
        }
        self.read(&[]).await
    }

    /// Runs `refresh_member_spends` and settles the ledger's standing on it. The writer calls this with nothing held,
    /// so a read that succeeded and reached the router clears `degraded`. One that did not, while degraded, runs again
    /// one `event_poll` later rather than at the period's end, since every budgeted member waits on it. The budget
    /// rows the read crosses wait in `pending`.
    ///
    /// A failed read keeps the armed deadline while it is still ahead of `now` (a transient failure leaves an armed
    /// tick where it was rather than disarming it), otherwise arms a retry at `now + event_poll`: a tick that had
    /// already reached its deadline would otherwise re-fire at once on every following timer tick, repeating the
    /// failing read and its warning for as long as the error lasts.
    async fn read(&mut self, priced: &[PricedAttempt]) {
        let now = (self.handles.clock)();
        let degraded = self.handles.stats.degraded.load(Ordering::Relaxed);
        let snapshot = self.handles.router.snapshot();
        self.revision = snapshot.revision;
        let spend = refresh_member_spends(&self.handles, &snapshot, now, degraded, priced).await;
        let retry = now + self.event_poll.as_millis() as i64;
        let due = if spend.rows.is_none() {
            self.due.filter(|&at| at > now).or(Some(retry))
        } else {
            spend.due
        };
        if let Some(rows) = spend.rows {
            let edges = self.budgets.edges(&snapshot, rows, now);
            self.pending.extend(edges);
        }
        if spend.refreshed && self.handles.stats.degraded.swap(false, Ordering::Relaxed) {
            tracing::info!("the ledger caught up, so budgeted members are placed on again");
        }
        self.due = if spend.refreshed || !degraded {
            due
        } else {
            Some(due.map_or(retry, |due| due.min(retry)))
        };
    }

    /// `read`, then the pending budget rows landed.
    async fn refresh(&mut self, priced: &[PricedAttempt]) {
        self.read(priced).await;
        self.flush().await;
    }

    /// Lands the pending budget rows as a batch of their own, as the gap report lands its row.
    async fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let batch: Batch = Arc::new(self.pending.iter().cloned().map(Record::Event).collect());
        let landed = self.handles.land(&batch, 0..batch.len()).await;
        // A refused batch keeps its rows for the next batch or refresh, as a held batch keeps its records.
        if !matches!(landed, Landed::Refused { .. }) {
            self.pending.clear();
        }
    }

    /// The pending budget rows, for a batch to carry behind its records.
    fn take_pending(&mut self) -> impl Iterator<Item = Record> {
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(Record::Event)
    }
}

/// Re-reads every budgeted member's spend and reports it to the router, then the ledger's standing after it, which
/// is current: the writer runs this with nothing held, so a `degraded` ledger clears on a read that succeeds.
/// Answers the earliest `period_end` among the rows read, to arm the next boundary tick; `None` while no member
/// carries a budget, since a pool whose every member is excluded still refreshes on the revision and boundary arms
/// alone, without traffic, and there is truly nothing to arm a tick for, and nothing to read is a read that
/// succeeded.
async fn refresh_member_spends(
    handles: &Handles,
    snapshot: &RegistrySnapshot,
    now: i64,
    degraded: bool,
    priced: &[PricedAttempt],
) -> SpendRefresh {
    let Handles {
        db,
        pricing,
        router,
        ..
    } = handles;
    let members = budgeted_members(snapshot);
    if members.is_empty() {
        return SpendRefresh {
            due: None,
            refreshed: !degraded || reached(router, Report::Accounting { degraded: false }).await,
            rows: Some(Vec::new()),
        };
    }
    let revisions = pricing.revisions();
    let rows = db
        .writer_read(WriterRead::MemberSpends, move |conn| {
            member_spends(conn, &members, now, &revisions)
        })
        .await;
    match rows {
        Ok(rows) => {
            let earliest = rows.iter().map(|r| r.period_end).min();
            // The standing follows the rows on the same queue, so the selector resumes on spend it has read.
            let refreshed = reached(router, Report::MemberSpend(rows.clone())).await
                && reached(router, Report::Accounting { degraded: false }).await;
            let rows = rows
                .into_iter()
                .map(|spend| {
                    let source_request_id = priced
                        .iter()
                        .filter(|attempt| {
                            attempt.pool_id == spend.pool_id
                                && attempt.account_id == spend.account_id
                                && (spend.period_start..spend.period_end)
                                    .contains(&attempt.received_at)
                        })
                        .max_by_key(|attempt| attempt.received_at)
                        .map(|attempt| attempt.request_id.clone());
                    SpentRow {
                        spend,
                        source_request_id,
                    }
                })
                .collect();
            SpendRefresh {
                due: earliest,
                refreshed,
                rows: Some(rows),
            }
        }
        Err(error) => {
            tracing::warn!(%error, "a member's spend could not be read, so the last report stands");
            SpendRefresh {
                due: None,
                refreshed: false,
                rows: None,
            }
        }
    }
}

/// Sends a replacing reading and answers whether the router can still take it: the reading always lands in the
/// router's pending map, so a stopped router is the one way it is lost.
async fn reached(router: &RouterHandle, report: Report) -> bool {
    router.report(report).await;
    router.is_alive()
}

/// Resolves once the ledger's clock reaches `deadline`, read every `CLOCK_TICK` so a stepped clock is seen as a
/// moving one is; `None` waits forever, so the select arm this backs never fires while no member carries a budget.
async fn sleep_until(deadline: Option<i64>, clock: &Clock) {
    let Some(at) = deadline else {
        return std::future::pending().await;
    };
    loop {
        let now = clock();
        if now >= at {
            return;
        }
        tokio::time::sleep(CLOCK_TICK.min(Duration::from_millis((at - now) as u64))).await;
    }
}

/// One `PricedAttempt` per attempt with usage of each request in the batch: what the fold sums, the signal the
/// member spend refresh reacts to. A request without a pool names no member, so its pool reads empty.
fn priced_attempts(batch: &[Record]) -> Vec<PricedAttempt> {
    batch
        .iter()
        .flat_map(|record| match record {
            Record::Request {
                request, attempts, ..
            } => attempts
                .iter()
                .filter(|attempt| attempt.has_usage())
                .map(|attempt| PricedAttempt {
                    pool_id: request.pool_id.clone().unwrap_or_default(),
                    account_id: attempt.account_id.clone(),
                    request_id: request.id.clone(),
                    received_at: request.received_at,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn count_drop(stats: &LedgerStats, metrics: &Metrics, table: &'static str) {
    stats.dropped.fetch_add(1, Ordering::Relaxed);
    metrics.observe_drop(table);
}

fn table_of(record: &Record) -> &'static str {
    match record {
        Record::CacheMap(_) => "cache_map",
        Record::Decision(_) => "decisions",
        Record::Event(_) => "events",
        Record::Keepalive(_) => "rollups",
        Record::Request { .. } => "requests",
        Record::RouterStarted { .. } => "events",
        Record::Samples { .. } => "samples",
    }
}

impl Handles {
    fn count_drop(&self, table: &'static str) {
        count_drop(&self.stats, &self.metrics, table);
    }

    /// One transaction over `range` of the batch on a blocking thread. A transaction that fails rolls back and leaves
    /// the records to the caller to retry, replay or park, and only a panicking closure drops them, counted, so the gap
    /// that follows names them. Metrics are observed only after the commit succeeds, so a failed write never reaches
    /// the request or sample series.
    async fn land(&self, batch: &Batch, range: Range<usize>) -> Landed {
        self.land_acknowledging(batch, range, None).await
    }

    /// `land`, deleting the parked row the batch replays in the record's own transaction. A row deleted after the
    /// commit could outlive its landing, through a failed delete or a retention pass that cut the raw rows, and the
    /// next start would charge the record again, while a row deleted ahead of a failed commit would lose the money.
    async fn land_acknowledging(
        &self,
        batch: &Batch,
        range: Range<usize>,
        parked: Option<i64>,
    ) -> Landed {
        let Handles {
            clock,
            db,
            metrics,
            pricing,
            stats,
            tables,
            unpriced,
            ..
        } = self;
        let db = db.clone();
        let clock = clock.clone();
        let revisions = pricing.revisions();
        let records = batch.clone();
        let size = range.len() as u64;
        let landed = range.clone();
        let observed = revisions.clone();
        let written = tokio::task::spawn_blocking(move || {
            // The cache-map writes that found no decision, counted once the transaction commits: an orphan in a batch
            // that rolls back is tried again with it.
            let mut orphans = Vec::new();
            // Made at the transaction's first decision and kept for the rest, rather than a context per decision.
            let mut compressor: Option<zstd::bulk::Compressor> = None;
            let result = db.write(|conn| {
                let tx = begin_write(conn)?;
                for record in &records[range] {
                    match record {
                        Record::Event(event) => {
                            insert_event_in(&tx, event)?;
                            crate::rollup::fold_event(&tx, event)?;
                        }
                        Record::Keepalive(delta) => {
                            crate::rollup::fold_keepalive(&tx, delta)?;
                        }
                        Record::Request {
                            request,
                            attempts,
                            bodies,
                        } => {
                            let previous = insert_request_in(
                                &tx,
                                request,
                                attempts,
                                bodies.as_ref(),
                                clock(),
                            )?;
                            crate::rollup::fold_request(
                                &tx,
                                request,
                                attempts,
                                previous.as_ref().map(|(r, a)| (r, a.as_slice())),
                                &revisions,
                            )?;
                        }
                        Record::Samples { quota, extra, .. } => {
                            insert_samples_in(&tx, quota, extra.as_ref())?;
                        }
                        Record::Decision(row) => {
                            let summary = serde_json::to_string(&row.summary)?;
                            // Compressed here, inside the batch on the blocking thread, so the request path and the
                            // broadcast carry the typed record and no frame. The default level folds this JSON four
                            // to six times. The bulk compressor writes the content size into the frame's header, which
                            // a reader checks against its bound before it decodes.
                            let compressor = match &mut compressor {
                                Some(compressor) => compressor,
                                None => compressor.insert(zstd::bulk::Compressor::new(0)?),
                            };
                            let frame = compressor.compress(&serde_json::to_vec(&row.record)?)?;
                            insert_decision_in(&tx, row, &summary, &frame)?;
                        }
                        Record::CacheMap(write) => {
                            let entries = serde_json::to_string(&write.entries)?;
                            if update_cache_map_in(&tx, write, &entries)? == 0 {
                                orphans.push((write.request_id.clone(), write.seq, write.attempt));
                            }
                        }
                        Record::RouterStarted {
                            version,
                            counts,
                            at,
                        } => {
                            let event = EventRecord {
                                at: *at,
                                kind: ROUTER_STARTED.into(),
                                detail: Some(
                                    json!({
                                        "version": version,
                                        "pins_loaded": counts.pins,
                                        "shedding_marks_loaded": counts.shed_marks,
                                        "shedding_rounds_stale": counts.shed_rounds_stale,
                                        "windows_loaded": counts.windows,
                                        "affinity_entries_loaded": counts.affinity_entries,
                                        "down_since": newest_row_at(&tx)?,
                                        // A held request lives in the process alone, so no start keeps one.
                                        "holds_lost": true,
                                    })
                                    .to_string(),
                                ),
                                ..Default::default()
                            };
                            insert_event_in(&tx, &event)?;
                            crate::rollup::fold_event(&tx, &event)?;
                        }
                    }
                }
                if let Some(id) = parked {
                    delete_parked_in(&tx, id)?;
                }
                tx.commit()?;
                Ok::<(), LedgerError>(())
            });
            (result, orphans)
        })
        .await;
        match written {
            Ok((Ok(()), orphans)) => {
                stats.written.fetch_add(size, Ordering::Relaxed);
                stats
                    .cache_map_orphans
                    .fetch_add(orphans.len() as u64, Ordering::Relaxed);
                for (request_id, seq, attempt) in orphans {
                    tracing::debug!(
                        %request_id,
                        seq,
                        attempt,
                        "a cache map found no decision to land on, so it is counted and dropped"
                    );
                }
                for record in &batch[landed] {
                    match record {
                        Record::Request {
                            request, attempts, ..
                        } => {
                            let in_force = observed.in_force(request.received_at);
                            // Counted at the landing, once per request, since every read prices the same counts again,
                            // and after the commit, since a refused batch runs its transaction again on the retry or
                            // the replay.
                            if attempts.iter().any(AttemptRecord::has_usage)
                                && in_force.price(&request.canonical_model).is_none()
                            {
                                pricing.unknown_counter().fetch_add(1, Ordering::Relaxed);
                            }
                            note_unpriced(tables, unpriced, request, attempts);
                            metrics.observe_request(in_force, request, attempts);
                        }
                        Record::Samples { quota, poll, .. } => {
                            metrics.observe_samples(quota, *poll)
                        }
                        _ => {}
                    }
                }
                Landed::Written
            }
            Ok((Err(error), _)) => {
                tracing::error!(%error, records = size, "a ledger batch could not be written");
                stats.write_failures.fetch_add(1, Ordering::Relaxed);
                Landed::Refused {
                    database_wide: error.is_database_wide(),
                    error: error.to_string(),
                }
            }
            Err(error) => {
                tracing::error!(%error, records = size, "the ledger writer panicked, so the batch is lost");
                stats.write_failures.fetch_add(1, Ordering::Relaxed);
                for record in &batch[landed] {
                    self.count_drop(table_of(record));
                }
                Landed::Lost
            }
        }
    }
}

/// Records a plan-funded landing with usage on a model the cost model in force at its arrival gives no weight on the
/// 7-day window, never a free classifier call (`is_free_classifier`): the entry enters, or counts the request once
/// however many of its attempts answered. A landing that finds the model weighted clears the entry and keeps its key,
/// so its gauge series reads 0.
/// The entry follows landing order, by design: a request received before the revision that weighted its model and
/// landed after the clear raises the entry again with its own earlier receipt as `since`.
fn note_unpriced(
    tables: &PlanTables,
    unpriced: &Unpriced,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
) {
    // A free classifier call prices at no share, so its model's weight is never read.
    if is_free_classifier(request.classifier.as_deref()) {
        return;
    }
    let plan_funded = attempts
        .iter()
        .any(|a| a.has_usage() && a.funding() == Funding::Plan);
    if !plan_funded {
        return;
    }
    let canonical = request.canonical_model.as_str();
    let model = phoebis_routing::listing::undated_id(canonical);
    let costs = tables.revisions.in_force(request.received_at);
    let mut served = unpriced.lock().expect(UNPRICED_POISONED);
    match costs.weight(canonical, SEVEN_DAY) {
        None => {
            let entry = served
                .entry(model.to_string())
                .or_insert_with(|| UnpricedModel {
                    model: model.to_string(),
                    since: None,
                    requests: 0,
                });
            // A cleared entry starts a new run.
            entry.since.get_or_insert(request.received_at);
            entry.requests += 1;
        }
        Some(_) => {
            if let Some(entry) = served.get_mut(model) {
                entry.since = None;
                entry.requests = 0;
            }
        }
    }
}

/// One fold of the routing conditions at `now` on the purge task's connection, off the runtime thread. The windows
/// completed since the last tick are reconciled first, on the same transaction, so `meter_drift` reads the ratios this
/// tick wrote and a failed fold writes neither. A fold that fails leaves the last one's rows, which the next tick
/// replaces.
async fn fold_conditions(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    tables: &Arc<PlanTables>,
    now: i64,
) {
    let prices = pricing.revisions();
    let tables = tables.clone();
    let folded = blocking(db, move |db| {
        db.purge(|conn| {
            let tx = crate::db::begin_write(conn)?;
            crate::reconcile::reconcile(&tx, &prices, &tables, now)?;
            crate::rollup::fold_routing_conditions(&tx, &tables, now)?;
            tx.commit()?;
            Ok(())
        })
    })
    .await;
    if let Err(error) = folded {
        tracing::warn!(%error, "the routing conditions could not be folded");
    }
}

async fn run_purge(
    db: &Arc<LedgerDb>,
    store: &Arc<Store>,
    config: &LedgerConfig,
    router: &RouterHandle,
    now: i64,
) {
    let budgeted = budgeted_members(&router.snapshot());
    let purged = run_retention(db.clone(), config.clone(), now, budgeted).await;
    // The registry's settled refresh attempts and its change log follow the requests' cutoff, so a zero
    // `retention_days` keeps them as it keeps the requests.
    let history = match request_cutoff(config.retention_days, now) {
        None => None,
        Some(cutoff) => {
            let store = store.clone();
            match tokio::task::spawn_blocking(move || store.purge_history(cutoff)).await {
                Ok(Ok(history)) => Some(history),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "registry history retention failed");
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "registry history retention did not finish");
                    None
                }
            }
        }
    };
    match blocking(db, |db| db.refresh_statistics()).await {
        Ok(true) => tracing::debug!("the ledger's planner statistics were refreshed"),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "the ledger's planner statistics could not be refreshed")
        }
    }
    match purged {
        Ok(purged) => tracing::debug!(?purged, ?history, "ledger retention ran"),
        Err(error) => tracing::warn!(%error, ?history, "ledger retention failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{PriceRevisions, PriceTable};
    use phoebis_store::AccountKind;

    /// A ledger whose channel holds one record and has no writer draining it.
    fn stalled() -> (Ledger, mpsc::Receiver<Record>) {
        let (tx, rx) = mpsc::channel(1);
        let (stop, _) = watch::channel(false);
        let ledger = Ledger {
            db: Arc::new(LedgerDb::open_in_memory().unwrap()),
            stats: Arc::new(LedgerStats::default()),
            assembler: Arc::default(),
            unpriced: Unpriced::default(),
            metrics: Arc::new(Metrics::new()),
            tx,
            stop: Arc::new(stop),
            done: Arc::new(Mutex::new(None)),
            tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            control: mpsc::channel::<ReplayParked>(1).0,
            clock: Arc::new(|| 0),
            retention_days: LedgerConfig::default().retention_days,
        };
        (ledger, rx)
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_record_waits_for_room_in_the_channel_rather_than_drop() {
        let (ledger, mut rx) = stalled();
        ledger.record(Record::Event(EventRecord::default()));
        let landing = tokio::spawn({
            let ledger = ledger.clone();
            let records = vec![Record::Request {
                request: RequestRecord::default(),
                attempts: Vec::new(),
                bodies: None,
            }];
            async move { land_all(&ledger, records).await }
        });
        // The paused clock moves only once every other task is parked, so the sleep ends with the landing run up
        // to its send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ledger.dropped(), 0, "the record was not dropped");
        assert!(
            !landing.is_finished(),
            "the landing waits while the channel is full"
        );
        assert!(matches!(rx.recv().await, Some(Record::Event(_))));
        assert!(matches!(rx.recv().await, Some(Record::Request { .. })));
        landing.await.unwrap();
        assert_eq!(ledger.dropped(), 0);
    }

    #[tokio::test]
    async fn a_failed_spend_refresh_keeps_the_previous_boundary_armed() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (mut spends, _) = spends_over(&[("a", 10.0)], clock.clone());

        let first = refresh_member_spends(
            &spends.handles,
            &spends.handles.router.snapshot(),
            1_000,
            false,
            &[],
        )
        .await;
        assert!(first.due.is_some(), "a working read arms the boundary tick");
        assert!(
            first.refreshed,
            "and reports the read as one that succeeded"
        );
        spends.due = first.due;

        spends
            .handles
            .db
            .write(|c| {
                c.execute("DROP TABLE rollups_hourly", [])?;
                Ok(())
            })
            .unwrap();

        let after_failure = refresh_member_spends(
            &spends.handles,
            &spends.handles.router.snapshot(),
            2_000,
            false,
            &[],
        )
        .await;
        assert!(
            !after_failure.refreshed,
            "a failed read never reads as a refresh, so it cannot clear a degraded ledger"
        );
        clock.store(2_000, Ordering::SeqCst);
        spends.read(&[]).await;
        assert_eq!(
            spends.due, first.due,
            "a failed read keeps the previous boundary armed rather than disarming it"
        );
    }

    #[tokio::test]
    async fn a_failed_spend_refresh_past_its_deadline_retries_at_the_poll_cadence() {
        let now = 1_000;
        let (mut spends, _) = spends_over(&[("a", 10.0)], Arc::new(AtomicI64::new(now)));

        spends
            .handles
            .db
            .write(|c| {
                c.execute("DROP TABLE rollups_hourly", [])?;
                Ok(())
            })
            .unwrap();

        // The boundary tick's own deadline already passed: keeping it would fire again at once on every
        // following timer tick, so the retry arms one poll interval out instead.
        let past_due = Some(500);
        spends.due = past_due;
        spends.read(&[]).await;
        assert_eq!(
            spends.due,
            Some(now + spends.event_poll.as_millis() as i64),
            "a past deadline retries at the poll cadence rather than at once"
        );
    }

    /// The writer's spend refresh over a router whose pool `p` holds the API-key members given with their monthly
    /// budgets, and a ledger priced at a dollar per token of model `m`, read at `clock`, with the registry beside it.
    fn spends_over(members: &[(&str, f64)], clock: Arc<AtomicI64>) -> (Spends, Arc<Store>) {
        let store =
            Arc::new(Store::open_in_memory(&phoebis_store::StorageKey::generate()).unwrap());
        for (id, _) in members {
            store
                .insert_account(&phoebis_store::AccountRow::for_tests(
                    id,
                    AccountKind::ApiKey,
                ))
                .unwrap();
        }
        store
            .insert_pool(&phoebis_store::PoolRow::for_tests("p"), "test")
            .unwrap();
        let writes: Vec<phoebis_store::MemberWrite> = members
            .iter()
            .enumerate()
            .map(|(priority, (id, budget))| phoebis_store::MemberWrite {
                account_id: id.to_string(),
                priority: priority as u32,
                budget_daily_usd: None,
                budget_monthly_usd: Some(*budget),
            })
            .collect();
        store.set_pool_members("p", &writes, "test").unwrap();
        let router = phoebis_routing::spawn(
            store.clone(),
            Arc::new(|| 1_000),
            phoebis_routing::RouterConfig::default(),
        )
        .unwrap();
        let dollar = crate::pricing::ModelPrice {
            input: 1_000_000.0,
            cache_write_5m: 1_000_000.0,
            cache_write_1h: 1_000_000.0,
            cache_read: 1_000_000.0,
            output: 1_000_000.0,
        };
        let table = PriceTable {
            effective: "2026-01-01".into(),
            models: [(
                "m".to_string(),
                crate::pricing::ModelRow {
                    standard: dollar,
                    fast: None,
                    long_context: None,
                },
            )]
            .into(),
        };
        let spends = Spends {
            handles: Handles {
                clock: Arc::new(move || clock.load(Ordering::SeqCst)),
                db: Arc::new(LedgerDb::open_in_memory().unwrap()),
                metrics: Arc::new(Metrics::new()),
                pricing: Arc::new(Pricing::new(
                    PriceRevisions::new(vec![table], "test").expect("one entry is a list"),
                )),
                router,
                stats: Arc::new(LedgerStats::default()),
                tables: Arc::new(PlanTables::shipped()),
                unpriced: Unpriced::default(),
            },
            event_poll: Duration::from_secs(5),
            revision: 0,
            due: None,
            budgets: Budgets::default(),
            pending: Vec::new(),
        };
        (spends, store)
    }

    /// A request on pool `p` whose one attempt on `account` used `tokens` input tokens of `m`, a dollar each: written
    /// to the ledger, and answered as the batch record that carried it.
    fn spend(spends: &Spends, id: &str, account: &str, received_at: i64, tokens: u64) -> Record {
        let request = RequestRecord {
            id: id.into(),
            received_at,
            pool_id: Some("p".into()),
            canonical_model: "m".into(),
            model: "m".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            input_tokens: Some(tokens),
            ..Default::default()
        };
        spends
            .handles
            .db
            .insert_request(&request, std::slice::from_ref(&attempt), None)
            .unwrap();
        Record::Request {
            request,
            attempts: vec![attempt],
            bodies: None,
        }
    }

    /// The budget rows the ledger holds, in order: kind, account, request column and detail.
    async fn budget_rows(
        spends: &Spends,
    ) -> Vec<(String, Option<String>, Option<String>, serde_json::Value)> {
        spends
            .handles
            .db
            .events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind.starts_with("budget_"))
            .map(|e| {
                assert_eq!(e.pool_id.as_deref(), Some("p"));
                let detail = serde_json::from_str(e.detail.as_deref().unwrap()).unwrap();
                (e.kind, e.account_id, e.request_id, detail)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_spend_that_crosses_its_budget_writes_budget_reached_once() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (mut spends, _) = spends_over(&[("a", 10.0)], clock.clone());
        let under = spend(&spends, "r1", "a", 100, 6);
        spends.refresh(&priced_attempts(&[under])).await;
        assert!(budget_rows(&spends).await.is_empty(), "6 of 10 spent");
        let over = spend(&spends, "r2", "a", 200, 6);
        spends.refresh(&priced_attempts(&[over])).await;
        spends.refresh(&[]).await;
        let rows = budget_rows(&spends).await;
        assert_eq!(rows.len(), 1, "two refreshes over the budget write one row");
        let (kind, account, request, detail) = &rows[0];
        assert_eq!(
            (kind.as_str(), account.as_deref(), request.as_deref()),
            ("budget_reached", Some("a"), Some("r2"))
        );
        assert_eq!(
            *detail,
            serde_json::json!({
                "period": "month",
                "budget_usd": 10.0,
                "spent_usd": 12.0,
                "period_end": 2_678_400_000_i64,
                "source_request_id": "r2",
            })
        );
    }

    #[tokio::test]
    async fn a_budget_crossing_names_the_member_s_own_request_and_not_a_sibling_s_in_the_batch() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (mut spends, _) = spends_over(&[("a", 10.0), ("b", 10.0)], clock.clone());
        // One batch: `a`'s request crosses its budget, then a later one of `b` lands beside it.
        let batch = [
            spend(&spends, "ra", "a", 100, 12),
            spend(&spends, "rb", "b", 200, 1),
        ];
        spends.refresh(&priced_attempts(&batch)).await;
        // `b` crosses on spend the timer's refresh finds, with no batch behind it.
        spend(&spends, "rb2", "b", 300, 10);
        spends.refresh(&[]).await;
        let named: Vec<(Option<String>, Option<String>, serde_json::Value)> = budget_rows(&spends)
            .await
            .into_iter()
            .map(|(kind, account, request, detail)| {
                assert_eq!(kind, "budget_reached");
                (account, request, detail["source_request_id"].clone())
            })
            .collect();
        assert_eq!(
            named,
            [
                (Some("a".into()), Some("ra".into()), serde_json::json!("ra")),
                (Some("b".into()), None, serde_json::Value::Null),
            ]
        );
    }

    #[tokio::test]
    async fn a_new_period_writes_budget_period_ended_with_the_previous_spend() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (mut spends, _) = spends_over(&[("a", 10.0)], clock.clone());
        let over = spend(&spends, "r1", "a", 100, 12);
        spends.refresh(&priced_attempts(&[over])).await;
        // The first of February 1970, the next monthly period.
        let february = 2_678_400_000_i64;
        clock.store(february + 1_000, Ordering::SeqCst);
        spends.refresh(&[]).await;
        spends.refresh(&[]).await;
        let rows = budget_rows(&spends).await;
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["budget_reached", "budget_period_ended"]
        );
        assert_eq!(
            (rows[1].1.as_deref(), &rows[1].3),
            (
                Some("a"),
                &serde_json::json!({
                    "period": "month",
                    "previous_spent_usd": 12.0,
                    "period_start": february,
                })
            ),
            "the first refresh of the new period names what the last one spent"
        );
    }

    #[tokio::test]
    async fn a_period_whose_budget_was_never_reached_ends_without_a_row() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (mut spends, _) = spends_over(&[("a", 10.0)], clock.clone());
        let under = spend(&spends, "r1", "a", 100, 6);
        spends.refresh(&priced_attempts(&[under])).await;
        clock.store(2_678_400_000 + 1_000, Ordering::SeqCst);
        spends.refresh(&[]).await;
        assert!(budget_rows(&spends).await.is_empty());
    }

    #[tokio::test]
    async fn a_budget_lowered_under_the_spend_writes_budget_reached() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let (mut spends, store) = spends_over(&[("a", 10.0)], clock.clone());
        let under = spend(&spends, "r1", "a", 100, 6);
        spends.refresh(&priced_attempts(&[under])).await;
        store
            .set_pool_members(
                "p",
                &[phoebis_store::MemberWrite {
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: Some(5.0),
                }],
                "test",
            )
            .unwrap();
        spends.handles.router.reload().await.unwrap();
        spends.refresh(&[]).await;
        spends.refresh(&[]).await;
        let rows = budget_rows(&spends).await;
        assert_eq!(
            rows.iter()
                .map(|r| (
                    r.0.as_str(),
                    r.3["budget_usd"].clone(),
                    r.3["spent_usd"].clone()
                ))
                .collect::<Vec<_>>(),
            [(
                "budget_reached",
                serde_json::json!(5.0),
                serde_json::json!(6.0)
            )],
            "the edit put the spend over the budget, once"
        );
    }
}
