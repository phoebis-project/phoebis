//! The ledger task: one bounded channel from the assembler and every other producer, one writer that lands
//! batches through `spawn_blocking`, and the gap event that says what was lost.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use phoebis_bus::{spawn_subscriber, Bus, Delivery, Subscriber};
use phoebis_oauth::Clock;
use phoebis_routing::{RegistrySnapshot, Report, RouterHandle};
use phoebis_store::{AccountKind, Store};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::assemble::{Assembler, Record};
use crate::bodies::run_retention;
use crate::db::{begin_write, insert_event_in, insert_request_in, insert_samples_in};
use crate::events::spawn_event_mirror;
use crate::metrics::Metrics;
use crate::pricing::{price_attempts, PriceTable, Pricing};
use crate::query::{member_spends, BudgetedMember};
use crate::record::{
    AttemptRecord, EventRecord, ParkedRecord, ParkedRows, QuotaSampleRecord, RequestRecord,
};
use crate::{LedgerDb, LedgerError};

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

#[derive(Clone, Debug)]
pub struct LedgerConfig {
    pub retention_days: u32,
    pub rollup_retention_days: u32,
    pub purge_interval: Duration,
    pub event_poll: Duration,
    /// How often the assembler ages out entries whose completion never arrived.
    pub expire_interval: Duration,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        LedgerConfig {
            retention_days: 90,
            rollup_retention_days: 400,
            purge_interval: Duration::from_secs(3_600),
            event_poll: Duration::from_secs(5),
            expire_interval: EXPIRE_INTERVAL,
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
}

/// What the relay's ledger check reads: counted here, since every `behind` answer is one `ledger_behind` refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
    pub behind: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("the ledger task has stopped")]
pub struct LedgerGone;

#[derive(Clone)]
pub struct Ledger {
    db: Arc<LedgerDb>,
    stats: Arc<LedgerStats>,
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
}

impl Ledger {
    pub fn db(&self) -> &Arc<LedgerDb> {
        &self.db
    }

    pub fn stats(&self) -> Arc<LedgerStats> {
        self.stats.clone()
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

    /// Whether the writer still takes records; false once it closed its receiver, at a shutdown or a panic.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
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
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                self.metrics.observe_drop(table_of(&record));
            }
        }
    }

    /// Waits for room in the writer's channel: a request's row must land, or a completed request leaves no
    /// trace at all. A record offered after the channel closed is counted as dropped, as `record` counts one
    /// offered to a full or closed channel.
    pub async fn record_request(&self, record: Record) {
        if let Err(mpsc::error::SendError(record)) = self.tx.send(record).await {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            self.metrics.observe_drop(table_of(&record));
        }
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
    store: Arc<Store>,
    bus: &Bus,
    router: RouterHandle,
    clock: Clock,
    metrics: Arc<Metrics>,
    config: LedgerConfig,
) -> Ledger {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    let stats = Arc::new(LedgerStats::default());
    let (done_tx, done_rx) = oneshot::channel();
    let (stop, stopped) = watch::channel(false);
    let ledger = Ledger {
        db: db.clone(),
        stats: stats.clone(),
        metrics: metrics.clone(),
        tx,
        stop: Arc::new(stop),
        done: Arc::new(Mutex::new(Some(done_rx))),
        tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let mirror = spawn_event_mirror(
        db.clone(),
        ledger.clone(),
        store,
        router.clone(),
        clock.clone(),
        config.event_poll,
    );
    ledger.tasks.lock().expect(TASKS_POISONED).push(mirror);
    let subscriber = bus.subscribe("ledger", Delivery::Durable);
    let assembling = ledger.clone();
    let assembler_stop = stopped.clone();
    let assembler_clock = clock.clone();
    let expire_interval = config.expire_interval;
    let assembler = spawn_subscriber(subscriber, move |subscriber| {
        assemble(
            subscriber,
            assembling,
            assembler_clock,
            expire_interval,
            assembler_stop,
        )
    });
    // The purge has its own task and connection so a pass never takes the writer's lock or turn: the cut runs
    // in slices, so a write waits behind one slice at most and never on a whole day's deletion.
    let purging = ledger.clone();
    let purge_router = router.clone();
    let purge_clock = clock.clone();
    let purge_config = config.clone();
    let purge = tokio::spawn(async move {
        let mut tick = tokio::time::interval(purge_config.purge_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            run_purge(purging.db(), &purge_config, &purge_router, purge_clock()).await;
        }
    });
    ledger.track(purge);
    tokio::spawn(writer(
        db.clone(),
        pricing,
        router,
        rx,
        stopped,
        assembler,
        stats,
        metrics,
        clock,
        config,
        done_tx,
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
    clock: Clock,
    expire_interval: Duration,
    mut stopped: watch::Receiver<bool>,
) {
    let mut assembler = Assembler::new(clock.clone());
    let mut expire = tokio::time::interval(expire_interval);
    expire.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = subscriber.events.recv() => match event {
                Some(event) => land_all(&ledger, assembler.observe(event)).await,
                None => break,
            },
            _ = expire.tick() => {
                let now = clock();
                for id in assembler.expire(now).ids {
                    // The live table ends the same request as `lost`; the ledger names it so the system page's
                    // gap list has a row for it rather than a count with nothing behind it.
                    ledger.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    ledger.metrics.observe_drop("requests");
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
    db: Arc<LedgerDb>,
    pricing: Arc<Pricing>,
    router: RouterHandle,
    mut rx: mpsc::Receiver<Record>,
    mut stopped: watch::Receiver<bool>,
    assembler: tokio::task::JoinHandle<()>,
    stats: Arc<LedgerStats>,
    metrics: Arc<Metrics>,
    clock: Clock,
    config: LedgerConfig,
    done: oneshot::Sender<()>,
) {
    let mut assembler = Some(assembler);
    // The assembler's handle once the stop was seen: the writer keeps reading while it waits for the
    // assembler to end, since the assembler may be waiting on this very channel for room.
    let mut ending: Option<tokio::task::JoinHandle<()>> = None;
    // A registry change wakes the writer through the mirror's own `registry_changed` record; when that record
    // lands ahead of `router.reload()` publishing the new revision, the top-of-loop check sees the old one and
    // nothing else wakes the loop again until the next record or a boundary tick days out.
    // This arm re-checks the revision at the mirror's own cadence regardless, at the cost of one extra
    // `router.snapshot()` per poll.
    let mut revision_poll = tokio::time::interval(config.event_poll);
    revision_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    revision_poll.tick().await;
    // Drops the writer has not yet reported: the channel's, read from the counter each round, and its own.
    let mut reported: u64 = 0;
    let mut member_revision = router.snapshot().revision;
    let refresh_spend = |due: Option<i64>| {
        refresh(
            &db,
            &pricing,
            &router,
            &stats,
            &clock,
            due,
            config.event_poll,
        )
    };
    // Ahead of the start's spend read, so a parked priced record that lands now is in the first report.
    land_parked(&db, &pricing, &stats, &metrics, clock()).await;
    let mut spend_due = refresh_spend(None).await;
    // A batch whose transaction failed: while it is held the loop takes no new record, so order holds and the
    // channel fills behind it, which is what moves admission's mark before more money is spent.
    let mut held: Option<Held> = None;
    loop {
        if let Some(batch) = held.take() {
            let mut ctx = WriterCtx {
                db: &db,
                pricing: &pricing,
                stats: &stats,
                metrics: &metrics,
                clock: &clock,
                stopped: &mut stopped,
            };
            let retry = if batch.retried {
                hold(&mut ctx, batch).await
            } else {
                retry_held(&mut ctx, batch).await
            };
            match retry {
                Retry::Held(rest) => held = Some(rest),
                // Back to the turn below once the drain's one try is done, so the stop arm and `ended` still close
                // and drain the channel.
                Retry::Stopped(rest) => {
                    drain_held(&mut ctx, rest).await;
                    stats.retrying.store(0, Ordering::Relaxed);
                }
                Retry::Settled => {
                    stats.retrying.store(0, Ordering::Relaxed);
                    member_revision = router.snapshot().revision;
                    spend_due = refresh_spend(spend_due).await;
                }
            }
            continue;
        }
        // Checked on every round, whatever woke it: a member's budget or period edited under the writer,
        // with no flush of its own, still recomputes without waiting on the next request or period boundary.
        let revision = router.snapshot().revision;
        if revision != member_revision {
            member_revision = revision;
            spend_due = refresh_spend(spend_due).await;
        }
        let first = tokio::select! {
            // `recv` on a closed channel still yields what was queued before the close, then `None`.
            record = rx.recv() => match record { Some(record) => record, None => break },
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
            // Nothing but the top-of-loop check runs here; see the arm's own comment above.
            _ = revision_poll.tick() => continue,
            // Re-armed after every run: the boundary a budgeted member's period ends on, so a pool whose
            // traffic never touches this writer still turns over its spend at the period's edge.
            _ = sleep_until(spend_due, &clock) => {
                spend_due = refresh_spend(spend_due).await;
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
        let priced = wrote_priced_attempt(&records);
        let batch = Arc::new(records);
        match land(&db, &pricing, &stats, &metrics, &batch, 0..batch.len()).await {
            // A degraded ledger reads the spend after any batch that lands, since that read is what clears it.
            Landed::Written => {
                if priced || stats.degraded.load(Ordering::Relaxed) {
                    member_revision = router.snapshot().revision;
                    spend_due = refresh_spend(spend_due).await;
                }
            }
            Landed::Refused { .. } => {
                stats.retrying.store(1, Ordering::Relaxed);
                if !stats.degraded.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "a ledger batch could not be written; budgeted members wait until it lands"
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
        // The channel is empty: whatever was dropped since the last report is one gap.
        if rx.is_empty() {
            let dropped = stats.dropped.load(Ordering::Relaxed);
            if dropped > reported {
                let gap = Arc::new(vec![Record::Event(EventRecord {
                    at: clock(),
                    kind: "ledger_gap".into(),
                    detail: Some(format!(r#"{{"dropped":{}}}"#, dropped - reported)),
                    ..Default::default()
                })]);
                if let Landed::Written = land(&db, &pricing, &stats, &metrics, &gap, 0..1).await {
                    reported = dropped;
                    stats.gaps.fetch_add(1, Ordering::Relaxed);
                    stats.last_gap_at.store(clock(), Ordering::Relaxed);
                }
            }
        }
    }
    let _ = done.send(());
}

/// A batch the writer holds while it lands: shared with the blocking worker by reference count, so a failed batch is
/// retried and its records replayed or parked without a copy.
type Batch = Arc<Vec<Record>>;

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

/// The writer's own handles, for the retries. The writer is what the `Ledger` handle sends to, so a record of its
/// own (a poison row's gap) lands through `land`, as the gap report's does.
struct WriterCtx<'a> {
    db: &'a Arc<LedgerDb>,
    pricing: &'a Arc<Pricing>,
    stats: &'a Arc<LedgerStats>,
    metrics: &'a Arc<Metrics>,
    clock: &'a Clock,
    stopped: &'a mut watch::Receiver<bool>,
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
    loop {
        let now = clock();
        if now >= deadline {
            return Wait::Elapsed;
        }
        let slice = CLOCK_TICK.min(Duration::from_millis((deadline - now) as u64));
        tokio::select! {
            _ = tokio::time::sleep(slice) => {}
            _ = stop_requested(stopped) => return Wait::Stopped,
        }
    }
}

/// A failed batch is retried in place, then replayed one record at a time. Nothing new is taken from the channel
/// while this runs, so order holds and admission's mark fires behind the held batch.
async fn retry_held(ctx: &mut WriterCtx<'_>, held: Held) -> Retry {
    for delay in BATCH_RETRY_DELAYS {
        if wait_on_clock(delay, ctx.clock, ctx.stopped).await == Wait::Stopped {
            return Retry::Stopped(held);
        }
        let rest = held.from..held.records.len();
        match land(
            ctx.db,
            ctx.pricing,
            ctx.stats,
            ctx.metrics,
            &held.records,
            rest,
        )
        .await
        {
            Landed::Written | Landed::Lost => return Retry::Settled,
            Landed::Refused { .. } => {}
        }
    }
    replay(ctx, held).await
}

/// A batch the database as a whole refused is tried again every `BATCH_RETRY_HOLD`, one record at a time, until it
/// lands or the process stops; it is never dropped.
async fn hold(ctx: &mut WriterCtx<'_>, held: Held) -> Retry {
    if wait_on_clock(BATCH_RETRY_HOLD, ctx.clock, ctx.stopped).await == Wait::Stopped {
        return Retry::Stopped(held);
    }
    replay(ctx, held).await
}

/// One record per transaction. A record the database as a whole refuses is held with everything after it; one
/// refused on its own row is a poison row, parked when it carries money and dropped with its gap otherwise.
async fn replay(ctx: &mut WriterCtx<'_>, held: Held) -> Retry {
    for index in held.from..held.records.len() {
        let one = index..index + 1;
        let error = match land(
            ctx.db,
            ctx.pricing,
            ctx.stats,
            ctx.metrics,
            &held.records,
            one,
        )
        .await
        {
            Landed::Written | Landed::Lost => continue,
            // A full disk, an I/O error or a lock held past the timeout refuses every record after this one as
            // well, so the rest of the batch waits with it; a constraint or a trigger refused this row alone.
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
                if park(ctx, request, attempts, bodies.is_some(), &error)
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
                drop_counted(ctx, record);
                // The writer lands its own gap as a batch of one, as the gap report does: a send into its own
                // channel would meet the queue it stopped draining.
                let gap = Arc::new(vec![crate::assemble::gap(
                    &request.id,
                    "poison",
                    (ctx.clock)(),
                )]);
                let _ = land(ctx.db, ctx.pricing, ctx.stats, ctx.metrics, &gap, 0..1).await;
            }
            other => drop_counted(ctx, other),
        }
    }
    Retry::Settled
}

/// Parks a priced record the database refused on its own row: one `parked_records` row with the request row and
/// its attempts and without the captured bodies, so the capture is lost and the money is not.
async fn park(
    ctx: &WriterCtx<'_>,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    captured: bool,
    error: &str,
) -> Result<(), LedgerError> {
    let record = serde_json::to_string(&ParkedRows { request, attempts })?;
    let at = (ctx.clock)();
    let request_id = request.id.clone();
    let pool_id = request.pool_id.clone();
    let account_id = attempts.last().map(|a| a.account_id.clone());
    let refusal = error.to_string();
    let parked = blocking(ctx.db, move |db| {
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
            ctx.stats.parked_priced.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                request_id = %request.id,
                pool = request.pool_id.as_deref().unwrap_or(crate::rollup::UNATTRIBUTED),
                %error,
                capture_dropped = captured,
                "a priced record could not be written; it is parked and tried again at the next start"
            );
        }
        Err(park_error) => tracing::error!(
            request_id = %request.id,
            error = %park_error,
            "a priced record could not be parked; it stays held"
        ),
    }
    parked
}

fn drop_counted(ctx: &WriterCtx<'_>, record: &Record) {
    ctx.stats.dropped.fetch_add(1, Ordering::Relaxed);
    ctx.metrics.observe_drop(table_of(record));
}

/// The stop's last try at a held batch: one replay, and what the database still refuses is lost and counted, the
/// one thing a stop may lose.
async fn drain_held(ctx: &mut WriterCtx<'_>, held: Held) {
    let rest = match replay(ctx, held).await {
        Retry::Held(rest) | Retry::Stopped(rest) => rest,
        Retry::Settled => return,
    };
    let lost = &rest.records[rest.from..];
    tracing::error!(
        records = lost.len(),
        "the ledger stopped with records it could not write; they are lost"
    );
    for record in lost {
        drop_counted(ctx, record);
    }
}

/// Every parked record is tried once at start, in the order it was parked, each in its own transaction: one that
/// lands leaves the table and is observed as a landed record; one that fails keeps its row with the error it met
/// this time.
async fn land_parked(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    stats: &Arc<LedgerStats>,
    metrics: &Arc<Metrics>,
    now: i64,
) {
    let rows = match blocking(db, |db| db.parked_rows()).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "the parked records could not be read; they wait for the next start");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    let (mut landed, mut kept) = (0u64, 0u64);
    for row in rows {
        let outcome = match serde_json::from_str::<ParkedRecord>(&row.record) {
            Ok(parked) => {
                let batch = Arc::new(vec![Record::Request {
                    request: parked.request,
                    attempts: parked.attempts,
                    bodies: None,
                }]);
                land(db, pricing, stats, metrics, &batch, 0..1).await
            }
            Err(error) => Landed::Refused {
                database_wide: false,
                error: error.to_string(),
            },
        };
        let id = row.id;
        let settled = match outcome {
            Landed::Written => {
                landed += 1;
                blocking(db, move |db| db.delete_parked(id)).await
            }
            Landed::Refused { error, .. } => {
                kept += 1;
                blocking(db, move |db| db.update_parked(id, &error, now)).await
            }
            Landed::Lost => {
                kept += 1;
                blocking(db, move |db| {
                    db.update_parked(id, "the write panicked", now)
                })
                .await
            }
        };
        if let Err(error) = settled {
            tracing::warn!(%error, id, "a parked record's row could not be updated");
        }
    }
    tracing::info!(landed, kept, "the parked records were tried at start");
}

/// Runs `f` against the ledger on a blocking worker; a worker that died answers as an I/O error.
async fn blocking<T: Send + 'static>(
    db: &Arc<LedgerDb>,
    f: impl FnOnce(&LedgerDb) -> Result<T, LedgerError> + Send + 'static,
) -> Result<T, LedgerError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .map_err(|join| LedgerError::Io(std::io::Error::other(join.to_string())))?
}

/// Every budgeted period of every pool member, from the snapshot; a subscription member's sum reads its metered
/// rows alone (overage and fast), since its plan-funded traffic is no spend.
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

/// What a spend refresh did: the deadline to arm next, and whether the read succeeded and its reports reached the
/// router, which is what lets a degraded ledger clear.
#[derive(Debug)]
struct SpendRefresh {
    due: Option<i64>,
    refreshed: bool,
}

/// Runs `refresh_member_spends` and settles the ledger's standing on it. The writer calls this with nothing held,
/// so a read that succeeded and reached the router clears `degraded`; one that did not, while degraded, runs again
/// one `event_poll` later rather than at the period's end, since every budgeted member waits on it.
async fn refresh(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    router: &RouterHandle,
    stats: &LedgerStats,
    clock: &Clock,
    current: Option<i64>,
    event_poll: Duration,
) -> Option<i64> {
    let now = clock();
    let degraded = stats.degraded.load(Ordering::Relaxed);
    let spend =
        refresh_member_spends(db, pricing, router, now, current, event_poll, degraded).await;
    if spend.refreshed {
        if stats.degraded.swap(false, Ordering::Relaxed) {
            tracing::info!("the ledger caught up; budgeted members are placed on again");
        }
        return spend.due;
    }
    if !degraded {
        return spend.due;
    }
    let retry = now + event_poll.as_millis() as i64;
    Some(spend.due.map_or(retry, |due| due.min(retry)))
}

/// Re-reads every budgeted member's spend and reports it to the router, then the ledger's standing after it, which
/// is current: the writer runs this with nothing held, so a `degraded` ledger clears on a read that succeeds.
/// Answers the earliest `period_end` among the rows read, to arm the next boundary tick; `None` while no member
/// carries a budget, since a pool whose every member is excluded still refreshes on the revision and boundary arms
/// alone, without traffic, and there is truly nothing to arm a tick for, and nothing to read is a read that
/// succeeded. On a read error, keeps `current` while it is still ahead
/// of `now` (a transient failure leaves an armed tick where it was rather than disarming it), otherwise arms a retry
/// at `now + event_poll`: a tick that had already reached its deadline would otherwise re-fire at once on every
/// following timer tick, repeating the failing read and its warning for as long as the error lasts.
async fn refresh_member_spends(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    router: &RouterHandle,
    now: i64,
    current: Option<i64>,
    event_poll: Duration,
    degraded: bool,
) -> SpendRefresh {
    let members = budgeted_members(&router.snapshot());
    if members.is_empty() {
        return SpendRefresh {
            due: None,
            refreshed: !degraded || reached(router, Report::Accounting { degraded: false }).await,
        };
    }
    let table = pricing.current();
    let rows = db
        .read(move |conn| member_spends(conn, &members, now, &table))
        .await;
    match rows {
        Ok(rows) => {
            let earliest = rows.iter().map(|r| r.period_end).min();
            // The standing follows the rows on the same queue, so the selector resumes on spend it has read.
            let refreshed = reached(router, Report::MemberSpend(rows)).await
                && reached(router, Report::Accounting { degraded: false }).await;
            SpendRefresh {
                due: earliest,
                refreshed,
            }
        }
        Err(error) => {
            tracing::warn!(%error, "a member's spend could not be read; the last report stands");
            let due = match current {
                Some(at) if at > now => current,
                _ => Some(now + event_poll.as_millis() as i64),
            };
            SpendRefresh {
                due,
                refreshed: false,
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

/// Whether the batch landed a request whose attempt carried usage: what `price_attempts` would have priced,
/// the signal the member spend refresh reacts to.
fn wrote_priced_attempt(batch: &[Record]) -> bool {
    batch.iter().any(|record| match record {
        Record::Request { attempts, .. } => attempts.iter().any(AttemptRecord::has_usage),
        _ => false,
    })
}

fn table_of(record: &Record) -> &'static str {
    match record {
        Record::Event(_) => "events",
        Record::Keepalive(_) => "rollups",
        Record::Request { .. } => "requests",
        Record::Samples { .. } => "samples",
    }
}

/// What a landed batch reports to the metrics registry, once the transaction that produced it has committed.
// Boxing the request would put an allocation on a path every landed request takes, to shrink a value built
// once per batch and consumed immediately after.
#[allow(clippy::large_enum_variant)]
enum Observation {
    Request {
        table: Arc<PriceTable>,
        request: RequestRecord,
        attempts: Vec<AttemptRecord>,
    },
    Samples {
        quota: Vec<QuotaSampleRecord>,
    },
}

/// One transaction over `range` of the batch on a blocking thread. A transaction that fails rolls back and leaves
/// the records to the caller to retry, replay or park; only a panicking closure drops them, counted, so the gap
/// that follows names them. Metrics are observed only after the commit succeeds, so a failed write never reaches
/// the request or sample series.
async fn land(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    stats: &Arc<LedgerStats>,
    metrics: &Arc<Metrics>,
    batch: &Batch,
    range: Range<usize>,
) -> Landed {
    let db = db.clone();
    let pricing = pricing.clone();
    let records = batch.clone();
    let size = range.len() as u64;
    let tables: Vec<&'static str> = batch[range.clone()].iter().map(table_of).collect();
    let written = tokio::task::spawn_blocking(move || {
        let mut observations = Vec::new();
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
                        let table = pricing.current();
                        let mut attempts = attempts.to_vec();
                        price_attempts(
                            &table,
                            &request.canonical_model,
                            &mut attempts,
                            pricing.unknown_counter(),
                        );
                        let previous = insert_request_in(&tx, request, &attempts, bodies.as_ref())?;
                        crate::rollup::fold_request(
                            &tx,
                            request,
                            &attempts,
                            previous.as_ref().map(|(r, a)| (r, a.as_slice())),
                        )?;
                        observations.push(Observation::Request {
                            table,
                            request: request.clone(),
                            attempts,
                        });
                    }
                    Record::Samples { quota, extra } => {
                        insert_samples_in(&tx, quota, extra.as_ref())?;
                        observations.push(Observation::Samples {
                            quota: quota.clone(),
                        });
                    }
                }
            }
            tx.commit()?;
            Ok::<(), LedgerError>(())
        });
        (result, observations)
    })
    .await;
    match written {
        Ok((Ok(()), observations)) => {
            stats.written.fetch_add(size, Ordering::Relaxed);
            for observation in observations {
                match observation {
                    Observation::Request {
                        table,
                        request,
                        attempts,
                    } => metrics.observe_request(&table, &request, &attempts),
                    Observation::Samples { quota } => metrics.observe_samples(&quota),
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
            tracing::error!(%error, records = size, "the ledger writer panicked; the batch is lost");
            stats.write_failures.fetch_add(1, Ordering::Relaxed);
            stats.dropped.fetch_add(size, Ordering::Relaxed);
            for table in &tables {
                metrics.observe_drop(table);
            }
            Landed::Lost
        }
    }
}

async fn run_purge(db: &Arc<LedgerDb>, config: &LedgerConfig, router: &RouterHandle, now: i64) {
    let budgeted = budgeted_members(&router.snapshot());
    match run_retention(db.clone(), config.clone(), now, budgeted).await {
        Ok(purged) => tracing::debug!(?purged, "ledger retention ran"),
        Err(error) => tracing::warn!(%error, "ledger retention failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::AccountKind;

    /// A ledger whose channel holds one record and has no writer draining it.
    fn stalled() -> (Ledger, mpsc::Receiver<Record>) {
        let (tx, rx) = mpsc::channel(1);
        let (stop, _) = watch::channel(false);
        let ledger = Ledger {
            db: Arc::new(LedgerDb::open_in_memory().unwrap()),
            stats: Arc::new(LedgerStats::default()),
            metrics: Arc::new(Metrics::new()),
            tx,
            stop: Arc::new(stop),
            done: Arc::new(Mutex::new(None)),
            tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        (ledger, rx)
    }

    #[tokio::test]
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

    /// A store carrying one pool, one account, and one budgeted member, and the router spawned over it: enough
    /// for `refresh_member_spends` to find a member to read.
    fn budgeted_router() -> RouterHandle {
        let store =
            Arc::new(Store::open_in_memory(&phoebis_store::StorageKey::generate()).unwrap());
        store
            .insert_account(&phoebis_store::AccountRow {
                id: "a".into(),
                kind: AccountKind::Oauth,
                label: "a".into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: phoebis_store::AccountState::Ready,
            })
            .unwrap();
        store
            .insert_pool(
                &phoebis_store::PoolRow {
                    id: "p".into(),
                    label: "p".into(),
                    enabled: true,
                    strategy: phoebis_store::Strategy::Priority,
                    affinity_ttl_seconds: 3_600,
                    overage_policy: phoebis_store::OveragePolicy::PreferPlan,
                    migrate_above_utilization: None,
                    select_below_utilization: None,
                    model_rules: Vec::new(),
                    keepalive_json: "{}".into(),
                    body_capture_hours: 0,
                    prefer_subscription: false,
                    shedding: phoebis_store::Shedding::default(),
                    failover: phoebis_store::Failover::default(),
                    cache_ttl_json: "{}".into(),
                    cache_diagnostics: false,
                    auto_mode_server: true,
                    affinity: phoebis_store::Affinity::default(),
                    created_at: 0,
                    updated_at: 0,
                },
                "test",
            )
            .unwrap();
        store
            .set_pool_members(
                "p",
                &[phoebis_store::MemberWrite {
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: Some(10.0),
                }],
                "test",
            )
            .unwrap();
        let clock: Clock = Arc::new(|| 1_000);
        phoebis_routing::spawn(store, clock, phoebis_routing::RouterConfig::default()).unwrap()
    }

    #[tokio::test]
    async fn a_failed_spend_refresh_keeps_the_previous_boundary_armed() {
        let router = budgeted_router();
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let pricing = Arc::new(Pricing::new(PriceTable::shipped()));
        let event_poll = Duration::from_secs(5);

        let first =
            refresh_member_spends(&db, &pricing, &router, 1_000, None, event_poll, false).await;
        assert!(first.due.is_some(), "a working read arms the boundary tick");
        assert!(
            first.refreshed,
            "and reports the read as one that succeeded"
        );

        db.write(|c| {
            c.execute("DROP TABLE rollups_hourly", [])?;
            Ok(())
        })
        .unwrap();

        let after_failure =
            refresh_member_spends(&db, &pricing, &router, 2_000, first.due, event_poll, false)
                .await;
        assert_eq!(
            after_failure.due, first.due,
            "a failed read keeps the previous boundary armed rather than disarming it"
        );
        assert!(
            !after_failure.refreshed,
            "a failed read never reads as a refresh, so it cannot clear a degraded ledger"
        );
    }

    #[tokio::test]
    async fn a_failed_spend_refresh_past_its_deadline_retries_at_the_poll_cadence() {
        let router = budgeted_router();
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let pricing = Arc::new(Pricing::new(PriceTable::shipped()));
        let event_poll = Duration::from_secs(5);

        db.write(|c| {
            c.execute("DROP TABLE rollups_hourly", [])?;
            Ok(())
        })
        .unwrap();

        // The boundary tick's own deadline already passed: keeping it would fire again at once on every
        // following timer tick, so the retry arms one poll interval out instead.
        let past_due = Some(500);
        let now = 1_000;
        let after_failure =
            refresh_member_spends(&db, &pricing, &router, now, past_due, event_poll, false).await;
        assert_eq!(
            after_failure.due,
            Some(now + event_poll.as_millis() as i64),
            "a past deadline retries at the poll cadence rather than at once"
        );
    }
}
