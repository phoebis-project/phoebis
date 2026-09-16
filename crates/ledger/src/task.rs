//! The ledger task: one bounded channel from the assembler and every other producer, one writer that lands
//! batches through `spawn_blocking`, and the gap event that says what was lost.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use phoebis_bus::{spawn_subscriber, Bus, Subscriber};
use phoebis_oauth::{Clock, UsageService};
use phoebis_store::Store;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::assemble::{Assembler, Record};
use crate::bodies::run_retention;
use crate::db::{insert_event_in, insert_request_in, insert_samples_in};
use crate::pricing::{price_attempts, Pricing};
use crate::quota::QuotaPoll;
use crate::record::EventRecord;
use crate::{LedgerDb, LedgerError};

pub const CHANNEL_DEPTH: usize = 4_096;

/// Records landed per transaction: enough to amortise the fsync, few enough that a drain on shutdown is quick.
const BATCH: usize = 64;

/// How often the assembler ages out entries without a completion.
const EXPIRE_INTERVAL: Duration = Duration::from_secs(60);

const TASKS_POISONED: &str = "ledger background-task list poisoned by a prior panic";

#[derive(Clone, Debug)]
pub struct LedgerConfig {
    pub retention_days: u32,
    pub sample_retention_days: u32,
    pub purge_interval: Duration,
    pub quota_poll_interval: Duration,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        LedgerConfig {
            retention_days: 90,
            sample_retention_days: 30,
            purge_interval: Duration::from_secs(3_600),
            quota_poll_interval: Duration::from_secs(3_600),
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
}

#[derive(Debug, thiserror::Error)]
#[error("the ledger task has stopped")]
pub struct LedgerGone;

#[derive(Clone)]
pub struct Ledger {
    db: Arc<LedgerDb>,
    stats: Arc<LedgerStats>,
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

    /// Never waits: the relay's body task and the assembler both call this, and neither may stall on disk.
    pub fn record(&self, record: Record) {
        if self.tx.try_send(record).is_err() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
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
    usage: Option<Arc<UsageService>>,
    bus: &Bus,
    clock: Clock,
    config: LedgerConfig,
) -> Ledger {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    let stats = Arc::new(LedgerStats::default());
    let (done_tx, done_rx) = oneshot::channel();
    let (stop, stopped) = watch::channel(false);
    let ledger = Ledger {
        db: db.clone(),
        stats: stats.clone(),
        tx,
        stop: Arc::new(stop),
        done: Arc::new(Mutex::new(Some(done_rx))),
        tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    if let Some(usage) = usage {
        let poll = QuotaPoll::spawn(
            db.clone(),
            store,
            usage,
            ledger.clone(),
            clock.clone(),
            config.quota_poll_interval,
        );
        ledger.tasks.lock().expect(TASKS_POISONED).push(poll);
    }
    let subscriber = bus.subscribe("ledger");
    let assembling = ledger.clone();
    let assembler_stop = stopped.clone();
    let assembler_clock = clock.clone();
    let assembler = spawn_subscriber(subscriber, move |subscriber| {
        assemble(subscriber, assembling, assembler_clock, assembler_stop)
    });
    tokio::spawn(writer(
        db.clone(),
        pricing,
        rx,
        stopped,
        assembler,
        stats,
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

/// The bus side: every event, chunks included, into the assembler in publish order, completed records into
/// the channel. On stop it drains what the bus already queued before it exits, so a completion published
/// just before shutdown reaches the writer. One receiver, so one `None` ends it.
async fn assemble(
    mut subscriber: Subscriber,
    ledger: Ledger,
    clock: Clock,
    mut stopped: watch::Receiver<bool>,
) {
    let mut assembler = Assembler::new(clock.clone());
    let mut expire = tokio::time::interval(EXPIRE_INTERVAL);
    expire.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = subscriber.events.recv() => match event {
                Some(event) => {
                    for record in assembler.observe(event) {
                        ledger.record(record);
                    }
                }
                None => break,
            },
            _ = expire.tick() => {
                assembler.expire(clock());
            }
            _ = stop_requested(&mut stopped) => {
                while let Ok(event) = subscriber.events.try_recv() {
                    for record in assembler.observe(event) { ledger.record(record); }
                }
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn writer(
    db: Arc<LedgerDb>,
    pricing: Arc<Pricing>,
    mut rx: mpsc::Receiver<Record>,
    mut stopped: watch::Receiver<bool>,
    assembler: tokio::task::JoinHandle<()>,
    stats: Arc<LedgerStats>,
    clock: Clock,
    config: LedgerConfig,
    done: oneshot::Sender<()>,
) {
    let mut assembler = Some(assembler);
    let mut purge = tokio::time::interval(config.purge_interval);
    purge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    purge.tick().await;
    // Drops the writer has not yet reported: the channel's, read from the counter each round, and its own.
    let mut reported: u64 = 0;
    loop {
        let first = tokio::select! {
            // `recv` on a closed channel still yields what was queued before the close, then `None`.
            record = rx.recv() => match record { Some(record) => record, None => break },
            // The flag stays set once flipped, so the arm is guarded to the one round that closes the channel.
            _ = stop_requested(&mut stopped), if assembler.is_some() => {
                // The assembler drains the bus queues first; whatever it enqueues is still ahead of the close.
                if let Some(assembler) = assembler.take() { let _ = assembler.await; }
                rx.close();
                continue;
            }
            _ = purge.tick() => {
                run_purge(&db, &config, clock()).await;
                continue;
            }
        };
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(record) => batch.push(record),
                Err(_) => break,
            }
        }
        land(&db, &pricing, &stats, batch).await;
        // The channel is empty: whatever was dropped since the last report is one gap.
        if rx.is_empty() {
            let dropped = stats.dropped.load(Ordering::Relaxed);
            if dropped > reported {
                let gap = EventRecord {
                    at: clock(),
                    kind: "ledger_gap".into(),
                    detail: Some(format!(r#"{{"dropped":{}}}"#, dropped - reported)),
                    ..Default::default()
                };
                if land(&db, &pricing, &stats, vec![Record::Event(gap)]).await {
                    reported = dropped;
                    stats.gaps.fetch_add(1, Ordering::Relaxed);
                    stats.last_gap_at.store(clock(), Ordering::Relaxed);
                }
            }
        }
    }
    let _ = done.send(());
}

/// One transaction per batch on a blocking thread. A batch that fails is lost as a whole and counted, so the
/// gap that follows names it.
async fn land(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    stats: &Arc<LedgerStats>,
    batch: Vec<Record>,
) -> bool {
    let db = db.clone();
    let pricing = pricing.clone();
    let size = batch.len() as u64;
    let written = tokio::task::spawn_blocking(move || {
        db.write(|conn| {
            let tx = conn.transaction()?;
            for record in &batch {
                match record {
                    Record::Event(event) => insert_event_in(&tx, event)?,
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
                    }
                    Record::Samples { quota, extra } => {
                        insert_samples_in(&tx, quota, extra.as_ref())?
                    }
                }
            }
            tx.commit()?;
            Ok::<(), LedgerError>(())
        })
    })
    .await;
    match written {
        Ok(Ok(())) => {
            stats.written.fetch_add(size, Ordering::Relaxed);
            true
        }
        Ok(Err(error)) => {
            tracing::error!(%error, records = size, "a ledger batch could not be written");
            stats.write_failures.fetch_add(1, Ordering::Relaxed);
            stats.dropped.fetch_add(size, Ordering::Relaxed);
            false
        }
        Err(error) => {
            tracing::error!(%error, records = size, "the ledger writer panicked; the batch is lost");
            stats.write_failures.fetch_add(1, Ordering::Relaxed);
            stats.dropped.fetch_add(size, Ordering::Relaxed);
            false
        }
    }
}

async fn run_purge(db: &Arc<LedgerDb>, config: &LedgerConfig, now: i64) {
    let (db, config) = (db.clone(), config.clone());
    match tokio::task::spawn_blocking(move || run_retention(&db, &config, now)).await {
        Ok(Ok(purged)) => tracing::debug!(?purged, "ledger retention ran"),
        Ok(Err(error)) => tracing::warn!(%error, "ledger retention failed"),
        Err(error) => tracing::error!(%error, "ledger retention panicked"),
    }
}
