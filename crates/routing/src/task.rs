//! The router task: one loop that owns the routing state, answers selections over a channel, reloads the
//! registry snapshot on request, folds the readings and the usage the relay and the poll report, and writes state
//! through a single writer so the store sees every change in the order the loop made it.

use std::collections::{btree_map, BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use phoebis_oauth::{Clock, LandedMark, UsageSnapshot};
use phoebis_store::{
    AccountState, CooldownRow, CooldownScope, PinBurn, PinKey, PinRow, PoolRow, RegistryLoad,
    RekeyWrite, RoutingLoad, RoutingTx, ShedRoundRow, StateCause, Store, StoreError,
};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::affinity::{
    AffinityMap, AffinityReport, AffinityRow, ConversationLookup, ConversationMatch,
    ConversationQuery, PredecessorQuery, PurgedEntry,
};
use crate::record::{
    CacheMapEntry, DecisionRecord, DecisionSummary, MoveCause, PinEventFacts, RefusalFacts,
    RefusedRequest, ShedRoundFacts,
};
use crate::select::{
    decide, obstacle, pace_of, passes_reset, projection_of, projections_of, release_reasons,
    routed_agent, slow_down_ramp, BurnRead, Decided, FailedOver, Obstacle, Refusal, SelectRequest,
    SelectResult, MAIN_AGENT, RATE_LIMIT_REASON,
};
use crate::state::{
    all_plan_windows, burn_average_ms, burn_decayed, burn_folded, ceiling, default_burn_average_ms,
    effort_of, entrypoint_of, is_plan_window, latest_full_reset, mark_holds, plan_windows,
    round_in_dwell, shed_dwell_ms, slow_down_hold_ms, spend_reset_estimate, standing_mark,
    wait_until, AccountTicket, BurnUnit, CreditsState, EvidenceSource, InFlight, MemberSpend,
    OverageEvidence, PinRef, PinUsage, RateTable, Recorded, RoutingState, SharePricer,
    SlowDownEdge, SpendWindow, StateWrite, UtilizationSample, WindowReading, WindowSource,
    FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use crate::{model_week, resolve, term, window_length_ms, Candidate, Presented, RegistrySnapshot};

/// The router loop's command queue size, which `/admin/system` reports. A report that finds the queue full waits for a
/// slot; one the closed queue refuses at shutdown is dropped and counted under its kind.
pub const COMMANDS: usize = 4_096;

/// The pool a pin of a key the snapshot no longer holds counts under. The ledger files a key, pool or account a
/// request never reached under this id too, so the pin's pool label is one the rollup rows use.
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

/// The pin a failover invalidated, kept for `MIGRATION_WINDOW_MS` so the landing reads its figures and names its cause.
/// An entry holds its session's first-seen records while it stands inside `MIGRATION_WINDOW_MS`, since a pin in the
/// gap has not left.
struct Invalidated {
    account_id: String,
    /// `cooldown`, `fast_lane` or `mark`.
    reason: &'static str,
    at: i64,
    row: PinRow,
    cause: Option<MoveCause>,
}

/// How far apart two readings' resets may lie and still name one reset: a head names its resets in whole seconds and
/// the poll in milliseconds, so the two sources disagree about one reset by under a second, while a window's next
/// reset lands hours after the last.
pub const RESET_SLACK_MS: i64 = 60_000;

/// Whether two readings' resets name one reset.
pub fn same_reset(a: i64, b: i64) -> bool {
    a.abs_diff(b) <= RESET_SLACK_MS.unsigned_abs()
}

/// How far under 1.0 a reading may land over a stored reading at the wall and still read the wall. A poll reads the
/// endpoint's whole percent and a head the utilization's float, so near a wall a head lands up to a point under the
/// poll's figure (0.96 against 0.97, 0.99 after 1.0) and can sit a hair under that point too (0.989 where the poll
/// read 1.0), which a one-point margin would read as a clear. Two points cover the head's point under the poll with
/// one point of room. It is a constant, measured on live samples.
pub const SPENT_MARGIN: f64 = 0.02;

/// The words a pin event's kind is named by, in a decision record's `pin_events` and in the ledger's events table
/// alike, so the console reads one vocabulary in both.
pub const PIN_CREATED: &str = "pin_created";
pub const PIN_EXPIRED: &str = "pin_expired";
pub const PIN_MIGRATED: &str = "pin_migrated";

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
    /// A shedding mark left a pin: the heal check found the window back under its test at the pin's request (`healed`,
    /// with the healing projection, and with the stretch and the threshold where the stretch healed a 5-hour mark), the
    /// pin moved off the account the mark was made on (`moved`), or the pin was released, expired or lost a rekey's
    /// merge without a move, or asked while its pool's load shedding was off (`released`). `account_id` is the account
    /// the pin sat on when the mark cleared.
    MarkCleared {
        account_id: String,
        key_id: String,
        session_id: String,
        agent_id: String,
        model: String,
        reason: &'static str,
        marked_at: i64,
        marked_on: String,
        marked_window: String,
        projection: Option<f64>,
        stretch_ms: Option<i64>,
        threshold_ms: Option<i64>,
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
        /// The destination's fit as the move read it: the verdict (`fits`, `rewrite_only`, `least_bad`), the runway
        /// on the binding window (`None` for an unbounded one) and that window.
        fit: Option<&'static str>,
        runway_ms: Option<i64>,
        binding: Option<String>,
        /// Why a failover moved the session, `None` for a rule's move and for a failover the relay sent with
        /// neither a cooldown nor a mark.
        cause: Option<MoveCause>,
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
    /// A reservation left the account's prediction without the usage that would have replaced it: the request
    /// ended without a committed usage (`cause` the relay's word for how), or the purge gave up on it (`timeout`).
    /// `shares` is the rewrite the booking carried per plan window, `age_ms` how long it stood.
    ReservationExpired {
        account_id: String,
        request_id: String,
        attempt: u32,
        cause: &'static str,
        shares: BTreeMap<String, f64>,
        age_ms: i64,
        at: i64,
    },
    /// One decision as the engine made it. The ledger stores the summary as the list row and the record as the
    /// frame. Shared so the enum stays the size of its other variants on the broadcast, and so a receive clones a
    /// pointer where a box would clone the decision.
    Decision {
        summary: Arc<DecisionSummary>,
        record: Arc<DecisionRecord>,
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
    /// A shedding round that ran on `account_id` for a request of `pool_id`: the projection that fired it, on the first
    /// plan window whose projection at its horizon read over the ceiling, the round's unit, its pins in the round's
    /// order and the marks it made.
    SheddingRound {
        account_id: String,
        pool_id: String,
        request_id: String,
        key_id: String,
        session_id: String,
        agent_id: String,
        model: String,
        /// Boxed: the round's facts carry every pin of the round, and the broadcast copies the enum whole.
        round: Box<ShedRoundFacts>,
        at: i64,
    },
    /// The account's factor on the pool read under 1 for the first time since it last read 1: the read's factor,
    /// stretch and threshold, its demand in share of the 5-hour window per hour and the window's reset.
    SlowDownStarted {
        account_id: String,
        pool_id: String,
        factor: f64,
        stretch_ms: i64,
        threshold_ms: i64,
        demand: f64,
        reset: Option<i64>,
        at: i64,
    },
    /// The factor read 1 for the safety wait, the window's reset cleared the entry, or the purge dropped it: the span
    /// from `since` to the instant the factor returned to 1, to the passed reset of the window it slowed for where the
    /// reset cleared the entry, or to the entry's last read where the purge dropped the entry before either, with the
    /// lowest factor read in it and the time its new holds waited.
    SlowDownEnded {
        account_id: String,
        pool_id: String,
        since: i64,
        duration_ms: i64,
        factor_min: f64,
        held_ms: i64,
        at: i64,
    },
    /// The credits read answered differently before and after a credits reading landed: `blocked_before` as the
    /// selector read the held state at the fold's clock, `blocked_after` the reading's own `blocked()`.
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
    /// A plan window read under 1.0 after a reading at 1.0 or above, or after a wall the prediction announced, under
    /// the same reset: the figure before, the stored reading's or the prediction's, the reading after and the
    /// unchanged reset, nothing of why, since a reset grant and an upstream correction read alike and the router
    /// infers no cause.
    WindowCleared {
        account_id: String,
        window: String,
        utilization_before: f64,
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
        /// Announced by the first purge after the start, over a reading the router restored: the previous run may
        /// have announced it already, which the ledger decides from its own newest row.
        restored: bool,
        at: i64,
        /// The plan and the tier of the stored reading whose reset passed, never the account's pair at the emission, so
        /// a profile read between the reading and the reset leaves the old week under the pair it ran under.
        plan: Option<String>,
        tier: Option<String>,
    },
    /// The entries the purge removed for one root on one account, with the largest among them. `deepest.depth` is
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
        /// Announced by the first purge after the start, which finds every member already inside its window: the
        /// previous run may have announced it already, which the ledger decides from its own newest row.
        restored: bool,
        at: i64,
    },
    /// The writer reported an `AccountState` row its write moved to another state: the state written, the row's
    /// identity after the write (its incarnation and the revision the write bumped, `AccountRow.revision`), and the
    /// cause the fenced write stored with the state, the word the mirror's tick reads off the row. The mirror
    /// reconciles on the identity, so a transition it observed through the registry already, or a newer one, drops
    /// this event.
    AccountStateChanged {
        account_id: String,
        to: AccountState,
        incarnation: u64,
        revision: u64,
        cause: StateCause,
        detail: Option<String>,
        at: i64,
    },
}

impl RouterEvent {
    fn pin_created(key: &PinKey, account_id: &str, epoch: u64, at: i64) -> RouterEvent {
        RouterEvent::PinCreated {
            key_id: key.key_id.clone(),
            session_id: key.session_id.clone(),
            agent_id: key.agent_id.clone(),
            model: key.model.clone(),
            account_id: account_id.to_string(),
            epoch,
            at,
        }
    }

    fn pin_expired(
        key: &PinKey,
        account_id: &str,
        reasons: Vec<&'static str>,
        at: i64,
    ) -> RouterEvent {
        RouterEvent::PinExpired {
            key_id: key.key_id.clone(),
            session_id: key.session_id.clone(),
            agent_id: key.agent_id.clone(),
            model: key.model.clone(),
            account_id: account_id.to_string(),
            reasons,
            at,
        }
    }

    fn pin_migrated(
        key: &PinKey,
        from_account: String,
        to: &PinRow,
        reason: &'static str,
        record: &DecisionRecord,
        cause: Option<MoveCause>,
        at: i64,
    ) -> RouterEvent {
        let (fit, runway_ms, binding) = destination_fit(record, &to.account_id);
        RouterEvent::PinMigrated {
            key_id: key.key_id.clone(),
            session_id: key.session_id.clone(),
            agent_id: key.agent_id.clone(),
            model: key.model.clone(),
            from_account,
            to_account: to.account_id.clone(),
            epoch: to.epoch,
            reason: reason.into(),
            fit,
            runway_ms,
            binding,
            cause,
            at,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    /// The meter's peak band at a renewal's instant: a pin's banded copy folds its tokens times it, the plain
    /// copy does not.
    pub band: BandWeight,
    /// Dates an overage whose evidence names no reset in a projections answer, as a selection's request carries its
    /// own copy: the configuration's `routing.cooldown_fallback_seconds`.
    pub cooldown_fallback: Duration,
    /// The wait before a `funding_required` account is tried again: the shorter of five minutes and the model
    /// rejection interval, which the router computes from its configuration.
    pub funding_retry: Duration,
    /// Whether the loop skips the purge tick until `RouterHandle::release_purge`, so the first purge runs at the
    /// first tick after the release: the tick that came due meanwhile, at once. The interval's first tick fires at
    /// once, before the spawner can subscribe to the broadcast, and the first purge after a start announces the
    /// downtime's clock resets and wind-downs. The router's start holds it until the ledger subscribed and holds the
    /// start row.
    pub hold_first_purge: bool,
    pub purge_interval: Duration,
    /// The dollar table and the subscription cost model's 5-hour share tables per regime a reader prices each pin's
    /// row through, and the fast table it prices the pin's fast usage through. A renewal folds tokens and reads none of
    /// them. The task holds them because neither this crate nor the relay can see the ledger's tables.
    pub rates: BurnRates,
    /// The share of an account's windows a completed request consumed; the default prices nothing.
    pub share: SharePricer,
    pub touch_interval: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            band: BandWeight::default(),
            cooldown_fallback: Duration::from_secs(60),
            funding_retry: Duration::from_secs(300),
            hold_first_purge: false,
            purge_interval: Duration::from_secs(60),
            rates: BurnRates::default(),
            share: SharePricer::default(),
            touch_interval: Duration::from_secs(60),
        }
    }
}

/// One plan window's share tables per regime, read once at start: the base table and one per regime the revision in
/// force names, with each listed entrypoint mapped to its regime's name.
#[derive(Clone, Debug, PartialEq)]
pub struct RegimeRates {
    pub base: RateTable,
    pub regimes: HashMap<String, RateTable>,
    pub entrypoints: HashMap<String, String>,
}

impl RegimeRates {
    /// The unit table as the base and no regime, so every entrypoint reads a token count.
    pub fn unit() -> RegimeRates {
        RegimeRates {
            base: RateTable::unit(),
            regimes: HashMap::new(),
            entrypoints: HashMap::new(),
        }
    }

    /// The regime's table for a listed entrypoint, the base table for `None` and for an unlisted one.
    pub fn for_entrypoint(&self, entrypoint: Option<&str>) -> &RateTable {
        entrypoint
            .and_then(|entrypoint| self.entrypoints.get(entrypoint))
            .and_then(|regime| self.regimes.get(regime))
            .unwrap_or(&self.base)
    }
}

/// The four tables a reader prices a pin's burn through: dollars per token, the subscription cost model's 5-hour and
/// 7-day shares on its anchor tier per token for the pin's row under the pin's regime, and dollars per token at the
/// fast cells for the pin's fast usage. All four default to the unit tables, so a burn read through the default is a
/// token count.
#[derive(Clone, Debug, PartialEq)]
pub struct BurnRates {
    pub dollars: RateTable,
    /// The 5-hour share per token of the pin's row under its regime, the table `BurnUnit::table` reads for
    /// `WindowShare` when `price_pin` prices a pin on `five_hour`. The router's own burn reads price in dollars alone,
    /// so the admin's Sessions pricing is the reader in production, while the fit and the pace read the per-window map.
    pub five_hour: RegimeRates,
    /// The 7-day share per token under the same regimes, the table a reader prices an account's burn on where its
    /// window set lacks the 5-hour window, and the table the Pro-week figure of every pin's burn reads.
    pub seven_day: RegimeRates,
    /// Dollars per token at the fast cells, the table a pin's fast usage prices through. A row without fast cells
    /// carries its own standard and long-context rates here, as the ledger builds it, so a fast answer on such a
    /// row prices at the set its prompt selects, as it does in the ledger.
    pub fast: RateTable,
}

impl Default for BurnRates {
    fn default() -> Self {
        BurnRates {
            dollars: RateTable::unit(),
            five_hour: RegimeRates::unit(),
            seven_day: RegimeRates::unit(),
            fast: RateTable::unit(),
        }
    }
}

impl BurnRates {
    /// One second of the pin's burn in `unit` on `window` (a plan window under `WindowShare`, ignored under `Usd`),
    /// with the fast usage read from the row's own `burn_fast` and the usage of the client's own classifier calls from
    /// the row's own `burn_unmetered`. Under `Usd` the row's plain copy at the set the pin's last prompt selects plus
    /// the fast usage through the fast table plus the unmetered usage through the dollar table at the set the last
    /// prompt selects. On a row with fast cells the fast table carries them whatever the prompt, so a prompt that
    /// crosses the threshold reprices the ordinary usage alone, and on a row without them it carries the row's own
    /// sets, so the fast usage reprices with the row, as the ledger prices a fast answer there. Under `WindowShare` the
    /// copy `BurnUnit::read` takes on `window` alone, the banded one on `five_hour`, through the table of the window it
    /// prices on under the pin's regime: fast usage draws usage credits and a classifier call takes no share of a plan
    /// window, so neither reaches the plan's figure, while an account in overage or on an API key pays the dollars of
    /// both. `None` where a type the pin burns has no rate.
    pub fn price_pin(&self, unit: BurnUnit, pin: &PinRow, window: &str) -> Option<f64> {
        let own = unit.table(self, entrypoint_of(pin), window).price_burn(
            &pin.model,
            &unit.read(pin, window),
            pin.last_input_tokens,
        )?;
        match unit {
            BurnUnit::Usd => Some(
                own + self
                    .fast
                    .price_burn(&pin.model, &pin.burn_fast, pin.last_input_tokens)?
                    + self.dollars.price_burn(
                        &pin.model,
                        &pin.burn_unmetered,
                        pin.last_input_tokens,
                    )?,
            ),
            BurnUnit::WindowShare => Some(own),
        }
    }
}

/// The weight the meter puts on a charge landing on a window at an instant, read under the regime of the request's
/// entrypoint (the base one for `None`), since each regime carries its own band factor. The ledger's peak band decides
/// it, and this crate cannot name the ledger's table, so the band arrives as a function. The default weighs every
/// instant 1.
#[derive(Clone)]
pub struct BandWeight(Arc<Weigh>);

type Weigh = dyn Fn(&str, Option<&str>, i64) -> f64 + Send + Sync;

impl BandWeight {
    pub fn new(
        weigh: impl Fn(&str, Option<&str>, i64) -> f64 + Send + Sync + 'static,
    ) -> BandWeight {
        BandWeight(Arc::new(weigh))
    }

    pub fn at(&self, window: &str, entrypoint: Option<&str>, at: i64) -> f64 {
        (self.0)(window, entrypoint, at)
    }
}

impl Default for BandWeight {
    fn default() -> BandWeight {
        BandWeight::new(|_, _, _| 1.0)
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
    /// The funding ordinal the claim answered for a `funding_required` account's attempt, `None` for any other.
    pub funding_ordinal: Option<u64>,
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
        /// Who moved the state, named at the relay's mark site. The fenced write stores it with the state and the
        /// event a moved row emits carries it.
        cause: StateCause,
        /// The upstream error's message where the verdict read one.
        detail: Option<String>,
        /// The funding ordinal the attempt was dispatched under, on a funding retry's success alone. An attempt
        /// dispatched under an older mark than the account's newest says nothing about the balance that mark's refusal
        /// found, so its success clears nothing.
        dispatched_ordinal: Option<u64>,
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
    /// A request that ended without a committed usage, sent once by the relay's end guard, which belongs to the
    /// request across its wire attempts: the booking under `request_id` leaves the destination's prediction, and a
    /// request that holds none changes nothing. `attempt` is the wire attempt that ended, a fact for the event.
    /// `cause` is `canceled`, `exhausted`, `refused` or `transport_error`.
    Ended {
        request_id: String,
        attempt: u32,
        at: i64,
        cause: &'static str,
    },
    /// A leg's answer is known: its head came, the pre-commit hold ended without one, or the leg ended before
    /// either. The mark the moved pin's next requests wait on clears where it names `request_id` at `attempt`. A
    /// control fact, never coalesced.
    Head {
        request_id: String,
        attempt: u32,
        at: i64,
    },
    /// The ledger's latest read of a pool member's spend, replacing every row it names.
    MemberSpend(Vec<MemberSpend>),
    Overage {
        account_id: String,
        evidence: OverageEvidence,
        incarnation: u64,
    },
    /// A failover without a cooldown ended the pin. `mark` is the state the verdict marks the account with and who
    /// moved it, `None` for a failover that marks nothing (a 401 whose refresh failed without marking the account).
    PinInvalidated {
        account_id: String,
        pin: PinRef,
        mark: Option<(AccountState, StateCause)>,
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
        /// Whether a keepalive probe renewed the pin, rather than the client's own request.
        keepalive: bool,
        /// The cache TTL the relay resolved for the attempt: the policy's rewrite, else the client's marker.
        cache_ttl_ms: i64,
        /// The answer reported `usage.speed` `fast`.
        fast: bool,
        /// The request is the client's own classifier call on the account and incarnation its judged turn ended on
        /// (`Selection::judged` reads `On`), which charges no plan window and enters the pin's unmetered copy alone.
        /// False for a classifier call the router answered off, which folds as a standard answer and leaves its body
        /// as the pin's last prompt, the rewrite the pin's next move prices.
        classifier: bool,
        /// The request's class is a turn's, `main` or `subagent`: its stream's end is the end of the agent's turn,
        /// which `latest_turn_pin` answers. False for a compaction and every auxiliary call, the classifier's among
        /// them. A probe that replays such a request carries it too, so the task reads it beside `keepalive`.
        turn: bool,
    },
    Sample {
        account_id: String,
        sample: UtilizationSample,
        incarnation: u64,
        /// The request whose response head carried the reading, or `None` for the usage poll's.
        source_request_id: Option<String>,
    },
    /// A head on a usage-based account: the overage family read as the spend window's share and reset, no overage
    /// evidence. Coalesced per account, holder and the members the head names.
    SpendHead {
        account_id: String,
        /// `anthropic-ratelimit-unified-overage-utilization`, a fraction.
        utilization: Option<f64>,
        /// `anthropic-ratelimit-unified-overage-reset`, epoch milliseconds.
        reset: Option<i64>,
        observed_at: i64,
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
        /// The request is the client's own classifier call on the account and incarnation its judged turn ended on,
        /// which charges no plan window, so the report only releases its booking. False for a classifier call the
        /// router answered off, which the upstream charges as any call.
        classifier: bool,
        /// The request whose booking the usage releases before it charges, and the wire attempt that answered.
        request_id: String,
        attempt: u32,
        /// The request's entrypoint as the identity read it, what the charge prices under: `None` for a client that
        /// is not Claude Code, which prices at the base regime.
        entrypoint: Option<String>,
    },
}

/// A mark the credential manager landed, a refresh's or a usage 401's, as the routing hears it: no upstream detail,
/// and no funding ordinal, which a funding retry's success alone carries.
impl From<&LandedMark> for Report {
    fn from(mark: &LandedMark) -> Report {
        Report::AccountState {
            account_id: mark.account_id.clone(),
            state: mark.state,
            incarnation: mark.incarnation,
            lifecycle_generation: mark.lifecycle_generation,
            credential_generation: mark.credential_generation,
            cause: mark.cause,
            detail: None,
            dispatched_ordinal: None,
        }
    }
}

impl Report {
    /// Every kind, alphabetical, so a reader sees the whole set at zero.
    pub const KINDS: &'static [&'static str] = &[
        "account_state",
        "accounting",
        "affinity",
        "cooldown",
        "credits",
        "ended",
        "head",
        "member_spend",
        "overage",
        "pin_invalidated",
        "pin_released",
        "pin_renewed",
        "sample",
        "spend_head",
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
            Report::Ended { .. } => "ended",
            Report::Head { .. } => "head",
            Report::MemberSpend(_) => "member_spend",
            Report::Overage { .. } => "overage",
            Report::PinInvalidated { .. } => "pin_invalidated",
            Report::PinReleased { .. } => "pin_released",
            Report::PinRenewed { .. } => "pin_renewed",
            Report::Sample { .. } => "sample",
            Report::SpendHead { .. } => "spend_head",
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
    /// The window set, sorted, and the source: a head over the plan windows and a poll over the same ones hold two
    /// slots, since a head folded over a pending poll would lose the set the poll settles, and the `BTreeMap` drains
    /// the head first.
    Sample {
        account_id: String,
        incarnation: u64,
        windows: Vec<String>,
        poll: bool,
    },
    /// The members the head names, so a head that names the reset alone never replaces one that carries a share:
    /// each header is its own reading, which the fold's own age tests order.
    SpendHead {
        account_id: String,
        incarnation: u64,
        share: bool,
        reset: bool,
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
                poll: sample.poll,
            }),
            Report::SpendHead {
                account_id,
                utilization,
                reset,
                incarnation,
                ..
            } => Some(CoalesceKey::SpendHead {
                account_id: account_id.clone(),
                incarnation: *incarnation,
                share: utilization.is_some(),
                reset: reset.is_some(),
            }),
            Report::Affinity(_)
            | Report::Cooldown { .. }
            | Report::Ended { .. }
            | Report::Head { .. }
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
            // Two samples under one key read the same windows from the same source, so the sample's own age speaks for
            // each of them.
            (Report::Sample { sample, .. }, Report::Sample { sample: held, .. }) => {
                sample.observed_at >= held.observed_at
            }
            // A reconnect restarts the credential generation at 1 under a new lifecycle, so the lifecycle is read
            // first, and under one lifecycle a stale credential's verdict never replaces a current one's. A refusal
            // held under the same generations is stamped when the loop drains it, after any retry already queued
            // beside it went out, so that retry's success clears nothing and never replaces it.
            (
                Report::AccountState {
                    state,
                    lifecycle_generation,
                    credential_generation,
                    dispatched_ordinal,
                    ..
                },
                Report::AccountState {
                    state: held_state,
                    lifecycle_generation: held_lifecycle,
                    credential_generation: held_credential,
                    ..
                },
            ) => {
                let generations = (lifecycle_generation, credential_generation);
                let held_generations = (held_lifecycle, held_credential);
                let stale_retry = generations == held_generations
                    && *held_state == AccountState::FundingRequired
                    && *state != AccountState::FundingRequired
                    && dispatched_ordinal.is_some();
                !stale_retry && generations >= held_generations
            }
            (Report::Credits { state, .. }, Report::Credits { state: held, .. }) => {
                state.observed_at >= held.observed_at
            }
            (Report::Overage { evidence, .. }, Report::Overage { evidence: held, .. }) => {
                evidence.observed_at >= held.observed_at
            }
            (
                Report::SpendHead { observed_at, .. },
                Report::SpendHead {
                    observed_at: held, ..
                },
            ) => observed_at >= held,
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
        scope: CooldownScope,
        reply: oneshot::Sender<bool>,
    },
    /// A replacing reading landed in the pending map; the loop drains the map on every turn whether or not the
    /// marker reached it.
    Coalesced,
    Cooldowns {
        reply: oneshot::Sender<Vec<CooldownRow>>,
    },
    #[cfg(any(test, feature = "debug-hooks"))]
    FastBurns {
        reply: oneshot::Sender<HashMap<PinKey, PinBurn>>,
    },
    FundingRetries {
        reply: oneshot::Sender<Vec<(String, i64)>>,
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
    PinsStanding {
        reply: oneshot::Sender<Vec<PinStanding>>,
    },
    Projections {
        pool_id: String,
        reply: oneshot::Sender<Vec<AccountProjection>>,
    },
    /// A request the relay refused before asking for a selection. The loop records it as the request's decision.
    RecordRefusal {
        request: RefusedRequest,
        refusal: RefusalFacts,
    },
    ReleasePurge,
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
    #[cfg(any(test, feature = "debug-hooks"))]
    StampProjectionOver {
        pool_id: String,
        account_id: String,
        window: String,
        at: i64,
        reset: Option<i64>,
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
    ReportedSamples {
        reply: oneshot::Sender<Vec<(String, UtilizationSample)>>,
    },
    SpendWindows {
        reply: oneshot::Sender<Vec<(String, SpendWindow)>>,
    },
    /// The funding marks a writer wake finished, sent ahead of the reload a landed one asks for.
    FundingWritten {
        rows: Vec<FundingWritten>,
    },
    /// The account-state rows a writer wake moved, sent ahead of the reload the move asks for.
    StateMoved {
        rows: Vec<MovedState>,
    },
    /// Boxed, since a failed-over request carries a whole pin row and the mailbox holds every command at the largest
    /// variant's size.
    Select {
        request: Box<SelectRequest>,
        reply: oneshot::Sender<SelectAnswer>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    /// A finished registry load, sent back by the task that ran it; the loop applies the swap.
    Swap {
        loaded: Result<(RegistryLoad, crate::CompiledGuardrails, [u8; 32]), String>,
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
    /// Loaded pins whose mark stands: made on the account the pin sits on.
    pub shed_marks: usize,
    /// Round rows whose time lies past their pool's dwell at start, which the purge drops once no pool's dwell reads
    /// them.
    pub shed_rounds_stale: usize,
}

impl LoadCounts {
    fn of(load: &RoutingLoad, snapshot: &RegistrySnapshot, now: i64) -> LoadCounts {
        // A row of a pool the registry no longer holds has no dwell left to be read within.
        let within_dwell = |row: &ShedRoundRow| {
            snapshot
                .pool(&row.pool_id)
                .is_some_and(|pool| round_in_dwell(pool, row.round_at, now))
        };
        LoadCounts {
            pins: load.pins.len(),
            windows: load
                .windows
                .iter()
                .map(|row| row.account_id.as_str())
                .collect::<HashSet<_>>()
                .len(),
            affinity_entries: load.affinity.len(),
            shed_marks: load.pins.iter().filter(|pin| mark_holds(pin)).count(),
            shed_rounds_stale: load
                .shed_rounds
                .iter()
                .filter(|row| !within_dwell(row))
                .count(),
        }
    }
}

/// One account's projection per plan window, read under the state lock at one instant, and its Pace score, all with the
/// settings of the pool the ask named.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountProjection {
    pub account_id: String,
    pub windows: BTreeMap<String, WindowProjection>,
    /// `pace_of`'s score whatever the account's kind: the spare rate on `seven_day` for an OAuth account, the
    /// remaining worst bucket for an api_key account (whose `windows` is empty).
    pub pace_score: f64,
    /// The slow-down entry in force for the account at the ask's instant, read through `slow_down_in_force`: `None`
    /// where the pool's switch is off, the account is an api_key one, an untiered one or one with no 5-hour reading,
    /// the entry was read before its window's passed reset, or no pinned selection has read it since the last purge or
    /// restart.
    pub slow_down: Option<AccountSlowDown>,
    /// What the selector holds against the account for a request on no model in particular, with no account tried
    /// and the fast lane off, `None` where it would place on the account at the ask's instant.
    pub obstacle: Option<Obstacle>,
}

/// The slow-down entry an account's last pinned selection wrote, with the pool's hold cap: the figures the holds apply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccountSlowDown {
    pub factor: f64,
    /// The realized speed the demand divides the pins' burn by.
    pub speed: f64,
    /// When the factor first read under 1 since it last read 1, `None` outside a span.
    pub since: Option<i64>,
    /// The share of the factor the holds apply at the ask's instant, by the selection's own rule, `None` outside a
    /// span.
    pub ramp: Option<f64>,
    pub stretch_ms: i64,
    pub threshold_ms: i64,
    pub hold_ms: i64,
}

/// One plan window of an account's projection: a window the account holds no reading of has none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowProjection {
    pub predicted: f64,
    /// At the horizon.
    pub projection: f64,
    pub horizon: i64,
    /// Share per hour, the account's `burn_shares` on the window.
    pub burn: f64,
    /// The pins' burn over the speed with the live bookings' burn undivided, in share per hour.
    pub demand: f64,
    /// `remaining / hours - burn` on this window, what Pace scores a candidate by for a model that draws it.
    pub spare_rate: f64,
}

/// A pin with its burn decayed to now and its runway on its account: the time until the account's projection reaches
/// the ceiling on the window that binds among those the pin's model draws.
#[derive(Clone, Debug, PartialEq)]
pub struct PinStanding {
    /// The pin with its four burn copies decayed with its pool's burn average and `burn_at` the instant they are
    /// decayed to. A pin never folded keeps its burn and its `burn_at` as stored.
    pub pin: PinRow,
    pub runway_ms: Option<i64>,
    /// `None` where the pin's account has no projection on any window the model draws (`runway_ms` null too),
    /// `Some(false)` for an unbounded runway, one no burn bounds or one past its window's reset on every window
    /// (`runway_ms` null), `Some(true)` for a bounded one (`runway_ms` set).
    pub runway_bounded: Option<bool>,
    /// The window the runway reads, `None` unless it is bounded.
    pub binding: Option<String>,
    /// The account's factor from the slow-down entry in force, `None` without one.
    pub factor: Option<f64>,
    /// The speed the account's holds realized, from the same entry, `None` where `factor` is.
    pub speed: Option<f64>,
    /// The pin's cycle from its pace, `None` until two requests of the pin after its placement have measured a gap
    /// under the pool's burn average.
    pub cycle_ms: Option<f64>,
    /// Only auxiliary requests have used the pin since its placement, read from the row.
    pub auxiliary_only: bool,
    /// The pin's stored effort, `None` for `""`.
    pub effort: Option<String>,
    /// The first placement of the pin's agent under the key and session, `None` where no record stands.
    pub first_seen_at: Option<i64>,
    /// The first placement of the session's main agent, `None` where main never placed a pin.
    pub session_first_seen_at: Option<i64>,
}

#[derive(Clone)]
pub struct RouterHandle {
    affinity: Arc<RwLock<AffinityMap>>,
    /// The reports the closed queue refused, one counter per kind at its position in `Report::KINDS`; the
    /// process's alone, zero at start.
    dropped: Arc<[AtomicU64; Report::KINDS.len()]>,
    /// The refusal records a full or closed mailbox refused, counted for this process alone from zero at start.
    dropped_refusals: Arc<AtomicU64>,
    events: broadcast::Sender<RouterEvent>,
    in_flight: InFlight,
    load_counts: LoadCounts,
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
        current(&self.snapshot)
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

    /// Lets the first purge run, under `RouterConfig::hold_first_purge`. A stopped task takes nothing.
    pub async fn release_purge(&self) {
        let _ = self.tx.send(Command::ReleasePurge).await;
    }

    /// What changed in the routing state, as it changes. A receiver that falls behind loses the oldest
    /// events and is told so; the task never waits on a receiver.
    pub fn subscribe_events(&self) -> broadcast::Receiver<RouterEvent> {
        self.events.subscribe()
    }

    pub async fn select(&self, request: SelectRequest) -> Result<SelectAnswer, RouterGone> {
        self.ask(|reply| Command::Select {
            request: Box::new(request),
            reply,
        })
        .await
    }

    /// A request the relay refused before asking for a selection, recorded as the request's one decision: sends
    /// `Command::RecordRefusal { request, refusal }` through `try_send` and never waits for mailbox room, so the
    /// 403 toward the client is not delayed by a full router queue. A full or closed mailbox drops the record,
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

    /// The refusal records a full or closed mailbox refused, beside `dropped_reports`. It counts this process's
    /// alone, from zero at start.
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

    /// Resolves true once the task has applied the report numbered `seq`, false at `deadline`.
    pub async fn wait_for_report(&self, seq: u64, deadline: Instant) -> bool {
        wait_until(
            &self.reports.applied_notify,
            Some(deadline),
            || self.applied_report() >= seq,
            || {
                #[cfg(any(test, feature = "debug-hooks"))]
                self.reports.waiter_registered.notify_one();
            },
        )
        .await
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
    pub async fn clear_cooldown(
        &self,
        account_id: &str,
        scope: CooldownScope,
    ) -> Result<bool, RouterGone> {
        let a = account_id.to_string();
        self.ask(|reply| Command::ClearCooldown {
            account_id: a,
            scope,
            reply,
        })
        .await
    }
    pub async fn pins(&self) -> Result<Vec<PinRow>, RouterGone> {
        self.ask(|reply| Command::Pins { reply }).await
    }
    /// The pins with their standing, the one ask the sessions list makes.
    pub async fn pins_standing(&self) -> Result<Vec<PinStanding>, RouterGone> {
        self.ask(|reply| Command::PinsStanding { reply }).await
    }
    /// Every member of `pool_id` with its projections, its Pace score, its slow-down entry in force and its obstacle
    /// under that pool's settings (the pins the pool keeps, its burn average, its ceiling), one ask, one instant, by
    /// account: an api_key account answers its score alone, and a pool the registry does not hold answers none.
    pub async fn projections(&self, pool_id: &str) -> Result<Vec<AccountProjection>, RouterGone> {
        let pool_id = pool_id.to_string();
        self.ask(|reply| Command::Projections { pool_id, reply })
            .await
    }
    /// Each pin's fast usage by key, the row's own copy, for a test or a debug hook that reads a pin's burn apart
    /// from its row. A pin without fast usage has no entry. The fast-mode tests probe the copy through it and no
    /// production path reads it, so it is compiled under `test` and the `debug-hooks` feature alone.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub async fn fast_burns(&self) -> Result<HashMap<PinKey, PinBurn>, RouterGone> {
        self.ask(|reply| Command::FastBurns { reply }).await
    }
    /// The live pins counted per pool by the loop itself, sorted by pool: the metrics scrape reads the counts
    /// rather than a copy of the pin table.
    pub async fn pins_by_pool(&self) -> Result<Vec<(String, u64)>, RouterGone> {
        self.ask(|reply| Command::PinsByPool { reply }).await
    }
    pub async fn cooldowns(&self) -> Result<Vec<CooldownRow>, RouterGone> {
        self.ask(|reply| Command::Cooldowns { reply }).await
    }
    /// Each `funding_required` account's retry instant as `(account_id, retry_at)`, sorted by account. A marked
    /// account has none after a restart until its first retry claims it.
    pub async fn funding_retries(&self) -> Result<Vec<(String, i64)>, RouterGone> {
        self.ask(|reply| Command::FundingRetries { reply }).await
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
    /// One ask for every account's stored sample, each window as the poll or a head reported it with its own reset:
    /// no charge added and no passed reset rolled, for a view that shows the reading itself.
    pub async fn reported_samples(&self) -> Result<Vec<(String, UtilizationSample)>, RouterGone> {
        self.ask(|reply| Command::ReportedSamples { reply }).await
    }
    /// Every usage-based account's spend window at the router's clock.
    pub async fn spend_windows(&self) -> Result<Vec<(String, SpendWindow)>, RouterGone> {
        self.ask(|reply| Command::SpendWindows { reply }).await
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
/// carries no way to stall the task's reports, to put an event of its own on the task's broadcast or to stamp a
/// window of the projection test.
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

    /// Stamps `window` of the projection test on the account as over the ceiling since `at` under the reading's
    /// `reset`, as `RoutingState::stamp_projection_over` does, answering once the task has the stamp, so a selection
    /// sent after it reads it.
    pub async fn stamp_projection_over(
        &self,
        pool_id: &str,
        account_id: &str,
        window: &str,
        at: i64,
        reset: Option<i64>,
    ) {
        self.ask(|reply| Command::StampProjectionOver {
            pool_id: pool_id.to_string(),
            account_id: account_id.to_string(),
            window: window.to_string(),
            at,
            reset,
            reply,
        })
        .await
        .expect("the router task is running");
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
        self.affinity
            .read()
            .expect("affinity lock poisoned")
            .conversation(query, |account_id| snapshot.incarnation(account_id))
    }

    fn predecessor(&self, query: &PredecessorQuery<'_>) -> Option<String> {
        let snapshot = self.snapshot();
        self.affinity
            .read()
            .expect("affinity lock poisoned")
            .predecessor(query, |account_id| snapshot.incarnation(account_id))
    }
}

struct Task {
    cancel: HashMap<String, CancellationToken>,
    clock: Clock,
    config: RouterConfig,
    /// The writes the full channel could not take; while it holds any, every new write folds into it.
    deferred: Deferred,
    events: broadcast::Sender<RouterEvent>,
    /// The loop's own way back into its mailbox, weak so it never keeps its receiver alive: upgraded for the one
    /// load a task runs, and dropped with that task.
    handle_tx: mpsc::WeakSender<Command>,
    /// The pin a failover invalidated, under its key. The next select for that same key within
    /// `MIGRATION_WINDOW_MS` lands on the row's figures and reports a migration rather than a fresh pin, and the purge
    /// drops older entries.
    invalidated: HashMap<PinKey, Invalidated>,
    /// The reloads the running load answers, those that were waiting when it started, and `None` while no registry
    /// load runs. At most one does.
    load_waiters: Option<Vec<oneshot::Sender<Result<(), String>>>>,
    /// While set the loop skips the purge tick, so no purge runs. `Command::ReleasePurge` clears it, and the first
    /// purge runs at the first tick after it.
    purge_held: bool,
    /// Whether a purge ran since the start: the first one announces the restored state again.
    purged: bool,
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
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    /// The reset each account and window's wall was announced for, `None` where the reading named no reset, as a
    /// 5-hour window open past its reset does, beside the instant of the announcement, or of the reset's passing for a
    /// wall that went on past it: a charge, a booking or a sample announces a wall once per reset cycle, and the sample
    /// that confirms it names that cycle's reset and announces nothing.
    spent_announced: HashMap<(String, String), (Option<i64>, i64)>,
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
    let registry = store.load_registry()?;
    // Built here as well as on each reload, else no guardrail would run between the start and the first reload.
    let guardrails = crate::CompiledGuardrails::build(&registry);
    let initial = Arc::new(
        RegistrySnapshot::from_load(registry, store.digest_key(), now, 1)
            .with_guardrails(guardrails),
    );
    // Here and never in the load, which a reader may take while a failover's landing is a selection away and the
    // session holds no pin.
    store.sweep_first_seen()?;
    let load = store.load_routing()?;
    let load_counts = LoadCounts::of(&load, &initial, now);
    let mut state = RoutingState::from_load(load);
    state.drop_usage_based_overage(&initial);
    let snapshot = Arc::new(RwLock::new(initial));
    // A restored wall keeps the anchor its stored pair gives it: without the seed every within-margin head after a
    // restart would move the clamp's anchor onto its own instant and hold the account spent past the window's length.
    let spent_announced: HashMap<(String, String), (Option<i64>, i64)> = state
        .samples()
        .into_iter()
        .flat_map(|(account_id, sample)| {
            sample
                .windows
                .into_iter()
                .filter_map(move |(window, reading)| {
                    (is_plan_window(&window) && reading.utilization >= 1.0).then(|| {
                        (
                            (account_id.clone(), window),
                            (reading.reset, reading.observed_at),
                        )
                    })
                })
        })
        .collect();
    // One table, shared by the handle (the relay's synchronous permits and tickets) and the state (the
    // selector's cap partition), so a count is exact wherever it is read.
    let in_flight = InFlight::default();
    state.set_in_flight(in_flight.clone());
    state.set_rates(config.rates.clone());
    state.set_share(config.share.clone());
    // Shared the same way, so the relay's conversation climb reads the map under the same lock the report
    // handler writes, without a command round trip.
    let affinity = state.affinity();
    let (tx, mut rx) = mpsc::channel(COMMANDS);
    // Bounded so a stalled store cannot grow the channel without limit; past the bound the loop keeps the writes in
    // `deferred`, coalesced by row, which the state's own caps bound.
    let (writer, mut writes) = mpsc::channel::<Vec<StateWrite>>(WRITER_DEPTH);
    let (done_tx, done_rx) = oneshot::channel();
    let (events, _) = broadcast::channel(EVENTS);
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
            // reload once the write is on disk rather than before. The rows whose state it moved go first, so their
            // events leave ahead of the snapshot that shows them.
            let marks = writer.wake(batch).await;
            if marks.landed || !marks.funding.is_empty() {
                if let Some(tx) = writer_tx.upgrade() {
                    // Ahead of the reload, so the swap it leads to finds each funding retry's write acknowledged.
                    if !marks.funding.is_empty() {
                        let _ = tx
                            .send(Command::FundingWritten {
                                rows: marks.funding,
                            })
                            .await;
                    }
                    if marks.landed {
                        if !marks.moved.is_empty() {
                            let _ = tx.send(Command::StateMoved { rows: marks.moved }).await;
                        }
                        let (reply, _rx) = oneshot::channel();
                        let _ = tx.send(Command::Reload { reply }).await;
                    }
                }
            }
        }
        writer.retry().await;
        let _ = done_tx.send(());
    });
    let purge_held = config.hold_first_purge;
    let mut task = Task {
        cancel: HashMap::new(),
        clock,
        config,
        deferred: Deferred::default(),
        events,
        handle_tx: tx.downgrade(),
        invalidated: HashMap::new(),
        load_waiters: None,
        #[cfg(any(test, feature = "debug-hooks"))]
        parked: None,
        purge_held,
        purged: false,
        pending,
        reload_waiters: Vec::new(),
        reports,
        reset_announced: HashMap::new(),
        snapshot,
        spent_announced,
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
                            if matches!(
                                command,
                                Command::Report(..)
                                    // A resolution is a report with an answer: it lands with the drain rather
                                    // than leaving the catalog cache to publish the spelling again after a restart.
                                    | Command::Resolve { .. }
                                    // The mailbox took it, so it is emitted rather than lost uncounted.
                                    | Command::RecordRefusal { .. }
                                    | Command::StateMoved { .. }
                            ) {
                                task.handle(command).await;
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
                _ = purge.tick(), if !task.purge_held => task.purge(),
                // A slot the writer frees takes the backlog at the top of the next turn, so a store that recovers
                // clears `degraded` without waiting on a command or the purge tick.
                _ = task.writer_room(), if !task.deferred.is_empty() => {}
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
/// identity after the write and the cause the write stored.
struct MovedState {
    account_id: String,
    state: AccountState,
    incarnation: u64,
    revision: u64,
    cause: StateCause,
    detail: Option<String>,
}

/// A funding mark's write as the writer finished it, for the retry entry `ordinal` names: the row's revision after
/// it, `None` when the fences refused it.
struct FundingWritten {
    account_id: String,
    incarnation: u64,
    ordinal: u64,
    revision: Option<u64>,
}

/// What a wake's account-state writes did: whether any passed its fences, which the snapshot is rebuilt from even
/// when the state was already the one written, the rows whose state moved, which alone are transitions, and the
/// funding marks it finished, landed or refused.
#[derive(Default)]
struct Marks {
    landed: bool,
    moved: Vec<MovedState>,
    funding: Vec<FundingWritten>,
}

impl Marks {
    fn absorb(&mut self, other: Marks) {
        self.landed |= other.landed;
        self.moved.extend(other.moved);
        self.funding.extend(other.funding);
    }

    /// One state write's marks from the store's answer: the row's revision after it and the state before it, `None`
    /// when the fences refused it.
    fn of(write: &StateWrite, answer: Option<(u64, AccountState)>) -> Marks {
        let StateWrite::AccountState {
            account_id,
            state,
            incarnation,
            cause,
            detail,
            marked_ordinal,
            ..
        } = write
        else {
            return Marks::default();
        };
        let funding = (*state == AccountState::FundingRequired)
            .then(|| FundingWritten {
                account_id: account_id.clone(),
                incarnation: *incarnation,
                // An unstamped mark, a superseded verdict's, names no entry: an account's ordinals start at 1, so
                // no entry carries the ordinal 0 and `funding_written` credits nothing for it.
                ordinal: marked_ordinal.unwrap_or(0),
                revision: answer.map(|(revision, _)| revision),
            })
            .into_iter()
            .collect();
        let Some((revision, before)) = answer else {
            return Marks {
                funding,
                ..Marks::default()
            };
        };
        Marks {
            landed: true,
            funding,
            moved: (before != *state)
                .then(|| MovedState {
                    account_id: account_id.clone(),
                    state: *state,
                    incarnation: *incarnation,
                    revision,
                    cause: *cause,
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
    /// Lands what an earlier wake left to try again, then `batch`, and answers what their account-state marks did.
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
                             unapplied, after {skipped} refused, so they are dropped"
                        );
                    } else {
                        tracing::error!(
                            error = %stopped,
                            refused = refused.as_ref().map(tracing::field::display),
                            batch = size,
                            skipped,
                            left,
                            "a database-wide failure left {left} of {size} routing state writes unapplied, \
                             after {skipped} refused, so they are tried once more at the next wake"
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
        match store.write_routing(|tx| apply_in(tx, std::slice::from_ref(&write))) {
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
                cause,
                ..
            } => {
                let answer = tx.set_state_if_generations(
                    account_id,
                    *state,
                    *cause,
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
            StateWrite::DeleteFirstSeen {
                key_id,
                session_id,
                agent_id,
            } => {
                tx.delete_first_seen_record(key_id, session_id, agent_id)?;
            }
            StateWrite::DeletePin(key) => {
                tx.delete_pin(key)?;
            }
            StateWrite::DeleteWindow {
                account_id,
                window,
                incarnation,
            } => {
                tx.delete_window(account_id, window, *incarnation)?;
            }
            StateWrite::EndPin(key) => {
                tx.end_pin(key)?;
            }
            StateWrite::InsertFirstSeen(row) => tx.insert_first_seen(row)?,
            StateWrite::PurgeShedRounds { before } => {
                tx.purge_shed_rounds_before(*before)?;
            }
            StateWrite::PurgeStaleWindows => {
                tx.purge_stale_windows()?;
            }
            StateWrite::Rekey(rekey) => tx.rekey_resolution(rekey)?,
            StateWrite::ShedRound {
                pool_id,
                account_id,
                incarnation,
                round_at,
            } => tx.replace_shed_round(pool_id, account_id, *incarnation, *round_at)?,
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
            StateWrite::UpsertWindowSet(row) => {
                tx.upsert_window_set(row)?;
            }
        }
    }
    Ok(marks)
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

/// The rows a completed answer wrote, for the decision that selected it.
fn cache_map_written(report: &AffinityReport, rows: &[AffinityRow]) -> RouterEvent {
    let entries = rows
        .iter()
        .map(|row| CacheMapEntry {
            tier: row.chain.as_str(),
            depth: row.depth,
            prefix_tokens: row.prefix_tokens,
            ttl_ms: row.ttl_ms,
            account_id: row.account_id.clone(),
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

/// The `mark_cleared` of a pin that left its account without a move, when its mark stood: a release, an expiry, a
/// fail-over's invalidation or a rekey's merge the row lost. `None` for an unmarked pin.
fn released_mark(row: &PinRow, at: i64) -> Option<RouterEvent> {
    let mark = standing_mark(row)?;
    Some(RouterEvent::MarkCleared {
        account_id: row.account_id.clone(),
        key_id: row.key_id.clone(),
        session_id: row.session_id.clone(),
        agent_id: row.agent_id.clone(),
        model: row.model.clone(),
        reason: "released",
        marked_at: mark.at,
        marked_on: mark.on.clone(),
        marked_window: mark.window.clone(),
        projection: None,
        stretch_ms: None,
        threshold_ms: None,
        at,
    })
}

/// The fit of a move's destination as the decision's candidate table holds it: what the move's gate read there, or
/// the fresh placement's fit for a fail-over's landing. `None` throughout for a member the table does not list.
fn destination_fit(
    record: &DecisionRecord,
    account_id: &str,
) -> (Option<&'static str>, Option<i64>, Option<String>) {
    record
        .candidates
        .iter()
        .find(|candidate| candidate.account_id == account_id)
        .map_or((None, None, None), |candidate| {
            (
                candidate.fit,
                candidate.runway_ms,
                candidate.binding.clone(),
            )
        })
}

/// Whether a reading at `now` naming `reset` is in the cycle of a wall `announced` as `(reset, at)` on `window`. Two
/// named resets are one within `RESET_SLACK_MS`, as `announce_reset` compares them. A reading with none over a wall
/// announced with a reset is of that reset's cycle at or before the reset, with the same slack past it, and of another
/// cycle after. A wall announced with none was in a window open past its stored reset, which opened by `at` and runs
/// the window's length at most, so a reset inside that length after `at` names it, and a second reading with none
/// inside it is the same open window. Both bounds, the named reset's and the reading with none, take the same slack,
/// since the request that opened the window can reach the upstream after `at`.
fn same_cycle(announced: (Option<i64>, i64), reset: Option<i64>, now: i64, window: &str) -> bool {
    let (announced_reset, at) = announced;
    let open_until = at.saturating_add(window_length_ms(window));
    let bound = open_until.saturating_add(RESET_SLACK_MS);
    match (announced_reset, reset) {
        (Some(announced), Some(reset)) => same_reset(announced, reset),
        (None, Some(reset)) => at <= reset && reset <= bound,
        (None, None) => now <= bound,
        (Some(announced), None) => now <= announced.saturating_add(RESET_SLACK_MS),
    }
}

/// The reset `reading` stands under: its own, or where it names none and was taken before `stored`'s reset passed,
/// the stored one's, since such a reading is of the stored reset's cycle. A reading with none taken past the stored
/// reset is another cycle, as `window_edges` reads that reset passed.
fn reset_under(stored: Option<&WindowReading>, reading: &WindowReading) -> Option<i64> {
    match (reading.reset, stored.and_then(|stored| stored.reset)) {
        (None, Some(stored_reset)) if reading.observed_at < stored_reset => Some(stored_reset),
        (reset, _) => reset,
    }
}

/// A pin event as the decision names it: a move with its reason, an expiry with the first reason the task read
/// against the pin.
fn pin_event_facts(event: &RouterEvent) -> Option<PinEventFacts> {
    let (kind, reason) = match event {
        RouterEvent::PinCreated { .. } => (PIN_CREATED, None),
        RouterEvent::PinMigrated { reason, .. } => (PIN_MIGRATED, Some(reason.clone())),
        RouterEvent::PinExpired { reasons, .. } => {
            (PIN_EXPIRED, reasons.first().map(|r| r.to_string()))
        }
        _ => return None,
    };
    Some(PinEventFacts { kind, reason })
}

/// The members of `pool_id` in account order, each with its projection per plan window and its Pace score at `now`
/// under that pool's settings: the windows from one `projections_of` read on the achieved burn and one on the demand,
/// so each window is built once per read, with `pace_of`'s spare rate on each, the score from `pace_of` on
/// `seven_day`, the window Pace reads for a model without a week of its own, the slow-down entry in force, which
/// the last pinned selection on the account wrote, and the selector's obstacle on the account. Empty for a pool the
/// snapshot does not hold.
fn projections(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool_id: &str,
    now: i64,
    fallback_ms: i64,
) -> Vec<AccountProjection> {
    let Some(pool) = snapshot.pool(pool_id) else {
        return Vec::new();
    };
    let mut members: Vec<(&str, u32)> = snapshot
        .members(pool_id)
        .iter()
        .map(|m| (m.account_id.as_str(), m.priority))
        .collect();
    members.sort();
    members.dedup_by(|a, b| a.0 == b.0);
    members
        .into_iter()
        .map(|(account_id, priority)| {
            let candidate = Candidate {
                account_id: account_id.to_string(),
                incarnation: snapshot.incarnation(account_id).unwrap_or(0),
                priority,
                upstream_model: String::new(),
            };
            let tier = snapshot.priced_tier(account_id).unwrap_or_default();
            let achieved = projections_of(
                state,
                snapshot,
                pool,
                account_id,
                tier,
                now,
                BurnRead::Achieved,
                None,
            );
            let demands = projections_of(
                state,
                snapshot,
                pool,
                account_id,
                tier,
                now,
                BurnRead::Demand,
                None,
            );
            let windows = all_plan_windows()
                .filter_map(|window| {
                    let projection = achieved.get(window)?;
                    let demand = demands.get(window)?;
                    Some((
                        window.to_string(),
                        WindowProjection {
                            predicted: projection.predicted,
                            projection: projection.at_horizon,
                            horizon: projection.horizon,
                            burn: projection.burn * 3_600.0,
                            demand: demand.burn * 3_600.0,
                            spare_rate: pace_of(
                                state, snapshot, pool, &candidate, window, now, None,
                            )
                            .score,
                        },
                    ))
                })
                .collect();
            AccountProjection {
                account_id: account_id.to_string(),
                windows,
                pace_score: pace_of(state, snapshot, pool, &candidate, SEVEN_DAY, now, None).score,
                slow_down: state
                    .slow_down_in_force(pool, account_id, now)
                    .map(|entry| AccountSlowDown {
                        factor: entry.factor,
                        speed: entry.speed,
                        since: entry.slowed_since,
                        ramp: entry.slowed_since.map(|_| {
                            let at_ceiling =
                                state.predicted(account_id, FIVE_HOUR, now).is_some_and(
                                    |reading| reading.utilization >= ceiling(pool, FIVE_HOUR),
                                );
                            slow_down_ramp(
                                Some(entry),
                                at_ceiling,
                                entry.factor,
                                burn_average_ms(pool),
                                now,
                            )
                        }),
                        stretch_ms: entry.stretch_ms,
                        threshold_ms: entry.threshold_ms,
                        hold_ms: slow_down_hold_ms(pool),
                    }),
                obstacle: obstacle(
                    state,
                    snapshot,
                    pool,
                    account_id,
                    state.blocking_cooldown(account_id, "", now, false),
                    &[],
                    now,
                    fallback_ms,
                    false,
                ),
            }
        })
        .collect()
}

/// Every pin with its burn decayed to `now` by its pool's burn average (the default preset's for a key whose pool left
/// the registry) and its runway on its own account under that pool, read by `standing_runway` once per pool, account
/// and set of windows, since the account's projection already holds every pin's burn and every pin drawing those
/// windows there reads the same runway. Each pin reads its account's factor and realized speed from the slow-down entry
/// in force under that pool, its cycle from its pace, and its agent's and its session's main agent's first placements.
fn pins_standing(state: &RoutingState, snapshot: &RegistrySnapshot, now: i64) -> Vec<PinStanding> {
    type Runway = Option<(Option<i64>, Option<String>)>;
    let mut runways: HashMap<(String, String, Vec<&'static str>), Runway> = HashMap::new();
    state
        .pins()
        .into_iter()
        .map(|pin| {
            let pool = snapshot.key_pool(&pin.key_id);
            let average_ms = pool.map_or_else(default_burn_average_ms, burn_average_ms);
            let runway = pool.and_then(|pool| {
                runways
                    .entry((
                        pool.id.clone(),
                        pin.account_id.clone(),
                        plan_windows(&pin.model).collect(),
                    ))
                    .or_insert_with(|| {
                        standing_runway(state, snapshot, pool, &pin.account_id, &pin.model, now)
                    })
                    .clone()
            });
            let (runway_ms, runway_bounded, binding) = match runway {
                None => (None, None, None),
                Some((runway_ms, binding)) => (runway_ms, Some(runway_ms.is_some()), binding),
            };
            let slow_down = pool
                .and_then(|pool| state.slow_down_in_force(pool, &pin.account_id, now))
                .map(|entry| (entry.factor, entry.speed));
            let cycle_ms = state.pin_pace(&pin.key()).and_then(|pace| pace.cycle_ms);
            let auxiliary_only = state.auxiliary_only(&pin.key());
            let effort = effort_of(&pin).map(String::from);
            let first_seen_at = state.first_seen(&pin.key_id, &pin.session_id, &pin.agent_id);
            let session_first_seen_at = state.first_seen(&pin.key_id, &pin.session_id, MAIN_AGENT);
            PinStanding {
                pin: decayed(pin, now, average_ms),
                runway_ms,
                runway_bounded,
                binding,
                factor: slow_down.map(|(factor, _)| factor),
                speed: slow_down.map(|(_, speed)| speed),
                cycle_ms,
                auxiliary_only,
                effort,
                first_seen_at,
                session_first_seen_at,
            }
        })
        .collect()
}

/// The pin's four burn copies decayed from their last fold to `now`. Every type decays by one factor, as
/// `burn_shares` decays the share, so pricing the decayed copy equals decaying the priced one.
fn decayed(pin: PinRow, now: i64, average_ms: i64) -> PinRow {
    let Some(at) = pin.burn_at else {
        return pin;
    };
    let factor = burn_decayed(1.0, Some(at), now, average_ms);
    let scale = |burn: PinBurn| PinBurn {
        cache_read: burn.cache_read * factor,
        input: burn.input * factor,
        output: burn.output * factor,
        write_1h: burn.write_1h * factor,
        write_5m: burn.write_5m * factor,
    };
    PinRow {
        burn: scale(pin.burn),
        burn_banded: scale(pin.burn_banded),
        burn_fast: scale(pin.burn_fast),
        burn_unmetered: scale(pin.burn_unmetered),
        // A fold dated past `now` (two relays' clocks) decays nothing, so its burn stands at its own instant.
        burn_at: Some(now.max(at)),
        ..pin
    }
}

/// A standing pin's runway on its own account: per window the model draws, the time `projection_of`'s burn takes to
/// fill `ceiling - predicted` over the band's segments, as the projection beside it weighs them. Its burn already
/// holds the pin, so neither the pin's burn nor a rewrite is added again (that is `fit_of`'s arithmetic for a
/// destination). `Some(0)` where `predicted` already reaches the ceiling, an unbounded runway (`None` with no binding
/// window) where on every drawn window it projects the account's burn is zero or the runway passes the window's reset,
/// since the window resets before the ceiling is reached, and the binding window the shortest of the rest. The outer
/// `None` where the account projects none of the drawn windows: an api_key account, or one with no reading of them.
fn standing_runway(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    model: &str,
    now: i64,
) -> Option<(Option<i64>, Option<String>)> {
    let tier = snapshot.priced_tier(account_id).unwrap_or_default();
    let mut projected = false;
    let mut bounded: Vec<(&str, i64)> = Vec::new();
    for window in plan_windows(model) {
        let Some(projection) = projection_of(
            state,
            snapshot,
            pool,
            account_id,
            tier,
            window,
            now,
            BurnRead::Achieved,
        ) else {
            continue;
        };
        projected = true;
        let limit = ceiling(pool, window);
        let runway_ms = if projection.predicted >= limit {
            0
        } else {
            let Some(runway_ms) = projection.banded.time_to(limit - projection.predicted) else {
                continue;
            };
            runway_ms
        };
        if !passes_reset(runway_ms, projection.reset, now) {
            bounded.push((window, runway_ms));
        }
    }
    projected.then(|| {
        bounded
            .iter()
            .min_by_key(|(_, runway_ms)| *runway_ms)
            .map_or((None, None), |(window, runway_ms)| {
                (Some(*runway_ms), Some(window.to_string()))
            })
    })
}

impl Task {
    fn snapshot(&self) -> Arc<RegistrySnapshot> {
        current(&self.snapshot)
    }

    /// A send with no receiver is an error and means nothing: the loop must never depend on anyone listening.
    fn emit(&self, event: RouterEvent) {
        let _ = self.events.send(event);
    }

    /// The slow-down span edges the state recorded, one event each, and the spans whose factor has read 1 for the
    /// wait by `now`.
    fn slow_down_edges(&mut self, now: i64) {
        for (pool_id, account_id, edge) in self.state.take_slow_down_edges(now) {
            self.emit(match edge {
                SlowDownEdge::Started {
                    factor,
                    stretch_ms,
                    threshold_ms,
                    demand,
                    reset,
                } => RouterEvent::SlowDownStarted {
                    account_id,
                    pool_id,
                    factor,
                    stretch_ms,
                    threshold_ms,
                    demand,
                    reset,
                    at: now,
                },
                SlowDownEdge::Ended {
                    since,
                    duration_ms,
                    factor_min,
                    held_ms,
                } => RouterEvent::SlowDownEnded {
                    account_id,
                    pool_id,
                    since,
                    duration_ms,
                    factor_min,
                    held_ms,
                    at: now,
                },
            });
        }
    }

    /// The decision of a request the relay refused before asking for a selection, built on the current snapshot.
    fn record_refusal(&self, request: &RefusedRequest, refusal: &RefusalFacts) {
        let snapshot = self.snapshot();
        let (record, summary) = DecisionRecord::refused(request, &snapshot, refusal);
        self.emit(RouterEvent::Decision {
            summary: Arc::new(summary),
            record: Arc::new(record),
            at: request.at,
        });
    }

    /// One transition per account-state row the writer moved, with the cause the write stored.
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
        refusal: Refusal,
        reply: oneshot::Sender<SelectAnswer>,
    ) {
        self.record_refusal(&RefusedRequest::from(request), &refusal.facts());
        let _ = reply.send(SelectAnswer {
            result: SelectResult::Refused(refusal),
            cancel: CancellationToken::new(),
            ticket: None,
            funding_ordinal: None,
        });
    }

    /// Dropping the sender ends the writer's loop; waiting on `writer_done` is what makes an acknowledged pin or
    /// cooldown durable before the process exits.
    async fn drain_writer(&mut self) {
        if !self.deferred.is_empty() {
            let backlog = self.deferred.take();
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
                    "the routing state writer has stopped, so the writes held in memory are lost"
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
                let snapshot = self.snapshot();
                // Keyed on the dated id from the first request after a resolution lands, so a fresh pin and a
                // fresh cooldown never start a second life under the spelling the resolution just retired.
                request.canonical_model =
                    self.state.canonical(&request.canonical_model).to_string();
                // The lane's model part with it: admitted before the resolution, the lane still names the spelling
                // the pin was relaned from, and would read that pin as another cache.
                request.lane = self.state.canonical_lane(&request.lane);
                if request.revision < snapshot.revision {
                    // The registry moved after admission: the key, the pool and the candidates are re-derived from what
                    // is current, and a request the current registry would refuse is refused.
                    match rederived(&snapshot, &request, (self.clock)()) {
                        Ok(candidates) => {
                            request.candidates = candidates;
                            request.revision = snapshot.revision;
                        }
                        Err(refusal) => {
                            self.refuse(&request, refusal, reply);
                            return;
                        }
                    }
                }
                // The client's classifier call is routed as the agent of the turn it judges (`routed_agent`), so the
                // pin the facts and the events below read is that agent's.
                if let Some(agent_id) = routed_agent(&self.state, &request) {
                    request.agent_id = agent_id;
                }
                let pin_key = request.pin_key();
                let before = self.state.pin(&pin_key).cloned();
                // A failover inside the window lands on the row its source lost: the selection prices the session's
                // own burn and names the cause on the record. A key that holds a pin again is no landing, and the
                // entry stays until a selection lands the session, so a held one leaves it for the next.
                request.failed_over = self
                    .invalidated
                    .get(&pin_key)
                    .filter(|_| before.is_none())
                    .filter(|entry| request.now - entry.at <= MIGRATION_WINDOW_MS)
                    .map(|entry| FailedOver {
                        from_account_id: entry.account_id.clone(),
                        reason: entry.reason,
                        row: entry.row.clone(),
                        cause: entry.cause.clone(),
                    });
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
                // A round that marked the request's own pin wrote that row ahead of the write that replaced it, so a
                // release in the same selection reads the mark off the round's write: the row as it stood before the
                // selection carries none.
                let marked_here = writes.iter().rev().find_map(|write| match write {
                    StateWrite::UpsertPin(row)
                        if row.key_parts() == pin_key.parts()
                            && mark_holds(row)
                            && before.as_ref().is_some_and(|old| old.epoch == row.epoch) =>
                    {
                        Some(row.clone())
                    }
                    _ => None,
                });
                self.write(writes);
                if let Some(pin) = record.pin.as_mut() {
                    pin.released = released.clone();
                }
                // Collected rather than emitted, so the decision naming them goes out first and the ledger lands
                // its row and its pin events in one tick.
                let mut pin_events = Vec::new();
                let after = self.state.pin(&pin_key);
                let now = request.now;
                let selected = matches!(result, SelectResult::Selected(_));
                match (before.as_ref(), after) {
                    (None, Some(new)) if selected => {
                        match self
                            .invalidated
                            .remove(&pin_key)
                            .filter(|entry| now - entry.at <= MIGRATION_WINDOW_MS)
                        {
                            Some(Invalidated {
                                account_id: from,
                                reason,
                                ..
                            }) => {
                                // A hard fail-over's move is named on the attempt row the same way a
                                // selector's move is.
                                if let SelectResult::Selected(selection) = &mut result {
                                    selection.trace.moved = Some(reason);
                                }
                                record.outcome.rule = Some(reason);
                                // The invalidation removed the pin `finish` reads a move's source from, so the
                                // entry it left behind names the account the request left.
                                record.outcome.from_account_id = Some(from.clone());
                                summary.rule = Some(reason);
                                pin_events.push(RouterEvent::pin_migrated(
                                    &pin_key,
                                    from,
                                    new,
                                    reason,
                                    &record,
                                    record.outcome.cause.clone(),
                                    now,
                                ));
                            }
                            None => pin_events.push(RouterEvent::pin_created(
                                &pin_key,
                                &new.account_id,
                                new.epoch,
                                now,
                            )),
                        }
                    }
                    (Some(old), Some(new)) if old.epoch != new.epoch => {
                        if let SelectResult::Selected(selection) = &result {
                            match selection.migrated {
                                None => {
                                    pin_events.push(RouterEvent::pin_expired(
                                        &pin_key,
                                        &old.account_id,
                                        released.clone(),
                                        now,
                                    ));
                                    pin_events.push(RouterEvent::pin_created(
                                        &pin_key,
                                        &new.account_id,
                                        new.epoch,
                                        now,
                                    ));
                                }
                                Some(reason) => pin_events.push(RouterEvent::pin_migrated(
                                    &pin_key,
                                    old.account_id.clone(),
                                    new,
                                    reason.as_str(),
                                    &record,
                                    None,
                                    now,
                                )),
                            }
                        }
                    }
                    (Some(old), None) => pin_events.push(RouterEvent::pin_expired(
                        &pin_key,
                        &old.account_id,
                        released,
                        now,
                    )),
                    _ => {}
                }
                record.outcome.pin_events = pin_events.iter().filter_map(pin_event_facts).collect();
                // The mark the selection cleared (a heal, the move that consumed it, or the pool's switch turned off),
                // else the one a pin released or expired here carried: the pin left without a move, so its mark left
                // with it.
                let mark = before.as_ref().and_then(|old| {
                    let cleared = record.outcome.mark_cleared.as_ref();
                    let account_id = old.account_id.clone();
                    match cleared {
                        Some(facts) => Some(RouterEvent::MarkCleared {
                            account_id,
                            key_id: pin_key.key_id.clone(),
                            session_id: pin_key.session_id.clone(),
                            agent_id: pin_key.agent_id.clone(),
                            model: pin_key.model.clone(),
                            reason: facts.reason,
                            marked_at: facts.marked_at,
                            marked_on: facts.marked_on.clone(),
                            marked_window: facts.marked_window.clone(),
                            projection: facts.projection,
                            stretch_ms: facts.stretch_ms,
                            threshold_ms: facts.threshold_ms,
                            at: request.now,
                        }),
                        None if pin_events
                            .iter()
                            .any(|event| matches!(event, RouterEvent::PinExpired { .. })) =>
                        {
                            released_mark(old, request.now).or_else(|| {
                                marked_here
                                    .as_ref()
                                    .and_then(|row| released_mark(row, request.now))
                            })
                        }
                        None => None,
                    }
                });
                // What the selection booked, read before the record leaves on the broadcast.
                let booked = record
                    .outcome
                    .reservation
                    .as_ref()
                    .map(|booked| (booked.account_id.clone(), booked.rewrite.clone()));
                let round = shed_round.map(|round| RouterEvent::SheddingRound {
                    account_id: round.account_id,
                    pool_id: record.request.pool_id.clone(),
                    request_id: request.request_id.clone(),
                    key_id: round.pin.key_id,
                    session_id: round.pin.session_id,
                    agent_id: round.pin.agent_id,
                    model: round.pin.model,
                    round: Box::new(round.facts),
                    at: request.now,
                });
                self.emit(RouterEvent::Decision {
                    summary: Arc::new(summary),
                    record: Arc::new(record),
                    at: request.now,
                });
                for event in pin_events.into_iter().chain(mark).chain(round) {
                    self.emit(event);
                }
                self.slow_down_edges(request.now);
                if let Some((account_id, rewrite)) = booked {
                    for (window, share) in rewrite {
                        let Some(after) = self.state.predicted(&account_id, &window, request.now)
                        else {
                            continue;
                        };
                        self.predicted_spent(
                            &account_id,
                            &window,
                            after.utilization - share,
                            &request.request_id,
                            request.now,
                        );
                    }
                }
                let (cancel, ticket, funding_ordinal) = match &result {
                    // Charged here, in the one place selections are serialized, so the count the next selection
                    // reads already holds this one: a ticket taken at the dial, awaits later, would let every
                    // selection answered in between read the same count and land over the cap. A token count
                    // takes none: it is bound to no cap.
                    SelectResult::Selected(selection) => {
                        let ticket = (!request.count).then(|| {
                            self.state
                                .in_flight()
                                .dispatched(&selection.account.id, request.now)
                        });
                        // The placed attempt's ticket is the funding retry's claim, so the one slot frees when that
                        // attempt ends, however long its head takes.
                        let funding_ordinal = match (&ticket, selection.account.state) {
                            (Some(ticket), AccountState::FundingRequired) => {
                                Some(self.state.claim_funding_retry(
                                    &selection.account.id,
                                    selection.account.incarnation,
                                    ticket.id(),
                                    request.now,
                                ))
                            }
                            _ => None,
                        };
                        (
                            self.token(&selection.account.id).child_token(),
                            ticket,
                            funding_ordinal,
                        )
                    }
                    SelectResult::Exhausted(_)
                    | SelectResult::Held(_)
                    | SelectResult::Refused(_) => (CancellationToken::new(), None, None),
                };
                let _ = reply.send(SelectAnswer {
                    result,
                    cancel,
                    ticket,
                    funding_ordinal,
                });
            }
            Command::Report(report, seq) => self.numbered_report(report, seq),
            Command::RecordRefusal { request, refusal } => self.record_refusal(&request, &refusal),
            // The top of this turn drained the map already.
            Command::Coalesced => {}
            Command::FundingWritten { rows } => {
                for row in rows {
                    self.state.funding_written(
                        &row.account_id,
                        row.incarnation,
                        row.ordinal,
                        row.revision,
                    );
                }
            }
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
            #[cfg(any(test, feature = "debug-hooks"))]
            Command::StampProjectionOver {
                pool_id,
                account_id,
                window,
                at,
                reset,
                reply,
            } => {
                self.state
                    .stamp_projection_over(&pool_id, &account_id, &window, at, reset);
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
            // The purge tick held since the start is still due, so the loop's next turn runs the first purge.
            Command::ReleasePurge => self.purge_held = false,
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
                if self.load_waiters.is_none() {
                    self.start_load();
                }
            }
            Command::Swap { loaded, started_at } => {
                let answer = loaded.map(|(load, guardrails, digest_key)| {
                    self.swap(load, guardrails, digest_key, started_at)
                });
                for waiter in self.load_waiters.take().into_iter().flatten() {
                    let _ = waiter.send(answer.clone());
                }
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
                // Every agent's pin of the session leaves together: the session's next request starts fresh
                // wherever each of its agents lands, not only its main turn's.
                let gone = self.state.release_session(&key_id, &session_id);
                // The deletes follow the records memory dropped, not the pins the release took, so a subagent whose
                // pin sits in a failover gap loses its row too.
                let deletes = self.first_seen_deletes(&key_id, &session_id, false);
                let count = gone.len();
                let now = (self.clock)();
                for row in &gone {
                    self.emit(RouterEvent::PinReleased {
                        key_id: row.key_id.clone(),
                        session_id: row.session_id.clone(),
                        agent_id: row.agent_id.clone(),
                        model: row.model.clone(),
                        account_id: row.account_id.clone(),
                        at: now,
                    });
                    if let Some(event) = released_mark(row, now) {
                        self.emit(event);
                    }
                }
                self.write(
                    gone.iter()
                        .map(|row| StateWrite::EndPin(row.key()))
                        .chain(deletes)
                        .collect(),
                );
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
            Command::PinsStanding { reply } => {
                let snapshot = self.snapshot();
                let _ = reply.send(pins_standing(&self.state, &snapshot, (self.clock)()));
            }
            Command::Projections { pool_id, reply } => {
                let snapshot = self.snapshot();
                let fallback_ms =
                    i64::try_from(self.config.cooldown_fallback.as_millis()).unwrap_or(i64::MAX);
                let _ = reply.send(projections(
                    &self.state,
                    &snapshot,
                    &pool_id,
                    (self.clock)(),
                    fallback_ms,
                ));
            }
            #[cfg(any(test, feature = "debug-hooks"))]
            Command::FastBurns { reply } => {
                let _ = reply.send(self.state.fast_burns());
            }
            Command::PinsByPool { reply } => {
                let snapshot = self.snapshot();
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
            Command::FundingRetries { reply } => {
                let _ = reply.send(self.state.funding_retries());
            }
            Command::AccountingDegraded { reply } => {
                let _ = reply.send(self.state.accounting_degraded());
            }
            Command::MemberSpends { reply } => {
                let snapshot = self.snapshot();
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
                        let predicted = self.state.predict_sample(&id, read, now);
                        (id, predicted)
                    })
                    .collect();
                let _ = reply.send(samples);
            }
            Command::ReportedSamples { reply } => {
                let _ = reply.send(self.state.samples());
            }
            Command::SpendWindows { reply } => {
                let now = (self.clock)();
                let snapshot = self.snapshot();
                let mut windows: Vec<(String, SpendWindow)> = snapshot
                    .accounts()
                    .filter_map(|account| {
                        self.state
                            .spend_window(&snapshot, &account.id, now)
                            .map(|window| (account.id.clone(), window))
                    })
                    .collect();
                windows.sort_by(|a, b| a.0.cmp(&b.0));
                let _ = reply.send(windows);
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
            Report::Affinity(report) => {
                // The account's next holder passes the report over, so it writes no row and its decision's cache map
                // reads empty, as an answer's that wrote none does.
                if self.moved_past(&report.account_id, report.incarnation) {
                    self.emit(cache_map_written(&report, &[]));
                    return;
                }
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
                let snapshot = self.snapshot();
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
                let fast_lane = match row.scope {
                    CooldownScope::Fast | CooldownScope::FastCredits => true,
                    CooldownScope::Account | CooldownScope::Model(_) => false,
                };
                let moved = if fast_lane { "fast_lane" } else { "cooldown" };
                let cause = MoveCause::Cooldown {
                    scope: row.scope.to_string(),
                    reason: row.reason.clone(),
                    window: Some(row.window.clone()),
                };
                let mut writes = Vec::new();
                if self.state.record_cooldown(row.clone()) {
                    self.emit(RouterEvent::CooldownSet {
                        account_id: account_id.clone(),
                        scope: row.scope.to_string(),
                        until: row.until,
                        reason: row.reason.clone(),
                        window: row.window.clone(),
                        source_request_id: row.source_request_id.clone(),
                        at: (self.clock)(),
                    });
                    writes.push(StateWrite::UpsertCooldown(row));
                }
                if let Some(mut pin) = pin {
                    let key = self.canonical_pin(&mut pin);
                    // A fast-lane row refuses the fast lane alone: once a later turn moved the pin to another
                    // lane, the session no longer asks for what the row refuses, so a late refusal leaves it.
                    let left_lane = fast_lane
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
                    } else {
                        if let Some(row) = self.state.invalidate_pin(&pin, &account_id) {
                            let now = (self.clock)();
                            if let Some(event) = released_mark(&row, now) {
                                self.emit(event);
                            }
                            self.invalidated.insert(
                                key.clone(),
                                Invalidated {
                                    account_id: account_id.clone(),
                                    reason: moved,
                                    at: now,
                                    row,
                                    cause: Some(cause),
                                },
                            );
                            writes.push(StateWrite::DeletePin(key));
                        }
                    }
                }
                self.write(writes);
            }
            Report::Credits {
                account_id,
                mut state,
                incarnation,
            } => {
                let snapshot = self.snapshot();
                if holder(&snapshot, &account_id, incarnation).is_none() {
                    return;
                }
                state.incarnation = incarnation;
                let observed_at = state.observed_at;
                let room = state.blocked().is_none();
                let row = RoutingState::credits_row(&account_id, &state);
                // Read before the fold replaces it, and the holder's own: another holder's state is no edge. The edge's
                // before is the state the selector read at this clock, a lapsed spend share included.
                let before = self.state.credits_of(&account_id, incarnation).cloned();
                let blocked_before = self
                    .state
                    .credits_blocked(&snapshot, &account_id, (self.clock)())
                    .map(String::from);
                let after = state.clone();
                if !self.state.record_credits(&account_id, state) {
                    return;
                }
                if snapshot.usage_based(&account_id) {
                    self.state.record_poll_share(&account_id, observed_at);
                }
                self.write(vec![StateWrite::UpsertCredits(row)]);
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
                // the state that refused the request. Extra usage funds a request past a spent plan window, so the
                // reading lifts the credits row and the spent-window rows: an account rate limit naming a plan
                // window, and a model's rate limit, which on an OAuth account (the only kind the poll reads) is its
                // week's. The fast lane's rate limit and a model rejection stay, since the reading measures neither.
                let now = (self.clock)();
                let mut lifted: Vec<CooldownScope> = self
                    .state
                    .cooldowns_of(&account_id, now)
                    .filter(|row| observed_at > row.set_at)
                    .filter(|row| match row.scope {
                        CooldownScope::FastCredits => true,
                        CooldownScope::Model(_) => row.reason == RATE_LIMIT_REASON,
                        CooldownScope::Account => {
                            row.reason == RATE_LIMIT_REASON && is_plan_window(&row.window)
                        }
                        CooldownScope::Fast => false,
                    })
                    .map(|row| row.scope.clone())
                    .collect();
                lifted.sort();
                for scope in lifted {
                    self.release_cooldown(account_id.clone(), scope, "recovered");
                }
            }
            Report::Overage {
                account_id,
                mut evidence,
                incarnation,
            } => {
                let snapshot = self.snapshot();
                if holder(&snapshot, &account_id, incarnation).is_none() {
                    return;
                }
                // A usage-based seat's overage family is its spend window, which `Report::SpendHead` carries. A leg
                // that read its snapshot before the class landed still reports it here, and it records nothing.
                if snapshot.usage_based(&account_id) {
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
                            source: current.source,
                            incarnation: current.incarnation,
                        })]);
                    }
                }
            }
            Report::SpendHead {
                account_id,
                utilization,
                reset,
                observed_at,
                incarnation,
            } => {
                if self.moved_past(&account_id, incarnation) {
                    return;
                }
                // A head on a usage-based seat is no overage episode, so the fold emits no event. The share persists as
                // the poll's does, and the reset stays in memory.
                let held = self.state.spend_head(&account_id);
                let moved = self.state.record_spend_head(
                    &account_id,
                    utilization,
                    reset,
                    observed_at,
                    incarnation,
                );
                // Once per instant: every head of the month repeats the reset, and only a new one is news.
                if let Some((head_reset, at)) = self
                    .state
                    .spend_head(&account_id)
                    .filter(|(head_reset, _)| held.is_none_or(|(before, _)| before != *head_reset))
                {
                    let estimate = spend_reset_estimate(at);
                    if head_reset != estimate {
                        tracing::info!(
                            account = %account_id,
                            head_reset,
                            estimate,
                            "a head named a spend reset apart from the estimate"
                        );
                    }
                }
                if !moved {
                    return;
                }
                if let Some(state) = self.state.credits_of(&account_id, incarnation) {
                    let row = RoutingState::credits_row(&account_id, state);
                    self.write(vec![StateWrite::UpsertCredits(row)]);
                }
            }
            Report::PinInvalidated {
                account_id,
                mut pin,
                mark,
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
                let key = self.canonical_pin(&mut pin);
                if let Some(row) = self.state.invalidate_pin(&pin, &account_id) {
                    let now = (self.clock)();
                    if let Some(event) = released_mark(&row, now) {
                        self.emit(event);
                    }
                    self.invalidated.insert(
                        key.clone(),
                        Invalidated {
                            account_id: account_id.clone(),
                            // The reason stays `mark` for a failover the relay sent unmarked too, which names no
                            // cause.
                            reason: "mark",
                            at: now,
                            row,
                            cause: mark.map(|(state, cause)| MoveCause::Mark {
                                state: state.as_str(),
                                cause: cause.as_str(),
                            }),
                        },
                    );
                    self.write(vec![StateWrite::DeletePin(key)]);
                }
            }
            Report::PinReleased {
                mut pin,
                incarnation,
                lifecycle_generation,
                credential_generation,
            } => {
                let key = self.canonical_pin(&mut pin);
                // The request that created the pin has ended, so a mark its landing set waits for nothing: the
                // backstop for a `Head` the closed mailbox refused.
                if self
                    .state
                    .pin(&key)
                    .is_some_and(|current| current.epoch == pin.epoch)
                {
                    self.state.clear_head_mark_of(&key);
                }
                // The epoch names the one pin the request created, and with it the account the generations are
                // checked against; a pin replaced since is another placement's to keep. A pin some selection
                // renewed after its placement is serving another turn of the session, so the error that ended
                // the first turn no longer speaks for it.
                if self.state.renewed_since_placement(&key) {
                    return;
                }
                let Some(current) = self
                    .state
                    .pin(&key)
                    .filter(|current| current.epoch == pin.epoch)
                    .cloned()
                else {
                    return;
                };
                if self.superseded(
                    &current.account_id,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                ) {
                    return;
                }
                // Nothing enters `invalidated`: the pin never served, so the session's next request is a fresh
                // placement rather than a move away from it.
                self.state.remove_pin(&key);
                let now = (self.clock)();
                let held = self.gapped(&key.key_id, &key.session_id, now);
                let deletes = self.first_seen_deletes(&key.key_id, &key.session_id, held);
                self.emit(RouterEvent::pin_expired(
                    &key,
                    &current.account_id,
                    vec!["client_error"],
                    now,
                ));
                if let Some(event) = released_mark(&current, now) {
                    self.emit(event);
                }
                self.write(
                    std::iter::once(StateWrite::EndPin(key))
                        .chain(deletes)
                        .collect(),
                );
            }
            Report::PinRenewed {
                mut pin,
                now,
                usage,
                keepalive,
                cache_ttl_ms,
                fast,
                classifier,
                turn,
            } => {
                let key = self.canonical_pin(&mut pin);
                if let Some(current) = self.state.pin(&key) {
                    let same_epoch = current.epoch == pin.epoch;
                    // A probe's renewal is the refresh of the cache it read, which is another cache once the session
                    // changed lanes while it was in flight. A client's renewal folds its own burn whatever lane
                    // the pin moved to since, so it is not held to the lane.
                    let left_lane =
                        keepalive && current.lane != self.state.canonical_lane(&pin.lane);
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
                    // A request of the standing row reached its stream's end: a mark is set at a placement and a
                    // placement takes a new epoch, so a renewal of the same epoch belongs to the marked row, and
                    // clearing early only shortens a hold. The backstop for a `Head` the closed mailbox refused.
                    if same_epoch {
                        self.state.clear_head_mark_of(&key);
                    }
                    let renewed = (same_epoch && !left_lane)
                        .then(|| self.state.pin_mut(&key))
                        .flatten();
                    if let Some(renewed) = renewed {
                        renewed.last_used_at = now;
                        renewed.cache_ttl_ms = cache_ttl_ms;
                        // A turn's stream end with its usage is the turn's end a classifier call that follows judges:
                        // `latest_turn_pin` reads it ahead of the pins' stamps. A probe (which replays a turn's class),
                        // an auxiliary call on the agent's turn pin (the subagent summary fork among them), a
                        // compaction and an answer without usage end no turn.
                        let turn_ended = !keepalive && turn && usage.is_some();
                        if keepalive {
                            // A probe's selection judged nothing, since it replays another request's facts; the
                            // renewal is the refresh it wrote. Its usage is the keepalive's cost, never the
                            // session's burn.
                            renewed.last_refresh_at = now;
                        } else {
                            // A probe replays another request's headers and body, so the row keeps its entrypoint
                            // and its effort as it keeps its lane.
                            renewed.entrypoint = pin.entrypoint.clone();
                            renewed.effort = pin.effort.clone().unwrap_or_default();
                            if let Some(usage) = usage {
                                // Every copy folds whatever the account's kind and nothing is priced here: the
                                // reader prices the copies its funding selects on the account the pin sits on when
                                // it reads, so leaving overage or moving between tiers needs no reset.
                                let window_ms =
                                    burn_window_ms(&self::current(&self.snapshot), &pin.key_id);
                                // The meter charges a request inside the peak band more, by its own regime's
                                // factor, so the banded copy weighs it by the band of the row's regime at the
                                // renewal, the row's entrypoint as the renewal leaves it. Dollars cost the same at
                                // every hour.
                                let band =
                                    self.config.band.at(FIVE_HOUR, entrypoint_of(renewed), now);
                                let at = renewed.burn_at;
                                let prompt = usage.prompt_tokens();
                                // Every copy decays from the same `burn_at`, so a copy this renewal adds nothing
                                // to folds at weight 0 and keeps step with the others.
                                let folded = |copy: PinBurn, weight: f64| {
                                    let fold = |rate: f64, tokens: u64| {
                                        burn_folded(
                                            rate,
                                            at,
                                            now,
                                            window_ms,
                                            tokens as f64 * weight,
                                        )
                                    };
                                    PinBurn {
                                        cache_read: fold(copy.cache_read, usage.cache_read),
                                        input: fold(copy.input, usage.input),
                                        output: fold(copy.output, usage.output),
                                        write_1h: fold(copy.write_1h, usage.write_1h),
                                        write_5m: fold(copy.write_5m, usage.write_5m),
                                    }
                                };
                                // A fast answer is usage credits: its tokens land in the row's fast copy, priced
                                // later through the fast table (`BurnRates::fast`). The client's classifier call on
                                // its judged turn's account takes no share of a plan window, so its tokens land in
                                // the unmetered copy alone, priced in dollars and never under the window share. A
                                // standard answer folds into the plain and banded copies, the banded one weighed by
                                // the band, and so does a classifier call the router answered off, which the relay
                                // reports unmarked: its body then stands as the pin's last prompt, which the pin's
                                // next move prices as its rewrite. Every copy an answer does not land in only decays.
                                let (plain, fast_weight, unmetered_weight) =
                                    match (classifier, fast) {
                                        (true, _) => (0.0, 0.0, 1.0),
                                        (false, true) => (0.0, 1.0, 0.0),
                                        (false, false) => (1.0, 0.0, 0.0),
                                    };
                                renewed.burn = folded(renewed.burn, plain);
                                renewed.burn_banded = folded(renewed.burn_banded, plain * band);
                                renewed.burn_fast = folded(renewed.burn_fast, fast_weight);
                                renewed.burn_unmetered =
                                    folded(renewed.burn_unmetered, unmetered_weight);
                                renewed.burn_at = Some(now);
                                renewed.last_input_tokens = prompt;
                                renewed.last_classifier = classifier;
                            }
                        }
                        let row = renewed.clone();
                        self.state.mark_renewed(&key);
                        if turn_ended {
                            self.state
                                .note_turn_end(&key, &row.account_id, row.incarnation);
                        }
                        self.write(vec![StateWrite::UpsertPin(row)]);
                    }
                }
            }
            Report::Sample {
                account_id,
                mut sample,
                incarnation,
                source_request_id,
            } => {
                let snapshot = self.snapshot();
                if holder(&snapshot, &account_id, incarnation).is_none() {
                    return;
                }
                // Neither the poll nor a head knows the account's plan, and the pair is the one the account holds as
                // the reading lands: a later profile read moves the readings after it and leaves this week's alone.
                let (plan, tier) = (snapshot.plan(&account_id), snapshot.tier(&account_id));
                for reading in sample.windows.values_mut() {
                    reading.plan = plan.map(String::from);
                    reading.tier = tier.map(String::from);
                }
                // The edges compare each plan window's stored reading with the one that replaces it, so the stored
                // ones are read before the fold.
                let stored: HashMap<String, WindowReading> = sample
                    .windows
                    .keys()
                    .filter(|window| is_plan_window(window))
                    .filter_map(|window| {
                        let reading = self.state.sample(&account_id)?.reading(window)?;
                        Some((window.clone(), reading))
                    })
                    .collect();
                let now = (self.clock)();
                // Read before the fold as the stored readings are, since a wall the prediction announced clears from
                // the figure the prediction read over them.
                let predicted: HashMap<String, f64> = stored
                    .keys()
                    .filter_map(|window| {
                        let reading = self.state.predicted(&account_id, window, now)?;
                        Some((window.clone(), reading.utilization))
                    })
                    .collect();
                let sample = self.held_at_the_wall(&account_id, sample, &stored, now);
                // A cooled account sends no requests, so a window the upstream reset ahead of the deadline it
                // announced reaches the router here and nowhere else. Only a sample newer than the stored one
                // answers for the account now: a head and a poll can cross, and the older one's reading is one
                // the window has already moved past.
                let Recorded { moved, dropped } =
                    self.state.record_sample(&account_id, sample.clone(), now);
                let mut writes = Vec::new();
                if !moved.is_empty() {
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
                            stored.get(window).cloned(),
                            predicted.get(window).copied(),
                            reading,
                            source_request_id.as_deref(),
                            now,
                        );
                    }
                    let rows = self.state.window_rows(&account_id, &windows, incarnation);
                    writes.extend(rows.into_iter().map(StateWrite::UpsertWindow));
                }
                // A settling poll can move nothing, as one that drains after a newer head does, so the drop and the
                // set are written whatever moved.
                for window in dropped {
                    tracing::info!(
                        account = %account_id,
                        window,
                        "a clean poll dropped the window, so its reading and charges leave the account"
                    );
                    writes.push(StateWrite::DeleteWindow {
                        account_id: account_id.clone(),
                        window,
                        incarnation,
                    });
                }
                // The set carries the instant of the poll that settled it, so a poll older than the set writes none.
                let settled = sample.poll
                    && sample
                        .windows
                        .keys()
                        .any(|window| window == FIVE_HOUR || window == SEVEN_DAY)
                    && self
                        .state
                        .sample(&account_id)
                        .and_then(|stored| stored.polled_at)
                        == Some(sample.observed_at);
                if settled {
                    writes.extend(
                        self.state
                            .window_set_row(&account_id, incarnation)
                            .map(StateWrite::UpsertWindowSet),
                    );
                }
                self.write(writes);
            }
            Report::Ended {
                request_id,
                attempt,
                at,
                cause,
            } => {
                // The request ended, so no head of it is still to come.
                self.state.clear_head_mark(&request_id, None);
                // A request that holds no booking (a count, a probe, a fast request, a renewal, one the purge gave up
                // on, one from before a restart) ends with nothing to release and nothing to say.
                if let Some(reservation) = self.state.release_reservation(&request_id) {
                    self.emit(RouterEvent::ReservationExpired {
                        account_id: reservation.account_id,
                        request_id,
                        attempt,
                        cause,
                        shares: reservation.rewrite,
                        age_ms: at - reservation.booked_at,
                        at,
                    });
                }
            }
            // A leg's report clears the mark its own landing set and never one a later landing of the same request
            // set, since the full mailbox's fallback can deliver an earlier leg's report after the failover's landing.
            Report::Head {
                request_id,
                attempt,
                ..
            } => {
                self.state.clear_head_mark(&request_id, Some(attempt));
            }
            Report::Usage {
                account_id,
                at,
                canonical_model,
                incarnation,
                usage,
                fast,
                classifier,
                request_id,
                attempt: _,
                entrypoint,
            } => {
                // The usage replaces the booking, on every path below: a request that holds none (a renewal, a
                // request from before a restart, one whose booking the purge expired) still charges its account.
                self.state.release_reservation(&request_id);
                let snapshot = self.snapshot();
                let Some(account) = holder(&snapshot, &account_id, incarnation) else {
                    return;
                };
                // A fast completion draws usage credits, and the client's classifier call on its judged turn's
                // account takes no share of a plan window, so neither charges one.
                if fast || classifier {
                    return;
                }
                // An api_key account bills its tokens and a usage-based seat its spend window, so neither has a plan
                // window to charge, whatever tier the seat's profile names, and an account whose tier the profile has
                // not named yet or whose plan the cost model does not price has no factor to price through.
                if account.kind != phoebis_store::AccountKind::Oauth
                    || snapshot.usage_based(&account_id)
                {
                    return;
                }
                let Some(tier) = snapshot.priced_tier(&account_id) else {
                    return;
                };
                // The report's own entrypoint, never a booking's: an ordinary renewal holds none.
                let price = |window: &str| {
                    self.config.share.price(
                        window,
                        entrypoint.as_deref(),
                        tier,
                        &canonical_model,
                        usage,
                        at,
                    )
                };
                // A model week the table does not measure is priced as the 7-day share once, until the cost table
                // measures it. The share pricer says which.
                if let Some(own) =
                    model_week(&canonical_model).filter(|own| !self.config.share.measured(own))
                {
                    tracing::debug!(window = own, "a model window is priced unmeasured");
                }
                // A 5-hour window the account's set lacks takes no charge, so `predicted_spent` announces no wall
                // on it.
                let shares: Vec<(&str, f64)> = plan_windows(&canonical_model)
                    .filter(|window| {
                        *window != FIVE_HOUR || self.state.has_window(&account_id, FIVE_HOUR)
                    })
                    .filter_map(|window| price(window).map(|share| (window, share)))
                    .collect();
                let before: Vec<(&str, f64)> = shares
                    .iter()
                    .filter_map(|(window, _)| {
                        let reading = self.state.predicted(&account_id, window, at)?;
                        Some((*window, reading.utilization))
                    })
                    .collect();
                self.state.charge(&account_id, at, &shares);
                for (window, before) in before {
                    self.predicted_spent(&account_id, window, before, &request_id, at);
                }
            }
            Report::AccountState {
                account_id,
                state,
                incarnation,
                lifecycle_generation,
                credential_generation,
                cause,
                detail,
                dispatched_ordinal,
            } => {
                if state != AccountState::FundingRequired
                    && dispatched_ordinal.is_some_and(|ordinal| {
                        self.state.funding_marked_since(&account_id, ordinal)
                    })
                {
                    tracing::debug!(
                        account = %account_id,
                        "a funding retry's success went out under an older mark than the newest and clears nothing"
                    );
                    return;
                }
                // The store keeps no instant for a mark, so the retry is stamped here. A verdict about a holder the
                // id no longer has moves no retry, as its write moves no state.
                let mut marked_ordinal = None;
                if !self.superseded(
                    &account_id,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                ) {
                    if state == AccountState::FundingRequired {
                        let interval = i64::try_from(self.config.funding_retry.as_millis())
                            .unwrap_or(i64::MAX);
                        marked_ordinal = Some(self.state.funding_marked(
                            &account_id,
                            incarnation,
                            (self.clock)(),
                            interval,
                        ));
                    } else {
                        self.state.funding_cleared(&account_id);
                    }
                }
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
                    marked_ordinal,
                }]);
            }
        }
    }

    fn resolve(&mut self, spelling: &str, dated_id: &str, at: i64) {
        let (rekey, merged_away) = self.state.rekey(spelling, dated_id, at);
        for row in &merged_away {
            if let Some(event) = released_mark(row, at) {
                self.emit(event);
            }
        }
        // A late invalidation from a request admitted under the retiring spelling must still land beside the
        // moved pin rather than starting a fresh migration window under a key nothing selects on any more.
        let keys: Vec<PinKey> = self
            .invalidated
            .keys()
            .filter(|key| key.model == spelling)
            .cloned()
            .collect();
        for key in keys {
            if let Some(mut entry) = self.invalidated.remove(&key) {
                entry.row.model = dated_id.to_string();
                self.invalidated.insert(
                    PinKey {
                        model: dated_id.to_string(),
                        ..key
                    },
                    entry,
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
        let snapshot = self.snapshot();
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
        let snapshot = self.snapshot();
        holder(&snapshot, account_id, incarnation).is_none()
    }

    /// A pin report is keyed on the model the state holds the pin under, whichever spelling the report named.
    fn canonical_pin(&self, pin: &mut PinRef) -> PinKey {
        pin.model = self.state.canonical(&pin.model).to_string();
        pin.key()
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
        self.load_waiters = Some(std::mem::take(&mut self.reload_waiters));
        let store = self.store.clone();
        let started_at = (self.clock)();
        // The blocking load runs under a task that awaits it, so a panic in the load still sends its `Swap` and
        // no reload waits on an answer that never comes.
        tokio::spawn(async move {
            let loaded = match tokio::task::spawn_blocking(move || {
                store.load_registry().map(|load| {
                    // Compiled here on the blocking thread, so no pattern compiles on the router task.
                    let guardrails = crate::CompiledGuardrails::build(&load);
                    (load, guardrails, store.digest_key())
                })
            })
            .await
            {
                Ok(Ok(loaded)) => Ok(loaded),
                Ok(Err(error)) => {
                    tracing::error!(
                        %error,
                        "the registry could not be reloaded, so the previous snapshot stays in force"
                    );
                    Err(error.to_string())
                }
                Err(error) => {
                    tracing::error!(%error, "the registry load panicked, so the previous snapshot stays in force");
                    Err(error.to_string())
                }
            };
            let _ = tx.send(Command::Swap { loaded, started_at }).await;
        });
    }

    /// The in-memory half of a reload: the load becomes the snapshot under the next revision, the streams of a
    /// departed holder end, and what the registry no longer holds is purged.
    fn swap(
        &mut self,
        load: RegistryLoad,
        guardrails: crate::CompiledGuardrails,
        digest_key: [u8; 32],
        loaded_at: i64,
    ) {
        let previous = self.snapshot();
        let next = Arc::new(
            RegistrySnapshot::from_load(load, digest_key, loaded_at, previous.revision + 1)
                .with_guardrails(guardrails),
        );
        // A pause is not a reason to cancel: `enabled` governs routing and probes only, and an admitted request
        // finishes on its account. A disconnect or a deletion takes the credential away, so its streams end,
        // and so do those of a holder deleted and created again under its id since the previous snapshot.
        // Samples and charges are keyed by id alone, so a moved holder's go here, before the purge.
        for account in previous.accounts() {
            let held = next.holder(&account.id, account.incarnation);
            let moved = held.is_none();
            let present = held.is_some_and(|a| {
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
        // The operator's Clear reaches the router as a store write and a reload, never as a report.
        self.state.reconcile_funding_retries(&next);
        *self.snapshot.write().expect("snapshot lock poisoned") = next;
        // Pins of a deleted key or pool, everything held against an account the registry no longer has, and
        // a cooldown past its deadline go now rather than at the next tick. An account that left its pool is
        // not one of them: `select` step 1 decides that per request, since a non-portable request keeps such
        // a pin by design.
        self.purge();
    }

    fn token(&mut self, account_id: &str) -> &CancellationToken {
        self.cancel.entry(account_id.to_string()).or_default()
    }

    /// The three steps a release takes wherever it starts: drop the row, say what released it, and delete it.
    /// The purge tick has its own, because it clears a batch the state already dropped.
    fn release_cooldown(
        &mut self,
        account_id: String,
        scope: CooldownScope,
        cause: &'static str,
    ) -> bool {
        let cleared = self.state.clear_cooldown(&account_id, &scope);
        if cleared {
            self.emit(RouterEvent::CooldownCleared {
                account_id: account_id.clone(),
                scope: scope.to_string(),
                cause,
                at: (self.clock)(),
            });
            self.write(vec![StateWrite::DeleteCooldown(account_id, scope)]);
        }
        cleared
    }

    /// Whether a pin of the session sits in a failover gap at `now`. Reads the window itself, since the purge's
    /// expiry asks before the retain drops the lapsed entries.
    fn gapped(&self, key_id: &str, session_id: &str, now: i64) -> bool {
        self.invalidated.iter().any(|(key, entry)| {
            key.key_id == key_id
                && key.session_id == session_id
                && now - entry.at <= MIGRATION_WINDOW_MS
        })
    }

    /// One delete per first-seen record the session's end dropped from memory, `held` as `end_first_seen` reads it.
    fn first_seen_deletes(
        &mut self,
        key_id: &str,
        session_id: &str,
        held: bool,
    ) -> Vec<StateWrite> {
        self.state
            .end_first_seen(key_id, session_id, held)
            .into_iter()
            .map(|agent_id| StateWrite::DeleteFirstSeen {
                key_id: key_id.to_string(),
                session_id: session_id.to_string(),
                agent_id,
            })
            .collect()
    }

    fn purge(&mut self) {
        let snapshot = self.snapshot();
        let now = (self.clock)();
        // A permit or a ticket whose guard's drop never reached the table: a panic, or a stream past the relay's
        // idle bound with no bound of its own.
        self.state.in_flight().expire(now);
        // A booking whose request's end never reached the task, under the same bound.
        for (request_id, reservation) in self.state.expire_reservations(now) {
            self.emit(RouterEvent::ReservationExpired {
                account_id: reservation.account_id,
                request_id,
                attempt: reservation.attempt,
                cause: "timeout",
                shares: reservation.rewrite,
                age_ms: now - reservation.booked_at,
                at: now,
            });
        }
        let purged = self.state.purge(&snapshot, now);
        // After the takes, so a session whose last pin the purge took ends with it.
        let mut deletes = Vec::new();
        for (row, _) in &purged.pins {
            let held = self.gapped(&row.key_id, &row.session_id, now);
            deletes.extend(self.first_seen_deletes(&row.key_id, &row.session_id, held));
        }
        self.slow_down_edges(now);
        tracing::debug!(
            affinity = purged.affinity.len(),
            cooldowns = purged.cooldowns.len(),
            pins = purged.pins.len(),
            "purged expired and orphaned routing state"
        );
        let mut lapsed = Vec::new();
        self.invalidated.retain(|key, entry| {
            let within = now - entry.at <= MIGRATION_WINDOW_MS;
            if !within {
                lapsed.push(key.clone());
            }
            within
        });
        // A failover whose landing never came ends its session as an expiry does, unless another of its pins still
        // sits in a gap. The invalidation already took the pin's row, so only the records leave.
        for key in &lapsed {
            let held = self.gapped(&key.key_id, &key.session_id, now);
            deletes.extend(self.first_seen_deletes(&key.key_id, &key.session_id, held));
        }
        for (account_id, scope) in &purged.cooldowns {
            self.emit(RouterEvent::CooldownCleared {
                account_id: account_id.clone(),
                scope: scope.to_string(),
                cause: "expired",
                at: now,
            });
        }
        let mut pin_ends = Vec::with_capacity(purged.pins.len());
        for (row, reason) in &purged.pins {
            let key = row.key();
            self.emit(RouterEvent::pin_expired(
                &key,
                &row.account_id,
                vec![*reason],
                now,
            ));
            if let Some(event) = released_mark(row, now) {
                self.emit(event);
            }
            pin_ends.push(StateWrite::EndPin(key));
        }
        let restored = !std::mem::replace(&mut self.purged, true);
        self.clock_resets(now, restored);
        self.wind_downs(&snapshot, now, restored);
        self.expiries(&purged.affinity_entries, now);
        let mut writes: Vec<StateWrite> = purged
            .cooldowns
            .into_iter()
            .map(|(a, s)| StateWrite::DeleteCooldown(a, s))
            .collect();
        writes.extend(pin_ends);
        writes.extend(deletes);
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
        // One cut over every pool, the longest dwell's: a faster pool's round stays on disk while a slower pool that
        // holds the account can still read the account's stamp from it after a restart. With no pool, nothing reads
        // a round.
        let longest_dwell_ms = snapshot.pools().map(shed_dwell_ms).max().unwrap_or(0);
        writes.push(StateWrite::PurgeShedRounds {
            before: now - longest_dwell_ms,
        });
        // The fenced upsert keeps a previous holder's window rows off disk; this sweep is the backstop, so none that
        // got there anyway outlives a tick.
        writes.push(StateWrite::PurgeStaleWindows);
        self.write(writes);
    }

    /// The edges a plan window's new reading crosses against the stored one: its reset passing, which a later
    /// reset names, or no reset on a reading taken past it, then the window cleared, which a reading under 1.0 names
    /// over one at 1.0 or above with no reset passed, or over a wall the prediction announced in the reading's cycle,
    /// from `predicted`, the stored reading's prediction before this one replaced it, or the window spent, which a
    /// reading at 1.0 or above names over one under it, or over any one where it names the next reset after a passed
    /// one.
    #[allow(clippy::too_many_arguments)]
    fn window_edges(
        &mut self,
        account_id: &str,
        window: &str,
        stored: Option<WindowReading>,
        predicted: Option<f64>,
        reading: WindowReading,
        source_request_id: Option<&str>,
        now: i64,
    ) {
        let passed = stored.as_ref().and_then(|stored| {
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
                    restored: false,
                    at: now,
                    plan: stored.as_ref().and_then(|stored| stored.plan.clone()),
                    tier: stored.as_ref().and_then(|stored| stored.tier.clone()),
                });
            }
            // A wall announced under the passed reset that goes on into the window open past it is the same wall: its
            // announcement moves to the reset's passing, so its clamp runs from here and it is announced no second
            // time. A reading below the wall leaves the old cycle's announcement standing, and a reading at the wall
            // that names the next reset is that cycle's own wall, announced below.
            let key = (account_id.to_string(), window.to_string());
            let goes_on =
                utilization_before >= 1.0 && reading.utilization >= 1.0 && next_reset.is_none();
            if goes_on
                && self
                    .spent_announced
                    .get(&key)
                    .is_some_and(|(announced, _)| {
                        announced.is_some_and(|announced| same_reset(announced, reset_at))
                    })
            {
                self.spent_announced.insert(key, (reading.reset, now));
            }
        }
        // The clear leaves the reset to the next sample or the clock walk, and the wall's announcement, the
        // prediction's or a sample's, leaves with it, so a reading back at the limit under the same reset is spent
        // again.
        if let Some(stored) = stored.as_ref().filter(|stored| stored.utilization >= 1.0) {
            if reading.utilization < 1.0 && passed.is_none() {
                self.spent_announced
                    .remove(&(account_id.to_string(), window.to_string()));
                self.emit(RouterEvent::WindowCleared {
                    account_id: account_id.to_string(),
                    window: window.to_string(),
                    utilization_before: stored.utilization,
                    utilization: reading.utilization,
                    // A reading that names no reset before the stored one passed stands under the stored reset.
                    reset: reading.reset.or(stored.reset),
                    source: reading.source,
                    source_request_id: source_request_id.map(String::from),
                    at: now,
                });
            }
        } else if reading.utilization < 1.0 {
            // A wall the prediction announced over a stored reading under it clears as a stored reading's fall does,
            // from the prediction's figure, once a reading of the announced cycle reads the window under its limit. The
            // announcement a reading below the wall left standing past its reset is another cycle's and clears nothing.
            let key = (account_id.to_string(), window.to_string());
            let reset = reset_under(stored.as_ref(), &reading);
            let announced = self
                .spent_announced
                .get(&key)
                .is_some_and(|announced| same_cycle(*announced, reset, now, window));
            if let Some(before) = predicted.filter(|_| announced) {
                self.spent_announced.remove(&key);
                self.emit(RouterEvent::WindowCleared {
                    account_id: account_id.to_string(),
                    window: window.to_string(),
                    utilization_before: before,
                    utilization: reading.utilization,
                    reset,
                    source: reading.source,
                    source_request_id: source_request_id.map(String::from),
                    at: now,
                });
            }
        }
        // A wall the prediction or an earlier sample announced is not announced again by a reading of its cycle, and
        // a reading that names no reset before the stored reset passed stands under that one.
        let reset = reset_under(stored.as_ref(), &reading);
        let announced = self
            .spent_announced
            .get(&(account_id.to_string(), window.to_string()))
            .is_some_and(|announced| same_cycle(*announced, reset, now, window));
        // A reading at the wall that names the reset after a passed one is a new wall, over a stored reading at the
        // wall too.
        let next_cycle = matches!(passed, Some((_, _, Some(_))));
        if (next_cycle
            || stored
                .as_ref()
                .is_none_or(|stored| stored.utilization < 1.0))
            && reading.utilization >= 1.0
            && !announced
        {
            // Recorded as the prediction records its own, so a repeat of this cycle announces nothing whichever
            // source announced first.
            self.spent_announced.insert(
                (account_id.to_string(), window.to_string()),
                (reading.reset, now),
            );
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

    /// The sample as the routing reads it. A plan window's reading that names no reset, taken before the stored
    /// reading's reset passed, is of that reset's cycle and takes that reset, raised or not, so the reading the fold
    /// stores keeps it and the next edge reads it passed. A reading within `SPENT_MARGIN` of 1.0 over a stored reading
    /// at the wall under the same reset cycle is then raised to the stored utilization with its own instant and source,
    /// so a head after a poll at 1.0 neither clears the window nor lets the next poll announce the wall again. The cycle
    /// is the announcement's for the account and window where one stands and the stored reading's reset and instant
    /// where none does, and since no raised reading moves the announcement's instant, readings with no reset on either
    /// side are held for the window's length from it and no longer. A reading under the margin is a real clear and
    /// lands at its own utilization, so `window_edges` reads the window cleared and forgets the announcement, and the
    /// next crossing announces anew. A reading of another cycle lands as read. Only a reading newer than the stored one
    /// counts, since the fold drops any other: an older reading, or one of the stored instant, goes to the fold as read.
    fn held_at_the_wall(
        &self,
        account_id: &str,
        mut sample: UtilizationSample,
        stored: &HashMap<String, WindowReading>,
        now: i64,
    ) -> UtilizationSample {
        for (window, reading) in sample.windows.iter_mut() {
            let Some(stored) = stored.get(window) else {
                continue;
            };
            if reading.observed_at <= stored.observed_at {
                continue;
            }
            let reset = reset_under(Some(stored), reading);
            reading.reset = reset;
            if stored.utilization < 1.0 || reading.utilization >= 1.0 {
                continue;
            }
            let anchor = self
                .spent_announced
                .get(&(account_id.to_string(), window.to_string()))
                .copied()
                .unwrap_or((stored.reset, stored.observed_at));
            if reading.utilization >= 1.0 - SPENT_MARGIN && same_cycle(anchor, reset, now, window) {
                reading.utilization = stored.utilization;
            }
        }
        sample
    }

    /// Announces the window spent when the router's own figure took its prediction from `before`, under 1.0, to 1.0 or
    /// above: a charge, or a booking the usage has not replaced yet. Once per account, window and reset cycle (a wall
    /// with no reset counts through its window's length), so a later charge of the same cycle announces nothing.
    fn predicted_spent(
        &mut self,
        account_id: &str,
        window: &str,
        before: f64,
        request_id: &str,
        at: i64,
    ) {
        let Some(reading) = self.state.predicted(account_id, window, at) else {
            return;
        };
        if before >= 1.0 || reading.utilization < 1.0 {
            return;
        }
        let key = (account_id.to_string(), window.to_string());
        if self
            .spent_announced
            .get(&key)
            .is_some_and(|announced| same_cycle(*announced, reading.reset, at, window))
        {
            return;
        }
        self.spent_announced.insert(key, (reading.reset, at));
        self.emit(RouterEvent::WindowSpent {
            account_id: account_id.to_string(),
            window: window.to_string(),
            utilization: reading.utilization,
            reset: reading.reset,
            source: WindowSource::Predicted,
            source_request_id: Some(request_id.to_string()),
            at,
        });
    }

    /// Whether `reset_at` is a reset not yet announced for the account and window, recording it when it is: the
    /// sample edge and the clock walk both ask, so one passing reset is announced once whichever sees it first.
    fn announce_reset(&mut self, account_id: &str, window: &str, reset_at: i64) -> bool {
        let key = (account_id.to_string(), window.to_string());
        if self
            .reset_announced
            .get(&key)
            .is_some_and(|&announced| same_reset(announced, reset_at))
        {
            return false;
        }
        self.reset_announced.insert(key, reset_at);
        true
    }

    /// Every stored plan reading whose reset the clock has passed with no newer sample, announced once.
    fn clock_resets(&mut self, now: i64, restored: bool) {
        let passed: Vec<(String, String, WindowReading, i64)> = self
            .state
            .samples()
            .into_iter()
            .flat_map(|(account_id, sample)| {
                sample
                    .windows
                    .into_iter()
                    .filter_map(move |(window, reading)| {
                        let reset_at = reading.reset.filter(|reset| *reset <= now)?;
                        is_plan_window(&window)
                            .then(|| (account_id.clone(), window, reading, reset_at))
                    })
            })
            .collect();
        for (account_id, window, reading, reset_at) in passed {
            if self.announce_reset(&account_id, &window, reset_at) {
                self.emit(RouterEvent::WindowReset {
                    account_id,
                    window,
                    utilization_before: reading.utilization,
                    reset_at,
                    next_reset: None,
                    observed: "clock",
                    restored,
                    at: now,
                    plan: reading.plan,
                    tier: reading.tier,
                });
            }
        }
        // An account whose readings left takes what was announced for it along.
        self.reset_announced
            .retain(|(account_id, _), _| self.state.sample(account_id).is_some());
    }

    /// Every pool member inside its wind-down now that was not at the last tick, announced once while it stays.
    fn wind_downs(&mut self, snapshot: &RegistrySnapshot, now: i64, restored: bool) {
        let mut winding = HashSet::new();
        for pool in snapshot.pools() {
            let minutes = pool.failover.wind_down_minutes;
            for member in snapshot.members(&pool.id) {
                let Some(term) = snapshot.term(&member.account_id) else {
                    continue;
                };
                // A canceled term past its end still reads as winding down. Its account has ended, which is no start.
                let Some(ends_at) = term::end_of(term, now).filter(|&end| {
                    now >= term::wind_down_start(end, minutes) && !term::ended(term, now)
                }) else {
                    continue;
                };
                let key = (pool.id.clone(), member.account_id.clone());
                if !self.winding.contains(&key) {
                    self.emit(RouterEvent::WindDownStarted {
                        account_id: member.account_id.clone(),
                        pool_id: pool.id.clone(),
                        ends_at,
                        wind_down_minutes: minutes,
                        end_source: if term.canceled { "canceled" } else { "trial" },
                        restored,
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

    fn write(&mut self, writes: Vec<StateWrite>) {
        if writes.is_empty() {
            return;
        }
        // Nothing passes the backlog: a slot that frees between the backlog's offer and a newer write would let
        // the newer write land first, and a pin's upsert behind its own delete puts a released pin back.
        if self.deferred.is_empty() {
            match self.writer.try_reserve() {
                Ok(permit) => {
                    // Counted in before the send, so the writer never counts out a batch this has not counted in.
                    self.writer_status.depth.fetch_add(1, Ordering::Relaxed);
                    permit.send(writes);
                    return;
                }
                Err(mpsc::error::TrySendError::Full(())) => {
                    if !self.writer_status.degraded.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            depth = WRITER_DEPTH,
                            "the routing state writer is behind, so writes wait in memory, one per row"
                        );
                    }
                }
                // Only after `drain_writer`, which has landed everything acknowledged before it.
                Err(mpsc::error::TrySendError::Closed(())) => return,
            }
        }
        self.deferred.fold(writes);
        self.writer_status
            .deferred
            .store(self.deferred.len(), Ordering::Relaxed);
    }

    /// Hands the backlog to the channel as one batch once a slot is free.
    fn offer_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        match self.writer.try_reserve() {
            Ok(permit) => {
                self.writer_status.depth.fetch_add(1, Ordering::Relaxed);
                permit.send(self.deferred.take());
                self.writer_status.deferred.store(0, Ordering::Relaxed);
                self.writer_status.degraded.store(false, Ordering::Relaxed);
                tracing::info!(
                    "the routing state writer caught up, so the writes held in memory are queued"
                );
            }
            Err(mpsc::error::TrySendError::Full(())) => {}
            // The writer task is gone, so nothing would ever take the backlog. It closes as the loop shuts down,
            // where the loss is expected, so this warns as `drain_writer` does.
            Err(mpsc::error::TrySendError::Closed(())) => {
                let lost = self.deferred.take().len();
                self.writer_status.deferred.store(0, Ordering::Relaxed);
                tracing::warn!(
                    lost,
                    "the routing state writer has stopped, so the writes held in memory are lost"
                );
            }
        }
    }

    /// Resolves once the writer's channel has a free slot, which the next turn's offer takes.
    async fn writer_room(&self) {
        let _ = self.writer.reserve().await;
    }
}

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
        scope: CooldownScope,
    },
    Credits(String),
    /// An agent's record by key id, session id and agent id.
    FirstSeen(String, String, String),
    Overage(String),
    Pin(PinKey),
    PurgeShedRounds,
    PurgeStaleWindows,
    /// A pool's round on an account: a later round replaces the held one, as the store's write does.
    ShedRound {
        pool_id: String,
        account_id: String,
    },
    /// A window's row: a held drop and a later held restore of the window keep the last write.
    Window {
        account_id: String,
        window: String,
    },
    WindowSet {
        account_id: String,
    },
}

/// Writes the channel had no room for, the last write per row: a pin's last upsert, delete or end, an agent's
/// first-seen record's last insert or delete, a window's last upsert or delete, a window set's last upsert per
/// account, a cooldown's last upsert or delete per scope, an overage, credits or account state per account, an
/// affinity row's last upsert or delete per key, a pool's last round per account, one round purge, one stale-window
/// sweep, and every rekey in order. A rekey closes the rows before it into a segment of their own, so a pin's write
/// dated after a resolution never lands ahead of the rekey that moved its rows. A round may land on either side of the
/// pin writes held beside it: a mark is read back only for the pin and the epoch its row names. A segment lands in map
/// order, so a record's delete and its insert share the record's key: a session that ends and returns writes the
/// return's record over the end's delete, and the end never lands behind the return to take its row.
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
                Ok(rows) => {
                    for (key, mut write) in rows {
                        // An account-state write that carries no ordinal, a superseded verdict's, keeps the ordinal of
                        // the write it replaces, so the writer's acknowledgment still names the entry that write's
                        // mark stamped and a load that reads the row past it drops that entry.
                        if let StateWrite::AccountState { marked_ordinal, .. } = &mut write {
                            if marked_ordinal.is_none() {
                                if let Some(StateWrite::AccountState {
                                    marked_ordinal: held,
                                    ..
                                }) = self.open.get(&key)
                                {
                                    *marked_ordinal = *held;
                                }
                            }
                        }
                        self.open.insert(key, write);
                    }
                }
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
            ref account_id,
            incarnation,
            lifecycle_generation,
            credential_generation,
            ..
        } => one(
            DeferredKey::AccountState {
                account_id: account_id.clone(),
                incarnation,
                lifecycle_generation,
                credential_generation,
            },
            write,
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
        StateWrite::DeleteCooldown(ref account_id, ref scope) => one(
            DeferredKey::Cooldown {
                account_id: account_id.clone(),
                scope: scope.clone(),
            },
            write,
        ),
        StateWrite::DeleteFirstSeen {
            ref key_id,
            ref session_id,
            ref agent_id,
        } => one(
            DeferredKey::FirstSeen(key_id.clone(), session_id.clone(), agent_id.clone()),
            write,
        ),
        StateWrite::DeletePin(ref key) => one(DeferredKey::Pin(key.clone()), write),
        StateWrite::DeleteWindow {
            ref account_id,
            ref window,
            ..
        } => one(
            DeferredKey::Window {
                account_id: account_id.clone(),
                window: window.clone(),
            },
            write,
        ),
        StateWrite::EndPin(ref key) => one(DeferredKey::Pin(key.clone()), write),
        StateWrite::InsertFirstSeen(ref row) => one(
            DeferredKey::FirstSeen(
                row.key_id.clone(),
                row.session_id.clone(),
                row.agent_id.clone(),
            ),
            write,
        ),
        StateWrite::PurgeShedRounds { .. } => one(DeferredKey::PurgeShedRounds, write),
        StateWrite::PurgeStaleWindows => one(DeferredKey::PurgeStaleWindows, write),
        StateWrite::Rekey(rekey) => Err(Box::new(rekey)),
        StateWrite::ShedRound {
            ref pool_id,
            ref account_id,
            ..
        } => one(
            DeferredKey::ShedRound {
                pool_id: pool_id.clone(),
                account_id: account_id.clone(),
            },
            write,
        ),
        StateWrite::UpsertAffinity(rows) => Ok(rows
            .into_iter()
            .map(|row| {
                (
                    affinity(&row.model, &row.hash),
                    StateWrite::UpsertAffinity(vec![row]),
                )
            })
            .collect()),
        StateWrite::UpsertCooldown(ref row) => one(
            DeferredKey::Cooldown {
                account_id: row.account_id.clone(),
                scope: row.scope.clone(),
            },
            write,
        ),
        StateWrite::UpsertCredits(ref row) => {
            one(DeferredKey::Credits(row.account_id.clone()), write)
        }
        StateWrite::UpsertOverage(ref row) => {
            one(DeferredKey::Overage(row.account_id.clone()), write)
        }
        StateWrite::UpsertPin(ref row) => one(DeferredKey::Pin(row.key()), write),
        StateWrite::UpsertWindow(ref row) => one(
            DeferredKey::Window {
                account_id: row.account_id.clone(),
                window: row.window.clone(),
            },
            write,
        ),
        StateWrite::UpsertWindowSet(ref row) => one(
            DeferredKey::WindowSet {
                account_id: row.account_id.clone(),
            },
            write,
        ),
    }
}

/// The candidates the current registry gives a request admitted under an earlier one, or the refusal it answers.
fn rederived(
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    now: i64,
) -> Result<Vec<Candidate>, Refusal> {
    let key = snapshot
        .authenticate_id(&request.key_id, request.key_generation, now)
        .map_err(Refusal::Key)?;
    resolve(
        snapshot,
        key,
        &request.canonical_model,
        &request.folded_model,
    )
    .map_err(Refusal::Resolve)
}

/// The snapshot's account under the id while it is still the holder a report was formed about, `None` once the
/// account is gone or was deleted and created again, which resets both generations to the values the old holder
/// started at.
fn holder<'a>(
    snapshot: &'a RegistrySnapshot,
    account_id: &str,
    incarnation: u64,
) -> Option<&'a phoebis_store::AccountRow> {
    let account = snapshot.holder(account_id, incarnation);
    if account.is_none() {
        tracing::debug!(account = %account_id, "a report named a holder the id has moved past");
    }
    account
}

fn current(shared: &RwLock<Arc<RegistrySnapshot>>) -> Arc<RegistrySnapshot> {
    shared.read().expect("snapshot lock poisoned").clone()
}

/// The burn window of the pool the key routes through, or the default preset's for a key the snapshot no longer
/// holds, whose pin the next purge drops anyway.
fn burn_window_ms(snapshot: &RegistrySnapshot, key_id: &str) -> i64 {
    snapshot
        .key_pool(key_id)
        .map_or_else(default_burn_average_ms, burn_average_ms)
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
        poll: true,
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
                    plan: None,
                    tier: None,
                },
            );
        }
    }
    let read = [FIVE_HOUR, SEVEN_DAY]
        .map(|name| sample.windows.get(name).map(|r| (r.utilization, r.reset)));
    let overage = read.iter().any(Option::is_some).then(|| OverageEvidence {
        in_use: snapshot.extra_usage.as_ref().is_some_and(|e| e.is_enabled)
            && read
                .iter()
                .flatten()
                .any(|&(utilization, _)| utilization >= 1.0),
        since: None,
        reset: latest_full_reset(
            read.iter()
                .flatten()
                .map(|&(utilization, reset)| (Some(utilization), reset)),
        ),
        observed_at: snapshot.fetched_at,
        source: EvidenceSource::Sample,
        incarnation,
    });
    (sample, overage)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use phoebis_store::{AccountKind, AccountRow, ResolutionRow, StorageKey, WindowRow};

    use super::*;
    use crate::select::FIVE_HOUR_MS;

    fn cooldown(account_id: &str, scope: &str) -> CooldownRow {
        CooldownRow {
            account_id: account_id.into(),
            scope: CooldownScope::parse(scope).unwrap(),
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

    #[test]
    fn regime_rates_answer_the_regimes_table_for_a_listed_entrypoint_and_the_base_otherwise() {
        let desk = RateTable {
            fallback: crate::state::Rates::flat(0.5),
            ..RateTable::unit()
        };
        let rates = RegimeRates {
            base: RateTable::unit(),
            regimes: HashMap::from([("desk".to_string(), desk.clone())]),
            entrypoints: HashMap::from([
                ("desk".to_string(), "desk".to_string()),
                ("pane".to_string(), "desk".to_string()),
            ]),
        };
        assert_eq!(rates.for_entrypoint(Some("desk")), &desk);
        assert_eq!(
            rates.for_entrypoint(Some("pane")),
            &desk,
            "every entrypoint the regime lists reads its table"
        );
        assert_eq!(
            rates.for_entrypoint(None),
            &RateTable::unit(),
            "no entrypoint is the base regime"
        );
        assert_eq!(
            rates.for_entrypoint(Some("sdk-cli")),
            &RateTable::unit(),
            "an entrypoint no regime lists prices at the base"
        );
        assert_eq!(
            RegimeRates::unit().for_entrypoint(Some("desk")),
            &RateTable::unit(),
            "the unit tables list no regime"
        );
    }

    #[test]
    fn a_pins_price_adds_its_rows_fast_usage_under_usd_and_leaves_it_out_under_window_share() {
        let flat = |rate: f64| RateTable {
            fallback: crate::state::Rates::flat(rate),
            ..RateTable::unit()
        };
        let rates = BurnRates {
            dollars: flat(0.25),
            five_hour: RegimeRates {
                base: flat(0.125),
                ..RegimeRates::unit()
            },
            seven_day: RegimeRates::unit(),
            fast: flat(0.5),
        };
        let input = |input: f64| PinBurn {
            input,
            ..PinBurn::default()
        };
        let pin = PinRow {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
            account_id: "a".into(),
            epoch: 1,
            created_at: 0,
            last_used_at: 0,
            key_generation: 0,
            incarnation: 1,
            burn: input(4.0),
            burn_at: None,
            burn_banded: input(16.0),
            burn_fast: input(6.0),
            burn_unmetered: PinBurn::default(),
            last_input_tokens: 0,
            last_classifier: false,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed: None,
            static_hash: None,
            tail_hash: None,
            lane: crate::lane_key("m", "standard", "none", "-"),
            entrypoint: String::new(),
            mark: None,
            auxiliary_only: false,
            effort: String::new(),
        };
        assert_eq!(
            rates.price_pin(BurnUnit::Usd, &pin, FIVE_HOUR),
            Some(4.0 * 0.25 + 6.0 * 0.5),
            "the row's plain copy at the dollar rate plus its fast usage at the fast rate"
        );
        assert_eq!(
            rates.price_pin(BurnUnit::WindowShare, &pin, FIVE_HOUR),
            Some(16.0 * 0.125),
            "the banded copy alone, since fast usage draws usage credits and never the plan's window"
        );
    }

    /// Both arms of an open window's bound take the reset's slack: the request that opened the window can reach the
    /// upstream after `at`, so a second reading with no reset inside the slack past `open_until` is the same window,
    /// as a reading naming a reset inside it is.
    #[test]
    fn a_reading_with_no_reset_inside_the_slack_past_the_open_windows_length_is_the_same_cycle() {
        let at = 1_000;
        let open_until = at + FIVE_HOUR_MS;
        assert!(same_cycle((None, at), None, open_until, FIVE_HOUR));
        assert!(
            same_cycle((None, at), None, open_until + RESET_SLACK_MS / 2, FIVE_HOUR),
            "inside the slack, as the named-reset arm reads it"
        );
        assert!(
            same_cycle(
                (None, at),
                Some(open_until + RESET_SLACK_MS / 2),
                open_until,
                FIVE_HOUR
            ),
            "the named-reset arm's own slack"
        );
        assert!(
            !same_cycle((None, at), None, open_until + RESET_SLACK_MS + 1, FIVE_HOUR),
            "past the slack a later window"
        );
    }

    /// A reading that names no reset over a wall announced with one is of that wall's cycle until the announced reset
    /// passes, with the slack the named-reset arm takes, and of another cycle past it.
    #[test]
    fn a_reading_with_no_reset_is_of_the_announced_resets_cycle_up_to_the_reset_and_its_slack() {
        let at = 1_000;
        let reset = at + FIVE_HOUR_MS;
        assert!(
            !same_cycle(
                (Some(reset), at),
                None,
                reset + RESET_SLACK_MS + 1,
                FIVE_HOUR
            ),
            "past the slack another cycle"
        );
        assert!(
            same_cycle((Some(reset), at), None, reset - 1_000, FIVE_HOUR),
            "before the reset the announced cycle"
        );
        assert!(same_cycle((Some(reset), at), None, reset, FIVE_HOUR));
        assert!(
            same_cycle((Some(reset), at), None, reset + RESET_SLACK_MS, FIVE_HOUR),
            "inside the slack, as the named-reset arm reads it"
        );
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
            dropped(&handle),
            1,
            "after the final drain nothing takes the reading"
        );
        assert!(
            handle.dropped_reports().contains(&("accounting", 1)),
            "the drop counts under the reading's kind"
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

    /// A store at `scratch` holding account `a`, and a second store on the same file through a link, as a backup or
    /// an operator's `sqlite3` would hold it: the process lock is per directory, and SQLite resolves the link to the
    /// one file and its one WAL.
    fn linked_stores(scratch: &tempfile::TempDir) -> (Store, Store) {
        let key = StorageKey::generate();
        let path = scratch.path().join("store").join("router.sqlite");
        let store = Store::open(&path, &key).unwrap();
        store
            .insert_account(&AccountRow::for_tests("a", AccountKind::ApiKey))
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
        let scratch = tempfile::tempdir().unwrap();
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
            .map(|row| row.scope.to_string())
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
        let scratch = tempfile::tempdir().unwrap();
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
                .insert_account(&AccountRow::for_tests(id, AccountKind::Oauth))
                .unwrap();
        }
        // Two windows of `a` and none of `b`: one account with a reading.
        for window in [FIVE_HOUR, SEVEN_DAY] {
            assert!(store
                .upsert_window(&WindowRow {
                    account_id: "a".into(),
                    incarnation: 1,
                    observed_at: 500,
                    plan: None,
                    reset: Some(10_000),
                    tier: None,
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
                affinity_entries: 0,
                shed_marks: 0,
                shed_rounds_stale: 0,
            }
        );
    }

    #[test]
    fn the_projections_answer_each_windows_projection_and_demand_as_two_projection_of_reads_do() {
        use phoebis_store::{ApiKeyRow, PoolMember, SubscriptionTerm};

        use crate::select::{Banded, SlowDownRead};
        use crate::state::BandSegment;

        const HOUR: i64 = 3_600_000;
        const T: i64 = 10 * HOUR;
        let tier = "default_claude_max_20x";
        let mut pool = PoolRow::for_tests("p");
        pool.slow_down.slow_down = true;
        let snapshot = RegistrySnapshot::from_load(
            RegistryLoad {
                accounts: vec![AccountRow::for_tests("a", AccountKind::Oauth)],
                keys: vec![ApiKeyRow {
                    id: "k".into(),
                    label: "k".into(),
                    notes: None,
                    pool_id: "p".into(),
                    enabled: true,
                    revoked_at: None,
                    expires_at: None,
                    model_rules: Vec::new(),
                    model_rules_mode: Default::default(),
                    long_context_twins: false,
                    keepalive_opt_out: false,
                    body_capture_hours: None,
                    secret_digest: Vec::new(),
                    prefix: "k".into(),
                    incarnation: 1,
                    created_at: 0,
                    updated_at: 0,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                    auto_mode_server: None,
                }],
                members: vec![PoolMember {
                    pool_id: "p".into(),
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                }],
                pools: vec![pool],
                terms: vec![SubscriptionTerm {
                    account_id: "a".into(),
                    tier: Some(tier.into()),
                    plan: Some("max_20x".into()),
                    ..SubscriptionTerm::default()
                }],
                ..RegistryLoad::default()
            },
            [0; 32],
            0,
            1,
        );
        let pool = snapshot.pool("p").expect("the case's pool");
        let model = "claude-sonnet-5";
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![PinRow {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: model.into(),
                account_id: "a".into(),
                epoch: 1,
                created_at: 0,
                last_used_at: T,
                key_generation: 0,
                incarnation: 1,
                burn: PinBurn {
                    input: 1.0,
                    ..PinBurn::default()
                },
                burn_at: Some(T),
                burn_banded: PinBurn::default(),
                burn_fast: PinBurn::default(),
                burn_unmetered: PinBurn::default(),
                last_input_tokens: 0,
                last_classifier: false,
                last_refresh_at: T,
                cache_ttl_ms: 300_000,
                shed: None,
                static_hash: None,
                tail_hash: None,
                lane: crate::lane_key(model, "standard", "none", "-"),
                entrypoint: String::new(),
                mark: None,
                auxiliary_only: false,
                effort: String::new(),
            }],
            ..RoutingLoad::default()
        });
        // The case's own rates and band: the 5-hour window weighs twice inside the band, so its projection reads the
        // band's segments and not one flat rate.
        let (band_from, band_to) = (T + HOUR / 2, T + 3 * HOUR);
        state.set_share(
            SharePricer::default()
                .with_rate(|window, _, _, _, _, _| {
                    Some(if window == FIVE_HOUR { 1e-5 } else { 2e-6 })
                })
                .with_band_segments(move |window, _, from, to| {
                    if window != FIVE_HOUR {
                        return vec![BandSegment {
                            from,
                            to,
                            weight: 1.0,
                        }];
                    }
                    let mut edges = vec![from];
                    edges.extend(
                        [band_from, band_to]
                            .into_iter()
                            .filter(|edge| from < *edge && *edge < to),
                    );
                    edges.push(to);
                    edges
                        .windows(2)
                        .map(|pair| BandSegment {
                            from: pair[0],
                            to: pair[1],
                            weight: if band_from <= pair[0] && pair[0] < band_to {
                                2.0
                            } else {
                                1.0
                            },
                        })
                        .collect()
                }),
        );
        let reading = |utilization: f64, reset: i64| WindowReading {
            utilization,
            reset: Some(reset),
            observed_at: T,
            source: WindowSource::Poll,
            plan: None,
            tier: None,
        };
        state.record_sample(
            "a",
            UtilizationSample {
                observed_at: T,
                ..Default::default()
            }
            .with(FIVE_HOUR, reading(0.4, T + 2 * HOUR))
            .with(SEVEN_DAY, reading(0.3, T + 72 * HOUR)),
            T,
        );
        // An entry in force at a realized speed of a half, so the demand reads the pin's burn at twice the achieved.
        state.write_slow_down(
            "p",
            "a",
            &SlowDownRead {
                utilization: 0.4,
                horizon: T + 2 * HOUR,
                reset: Some(T + 2 * HOUR),
                demand: 0.0,
                banded: Banded::default(),
                stretch_ms: 0,
                threshold_ms: 0,
                factor: 0.8,
                ramp: 1.0,
                ramped: 0.8,
                speed: 1.0,
                plan: Vec::new(),
                realized: 0.5,
            },
            0.5,
            T,
            HOUR,
        );
        assert_eq!(state.slow_down_speed(pool, "a", T), 0.5);

        let answer = projections(&state, &snapshot, "p", T, 60_000);
        let [account] = &answer[..] else {
            panic!("one member answers: {answer:?}");
        };
        assert_eq!(
            account
                .windows
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [FIVE_HOUR, SEVEN_DAY],
            "the windows the account holds a reading of"
        );
        for (window, answered) in &account.windows {
            let read = |burn| {
                projection_of(&state, &snapshot, pool, "a", tier, window, T, burn)
                    .expect("a window with a reading projects")
            };
            let (projection, demand) = (read(BurnRead::Achieved), read(BurnRead::Demand));
            assert_eq!(
                (
                    answered.predicted,
                    answered.projection,
                    answered.horizon,
                    answered.burn,
                    answered.demand,
                ),
                (
                    projection.predicted,
                    projection.at_horizon,
                    projection.horizon,
                    projection.burn * 3_600.0,
                    demand.burn * 3_600.0,
                ),
                "{window}: one answer carries the projection and the demand to the bit"
            );
            assert!(
                answered.demand > answered.burn,
                "{window}: the demand divides the burn by the speed: {answered:?}"
            );
        }
    }
}
