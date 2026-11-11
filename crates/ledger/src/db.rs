//! The ledger's SQLite file: one writer connection the ledger task owns through `write` and reads its own figures on
//! through `writer_read`, one connection the retention purge owns through `purge`, and a pool of `READERS` read-only
//! connections the admin routes, the attention reader, the metrics scrape and `/v1/usage` share through `read` and
//! `read_blocking`. WAL lets a read run beside either writer, and the two take SQLite's one write lock in turn, each
//! waiting for the other's transaction under the busy timeout.

use std::collections::HashMap;
use std::iter::once;
use std::path::Path;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use phoebis_store::migrations::{self, VersionError};
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Statement, ToSql, Transaction,
    TransactionBehavior,
};

use crate::pricing::PriceRevisions;
use crate::query::{bind_all, request_by_id, RequestWithAttempts};
use crate::record::{
    AttemptRecord, BodyRecord, CacheMapWrite, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    GuardrailMatchRecord, QuotaSampleRecord, RequestRecord, SampleSource, ServingFacts,
};
use crate::{schema, LedgerError};

/// How long a writing connection waits for the write lock while the other holds it. A batch holds it for
/// milliseconds, and a purge slice, bounded in rows and for bodies in bytes, holds it for a fraction of this, so a
/// write waits behind one slice at most. The registry waits as long.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The writer connection's cache of prepared statements. One batch runs about twenty cached statements (a request's
/// landing, the samples, the events, the decisions, the cache map and the rollup fold), so rusqlite's default of 16
/// would evict and recompile some on every batch. The parked rows' insert and update run apart from a batch, when a
/// landing fails and when the start or a replay tries the parked rows. 64 holds them all with room, as the registry's
/// does.
const STATEMENT_CACHE_CAPACITY: usize = 64;

/// How many read-only connections serve the attention reader, the console, the metrics scrape and `/v1/usage` at
/// once. Four covers the five routes a Pools page polls on one tick with the sessions read still in flight, and bounds
/// the readers' page caches at 128 MiB.
pub const READERS: usize = 4;

/// Each pooled reader's page cache in KiB, 32 MiB, set through `cache_size`'s negative form. A day of rollups or a
/// session walk stays in the connection between two polls, and four such caches stay inside what a router host
/// spares. No `mmap_size`: the operating system's page cache already serves the warm reads, and a map of a ledger file
/// tens of gigabytes large buys nothing for cold random reads.
const READER_CACHE_KIB: i64 = 32 * 1024;

/// The rows an `ANALYZE` visits per index before it skips ahead (SQLite's approximate analysis,
/// <https://www.sqlite.org/lang_analyze.html> section 5), so the start of a large file scans a bounded slice of each
/// index rather than the file: the planner needs the ratio between two indexes, which the sample gets right, and the
/// statements whose plan must hold name their index. Crate-visible, so the seeded plan pass analyzes under the limit
/// the open uses.
pub(crate) const ANALYSIS_LIMIT: i64 = 1_000;

/// The open's `PRAGMA optimize` mask (<https://www.sqlite.org/pragma.html#pragma_optimize>, the mask table):
/// `0x10000` examines every table, and `0x2` runs `ANALYZE` where a table has no statistics or outgrew them. The
/// explicit `ANALYSIS_LIMIT` bounds the pragma's `ANALYZE`, and the `0x10` bit is kept as the default mask keeps it.
const OPEN_OPTIMIZE_MASK: i64 = 0x10012;

pub struct LedgerDb {
    writer: Mutex<Connection>,
    /// `None` for an in-memory ledger, whose one connection serves every role. In an `Arc`, so `read` hands its
    /// blocking worker the pool it matched.
    readers: Option<Arc<Readers>>,
    /// The retention purge's own connection, so a slice never holds the writer's lock or turn; `None` in memory.
    purger: Option<Mutex<Connection>>,
    /// The writes begun and ended through `write`, which `purge` reads to let a waiting write go first.
    writes: Writes,
    #[cfg(any(test, feature = "debug-hooks"))]
    fail_next_writer_read: Mutex<Option<WriterRead>>,
    #[cfg(any(test, feature = "debug-hooks"))]
    hold_next_writer_read: Mutex<Option<(WriterRead, Arc<phoebis_store::HoldGate>)>>,
    #[cfg(any(test, feature = "debug-hooks"))]
    page_reads: AtomicUsize,
}

/// The writer's own reads, named so a test can hold or fail one of them and no other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterRead {
    AnnouncedCrossings,
    MemberSpends,
    ParkedCount,
    ParkedRows,
}

/// One `parked_records` row as the start reads it: the row's id and the serialized `ParkedRecord`.
pub(crate) struct ParkedRow {
    pub id: i64,
    pub record: String,
}

/// SQLite's busy handler retries a waiting write on a backoff that grows to 100 ms, while back-to-back slices free
/// the lock for microseconds between them: a write left to the handler alone waits out most of a pass, and past the
/// busy timeout its batch fails into the writer's retries. So a slice starts only once every write begun before it
/// has ended, and a batch waits one slice at most. The handoff works in one direction: after its wait the purge's
/// `BEGIN IMMEDIATE` competes with the next batch under SQLite's busy backoff, so a writer landing batches back to
/// back can push a slice past the 5 s busy timeout, and the pass ends logged until the next hour. Two slices give way
/// to a write that began after their handoff: a bodies slice between two bodies, and a backfill slice between two rows.
#[derive(Default)]
struct Writes {
    begun: AtomicU64,
    ended: AtomicU64,
}

/// Counts its write as ended when dropped, so a closure that panicked still lets the purge go on.
struct Ended<'a>(&'a AtomicU64);

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// The pool: the idle connections as a stack, so the warmest cache serves the next read, a `Condvar` for a caller
/// that waits on its own thread and a `Notify` for one that waits on the runtime, both woken once per return. A read
/// holds its connection for its closure's span alone, every return wakes one blocking waiter and one async waiter,
/// and `Notify` wakes its waiters in order, so no caller starves under a steady 5 s poll.
struct Readers {
    /// Recovered from poisoning, since a push or a pop leaves the stack whole whatever panicked.
    idle: Mutex<Vec<Reader>>,
    returned: Condvar,
    wakeup: tokio::sync::Notify,
    /// The ledger file, which a reader behind `generation` reopens.
    path: std::path::PathBuf,
    /// The statistics generation the readers follow, moved by `refresh_statistics`.
    generation: AtomicU64,
}

struct Reader {
    conn: Connection,
    /// The pool's generation when this connection was opened.
    generation: u64,
}

impl Readers {
    /// `READERS` connections through `open_reader`.
    fn open(path: &Path) -> Result<Readers, LedgerError> {
        let generation = 0;
        let idle = (0..READERS)
            .map(|_| {
                Ok(Reader {
                    conn: open_reader(path)?,
                    generation,
                })
            })
            .collect::<Result<_, LedgerError>>()?;
        Ok(Readers {
            idle: Mutex::new(idle),
            returned: Condvar::new(),
            wakeup: tokio::sync::Notify::new(),
            path: path.to_path_buf(),
            generation: AtomicU64::new(generation),
        })
    }

    /// The newest idle reader, waiting on `wakeup` while none is idle. The waiter is registered before the stack is
    /// looked at, so a return between the look and the wait wakes this waiter and two returns wake two waiters.
    /// Dropping the future while it waits leaves nothing behind, since the waiter holds no reader until it returns.
    async fn take(&self) -> Reader {
        loop {
            let mut notified = std::pin::pin!(self.wakeup.notified());
            notified.as_mut().enable();
            let popped = recovered(&self.idle).pop();
            if let Some(reader) = popped {
                return reader;
            }
            notified.await;
        }
    }

    /// The same on the calling thread, waiting on `returned`.
    fn take_blocking(&self) -> Reader {
        let mut idle = recovered(&self.idle);
        loop {
            if let Some(reader) = idle.pop() {
                return reader;
            }
            idle = self
                .returned
                .wait(idle)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Pushes the reader back and wakes one waiter of each kind. A woken waiter that finds the stack empty, because a
    /// third caller popped first, registers again and waits on.
    fn put_back(&self, reader: Reader) {
        recovered(&self.idle).push(reader);
        self.returned.notify_one();
        self.wakeup.notify_one();
    }
}

/// The guard whether or not a panic poisoned the mutex, for state that a panic leaves whole.
fn recovered<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One pooled reader: read-only, no mutex, `BUSY_TIMEOUT`, `cache_size` at `-READER_CACHE_KIB`.
fn open_reader(path: &Path) -> Result<Connection, LedgerError> {
    let conn = phoebis_store::open_read_only(path, BUSY_TIMEOUT)?;
    conn.pragma_update(None, "cache_size", -READER_CACHE_KIB)?;
    Ok(conn)
}

/// Puts its reader back when dropped, a panic's unwind included, so a closure that panics mid-row leaves the pool
/// whole and the connection as consistent as its rolled-back read transaction left it.
struct Returned<'a> {
    reader: Option<Reader>,
    pool: &'a Readers,
}

impl Drop for Returned<'_> {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            self.pool.put_back(reader);
        }
    }
}

#[cfg(any(test, feature = "debug-hooks"))]
const INJECTED_READ_FAILURE: &str = "injected ledger read failure";

impl LedgerDb {
    pub fn open(path: &Path) -> Result<LedgerDb, LedgerError> {
        let dir = phoebis_store::permissions::data_dir_of(path);
        phoebis_store::permissions::create_private_dir(dir)?;
        if !path.exists() {
            // Captured bodies are prompts, so the file is private from its creation.
            phoebis_store::permissions::create_private_if_missing(path)?;
        }
        let mut writer = Connection::open(path)?;
        writer.busy_timeout(BUSY_TIMEOUT)?;
        writer.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        writer.pragma_update(None, "journal_mode", "WAL")?;
        // A power loss may lose the last transactions before a checkpoint, which the spec accepts for a
        // best-effort ledger; `FULL` would fsync on every request.
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        prepare(&mut writer, path, schema::MIGRATIONS)?;
        // The journal mode is the file's, which the writer set; `synchronous` is the connection's own.
        let purger = Connection::open(path)?;
        purger.busy_timeout(BUSY_TIMEOUT)?;
        purger.pragma_update(None, "synchronous", "NORMAL")?;
        // Before the readers open, since a connection loads `sqlite_stat1` with its schema and keeps what it loaded.
        analyze_at_open(&purger)?;
        let readers = Readers::open(path)?;
        Ok(LedgerDb::from_parts(writer, Some(readers), Some(purger)))
    }

    pub fn open_in_memory() -> Result<LedgerDb, LedgerError> {
        let mut writer = Connection::open_in_memory()?;
        writer.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        prepare(&mut writer, Path::new(":memory:"), schema::MIGRATIONS)?;
        analyze_at_open(&writer)?;
        Ok(LedgerDb::from_parts(writer, None, None))
    }

    fn from_parts(
        writer: Connection,
        readers: Option<Readers>,
        purger: Option<Connection>,
    ) -> LedgerDb {
        LedgerDb {
            writer: Mutex::new(writer),
            readers: readers.map(Arc::new),
            purger: purger.map(Mutex::new),
            writes: Writes::default(),
            #[cfg(any(test, feature = "debug-hooks"))]
            fail_next_writer_read: Mutex::new(None),
            #[cfg(any(test, feature = "debug-hooks"))]
            hold_next_writer_read: Mutex::new(None),
            #[cfg(any(test, feature = "debug-hooks"))]
            page_reads: AtomicUsize::new(0),
        }
    }

    pub async fn schema_version(self: &Arc<Self>) -> Result<u32, LedgerError> {
        self.read(read_version).await
    }

    /// The ledger's `meta.schema_version` through a read-only connection, taking no lock and creating nothing;
    /// `None` for an absent file or one without the table, which the next start creates or migrates.
    pub fn read_schema_version(path: &Path) -> Result<Option<u32>, LedgerError> {
        migrations::file_version(path, VERSION_WITHOUT_ROW, BUSY_TIMEOUT)
            .map_err(version_error(path))
    }

    /// Refuses a ledger a newer build wrote with the start's own refusal, through a read-only connection taking no
    /// lock and creating nothing. An absent file passes, since the next start creates it.
    pub fn check_readable(path: &Path) -> Result<(), LedgerError> {
        migrations::check_file(path, VERSION_WITHOUT_ROW, latest_version(), BUSY_TIMEOUT)
            .map_err(version_error(path))
    }

    /// The newest schema this build reads, which `open` refuses a file past.
    pub fn supported_schema_version() -> u32 {
        latest_version()
    }

    pub fn write<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        self.writes.begun.fetch_add(1, Ordering::SeqCst);
        let _ended = Ended(&self.writes.ended);
        // A panic inside a closure rolls its transaction back when the guard drops, so the connection is
        // consistent and the next batch lands; refusing every later write for one bug would stall the ledger for
        // good.
        let mut conn = recovered(&self.writer);
        f(&mut conn)
    }

    /// Runs `f` under the purger connection (the writer's in memory). A poisoned guard is recovered as `write`
    /// recovers its own: a rolled-back transaction leaves the connection consistent.
    pub fn purge<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        self.purge_yielding(|conn, _| f(conn))
    }

    /// `purge` with the handoff's reading passed in: `waiting()` answers whether a write began through `write` past
    /// the count the handoff waited on, so a write that began during the wait, which the wait did not cover, counts
    /// as waiting too. A slice that reads it between two rows can commit and let that write go first.
    pub fn purge_yielding<T>(
        &self,
        f: impl FnOnce(&mut Connection, &dyn Fn() -> bool) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        // A write that met the previous slice's lock goes first, as `Writes` says.
        let begun = self.writes.begun.load(Ordering::SeqCst);
        while self.writes.ended.load(Ordering::SeqCst) < begun {
            std::thread::sleep(Duration::from_millis(1));
        }
        let waiting = || self.writes.begun.load(Ordering::SeqCst) > begun;
        let purger = self.purger.as_ref().unwrap_or(&self.writer);
        let mut conn = recovered(purger);
        f(&mut conn, &waiting)
    }

    /// Runs `PRAGMA optimize` on the purger connection under the purge handoff, answers whether `sqlite_stat1` changed,
    /// and moves the pooled readers' statistics generation when it did, so a reader plans on the new figures at its
    /// next read. A re-analysis moves no schema cookie, so the readers would otherwise keep their open-time statistics
    /// until the next restart. It runs at the end of every purge, so the statistics' cadence is the purge's interval
    /// and no constant of its own.
    pub fn refresh_statistics(&self) -> Result<bool, LedgerError> {
        let changed = self.purge(|conn| {
            let before = stat1_rows(conn)?;
            conn.execute_batch("PRAGMA optimize")?;
            Ok(stat1_rows(conn)? != before)
        })?;
        if changed {
            self.bump_statistics_generation();
        }
        Ok(changed)
    }

    fn bump_statistics_generation(&self) {
        if let Some(readers) = &self.readers {
            readers.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Runs `f` on a blocking worker under a pooled reader connection, so a slow read never parks the runtime thread
    /// that serves proxy traffic. A read waits only while every pooled connection is out, and it waits before its
    /// worker spawns, so a waiting read holds no blocking worker. In memory it runs on the one connection, behind the
    /// writer's mutex.
    pub async fn read<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError> + Send + 'static,
    ) -> Result<T, LedgerError> {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.page_reads.fetch_add(1, Ordering::SeqCst);
        let task = match &self.readers {
            Some(readers) => {
                let reader = readers.take().await;
                let pool = Arc::clone(readers);
                tokio::task::spawn_blocking(move || LedgerDb::lend(&pool, reader, f))
            }
            None => {
                let db = Arc::clone(self);
                tokio::task::spawn_blocking(move || db.read_blocking(f))
            }
        };
        task.await.map_err(|_| LedgerError::ReadCanceled)?
    }

    /// The same read on the calling thread, for the event mirror's one read as the ledger task starts and for tests
    /// that hold no runtime worker to spare. It waits only while every pooled connection is out. Under the runtime
    /// every wait on the pool is `read`'s, and no blocking worker of the runtime waits on the pool here (`read`'s
    /// in-memory arm calls this on a worker, where it takes the writer's mutex and never the pool): a worker waiting
    /// on the pool can be the worker a queued `read` closure needs to return its connection, and with every worker
    /// waiting the pool never gets one back.
    pub fn read_blocking<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        // A read opens no transaction but a read one, such as `list_request_page`'s, which rolls back as it drops
        // while the panic unwinds, so a closure that panics mid-row leaves the connection as consistent as it was
        // before the call. Returning the reader through `Returned`, and recovering the writer's guard in memory,
        // rather than propagating the poisoning, keeps one panicked read from turning every later read into a
        // `ReadCanceled`.
        match &self.readers {
            Some(readers) => LedgerDb::lend(readers, readers.take_blocking(), f),
            None => f(&recovered(&self.writer)),
        }
    }

    /// Runs `f` on a pooled reader under `Returned`, so the reader goes back to the pool however `f` ends.
    ///
    /// A reader behind the pool's statistics generation is reopened through `open_reader` first, since the bundled
    /// SQLite reloads statistics on the analyzing connection alone and a connection reads `sqlite_stat1` again only
    /// when it loads its schema. The reopen runs here, on the blocking worker or the caller's thread and after the
    /// stack's lock is released, so on the `read` path a file open never holds the runtime, and on either path it
    /// never holds another taker. The generation is read before the open, so a refresh that lands during it leaves
    /// this reader behind and its next read reopens it again. A reopen that fails puts the old reader back and answers
    /// the error, so the pool never shrinks and the next read tries again.
    fn lend<T>(
        pool: &Readers,
        reader: Reader,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        let mut lent = Returned { reader: None, pool };
        let reader = lent.reader.insert(reader);
        let generation = pool.generation.load(Ordering::SeqCst);
        if reader.generation < generation {
            reader.conn = open_reader(&pool.path)?;
            reader.generation = generation;
        }
        f(&reader.conn)
    }

    /// Runs `f` on the writer connection on a blocking worker. The writer loop awaits it between two batches, so it
    /// reads every batch the writer committed and never queues behind a console read. It counts no write, so the purge
    /// handoff of `purge` waits for no read.
    #[cfg_attr(not(any(test, feature = "debug-hooks")), allow(unused_variables))]
    pub async fn writer_read<T: Send + 'static>(
        self: &Arc<Self>,
        which: WriterRead,
        f: impl FnOnce(&Connection) -> Result<T, LedgerError> + Send + 'static,
    ) -> Result<T, LedgerError> {
        let db = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            // Recovered as `write` recovers it, for the same reason: a closure that panicked rolled its transaction
            // back, so the connection is consistent.
            let conn = recovered(&db.writer);
            #[cfg(any(test, feature = "debug-hooks"))]
            db.writer_read_hooks(which)?;
            f(&conn)
        })
        .await
        .map_err(|_| LedgerError::ReadCanceled)?
    }

    #[cfg(any(test, feature = "debug-hooks"))]
    fn writer_read_hooks(&self, which: WriterRead) -> Result<(), LedgerError> {
        let failing = recovered(&self.fail_next_writer_read).take_if(|armed| *armed == which);
        if failing.is_some() {
            return Err(LedgerError::Io(std::io::Error::other(
                INJECTED_READ_FAILURE,
            )));
        }
        let held = recovered(&self.hold_next_writer_read).take_if(|(armed, _)| *armed == which);
        if let Some((_, gate)) = held {
            gate.park();
        }
        Ok(())
    }

    /// `insert_request_at` with `PriceRevisions::shipped()`. The production landing goes through `insert_request_in`
    /// inside the writer with the list it was started on, so this entry serves the test suites, and a test that
    /// prices on a list of its own lands through `insert_request_at`. Built for tests alone, since a production
    /// caller would band a request at the shipped thresholds whatever file the operator loaded.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_request(
        &self,
        request: &RequestRecord,
        attempts: &[AttemptRecord],
        bodies: Option<&BodyRecord>,
    ) -> Result<(), LedgerError> {
        self.insert_request_at(request, attempts, bodies, &PriceRevisions::shipped())
    }

    /// Writes the request and its attempts and folds them into the rollups in one transaction, each answered attempt
    /// under the prompt-length band `prices` selects at the revision in force at the request's `received_at`. The
    /// handle holds no clock, so the row's `landed_at` is one past the newest the ledger holds.
    pub fn insert_request_at(
        &self,
        request: &RequestRecord,
        attempts: &[AttemptRecord],
        bodies: Option<&BodyRecord>,
        prices: &PriceRevisions,
    ) -> Result<(), LedgerError> {
        self.write(|conn| {
            let tx = begin_write(conn)?;
            let previous = insert_request_in(&tx, request, attempts, bodies, 0)?;
            crate::rollup::fold_request(
                &tx,
                request,
                attempts,
                previous.as_ref().map(|(r, a)| (r, a.as_slice())),
                prices,
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Fails the next `writer_read` that names `which` with an I/O error whose text is `INJECTED_READ_FAILURE`.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_fail_next_writer_read(&self, which: WriterRead) {
        *recovered(&self.fail_next_writer_read) = Some(which);
    }

    /// Parks the next `writer_read` that names `which` on its blocking worker, with the writer connection taken,
    /// until the hold is released or dropped. It parks there because the tests that use it keep the writer short of a
    /// commit while another connection reads, which is what a writer-side read does in production too.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_hold_next_writer_read(&self, which: WriterRead) -> phoebis_store::Hold {
        let gate = Arc::new(phoebis_store::HoldGate::default());
        *recovered(&self.hold_next_writer_read) = Some((which, gate.clone()));
        phoebis_store::Hold::new(gate)
    }

    /// The `read` calls this handle has run since it opened. A paged walk such as the guardrail dry run's takes one
    /// per page, so a test pins the page size the walk reads by the count across it.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_page_reads(&self) -> usize {
        self.page_reads.load(Ordering::SeqCst)
    }

    /// The pooled readers' statistics generation. 0 on an in-memory ledger, since the generation lives on the pool,
    /// which an in-memory ledger lacks.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_statistics_generation(&self) -> u64 {
        self.readers
            .as_ref()
            .map_or(0, |readers| readers.generation.load(Ordering::SeqCst))
    }

    /// Moves the pooled readers' statistics generation as a refresh that changed `sqlite_stat1` moves it, so statistics
    /// a test wrote by hand reach each reader at its next read. Does nothing on an in-memory ledger, since the
    /// generation lives on the pool, which an in-memory ledger lacks.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_bump_statistics_generation(&self) {
        self.bump_statistics_generation();
    }

    /// Caps the writer connection's file at the pages it holds once a vacuum has left none free, so the next insert
    /// that needs a page meets `SQLITE_FULL` and the writer holds its batch under retry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fill_the_disk(&self) {
        self.write(|c| {
            c.execute_batch("VACUUM;")?;
            let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            c.pragma_update(None, "max_page_count", pages)?;
            Ok(())
        })
        .expect("the test's cap on the ledger file applies");
    }

    /// Lifts the cap, so the held batch lands on its next retry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn free_the_disk(&self) {
        self.write(|c| {
            c.pragma_update(None, "max_page_count", 1_073_741_823)?;
            Ok(())
        })
        .expect("the test's cap on the ledger file lifts");
    }

    /// Parks one priced record the writer could not land, `parked_at` and `last_tried_at` both `at`.
    pub(crate) fn insert_parked(
        &self,
        at: i64,
        request_id: &str,
        pool_id: Option<&str>,
        account_id: Option<&str>,
        record: &str,
        error: &str,
    ) -> Result<(), LedgerError> {
        self.write(|conn| {
            conn.prepare_cached(
                "INSERT INTO parked_records (parked_at, last_tried_at, request_id, pool_id, account_id, record, error) VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![at, request_id, pool_id, account_id, record, error])?;
            Ok(())
        })
    }

    /// Records the failure a parked row met at this start's try.
    pub(crate) fn update_parked(&self, id: i64, error: &str, at: i64) -> Result<(), LedgerError> {
        self.write(|conn| {
            conn.prepare_cached(
                "UPDATE parked_records SET error = ?2, last_tried_at = ?3 WHERE id = ?1",
            )?
            .execute(params![id, error, at])?;
            Ok(())
        })
    }

    pub fn insert_event(&self, event: &EventRecord) -> Result<(), LedgerError> {
        self.write(|conn| {
            insert_event_in(conn, event)?;
            Ok(())
        })
    }

    pub async fn request(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<Option<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
        let id = id.to_string();
        self.read(move |conn| request_by_id(conn, &id)).await
    }

    /// The captured bodies of a request whose expiry is after `now`, `None` past it as for a purged row, since the
    /// purge deletes an expired row on its next pass only.
    pub async fn bodies(
        self: &Arc<Self>,
        id: &str,
        now: i64,
    ) -> Result<Option<BodyRecord>, LedgerError> {
        let id = id.to_string();
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT request_id, request_body, response_body, request_len_seen, response_len_seen, expires_at FROM bodies WHERE request_id = ?1 AND expires_at > ?2",
                    params![id, now],
                    |r| Ok(BodyRecord {
                        request_id: r.get(0)?,
                        request_body: r.get(1)?,
                        response_body: r.get(2)?,
                        request_len_seen: r.get::<_, i64>(3)? as u64,
                        response_len_seen: r.get::<_, i64>(4)? as u64,
                        expires_at: r.get(5)?,
                    }),
                )
                .optional()?)
        }).await
    }

    pub async fn events(
        self: &Arc<Self>,
        from: i64,
        to: i64,
        limit: usize,
    ) -> Result<Vec<EventRecord>, LedgerError> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(concat!(
                event_select!(),
                " WHERE at >= ?1 AND at < ?2 ORDER BY at, rowid LIMIT ?3"
            ))?;
            let rows = stmt.query_map(params![from, to, limit as i64], row_to_event)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub fn insert_samples(
        &self,
        quota: &[QuotaSampleRecord],
        extra: Option<&ExtraUsageSampleRecord>,
    ) -> Result<(), LedgerError> {
        self.write(|conn| insert_samples_in(conn, quota, extra))
    }

    pub async fn quota_samples(
        self: &Arc<Self>,
        account_id: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<QuotaSampleRecord>, LedgerError> {
        let account_id = account_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn.prepare("SELECT account_id, bucket, window, utilization, reset_at, source, observed_at FROM quota_samples WHERE account_id = ?1 AND bucket >= ?2 AND bucket < ?3 ORDER BY window, bucket")?;
            let rows = stmt.query_map(params![account_id, from, to], row_to_quota_sample)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }

    pub async fn extra_usage_samples(
        self: &Arc<Self>,
        account_id: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<ExtraUsageSampleRecord>, LedgerError> {
        let account_id = account_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn.prepare("SELECT account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, disabled_reason, overage_reset, source, spend_limit_minor, spend_used_minor, spend_percent, spend_currency, spend_exponent FROM extra_usage_samples WHERE account_id = ?1 AND bucket >= ?2 AND bucket < ?3 ORDER BY bucket, source")?;
            let rows = stmt.query_map(params![account_id, from, to], row_to_extra_usage_sample)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }
}

/// The newest `at` among the requests, the events and the decisions: when the router that wrote them last did
/// anything, which is all a start can date its gap from, since nothing marks a clean stop.
pub(crate) fn newest_row_at(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(at) FROM (SELECT MAX(received_at) AS at FROM requests UNION ALL SELECT MAX(at) FROM events \
         UNION ALL SELECT MAX(at) FROM decisions)",
        [],
        |row| row.get(0),
    )
}

/// A transaction that takes SQLite's write lock at `BEGIN`, where the busy timeout applies. A deferred one that
/// reads before it writes is refused at once with `SQLITE_BUSY` while the other writing connection holds the lock,
/// or once that one committed past its snapshot, and its batch would be lost rather than wait.
pub(crate) fn begin_write(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

/// Deletes a parked row inside the transaction that lands its record, so the two commit or roll back together: a row
/// deleted on its own after the commit could outlive a landing the file then loses, and the next start would land the
/// record again.
pub(crate) fn delete_parked_in(tx: &Transaction<'_>, id: i64) -> Result<(), LedgerError> {
    tx.prepare_cached("DELETE FROM parked_records WHERE id = ?1")?
        .execute(params![id])?;
    Ok(())
}

/// Every parked row in the order it was parked, read through `writer_read` on the writer's connection, which the
/// start runs on.
pub(crate) fn parked_rows(conn: &Connection) -> Result<Vec<ParkedRow>, LedgerError> {
    let mut stmt = conn.prepare("SELECT id, record FROM parked_records ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ParkedRow {
                id: r.get(0)?,
                record: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

macro_rules! quota_sample_insert {
    ($verb:literal) => {
        concat!(
            $verb,
            " INTO quota_samples (account_id, bucket, window, utilization, reset_at, source, observed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        )
    };
}

macro_rules! extra_usage_sample_insert {
    ($verb:literal, $tail:literal) => {
        concat!(
            $verb,
            " INTO extra_usage_samples (account_id, bucket, is_enabled, in_use, monthly_limit, used_credits, utilization, currency, disabled_reason, overage_reset, source, spend_limit_minor, spend_used_minor, spend_percent, spend_currency, spend_exponent) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            $tail
        )
    };
}

pub(crate) fn insert_samples_in(
    conn: &Connection,
    quota: &[QuotaSampleRecord],
    extra: Option<&ExtraUsageSampleRecord>,
) -> Result<(), LedgerError> {
    for q in quota {
        // A head sample is the newest truth for its bucket, and a poll only fills a bucket nothing else wrote.
        let sql = match q.source {
            SampleSource::Head => quota_sample_insert!("INSERT OR REPLACE"),
            SampleSource::Poll => quota_sample_insert!("INSERT OR IGNORE"),
        };
        conn.prepare_cached(sql)?.execute(params![
            q.account_id,
            q.bucket,
            q.window,
            q.utilization,
            q.reset_at,
            q.source,
            q.observed_at
        ])?;
    }
    if let Some(e) = extra {
        // Keyed by the source too, so a head updates the bucket's head row and the bucket's first poll keeps its row:
        // neither hides the other. A head that names the share or the reset alone keeps the other one the row holds,
        // since each header is its own reading: the row carries the bucket's last share and last reset.
        let sql = match e.source {
            SampleSource::Head => extra_usage_sample_insert!(
                "INSERT",
                " ON CONFLICT (account_id, bucket, source) DO UPDATE SET is_enabled = excluded.is_enabled, in_use = excluded.in_use, utilization = COALESCE(excluded.utilization, utilization), disabled_reason = excluded.disabled_reason, overage_reset = COALESCE(excluded.overage_reset, overage_reset)"
            ),
            SampleSource::Poll => extra_usage_sample_insert!("INSERT OR IGNORE", ""),
        };
        conn.prepare_cached(sql)?.execute(params![
            e.account_id,
            e.bucket,
            e.is_enabled,
            e.in_use,
            e.monthly_limit,
            e.used_credits,
            e.utilization,
            e.currency,
            e.disabled_reason,
            e.overage_reset,
            e.source,
            e.spend_limit_minor,
            e.spend_used_minor,
            e.spend_percent,
            e.spend_currency,
            e.spend_exponent,
        ])?;
    }
    Ok(())
}

/// The instant the ledger wrote the row, from the writer's clock and strictly increasing across its rows
/// (`max(now, previous + 1)`), so two rows written in one millisecond still order. Null on a row from before the
/// column, which no `landed_after` filter answers. The previous stamp is read in the row's own transaction, so a
/// writer that starts again under a clock set back still stamps above every row written before.
fn landing_stamp(tx: &Transaction<'_>, now: i64) -> Result<i64, LedgerError> {
    let previous: i64 = tx
        .prepare_cached(LANDING_STAMP_SQL)?
        .query_row([], |r| r.get(0))?;
    Ok(now.max(previous + 1))
}

/// The writer's landing stamp read, a `MAX` the landed-at index answers from its last entry.
pub(crate) const LANDING_STAMP_SQL: &str = "SELECT COALESCE(MAX(landed_at), 0) FROM requests";

/// The serving facts' columns of the request row, in the order `serving_values` binds them, so the landing and the
/// backfill write one list.
const SERVING_COLUMNS: [&str; 11] = [
    "serving_account_id",
    "reached_upstream",
    "outcome_class",
    "speed",
    "cache_miss_reason",
    "stop_reason",
    "refusal_category",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
];

/// `facts` as the binds of `SERVING_COLUMNS`, in its order.
fn serving_values(facts: &ServingFacts) -> [Value; 11] {
    let tokens = |n: Option<u64>| Value::from(n.map(|n| n as i64));
    [
        facts.account_id.clone().into(),
        facts.reached_upstream.into(),
        facts.outcome_class.clone().into(),
        facts.speed.clone().into(),
        facts.cache_miss_reason.clone().into(),
        facts.stop_reason.clone().into(),
        facts.refusal_category.clone().into(),
        tokens(facts.input_tokens),
        tokens(facts.output_tokens),
        tokens(facts.cache_read_tokens),
        tokens(facts.cache_write_tokens),
    ]
}

/// The landing's insert, the serving facts' columns last at `?61` to `?71`.
static INSERT_REQUEST: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT OR REPLACE INTO requests (id, received_at, finished_at, key_id, key_generation, pool_id, session_id, agent_id, parent_agent_id, model, canonical_model, stream, kind, origin, status, outcome, attempts, request_bytes, response_bytes, ttfb_ms, ttfc_ms, duration_ms, error_code, client_drift, kept_warm, bodies_captured, user_agent, effort, thinking_type, thinking_budget, max_tokens, tool_count, system_blocks, message_count, context_edits, temperature, requested_speed, requested_service_tier, presented_retired, rotation_at, response_truncated, client, agent_type, compaction, context_compacted, prev_tool_ms, turn_origin, request_class, identified_by, identity_root, identity_depth, forked_from, fork_depth, auto_mode, held, stream_end_at, diagnostics_source, guardrail_id, classifier, client_contract, landed_at, refused, fallback_digest, {}) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48, NULL, NULL, NULL, ?49, ?50, ?51, ?52, ?53, ?54, ?55, ?56, ?57, ?58, ?59, ?60, ?61, ?62, ?63, ?64, ?65, ?66, ?67, ?68, ?69, ?70, ?71)",
        SERVING_COLUMNS.join(", ")
    )
});

/// Writes the request and its attempts, answering the rows the id held before, if any: a replacement has to
/// take its earlier contribution out of every aggregate that was folded from it. `now` is the writer's clock, which
/// the row's `landed_at` is stamped from. The row's serving facts are derived here from the attempts it writes, so
/// every landing path writes them through one rule.
pub(crate) fn insert_request_in(
    tx: &Transaction<'_>,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    bodies: Option<&BodyRecord>,
    now: i64,
) -> Result<Option<(RequestRecord, Vec<AttemptRecord>)>, LedgerError> {
    let previous = request_by_id(tx, &request.id)?;
    // A kept-warm credit is recorded at completion while the writer batches, so the mark can be in the table
    // before the request's own row lands here.
    let marked: bool = tx
        .prepare_cached("SELECT 1 FROM kept_warm_marks WHERE request_id = ?1")?
        .query_row(params![request.id], |_| Ok(()))
        .optional()?
        .is_some();
    let kept_warm = request.kept_warm || marked;
    let landed_at = landing_stamp(tx, now)?;
    let serving = ServingFacts::of(request, attempts);
    tx.prepare_cached(&INSERT_REQUEST)?
        .execute(params_from_iter(
            params![
                request.id,
                request.received_at,
                request.finished_at,
                request.key_id,
                request.key_generation,
                request.pool_id,
                request.session_id,
                request.agent_id,
                request.parent_agent_id,
                request.model,
                request.canonical_model,
                request.stream,
                request.kind,
                request.origin,
                request.status.map(i64::from),
                request.outcome,
                request.attempts,
                request.request_bytes.map(|n| n as i64),
                request.response_bytes.map(|n| n as i64),
                request.ttfb_ms,
                request.ttfc_ms,
                request.duration_ms,
                request.error_code,
                request.client_drift,
                kept_warm,
                request.bodies_captured,
                request.user_agent,
                request.effort,
                request.thinking_type,
                request.thinking_budget.map(|n| n as i64),
                request.max_tokens.map(|n| n as i64),
                i64::from(request.tool_count),
                i64::from(request.system_blocks),
                i64::from(request.message_count),
                request.context_edits,
                request.temperature,
                request.requested_speed,
                request.requested_service_tier,
                request.presented_retired,
                request.rotation_at,
                request.response_truncated,
                request.client,
                request.agent_type,
                request.compaction,
                request.context_compacted,
                request.prev_tool_ms,
                request.turn_origin,
                request.request_class,
                request.forked_from,
                request.fork_depth.map(|n| n as i64),
                request.auto_mode,
                request.held,
                request.stream_end_at,
                request.diagnostics_source,
                request.guardrail_id,
                request.classifier,
                request.client_contract,
                landed_at,
                request.refused,
                request.fallback_digest,
            ]
            .iter()
            .copied()
            .chain(serving_values(&serving).iter().map(|v| v as &dyn ToSql)),
        ))?;
    tx.prepare_cached("DELETE FROM attempts WHERE request_id = ?1")?
        .execute(params![request.id])?;
    for a in attempts {
        tx.prepare_cached(
            "INSERT INTO attempts (request_id, attempt, account_id, account_kind, pin_epoch, started_at, finished_at, ttfb_ms, ttfc_ms, upstream_status, upstream_request_id, outcome, cooldown_scope, cooldown_until, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, service_tier, cache_miss_reason, client_drift, ratelimit_5h_utilization, ratelimit_5h_reset, ratelimit_7d_utilization, ratelimit_7d_reset, ratelimit_7d_oi_utilization, ratelimit_7d_oi_reset, overage_in_use, overage_utilization, thinking_tokens, web_search_requests, web_fetch_requests, stop_reason, stop_sequence, served_model, speed, inference_geo, cleared_input_tokens, refusal_category, refusal_recommended_model, selection, upstream_message_id, written_at, tier, usage_based, issued_digest) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47)",
        )?
        .execute(params![
            a.request_id, a.attempt, a.account_id, a.account_kind, a.pin_epoch.map(|n| n as i64), a.started_at, a.finished_at,
            a.ttfb_ms, a.ttfc_ms, a.upstream_status.map(i64::from), a.upstream_request_id, a.outcome, a.cooldown_scope, a.cooldown_until,
            a.input_tokens.map(|n| n as i64), a.output_tokens.map(|n| n as i64), a.cache_read_tokens.map(|n| n as i64),
            a.cache_write_5m_tokens.map(|n| n as i64), a.cache_write_1h_tokens.map(|n| n as i64), a.service_tier, a.cache_miss_reason,
            a.client_drift, a.ratelimit_5h_utilization, a.ratelimit_5h_reset, a.ratelimit_7d_utilization, a.ratelimit_7d_reset,
            a.ratelimit_7d_oi_utilization, a.ratelimit_7d_oi_reset, a.overage_in_use, a.overage_utilization,
            a.thinking_tokens.map(|n| n as i64), a.web_search_requests.map(|n| n as i64), a.web_fetch_requests.map(|n| n as i64),
            a.stop_reason, a.stop_sequence, a.served_model, a.speed, a.inference_geo,
            a.cleared_input_tokens.map(|n| n as i64), a.refusal_category, a.refusal_recommended_model, a.selection,
            a.upstream_message_id, a.written_at, a.tier, a.usage_based, a.issued_digest,
        ])?;
    }
    // A new row with no match has no child row to clear, so it pays no statement. A replaced row keeps the delete
    // so a stale match never survives a re-landing.
    if previous.is_some() || !request.guardrails.is_empty() {
        tx.prepare_cached("DELETE FROM request_guardrails WHERE request_id = ?1")?
            .execute(params![request.id])?;
    }
    for m in &request.guardrails {
        tx.prepare_cached(
            "INSERT INTO request_guardrails (request_id, guardrail_id, position, blocked, received_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![request.id, m.guardrail_id, m.position, m.blocked, request.received_at])?;
    }
    if let Some(b) = bodies {
        tx.prepare_cached(
            "INSERT OR REPLACE INTO bodies (request_id, request_body, response_body, request_len_seen, response_len_seen, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![b.request_id, b.request_body, b.response_body, b.request_len_seen as i64, b.response_len_seen as i64, b.expires_at])?;
    }
    Ok(previous)
}

/// Request rows one slice of the backfill writes at most, the newest first, so the most recent history reads complete
/// soonest. One slice read chunk by chunk through `requests_serving_pending` beside ten filled slices holds the write
/// lock for about 33 ms in a release build on a 32-core server host, and for about 92 ms in an unoptimized build. A
/// start writes one slice and a pass of the purge task the rest, slice after slice, each going on where the last one
/// stopped.
pub const BACKFILL_SLICE: usize = 10_000;

/// Pending rows one read of a slice takes, so a landing batch that begins during a slice waits for one chunk's read and
/// one row rather than the whole slice's read, and a slice that yields discards one chunk at most.
const BACKFILL_CHUNK: usize = 500;

/// What one slice of the backfill did: the rows it wrote and the rows still null with attempts, read after the slice
/// committed. The rare row whose trail lost every attempt stays in the pending index and counts in neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backfilled {
    pub written: usize,
    pub left: usize,
}

/// The rows the backfill fills, appended to a `FROM requests`: the pending index's own predicate, which the
/// `INDEXED BY` holds the read to, and the attempt rows the facts derive from.
const SERVING_PENDING: &str = " INDEXED BY requests_serving_pending \
    WHERE serving_account_id IS NULL AND attempts > 0 \
    AND EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id)";

/// Fills `ServingFacts` on up to `BACKFILL_SLICE` request rows whose `serving_account_id` is null and whose attempts
/// exist, newest first through `requests_serving_pending`, the facts through `ServingFacts::of`, the one rule the
/// landing runs, so a row landed after the step and a backfilled row with the same attempts read alike. The slice is
/// one transaction under the purge handoff, as a retention slice is: it starts once every write begun before it has
/// ended, and it ends after the row in progress once a write is waiting, so a batch waits for one chunk's read and one
/// row at most. The start calls it once before the ledger task writes, where nothing waits and the slice runs whole.
/// `left` counts the same rows through the same index on the slice's connection after its transaction commits, with no
/// write lock held. A pass of the purge task runs the same slice through `backfill_serving_slice`, slice after slice
/// until no row is left.
pub fn backfill_serving_facts(db: &LedgerDb) -> Result<Backfilled, LedgerError> {
    db.purge_yielding(counted_serving_slice)
}

/// One slice of the purge task's pass, answering the rows it wrote and whether a row is left, asked through the
/// count's own predicate on the slice's connection after its commit. The pass reads nothing past whether to go on, so
/// the probe stops at the first pending row rather than counting them all after every slice.
pub(crate) fn backfill_serving_slice(db: &LedgerDb) -> Result<(usize, bool), LedgerError> {
    db.purge_yielding(|conn, waiting| {
        let written = fill_serving_slice(conn, waiting)?;
        let left = conn.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM requests{SERVING_PENDING})"),
            [],
            |r| r.get::<_, bool>(0),
        )?;
        Ok((written, left))
    })
}

/// The slice on `conn`, then the count of the rows it left.
fn counted_serving_slice(
    conn: &mut Connection,
    waiting: &dyn Fn() -> bool,
) -> Result<Backfilled, LedgerError> {
    let written = fill_serving_slice(conn, waiting)?;
    // Counted after the commit on the slice's own connection: no write lock is held, and no blocking worker of the
    // runtime waits on the reader pool.
    let left = conn.query_row(
        &format!("SELECT COUNT(*) FROM requests{SERVING_PENDING}"),
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;
    Ok(Backfilled { written, left })
}

/// The slice on `conn`: the reads chunk by chunk, the fills until `waiting` answers true past the first row, and the
/// commit. Answers the rows it wrote.
fn fill_serving_slice(
    conn: &mut Connection,
    waiting: &dyn Fn() -> bool,
) -> Result<usize, LedgerError> {
    let tx = begin_write(conn)?;
    let mut written = 0;
    {
        // `ServingFacts::of` reads the request's asked speed and its last attempt alone, so the slice joins each row's
        // serving attempt and reads those columns, every other field of the two records at its default.
        let mut pending = tx.prepare(
            "SELECT r.id, r.requested_speed, a.account_id, a.upstream_status, a.outcome, a.speed, \
             a.cache_miss_reason, a.stop_reason, a.refusal_category, a.input_tokens, a.output_tokens, \
             a.cache_read_tokens, a.cache_write_5m_tokens, a.cache_write_1h_tokens \
             FROM requests r INDEXED BY requests_serving_pending \
             JOIN attempts a ON a.request_id = r.id \
             AND a.attempt = (SELECT MAX(x.attempt) FROM attempts x WHERE x.request_id = r.id) \
             WHERE r.serving_account_id IS NULL AND r.attempts > 0 ORDER BY r.received_at DESC LIMIT ?1",
        )?;
        let set = SERVING_COLUMNS
            .iter()
            .enumerate()
            .map(|(i, column)| format!("{column} = ?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let mut fill = tx.prepare(&format!("UPDATE requests SET {set} WHERE id = ?1"))?;
        // A filled row leaves the pending index, so each chunk starts after the rows the one before it filled.
        'slice: while written < BACKFILL_SLICE {
            let chunk = pending
                .query_map(
                    params![BACKFILL_CHUNK.min(BACKFILL_SLICE - written) as i64],
                    |r| {
                        let request = RequestRecord {
                            id: r.get(0)?,
                            requested_speed: r.get(1)?,
                            ..Default::default()
                        };
                        let last = AttemptRecord {
                            account_id: r.get(2)?,
                            upstream_status: r.get::<_, Option<i64>>(3)?.map(|n| n as u16),
                            outcome: r.get(4)?,
                            speed: r.get(5)?,
                            cache_miss_reason: r.get(6)?,
                            stop_reason: r.get(7)?,
                            refusal_category: r.get(8)?,
                            input_tokens: opt_u64(r, 9)?,
                            output_tokens: opt_u64(r, 10)?,
                            cache_read_tokens: opt_u64(r, 11)?,
                            cache_write_5m_tokens: opt_u64(r, 12)?,
                            cache_write_1h_tokens: opt_u64(r, 13)?,
                            ..Default::default()
                        };
                        Ok((request, last))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if chunk.is_empty() {
                break;
            }
            for (request, last) in &chunk {
                // The first row lands whatever waits, so a slice under steady writes still moves the backfill on.
                if written > 0 && waiting() {
                    break 'slice;
                }
                let facts = ServingFacts::of(request, std::slice::from_ref(last));
                fill.execute(params_from_iter(
                    once(Value::from(request.id.clone())).chain(serving_values(&facts)),
                ))?;
                written += 1;
            }
        }
    }
    tx.commit()?;
    Ok(written)
}

pub(crate) fn insert_event_in(conn: &Connection, e: &EventRecord) -> Result<(), LedgerError> {
    conn.prepare_cached(
        "INSERT INTO events (at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![e.at, e.kind, e.account_id, e.key_id, e.session_id, e.model, e.actor, e.detail, e.pool_id, e.agent_id, e.request_id])?;
    Ok(())
}

/// `summary` is the summary's JSON, `frame` the record's zstd frame. The caller built both inside the batch.
pub(crate) fn insert_decision_in(
    conn: &Connection,
    row: &DecisionRow,
    summary: &str,
    frame: &[u8],
) -> Result<(), LedgerError> {
    conn.prepare_cached(
        "INSERT INTO decisions (request_id, seq, attempt, at, kind, probe, key_id, pool_id, account_id, from_account_id, session_id, agent_id, model, summary, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
    )?
    .execute(params![
        row.request_id,
        row.seq,
        row.attempt,
        row.at,
        row.kind,
        row.probe,
        row.key_id,
        row.pool_id,
        row.account_id,
        row.from_account_id,
        row.session_id,
        row.agent_id,
        row.model,
        summary,
        frame
    ])?;
    Ok(())
}

/// Sets `cache_map` on the decision `(request_id, seq)` names and answers the rows changed (0 when no decision row
/// exists). Keyed by the selecting decision alone, so a retry in place that completed on a later attempt still lands
/// on the decision that selected it.
pub(crate) fn update_cache_map_in(
    conn: &Connection,
    write: &CacheMapWrite,
    entries: &str,
) -> Result<usize, LedgerError> {
    Ok(conn
        .prepare_cached("UPDATE decisions SET cache_map = ?3 WHERE request_id = ?1 AND seq = ?2")?
        .execute(params![write.request_id, write.seq, entries])?)
}

pub(crate) const REQUEST_SELECT: &str = "SELECT id, received_at, finished_at, key_id, key_generation, pool_id, session_id, agent_id, parent_agent_id, model, canonical_model, stream, kind, origin, status, outcome, attempts, request_bytes, response_bytes, ttfb_ms, ttfc_ms, duration_ms, error_code, client_drift, kept_warm, bodies_captured, user_agent, effort, thinking_type, thinking_budget, max_tokens, tool_count, system_blocks, message_count, context_edits, temperature, requested_speed, requested_service_tier, presented_retired, rotation_at, response_truncated, client, agent_type, compaction, context_compacted, prev_tool_ms, turn_origin, request_class, forked_from, fork_depth, auto_mode, held, stream_end_at, diagnostics_source, guardrail_id, classifier, client_contract, refused, fallback_digest, serving_account_id, reached_upstream, outcome_class, speed, cache_miss_reason, stop_reason, refusal_category, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens FROM requests";
pub(crate) const ATTEMPT_SELECT: &str = "SELECT request_id, attempt, account_id, account_kind, pin_epoch, started_at, finished_at, ttfb_ms, ttfc_ms, upstream_status, upstream_request_id, outcome, cooldown_scope, cooldown_until, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, service_tier, cache_miss_reason, client_drift, ratelimit_5h_utilization, ratelimit_5h_reset, ratelimit_7d_utilization, ratelimit_7d_reset, ratelimit_7d_oi_utilization, ratelimit_7d_oi_reset, overage_in_use, overage_utilization, thinking_tokens, web_search_requests, web_fetch_requests, stop_reason, stop_sequence, served_model, speed, inference_geo, cleared_input_tokens, refusal_category, refusal_recommended_model, selection, upstream_message_id, written_at, tier, usage_based, issued_digest FROM attempts";

/// Column `i` as an optional count, `None` where it is null.
pub(crate) fn opt_u64(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<u64>> {
    Ok(r.get::<_, Option<i64>>(i)?.map(|n| n as u64))
}

pub(crate) fn row_to_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRecord> {
    Ok(RequestRecord {
        id: r.get(0)?,
        received_at: r.get(1)?,
        finished_at: r.get(2)?,
        key_id: r.get(3)?,
        key_generation: r.get(4)?,
        pool_id: r.get(5)?,
        session_id: r.get(6)?,
        agent_id: r.get(7)?,
        parent_agent_id: r.get(8)?,
        model: r.get(9)?,
        canonical_model: r.get(10)?,
        stream: r.get(11)?,
        kind: r.get(12)?,
        origin: r.get(13)?,
        status: r.get::<_, Option<i64>>(14)?.map(|n| n as u16),
        outcome: r.get(15)?,
        attempts: r.get(16)?,
        request_bytes: opt_u64(r, 17)?,
        response_bytes: opt_u64(r, 18)?,
        ttfb_ms: r.get(19)?,
        ttfc_ms: r.get(20)?,
        duration_ms: r.get(21)?,
        error_code: r.get(22)?,
        client_drift: r.get(23)?,
        kept_warm: r.get(24)?,
        bodies_captured: r.get(25)?,
        response_truncated: r.get(40)?,
        user_agent: r.get(26)?,
        effort: r.get(27)?,
        thinking_type: r.get(28)?,
        thinking_budget: opt_u64(r, 29)?,
        max_tokens: opt_u64(r, 30)?,
        tool_count: r.get::<_, i64>(31)? as u32,
        system_blocks: r.get::<_, i64>(32)? as u32,
        message_count: r.get::<_, i64>(33)? as u32,
        context_edits: r.get(34)?,
        temperature: r.get(35)?,
        requested_speed: r.get(36)?,
        requested_service_tier: r.get(37)?,
        presented_retired: r.get(38)?,
        rotation_at: r.get(39)?,
        client: r.get(41)?,
        agent_type: r.get(42)?,
        compaction: r.get(43)?,
        context_compacted: r.get(44)?,
        prev_tool_ms: r.get::<_, Option<i64>>(45)?,
        turn_origin: r.get(46)?,
        request_class: r.get(47)?,
        forked_from: r.get(48)?,
        fork_depth: r.get::<_, Option<i64>>(49)?.map(|n| n as u32),
        auto_mode: r.get(50)?,
        held: r.get(51)?,
        stream_end_at: r.get(52)?,
        diagnostics_source: r.get(53)?,
        guardrail_id: r.get(54)?,
        classifier: r.get(55)?,
        client_contract: r.get(56)?,
        refused: r.get(57)?,
        // The child table holds the matches, which the reads that answer a request fill beside its attempts.
        guardrails: Vec::new(),
        fallback_digest: r.get(58)?,
        serving: ServingFacts {
            account_id: r.get(59)?,
            reached_upstream: r.get(60)?,
            outcome_class: r.get(61)?,
            speed: r.get(62)?,
            cache_miss_reason: r.get(63)?,
            stop_reason: r.get(64)?,
            refusal_category: r.get(65)?,
            input_tokens: opt_u64(r, 66)?,
            output_tokens: opt_u64(r, 67)?,
            cache_read_tokens: opt_u64(r, 68)?,
            cache_write_tokens: opt_u64(r, 69)?,
        },
    })
}

/// The request's matches in evaluation order.
pub(crate) fn request_guardrails(
    conn: &Connection,
    id: &str,
) -> rusqlite::Result<Vec<GuardrailMatchRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT guardrail_id, position, blocked FROM request_guardrails WHERE request_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map(params![id], |r| {
        Ok(GuardrailMatchRecord {
            guardrail_id: r.get(0)?,
            position: r.get(1)?,
            blocked: r.get(2)?,
        })
    })?;
    rows.collect()
}

pub(crate) fn row_to_attempt(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRecord> {
    Ok(AttemptRecord {
        request_id: r.get(0)?,
        attempt: r.get(1)?,
        account_id: r.get(2)?,
        account_kind: r.get(3)?,
        pin_epoch: r.get::<_, Option<i64>>(4)?.map(|n| n as u64),
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
        ttfb_ms: r.get(7)?,
        ttfc_ms: r.get(8)?,
        upstream_status: r.get::<_, Option<i64>>(9)?.map(|n| n as u16),
        upstream_request_id: r.get(10)?,
        outcome: r.get(11)?,
        cooldown_scope: r.get(12)?,
        cooldown_until: r.get(13)?,
        input_tokens: opt_u64(r, 14)?,
        output_tokens: opt_u64(r, 15)?,
        cache_read_tokens: opt_u64(r, 16)?,
        cache_write_5m_tokens: opt_u64(r, 17)?,
        cache_write_1h_tokens: opt_u64(r, 18)?,
        service_tier: r.get(19)?,
        cache_miss_reason: r.get(20)?,
        client_drift: r.get(21)?,
        ratelimit_5h_utilization: r.get(22)?,
        ratelimit_5h_reset: r.get(23)?,
        ratelimit_7d_utilization: r.get(24)?,
        ratelimit_7d_reset: r.get(25)?,
        ratelimit_7d_oi_utilization: r.get(26)?,
        ratelimit_7d_oi_reset: r.get(27)?,
        overage_in_use: r.get(28)?,
        overage_utilization: r.get(29)?,
        tier: r.get(44)?,
        thinking_tokens: opt_u64(r, 30)?,
        web_search_requests: opt_u64(r, 31)?,
        web_fetch_requests: opt_u64(r, 32)?,
        stop_reason: r.get(33)?,
        stop_sequence: r.get(34)?,
        served_model: r.get(35)?,
        speed: r.get(36)?,
        inference_geo: r.get(37)?,
        cleared_input_tokens: opt_u64(r, 38)?,
        refusal_category: r.get(39)?,
        refusal_recommended_model: r.get(40)?,
        selection: r.get(41)?,
        upstream_message_id: r.get(42)?,
        written_at: r.get(43)?,
        usage_based: r.get(45)?,
        issued_digest: r.get(46)?,
    })
}

/// Each of `requests` with its attempts in attempt order, the requests' own order kept, through one statement over the
/// page's ids rather than one per request.
pub(crate) fn with_attempts(
    conn: &Connection,
    requests: Vec<RequestRecord>,
) -> Result<Vec<RequestWithAttempts>, LedgerError> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = requests.iter().map(|r| r.id.clone()).collect();
    let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len());
    let marks = bind_all(&ids, &mut args);
    let sql =
        format!("{ATTEMPT_SELECT} WHERE request_id IN ({marks}) ORDER BY request_id, attempt");
    let file = |stmt: &mut Statement<'_>| -> Result<_, LedgerError> {
        let mut filed: HashMap<String, Vec<AttemptRecord>> = HashMap::new();
        for attempt in stmt.query_map(params_from_iter(&args), row_to_attempt)? {
            let attempt = attempt?;
            filed
                .entry(attempt.request_id.clone())
                .or_default()
                .push(attempt);
        }
        Ok(filed)
    };
    // A read by id binds one id, whose text never changes, so it takes the statement from the cache. A page's text
    // changes with its length, so it compiles apart rather than evicting the cached statements.
    let mut filed = if ids.len() == 1 {
        file(&mut *conn.prepare_cached(&sql)?)?
    } else {
        file(&mut conn.prepare(&sql)?)?
    };
    Ok(requests
        .into_iter()
        .map(|request| {
            let attempts = filed.remove(&request.id).unwrap_or_default();
            (request, attempts)
        })
        .collect())
}

/// Each request of `page` given its matches in evaluation order, through one statement over the page's ids rather
/// than one per request.
pub(crate) fn fill_guardrails(
    conn: &Connection,
    page: &mut [RequestWithAttempts],
) -> Result<(), LedgerError> {
    if page.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = page.iter().map(|(r, _)| r.id.clone()).collect();
    let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len());
    let marks = bind_all(&ids, &mut args);
    let mut stmt = conn.prepare(&format!(
        "SELECT request_id, guardrail_id, position, blocked FROM request_guardrails WHERE request_id IN ({marks}) \
         ORDER BY request_id, position"
    ))?;
    let mut filed: HashMap<String, Vec<GuardrailMatchRecord>> = HashMap::new();
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok((
            r.get::<_, String>(0)?,
            GuardrailMatchRecord {
                guardrail_id: r.get(1)?,
                position: r.get(2)?,
                blocked: r.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (request_id, matched) = row?;
        filed.entry(request_id).or_default().push(matched);
    }
    for (request, _) in page {
        request.guardrails = filed.remove(&request.id).unwrap_or_default();
    }
    Ok(())
}

/// The select of the `events` columns in the order `row_to_event` reads them, a literal so a constant statement can
/// `concat!` it.
macro_rules! event_select {
    () => {
        "SELECT at, kind, account_id, key_id, session_id, model, actor, detail, pool_id, agent_id, request_id FROM events"
    };
}
pub(crate) use event_select;

/// An `events` row in the column order every reader of the table selects.
pub(crate) fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        at: r.get(0)?,
        kind: r.get(1)?,
        account_id: r.get(2)?,
        key_id: r.get(3)?,
        session_id: r.get(4)?,
        model: r.get(5)?,
        actor: r.get(6)?,
        detail: r.get(7)?,
        pool_id: r.get(8)?,
        agent_id: r.get(9)?,
        request_id: r.get(10)?,
    })
}

/// A `quota_samples` row in the column order every reader of the table selects.
pub(crate) fn row_to_quota_sample(r: &rusqlite::Row<'_>) -> rusqlite::Result<QuotaSampleRecord> {
    Ok(QuotaSampleRecord {
        account_id: r.get(0)?,
        bucket: r.get(1)?,
        window: r.get(2)?,
        utilization: r.get(3)?,
        reset_at: r.get(4)?,
        source: r.get(5)?,
        observed_at: r.get(6)?,
    })
}

/// An `extra_usage_samples` row in the column order every reader of the table selects.
pub(crate) fn row_to_extra_usage_sample(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<ExtraUsageSampleRecord> {
    Ok(ExtraUsageSampleRecord {
        account_id: r.get(0)?,
        bucket: r.get(1)?,
        is_enabled: r.get(2)?,
        in_use: r.get(3)?,
        monthly_limit: r.get(4)?,
        used_credits: r.get(5)?,
        utilization: r.get(6)?,
        currency: r.get(7)?,
        disabled_reason: r.get(8)?,
        overage_reset: r.get(9)?,
        source: r.get(10)?,
        spend_limit_minor: r.get(11)?,
        spend_used_minor: r.get(12)?,
        spend_percent: r.get(13)?,
        spend_currency: r.get(14)?,
        spend_exponent: r.get(15)?,
    })
}

/// The store's rule: a file a newer build wrote is refused before the schema or a migration runs over it. Then
/// `SCHEMA`, the baseline at version 1, and the steps above the stored version.
fn prepare(conn: &mut Connection, path: &Path, steps: &[(u32, &str)]) -> Result<(), LedgerError> {
    let current = check_schema(conn, path)?;
    Ok(migrations::apply(conn, schema::SCHEMA, current, steps)?)
}

/// What a ledger's `meta` without its `schema_version` row reads as: version 0, as a file without `meta` does.
const VERSION_WITHOUT_ROW: u32 = 0;

/// The refusal `prepare` makes before it writes anything, answering the stored version.
fn check_schema(conn: &Connection, path: &Path) -> Result<u32, LedgerError> {
    migrations::check_version(conn, VERSION_WITHOUT_ROW, latest_version())
        .map_err(version_error(path))
}

/// A version read's failure as the ledger answers it, its refusal of a newer file naming `path`.
fn version_error(path: &Path) -> impl FnOnce(VersionError) -> LedgerError + '_ {
    move |error| match error {
        VersionError::Io(error) => LedgerError::Io(error),
        VersionError::Sqlite(error) => LedgerError::Sqlite(error),
        VersionError::TooNew { found, supported } => LedgerError::SchemaTooNew {
            path: path.to_path_buf(),
            found,
            supported,
        },
    }
}

fn latest_version() -> u32 {
    migrations::latest_version(schema::MIGRATIONS)
}

/// A file that has never run `prepare` has no `meta` table yet, which reads as version 0, so the refusal of a
/// newer file runs before `schema::SCHEMA` creates the table.
pub(crate) fn read_version(conn: &Connection) -> Result<u32, LedgerError> {
    Ok(migrations::stored_version(conn, VERSION_WITHOUT_ROW)?)
}

/// Shared by both opens, so an in-memory ledger analyzes at its open as a file does.
fn analyze_at_open(conn: &Connection) -> Result<(), LedgerError> {
    conn.pragma_update(None, "analysis_limit", ANALYSIS_LIMIT)?;
    conn.execute_batch(&format!("PRAGMA optimize={OPEN_OPTIMIZE_MASK:#x}"))?;
    Ok(())
}

/// `sqlite_stat1`'s rows as (`tbl`, `idx`, `stat`), ordered by all three so two reads of the same rows compare equal.
pub(crate) fn stat1_rows(
    conn: &Connection,
) -> rusqlite::Result<Vec<(String, Option<String>, String)>> {
    let mut stmt =
        conn.prepare("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx, stat")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::*;
    use phoebis_store::AccountKind;
    use std::future::Future;
    use std::task::Poll;

    /// A read that holds its pooled reader until its release is sent, then answers the schema's version.
    type Held = (
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<Result<u32, LedgerError>>,
    );

    /// Holds every pooled reader at once. Each hold must take its reader within 1 s, so a pool short of a reader fails
    /// at `short` instead of hanging, and a failing test drops the releases, which ends the holds.
    async fn hold_every_reader(db: &Arc<LedgerDb>, short: &str) -> Vec<Held> {
        let (taken, mut takes) = tokio::sync::mpsc::channel::<()>(READERS);
        let mut held = Vec::new();
        for _ in 0..READERS {
            let (release, released) = std::sync::mpsc::channel::<()>();
            let taken = taken.clone();
            let task = tokio::spawn({
                let db = db.clone();
                async move {
                    db.read(move |conn| {
                        taken.blocking_send(()).unwrap();
                        released.recv().unwrap();
                        read_version(conn)
                    })
                    .await
                }
            });
            held.push((release, task));
        }
        for _ in 0..READERS {
            tokio::time::timeout(Duration::from_secs(1), takes.recv())
                .await
                .expect(short)
                .unwrap();
        }
        held
    }

    fn request(id: &str) -> RequestRecord {
        RequestRecord {
            id: id.into(),
            received_at: 1_000,
            finished_at: Some(1_500),
            key_id: Some("k".into()),
            key_generation: 0,
            pool_id: Some("p".into()),
            session_id: Some("s".into()),
            agent_id: Some("main".into()),
            parent_agent_id: None,
            model: "claude-sonnet-5[1m]".into(),
            canonical_model: "claude-sonnet-5".into(),
            stream: true,
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 2,
            request_bytes: Some(10),
            response_bytes: Some(20),
            ttfb_ms: Some(100),
            ttfc_ms: Some(150),
            duration_ms: Some(500),
            error_code: None,
            kept_warm: false,
            bodies_captured: true,
            user_agent: Some("claude-cli/2.1.300 (external, cli)".into()),
            client: Some("claude-code/cli".into()),
            agent_type: Some("Explore".into()),
            prev_tool_ms: Some(1_235),
            turn_origin: Some("human".into()),
            auto_mode: Some("server".into()),
            stream_end_at: Some(1_450),
            ..Default::default()
        }
    }

    fn attempt(id: &str, n: u32, account: &str, outcome: &str) -> AttemptRecord {
        AttemptRecord {
            request_id: id.into(),
            attempt: n,
            account_id: account.into(),
            account_kind: AccountKind::Oauth,
            pin_epoch: Some(3),
            started_at: 1_000 + n as i64,
            finished_at: Some(1_100),
            ttfb_ms: Some(100),
            ttfc_ms: None,
            upstream_status: Some(if outcome == "succeeded" { 200 } else { 429 }),
            upstream_request_id: Some("req_up".into()),
            upstream_message_id: (outcome == "succeeded").then(|| "msg_up".into()),
            outcome: outcome.into(),
            cooldown_scope: None,
            cooldown_until: None,
            input_tokens: Some(5),
            output_tokens: Some(7),
            cache_read_tokens: None,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            service_tier: None,
            cache_miss_reason: Some("none".into()),
            client_drift: None,
            ratelimit_5h_utilization: Some(0.25),
            ratelimit_5h_reset: Some(2_000),
            ratelimit_7d_utilization: None,
            ratelimit_7d_reset: None,
            ratelimit_7d_oi_utilization: None,
            ratelimit_7d_oi_reset: None,
            overage_in_use: Some(false),
            overage_utilization: None,
            tier: Some("default_claude_max_20x".into()),
            thinking_tokens: Some(4),
            web_search_requests: Some(2),
            web_fetch_requests: Some(1),
            stop_reason: Some("refusal".into()),
            stop_sequence: Some("<<stop>>".into()),
            served_model: Some("claude-opus-5".into()),
            speed: Some("standard".into()),
            inference_geo: Some("us".into()),
            cleared_input_tokens: Some(1_200),
            refusal_category: Some("cyber".into()),
            refusal_recommended_model: Some("claude-opus-5".into()),
            selection: Some(
                r#"{"strategy":"pace","tier":[0,1,0],"renewed":false,"moved":null,"skipped":[]}"#
                    .into(),
            ),
            written_at: Some(1_010 + n as i64),
            usage_based: true,
            issued_digest: None,
        }
    }

    #[tokio::test]
    async fn the_schema_opens_in_wal_mode_with_a_version_and_rows_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("data").join("ledger.sqlite")).unwrap());
        assert_eq!(db.read_blocking(read_version).unwrap(), 3);
        let mode: String = db
            .read_blocking(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode, "wal");
        let bodies = BodyRecord {
            request_id: "r1".into(),
            request_body: b"{}".to_vec(),
            response_body: Some(b"hi".to_vec()),
            request_len_seen: 2,
            response_len_seen: 2,
            expires_at: 9_000,
        };
        db.insert_request(
            &request("r1"),
            &[
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded"),
            ],
            Some(&bodies),
        )
        .unwrap();
        // A second write of the same request replaces rather than fails, so a retried record is not an error.
        db.insert_request(
            &request("r1"),
            &[
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded"),
            ],
            None,
        )
        .unwrap();
        let (row, attempts) = db.request("r1").await.unwrap().unwrap();
        assert_eq!(
            row,
            RequestRecord {
                serving: ServingFacts {
                    account_id: Some("b".into()),
                    reached_upstream: Some(true),
                    outcome_class: Some("succeeded".into()),
                    speed: Some("standard".into()),
                    cache_miss_reason: Some("none".into()),
                    stop_reason: Some("refusal".into()),
                    refusal_category: Some("cyber".into()),
                    input_tokens: Some(5),
                    output_tokens: Some(7),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                },
                ..request("r1")
            },
            "the written members read back beside the facts the landing took from the second attempt"
        );
        assert_eq!(
            attempts,
            vec![
                attempt("r1", 1, "a", "rate_limited"),
                attempt("r1", 2, "b", "succeeded")
            ]
        );
        assert_eq!(
            (row.stream_end_at, attempts[1].written_at),
            (Some(1_450), Some(1_012)),
            "the two instants cross the insert and the select"
        );
        assert_eq!(db.bodies("r1", 1_500).await.unwrap().unwrap(), bodies);
        assert!(db.request("nope").await.unwrap().is_none());
        db.insert_event(&EventRecord {
            at: 5,
            kind: "ledger_gap".into(),
            detail: Some(r#"{"dropped":3}"#.into()),
            ..Default::default()
        })
        .unwrap();
        db.insert_event(&EventRecord {
            at: 7,
            kind: "cooldown_set".into(),
            account_id: Some("a".into()),
            ..Default::default()
        })
        .unwrap();
        let events = db.events(0, 10, 10).await.unwrap();
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["ledger_gap", "cooldown_set"]
        );
        assert_eq!(db.events(6, 10, 10).await.unwrap().len(), 1);
        let mode = std::fs::metadata(dir.path().join("data").join("ledger.sqlite"))
            .unwrap()
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_read_runs_off_the_runtime_thread_and_answers_on_it() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let caller = std::thread::current().id();
        let (worker, version) = db
            .read(move |c| Ok((std::thread::current().id(), read_version(c)?)))
            .await
            .unwrap();
        assert_ne!(worker, caller, "the closure ran on a blocking worker");
        assert_eq!(version, 3, "and answered what the connection holds");
    }

    #[tokio::test]
    async fn reads_run_beside_each_other_and_a_writer_side_read_runs_while_every_reader_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
        let (taken, mut takes) = tokio::sync::mpsc::channel::<()>(READERS);
        let mut releases = Vec::new();
        let mut held = Vec::new();
        for _ in 0..READERS {
            let (release, released) = std::sync::mpsc::channel::<()>();
            releases.push(release);
            let taken = taken.clone();
            held.push(tokio::spawn({
                let db = db.clone();
                async move {
                    db.read(move |_| {
                        taken.blocking_send(()).unwrap();
                        released.recv().unwrap();
                        Ok(())
                    })
                    .await
                }
            }));
            tokio::time::timeout(Duration::from_secs(1), takes.recv())
                .await
                .expect("the newest hold took a free reader")
                .unwrap();
            if releases.len() < READERS {
                // A read completes beside the held ones while a connection is still free.
                let beside =
                    tokio::time::timeout(Duration::from_secs(1), db.read(read_version)).await;
                assert_eq!(
                    beside
                        .expect("a read never waited behind an unrelated held read")
                        .unwrap(),
                    3
                );
            }
        }
        // Every pooled reader is held: the writer's own read still answers, and a fifth read waits.
        let writer = db.writer_read(WriterRead::ParkedCount, crate::query::parked_count);
        let writer = tokio::time::timeout(Duration::from_secs(1), writer).await;
        assert_eq!(
            writer
                .expect("the writer's read never waits for a reader")
                .unwrap(),
            0
        );
        let mut fifth = Box::pin(db.read(read_version));
        let waiting = tokio::time::timeout(Duration::from_millis(200), &mut fifth).await;
        assert!(waiting.is_err(), "the fifth read waits for a connection");
        // Dropped while it waits: the pool owes it nothing.
        drop(fifth);
        // Two waiters registered on an empty pool, then two returns: each return wakes its own waiter.
        let mut first = std::pin::pin!(db.read(read_version));
        let mut second = std::pin::pin!(db.read(read_version));
        // One poll each drives both futures to their waits, so both wait before the first return.
        std::future::poll_fn(|cx| {
            let _ = Future::poll(first.as_mut(), cx);
            let _ = Future::poll(second.as_mut(), cx);
            Poll::Ready(())
        })
        .await;
        let mut releases = releases.into_iter();
        let mut held = held.into_iter();
        for _ in 0..2 {
            releases.next().unwrap().send(()).unwrap();
            held.next().unwrap().await.unwrap().unwrap();
        }
        let both = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(first, second)
        })
        .await;
        let (a, b) = both.expect("two returns woke two waiters");
        assert_eq!((a.unwrap(), b.unwrap()), (3, 3));
        for release in releases {
            release.send(()).unwrap();
        }
        for task in held {
            task.await.unwrap().unwrap();
        }
        let short = "every connection came back to the pool";
        for (release, task) in hold_every_reader(&db, short).await {
            release.send(()).unwrap();
            assert_eq!(task.await.unwrap().unwrap(), 3);
        }
    }

    #[tokio::test]
    async fn a_blocking_read_waits_while_every_reader_is_held_and_wakes_on_one_return() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
        let mut held = hold_every_reader(&db, "every pooled reader was free to hold").await;
        let blocked = std::thread::spawn({
            let db = db.clone();
            move || db.read_blocking(read_version)
        });
        fake_upstream::stays_for(Duration::from_millis(200), || !blocked.is_finished()).await;
        let (release, task) = held.pop().unwrap();
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap(), 3);
        fake_upstream::wait_for(Duration::from_secs(1), || {
            blocked.is_finished().then_some(())
        })
        .await;
        assert_eq!(blocked.join().unwrap().unwrap(), 3);
        for (release, task) in held {
            release.send(()).unwrap();
            task.await.unwrap().unwrap();
        }
    }

    const TWO_INDEX_SQL: &str =
        "SELECT id FROM requests WHERE key_id = ?1 AND session_id = ?2 ORDER BY received_at LIMIT 1";

    /// The plan of `TWO_INDEX_SQL`, which both `requests_key` and `requests_session` answer. `read_blocking` takes the
    /// newest idle reader, so consecutive calls on an otherwise idle pool land on one connection.
    fn plan(db: &LedgerDb) -> Vec<String> {
        db.read_blocking(|c| {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {TWO_INDEX_SQL}"))?;
            let rows = stmt
                .query_map(params!["main", "s"], |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap()
    }

    /// `stat` rows that make `requests_key` read as `per_key` rows per key and the session index as `per_session` per
    /// session.
    fn write_statistics(db: &LedgerDb, per_key: u32, per_session: u32) {
        db.purge(|c| {
            c.execute_batch(&format!(
                "DELETE FROM sqlite_stat1; \
                 INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES \
                 ('requests', 'requests_key', '1000 {per_key} 1'), ('requests', 'requests_session', '1000 {per_session} 1');"
            ))?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_open_analyzes_a_file_whose_tables_hold_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let db = LedgerDb::open(&path).unwrap();
        db.insert_request(&request("r1"), &[], None).unwrap();
        drop(db);
        let db = LedgerDb::open(&path).unwrap();
        let rows: i64 = db
            .read_blocking(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM sqlite_stat1 WHERE tbl = 'requests'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert!(
            rows >= 1,
            "the open analyzed the requests table once it held a row"
        );
    }

    #[test]
    fn a_pooled_reader_behind_the_statistics_generation_reopens_and_plans_on_the_new_statistics() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        write_statistics(&db, 1, 1_000);
        db.debug_bump_statistics_generation();
        assert!(
            plan(&db)
                .iter()
                .any(|s| s.starts_with("SEARCH requests USING INDEX requests_key")),
            "{:?}",
            plan(&db)
        );
        write_statistics(&db, 1_000, 1);
        assert!(
            plan(&db)
                .iter()
                .any(|s| s.starts_with("SEARCH requests USING INDEX requests_key")),
            "a reader keeps its loaded statistics until the generation moves: {:?}",
            plan(&db)
        );
        db.debug_bump_statistics_generation();
        assert!(
            plan(&db)
                .iter()
                .any(|s| s.starts_with("SEARCH requests USING INDEX requests_session")),
            "{:?}",
            plan(&db)
        );
        let cache: i64 = db
            .read_blocking(|c| Ok(c.query_row("PRAGMA cache_size", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(
            cache, -READER_CACHE_KIB,
            "the reopened reader carries the pool's page cache"
        );
    }

    #[test]
    fn a_reader_inside_a_closure_finishes_on_its_connection_and_reopens_at_its_next_read() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
        write_statistics(&db, 1, 1_000);
        db.debug_bump_statistics_generation();
        assert!(plan(&db)
            .iter()
            .any(|s| s.starts_with("SEARCH requests USING INDEX requests_key")));
        let (taken, on_taken) = std::sync::mpsc::channel::<()>();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let held = std::thread::spawn({
            let db = db.clone();
            move || {
                db.read_blocking(|c| {
                    taken.send(()).unwrap();
                    released.recv().unwrap();
                    let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {TWO_INDEX_SQL}"))?;
                    let rows = stmt
                        .query_map(params!["main", "s"], |r| r.get::<_, String>(3))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .unwrap()
            }
        });
        on_taken.recv().unwrap();
        write_statistics(&db, 1_000, 1);
        db.debug_bump_statistics_generation();
        release.send(()).unwrap();
        let inside = held.join().unwrap();
        assert!(
            inside
                .iter()
                .any(|s| s.starts_with("SEARCH requests USING INDEX requests_key")),
            "the closure in flight finished on its connection: {inside:?}"
        );
        assert!(
            plan(&db)
                .iter()
                .any(|s| s.starts_with("SEARCH requests USING INDEX requests_session")),
            "and that connection reopened at its next read: {:?}",
            plan(&db)
        );
    }

    #[test]
    fn a_refresh_moves_the_readers_generation_only_when_it_changed_the_statistics() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        assert_eq!(
            (
                db.refresh_statistics().unwrap(),
                db.debug_statistics_generation()
            ),
            (false, 0),
            "the open analyzed every table that held a row"
        );
        db.insert_request(&request("r1"), &[], None).unwrap();
        assert_eq!(
            (
                db.refresh_statistics().unwrap(),
                db.debug_statistics_generation()
            ),
            (true, 1),
            "the requests table holds a row and its indexes have no statistics"
        );
        assert_eq!(
            (
                db.refresh_statistics().unwrap(),
                db.debug_statistics_generation()
            ),
            (false, 1),
            "every table that holds a row has its statistics"
        );
    }

    #[tokio::test]
    async fn a_failed_reopen_answers_its_error_and_leaves_every_reader_in_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let db = Arc::new(LedgerDb::open(&path).unwrap());
        // A read-only open fails on a missing main file whatever its WAL sidecars hold, and the connections already
        // open keep their descriptors, so moving the main file away fails the reopen and nothing else.
        let away = dir.path().join("away.sqlite");
        std::fs::rename(&path, &away).unwrap();
        db.debug_bump_statistics_generation();
        let failed = db.read_blocking(read_version);
        assert!(
            matches!(
                failed,
                Err(LedgerError::Sqlite(rusqlite::Error::SqliteFailure(ref e, _)))
                    if e.code == rusqlite::ErrorCode::CannotOpen
            ),
            "{failed:?}"
        );
        std::fs::rename(&away, &path).unwrap();
        assert_eq!(db.read_blocking(read_version).unwrap(), 3);
        let short = "the reader whose reopen failed came back to the pool";
        for (release, task) in hold_every_reader(&db, short).await {
            release.send(()).unwrap();
            assert_eq!(task.await.unwrap().unwrap(), 3);
        }
    }

    #[tokio::test]
    async fn a_panicked_read_answers_read_canceled_and_the_next_read_on_the_same_ledger_succeeds() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let panicked = db
            .read(|_conn| -> Result<(), LedgerError> {
                panic!("a read closure that never means to answer");
            })
            .await;
        assert!(matches!(panicked, Err(LedgerError::ReadCanceled)));
        assert_eq!(
            db.read_blocking(read_version).unwrap(),
            3,
            "the poisoned guard recovered rather than turning every later read into ReadCanceled too"
        );
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
        let panicked = db
            .read(|_conn| -> Result<(), LedgerError> {
                panic!("a read closure that never means to answer");
            })
            .await;
        assert!(matches!(panicked, Err(LedgerError::ReadCanceled)));
        let short = "the panicked closure's connection came back to the pool";
        for (release, task) in hold_every_reader(&db, short).await {
            release.send(()).unwrap();
            assert_eq!(task.await.unwrap().unwrap(), 3);
        }
    }

    #[tokio::test]
    async fn a_writer_side_hook_fails_or_holds_the_read_it_names_and_no_other() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        assert_eq!(
            db.writer_read(WriterRead::ParkedCount, crate::query::parked_count)
                .await
                .unwrap(),
            0
        );
        db.debug_fail_next_writer_read(WriterRead::MemberSpends);
        assert_eq!(
            db.writer_read(WriterRead::ParkedCount, crate::query::parked_count)
                .await
                .unwrap(),
            0,
            "a hook on the spend read leaves the count read alone"
        );
        let failed = db.writer_read(WriterRead::MemberSpends, |_| Ok(())).await;
        assert!(
            matches!(failed, Err(LedgerError::Io(ref e)) if e.to_string() == INJECTED_READ_FAILURE),
            "{failed:?}"
        );
        assert!(
            db.writer_read(WriterRead::MemberSpends, |_| Ok(()))
                .await
                .is_ok(),
            "the hook fires once"
        );
        let hold = db.debug_hold_next_writer_read(WriterRead::ParkedCount);
        tokio::time::timeout(
            Duration::from_secs(5),
            db.writer_read(WriterRead::MemberSpends, |_| Ok(())),
        )
        .await
        .expect("a hold on the count read leaves the spend read alone")
        .unwrap();
        let parked = tokio::spawn({
            let db = db.clone();
            async move {
                db.writer_read(WriterRead::ParkedCount, crate::query::parked_count)
                    .await
            }
        });
        fake_upstream::wait_for(Duration::from_secs(5), || hold.is_parked().then_some(())).await;
        fake_upstream::stays_for(Duration::from_millis(200), || !parked.is_finished()).await;
        hold.release();
        assert_eq!(parked.await.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_writer_side_read_recovers_a_poisoned_writer_guard_and_maps_its_own_panic_to_read_canceled(
    ) {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.write(|_| -> Result<(), LedgerError> { panic!("a bug inside the closure") })
        }));
        assert!(poisoned.is_err());
        let panicked = db
            .writer_read(WriterRead::ParkedCount, |_| -> Result<(), LedgerError> {
                panic!("a read closure that never means to answer")
            })
            .await;
        assert!(
            matches!(panicked, Err(LedgerError::ReadCanceled)),
            "{panicked:?}"
        );
        assert_eq!(
            db.writer_read(WriterRead::ParkedCount, crate::query::parked_count)
                .await
                .unwrap(),
            0,
            "the poisoned guard recovered rather than turning every later writer-side read into ReadCanceled"
        );
    }

    #[tokio::test]
    async fn a_panic_inside_a_write_closure_leaves_the_writer_usable() {
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.write(|_| -> Result<(), LedgerError> { panic!("a bug inside the closure") })
        }));
        assert!(poisoned.is_err());
        db.write(|c| {
            c.execute("INSERT INTO meta (key, value) VALUES ('probe', '1')", [])?;
            Ok(())
        })
        .expect("the next write lands");
    }

    #[test]
    fn an_older_build_refuses_a_ledger_a_newer_build_wrote_and_leaves_it_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        LedgerDb::open(&path)
            .unwrap()
            .write(|c| {
                c.execute(
                    "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let refused = LedgerDb::open(&path)
            .err()
            .expect("the newer file is refused");
        assert!(
            matches!(
                refused,
                LedgerError::SchemaTooNew {
                    found: 99,
                    supported: 3,
                    ..
                }
            ),
            "{refused}"
        );
        let message = refused.to_string();
        assert!(
            message.contains("99")
                && message.contains("restore the backup taken before the upgrade")
                && message.contains("phoebis init"),
            "the refusal names the version it found and both remedies: {message}"
        );
        assert!(
            !message.contains(&path.display().to_string()),
            "the path is the caller's to name, as the start's wrap does: {message}"
        );
        let stored: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "99", "no migration ran over the newer file");
    }

    /// The baseline with no step above it, so the test starts at version 1 whatever the build's own chain holds.
    fn prepared_in_memory() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        prepare(&mut conn, Path::new(":memory:"), &[]).unwrap();
        conn
    }

    fn estimated_columns(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('window_reconciliations') WHERE name = 'estimated'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_ledger_stamped_at_version_one_loses_the_column_and_keeps_its_rows() {
        let mut conn = prepared_in_memory();
        let window_end = 1_768_485_600_000_i64;
        conn.execute(
            "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, last_reading_at, \
             observed_rise, predicted_share, ratio, requests, estimated, computed_at) \
             VALUES ('a', 'five_hour', ?1, ?2, ?3, 0.2, 0.25, 0.8, 7, 2, ?1)",
            params![window_end, window_end - 18_000_000, window_end - 60_000],
        )
        .unwrap();
        migrations::apply(&mut conn, schema::SCHEMA, 1, schema::MIGRATIONS).unwrap();
        let rows = crate::reconcile::read_reconciliations(&conn, &[], window_end).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row.account_id.as_str(),
                    row.window_end,
                    row.ratio,
                    row.requests
                ))
                .collect::<Vec<_>>(),
            [("a", window_end, 0.8, 7)],
            "the row keeps its other columns"
        );
        assert_eq!(
            (estimated_columns(&conn), read_version(&conn).unwrap()),
            (0, 3)
        );
    }

    #[tokio::test]
    async fn a_batch_waits_for_a_purge_slice_holding_the_write_lock_and_lands() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let slice = std::thread::spawn({
            let db = db.clone();
            move || {
                db.purge(|conn| {
                    let tx = begin_write(conn)?;
                    tx.execute("DELETE FROM bodies WHERE expires_at <= 0", [])?;
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    tx.commit()?;
                    Ok(())
                })
            }
        });
        held_rx.recv().unwrap();
        let batch = std::thread::spawn({
            let db = db.clone();
            move || db.insert_request(&request("r1"), &[attempt("r1", 1, "a", "succeeded")], None)
        });
        fake_upstream::stays_for(std::time::Duration::from_millis(200), || {
            !batch.is_finished()
        })
        .await;
        release_tx.send(()).unwrap();
        slice.join().unwrap().unwrap();
        batch
            .join()
            .unwrap()
            .expect("the batch waited for the slice's commit rather than failing at once");
        assert!(
            db.request("r1").await.unwrap().is_some(),
            "and its row is there"
        );
    }

    #[test]
    fn a_write_begun_after_the_handoff_ends_a_backfill_slice_after_the_row_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        db.write(|c| {
            c.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) \
                 INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
                 SELECT 'old-' || i, i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1 FROM n; \
                 INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome) \
                 SELECT id, 1, 'a', 'oauth', received_at, 'succeeded' FROM requests;",
            )?;
            Ok(())
        })
        .unwrap();
        // On a file ledger the write takes the writer's mutex, apart from the purger's the slice holds. The slice fills
        // inside the write's closure, so the write has begun and not ended while it fills, as a batch blocked on the
        // slice's lock has.
        let slice = |write: bool| {
            db.purge_yielding(|conn, waiting| {
                if write {
                    db.write(|_| counted_serving_slice(conn, waiting))
                } else {
                    counted_serving_slice(conn, waiting)
                }
            })
            .unwrap()
        };
        assert_eq!(
            slice(true),
            Backfilled {
                written: 1,
                left: 4
            },
            "the write waits for one row, and the other four wait for the next slice"
        );
        assert_eq!(
            slice(false),
            Backfilled {
                written: 4,
                left: 0
            },
            "with no write begun, the next slice fills the rest"
        );
    }

    #[tokio::test]
    async fn a_relanded_row_keeps_only_its_new_matches_and_a_new_row_with_none_leaves_the_child_table_as_it_was(
    ) {
        async fn matches(db: &Arc<LedgerDb>, id: &str) -> Vec<String> {
            let (row, _) = db.request(id).await.unwrap().unwrap();
            row.guardrails.into_iter().map(|m| m.guardrail_id).collect()
        }
        let db = Arc::new(LedgerDb::open_in_memory().unwrap());
        let matched = |id: &str, guardrails: &[&str]| RequestRecord {
            guardrails: guardrails
                .iter()
                .enumerate()
                .map(|(position, g)| GuardrailMatchRecord {
                    guardrail_id: (*g).into(),
                    position: position as u32,
                    blocked: false,
                })
                .collect(),
            ..request(id)
        };
        let child_rows = || -> i64 {
            db.read_blocking(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM request_guardrails", [], |r| r.get(0))?)
            })
            .unwrap()
        };
        db.insert_request(&matched("r1", &["g1", "g2"]), &[], None)
            .unwrap();
        assert_eq!(matches(&db, "r1").await, ["g1", "g2"]);
        db.insert_request(&matched("r1", &["g3"]), &[], None)
            .unwrap();
        assert_eq!(
            matches(&db, "r1").await,
            ["g3"],
            "the second landing's matches replace the first's"
        );
        db.insert_request(&matched("r2", &["g1"]), &[], None)
            .unwrap();
        db.insert_request(&matched("r2", &[]), &[], None).unwrap();
        assert!(
            matches(&db, "r2").await.is_empty(),
            "a row landed again with no match keeps none of its earlier ones"
        );
        assert_eq!(child_rows(), 1, "r1's one match is all the table holds");
        db.insert_request(&matched("r3", &[]), &[], None).unwrap();
        assert_eq!(
            child_rows(),
            1,
            "a new row with no match leaves the table as it was"
        );
        assert_eq!(matches(&db, "r1").await, ["g3"]);
    }
}
