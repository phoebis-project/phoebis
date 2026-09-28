//! The ledger task: one bounded channel from the assembler and every other producer, one writer that lands
//! batches through `spawn_blocking`, and the gap event that says what was lost.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use phoebis_bus::{spawn_subscriber, Bus, Subscriber};
use phoebis_oauth::{Clock, UsageService};
use phoebis_routing::{RegistrySnapshot, Report, RouterHandle};
use phoebis_store::{AccountKind, BudgetPeriod, Store};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::assemble::{Assembler, Record};
use crate::bodies::run_retention;
use crate::db::{insert_event_in, insert_request_in, insert_samples_in};
use crate::events::spawn_event_mirror;
use crate::fit::Fitter;
use crate::metrics::Metrics;
use crate::plan::{PlanFactors, PlanWindows};
use crate::pricing::{price_attempts, PriceTable, Pricing};
use crate::query::member_spends;
use crate::quota::QuotaPoll;
use crate::record::{AttemptRecord, EventRecord, QuotaSampleRecord, RequestRecord};
use crate::{LedgerDb, LedgerError};

pub const CHANNEL_DEPTH: usize = 4_096;

/// Records landed per transaction: enough to amortise the fsync, few enough that a drain on shutdown is quick.
const BATCH: usize = 64;

/// How often the assembler ages out entries without a completion, unless the configuration says otherwise.
const EXPIRE_INTERVAL: Duration = Duration::from_secs(60);

const TASKS_POISONED: &str = "ledger background-task list poisoned by a prior panic";

#[derive(Clone, Debug)]
pub struct LedgerConfig {
    pub retention_days: u32,
    pub rollup_retention_days: u32,
    pub sample_retention_days: u32,
    pub purge_interval: Duration,
    pub quota_poll_interval: Duration,
    pub event_poll: Duration,
    /// How often the assembler ages out entries whose completion never arrived.
    pub expire_interval: Duration,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        LedgerConfig {
            retention_days: 90,
            rollup_retention_days: 400,
            sample_retention_days: 30,
            purge_interval: Duration::from_secs(3_600),
            quota_poll_interval: Duration::from_secs(3_600),
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

    /// Registers a task the ledger owns beyond the writer, so `shutdown` aborts it the same way it aborts the
    /// periodic poll: a fetch still running when the process stops must not keep a strong reference (the
    /// store, for instance) alive past that.
    pub(crate) fn track(&self, handle: tokio::task::JoinHandle<()>) {
        self.tasks.lock().expect(TASKS_POISONED).push(handle);
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
    usage: Option<Arc<UsageService>>,
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
    if let Some(usage) = usage {
        let poll = QuotaPoll::spawn(
            db.clone(),
            store.clone(),
            usage,
            ledger.clone(),
            clock.clone(),
            config.quota_poll_interval,
        );
        ledger.tasks.lock().expect(TASKS_POISONED).push(poll);
    }
    let mirror = spawn_event_mirror(
        db.clone(),
        ledger.clone(),
        store.clone(),
        router.clone(),
        clock.clone(),
        config.event_poll,
    );
    ledger.tasks.lock().expect(TASKS_POISONED).push(mirror);
    let subscriber = bus.subscribe("ledger");
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
    tokio::spawn(writer(
        db.clone(),
        pricing,
        store,
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
                land_all(&ledger, assembler.expire(clock()).records).await;
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
/// the request that produced it is gone. So do the attempt records: the fitter never ages an attempt out on its
/// own, so a lost `AttemptDropped` would hold one open, and every foreign rise on its account would count as the
/// router's, until a restart. An event or a sample keeps the drop-and-count `record` always used.
async fn land_all(ledger: &Ledger, records: Vec<Record>) {
    for record in records {
        match record {
            Record::AttemptDropped { .. }
            | Record::AttemptStarted { .. }
            | Record::Request { .. } => ledger.record_request(record).await,
            other => ledger.record(other),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn writer(
    db: Arc<LedgerDb>,
    pricing: Arc<Pricing>,
    store: Arc<Store>,
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
    let mut purge = tokio::time::interval(config.purge_interval);
    purge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    purge.tick().await;
    // A registry change wakes the writer through the mirror's own `registry_changed` record; when that record
    // lands ahead of `router.reload()` publishing the new revision, the top-of-loop check sees the old one and
    // nothing else wakes the loop again until the next record, the hourly purge, or a boundary tick days out.
    // This arm re-checks the revision at the mirror's own cadence regardless, at the cost of one extra
    // `router.snapshot()` per poll; a price confirmation, which wakes nothing of its own, is seen here too.
    let mut revision_poll = tokio::time::interval(config.event_poll);
    revision_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    revision_poll.tick().await;
    // Drops the writer has not yet reported: the channel's, read from the counter each round, and its own.
    let mut reported: u64 = 0;
    let mut fitter = Fitter::new(
        Arc::new(PlanFactors::shipped()),
        Arc::new(PlanWindows::shipped()),
        persisted_fits(store).await,
    );
    let mut member_revision = router.snapshot().revision;
    // A confirmed price table moves no registry revision and prices no request, and a member refused at its
    // budget sends no traffic that would, so the table's version is watched beside the revision.
    let mut priced_with = pricing.current().version.clone();
    let mut spend_due =
        refresh_member_spends(&db, &pricing, &router, clock(), None, config.event_poll).await;
    loop {
        // Checked on every round, whatever woke it: a member's budget or period edited under the writer,
        // with no flush of its own, still recomputes without waiting on the next request or period boundary.
        let revision = router.snapshot().revision;
        let table = pricing.current();
        if revision != member_revision || table.version != priced_with {
            member_revision = revision;
            priced_with = table.version.clone();
            spend_due = refresh_member_spends(
                &db,
                &pricing,
                &router,
                clock(),
                spend_due,
                config.event_poll,
            )
            .await;
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
            _ = purge.tick() => {
                run_purge(&db, &config, &router, clock()).await;
                continue;
            }
            // Nothing but the top-of-loop check runs here; see the arm's own comment above.
            _ = revision_poll.tick() => continue,
            // Re-armed after every run: the boundary a budgeted member's period ends on, so a pool whose
            // traffic never touches this writer still turns over its spend at the period's edge.
            _ = sleep_until(spend_due, &clock) => {
                spend_due =
                    refresh_member_spends(&db, &pricing, &router, clock(), spend_due, config.event_poll).await;
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
        fit_batch(&mut fitter, &router, &clock, &batch);
        let priced = wrote_priced_attempt(&batch);
        let landed = land(&db, &pricing, &stats, &metrics, batch).await;
        if landed && priced {
            member_revision = router.snapshot().revision;
            priced_with = pricing.current().version.clone();
            spend_due = refresh_member_spends(
                &db,
                &pricing,
                &router,
                clock(),
                spend_due,
                config.event_poll,
            )
            .await;
        }
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
                if land(&db, &pricing, &stats, &metrics, vec![Record::Event(gap)]).await {
                    reported = dropped;
                    stats.gaps.fetch_add(1, Ordering::Relaxed);
                    stats.last_gap_at.store(clock(), Ordering::Relaxed);
                }
            }
        }
    }
    let _ = done.send(());
}

/// The fit rows the router persisted, the fitter's seed; a store that cannot answer starts every window over.
async fn persisted_fits(store: Arc<Store>) -> Vec<phoebis_store::FitRow> {
    match tokio::task::spawn_blocking(move || store.fits()).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(error)) => {
            tracing::warn!(%error, "the window fits could not be read; every window starts over");
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(%error, "reading the window fits panicked; every window starts over");
            Vec::new()
        }
    }
}

/// Every pool member with a spend budget, from the snapshot: its pool, account, and configured period.
fn budgeted_members(snapshot: &RegistrySnapshot) -> Vec<(String, String, BudgetPeriod)> {
    snapshot
        .budgeted_members()
        .map(|m| (m.pool_id.clone(), m.account_id.clone(), m.budget_period))
        .collect()
}

/// Re-reads every budgeted member's spend and reports it to the router. Answers the earliest `period_end`
/// among the rows read, to arm the next boundary tick; `None` while no member carries a budget, since a pool
/// whose every member is excluded still refreshes on the revision and boundary arms alone, without traffic, and
/// there is truly nothing to arm a tick for. On a read error, keeps `current` while it is still ahead of `now`
/// (a transient failure leaves an armed tick where it was rather than disarming it), otherwise arms a retry at
/// `now + event_poll`: a tick that had already reached its deadline would otherwise re-fire at once on every
/// following timer tick, repeating the failing read and its warning for as long as the error lasts.
async fn refresh_member_spends(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    router: &RouterHandle,
    now: i64,
    current: Option<i64>,
    event_poll: Duration,
) -> Option<i64> {
    let members = budgeted_members(&router.snapshot());
    if members.is_empty() {
        return None;
    }
    let table = pricing.current();
    let rows = db
        .read(move |conn| member_spends(conn, &members, now, &table))
        .await;
    match rows {
        Ok(rows) => {
            let earliest = rows.iter().map(|r| r.period_end).min();
            router.report(Report::MemberSpend(rows));
            earliest
        }
        Err(error) => {
            tracing::warn!(%error, "a member's spend could not be read; the last report stands");
            match current {
                Some(at) if at > now => current,
                _ => Some(now + event_poll.as_millis() as i64),
            }
        }
    }
}

/// The duration until `deadline`, zero rather than negative for one already past; `None` sleeps forever, so the
/// select arm this backs never fires while no member carries a budget.
async fn sleep_until(deadline: Option<i64>, clock: &Clock) {
    match deadline {
        Some(at) => tokio::time::sleep(Duration::from_millis((at - clock()).max(0) as u64)).await,
        None => std::future::pending().await,
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

/// Walks a batch through the fitter in the order it arrived, reporting every row it answers to the router,
/// whose writer persists it. Runs whether or not the batch then lands: the fit reads what happened, not what
/// the ledger kept.
fn fit_batch(fitter: &mut Fitter, router: &RouterHandle, clock: &Clock, batch: &[Record]) {
    for record in batch {
        match record {
            Record::AttemptStarted {
                account_id,
                request_id,
                attempt,
                at,
            } => fitter.started(account_id, request_id, *attempt, *at),
            Record::AttemptDropped {
                account_id,
                request_id,
                attempt,
                ..
            } => fitter.dropped(account_id, request_id, *attempt),
            Record::Request {
                request, attempts, ..
            } => {
                // Probes count, since the meter charges them; an attempt that answered no usage closes and
                // books nothing, and so does one on an API-key account, which has no plan window to fit.
                for attempt in attempts {
                    if attempt.account_kind != "oauth" {
                        fitter.dropped(&attempt.account_id, &attempt.request_id, attempt.attempt);
                        continue;
                    }
                    let completed = attempt
                        .finished_at
                        .or(request.finished_at)
                        .unwrap_or_else(|| clock());
                    fitter.dispatched(
                        &attempt.account_id,
                        &attempt.request_id,
                        attempt.attempt,
                        &request.canonical_model,
                        attempt,
                        completed,
                    );
                }
            }
            Record::Samples { quota, carrier, .. } => {
                let snapshot = router.snapshot();
                // The record carries only its five-minute bucket; the writer sees it within moments of the
                // reading, which is the precision the fit's one-minute gate needs.
                let now = clock();
                for sample in quota {
                    let Some(utilization) = sample.utilization else {
                        continue;
                    };
                    let Some(account) = snapshot
                        .account(&sample.account_id)
                        .filter(|account| account.kind == AccountKind::Oauth)
                    else {
                        continue;
                    };
                    if let Some(row) = fitter.sampled(
                        &sample.account_id,
                        account.incarnation,
                        &sample.window,
                        utilization,
                        sample.reset_at,
                        carrier
                            .as_ref()
                            .map(|(id, attempt)| (id.as_str(), *attempt)),
                        now,
                    ) {
                        router.report(Report::Fit {
                            row,
                            incarnation: account.incarnation,
                        });
                    }
                }
            }
            Record::Event(_) | Record::Keepalive(_) => {}
        }
    }
}

fn table_of(record: &Record) -> &'static str {
    match record {
        Record::AttemptDropped { .. } => "attempts_dropped",
        Record::AttemptStarted { .. } => "attempts_started",
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

/// One transaction per batch on a blocking thread. A batch that fails is lost as a whole and counted, so the
/// gap that follows names it. Metrics are observed only after the commit succeeds, so a failed batch is
/// reflected in `phoebis_ledger_dropped_total`, never in the request or sample series.
async fn land(
    db: &Arc<LedgerDb>,
    pricing: &Arc<Pricing>,
    stats: &Arc<LedgerStats>,
    metrics: &Arc<Metrics>,
    batch: Vec<Record>,
) -> bool {
    let db = db.clone();
    let pricing = pricing.clone();
    let size = batch.len() as u64;
    let tables: Vec<&'static str> = batch.iter().map(table_of).collect();
    let written = tokio::task::spawn_blocking(move || {
        let mut observations = Vec::new();
        let result = db.write(|conn| {
            let tx = conn.transaction()?;
            for record in &batch {
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
                    Record::Samples { quota, extra, .. } => {
                        insert_samples_in(&tx, quota, extra.as_ref())?;
                        observations.push(Observation::Samples {
                            quota: quota.clone(),
                        });
                    }
                    // The fitter's alone: the writer walked them through it before this batch.
                    Record::AttemptStarted { .. } | Record::AttemptDropped { .. } => {}
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
            true
        }
        Ok((Err(error), _)) => {
            tracing::error!(%error, records = size, "a ledger batch could not be written");
            stats.write_failures.fetch_add(1, Ordering::Relaxed);
            stats.dropped.fetch_add(size, Ordering::Relaxed);
            for table in &tables {
                metrics.observe_drop(table);
            }
            false
        }
        Err(error) => {
            tracing::error!(%error, records = size, "the ledger writer panicked; the batch is lost");
            stats.write_failures.fetch_add(1, Ordering::Relaxed);
            stats.dropped.fetch_add(size, Ordering::Relaxed);
            for table in &tables {
                metrics.observe_drop(table);
            }
            false
        }
    }
}

async fn run_purge(db: &Arc<LedgerDb>, config: &LedgerConfig, router: &RouterHandle, now: i64) {
    let (db, config) = (db.clone(), config.clone());
    let budgeted = budgeted_members(&router.snapshot());
    match tokio::task::spawn_blocking(move || run_retention(&db, &config, now, &budgeted)).await {
        Ok(Ok(purged)) => tracing::debug!(?purged, "ledger retention ran"),
        Ok(Err(error)) => tracing::warn!(%error, "ledger retention failed"),
        Err(error) => tracing::error!(%error, "ledger retention panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn attempt_records_wait_for_room_in_the_channel_rather_than_drop() {
        let (ledger, mut rx) = stalled();
        ledger.record(Record::Event(EventRecord::default()));
        let attempt = |started: bool| {
            let (account_id, request_id, attempt, at) = ("a".to_string(), "r1".to_string(), 1, 5);
            if started {
                Record::AttemptStarted {
                    account_id,
                    request_id,
                    attempt,
                    at,
                }
            } else {
                Record::AttemptDropped {
                    account_id,
                    request_id,
                    attempt,
                    at,
                }
            }
        };
        let landing = tokio::spawn({
            let ledger = ledger.clone();
            let records = vec![attempt(true), attempt(false)];
            async move { land_all(&ledger, records).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ledger.dropped(), 0, "neither record was dropped");
        assert!(
            !landing.is_finished(),
            "the landing waits while the channel is full"
        );
        assert!(matches!(rx.recv().await, Some(Record::Event(_))));
        assert!(matches!(
            rx.recv().await,
            Some(Record::AttemptStarted { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(Record::AttemptDropped { .. })
        ));
        landing.await.unwrap();
        assert_eq!(ledger.dropped(), 0);
    }

    /// A store carrying one pool, one account, and one budgeted member, and the router spawned over it: enough
    /// for `refresh_member_spends` to find a member to read.
    fn budgeted_router() -> RouterHandle {
        let store = Arc::new(Store::open_in_memory(&phoebis_store::StorageKey::generate()).unwrap());
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
                    cache_diagnostics: None,
                    affinity: phoebis_store::Affinity::default(),
                    quota_headers: phoebis_store::QuotaHeaders::Pass,
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
                    budget_usd: Some(10.0),
                    budget_period: BudgetPeriod::Month,
                }],
                "test",
            )
            .unwrap();
        let clock: Clock = Arc::new(|| 1_000);
        phoebis_routing::spawn(store, None, clock, phoebis_routing::RouterConfig::default()).unwrap()
    }

    #[tokio::test]
    async fn a_failed_spend_refresh_keeps_the_previous_boundary_armed() {
        let router = budgeted_router();
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let pricing = Arc::new(Pricing::new(PriceTable::shipped()));
        let event_poll = Duration::from_secs(5);

        let due = refresh_member_spends(&db, &pricing, &router, 1_000, None, event_poll).await;
        assert!(due.is_some(), "a working read arms the boundary tick");

        db.write(|c| {
            c.execute("DROP TABLE rollups_hourly", [])?;
            Ok(())
        })
        .unwrap();

        let after_failure =
            refresh_member_spends(&db, &pricing, &router, 2_000, due, event_poll).await;
        assert_eq!(
            after_failure, due,
            "a failed read keeps the previous boundary armed rather than disarming it"
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
            refresh_member_spends(&db, &pricing, &router, now, past_due, event_poll).await;
        assert_eq!(
            after_failure,
            Some(now + event_poll.as_millis() as i64),
            "a past deadline retries at the poll cadence rather than at once"
        );
    }
}
