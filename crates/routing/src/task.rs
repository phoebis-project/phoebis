//! The router task: one loop that owns the routing state, answers selections over a channel, reloads the
//! registry snapshot on request, folds the readings and the usage the relay and the poll report, and writes state
//! through a single writer so the store sees every change in the order the loop made it.

use std::collections::{btree_map, BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use phoebis_oauth::{Clock, UsageSnapshot};
use phoebis_store::{
    AccountState, CooldownRow, PinBurn, PinKey, PinRow, RegistryLoad, RekeyWrite, RoutingLoad,
    RoutingTx, Store, StoreError,
};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::affinity::{
    AffinityChain, AffinityMap, AffinityReport, AffinityRow, ConversationLookup, ConversationMatch,
    ConversationQuery, PurgedEntry,
};
use crate::record::{
    CacheMapEntry, DecisionRecord, DecisionSummary, PinEventFacts, RefusalFacts, RefusedRequest,
    ShedRoundFacts,
};
use crate::select::{
    decide, release_reasons, Decided, MigrateReason, Refusal, SelectRequest, SelectResult,
};
use crate::state::{
    burn_folded, is_plan_window, AccountTicket, CreditsState, EvidenceSource, InFlight,
    MemberSpend, OverageEvidence, PinRef, PinUsage, RateTable, RoutingState, SharePricer,
    StateWrite, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
    SEVEN_DAY_FABLE,
};
use crate::{resolve, term, window_for, KeyRefusal, Presented, RegistrySnapshot};

/// The router loop's command queue size, which `/admin/system` reports. A report that finds the queue full waits for a
/// slot; one the closed queue refuses at shutdown is dropped and counted under its kind.
pub const COMMANDS: usize = 4_096;

/// The pool a pin of a key the snapshot no longer holds counts under: the ledger's reserved id, which this crate
/// cannot import, spelled here.
pub const UNATTRIBUTED_POOL: &str = "-";

/// Batches the state writer's channel holds; past it the loop keeps the writes in memory, coalesced by row, so a
/// stalled disk costs memory proportional to the state the caps already bound and never to the request rate.
pub const WRITER_DEPTH: usize = 1_024;

/// The state writer as the system page and the scrape read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriterStatus {
    /// Batches waiting in the writer's channel now.
    pub depth: usize,
    /// Writes held in memory because the channel was full, one per row.
    pub deferred: usize,
    /// From a write the full channel could not take until the writes held in memory are queued again.
    pub degraded: bool,
    /// Batches a database-wide failure stopped since start, a retried remainder counted again.
    pub failed_batches: u64,
}

const EVENTS: usize = 1_024;

/// How long after an invalidation the next pin for the same triple counts as a migration rather than a new
/// session. A failover re-selects within one request, well inside this.
const MIGRATION_WINDOW_MS: i64 = 60_000;

/// How far apart two readings' resets may lie and still name one reset: a head names its resets in whole seconds and
/// the poll in milliseconds, so the two sources disagree about one reset by under a second, while a window's next
/// reset lands hours after the last.
const RESET_SLACK_MS: i64 = 60_000;

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
    /// One decision as the engine made it; the ledger stores the summary as the list row and the record as the
    /// frame. Boxed so the enum stays the size of its other variants on the broadcast.
    Decision {
        summary: Box<DecisionSummary>,
        record: Box<DecisionRecord>,
        at: i64,
    },
    /// The cache-map rows a completed answer wrote, for the `cache_map` column of the decision that selected it:
    /// `seq` names that decision, `attempt` the wire attempt that completed (the decision's own, or a later pass of
    /// a retry in place, the refresh ladder or the strip retry).
    CacheMapWritten {
        request_id: String,
        seq: u32,
        attempt: u32,
        entries: Vec<CacheMapEntry>,
        at: i64,
    },
    /// A shedding round that ran on `account_id` for a request of `pool_id`: the reading that fired it as the lead
    /// over pace, the round's unit, its pins in the round's order and the marks it made.
    SheddingRound {
        account_id: String,
        pool_id: String,
        request_id: String,
        key_id: String,
        session_id: String,
        agent_id: String,
        model: String,
        round: ShedRoundFacts,
        at: i64,
    },
    /// `blocked()` answered differently before and after a credits reading landed.
    FastCreditsChanged {
        account_id: String,
        before: Option<CreditsState>,
        after: CreditsState,
        blocked_before: Option<String>,
        blocked_after: Option<String>,
        at: i64,
    },
    /// A plan window's stored reading moved to 1.0 or above from under it.
    WindowSpent {
        account_id: String,
        window: String,
        utilization: f64,
        reset: Option<i64>,
        source: WindowSource,
        source_request_id: Option<String>,
        at: i64,
    },
    /// A window's reset passed: a sample named a later reset, or none once it was read past the stored one
    /// (`observed` `sample`), or the purge tick found the clock past a stored reset with no newer sample (`clock`),
    /// once per account, window and reset.
    WindowReset {
        account_id: String,
        window: String,
        utilization_before: f64,
        reset_at: i64,
        next_reset: Option<i64>,
        observed: &'static str,
        at: i64,
    },
    /// The entries the purge removed for one root on one account, with the largest among them; `deepest.depth` is
    /// the ordinal the entry was written at, never the count removed.
    WarmPrefixExpired {
        account_id: String,
        key_id: String,
        root: String,
        model: String,
        lane: String,
        entries: u32,
        deepest: CacheMapEntry,
        cause: &'static str,
        at: i64,
    },
    /// A pool member entered its wind-down window at a tick.
    WindDownStarted {
        account_id: String,
        pool_id: String,
        ends_at: i64,
        wind_down_minutes: u32,
        end_source: &'static str,
        at: i64,
    },
    /// The writer reported an `AccountState` row its write moved to another state: the state written, the row's
    /// identity after the write (its incarnation and the revision the write bumped, `AccountRow.revision`), and the
    /// relay's cause; `registry` is written by the ledger for a store-side transition this event never names. The
    /// mirror reconciles on the identity, so a transition it observed through the registry already, or a newer one,
    /// drops this event.
    AccountStateChanged {
        account_id: String,
        to: AccountState,
        incarnation: u64,
        revision: u64,
        cause: &'static str,
        detail: Option<String>,
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
        /// The relay's verdict as its classification names it (`auth_failed`, `upstream_error`), for the event a
        /// moved row emits.
        cause: &'static str,
        /// The upstream error's message where the verdict read one.
        detail: Option<String>,
    },
    /// The ledger's standing: `degraded` from a failed batch until one lands and the spend is read again. A replacing
    /// reading.
    Accounting {
        degraded: bool,
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
        /// The request whose response head carried the reading; `None` for the usage poll's.
        source_request_id: Option<String>,
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

impl Report {
    /// Every kind, alphabetical, so a reader sees the whole set at zero.
    pub const KINDS: &'static [&'static str] = &[
        "account_state",
        "accounting",
        "affinity",
        "cooldown",
        "credits",
        "member_spend",
        "overage",
        "pin_invalidated",
        "pin_released",
        "pin_renewed",
        "sample",
        "usage",
    ];

    /// The kind's name, the label of its drop counter.
    pub fn kind(&self) -> &'static str {
        // No wildcard, so a new variant fails to compile until it is named here; `sent` expects the name in `KINDS`.
        match self {
            Report::AccountState { .. } => "account_state",
            Report::Accounting { .. } => "accounting",
            Report::Affinity(_) => "affinity",
            Report::Cooldown { .. } => "cooldown",
            Report::Credits { .. } => "credits",
            Report::MemberSpend(_) => "member_spend",
            Report::Overage { .. } => "overage",
            Report::PinInvalidated { .. } => "pin_invalidated",
            Report::PinReleased { .. } => "pin_released",
            Report::PinRenewed { .. } => "pin_renewed",
            Report::Sample { .. } => "sample",
            Report::Usage { .. } => "usage",
        }
    }
}

/// The key a replacing reading replaces under: the newest reading per key is the one that lands.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CoalesceKey {
    Accounting,
    AccountState {
        account_id: String,
        incarnation: u64,
    },
    Credits {
        account_id: String,
        incarnation: u64,
    },
    MemberSpend,
    Overage {
        account_id: String,
        incarnation: u64,
    },
    /// The window set, sorted, so a head sample over the plan windows and one over an api_key's buckets replace
    /// each other only when they read the same windows.
    Sample {
        account_id: String,
        incarnation: u64,
        windows: Vec<String>,
    },
}

/// The replacing readings the loop drains at the top of every turn, in key order, and how far the shutdown has
/// gone; both under one lock, so a reading is either taken by a drain or counted as dropped, never both.
type Pending = Arc<Mutex<PendingReadings>>;

#[derive(Default)]
struct PendingReadings {
    readings: BTreeMap<CoalesceKey, Report>,
    mailbox: Mailbox,
}

/// Where the shutdown stands for a replacing reading. `Closing` runs from the mailbox's close to the final drain,
/// which takes the map whole, so a reading inserted then needs no marker and lands; past that drain the mailbox is
/// `Closed` and a reading lands nowhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mailbox {
    #[default]
    Open,
    Closing,
    Closed,
}

const PENDING_POISONED: &str = "pending readings lock poisoned";

impl Report {
    /// The key a replacing reading waits under in the pending map; `None` for a control fact, which takes a
    /// mailbox slot of its own.
    fn coalesce_key(&self) -> Option<CoalesceKey> {
        // No wildcard, so a new variant fails to compile until it is placed on one side.
        match self {
            Report::AccountState {
                account_id,
                incarnation,
                ..
            } => Some(CoalesceKey::AccountState {
                account_id: account_id.clone(),
                incarnation: *incarnation,
            }),
            Report::Accounting { .. } => Some(CoalesceKey::Accounting),
            Report::Credits {
                account_id,
                incarnation,
                ..
            } => Some(CoalesceKey::Credits {
                account_id: account_id.clone(),
                incarnation: *incarnation,
            }),
            Report::MemberSpend(_) => Some(CoalesceKey::MemberSpend),
            Report::Overage {
                account_id,
                incarnation,
                ..
            } => Some(CoalesceKey::Overage {
                account_id: account_id.clone(),
                incarnation: *incarnation,
            }),
            Report::Sample {
                account_id,
                sample,
                incarnation,
                ..
            } => Some(CoalesceKey::Sample {
                account_id: account_id.clone(),
                incarnation: *incarnation,
                windows: sample.windows.keys().cloned().collect(),
            }),
            Report::Affinity(_)
            | Report::Cooldown { .. }
            | Report::PinInvalidated { .. }
            | Report::PinReleased { .. }
            | Report::PinRenewed { .. }
            | Report::Usage { .. } => None,
        }
    }

    /// Folds `self` into the reading pending under the same key: the evidence's own age decides, never the
    /// arrival order, since the loop may be parked while a newer reading and then an older one arrive. Two overage
    /// readings of one account folded within one loop turn reach the loop as the newer alone, so an episode that
    /// began and ended between them never reaches the event log or the hourly `overage_seconds`, and two episodes
    /// either side of a gap can read as one.
    fn merge_into(self, pending: &mut Report) {
        let replaces = match (&self, &*pending) {
            // Two samples under one key read the same windows, so the sample's own age speaks for each of them.
            (Report::Sample { sample, .. }, Report::Sample { sample: held, .. }) => {
                sample.observed_at >= held.observed_at
            }
            // A reconnect restarts the credential generation at 1 under a new lifecycle, so the lifecycle is read
            // first; under one lifecycle a stale credential's verdict never replaces a current one's.
            (
                Report::AccountState {
                    lifecycle_generation,
                    credential_generation,
                    ..
                },
                Report::AccountState {
                    lifecycle_generation: held_lifecycle,
                    credential_generation: held_credential,
                    ..
                },
            ) => (lifecycle_generation, credential_generation) >= (held_lifecycle, held_credential),
            (Report::Credits { state, .. }, Report::Credits { state: held, .. }) => {
                state.observed_at >= held.observed_at
            }
            (Report::Overage { evidence, .. }, Report::Overage { evidence: held, .. }) => {
                evidence.observed_at >= held.observed_at
            }
            // One sequential producer, the ledger writer, sends both, so the later arrival is the later reading.
            (Report::Accounting { .. }, Report::Accounting { .. })
            | (Report::MemberSpend(_), Report::MemberSpend(_)) => true,
            (incoming, held) => unreachable!(
                "a key names one kind, yet a `{}` reading met a pending `{}`",
                incoming.kind(),
                held.kind()
            ),
        };
        if replaces {
            *pending = self;
        } else {
            tracing::debug!(
                kind = self.kind(),
                "a reading older than the one pending under its key is dropped"
            );
        }
    }
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
    AccountingDegraded {
        reply: oneshot::Sender<bool>,
    },
    Canonicals {
        models: Vec<String>,
        reply: oneshot::Sender<Vec<String>>,
    },
    ClearCooldown {
        account_id: String,
        scope: String,
        reply: oneshot::Sender<bool>,
    },
    /// A replacing reading landed in the pending map; the loop drains the map on every turn whether or not the
    /// marker reached it.
    Coalesced,
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
    PinsByPool {
        reply: oneshot::Sender<Vec<(String, u64)>>,
    },
    /// A request the relay refused before asking for a selection; the loop records it as the request's decision.
    RecordRefusal {
        request: RefusedRequest,
        refusal: RefusalFacts,
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
    Park {
        release: oneshot::Receiver<()>,
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
    /// The account-state rows a writer wake moved, sent ahead of the reload the move asks for.
    StateMoved {
        rows: Vec<MovedState>,
    },
    Select {
        request: SelectRequest,
        reply: oneshot::Sender<SelectAnswer>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    /// A finished registry load, sent back by the task that ran it; the loop applies the swap.
    Swap {
        loaded: Result<(RegistryLoad, [u8; 32]), String>,
        started_at: i64,
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

/// The state writer's figures: the loop moves `deferred` and `degraded` and counts a batch into `depth`, the writer
/// counts it out and counts `failed_batches`.
#[derive(Default)]
struct WriterShared {
    deferred: AtomicUsize,
    degraded: AtomicBool,
    depth: AtomicUsize,
    failed_batches: AtomicU64,
}

/// What the routing task loaded from the registry at start, for the ledger's `router_started` row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadCounts {
    pub pins: usize,
    /// Accounts with a reading, whatever the number of windows each carries.
    pub windows: usize,
    pub affinity_entries: usize,
}

impl LoadCounts {
    fn of(load: &RoutingLoad) -> LoadCounts {
        LoadCounts {
            pins: load.pins.len(),
            windows: load
                .windows
                .iter()
                .map(|row| row.account_id.as_str())
                .collect::<HashSet<_>>()
                .len(),
            affinity_entries: load.affinity.len(),
        }
    }
}

#[derive(Clone)]
pub struct RouterHandle {
    affinity: Arc<RwLock<AffinityMap>>,
    /// The reports the closed queue refused, one counter per kind at its position in `Report::KINDS`; the
    /// process's alone, zero at start.
    dropped: Arc<[AtomicU64; Report::KINDS.len()]>,
    /// The refusal records a full or closed mailbox refused; the process's alone, zero at start.
    dropped_refusals: Arc<AtomicU64>,
    events: broadcast::Sender<RouterEvent>,
    in_flight: InFlight,
    load_counts: LoadCounts,
    /// A receiver opened before the loop's first turn, taken by `subscribe_from_start`.
    start_events: Arc<Mutex<Option<broadcast::Receiver<RouterEvent>>>>,
    /// The replacing readings the loop has not drained yet, shared with the task.
    pending: Pending,
    reports: Arc<ReportSequence>,
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    tx: mpsc::Sender<Command>,
    /// The state writer's figures, kept by the loop and the writer.
    writer: Arc<WriterShared>,
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

    pub fn load_counts(&self) -> LoadCounts {
        self.load_counts
    }

    /// What changed in the routing state, as it changes. A receiver that falls behind loses the oldest
    /// events and is told so; the task never waits on a receiver.
    pub fn subscribe_events(&self) -> broadcast::Receiver<RouterEvent> {
        self.events.subscribe()
    }

    /// `subscribe_events` for the one subscriber that must also see what the loop broadcast before its spawner could
    /// subscribe: the purge tick fires at once, so the first purge after a start announces the clock resets and the
    /// wind-downs of the downtime ahead of every other subscriber. The first call takes the receiver opened before
    /// the loop's first turn; a later one subscribes now.
    pub fn subscribe_from_start(&self) -> broadcast::Receiver<RouterEvent> {
        self.start_events
            .lock()
            .expect("start receiver lock poisoned")
            .take()
            .unwrap_or_else(|| self.events.subscribe())
    }

    pub async fn select(&self, request: SelectRequest) -> Result<SelectAnswer, RouterGone> {
        self.ask(|reply| Command::Select { request, reply }).await
    }

    /// A request the relay refused before asking for a selection, recorded as the request's one decision: sends
    /// `Command::RecordRefusal { request, refusal }` through `try_send` and never waits for mailbox room, so the
    /// 400 toward the client is not delayed by a full router queue; a full or closed mailbox drops the record,
    /// counted in `dropped_refusals` beside a warn. The loop's arm builds `DecisionRecord::refused` on its snapshot
    /// and emits the `Decision`.
    pub fn record_refusal(&self, request: RefusedRequest, refusal: RefusalFacts) {
        let request_id = request.request_id.clone();
        if self
            .tx
            .try_send(Command::RecordRefusal { request, refusal })
            .is_err()
        {
            self.dropped_refusals.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                request = %request_id,
                "the router's mailbox could not take a refusal's record, which is dropped"
            );
        }
    }

    /// The refusal records a full or closed mailbox refused, beside `dropped_reports`; the process's alone, zero at
    /// start.
    pub fn dropped_refusals(&self) -> u64 {
        self.dropped_refusals.load(Ordering::Relaxed)
    }

    /// Waits for a mailbox slot: a report is a fact about the past that must land. The relay's end-of-answer
    /// reports await after the client's answer, so a slow loop holds the request's permit and never the client;
    /// its pre-dial ones (a failover report between legs, `touch_key`) await before selection, where no client
    /// byte has gone out yet. A replacing reading (a sample, an overage or credits state, an account state, the
    /// member spends, the ledger's accounting) is written into the pending map under its key and a wake-up marker
    /// try-sent: the newest reading always lands and the producer never waits. `Some(seq)` for an `Affinity`
    /// report's number, the sequence taken once the slot is held so numbers enter the queue in order; `None`
    /// otherwise, and for a report the closed channel refused at shutdown, the one drop left and the one the
    /// counters still count.
    pub async fn report(&self, report: Report) -> Option<u64> {
        let report = self.pend(report)?;
        match self.tx.reserve().await {
            Ok(permit) => self.through(permit, report),
            Err(_) => {
                self.count_drop(report.kind());
                None
            }
        }
    }

    /// `report` without the wait: a control fact takes a mailbox slot only if one is free now, else comes back
    /// as `Err` for the caller to await with `report` once nothing waits on it. A replacing reading never waits,
    /// so it is always `Ok`.
    pub fn try_report(&self, report: Report) -> Result<Option<u64>, Box<Report>> {
        let Some(report) = self.pend(report) else {
            return Ok(None);
        };
        match self.tx.try_reserve() {
            Ok(permit) => Ok(self.through(permit, report)),
            Err(mpsc::error::TrySendError::Full(())) => Err(Box::new(report)),
            Err(mpsc::error::TrySendError::Closed(())) => {
                self.count_drop(report.kind());
                Ok(None)
            }
        }
    }

    /// Folds a replacing reading into the pending map and answers `None`; hands a control fact back.
    fn pend(&self, report: Report) -> Option<Report> {
        let kind = report.kind();
        // The kind's counter is found only on a drop, so a kind missing from the list would otherwise pass every
        // test that never stops the task.
        debug_assert!(
            Report::KINDS.contains(&kind),
            "the report kind `{kind}` is missing from Report::KINDS"
        );
        let Some(key) = report.coalesce_key() else {
            return Some(report);
        };
        let dropped = {
            let mut pending = self.pending.lock().expect(PENDING_POISONED);
            match pending.mailbox {
                Mailbox::Closed => true,
                mailbox => {
                    let inserted = match pending.readings.entry(key) {
                        btree_map::Entry::Occupied(mut held) => {
                            report.merge_into(held.get_mut());
                            false
                        }
                        btree_map::Entry::Vacant(slot) => {
                            slot.insert(report);
                            true
                        }
                    };
                    // One marker per key: a reading that replaced a pending one is covered by that one's marker, or
                    // by the busy loop's next drain when the full mailbox refused it. Sent under the lock, so the
                    // shutdown's close cannot fall between the state read and the send. While `Closing`, the final
                    // drain takes the reading; an open mailbox that is closed anyway lost its loop without that
                    // drain.
                    mailbox == Mailbox::Open
                        && if inserted {
                            matches!(
                                self.tx.try_send(Command::Coalesced),
                                Err(mpsc::error::TrySendError::Closed(_))
                            )
                        } else {
                            self.tx.is_closed()
                        }
                }
            }
        };
        if dropped {
            self.count_drop(kind);
        }
        None
    }

    /// Sends a control fact through a held slot. An affinity report takes its number under the sequence lock
    /// with the send, so a later number never enters the queue ahead of an earlier one and the watermark the task
    /// publishes in queue order covers every number below it.
    fn through(&self, permit: mpsc::Permit<'_, Command>, report: Report) -> Option<u64> {
        if !matches!(report, Report::Affinity(_)) {
            permit.send(Command::Report(report, None));
            return None;
        }
        let mut next = self
            .reports
            .next
            .lock()
            .expect("report sequence lock poisoned");
        let seq = *next + 1;
        permit.send(Command::Report(report, Some(seq)));
        *next = seq;
        Some(seq)
    }

    fn count_drop(&self, kind: &'static str) {
        let index = Report::KINDS
            .iter()
            .position(|k| *k == kind)
            .expect("every kind is listed");
        self.dropped[index].fetch_add(1, Ordering::Relaxed);
        tracing::warn!(kind, "the router task has stopped, a report was dropped");
    }

    /// The reports the closed queue refused at shutdown, per kind in `Report::KINDS` order; never persisted.
    pub fn dropped_reports(&self) -> Vec<(&'static str, u64)> {
        Report::KINDS
            .iter()
            .zip(self.dropped.iter())
            .map(|(kind, count)| (*kind, count.load(Ordering::Relaxed)))
            .collect()
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

    /// Loads the registry again and answers once the new snapshot is in force. The load runs beside the loop and
    /// is shared: a reload asked while one runs waits for one more, since the write it follows may postdate the
    /// running load's read. A failed load leaves the previous snapshot in force and answers `Load`; a reload still
    /// waiting when the router shuts down answers `Gone`, since the shutdown drops its load's answer with every
    /// other command queued behind it.
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
    /// Whether the router loop still receives commands; false once the task ended, which is the condition every
    /// `ask` reports as `RouterGone`.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
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
    /// The live pins counted per pool by the loop itself, sorted by pool: the metrics scrape reads the counts
    /// rather than a copy of the pin table.
    pub async fn pins_by_pool(&self) -> Result<Vec<(String, u64)>, RouterGone> {
        self.ask(|reply| Command::PinsByPool { reply }).await
    }
    pub async fn cooldowns(&self) -> Result<Vec<CooldownRow>, RouterGone> {
        self.ask(|reply| Command::Cooldowns { reply }).await
    }
    /// Every budgeted member's spend as the ledger last reported it, from the state's table alone: the
    /// analytics route reads this rather than querying the ledger a second time.
    pub async fn member_spends(&self) -> Result<Vec<MemberSpend>, RouterGone> {
        self.ask(|reply| Command::MemberSpends { reply }).await
    }
    /// Whether the selector treats every budgeted member's spend as unknown, from the ledger's last
    /// `Report::Accounting`.
    pub async fn accounting_degraded(&self) -> Result<bool, RouterGone> {
        self.ask(|reply| Command::AccountingDegraded { reply })
            .await
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
    /// rotation stamps nothing on the new grace. Waits for a slot as a report does; a stopped task takes none.
    pub async fn touch_key(&self, key_id: &str, presented: Presented, rotated_at: Option<i64>) {
        let _ = self
            .tx
            .send(Command::Touch {
                key_id: key_id.to_string(),
                presented,
                rotated_at,
            })
            .await;
    }

    /// Commands waiting in the mailbox now: the figure that reads a wedged loop.
    pub fn queued(&self) -> usize {
        COMMANDS - self.tx.capacity()
    }

    pub fn writer_status(&self) -> WriterStatus {
        WriterStatus {
            depth: self.writer.depth.load(Ordering::Relaxed),
            deferred: self.writer.deferred.load(Ordering::Relaxed),
            degraded: self.writer.degraded.load(Ordering::Relaxed),
            failed_batches: self.writer.failed_batches.load(Ordering::Relaxed),
        }
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
/// carries no way to stall the task's reports or to put an event of its own on the task's broadcast.
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

    /// Parks the loop before its next command until the guard drops, so a test can fill the queue.
    pub async fn park_loop(&self) -> LoopPark {
        let (release, parked) = oneshot::channel();
        // Answered once the loop has taken the park off the queue, so the parked command holds no slot.
        self.ask(|reply| Command::Park {
            release: parked,
            reply,
        })
        .await
        .expect("the router task is running");
        LoopPark {
            release: Some(release),
        }
    }

    /// Puts `event` on the broadcast as the loop puts its own, so a test can order what a subscriber receives
    /// against a store write the loop never made.
    pub fn emit(&self, event: RouterEvent) {
        let _ = self.events.send(event);
    }
}

/// The router loop parked from `RouterHandle::park_loop` until this drops.
#[cfg(any(test, feature = "debug-hooks"))]
pub struct LoopPark {
    release: Option<oneshot::Sender<()>>,
}

#[cfg(any(test, feature = "debug-hooks"))]
impl Drop for LoopPark {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
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
    /// The writes the full channel could not take; while it holds any, every new write folds into it.
    deferred: Mutex<Deferred>,
    events: broadcast::Sender<RouterEvent>,
    /// The loop's own way back into its mailbox, weak so it never keeps its receiver alive: upgraded for the one
    /// load a task runs, and dropped with that task.
    handle_tx: mpsc::WeakSender<Command>,
    /// The account a pin was invalidated from, why, and when. The next select for that same key
    /// within `MIGRATION_WINDOW_MS` reports a migration rather than a fresh pin; older entries go on purge.
    invalidated: HashMap<PinKey, (String, &'static str, i64)>,
    /// The reloads the running load answers: those that were waiting when it started.
    load_waiters: Vec<oneshot::Sender<Result<(), String>>>,
    /// Whether a registry load runs; at most one does.
    loading: bool,
    /// The affinity reports a test's hold keeps unapplied, in queue order; `None` while no hold is on.
    #[cfg(any(test, feature = "debug-hooks"))]
    parked: Option<Vec<(Report, u64)>>,
    pending: Pending,
    /// The reloads that arrived while a load ran, which one more load answers: the write each follows may postdate
    /// the running load's read.
    reload_waiters: Vec<oneshot::Sender<Result<(), String>>>,
    reports: Arc<ReportSequence>,
    /// The reset last announced per account and window, by a sample or by the clock, so one passing reset is
    /// announced once whichever saw it first.
    reset_announced: HashMap<(String, String), i64>,
    revision: u64,
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    state: RoutingState,
    store: Arc<Store>,
    /// The rotation each slot's last touch was stamped under, beside its timestamp: a touch under a different
    /// rotation is a first sight of that presentation, not a repeat the grace period should swallow.
    touched_at: HashMap<(String, Presented), (i64, Option<i64>)>,
    /// The pool members inside their wind-down at the last purge, by pool and account: a member's start is the
    /// tick that first finds it there.
    winding: HashSet<(String, String)>,
    writer: mpsc::Sender<Vec<StateWrite>>,
    /// Resolves once the writer has landed everything it was sent and exited; `shutdown` waits on it.
    writer_done: Option<oneshot::Receiver<()>>,
    writer_status: Arc<WriterShared>,
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
    let load = store.load_routing()?;
    let load_counts = LoadCounts::of(&load);
    let mut state = RoutingState::from_load(load);
    // One table, shared by the handle (the relay's synchronous permits and tickets) and the state (the
    // selector's cap partition), so a count is exact wherever it is read.
    let in_flight = InFlight::default();
    state.set_in_flight(in_flight.clone());
    state.set_rates(config.rates.clone());
    // Shared the same way, so the relay's conversation climb reads the map under the same lock the report
    // handler writes, without a command round trip.
    let affinity = state.affinity();
    let (tx, mut rx) = mpsc::channel(COMMANDS);
    // Bounded so a stalled store cannot grow the channel without limit; past the bound the loop keeps the writes in
    // `deferred`, coalesced by row, which the state's own caps bound.
    let (writer, mut writes) = mpsc::channel::<Vec<StateWrite>>(WRITER_DEPTH);
    let (done_tx, done_rx) = oneshot::channel();
    let (events, start_events) = broadcast::channel(EVENTS);
    let reports = Arc::new(ReportSequence::default());
    let pending = Pending::default();
    let writer_status = Arc::new(WriterShared::default());
    let handle = RouterHandle {
        affinity,
        dropped: Arc::default(),
        dropped_refusals: Arc::default(),
        events: events.clone(),
        in_flight,
        load_counts,
        start_events: Arc::new(Mutex::new(Some(start_events))),
        pending: pending.clone(),
        reports: reports.clone(),
        snapshot: snapshot.clone(),
        tx: tx.clone(),
        writer: writer_status.clone(),
    };
    let writer_store = store.clone();
    let writer_figures = writer_status.clone();
    // Weak: a strong sender would keep the loop running after the last handle is gone, and with it the process
    // lock a reopened on-disk store needs back.
    let writer_tx = tx.downgrade();
    tokio::spawn(async move {
        let mut writer = Writer {
            store: writer_store,
            figures: writer_figures,
            rekey: None,
            remainder: None,
        };
        // One batch at a time, in order: two blocking writes racing on separate threads could land a delete
        // before the upsert it was meant to follow.
        while let Some(first) = writes.recv().await {
            writer.figures.depth.fetch_sub(1, Ordering::Relaxed);
            // Everything queued since the last wake lands in one commit, so the registry pays one commit per
            // wake, and wakes coalesce under load.
            let mut batch = first;
            while let Ok(more) = writes.try_recv() {
                writer.figures.depth.fetch_sub(1, Ordering::Relaxed);
                batch.extend(more);
            }
            // An account-state mark that landed changes what the snapshot has to say, so the loop is asked for a
            // reload once the write is on disk rather than before; the rows whose state it moved go first, so their
            // events leave ahead of the snapshot that shows them.
            let marks = writer.wake(batch).await;
            if marks.landed {
                if let Some(tx) = writer_tx.upgrade() {
                    if !marks.moved.is_empty() {
                        let _ = tx.send(Command::StateMoved { rows: marks.moved }).await;
                    }
                    let (reply, _rx) = oneshot::channel();
                    let _ = tx.send(Command::Reload { reply }).await;
                }
            }
        }
        writer.retry().await;
        let _ = done_tx.send(());
    });
    let mut task = Task {
        cancel: HashMap::new(),
        clock,
        config,
        deferred: Mutex::default(),
        events,
        handle_tx: tx.downgrade(),
        invalidated: HashMap::new(),
        load_waiters: Vec::new(),
        loading: false,
        #[cfg(any(test, feature = "debug-hooks"))]
        parked: None,
        pending,
        reload_waiters: Vec::new(),
        reports,
        reset_announced: HashMap::new(),
        revision: 1,
        snapshot,
        state,
        store,
        touched_at: HashMap::new(),
        winding: HashSet::new(),
        writer,
        writer_done: Some(done_rx),
        writer_status,
    };
    drop(tx);
    tokio::spawn(async move {
        let mut purge = tokio::time::interval(task.config.purge_interval);
        purge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            task.offer_deferred();
            task.drain_pending();
            tokio::select! {
                command = rx.recv() => match command {
                    Some(Command::Shutdown { reply }) => {
                        // Nothing queued behind the shutdown is served: the handles that sent it are done.
                        close_mailbox(&task.pending, &mut rx);
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
                                // The mailbox took it, so it is emitted rather than lost uncounted.
                                Command::RecordRefusal { request, refusal } => {
                                    task.record_refusal(&request, &refusal);
                                }
                                Command::StateMoved { rows } => task.state_moved(rows),
                                _ => {}
                            }
                        }
                        // A reading inserted after the last turn's drain lands as a queued report does.
                        task.final_drain();
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
                    None => {
                        task.final_drain();
                        task.drain_writer().await;
                        break;
                    }
                },
                _ = purge.tick() => task.purge(),
                // A slot the writer frees takes the backlog at the top of the next turn, so a store that recovers
                // clears `degraded` without waiting on a command or the purge tick.
                _ = task.writer_room(), if task.has_deferred() => {}
            }
        }
    });
    Ok(handle)
}

/// What one wake landed, all of it routing state the store commits at `NORMAL`: a power loss may take the last of
/// it, which the next requests rebuild, and an operator's cooldown clear or session release among it is made again.
/// `refused` is the first refusal's error and `skipped` how many writes the store refused on their own; `stopped` is
/// the database-wide failure that gave up the rest of the wake, `left` how many writes it left unapplied, counting the
/// one it met, and `remainder` those writes in order, rekeys among them; `rekey` is the refused rekey the writer tries
/// once more.
struct Landed {
    marks: Marks,
    refused: Option<StoreError>,
    skipped: usize,
    stopped: Option<StoreError>,
    left: usize,
    rekey: Option<Box<RekeyWrite>>,
    remainder: Vec<StateWrite>,
}

/// An account-state row a writer wake moved, for the loop's `AccountStateChanged`: the state written, the row's
/// identity after the write and the verdict's cause.
struct MovedState {
    account_id: String,
    state: AccountState,
    incarnation: u64,
    revision: u64,
    cause: &'static str,
    detail: Option<String>,
}

/// What a wake's account-state writes did: whether any passed its fences, which the snapshot is rebuilt from even
/// when the state was already the one written, and the rows whose state moved, which alone are transitions.
#[derive(Default)]
struct Marks {
    landed: bool,
    moved: Vec<MovedState>,
}

impl Marks {
    fn absorb(&mut self, other: Marks) {
        self.landed |= other.landed;
        self.moved.extend(other.moved);
    }

    /// One state write's marks from the store's answer: the row's revision after it and the state before it, `None`
    /// when the fences refused it.
    fn of(write: &StateWrite, answer: Option<(u64, AccountState)>) -> Marks {
        let (
            StateWrite::AccountState {
                account_id,
                state,
                incarnation,
                cause,
                detail,
                ..
            },
            Some((revision, before)),
        ) = (write, answer)
        else {
            return Marks::default();
        };
        Marks {
            landed: true,
            moved: (before != *state)
                .then(|| MovedState {
                    account_id: account_id.clone(),
                    state: *state,
                    incarnation: *incarnation,
                    revision,
                    cause,
                    detail: detail.clone(),
                })
                .into_iter()
                .collect(),
        }
    }
}

/// The writer task's own state between wakes.
struct Writer {
    store: Arc<Store>,
    figures: Arc<WriterShared>,
    /// The memory has already moved when a rekey fails, and only the next landing of the same spelling would
    /// write it again; one more try at the next wake covers a passing failure without letting a write the store
    /// keeps refusing hold up every later one.
    rekey: Option<Box<RekeyWrite>>,
    /// The writes a database-wide failure left unapplied, in order: tried once more at the next wake, ahead of
    /// anything newer, which the purge tick's write brings within `purge_interval` at the latest; a second
    /// database-wide failure drops them with the error line.
    remainder: Option<Vec<StateWrite>>,
}

impl Writer {
    /// Lands what an earlier wake left to try again, then `batch`; answers what their account-state marks did.
    async fn wake(&mut self, batch: Vec<StateWrite>) -> Marks {
        let mut marks = self.retry().await;
        marks.absorb(self.land(batch, false).await);
        marks
    }

    /// The refused rekey's one more try, then the unapplied remainder's.
    async fn retry(&mut self) -> Marks {
        if let Some(rekey) = self.rekey.take() {
            retry_rekey(&self.store, rekey).await;
        }
        match self.remainder.take() {
            Some(remainder) => self.land(remainder, true).await,
            None => Marks::default(),
        }
    }

    /// `retried` is whether `batch` is an earlier wake's remainder, which a second failure drops.
    async fn land(&mut self, batch: Vec<StateWrite>, retried: bool) -> Marks {
        let store = self.store.clone();
        let size = batch.len();
        match tokio::task::spawn_blocking(move || land(&store, batch)).await {
            Ok(Landed {
                marks,
                refused,
                skipped,
                stopped,
                left,
                rekey,
                remainder,
            }) => {
                if let Some(stopped) = stopped {
                    self.figures.failed_batches.fetch_add(1, Ordering::Relaxed);
                    if retried {
                        tracing::error!(
                            error = %stopped,
                            refused = refused.as_ref().map(tracing::field::display),
                            batch = size,
                            skipped,
                            left,
                            "a database-wide failure left {left} of {size} retried routing state writes \
                             unapplied, after {skipped} refused; they are dropped"
                        );
                    } else {
                        tracing::error!(
                            error = %stopped,
                            refused = refused.as_ref().map(tracing::field::display),
                            batch = size,
                            skipped,
                            left,
                            "a database-wide failure left {left} of {size} routing state writes unapplied, \
                             after {skipped} refused; they are tried once more at the next wake"
                        );
                        self.remainder = Some(remainder);
                    }
                } else if let Some(refused) = refused {
                    tracing::error!(
                        error = %refused,
                        batch = size,
                        skipped,
                        "{skipped} of {size} routing state writes failed and were skipped"
                    );
                }
                if let Some(rekey) = rekey {
                    // A wake lands its remainder and then its batch, and keeps one refused rekey to retry: the
                    // newer, which for one spelling is the latest resolution.
                    if let Some(replaced) = self.rekey.replace(rekey) {
                        tracing::warn!(
                            spelling = %replaced.resolution.spelling,
                            dated_id = %replaced.resolution.dated_id,
                            "a refused model resolution's rekey is dropped: a later one of this wake replaces it"
                        );
                    }
                }
                marks
            }
            Err(error) => {
                tracing::error!(%error, batch = size, "a routing state write panicked, its batch is lost");
                Marks::default()
            }
        }
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

/// One transaction for the wake's batch. A refusal rolls the whole of it back, and the batch is replayed one write
/// per commit, so the refused write is the only one lost.
fn land(store: &Store, batch: Vec<StateWrite>) -> Landed {
    let size = batch.len();
    match store.write_routing(|tx| apply_in(tx, &batch)) {
        Ok(marks) => Landed {
            marks,
            refused: None,
            skipped: 0,
            stopped: None,
            left: 0,
            rekey: None,
            remainder: Vec::new(),
        },
        Err(error) => {
            let landed = replay(store, batch);
            // A replay that met no refusal says nothing about why the transaction failed: an outside holder of
            // the file's lock or a disk that failed once.
            if landed.refused.is_none() && landed.stopped.is_none() {
                tracing::warn!(
                    %error,
                    batch = size,
                    "a wake's routing state transaction failed and its replay landed every write"
                );
            }
            landed
        }
    }
}

fn replay(store: &Store, batch: Vec<StateWrite>) -> Landed {
    let size = batch.len();
    let mut marks = Marks::default();
    let mut refused = None;
    let mut skipped = 0;
    let mut stopped = None;
    let mut left = 0;
    let mut refused_rekeys = Vec::new();
    let mut remainder = Vec::new();
    let mut writes = batch.into_iter().enumerate();
    while let Some((index, write)) = writes.next() {
        match apply_one(store, &write) {
            Ok(write_marks) => {
                marks.absorb(write_marks);
                // The rekey's upsert carries no generation check, so retrying a refused one after a later rekey
                // of its spelling landed would put the older mapping back and restore the rows the later one moved.
                if let StateWrite::Rekey(landed) = &write {
                    let spelling = &landed.resolution.spelling;
                    refused_rekeys.retain(|refused: &RekeyWrite| {
                        let superseded = &refused.resolution.spelling == spelling;
                        if superseded {
                            tracing::info!(
                                %spelling,
                                "a refused model resolution's rekey is dropped: a later one of its spelling landed"
                            );
                        }
                        !superseded
                    });
                }
            }
            // Every write after this one would wait out the same lock or meet the same disk, so the rest of the
            // wake is given up at once and handed back whole, its rekeys in their places, for the writer's one
            // more try; a rekey among them joins no retry of its own, or it would be tried twice.
            Err(failure) if failure.is_database_wide() => {
                left = size - index;
                stopped = Some(failure);
                remainder.extend(std::iter::once(write).chain(writes.map(|(_, write)| write)));
                break;
            }
            Err(refusal) => {
                skipped += 1;
                refused.get_or_insert(refusal);
                if let StateWrite::Rekey(rekey) = write {
                    refuse_rekey(&mut refused_rekeys, rekey);
                }
            }
        }
    }
    Landed {
        marks,
        refused,
        skipped,
        stopped,
        left,
        // One retry per wake, as the writer's `rekey` holds: the first refused rekey still standing; any after it
        // is dropped.
        rekey: refused_rekeys.into_iter().next().map(Box::new),
        remainder,
    }
}

/// The newest refused rekey of a spelling is the latest resolution, so it replaces an earlier refused one rather
/// than queueing behind it.
fn refuse_rekey(refused_rekeys: &mut Vec<RekeyWrite>, rekey: RekeyWrite) {
    refused_rekeys.retain(|earlier| earlier.resolution.spelling != rekey.resolution.spelling);
    refused_rekeys.push(rekey);
}

/// Reports what the batch's account-state marks did, the one write here that the registry snapshot has to be
/// rebuilt from.
fn apply_in(tx: &mut RoutingTx<'_>, batch: &[StateWrite]) -> Result<Marks, StoreError> {
    let mut marks = Marks::default();
    for write in batch {
        match write {
            StateWrite::AccountState {
                account_id,
                state,
                incarnation,
                lifecycle_generation,
                credential_generation,
                ..
            } => {
                let answer = tx.set_state_if_generations(
                    account_id,
                    *state,
                    *incarnation,
                    *lifecycle_generation,
                    *credential_generation,
                )?;
                marks.absorb(Marks::of(write, answer));
            }
            StateWrite::DeleteAffinity(keys) => {
                tx.delete_affinity(keys)?;
            }
            StateWrite::DeleteCooldown(account_id, scope) => {
                tx.delete_cooldown(account_id, scope)?;
            }
            StateWrite::DeletePin(key) => {
                tx.delete_pin(key)?;
            }
            StateWrite::PurgeStaleWindows => {
                tx.purge_stale_windows()?;
            }
            StateWrite::Rekey(rekey) => tx.rekey_resolution(rekey)?,
            StateWrite::UpsertAffinity(rows) => tx.upsert_affinity(rows)?,
            StateWrite::UpsertCooldown(row) => tx.upsert_cooldown(row)?,
            StateWrite::UpsertCredits(row) => {
                tx.upsert_credits(row)?;
            }
            StateWrite::UpsertOverage(row) => {
                tx.upsert_overage(row)?;
            }
            StateWrite::UpsertPin(row) => tx.upsert_pin(row)?,
            StateWrite::UpsertWindow(row) => {
                tx.upsert_window(row)?;
            }
        }
    }
    Ok(marks)
}

/// One write through the store's own method for it, its own commit; answers what an account-state mark did.
fn apply_one(store: &Store, write: &StateWrite) -> Result<Marks, StoreError> {
    let none = Marks::default;
    match write {
        StateWrite::AccountState {
            account_id,
            state,
            incarnation,
            lifecycle_generation,
            credential_generation,
            ..
        } => store
            .set_state_if_generations(
                account_id,
                *state,
                *incarnation,
                *lifecycle_generation,
                *credential_generation,
            )
            .map(|answer| Marks::of(write, answer)),
        StateWrite::DeleteAffinity(keys) => store.delete_affinity(keys).map(|_| none()),
        StateWrite::DeleteCooldown(account_id, scope) => {
            store.delete_cooldown(account_id, scope).map(|_| none())
        }
        StateWrite::DeletePin(key) => store.delete_pin(key).map(|_| none()),
        StateWrite::PurgeStaleWindows => store.purge_stale_windows().map(|_| none()),
        StateWrite::Rekey(rekey) => store.rekey_resolution(rekey).map(|()| none()),
        StateWrite::UpsertAffinity(rows) => store.upsert_affinity(rows).map(|()| none()),
        StateWrite::UpsertCooldown(row) => store.upsert_cooldown(row).map(|()| none()),
        StateWrite::UpsertCredits(row) => store.upsert_credits(row).map(|_| none()),
        StateWrite::UpsertOverage(row) => store.upsert_overage(row).map(|_| none()),
        StateWrite::UpsertPin(row) => store.upsert_pin(row).map(|()| none()),
        StateWrite::UpsertWindow(row) => store.upsert_window(row).map(|_| none()),
    }
}

/// Closes the mailbox under the pending lock, so a reading pended from here on waits for the final drain rather than
/// sending a marker the closed mailbox would refuse.
fn close_mailbox(pending: &Pending, rx: &mut mpsc::Receiver<Command>) {
    let mut pending = pending.lock().expect(PENDING_POISONED);
    pending.mailbox = Mailbox::Closing;
    rx.close();
}

/// The pending map's last take: a reading after it lands nowhere and counts as dropped.
fn take_final(pending: &Pending) -> BTreeMap<CoalesceKey, Report> {
    let mut pending = pending.lock().expect(PENDING_POISONED);
    pending.mailbox = Mailbox::Closed;
    std::mem::take(&mut pending.readings)
}

/// The rows a completed answer wrote, for the decision that selected it. `record` answers its rows in chain order,
/// so one pass over each chain finds every row's depth.
fn cache_map_written(report: &AffinityReport, rows: &[AffinityRow]) -> RouterEvent {
    let mut message = report.message_boundaries.iter().enumerate();
    let mut static_ = report.static_boundaries.iter().enumerate();
    let entries = rows
        .iter()
        .map(|row| {
            let chain = match row.chain {
                AffinityChain::Message => &mut message,
                AffinityChain::Static => &mut static_,
            };
            let depth = chain
                .find(|(_, hash)| **hash == row.hash)
                .map_or(0, |(index, _)| u32::try_from(index + 1).unwrap_or(u32::MAX));
            CacheMapEntry {
                tier: row.chain.as_str(),
                depth,
                prefix_tokens: row.prefix_tokens,
                ttl_ms: row.ttl_ms,
                account_id: row.account_id.clone(),
            }
        })
        .collect();
    RouterEvent::CacheMapWritten {
        request_id: report.request_id.clone(),
        seq: report.seq,
        attempt: report.attempt,
        entries,
        at: report.now,
    }
}

/// A pin event as the decision names it: a move with its reason, an expiry with the first reason the task read
/// against the pin.
fn pin_event_facts(event: &RouterEvent) -> Option<PinEventFacts> {
    let (kind, reason) = match event {
        RouterEvent::PinCreated { .. } => ("pin_created", None),
        RouterEvent::PinMigrated { reason, .. } => ("pin_migrated", Some(reason.clone())),
        RouterEvent::PinExpired { reasons, .. } => {
            ("pin_expired", reasons.first().map(|r| r.to_string()))
        }
        _ => return None,
    };
    Some(PinEventFacts { kind, reason })
}

impl Task {
    /// A send with no receiver is an error and means nothing: the loop must never depend on anyone listening.
    fn emit(&self, event: RouterEvent) {
        let _ = self.events.send(event);
    }

    /// The decision of a request the relay refused before asking for a selection, built on the current snapshot.
    fn record_refusal(&self, request: &RefusedRequest, refusal: &RefusalFacts) {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock poisoned")
            .clone();
        let (record, summary) = DecisionRecord::refused(request, &snapshot, refusal);
        self.emit(RouterEvent::Decision {
            summary: Box::new(summary),
            record: Box::new(record),
            at: request.at,
        });
    }

    /// One transition per account-state row the writer moved, with the verdict's cause.
    fn state_moved(&self, rows: Vec<MovedState>) {
        let at = (self.clock)();
        for row in rows {
            self.emit(RouterEvent::AccountStateChanged {
                account_id: row.account_id,
                to: row.state,
                incarnation: row.incarnation,
                revision: row.revision,
                cause: row.cause,
                detail: row.detail,
                at,
            });
        }
    }

    /// Answers a request the current registry refuses before the engine runs, its decision emitted first.
    fn refuse(
        &self,
        request: &SelectRequest,
        snapshot: &RegistrySnapshot,
        refusal: Refusal,
        reply: oneshot::Sender<SelectAnswer>,
    ) {
        let reason = match &refusal {
            Refusal::Key(refusal) => refusal.to_string(),
            Refusal::Resolve(refusal) => refusal.to_string(),
        };
        let (record, summary) = DecisionRecord::refused(
            &RefusedRequest::from(request),
            snapshot,
            &RefusalFacts {
                kind: "key",
                reason,
            },
        );
        self.emit(RouterEvent::Decision {
            summary: Box::new(summary),
            record: Box::new(record),
            at: request.now,
        });
        let _ = reply.send(SelectAnswer {
            result: SelectResult::Refused(refusal),
            cancel: CancellationToken::new(),
            ticket: None,
        });
    }

    /// Dropping the sender ends the writer's loop; waiting on `writer_done` is what makes an acknowledged pin or
    /// cooldown durable before the process exits.
    async fn drain_writer(&mut self) {
        let backlog = {
            let mut deferred = self.deferred.lock().expect(DEFERRED_POISONED);
            (!deferred.is_empty()).then(|| deferred.take())
        };
        if let Some(backlog) = backlog {
            // Awaited: the channel is bounded and the writer is still draining it, so the backlog takes the next
            // slot it frees, and a shutdown after a full channel still lands every acknowledged pin and cooldown.
            self.writer_status.depth.fetch_add(1, Ordering::Relaxed);
            if let Err(mpsc::error::SendError(lost)) = self.writer.send(backlog).await {
                self.writer_status.depth.fetch_sub(1, Ordering::Relaxed);
                let rekeys = lost
                    .iter()
                    .filter(|write| matches!(write, StateWrite::Rekey(_)))
                    .count();
                tracing::warn!(
                    rows = lost.len() - rekeys,
                    rekeys,
                    "the routing state writer has stopped; the writes held in memory are lost"
                );
            }
            // Cleared whatever the send did: the loop is leaving, and nothing is held for it any more.
            self.writer_status.deferred.store(0, Ordering::Relaxed);
            self.writer_status.degraded.store(false, Ordering::Relaxed);
        }
        let (tx, _) = mpsc::channel::<Vec<StateWrite>>(1);
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
                            self.refuse(&request, &snapshot, Refusal::Key(refusal), reply);
                            return;
                        }
                    };
                    // The id is reusable, so the lifetime is the identity: a key created again under the id
                    // since admission is another key, and the request is refused as a refused id is.
                    if key.created_at != request.key_generation {
                        self.refuse(
                            &request,
                            &snapshot,
                            Refusal::Key(KeyRefusal::Replaced),
                            reply,
                        );
                        return;
                    }
                    match resolve(
                        &snapshot,
                        key,
                        &request.canonical_model,
                        &request.folded_model,
                    ) {
                        Ok(candidates) => request.candidates = candidates,
                        Err(refusal) => {
                            self.refuse(&request, &snapshot, Refusal::Resolve(refusal), reply);
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
                let Decided {
                    mut result,
                    writes,
                    mut record,
                    mut summary,
                    shed_round,
                } = decide(&mut self.state, &snapshot, &request, &mut rand::rng());
                self.write(writes);
                if let Some(pin) = record.pin.as_mut() {
                    pin.released = released.clone();
                }
                // Collected rather than emitted, so the decision naming them goes out first and the ledger lands
                // its row and its pin events in one tick.
                let mut pin_events = Vec::new();
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
                                    record.outcome.rule = Some(reason);
                                    summary.rule = Some(reason);
                                    pin_events.push(RouterEvent::PinMigrated {
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
                                None => pin_events.push(RouterEvent::PinCreated {
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
                                    pin_events.push(RouterEvent::PinExpired {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        agent_id: ag.clone(),
                                        model: m.clone(),
                                        account_id: old.account_id.clone(),
                                        reasons: released.clone(),
                                        at: now,
                                    });
                                    pin_events.push(RouterEvent::PinCreated {
                                        key_id: k.clone(),
                                        session_id: s.clone(),
                                        agent_id: ag.clone(),
                                        model: m.clone(),
                                        account_id: new.account_id.clone(),
                                        epoch: new.epoch,
                                        at: now,
                                    });
                                } else {
                                    pin_events.push(RouterEvent::PinMigrated {
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
                        (Some(old), None) => pin_events.push(RouterEvent::PinExpired {
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
                record.outcome.pin_events = pin_events.iter().filter_map(pin_event_facts).collect();
                let round = shed_round.map(|round| RouterEvent::SheddingRound {
                    account_id: round.account_id,
                    pool_id: record.request.pool_id.clone(),
                    request_id: request.request_id.clone(),
                    key_id: round.pin.key_id,
                    session_id: round.pin.session_id,
                    agent_id: round.pin.agent_id,
                    model: round.pin.model,
                    round: round.facts,
                    at: request.now,
                });
                self.emit(RouterEvent::Decision {
                    summary: Box::new(summary),
                    record: Box::new(record),
                    at: request.now,
                });
                for event in pin_events.into_iter().chain(round) {
                    self.emit(event);
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
            Command::RecordRefusal { request, refusal } => self.record_refusal(&request, &refusal),
            // The top of this turn drained the map already.
            Command::Coalesced => {}
            Command::StateMoved { rows } => self.state_moved(rows),
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
            #[cfg(any(test, feature = "debug-hooks"))]
            Command::Park { release, reply } => {
                let _ = reply.send(());
                // Awaited inside the loop, so no command is taken until the guard drops.
                let _ = release.await;
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
                self.reload_waiters.push(reply);
                if !self.loading {
                    self.start_load();
                }
            }
            Command::Swap { loaded, started_at } => {
                let answer =
                    loaded.map(|(load, digest_key)| self.swap(load, digest_key, started_at));
                for waiter in self.load_waiters.drain(..) {
                    let _ = waiter.send(answer.clone());
                }
                self.loading = false;
                if !self.reload_waiters.is_empty() {
                    self.start_load();
                }
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
            Command::PinsByPool { reply } => {
                let snapshot = self
                    .snapshot
                    .read()
                    .expect("snapshot lock poisoned")
                    .clone();
                let mut by_pool: BTreeMap<&str, u64> = BTreeMap::new();
                for key_id in self.state.pin_key_ids() {
                    let pool = snapshot
                        .key(key_id)
                        .map_or(UNATTRIBUTED_POOL, |k| k.pool_id.as_str());
                    *by_pool.entry(pool).or_default() += 1;
                }
                let counts = by_pool
                    .into_iter()
                    .map(|(pool, count)| (pool.to_string(), count))
                    .collect();
                let _ = reply.send(counts);
            }
            Command::Cooldowns { reply } => {
                let _ = reply.send(self.state.cooldowns());
            }
            Command::AccountingDegraded { reply } => {
                let _ = reply.send(self.state.accounting_degraded());
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

    /// Applies every replacing reading the producers left in the pending map, in key order. A reading inserted
    /// before a control fact is queued always drains ahead of it, so a head's own sample still precedes its
    /// cooldown; one taken after a cooldown may drain before that cooldown's command, and then it lifts nothing
    /// and the next reading does.
    fn drain_pending(&mut self) {
        let readings = std::mem::take(&mut self.pending.lock().expect(PENDING_POISONED).readings);
        for report in readings.into_values() {
            self.report(report);
        }
    }

    /// The loop's last drain: what it takes lands, and a reading after it counts as dropped.
    fn final_drain(&mut self) {
        for report in take_final(&self.pending).into_values() {
            self.report(report);
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
                // The account's next holder passes the report over, so it writes no row and its decision's cache map
                // reads empty, as an answer's that wrote none does.
                if self.moved_past(&report.account_id, report.incarnation) {
                    self.emit(cache_map_written(&report, &[]));
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
                self.emit(cache_map_written(&report, &rows));
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
            Report::Accounting { degraded } => self.state.set_accounting_degraded(degraded),
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
                // Read before the fold replaces it, and the holder's own: another holder's state is no edge.
                let before = self.state.credits_of(&account_id, incarnation).cloned();
                let after = state.clone();
                if !self.state.record_credits(&account_id, state) {
                    return;
                }
                self.write(vec![StateWrite::UpsertCredits(row)]);
                let blocked_before = before
                    .as_ref()
                    .and_then(CreditsState::blocked)
                    .map(String::from);
                let blocked_after = after.blocked().map(String::from);
                if blocked_before != blocked_after {
                    self.emit(RouterEvent::FastCreditsChanged {
                        account_id: account_id.clone(),
                        before,
                        after,
                        blocked_before,
                        blocked_after,
                        at: (self.clock)(),
                    });
                }
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
                source_request_id,
            } => {
                if self.moved_past(&account_id, incarnation) {
                    return;
                }
                // The edges compare each plan window's stored reading with the one that replaces it, so the stored
                // ones are read before the fold.
                let stored: HashMap<&str, WindowReading> = sample
                    .windows
                    .keys()
                    .filter(|window| is_plan_window(window))
                    .filter_map(|window| {
                        let reading = self.state.sample(&account_id)?.reading(window)?;
                        Some((window.as_str(), reading))
                    })
                    .collect();
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
                    let windows: Vec<String> =
                        moved.into_iter().filter(|w| is_plan_window(w)).collect();
                    for window in &windows {
                        let Some(reading) = sample.reading(window) else {
                            continue;
                        };
                        self.window_edges(
                            &account_id,
                            window,
                            stored.get(window.as_str()).copied(),
                            reading,
                            source_request_id.as_deref(),
                            now,
                        );
                    }
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
                let own = window_for(&canonical_model);
                let mut shares = vec![(FIVE_HOUR, price(FIVE_HOUR)), (SEVEN_DAY, price(SEVEN_DAY))];
                if own != SEVEN_DAY {
                    // A model week the table does not measure is priced as the 7-day share once, until the cost
                    // table measures it; the share pricer says which.
                    if !self.config.share.measured(own) {
                        tracing::debug!(window = own, "a model window is priced unmeasured");
                    }
                    shares.push((own, price(own)));
                }
                let shares: Vec<(&str, f64)> = shares
                    .into_iter()
                    .filter_map(|(w, s)| s.map(|s| (w, s)))
                    .collect();
                self.state.charge(&account_id, at, &shares);
            }
            Report::AccountState {
                account_id,
                state,
                incarnation,
                lifecycle_generation,
                credential_generation,
                cause,
                detail,
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
                    cause,
                    detail,
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

    /// Starts the one registry load, which answers every reload waiting now. Off the loop: a selection is answered
    /// while the load runs, and a burst of reloads costs two loads at most (the one running, and one more for the
    /// writes that may postdate its read transaction).
    fn start_load(&mut self) {
        // A strong sender held by the task itself would keep the receiver alive after the last handle is gone, and
        // the loop with it; this one lives as long as the load. A refused upgrade means no handle is left, so
        // nothing waits on the answer either.
        let Some(tx) = self.handle_tx.upgrade() else {
            return;
        };
        self.loading = true;
        self.load_waiters = std::mem::take(&mut self.reload_waiters);
        let store = self.store.clone();
        let started_at = (self.clock)();
        // The blocking load runs under a task that awaits it, so a panic in the load still sends its `Swap` and
        // no reload waits on an answer that never comes.
        tokio::spawn(async move {
            let loaded = match tokio::task::spawn_blocking(move || {
                store.load_registry().map(|load| (load, store.digest_key()))
            })
            .await
            {
                Ok(Ok(loaded)) => Ok(loaded),
                Ok(Err(error)) => {
                    tracing::error!(%error, "the registry could not be reloaded; the previous snapshot stays in force");
                    Err(error.to_string())
                }
                Err(error) => {
                    tracing::error!(%error, "the registry load panicked; the previous snapshot stays in force");
                    Err(error.to_string())
                }
            };
            let _ = tx.send(Command::Swap { loaded, started_at }).await;
        });
    }

    /// The in-memory half of a reload: the load becomes the snapshot under the next revision, the streams of a
    /// departed holder end, and what the registry no longer holds is purged.
    fn swap(&mut self, load: RegistryLoad, digest_key: [u8; 32], loaded_at: i64) {
        self.revision += 1;
        let next = Arc::new(RegistrySnapshot::from_load(
            load,
            digest_key,
            loaded_at,
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
        self.clock_resets(now);
        self.wind_downs(&snapshot, now);
        self.expiries(&purged.affinity_entries, now);
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

    /// The edges a plan window's new reading crosses against the stored one: its reset passing, which a later
    /// reset names, or no reset on a reading taken past it, then the window spent, which a reading at 1.0 or above
    /// names over one under it.
    fn window_edges(
        &mut self,
        account_id: &str,
        window: &str,
        stored: Option<WindowReading>,
        reading: WindowReading,
        source_request_id: Option<&str>,
        now: i64,
    ) {
        let passed = stored.and_then(|stored| {
            let reset_at = stored.reset?;
            let next_reset = match reading.reset {
                Some(next) => {
                    (next > reset_at.saturating_add(RESET_SLACK_MS)).then_some(Some(next))
                }
                // The reading replaces the stored one whole, so the clock walk would never see this reset: it is
                // announced here or nowhere.
                None => (reset_at <= reading.observed_at).then_some(None),
            }?;
            Some((stored.utilization, reset_at, next_reset))
        });
        if let Some((utilization_before, reset_at, next_reset)) = passed {
            if self.announce_reset(account_id, window, reset_at) {
                self.emit(RouterEvent::WindowReset {
                    account_id: account_id.to_string(),
                    window: window.to_string(),
                    utilization_before,
                    reset_at,
                    next_reset,
                    observed: "sample",
                    at: now,
                });
            }
        }
        if stored.is_none_or(|stored| stored.utilization < 1.0) && reading.utilization >= 1.0 {
            self.emit(RouterEvent::WindowSpent {
                account_id: account_id.to_string(),
                window: window.to_string(),
                utilization: reading.utilization,
                reset: reading.reset,
                source: reading.source,
                source_request_id: source_request_id.map(String::from),
                at: now,
            });
        }
    }

    /// Whether `reset_at` is a reset not yet announced for the account and window, recording it when it is: the
    /// sample edge and the clock walk both ask, so one passing reset is announced once whichever sees it first.
    fn announce_reset(&mut self, account_id: &str, window: &str, reset_at: i64) -> bool {
        let key = (account_id.to_string(), window.to_string());
        if self
            .reset_announced
            .get(&key)
            .is_some_and(|announced| announced.abs_diff(reset_at) <= RESET_SLACK_MS.unsigned_abs())
        {
            return false;
        }
        self.reset_announced.insert(key, reset_at);
        true
    }

    /// Every stored plan reading whose reset the clock has passed with no newer sample, announced once.
    fn clock_resets(&mut self, now: i64) {
        let passed: Vec<(String, String, WindowReading)> = self
            .state
            .samples()
            .into_iter()
            .flat_map(|(account_id, sample)| {
                sample
                    .windows
                    .into_iter()
                    .filter(|(window, reading)| {
                        is_plan_window(window) && reading.reset.is_some_and(|reset| reset <= now)
                    })
                    .map(move |(window, reading)| (account_id.clone(), window, reading))
            })
            .collect();
        for (account_id, window, reading) in passed {
            let Some(reset_at) = reading.reset else {
                continue;
            };
            if self.announce_reset(&account_id, &window, reset_at) {
                self.emit(RouterEvent::WindowReset {
                    account_id,
                    window,
                    utilization_before: reading.utilization,
                    reset_at,
                    next_reset: None,
                    observed: "clock",
                    at: now,
                });
            }
        }
        // An account whose readings left takes what was announced for it along.
        self.reset_announced
            .retain(|(account_id, _), _| self.state.sample(account_id).is_some());
    }

    /// Every pool member inside its wind-down now that was not at the last tick, announced once while it stays.
    fn wind_downs(&mut self, snapshot: &RegistrySnapshot, now: i64) {
        let mut winding = HashSet::new();
        for pool in snapshot.pools() {
            let minutes = pool.failover.wind_down_minutes;
            for member in snapshot.members(&pool.id) {
                let Some(term) = snapshot.term(&member.account_id) else {
                    continue;
                };
                // A cancelled term past its end still reads as winding down; its account has ended, which is no start.
                let Some(ends_at) = term::end_of(term, now)
                    .filter(|_| term::winding_down(term, now, minutes) && !term::ended(term, now))
                else {
                    continue;
                };
                let key = (pool.id.clone(), member.account_id.clone());
                if !self.winding.contains(&key) {
                    self.emit(RouterEvent::WindDownStarted {
                        account_id: member.account_id.clone(),
                        pool_id: pool.id.clone(),
                        ends_at,
                        wind_down_minutes: minutes,
                        end_source: if term.cancelled { "cancelled" } else { "trial" },
                        at: now,
                    });
                }
                winding.insert(key);
            }
        }
        self.winding = winding;
    }

    /// One expiry per root and account from the entries the purge removed: how many left, the largest of them, and
    /// `incarnation` when any left with its account rather than its age.
    fn expiries(&self, purged: &[PurgedEntry], now: i64) {
        let mut groups: BTreeMap<(&str, &str), Vec<&PurgedEntry>> = BTreeMap::new();
        for purged in purged {
            groups
                .entry((&purged.entry.root, &purged.entry.account_id))
                .or_default()
                .push(purged);
        }
        for ((root, account_id), entries) in groups {
            // A chain's message entries all carry the size of the report that wrote them, so the deeper of two
            // equal ones stands for the root.
            let Some(deepest) = entries
                .iter()
                .max_by_key(|purged| (purged.entry.prefix_tokens, purged.entry.depth))
            else {
                continue;
            };
            let cause = if entries.iter().any(|purged| purged.cause == "incarnation") {
                "incarnation"
            } else {
                "ttl"
            };
            self.emit(RouterEvent::WarmPrefixExpired {
                account_id: account_id.to_string(),
                key_id: deepest.entry.key_id.clone(),
                root: root.to_string(),
                model: deepest.model.clone(),
                lane: deepest.entry.lane.clone(),
                entries: u32::try_from(entries.len()).unwrap_or(u32::MAX),
                deepest: CacheMapEntry {
                    tier: deepest.entry.chain.as_str(),
                    depth: deepest.entry.depth,
                    prefix_tokens: deepest.entry.prefix_tokens,
                    ttl_ms: deepest.entry.ttl_ms,
                    account_id: account_id.to_string(),
                },
                cause,
                at: now,
            });
        }
    }

    fn write(&self, writes: Vec<StateWrite>) {
        if writes.is_empty() {
            return;
        }
        let mut deferred = self.deferred.lock().expect(DEFERRED_POISONED);
        // Nothing passes the backlog: a slot that frees between the backlog's offer and a newer write would let
        // the newer write land first, and a pin's upsert behind its own delete puts a released pin back.
        if !deferred.is_empty() {
            deferred.fold(writes);
            self.writer_status
                .deferred
                .store(deferred.len(), Ordering::Relaxed);
            return;
        }
        match self.writer.try_reserve() {
            Ok(permit) => {
                // Counted in before the send, so the writer never counts out a batch this has not counted in.
                self.writer_status.depth.fetch_add(1, Ordering::Relaxed);
                permit.send(writes);
            }
            Err(mpsc::error::TrySendError::Full(())) => {
                deferred.fold(writes);
                self.writer_status
                    .deferred
                    .store(deferred.len(), Ordering::Relaxed);
                if !self.writer_status.degraded.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        depth = WRITER_DEPTH,
                        "the routing state writer is behind; writes wait in memory, one per row"
                    );
                }
            }
            // Only after `drain_writer`, which has landed everything acknowledged before it.
            Err(mpsc::error::TrySendError::Closed(())) => {}
        }
    }

    /// Hands the backlog to the channel as one batch once a slot is free.
    fn offer_deferred(&self) {
        let mut deferred = self.deferred.lock().expect(DEFERRED_POISONED);
        if deferred.is_empty() {
            return;
        }
        match self.writer.try_reserve() {
            Ok(permit) => {
                self.writer_status.depth.fetch_add(1, Ordering::Relaxed);
                permit.send(deferred.take());
                self.writer_status.deferred.store(0, Ordering::Relaxed);
                self.writer_status.degraded.store(false, Ordering::Relaxed);
                tracing::info!(
                    "the routing state writer caught up; the writes held in memory are queued"
                );
            }
            Err(mpsc::error::TrySendError::Full(())) => {}
            // The writer task is gone, so nothing would ever take the backlog. It closes as the loop shuts down,
            // where the loss is expected, so this warns as `drain_writer` does.
            Err(mpsc::error::TrySendError::Closed(())) => {
                let lost = deferred.take().len();
                self.writer_status.deferred.store(0, Ordering::Relaxed);
                tracing::warn!(
                    lost,
                    "the routing state writer has stopped; the writes held in memory are lost"
                );
            }
        }
    }

    fn has_deferred(&self) -> bool {
        !self.deferred.lock().expect(DEFERRED_POISONED).is_empty()
    }

    /// Resolves once the writer's channel has a free slot, which the next turn's offer takes.
    async fn writer_room(&self) {
        let _ = self.writer.reserve().await;
    }
}

const DEFERRED_POISONED: &str = "deferred writes lock poisoned";

/// A row the backlog keeps one write for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeferredKey {
    /// Keyed by the fence as well as the account: a mark under a fence the store has moved past lands nowhere, so
    /// it never replaces one that would land.
    AccountState {
        account_id: String,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    Affinity {
        model: String,
        hash: String,
    },
    Cooldown {
        account_id: String,
        scope: String,
    },
    Credits(String),
    Overage(String),
    Pin(PinKey),
    PurgeStaleWindows,
    Window {
        account_id: String,
        window: String,
    },
}

/// Writes the channel had no room for, the last write per row: a pin's last upsert or delete, a window's last
/// upsert, a cooldown's last upsert or delete per scope, an overage, credits or account state per account, an
/// affinity row's last upsert or delete per key, one stale-window sweep, and every rekey in order. A rekey closes the
/// rows before it into a segment of their own, so a pin's write dated after a resolution never lands ahead of the
/// rekey that moved its rows.
#[derive(Default)]
struct Deferred {
    /// Oldest first: each closed segment's rows and the rekey that closed it.
    closed: Vec<(HashMap<DeferredKey, StateWrite>, RekeyWrite)>,
    /// The rows written since the last rekey.
    open: HashMap<DeferredKey, StateWrite>,
}

impl Deferred {
    fn is_empty(&self) -> bool {
        self.closed.is_empty() && self.open.is_empty()
    }

    /// The writes held: one per row and one per rekey.
    fn len(&self) -> usize {
        self.closed
            .iter()
            .map(|(rows, _)| rows.len() + 1)
            .sum::<usize>()
            + self.open.len()
    }

    fn fold(&mut self, writes: Vec<StateWrite>) {
        for write in writes {
            match rows_of(write) {
                Ok(rows) => self.open.extend(rows),
                Err(rekey) => {
                    let rows = std::mem::take(&mut self.open);
                    self.closed.push((rows, *rekey));
                }
            }
        }
    }

    /// The backlog as one batch in its order: each segment's rows, then the rekey that closed it.
    fn take(&mut self) -> Vec<StateWrite> {
        let Deferred { closed, open } = std::mem::take(self);
        let mut batch = Vec::new();
        for (rows, rekey) in closed {
            batch.extend(rows.into_values());
            batch.push(StateWrite::Rekey(rekey));
        }
        batch.extend(open.into_values());
        batch
    }
}

/// The rows a write lands on, each with the write that lands there: an affinity batch splits into one write per
/// key. A rekey comes back as `Err`, since it keeps its place in order rather than a row's. No wildcard, so a new
/// write fails to compile until it is placed.
fn rows_of(write: StateWrite) -> Result<Vec<(DeferredKey, StateWrite)>, Box<RekeyWrite>> {
    let one = |key: DeferredKey, write: StateWrite| Ok(vec![(key, write)]);
    let affinity = |model: &str, hash: &str| DeferredKey::Affinity {
        model: model.to_string(),
        hash: hash.to_string(),
    };
    match write {
        StateWrite::AccountState {
            account_id,
            state,
            incarnation,
            lifecycle_generation,
            credential_generation,
            cause,
            detail,
        } => one(
            DeferredKey::AccountState {
                account_id: account_id.clone(),
                incarnation,
                lifecycle_generation,
                credential_generation,
            },
            StateWrite::AccountState {
                account_id,
                state,
                incarnation,
                lifecycle_generation,
                credential_generation,
                cause,
                detail,
            },
        ),
        StateWrite::DeleteAffinity(keys) => Ok(keys
            .into_iter()
            .map(|(model, hash)| {
                (
                    affinity(&model, &hash),
                    StateWrite::DeleteAffinity(vec![(model, hash)]),
                )
            })
            .collect()),
        StateWrite::DeleteCooldown(account_id, scope) => one(
            DeferredKey::Cooldown {
                account_id: account_id.clone(),
                scope: scope.clone(),
            },
            StateWrite::DeleteCooldown(account_id, scope),
        ),
        StateWrite::DeletePin(key) => {
            one(DeferredKey::Pin(key.clone()), StateWrite::DeletePin(key))
        }
        StateWrite::PurgeStaleWindows => one(
            DeferredKey::PurgeStaleWindows,
            StateWrite::PurgeStaleWindows,
        ),
        StateWrite::Rekey(rekey) => Err(Box::new(rekey)),
        StateWrite::UpsertAffinity(rows) => Ok(rows
            .into_iter()
            .map(|row| {
                (
                    affinity(&row.model, &row.hash),
                    StateWrite::UpsertAffinity(vec![row]),
                )
            })
            .collect()),
        StateWrite::UpsertCooldown(row) => one(
            DeferredKey::Cooldown {
                account_id: row.account_id.clone(),
                scope: row.scope.clone(),
            },
            StateWrite::UpsertCooldown(row),
        ),
        StateWrite::UpsertCredits(row) => one(
            DeferredKey::Credits(row.account_id.clone()),
            StateWrite::UpsertCredits(row),
        ),
        StateWrite::UpsertOverage(row) => one(
            DeferredKey::Overage(row.account_id.clone()),
            StateWrite::UpsertOverage(row),
        ),
        StateWrite::UpsertPin(row) => one(DeferredKey::Pin(row.key()), StateWrite::UpsertPin(row)),
        StateWrite::UpsertWindow(row) => one(
            DeferredKey::Window {
                account_id: row.account_id.clone(),
                window: row.window.clone(),
            },
            StateWrite::UpsertWindow(row),
        ),
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

/// `incarnation` is the holder of the account id the snapshot was fetched for, which the evidence names. The
/// sample keeps `five_hour`, `seven_day` and every other `seven_day_*` window under the endpoint's key, and the
/// `limits` window as `seven_day_fable`; a key no family row names is kept and feeds no allowance.
pub fn sample_from_usage(
    snapshot: &UsageSnapshot,
    incarnation: u64,
) -> (UtilizationSample, Option<OverageEvidence>) {
    let mut sample = UtilizationSample {
        observed_at: snapshot.fetched_at,
        ..Default::default()
    };
    let kept = snapshot
        .windows
        .iter()
        .filter(|w| is_plan_window(&w.key))
        .map(|w| (w.key.as_str(), w));
    // The `limits` window comes last, so it is the Fable week's reading whatever top-level key the endpoint sends.
    let fable = snapshot.fable_window.iter().map(|w| (SEVEN_DAY_FABLE, w));
    for (name, window) in kept.chain(fable) {
        if let Some(utilization) = window.utilization {
            sample = sample.with(
                name,
                WindowReading {
                    utilization: utilization / 100.0,
                    reset: window.resets_at,
                    observed_at: snapshot.fetched_at,
                    source: WindowSource::Poll,
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
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    use phoebis_store::{AccountKind, AccountRow, ResolutionRow, StorageKey, WindowRow};

    use super::*;

    fn cooldown(account_id: &str, scope: &str) -> CooldownRow {
        CooldownRow {
            account_id: account_id.into(),
            scope: scope.into(),
            until: 10_000,
            set_at: 0,
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
        }
    }

    /// A rekey whose moved cooldown names an account the store does not hold, which
    /// `cooldowns.account_id REFERENCES accounts(id)` refuses.
    fn refused_rekey(spelling: &str, dated_id: &str) -> StateWrite {
        StateWrite::Rekey(RekeyWrite {
            resolution: ResolutionRow {
                spelling: spelling.into(),
                dated_id: dated_id.into(),
                resolved_at: 1_000,
            },
            delete_pins: Vec::new(),
            upsert_pins: Vec::new(),
            delete_cooldowns: Vec::new(),
            upsert_cooldowns: vec![cooldown("ghost", &format!("model:{dated_id}"))],
        })
    }

    /// A handle over a mailbox whose receiver the test holds, built as `spawn` builds one.
    fn handle_over(tx: tokio::sync::mpsc::Sender<Command>) -> RouterHandle {
        RouterHandle {
            affinity: Arc::default(),
            dropped: Arc::default(),
            dropped_refusals: Arc::default(),
            events: broadcast::channel(1).0,
            in_flight: InFlight::default(),
            load_counts: LoadCounts::default(),
            start_events: Arc::default(),
            pending: Pending::default(),
            reports: Arc::default(),
            snapshot: Arc::new(RwLock::new(Arc::new(RegistrySnapshot::from_load(
                RegistryLoad::default(),
                [0; 32],
                0,
                1,
            )))),
            tx,
            writer: Arc::default(),
        }
    }

    fn dropped(handle: &RouterHandle) -> u64 {
        handle.dropped_reports().iter().map(|(_, n)| n).sum()
    }

    /// A reading taken after the shutdown closed the mailbox and before the final drain lands with that drain and
    /// counts no drop; one taken after the final drain lands nowhere and counts one.
    #[test]
    fn a_reading_between_the_mailbox_closing_and_the_final_drain_lands_and_one_after_it_counts_a_drop(
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let handle = handle_over(tx);
        close_mailbox(&handle.pending, &mut rx);
        assert!(handle.pend(Report::Accounting { degraded: true }).is_none());
        assert_eq!(
            dropped(&handle),
            0,
            "the final drain still takes the reading"
        );
        assert_eq!(take_final(&handle.pending).len(), 1);
        assert!(handle
            .pend(Report::Accounting { degraded: false })
            .is_none());
        assert_eq!(
            handle.dropped_reports(),
            Report::KINDS
                .iter()
                .map(|kind| (*kind, u64::from(*kind == "accounting")))
                .collect::<Vec<_>>(),
            "after the final drain nothing takes the reading"
        );
    }

    #[test]
    fn of_two_refused_rekeys_of_one_spelling_the_newer_is_the_one_retried() {
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        let landed = replay(
            &store,
            vec![
                refused_rekey("claude-sonnet-4-5", "claude-sonnet-4-5-20250929"),
                refused_rekey("claude-sonnet-4-5", "claude-sonnet-4-5-20251001"),
            ],
        );
        assert_eq!(landed.skipped, 2);
        let carried = landed.rekey.expect("a rekey to retry");
        assert_eq!(
            carried.resolution.dated_id, "claude-sonnet-4-5-20251001",
            "the latest resolution of the spelling"
        );
    }

    /// A directory under the system's temporary one, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Scratch(
                std::env::temp_dir().join(format!("phoebis-{name}-{}-{nanos}", std::process::id())),
            )
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A store at `scratch` holding account `a`, and a second store on the same file through a link, as a backup or
    /// an operator's `sqlite3` would hold it: the process lock is per directory, and SQLite resolves the link to the
    /// one file and its one WAL.
    fn linked_stores(scratch: &Scratch) -> (Store, Store) {
        let key = StorageKey::generate();
        let path = scratch.path().join("store").join("router.sqlite");
        let store = Store::open(&path, &key).unwrap();
        store
            .insert_account(&AccountRow {
                id: "a".into(),
                kind: AccountKind::ApiKey,
                label: "a".into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
        let linked = scratch.path().join("holder").join("router.sqlite");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&path, &linked).unwrap();
        (store, Store::open(&linked, &key).unwrap())
    }

    /// Holds `holder`'s write lock on a thread until the answered sender is dropped or sent to.
    fn hold_the_lock(holder: Store) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (locked, held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let holding = std::thread::spawn(move || {
            holder
                .write_routing(|_| {
                    locked.send(()).unwrap();
                    let _ = released.recv();
                    Ok(())
                })
                .unwrap();
        });
        held.recv().unwrap();
        (release, holding)
    }

    #[tokio::test]
    async fn a_database_wide_failure_counts_a_failed_batch_and_lands_its_remainder_at_the_next_wake(
    ) {
        let scratch = Scratch::new("writer-remainder");
        let (store, holder) = linked_stores(&scratch);
        store
            .debug_set_busy_timeout(Duration::from_millis(100))
            .unwrap();
        let store = Arc::new(store);
        let figures = Arc::new(WriterShared::default());
        let mut writer = Writer {
            store: store.clone(),
            figures: figures.clone(),
            rekey: None,
            remainder: None,
        };
        let (release, holding) = hold_the_lock(holder);
        // Waits out the busy timeout twice: the wake's transaction, then the replay's first write.
        writer
            .wake(
                ["model:claude-haiku-4-5", "model:claude-opus-5"]
                    .map(|scope| StateWrite::UpsertCooldown(cooldown("a", scope)))
                    .into(),
            )
            .await;
        release.send(()).unwrap();
        holding.join().unwrap();
        assert_eq!(figures.failed_batches.load(Ordering::Relaxed), 1);
        assert!(store.load_routing().unwrap().cooldowns.is_empty());

        writer
            .wake(vec![StateWrite::UpsertCooldown(cooldown(
                "a",
                "model:claude-sonnet-5",
            ))])
            .await;
        let mut scopes: Vec<String> = store
            .load_routing()
            .unwrap()
            .cooldowns
            .into_iter()
            .map(|row| row.scope)
            .collect();
        scopes.sort();
        assert_eq!(
            scopes,
            [
                "model:claude-haiku-4-5",
                "model:claude-opus-5",
                "model:claude-sonnet-5"
            ],
            "the remainder is tried once more at the next wake, ahead of that wake's own writes"
        );
        assert_eq!(figures.failed_batches.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_lock_held_past_the_busy_timeout_stops_the_replay_at_its_first_write() {
        let scratch = Scratch::new("replay-busy");
        let (store, holder) = linked_stores(&scratch);
        store
            .debug_set_busy_timeout(Duration::from_millis(100))
            .unwrap();
        let (release, holding) = hold_the_lock(holder);

        let landed = replay(
            &store,
            [
                "model:claude-haiku-4-5",
                "model:claude-opus-5",
                "model:claude-sonnet-5",
            ]
            .map(|scope| StateWrite::UpsertCooldown(cooldown("a", scope)))
            .into(),
        );
        release.send(()).unwrap();
        holding.join().unwrap();

        assert_eq!(
            (landed.skipped, landed.left),
            (0, 3),
            "the first write's busy answer gave up the wake rather than waiting it out once per write"
        );
        assert_eq!(
            landed.remainder.len(),
            3,
            "the unapplied writes go back to the writer whole"
        );
        assert!(landed.stopped.expect("the busy store").is_database_wide());
        assert!(landed.refused.is_none());
        assert!(store.load_routing().unwrap().cooldowns.is_empty());
    }

    #[tokio::test]
    async fn the_handle_counts_the_windows_by_account_as_the_load_carried_them() {
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
        for id in ["a", "b"] {
            store
                .insert_account(&AccountRow {
                    id: id.into(),
                    kind: AccountKind::Oauth,
                    label: id.into(),
                    enabled: true,
                    identity: None,
                    lifecycle_generation: 1,
                    incarnation: 1,
                    revision: 1,
                    state: AccountState::Ready,
                })
                .unwrap();
        }
        // Two windows of `a` and none of `b`: one account with a reading.
        for window in [FIVE_HOUR, SEVEN_DAY] {
            assert!(store
                .upsert_window(&WindowRow {
                    account_id: "a".into(),
                    incarnation: 1,
                    observed_at: 500,
                    reset: Some(10_000),
                    utilization: 0.5,
                    window: window.into(),
                })
                .unwrap());
        }
        let handle = spawn(store, Arc::new(|| 1_000), RouterConfig::default()).unwrap();
        assert_eq!(
            handle.load_counts(),
            LoadCounts {
                pins: 0,
                windows: 1,
                affinity_entries: 0
            }
        );
    }
}
