//! The operator conditions as gauges: a stateless reader over the state the router already holds, fed the request
//! outcomes off the bus. Nothing here is configured, delivered or recorded.

mod reader;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use phoebis_bus::{spawn_subscriber, Bus, Delivery, Event};
use phoebis_ledger::{parked_summary, AttentionGaugeSource, AttentionGauges, Ledger, LedgerError};
use phoebis_oauth::{Clock, UsageService};
use phoebis_routing::RouterHandle;
use phoebis_store::{Store, StoreError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::reader::ReaderState;

pub use reader::{
    Condition, Kind, Link, Subject, SubjectKind, BACKOFF_ATTENTION_AFTER_MS,
    CERTIFICATE_WARNING_DAYS, REFRESH_RUN_READ,
};

const STATE_POISONED: &str = "attention state mutex poisoned by a prior panic";

/// One listener's pair as the running router serves it: what the two certificate conditions read. Declared here
/// so the reader never names the router crate. The days left are the reader's to compute at each read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairFacts {
    /// The listener key, `proxy.tls` or `admin.tls`.
    pub key: String,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// The loaded leaf's `not_after`, in Unix milliseconds.
    pub not_after_ms: i64,
    /// When the pair the listener serves was loaded, at the start or by the last reload that landed.
    pub loaded_at: i64,
    /// The last reload the loader refused, its instant and the loader's line, until a reload lands.
    pub failed: Option<(i64, String)>,
}

/// The router's loaded pairs, in the listeners' order, a listener serving plain HTTP left out.
pub trait TlsPairs: Send + Sync {
    fn pairs(&self) -> Vec<PairFacts>;
}

pub struct AttentionParts {
    pub bus: Bus,
    pub clock: Clock,
    /// The ledger's degraded flag and its parked rows; `None` where a harness builds no ledger, and then
    /// `writer_degraded` for the ledger reads 0 and `ledger_record_parked` has no series.
    pub ledger: Option<Ledger>,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    /// The router's loaded pairs; `None` where a harness builds no listeners, and then neither certificate kind has
    /// a series.
    pub tls: Option<Arc<dyn TlsPairs>>,
    /// Where the poll's standing per account is read; `None` where a harness builds no usage service, and no account
    /// then reads a backoff.
    pub usage: Option<Arc<UsageService>>,
}

/// A read the reader could not finish; the route answers 500 and the scrape answers 500 on either, so no condition
/// is reported clear that was not read.
#[derive(Debug, thiserror::Error)]
pub enum AttentionError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The one reader both the metrics listener and the admin route ask, with the request-fed state the bus arm writes.
pub struct Attention {
    clock: Clock,
    ledger: Option<Ledger>,
    router: RouterHandle,
    state: Mutex<ReaderState>,
    store: Arc<Store>,
    tls: Option<Arc<dyn TlsPairs>>,
    usage: Option<Arc<UsageService>>,
}

impl Attention {
    /// Every subject of every kind, holding or clear; a failed store or ledger read fails the whole read.
    pub fn conditions(&self, now: i64) -> Result<Vec<Condition>, AttentionError> {
        // Read from the file on every call, as `refresh_failed` reads the registry, so the condition holds across a
        // restart and clears on its own when the start lands the rows. A read that fails fails the whole answer, as
        // the refresh read's does: a scrape never reports clear what it could not read. Read before the state lock,
        // so a read waiting on the ledger's reader connection never holds the bus arm's `observe` behind it.
        let parked = self
            .ledger
            .as_ref()
            .map(|ledger| ledger.db().read_blocking(parked_summary))
            .transpose()?;
        // The registry's reads come before the lock for the same reason: a stalled registry file holds the store's
        // connection mutex, and `observe` waits on this lock.
        let accounts = self.store.accounts()?;
        let refresh_runs = reader::refresh_runs(&self.store, &accounts)?;
        // The pairs before the lock too: their lock is the router's, which a reload holds across a whole load from
        // disk.
        let pairs = self.tls.as_ref().map(|tls| tls.pairs());
        let mut state = self.state.lock().expect(STATE_POISONED);
        reader::conditions(
            &mut state,
            &accounts,
            &refresh_runs,
            &self.router,
            self.ledger.as_ref().is_some_and(Ledger::degraded),
            parked.as_deref(),
            pairs.as_deref(),
            self.usage.as_deref(),
            now,
        )
    }

    /// The bus arm's entry, public so a test drives it event by event.
    pub fn observe(&self, event: &Event) {
        let now = (self.clock)();
        reader::observe(&mut self.state.lock().expect(STATE_POISONED), event, now);
    }

    /// The metrics listener reads the conditions through a closure, since the ledger that renders them is one of this
    /// crate's dependencies and cannot name its types. Sorted, so two scrapes list the series in one order.
    pub fn gauge_source(self: &Arc<Self>) -> AttentionGaugeSource {
        let attention = self.clone();
        Arc::new(move || {
            let mut conditions = attention.conditions((attention.clock)())?;
            conditions.sort_by(|a, b| (a.kind, &a.subject.id).cmp(&(b.kind, &b.subject.id)));
            Ok(AttentionGauges {
                conditions: conditions
                    .into_iter()
                    .map(|c| (c.kind.as_str(), c.subject.id, c.holds))
                    .collect(),
            })
        })
    }
}

/// The bus arm's task, so the binary's teardown can end it.
pub struct AttentionHandles {
    arm: JoinHandle<()>,
    cancel: CancellationToken,
}

impl AttentionHandles {
    /// Ends the arm once the event in hand is read.
    pub fn stop(&mut self) {
        self.cancel.cancel();
    }

    /// Ends the arm wherever it is, for a teardown out of time.
    pub fn abort(&mut self) {
        self.cancel.cancel();
        self.arm.abort();
    }
}

/// The reader and its bus arm, which hands every event to `observe`. Nothing else runs: every other condition is read
/// when a scrape or the admin route asks.
pub fn spawn(parts: AttentionParts) -> (Arc<Attention>, AttentionHandles) {
    // Best-effort: every condition is level-triggered from the newest event, so the next one decides.
    let subscriber = parts.bus.subscribe("attention", Delivery::BestEffort);
    let attention = Arc::new(Attention {
        clock: parts.clock,
        ledger: parts.ledger,
        router: parts.router,
        state: Mutex::new(ReaderState::default()),
        store: parts.store,
        tls: parts.tls,
        usage: parts.usage,
    });
    let cancel = CancellationToken::new();
    let arm = spawn_subscriber(subscriber, {
        let attention = attention.clone();
        let cancel = cancel.clone();
        move |mut subscriber| async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = subscriber.events.recv() => match event {
                        Some(event) => attention.observe(&event),
                        None => break,
                    },
                }
            }
        }
    });
    (attention, AttentionHandles { arm, cancel })
}
