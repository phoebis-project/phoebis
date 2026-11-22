//! The operator conditions as gauges: a stateless reader over the state the router already holds, fed the request
//! outcomes off the bus. One threshold is configured, the days before an unused limit reset ends. Nothing here is
//! delivered or recorded.

mod reader;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use phoebis_bus::{spawn_subscriber, Bus, Delivery, Event};
use phoebis_ledger::{
    parked_summary, read_expired_between_turns, routing_conditions, trailing_day_hours,
    AttentionGaugeSource, AttentionGauges, Connection, ExpiredBetweenTurns, Ledger, LedgerError,
    ParkedSummary, RoutingCondition,
};
use phoebis_oauth::{Clock, UsageService};
use phoebis_routing::RouterHandle;
use phoebis_store::{Store, StoreError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::reader::ReaderState;

pub use reader::{
    Condition, Kind, Link, Subject, SubjectKind, BACKOFF_ATTENTION_AFTER_MS,
    CERTIFICATE_WARNING_DAYS, FEATURE_CA, REFRESH_RUN_READ,
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

/// The feature CA as the running router holds it: what `certificate_expiring` reads under `features.ca`.
#[derive(Clone, Debug)]
pub struct FeatureCaFacts {
    pub cert_file: PathBuf,
    /// The certificate's `not_after`, in Unix milliseconds.
    pub not_after_ms: i64,
}

/// The router's loaded pairs, in the listeners' order, a listener serving plain HTTP left out.
pub trait TlsPairs: Send + Sync {
    fn pairs(&self) -> Vec<PairFacts>;
}

/// What the keepalive engine stands at when a read runs: the gap counter and the last gap's instant, the three caps
/// and the pools whose policy warms caches. Behind a trait, so this crate names no keepalive type, as `TlsPairs`
/// keeps the listeners out of it.
#[derive(Debug)]
pub struct KeepaliveStanding {
    pub bus_gaps: u64,
    pub last_bus_gap_at: Option<i64>,
    /// `sessions`, `agents`, `retained`, in that order, each its count against its max.
    pub caps: Vec<KeepaliveCap>,
    /// The ids of the pools whose keepalive policy is on.
    pub warming: BTreeSet<String>,
}

#[derive(Debug)]
pub struct KeepaliveCap {
    pub name: &'static str,
    pub count: usize,
    pub max: usize,
}

pub trait KeepaliveFacts: Send + Sync {
    fn standing(&self) -> KeepaliveStanding;
}

pub struct AttentionParts {
    pub bus: Bus,
    pub clock: Clock,
    /// The feature CA, read once at the start since nothing replaces it under a running router. `None` on a router
    /// without the feature listener, and `features.ca` then has no series.
    pub feature_ca: Option<FeatureCaFacts>,
    pub keepalive: Arc<dyn KeepaliveFacts>,
    /// The ledger's degraded flag, its parked rows, the unpriced models its landing or a catalog listing saw, the
    /// routing conditions its fold wrote and the expiries between turns its rollups count. `None` where a harness
    /// builds no ledger, and then `writer_degraded` for the ledger reads 0 and none of `ledger_record_parked`,
    /// `model_unpriced`, `meter_drift`, `wall_reached`, `destination_spent`, `shed_no_runway`, `slowed_long` and
    /// `expired_between_turns` has a series.
    pub ledger: Option<Ledger>,
    /// `usage.limit_reset_warning_days`: how many days before an unused limit reset ends `limit_reset_ending` holds, 0
    /// for never.
    pub limit_reset_warning_days: u32,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    /// The router's loaded pairs, `None` where a harness builds no listeners, and then no pair has a series of either
    /// certificate kind.
    pub tls: Option<Arc<dyn TlsPairs>>,
    /// Where the poll's standing and the latest snapshot per account are read. It is `None` where a harness builds no
    /// usage service, and no account then reads a backoff and no wall names a reset.
    pub usage: Option<Arc<UsageService>>,
}

/// A read the reader could not finish. The route answers 500 and the scrape answers 500 on any of them, so no
/// condition is reported clear that was not read.
#[derive(Debug, thiserror::Error)]
pub enum AttentionError {
    /// The blocking worker that runs the registry's reads and the conditions ended without an answer, as a panic there
    /// ends it. Its own variant, since `LedgerError::ReadCanceled` names a ledger read.
    #[error("the attention read did not finish")]
    Canceled,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The one reader both the metrics listener and the admin route ask, with the request-fed state the bus arm writes.
pub struct Attention {
    clock: Clock,
    feature_ca: Option<FeatureCaFacts>,
    keepalive: Arc<dyn KeepaliveFacts>,
    ledger: Option<Ledger>,
    limit_reset_warning_days: u32,
    router: RouterHandle,
    state: Mutex<ReaderState>,
    store: Arc<Store>,
    tls: Option<Arc<dyn TlsPairs>>,
    usage: Option<Arc<UsageService>>,
}

impl Attention {
    /// Every subject of every kind, holding or clear. A failed store or ledger read fails the whole read. For a caller
    /// on the runtime: the ledger read waits for its connection there, where a wait holds no blocking worker, and the
    /// rest runs on a blocking worker, since the registry's reads are SQLite reads that stay off the runtime thread.
    pub async fn conditions(self: &Arc<Self>, now: i64) -> Result<Vec<Condition>, AttentionError> {
        // The parked rows, the fold's table and the expiries between turns share one read, so a call waits on the
        // reader pool once. The parked rows are read from the file on every call, as `refresh_failed` reads the
        // registry, so the condition holds across a restart and clears on its own when the next start or a replay
        // lands the rows. The fold's table is one lookup, since the ledger folds the routing log into it hourly and at
        // start rather than on this read. A read that fails fails the whole answer, as the refresh read's does: a
        // scrape never reports clear what it could not read.
        let ledger = match &self.ledger {
            Some(ledger) => Some(ledger.db().read(move |c| ledger_facts(c, now)).await?),
            None => None,
        };
        let attention = Arc::clone(self);
        tokio::task::spawn_blocking(move || attention.conditions_over(ledger, now))
            .await
            .map_err(|_| AttentionError::Canceled)?
    }

    /// The conditions over the ledger's parked rows, routing conditions and expiries between turns, read before this
    /// runs so a read waiting on a reader connection never holds the bus arm's `observe` behind the state lock.
    fn conditions_over(
        &self,
        ledger: Option<LedgerFacts>,
        now: i64,
    ) -> Result<Vec<Condition>, AttentionError> {
        // The registry's reads come before the lock for the same reason: a stalled registry file holds the store's
        // connection mutex, and `observe` waits on this lock.
        let accounts = self.store.accounts()?;
        let extras = self.store.account_extras()?;
        let refresh_facts = reader::refresh_facts(&self.store, &accounts)?;
        // The pairs before the lock too: their lock is the router's, which a reload holds across a whole load from
        // disk.
        let pairs = self.tls.as_ref().map(|tls| tls.pairs());
        // The engine's standing before the lock too, since it waits on the engine's own lock.
        let keepalive = self.keepalive.standing();
        let unpriced = self
            .ledger
            .as_ref()
            .map(|ledger| ledger.unpriced_served(now))
            .unwrap_or_default();
        let mut state = self.state.lock().expect(STATE_POISONED);
        reader::conditions(
            &mut state,
            &accounts,
            &refresh_facts,
            &reader::ResetFacts {
                extras: &extras,
                warning_days: self.limit_reset_warning_days,
            },
            &self.router,
            self.ledger.as_ref().is_some_and(Ledger::degraded),
            ledger.as_ref(),
            &unpriced,
            pairs.as_deref(),
            self.feature_ca.as_ref(),
            self.usage.as_deref(),
            &keepalive,
            now,
        )
    }

    /// The bus arm's entry, public so a test drives it event by event.
    pub fn observe(&self, event: &Event) {
        reader::observe(&mut self.state.lock().expect(STATE_POISONED), event);
    }

    /// The metrics listener reads the conditions through a closure, since the ledger that renders them is one of this
    /// crate's dependencies and cannot name its types. Sorted, so two scrapes list the series in one order.
    pub fn gauge_source(self: &Arc<Self>) -> AttentionGaugeSource {
        let attention = self.clone();
        Arc::new(move || {
            let attention = attention.clone();
            Box::pin(async move {
                let mut conditions = attention.conditions((attention.clock)()).await?;
                conditions.sort_by(|a, b| (a.kind, &a.subject.id).cmp(&(b.kind, &b.subject.id)));
                Ok(AttentionGauges {
                    conditions: conditions
                        .into_iter()
                        .map(|c| (c.kind.as_str(), c.subject.id, c.holds))
                        .collect(),
                })
            })
        })
    }
}

/// What a read takes from the ledger: the parked rows, the fold's routing conditions and the expiries between turns.
struct LedgerFacts {
    parked: Vec<ParkedSummary>,
    routing: Vec<RoutingCondition>,
    expired: Vec<ExpiredBetweenTurns>,
}

/// The ledger's part of a read, on one connection. The expiries span `trailing_day_hours`, the hours the keepalive
/// overview sums, so the item reads the overview's hours and counts the rows of a warming pool under a key with the
/// `keepalive` permission among them.
fn ledger_facts(c: &Connection, now: i64) -> Result<LedgerFacts, LedgerError> {
    let (from, to) = trailing_day_hours(now);
    Ok(LedgerFacts {
        parked: parked_summary(c)?,
        routing: routing_conditions(c)?,
        expired: read_expired_between_turns(c, from, to)?,
    })
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

/// The reader and its bus arm, which hands every completion to `observe`. Nothing else runs: every other condition is
/// read when a scrape or the admin route asks.
pub fn spawn(parts: AttentionParts) -> (Arc<Attention>, AttentionHandles) {
    // Best-effort: every condition is level-triggered from the newest event, so the next one decides. It takes
    // completions alone, the one kind `observe` reads, so the slot's queue and its drop count hold nothing else.
    let subscriber = parts
        .bus
        .subscribe_filtered("attention", Delivery::BestEffort, |event| {
            matches!(event, Event::RequestComplete(_))
        });
    let attention = Arc::new(Attention {
        clock: parts.clock,
        feature_ca: parts.feature_ca,
        keepalive: parts.keepalive,
        ledger: parts.ledger,
        limit_reset_warning_days: parts.limit_reset_warning_days,
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
