//! What the router task owns between selections: pins, cooldowns, the newest overage evidence per account, the
//! newest utilization sample per account with the router's own charges since it, and the in-flight counts per key
//! and per account. Every mutation reports whether it changed anything so the task knows what to write through.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use phoebis_store::{
    AccountKind, AccountState, AffinityRow, BudgetPeriod, CooldownRow, CreditsRow, OverageRow,
    PinBurn, PinKey, PinRow, PoolMember, PoolRow, RekeyWrite, ResolutionRow, RoutingLoad,
    WindowRow,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::affinity::AffinityMap;
use crate::listing::{row_for, snapshot_date};
use crate::select::{affinity_ttl_ms, seconds_ms};
use crate::task::BurnRates;
use crate::windows::window_for;
use crate::RegistrySnapshot;

/// How long an api_key bucket read without its reset header stands: the buckets refill within a minute, so a
/// bucket older than that says nothing about the account now.
pub const BUCKET_LIFE_MS: i64 = 60_000;
/// A bucket refills within a minute; ten minutes keeps a live model's last reading through a quiet spell and lets
/// a model nobody asks for leave the map.
pub const BUCKET_TTL_MS: i64 = 600_000;
/// Bucket windows an account holds at most, the oldest reading evicted at insert.
pub const MAX_BUCKETS_PER_ACCOUNT: usize = 64;
pub const WEEK_MS: i64 = 7 * 24 * 3_600_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRef {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
    pub epoch: u64,
    /// The lane the pin held once the selection that handed this out placed or renewed it. A renewal keeps the
    /// epoch across a lane change, so a report whose effect is bound to a lane checks this as well.
    pub lane: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceSource {
    Head,
    Sample,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::Head => "head",
            EvidenceSource::Sample => "sample",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverageEvidence {
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
    pub source: EvidenceSource,
    /// The holder of the account id the reading is about.
    pub incarnation: u64,
}

impl OverageEvidence {
    /// The one overage predicate: the flag stands while the reset the evidence names is ahead of `now`, or,
    /// when it names none, for `fallback_ms` past the observation. Every reader, the keepalive's included,
    /// applies this so the probe gate and the selector never disagree about the same evidence.
    pub fn stands(&self, now: i64, fallback_ms: i64) -> bool {
        if !self.in_use {
            return false;
        }
        match self.reset {
            Some(reset) => reset > now,
            None => now - self.observed_at < fallback_ms,
        }
    }
}

/// The poll's newest extra-usage state of an account, for the fast eligibility; the registry keeps the newest one
/// per holder, so a restart judges on it until the next poll lands.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditsState {
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    /// A fraction of the monthly limit, as the plan windows read.
    pub utilization: Option<f64>,
    pub observed_at: i64,
    pub incarnation: u64,
}

impl CreditsState {
    /// The reason fast mode cannot draw credits here: the poll's own reason, `extra_usage_disabled` for a disabled
    /// state without one, `org_spend_cap_reached` at a utilization of one or more; `None` while credits have room.
    pub fn blocked(&self) -> Option<&str> {
        if let Some(reason) = self.disabled_reason.as_deref().filter(|r| !r.is_empty()) {
            return Some(reason);
        }
        if !self.enabled {
            return Some("extra_usage_disabled");
        }
        self.utilization
            .is_some_and(|u| u >= 1.0)
            .then_some("org_spend_cap_reached")
    }
}

pub const FIVE_HOUR: &str = "five_hour";
pub const SEVEN_DAY: &str = "seven_day";
pub const SEVEN_DAY_FABLE: &str = "seven_day_fable";

/// `five_hour`, `seven_day`, or a `seven_day_` claim: the plan windows the classifier names on a cooldown row.
pub fn is_plan_window(window: &str) -> bool {
    window == FIVE_HOUR || window == SEVEN_DAY || window.starts_with("seven_day_")
}

/// The one fast-eligibility rule over a live cooldown row, which the selector's walk (through `blocking_cooldown`)
/// and the keepalive's gate both call: whether the row keeps a request of this speed for `canonical_model` off its
/// account. An `account` row holds either speed unless the request is fast and the row's window is a plan window,
/// since a fast request draws usage credits and no plan window; the model's `model:` row holds either speed; the
/// fast lane's two rows, `fast` (its rate limit) and `fast_credits` (its usage-credits refusal), hold a fast request
/// alone. For the same reason no overage hold applies to a fast request, a rule both callers apply beside this one.
pub fn cooldown_holds(scope: &str, window: &str, canonical_model: &str, fast: bool) -> bool {
    match scope {
        "account" => !(fast && is_plan_window(window)),
        "fast" | "fast_credits" => fast,
        _ => scope.strip_prefix("model:") == Some(canonical_model),
    }
}

/// One window's reading: utilization as a fraction, the reset that refunds it when the source named one, and
/// when it was read; a reading is replaced only by a newer one for the same window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowReading {
    pub utilization: f64,
    pub reset: Option<i64>,
    pub observed_at: i64,
}

/// The fast lane's two buckets, per organization and shared by every fast-capable model, so the names carry no
/// model suffix and neither `worst_bucket`'s prefixes nor the plan-window persistence match them.
pub const FAST_INPUT: &str = "fast_input";
pub const FAST_OUTPUT: &str = "fast_output";

/// An api_key bucket, one per canonical model under each prefix.
fn is_model_bucket(window: &str) -> bool {
    window.starts_with("rpm:") || window.starts_with("itpm:") || window.starts_with("otpm:")
}

/// A window `BUCKET_TTL_MS` and `MAX_BUCKETS_PER_ACCOUNT` bound: an api_key bucket or one of the fast lane's two,
/// never a plan window.
fn is_bucket_window(window: &str) -> bool {
    is_model_bucket(window) || window == FAST_INPUT || window == FAST_OUTPUT
}

/// An account's latest readings by window name: `five_hour`, `seven_day`, `seven_day_fable` and any other
/// `seven_day_*` key the usage endpoint reports for a subscription account; `rpm:<model>`, `itpm:<model>`,
/// `otpm:<model>` per canonical model for an api_key account; and for either kind `FAST_INPUT` and `FAST_OUTPUT`,
/// the fast lane's two buckets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UtilizationSample {
    pub windows: BTreeMap<String, WindowReading>,
    pub observed_at: i64,
}

impl UtilizationSample {
    /// The stored reading as it arrived.
    pub fn reading(&self, window: &str) -> Option<WindowReading> {
        self.windows.get(window).copied()
    }

    /// The worst api_key bucket still standing at `now`: its reset ahead, or read within `BUCKET_LIFE_MS` when it
    /// named none; `None` when every bucket was refunded or aged, which reads as untouched.
    pub fn worst_bucket(&self, now: i64) -> Option<WindowReading> {
        self.windows
            .iter()
            .filter(|(name, _)| is_model_bucket(name))
            .filter(|(_, reading)| match reading.reset {
                Some(reset) => reset > now,
                None => now - reading.observed_at < BUCKET_LIFE_MS,
            })
            .map(|(_, reading)| *reading)
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    }

    /// A builder for tests.
    pub fn with(mut self, window: &str, reading: WindowReading) -> UtilizationSample {
        self.windows.insert(window.to_string(), reading);
        self
    }
}

// A pin write goes out on nearly every placement and moves once into the writer's batch; boxing it would put an
// allocation on that path to shrink the rarer writes queued beside it.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum StateWrite {
    /// The relay's verdict on an account, fenced by the incarnation and the generations the attempt that
    /// formed it went out with. It travels with the rest of the routing writes so shutdown drains it like any
    /// other.
    AccountState {
        account_id: String,
        state: AccountState,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    DeleteAffinity(Vec<(String, String)>),
    DeleteCooldown(String, String),
    DeletePin(PinKey),
    /// The purge tick's sweep of window and credits rows no current holder of their id wrote.
    PurgeStaleWindows,
    /// One resolution landing, committed as one transaction so the store never holds moved rows without the
    /// spelling row that finds them after a restart.
    Rekey(RekeyWrite),
    UpsertAffinity(Vec<AffinityRow>),
    UpsertCooldown(CooldownRow),
    UpsertCredits(CreditsRow),
    UpsertOverage(OverageRow),
    UpsertPin(PinRow),
    UpsertWindow(WindowRow),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub affinity: Vec<(String, String)>,
    pub cooldowns: Vec<(String, String)>,
    /// Each dropped pin with the one fact that dropped it: `deleted`, `expired`, or `unserved`.
    pub pins: Vec<(PinKey, &'static str)>,
}

/// A member's spend in one budgeted period, as the ledger last answered it; memory only. The period it was summed
/// over travels with it: a row whose `period` the member carries no budget for, or whose bounds do not hold
/// `now`, is no obstacle.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberSpend {
    pub pool_id: String,
    pub account_id: String,
    pub period: BudgetPeriod,
    pub period_start: i64,
    pub period_end: i64,
    pub spent_usd: f64,
    pub observed_at: i64,
}

/// A candidate's sort key in `choose`: group, priority, and the overage bit under the kind order; otherwise 1 for
/// a candidate kept on extra usage with its model week spent (0 for every other), priority, and the overage bit.
pub type Tier = (u8, u32, u8);

/// One usage report's token counts, the shape the relay reads off `Usage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinUsage {
    pub input: u64,
    pub cache_read: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub output: u64,
}

/// The share of an account's own window a completed request consumed, priced through the ledger's tables, which
/// this crate cannot name; the default prices nothing, so a task spawned without one predicts from readings alone.
#[derive(Clone)]
pub struct SharePricer {
    price: Arc<Price>,
    measured: Arc<Measured>,
}

type Price = dyn Fn(&str, &str, &str, PinUsage, i64) -> Option<f64> + Send + Sync;
type Measured = dyn Fn(&str) -> bool + Send + Sync;

impl SharePricer {
    /// `price(window, tier, canonical_model, usage, at)`: the share of `window` on `tier` at `at` (the band
    /// weighs the 5-hour window at a completion), `None` for a window the tables cannot price for the model on
    /// the tier.
    pub fn new(
        price: impl Fn(&str, &str, &str, PinUsage, i64) -> Option<f64> + Send + Sync + 'static,
    ) -> SharePricer {
        SharePricer {
            price: Arc::new(price),
            measured: Arc::new(|_| true),
        }
    }

    /// `measured(window)`: whether the tables price `window` from cells of its own rather than as the 7-day share
    /// once; every window reads measured until this is set.
    pub fn with_measured(
        self,
        measured: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> SharePricer {
        SharePricer {
            measured: Arc::new(measured),
            ..self
        }
    }

    pub fn price(
        &self,
        window: &str,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        (self.price)(window, tier, canonical_model, usage, at)
    }

    pub fn measured(&self, window: &str) -> bool {
        (self.measured)(window)
    }
}

impl Default for SharePricer {
    /// Prices nothing: a task spawned without the ledger's tables predicts from readings alone.
    fn default() -> Self {
        SharePricer::new(|_, _, _, _, _| None)
    }
}

// A closure has no `Debug`, and `RouterConfig` derives it, so the pricer prints its name alone.
impl std::fmt::Debug for SharePricer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharePricer")
    }
}

/// The router's own requests completed on one account since its readings, as `(at, share)` per plan window.
#[derive(Clone, Debug, Default)]
struct Charges(HashMap<String, Vec<(i64, f64)>>);

impl Charges {
    /// The window's entries, empty for a window nothing was charged to.
    fn of(&self, window: &str) -> &[(i64, f64)] {
        self.0.get(window).map_or(&[], Vec::as_slice)
    }

    fn of_mut(&mut self, window: &str) -> &mut Vec<(i64, f64)> {
        self.0.entry(window.to_string()).or_default()
    }
}

/// The price of one token of each type; `None` for a type the table cannot price on this model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub input: Option<f64>,
    pub cache_read: Option<f64>,
    pub write_5m: Option<f64>,
    pub write_1h: Option<f64>,
    pub output: Option<f64>,
}

/// `count` tokens (or tokens a second) at `rate`: nothing burned prices at zero whatever the rate, and a burned
/// type without a rate has no price.
fn priced(count: f64, rate: Option<f64>) -> Option<f64> {
    match rate {
        Some(rate) => Some(count * rate),
        None if count == 0.0 => Some(0.0),
        None => None,
    }
}

/// A price per token by model and token type; `fallback` prices a model the table does not list.
#[derive(Clone, Debug, PartialEq)]
pub struct RateTable {
    pub rows: HashMap<String, Rates>,
    pub fallback: Rates,
}

impl RateTable {
    /// Every token of every model at 1, so a burn priced through it is a token count.
    pub fn unit() -> RateTable {
        RateTable {
            rows: HashMap::new(),
            fallback: Rates {
                input: Some(1.0),
                cache_read: Some(1.0),
                write_5m: Some(1.0),
                write_1h: Some(1.0),
                output: Some(1.0),
            },
        }
    }

    /// The row `listing::row_for` finds for `canonical_model`, else `fallback`.
    pub fn rates_for(&self, canonical_model: &str) -> Rates {
        *row_for(canonical_model, |k| self.rows.get(k)).unwrap_or(&self.fallback)
    }

    /// The price of one second of a pin's burn; `None` when a type the pin burns has no rate.
    pub fn price_burn(&self, canonical_model: &str, burn: &PinBurn) -> Option<f64> {
        let rates = self.rates_for(canonical_model);
        Some(
            priced(burn.input, rates.input)?
                + priced(burn.cache_read, rates.cache_read)?
                + priced(burn.write_5m, rates.write_5m)?
                + priced(burn.write_1h, rates.write_1h)?
                + priced(burn.output, rates.output)?,
        )
    }

    /// Each count times its rate, summed; `None` when a type with a count has no rate.
    pub fn price(&self, canonical_model: &str, usage: PinUsage) -> Option<f64> {
        let rates = self.rates_for(canonical_model);
        Some(
            priced(usage.input as f64, rates.input)?
                + priced(usage.cache_read as f64, rates.cache_read)?
                + priced(usage.write_5m as f64, rates.write_5m)?
                + priced(usage.write_1h as f64, rates.write_1h)?
                + priced(usage.output as f64, rates.output)?,
        )
    }
}

/// Which figure a reader takes for a pin's burn: the share of the account's 5-hour window while a subscription
/// account runs on its plan, dollars on an api_key account or a subscription account in overage. Serializes
/// as `window_share` or `usd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BurnUnit {
    Usd,
    WindowShare,
}

impl BurnUnit {
    pub fn of(kind: AccountKind, in_overage: bool) -> BurnUnit {
        match (kind, in_overage) {
            (AccountKind::Oauth, false) => BurnUnit::WindowShare,
            (AccountKind::Oauth, true) | (AccountKind::ApiKey, _) => BurnUnit::Usd,
        }
    }

    /// `pin.burn_banded` under `WindowShare` (the band is part of what the meter charged), `pin.burn` under `Usd`.
    pub fn read(self, pin: &PinRow) -> PinBurn {
        match self {
            BurnUnit::Usd => pin.burn,
            BurnUnit::WindowShare => pin.burn_banded,
        }
    }

    /// The table `rates` prices this unit's copy through.
    pub fn table(self, rates: &BurnRates) -> &RateTable {
        match self {
            BurnUnit::Usd => &rates.dollars,
            BurnUnit::WindowShare => &rates.five_hour,
        }
    }
}

/// A burn average decayed from its last fold to `now` with time constant `window_ms`. An average never folded
/// has nothing to decay and reads as stored.
pub fn burn_decayed(burn: f64, burn_at: Option<i64>, now: i64, window_ms: i64) -> f64 {
    match burn_at {
        None => burn,
        // A report dated before the last fold (two relays' clocks) decays nothing rather than growing it.
        Some(at) => burn * (-((now - at).max(0) as f64) / window_ms.max(1) as f64).exp(),
    }
}

/// The fold a renewal with usage applies at `now`: the average decayed to `now`, plus `tokens` spread over the
/// window, so the average is tokens per second.
pub(crate) fn burn_folded(
    burn: f64,
    burn_at: Option<i64>,
    now: i64,
    window_ms: i64,
    tokens: f64,
) -> f64 {
    burn_decayed(burn, burn_at, now, window_ms) + tokens / (window_ms.max(1) as f64 / 1_000.0)
}

/// Whether the pin's cache entry is still alive at `now`: its last refresh is younger than the TTL in force
/// for it.
pub fn warm(pin: &PinRow, now: i64) -> bool {
    now - pin.last_refresh_at < pin.cache_ttl_ms
}

/// The pins a round marks, in the round's order: never the last unmarked pin, never an unpriced pin, nothing for
/// a zero target.
pub fn shed_marks(ordered: &[(PinRow, Option<f64>)], target: f64) -> Vec<PinKey> {
    let mut marks = Vec::new();
    if target.is_nan() || target <= 0.0 {
        return marks;
    }
    let mut marked = 0.0;
    for (pin, burn) in ordered {
        // The last unmarked pin stays whatever the target still asks, so no round takes all of an account's
        // burn; an indivisible pin may carry the marked burn past the target, and the round stops there.
        if marked >= target || marks.len() + 1 >= ordered.len() {
            break;
        }
        // An unpriced burn moves the marked sum nowhere, so marking the pin would shed a session on a guess: it
        // stays, and it still counts as one of the account's pins for the last-pin rule.
        let Some(burn) = burn else {
            continue;
        };
        marks.push(pin.key());
        marked += burn;
    }
    marks
}

/// Past this age a permit or a ticket is a guard whose drop never reached the table: the relay bounds a
/// stream's silence, not its length, so a request older than the longest turn a client runs is one the table
/// gives up waiting on.
pub const IN_FLIGHT_RECLAIM_MS: i64 = 3_600_000;

#[derive(Default)]
struct InFlightTable {
    next_id: u64,
    keys: HashMap<String, HashMap<u64, i64>>,
    accounts: HashMap<String, HashMap<u64, i64>>,
}

/// The table and the notifier a freed account slot wakes, shared by the handle, the task and every guard.
#[derive(Default)]
struct InFlightShared {
    table: Mutex<InFlightTable>,
    freed: Notify,
}

impl InFlightShared {
    fn table(&self) -> MutexGuard<'_, InFlightTable> {
        self.table.lock().expect("in-flight table lock poisoned")
    }
}

/// Requests in flight per key and per account, counted from the relay's synchronous calls; shared by the
/// handle and the task. A key permit is one per request, taken before selection and held across every attempt;
/// it counts the key's requests and bounds nothing. An account ticket is one per wire attempt, a token count's
/// excepted (it is bound to no cap), a leg's first taken by the selection that chose the account and each later
/// one at its dial. Each is released exactly once, by its guard's drop, when the response it counts ends.
#[derive(Clone, Default)]
pub struct InFlight(Arc<InFlightShared>);

impl InFlight {
    /// Counts one more request on the key; a key has no cap, so the permit is never refused.
    pub fn try_acquire_key(&self, key_id: &str, now: i64) -> KeyPermit {
        let mut table = self.0.table();
        let id = table.next_id;
        table.next_id += 1;
        table
            .keys
            .entry(key_id.to_string())
            .or_default()
            .insert(id, now);
        KeyPermit {
            shared: self.0.clone(),
            key_id: key_id.to_string(),
            id,
        }
    }

    pub fn dispatched(&self, account_id: &str, now: i64) -> AccountTicket {
        let mut table = self.0.table();
        let id = table.next_id;
        table.next_id += 1;
        table
            .accounts
            .entry(account_id.to_string())
            .or_default()
            .insert(id, now);
        AccountTicket {
            shared: self.0.clone(),
            account_id: account_id.to_string(),
            id,
        }
    }

    pub fn account(&self, account_id: &str) -> u32 {
        self.0
            .table()
            .accounts
            .get(account_id)
            .map(HashMap::len)
            .unwrap_or(0) as u32
    }

    pub fn key(&self, key_id: &str) -> u32 {
        self.0
            .table()
            .keys
            .get(key_id)
            .map(HashMap::len)
            .unwrap_or(0) as u32
    }

    /// Drops permits and tickets older than `IN_FLIGHT_RECLAIM_MS` (one hour): a guard whose drop never
    /// landed.
    pub fn expire(&self, now: i64) -> usize {
        let reclaimed = {
            let mut table = self.0.table();
            let mut reclaimed = 0;
            for slots in table.keys.values_mut() {
                let before = slots.len();
                slots.retain(|_, taken_at| now - *taken_at <= IN_FLIGHT_RECLAIM_MS);
                reclaimed += before - slots.len();
            }
            table.keys.retain(|_, slots| !slots.is_empty());
            for slots in table.accounts.values_mut() {
                let before = slots.len();
                slots.retain(|_, taken_at| now - *taken_at <= IN_FLIGHT_RECLAIM_MS);
                reclaimed += before - slots.len();
            }
            table.accounts.retain(|_, slots| !slots.is_empty());
            reclaimed
        };
        if reclaimed > 0 {
            self.0.freed.notify_waiters();
        }
        reclaimed
    }

    /// Resolves when `account_id`'s count is under `cap` or at `deadline`, whichever first; true for a slot.
    /// The slot is read, not reserved: the caller's next selection charges it, or finds that another request's
    /// selection already did.
    pub async fn wait_for_slot(&self, account_id: &str, cap: u32, deadline: Instant) -> bool {
        loop {
            // Registered before the count is read, so a release landing between the read and the wait still
            // wakes this waiter.
            let freed = self.0.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if self.account(account_id) < cap {
                return true;
            }
            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                return self.account(account_id) < cap;
            }
        }
    }
}

/// Dropping releases the key's count once; a permit the purge already reclaimed releases nothing.
pub struct KeyPermit {
    shared: Arc<InFlightShared>,
    key_id: String,
    id: u64,
}

impl Drop for KeyPermit {
    fn drop(&mut self) {
        let mut table = self.shared.table();
        if let Some(slots) = table.keys.get_mut(&self.key_id) {
            slots.remove(&self.id);
            if slots.is_empty() {
                table.keys.remove(&self.key_id);
            }
        }
    }
}

/// Dropping releases the account's count once and wakes every waiter on a slot; same reclaim rule.
pub struct AccountTicket {
    shared: Arc<InFlightShared>,
    account_id: String,
    id: u64,
}

impl Drop for AccountTicket {
    fn drop(&mut self) {
        let released = {
            let mut table = self.shared.table();
            let Some(slots) = table.accounts.get_mut(&self.account_id) else {
                return;
            };
            let released = slots.remove(&self.id).is_some();
            if slots.is_empty() {
                table.accounts.remove(&self.account_id);
            }
            released
        };
        if released {
            self.shared.freed.notify_waiters();
        }
    }
}

/// When a shedding round ran, and the pins it marked.
type ShedRound = (i64, HashSet<PinKey>);

pub struct RoutingState {
    /// The ledger's standing from its last `Report::Accounting`: while it is degraded a budgeted member's spend rows
    /// may lack a batch the writer still holds. Memory only, so a restart starts clear until the ledger's first
    /// refresh reports again.
    accounting_degraded: bool,
    /// Shared with the handle the way `in_flight` is, so a lookup never waits on a command round trip.
    affinity: Arc<RwLock<AffinityMap>>,
    /// The router's own requests since each account's readings, what `predicted` adds to them. Memory only: a
    /// restart loads the newest readings and predicts from them alone until the next request completes.
    charges: HashMap<String, Charges>,
    cooldowns: HashMap<(String, String), CooldownRow>,
    /// The poll's newest extra-usage state per account, loaded at startup from the rows the last run persisted.
    credits: HashMap<String, CreditsState>,
    in_flight: InFlight,
    /// The ledger's last-answered spend per pool member and budgeted period, by `(pool_id, account_id, period)`;
    /// memory only.
    member_spend: HashMap<(String, String, BudgetPeriod), MemberSpend>,
    /// When the newest round ran on each account: what a move's cool check reads, so no move prefers a destination
    /// just shed from. A stamp of its own, since the rounds are keyed by pool and an account that moved pools keeps
    /// the round its former pool ran. Memory only, as the rounds are.
    newest_shed: HashMap<String, i64>,
    next_epoch: u64,
    overage: HashMap<String, OverageEvidence>,
    pins: HashMap<PinKey, PinRow>,
    /// The ledger's tables, which this crate cannot name, handed in by the task; unit tables until then, so a
    /// burn read through them is a token count.
    rates: BurnRates,
    /// The pins some selection or completion renewed since their placement. A client error on the request that
    /// created a pin releases the pin only while its key is absent here: a renewal means the pin serves another
    /// turn, however close in time. Memory only, since no request outlives a restart to report on a loaded pin.
    renewed: HashSet<PinKey>,
    /// A folded spelling to the dated id it resolved to, so every lookup below can key a model's rows on the
    /// dated id once it is known, whichever spelling the caller names.
    resolutions: HashMap<String, String>,
    round_robin: HashMap<(String, Tier, String), usize>,
    samples: HashMap<String, UtilizationSample>,
    /// The latest shedding round on each account, by the id of the pool that ran it and the account id. Memory only,
    /// so a restart forgets the marks with the dwell and the next hot request may start a fresh round.
    shed_rounds: HashMap<(String, String), ShedRound>,
}

impl RoutingState {
    pub fn from_load(load: RoutingLoad) -> RoutingState {
        let next_epoch = load.pins.iter().map(|p| p.epoch).max().unwrap_or(0) + 1;
        let mut affinity_map = AffinityMap::from_rows(load.affinity);
        for resolution in &load.resolutions {
            affinity_map.learn_resolution(&resolution.spelling, &resolution.dated_id);
        }
        let mut samples: HashMap<String, UtilizationSample> = HashMap::new();
        for row in load.windows {
            let sample = samples.entry(row.account_id).or_default();
            sample.observed_at = sample.observed_at.max(row.observed_at);
            sample.windows.insert(
                row.window,
                WindowReading {
                    utilization: row.utilization,
                    reset: row.reset,
                    observed_at: row.observed_at,
                },
            );
        }
        RoutingState {
            accounting_degraded: false,
            affinity: Arc::new(RwLock::new(affinity_map)),
            charges: HashMap::new(),
            cooldowns: load
                .cooldowns
                .into_iter()
                .map(|c| ((c.account_id.clone(), c.scope.clone()), c))
                .collect(),
            credits: load
                .credits
                .into_iter()
                .map(|row| {
                    (
                        row.account_id,
                        CreditsState {
                            enabled: row.enabled,
                            disabled_reason: row.disabled_reason,
                            utilization: row.utilization,
                            observed_at: row.observed_at,
                            incarnation: row.incarnation,
                        },
                    )
                })
                .collect(),
            in_flight: InFlight::default(),
            member_spend: HashMap::new(),
            newest_shed: HashMap::new(),
            next_epoch,
            overage: load
                .overage
                .into_iter()
                .map(|o| {
                    (
                        o.account_id.clone(),
                        OverageEvidence {
                            in_use: o.in_use,
                            since: o.since,
                            reset: o.reset,
                            observed_at: o.observed_at,
                            source: if o.source == "sample" {
                                EvidenceSource::Sample
                            } else {
                                EvidenceSource::Head
                            },
                            incarnation: o.incarnation,
                        },
                    )
                })
                .collect(),
            pins: load.pins.into_iter().map(|p| (p.key(), p)).collect(),
            rates: BurnRates::default(),
            renewed: HashSet::new(),
            resolutions: load
                .resolutions
                .into_iter()
                .map(|r| (r.spelling, r.dated_id))
                .collect(),
            round_robin: HashMap::new(),
            samples,
            shed_rounds: HashMap::new(),
        }
    }

    pub(crate) fn fresh_epoch(&mut self) -> u64 {
        let e = self.next_epoch;
        self.next_epoch += 1;
        e
    }

    /// The name the state keys a model's rows on: the dated id `model` resolved to, else `model` itself.
    pub fn canonical<'a>(&'a self, model: &'a str) -> &'a str {
        self.resolutions
            .get(model)
            .map(String::as_str)
            .unwrap_or(model)
    }

    /// `canonical` applied to a cooldown scope: `model:{spelling}` becomes `model:{dated}`; `account` stays.
    pub fn canonical_scope(&self, scope: &str) -> String {
        match scope.strip_prefix("model:") {
            Some(spelling) => format!("model:{}", self.canonical(spelling)),
            None => scope.to_string(),
        }
    }

    /// `canonical` applied to a lane's model part, the one ahead of its last three: a lane composed under a
    /// spelling before its resolution landed then names the model its pin and entries were relaned to.
    pub fn canonical_lane(&self, lane: &str) -> String {
        match lane.rmatch_indices('|').nth(2) {
            Some((at, _)) => format!("{}{}", self.canonical(&lane[..at]), &lane[at..]),
            None => lane.to_string(),
        }
    }

    pub fn pin(&self, key: &PinKey) -> Option<&PinRow> {
        let canonical = PinKey {
            model: self.canonical(&key.model).to_string(),
            ..key.clone()
        };
        self.pins.get(&canonical)
    }

    /// A fresh pin under a key another pin held (a release that lands the session elsewhere) takes the old one
    /// off its account, so it spends that pin's mark as a removal does and starts unrenewed; a renewal keeps the
    /// epoch, the mark, and the renewal.
    pub(crate) fn put_pin(&mut self, mut pin: PinRow) {
        pin.model = self.canonical(&pin.model).to_string();
        let key = pin.key();
        let epoch = pin.epoch;
        match self.pins.insert(key.clone(), pin) {
            Some(replaced) if replaced.epoch == epoch => {}
            Some(replaced) => {
                self.spend_mark(&replaced);
                self.renewed.remove(&key);
            }
            None => {
                self.renewed.remove(&key);
            }
        }
    }

    /// Records that a selection or a completion renewed the pin under the key since its placement.
    pub(crate) fn mark_renewed(&mut self, key: &PinKey) {
        let key = PinKey {
            model: self.canonical(&key.model).to_string(),
            ..key.clone()
        };
        if self.pins.contains_key(&key) {
            self.renewed.insert(key);
        }
    }

    /// Whether the pin under the key was renewed since its placement.
    pub fn renewed_since_placement(&self, key: &PinKey) -> bool {
        let key = PinKey {
            model: self.canonical(&key.model).to_string(),
            ..key.clone()
        };
        self.renewed.contains(&key)
    }

    pub(crate) fn remove_pin(&mut self, key: &PinKey) -> Option<PinRow> {
        let key = PinKey {
            model: self.canonical(&key.model).to_string(),
            ..key.clone()
        };
        self.take_pin(&key)
    }

    /// The one way a pin leaves the map: a pin that left its account, by a move, a fail-over, an expiry, a
    /// release or the purge, spends its mark there, so coming back inside the dwell does not shed it on a round
    /// that marked it before it left.
    fn take_pin(&mut self, key: &PinKey) -> Option<PinRow> {
        let pin = self.pins.remove(key)?;
        self.renewed.remove(key);
        self.spend_mark(&pin);
        Some(pin)
    }

    fn spend_mark(&mut self, pin: &PinRow) {
        // The account's rounds are walked rather than the pin's pool resolved through a snapshot this state does
        // not hold; the walk also spends a mark left in the round of a pool the key moved out of inside its dwell.
        for ((_, account_id), (_, marks)) in self.shed_rounds.iter_mut() {
            if account_id == &pin.account_id {
                marks.remove(&pin.key());
            }
        }
    }

    /// The next index into `cohort` for a pool's tier under round robin. The key carries the cohort because
    /// `resolve` builds a candidate list per model: a singleton model and a three-account model share a pool and
    /// a tier but rotate apart, and a cohort whose membership changed is a key of its own that starts at 0. An
    /// account id is a slug, so joining the sorted ids names one cohort and no other; the caller sorts, and an
    /// unsorted cohort would split one rotation into several. In memory, so a restart starts every round again.
    /// Only a placement passes the turn on (`advance`): a held request lands nowhere, so the account it named
    /// keeps its turn for the placement that follows.
    pub(crate) fn round_robin_next(
        &mut self,
        pool_id: &str,
        tier: Tier,
        cohort: &[&str],
        advance: bool,
    ) -> usize {
        let key = (pool_id.to_string(), tier, cohort.join(","));
        let index = self
            .round_robin
            .get(&key)
            .map_or(0, |slot| slot % cohort.len());
        if advance {
            self.round_robin.insert(key, index + 1);
        }
        index
    }

    /// The later deadline always wins, as in the store, and `set_at` advances on its own whenever the
    /// incoming rejection is the newer one. True when either moved, which is what puts the row on the writer.
    pub fn record_cooldown(&mut self, mut row: CooldownRow) -> bool {
        row.scope = self.canonical_scope(&row.scope);
        let key = (row.account_id.clone(), row.scope.clone());
        let Some(current) = self.cooldowns.get_mut(&key) else {
            self.cooldowns.insert(key, row);
            return true;
        };
        // The deadline only ever grows, but every rejection dates itself whatever deadline it announces: a
        // repeat under the standing one still moves the fence recovery has to be newer than, or a sample
        // taken between the two rejections clears a row the upstream had just set again.
        let later_deadline = row.until > current.until;
        let later_rejection = row.set_at > current.set_at;
        if later_deadline {
            let set_at = current.set_at.max(row.set_at);
            *current = row;
            current.set_at = set_at;
        } else if later_rejection {
            current.set_at = row.set_at;
        }
        later_deadline || later_rejection
    }

    pub fn clear_cooldown(&mut self, account_id: &str, scope: &str) -> bool {
        self.cooldowns
            .remove(&(account_id.to_string(), scope.to_string()))
            .is_some()
    }

    /// The live rate-limit rows on the account that `sample` shows recovered: the window the row answers for (a
    /// model row's is the model's own week) has allowance again, read from a sample taken after the rejection that
    /// set the row. A window the sample never read is not evidence, and neither is a row no usage window answers
    /// for. A row already past its deadline is left to the purge tick, which says so. Only an OAuth account is ever
    /// sampled, which is what keeps an api-key account's rows out of here without a registry lookup.
    pub fn recovered_cooldowns(
        &self,
        account_id: &str,
        sample: &UtilizationSample,
        now: i64,
    ) -> Vec<CooldownRow> {
        let below = |window: &str| sample.reading(window).is_some_and(|r| r.utilization < 1.0);
        let recovered = |row: &CooldownRow| {
            // A deadline the upstream handed out names no window, whatever the row's scope, so a reading is a
            // guess about which window the rejection was for.
            if row.window == "retry_after" {
                return false;
            }
            // A model row recovers on the model's own week: its window keeps the upstream's claim, which the
            // client reads, and the spend cap's claim names no family's week.
            if let Some(model) = row.scope.strip_prefix("model:") {
                return below(window_for(model));
            }
            match row.window.as_str() {
                "five_hour" => below(FIVE_HOUR),
                "seven_day" => below(SEVEN_DAY),
                // Any other representative claim: it does not say which shared window was exhausted, so the
                // account is only back once both have allowance.
                _ => below(FIVE_HOUR) && below(SEVEN_DAY),
            }
        };
        let mut rows: Vec<_> = self
            .cooldowns
            .values()
            .filter(|row| {
                row.account_id == account_id
                    && row.reason == "rate_limit"
                    && row.until > now
                    && sample.observed_at > row.set_at
                    && recovered(row)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.scope.cmp(&b.scope));
        rows
    }

    /// The live row that blocks `canonical_model` on the account, the later of the account-wide and the model row.
    pub fn cooldown(
        &self,
        account_id: &str,
        canonical_model: &str,
        now: i64,
    ) -> Option<&CooldownRow> {
        self.blocking_cooldown(account_id, canonical_model, now, false)
    }

    /// The later of the account's live rows `cooldown_holds` answers true for, the model canonicalized as
    /// `cooldown` does; for a standard request it answers what `cooldown` answers, an account row winning a tie.
    pub fn blocking_cooldown(
        &self,
        account_id: &str,
        canonical_model: &str,
        now: i64,
        fast: bool,
    ) -> Option<&CooldownRow> {
        let canonical_model = self.canonical(canonical_model);
        let model_scope = format!("model:{canonical_model}");
        ["account", "fast", "fast_credits", model_scope.as_str()]
            .into_iter()
            .filter_map(|scope| {
                self.cooldowns
                    .get(&(account_id.to_string(), scope.to_string()))
            })
            .filter(|row| {
                row.until > now && cooldown_holds(&row.scope, &row.window, canonical_model, fast)
            })
            .fold(None, |held: Option<&CooldownRow>, row| match held {
                Some(held) if held.until >= row.until => Some(held),
                _ => Some(row),
            })
    }

    /// Only a newer reading replaces the held one, so a slow poll cannot roll the state back; a reading of another
    /// holder of the id is no timeline to be newer than.
    pub fn record_credits(&mut self, account_id: &str, state: CreditsState) -> bool {
        if self.credits.get(account_id).is_some_and(|held| {
            held.incarnation == state.incarnation && held.observed_at >= state.observed_at
        }) {
            return false;
        }
        self.credits.insert(account_id.to_string(), state);
        true
    }

    pub fn credits(&self, account_id: &str) -> Option<&CreditsState> {
        self.credits.get(account_id)
    }

    /// The credits state of the holder `incarnation` names: one a previous holder of the id left, which the purge
    /// tick drops only on its next pass, says nothing about the account the id names now.
    pub fn credits_of(&self, account_id: &str, incarnation: u64) -> Option<&CreditsState> {
        self.credits
            .get(account_id)
            .filter(|state| state.incarnation == incarnation)
    }

    /// Every live cooldown row of one account, whatever its scope, in no particular order. `cooldown` answers
    /// the one row that blocks a model; this is for a reader asking whether anything on the account is still
    /// standing.
    pub fn cooldowns_of<'a>(
        &'a self,
        account_id: &'a str,
        now: i64,
    ) -> impl Iterator<Item = &'a CooldownRow> + 'a {
        self.cooldowns
            .values()
            .filter(move |row| row.account_id == account_id && row.until > now)
    }

    pub fn cooldowns(&self) -> Vec<CooldownRow> {
        let mut v: Vec<_> = self.cooldowns.values().cloned().collect();
        v.sort_by(|a, b| (&a.account_id, &a.scope).cmp(&(&b.account_id, &b.scope)));
        v
    }

    /// Only newer evidence replaces the current state, so a slow response cannot roll it back.
    pub fn record_overage(&mut self, account_id: &str, evidence: OverageEvidence) -> bool {
        let current = self.overage.get(account_id);
        if current.is_some_and(|c| c.observed_at >= evidence.observed_at) {
            return false;
        }
        let since = since_of(current, &evidence);
        self.overage.insert(
            account_id.to_string(),
            OverageEvidence { since, ..evidence },
        );
        true
    }

    /// `OverageEvidence::stands` over the account's evidence; false without evidence.
    pub fn in_overage(&self, account_id: &str, now: i64, fallback_ms: i64) -> bool {
        self.overage
            .get(account_id)
            .is_some_and(|e| e.stands(now, fallback_ms))
    }
    pub fn overage(&self, account_id: &str) -> Option<&OverageEvidence> {
        self.overage.get(account_id)
    }
    pub fn overages(&self) -> Vec<(String, OverageEvidence)> {
        let mut v: Vec<_> = self
            .overage
            .iter()
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// When the account counts as cooled under `never`: the latest reset among its exhausted shared windows (the
    /// evidence's `reset` is built that way on both paths), else `fallback_ms` (the bootstrap's
    /// `cooldown_fallback_seconds`) past the observation and then re-checked, because a permanent skip would
    /// need an operator to notice.
    pub fn overage_deadline(&self, account_id: &str, fallback_ms: i64) -> Option<i64> {
        let evidence = self.overage.get(account_id).filter(|e| e.in_use)?;
        Some(evidence.reset.unwrap_or(evidence.observed_at + fallback_ms))
    }

    /// Folds `sample` in window by window and answers the names of the windows it moved, empty when none. A
    /// reading lands only when it is newer than the one already stored for that name, so an api_key head that
    /// carries one model's buckets never erases another model's. A moved plan window's charges observed at or
    /// before the new reading are dropped, since the reading carries them. Past `MAX_BUCKETS_PER_ACCOUNT` bucket
    /// windows the oldest readings leave, and a window that left is not reported as moved.
    pub fn record_sample(&mut self, account_id: &str, sample: UtilizationSample) -> Vec<String> {
        if sample.windows.is_empty() {
            return Vec::new();
        }
        let current = self.samples.entry(account_id.to_string()).or_default();
        let mut moved = Vec::new();
        for (window, reading) in sample.windows {
            let newer = current
                .windows
                .get(&window)
                .is_none_or(|existing| reading.observed_at > existing.observed_at);
            if newer {
                // Looked up rather than created: most of a sample's names (the api_key and fast buckets) are never
                // charged.
                if let Some(charges) = self
                    .charges
                    .get_mut(account_id)
                    .and_then(|c| c.0.get_mut(&window))
                {
                    charges.retain(|(at, _)| *at > reading.observed_at);
                }
                current.windows.insert(window.clone(), reading);
                moved.push(window);
            }
        }
        // An account asked for many models would otherwise grow a bucket per model between two purge ticks.
        let buckets = current
            .windows
            .keys()
            .filter(|window| is_bucket_window(window))
            .count();
        if buckets > MAX_BUCKETS_PER_ACCOUNT {
            let mut oldest: Vec<(i64, String)> = current
                .windows
                .iter()
                .filter(|(window, _)| is_bucket_window(window))
                .map(|(window, reading)| (reading.observed_at, window.clone()))
                .collect();
            oldest.sort();
            for (_, window) in oldest.into_iter().take(buckets - MAX_BUCKETS_PER_ACCOUNT) {
                current.windows.remove(&window);
                moved.retain(|w| *w != window);
            }
        }
        if !moved.is_empty() {
            current.observed_at = current
                .windows
                .values()
                .map(|r| r.observed_at)
                .max()
                .unwrap_or(0);
        }
        moved
    }

    /// Books the shares on their windows at `at`; a window with no entry keeps its reading. A charge lives until a
    /// reading observed after it lands on its window, or a week.
    pub fn charge(&mut self, account_id: &str, at: i64, shares: &[(&str, f64)]) {
        let charges = self.charges.entry(account_id.to_string()).or_default();
        for (window, share) in shares {
            let entries = charges.of_mut(window);
            entries.push((at, *share));
            // No window outlives a week, so a charge older than that is past every reset it could count toward.
            entries.retain(|(booked, _)| *booked >= at - WEEK_MS);
        }
    }

    /// One window's reading at `now`, what every reader of a plan window takes. Before the reading's reset, or
    /// with none named: the reading, plus the share of the router's own requests completed on the account since
    /// it was observed. At or past the reset: zero plus the share of the requests completed since the reset; a
    /// weekly window's reset advanced by whole weeks past `now`, a 5-hour window's `None` until the next reading
    /// names one. `None` while the account holds no reading of the window.
    pub fn predicted(&self, account_id: &str, window: &str, now: i64) -> Option<WindowReading> {
        let reading = self.samples.get(account_id)?.reading(window)?;
        let charges = self
            .charges
            .get(account_id)
            .map_or(&[][..], |c| c.of(window));
        // Folded from positive zero: an empty `sum` of floats is negative zero, which a JSON reader shows as -0.
        let since = |after: i64| {
            charges
                .iter()
                .filter(|(at, _)| *at > after)
                .fold(0.0, |sum, (_, share)| sum + share)
        };
        match reading.reset {
            Some(reset) if reset <= now => match window {
                // The weekly windows reset at the same weekday time every week, so the reading's reset names every
                // later one; the 5-hour window opens with the next request and names its reset in the next reading.
                w if w.starts_with(SEVEN_DAY) => {
                    let next = reset + ((now - reset) / WEEK_MS + 1) * WEEK_MS;
                    Some(WindowReading {
                        utilization: since(next - WEEK_MS),
                        reset: Some(next),
                        observed_at: reading.observed_at,
                    })
                }
                _ => Some(WindowReading {
                    utilization: since(reset),
                    reset: None,
                    observed_at: reading.observed_at,
                }),
            },
            _ => Some(WindowReading {
                utilization: reading.utilization + since(reading.observed_at),
                ..reading
            }),
        }
    }

    /// The account's sample with each plan window replaced by its prediction at `now`, the api_key and fast-lane
    /// buckets as read; what `RouterHandle::samples` answers.
    pub fn predicted_sample(&self, account_id: &str, now: i64) -> Option<UtilizationSample> {
        let mut sample = self.samples.get(account_id)?.clone();
        let plan: Vec<String> = sample
            .windows
            .keys()
            .filter(|window| is_plan_window(window))
            .cloned()
            .collect();
        for window in plan {
            if let Some(predicted) = self.predicted(account_id, &window, now) {
                sample.windows.insert(window, predicted);
            }
        }
        Some(sample)
    }

    /// The stored rows of `windows` on the account, for the writer.
    pub fn window_rows(
        &self,
        account_id: &str,
        windows: &[String],
        incarnation: u64,
    ) -> Vec<WindowRow> {
        let Some(sample) = self.samples.get(account_id) else {
            return Vec::new();
        };
        windows
            .iter()
            .filter_map(|window| {
                let reading = sample.reading(window)?;
                Some(WindowRow {
                    account_id: account_id.to_string(),
                    incarnation,
                    observed_at: reading.observed_at,
                    reset: reading.reset,
                    utilization: reading.utilization,
                    window: window.clone(),
                })
            })
            .collect()
    }

    pub fn sample(&self, account_id: &str) -> Option<&UtilizationSample> {
        self.samples.get(account_id)
    }

    /// The row that persists `state` for `account_id`.
    pub(crate) fn credits_row(account_id: &str, state: &CreditsState) -> CreditsRow {
        CreditsRow {
            account_id: account_id.to_string(),
            incarnation: state.incarnation,
            enabled: state.enabled,
            disabled_reason: state.disabled_reason.clone(),
            utilization: state.utilization,
            observed_at: state.observed_at,
        }
    }

    pub(crate) fn forget_sample(&mut self, account_id: &str) {
        self.samples.remove(account_id);
        self.charges.remove(account_id);
        self.credits.remove(account_id);
    }

    /// Every account's latest reading, by id: one snapshot for a caller that evaluates thresholds over all of them.
    pub fn samples(&self) -> Vec<(String, UtilizationSample)> {
        let mut out: Vec<_> = self
            .samples
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The shared in-flight table this state's selections read the cap against: the same one the handle a test
    /// dispatches through would hold a ticket on, so a test can seed the cap without a handle.
    pub fn in_flight(&self) -> &InFlight {
        &self.in_flight
    }

    pub(crate) fn set_in_flight(&mut self, in_flight: InFlight) {
        self.in_flight = in_flight;
    }

    /// The tables the shedding round prices each pin's burn through.
    pub fn rates(&self) -> &BurnRates {
        &self.rates
    }

    /// Set by the task at spawn from its config, and by a test that prices a round through its own tables.
    pub fn set_rates(&mut self, rates: BurnRates) {
        self.rates = rates;
    }

    /// Set by the task from the ledger's `Report::Accounting`, and by a test that selects under a degraded ledger.
    pub fn set_accounting_degraded(&mut self, degraded: bool) {
        self.accounting_degraded = degraded;
    }

    pub(crate) fn accounting_degraded(&self) -> bool {
        self.accounting_degraded
    }

    /// The shared affinity map: the same one the handle's `ConversationLookup` reads without a command round
    /// trip, so a report the writer just applied is visible to the very next request.
    pub fn affinity(&self) -> Arc<RwLock<AffinityMap>> {
        self.affinity.clone()
    }

    /// The member's spend rows, one per period the ledger reported, the day's before the month's.
    pub fn member_spends_of(
        &self,
        pool_id: &str,
        account_id: &str,
    ) -> impl Iterator<Item = &MemberSpend> {
        let (pool_id, account_id) = (pool_id.to_string(), account_id.to_string());
        [BudgetPeriod::Day, BudgetPeriod::Month]
            .into_iter()
            .filter_map(move |period| {
                self.member_spend
                    .get(&(pool_id.clone(), account_id.clone(), period))
            })
    }

    /// The member's spend rows, each only while the snapshot's member still carries a budget over its period:
    /// between a budget's removal and the next report, a stored row of that period is left out here instead of
    /// the analytics reading a figure that no longer applies.
    pub fn member_spends_live(
        &self,
        snapshot: &RegistrySnapshot,
        pool_id: &str,
        account_id: &str,
    ) -> Vec<&MemberSpend> {
        self.member_spends_of(pool_id, account_id)
            .filter(|row| member_spend_matches(snapshot, pool_id, account_id, row.period))
            .collect()
    }

    /// Replaces the named members' rows, keeping one only while the snapshot's member still carries a budget
    /// over the period the row was summed over: removing that budget retires the old spend at once.
    pub fn record_member_spend(&mut self, snapshot: &RegistrySnapshot, rows: Vec<MemberSpend>) {
        for row in rows {
            let key = (row.pool_id.clone(), row.account_id.clone(), row.period);
            if member_spend_matches(snapshot, &row.pool_id, &row.account_id, row.period) {
                self.member_spend.insert(key, row);
            } else {
                self.member_spend.remove(&key);
            }
        }
    }

    /// The pins of `account_id` that `keep` admits, each priced in `unit` through `rates` and decayed to `now`, and
    /// the sum over the priced pins; `keep` is the account's current pool, built by the caller from the snapshot
    /// this state does not hold, so a pin a former pool left on the account is outside the round. Under
    /// `WindowShare` a burn is the cost table's reference share per second, which orders the pins as the tier's
    /// share does; `None` for a pin burning a type its row cannot price.
    pub fn burn_of(
        &self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        unit: BurnUnit,
        rates: &BurnRates,
        now: i64,
        window_ms: i64,
    ) -> (Vec<(PinRow, Option<f64>)>, f64) {
        let table = unit.table(rates);
        let mut pins: Vec<(PinRow, Option<f64>)> = self
            .pins
            .values()
            .filter(|p| p.account_id == account_id && keep(p))
            .map(|p| {
                // Every type decays by the same factor, so decaying the price equals pricing the decayed rates.
                let burn = table
                    .price_burn(&p.model, &unit.read(p))
                    .map(|priced| burn_decayed(priced, p.burn_at, now, window_ms));
                (p.clone(), burn)
            })
            .collect();
        pins.sort_by(|a, b| {
            (&a.0.key_id, &a.0.session_id, &a.0.agent_id, &a.0.model).cmp(&(
                &b.0.key_id,
                &b.0.session_id,
                &b.0.agent_id,
                &b.0.model,
            ))
        });
        let sum = pins.iter().filter_map(|(_, burn)| *burn).sum();
        (pins, sum)
    }

    /// Whether `pool_id`'s latest round on the pin's current account, run within `dwell_ms`, marked it. A pin
    /// marked on one account and moved keeps its key, so the round read is the one of the account it now sits on.
    pub fn shed_marked(&self, pool_id: &str, key: &PinKey, now: i64, dwell_ms: i64) -> bool {
        let Some(pin) = self.pin(key) else {
            return false;
        };
        self.shed_rounds
            .get(&(pool_id.to_string(), pin.account_id.clone()))
            .is_some_and(|(at, marks)| now - at < dwell_ms && marks.contains(&pin.key()))
    }

    /// When `pool_id`'s latest round on the account ran.
    pub fn last_shed(&self, pool_id: &str, account_id: &str) -> Option<i64> {
        self.shed_rounds
            .get(&(pool_id.to_string(), account_id.to_string()))
            .map(|(at, _)| *at)
    }

    /// When the newest round of any pool on the account ran, from the account's own stamp.
    pub fn newest_shed(&self, account_id: &str) -> Option<i64> {
        self.newest_shed.get(account_id).copied()
    }

    /// Writes the pool's round and moves the account's stamp to `at`.
    pub(crate) fn record_shed(
        &mut self,
        pool_id: &str,
        account_id: &str,
        at: i64,
        marks: Vec<PinKey>,
    ) {
        self.shed_rounds.insert(
            (pool_id.to_string(), account_id.to_string()),
            (at, marks.into_iter().collect()),
        );
        self.newest_shed.insert(account_id.to_string(), at);
    }

    /// A late failure from account A must not remove a newer pin on account B, so both have to match.
    pub fn invalidate_pin(&mut self, pin: &PinRef, account_id: &str) -> bool {
        let key = PinKey {
            key_id: pin.key_id.clone(),
            session_id: pin.session_id.clone(),
            agent_id: pin.agent_id.clone(),
            model: self.canonical(&pin.model).to_string(),
        };
        let matches = self
            .pins
            .get(&key)
            .is_some_and(|p| p.epoch == pin.epoch && p.account_id == account_id);
        if matches {
            self.take_pin(&key);
        }
        matches
    }

    /// Records the resolution and moves every pin and every model cooldown keyed on `spelling` to `dated_id`
    /// in one step, so nothing selects between the two halves: two rows that meet merge as the store merges
    /// them (the later cooldown deadline wins, the pin with the newer `last_used_at` wins). The rekey carries
    /// the deletes under the old key, the upserts under the new one, and the resolution row itself.
    ///
    /// The rows here are found and moved by their raw stored key rather than through `pin`/`cooldown`'s own
    /// canonicalizing lookups: those rows were written under the literal `spelling` before this resolution
    /// existed, so only a raw key finds them. A spelling re-pointed to a newer dated id moves only the rows
    /// still keyed on the literal spelling; rows already moved under the previous dated id stay there, since
    /// a new snapshot is a new model.
    pub fn apply_resolution(&mut self, spelling: &str, dated_id: &str, at: i64) -> RekeyWrite {
        if let Some(current) = self.resolutions.get(spelling) {
            // A caller whose own bookkeeping of the last move it saw was lost (its future dropped between
            // this state moving and its own record of the move landing) can resubmit a snapshot older than
            // what already stands; walking pins and cooldowns back to it would be a real regression, so the
            // move is ignored and the mapping is reasserted as it already stands.
            if snapshot_date(dated_id) <= snapshot_date(current) {
                return RekeyWrite {
                    resolution: ResolutionRow {
                        spelling: spelling.to_string(),
                        dated_id: current.clone(),
                        resolved_at: at,
                    },
                    delete_pins: Vec::new(),
                    upsert_pins: Vec::new(),
                    delete_cooldowns: Vec::new(),
                    upsert_cooldowns: Vec::new(),
                };
            }
        }
        let mut rekey = RekeyWrite {
            resolution: ResolutionRow {
                spelling: spelling.to_string(),
                dated_id: dated_id.to_string(),
                resolved_at: at,
            },
            delete_pins: Vec::new(),
            upsert_pins: Vec::new(),
            delete_cooldowns: Vec::new(),
            upsert_cooldowns: Vec::new(),
        };
        self.resolutions
            .insert(spelling.to_string(), dated_id.to_string());
        self.affinity
            .write()
            .expect("affinity lock poisoned")
            .learn_resolution(spelling, dated_id);

        let moved_pins: Vec<PinRow> = self
            .pins
            .values()
            .filter(|p| p.model == spelling)
            .cloned()
            .collect();
        for pin in moved_pins {
            let spelled = pin.key();
            // The pin keeps its epoch under the dated id, so a renewal it had still fences its release there.
            let renewed = self.renewed.contains(&spelled);
            self.take_pin(&spelled);
            rekey.delete_pins.push(spelled);
            let dated_key = PinKey {
                model: dated_id.to_string(),
                ..pin.key()
            };
            let keep_existing = self
                .pins
                .get(&dated_key)
                .is_some_and(|existing| existing.last_used_at >= pin.last_used_at);
            if !keep_existing {
                let moved = PinRow {
                    model: dated_id.to_string(),
                    lane: relaned(&pin.lane, spelling, dated_id),
                    ..pin
                };
                if let Some(replaced) = self.pins.insert(dated_key.clone(), moved.clone()) {
                    self.spend_mark(&replaced);
                }
                if renewed {
                    self.renewed.insert(dated_key);
                } else {
                    self.renewed.remove(&dated_key);
                }
                rekey.upsert_pins.push(moved);
            }
        }

        let old_scope = format!("model:{spelling}");
        let new_scope = format!("model:{dated_id}");
        let moved_cooldowns: Vec<CooldownRow> = self
            .cooldowns
            .values()
            .filter(|c| c.scope == old_scope)
            .cloned()
            .collect();
        for row in moved_cooldowns {
            self.cooldowns
                .remove(&(row.account_id.clone(), row.scope.clone()));
            rekey
                .delete_cooldowns
                .push((row.account_id.clone(), row.scope.clone()));
            let account_id = row.account_id.clone();
            let moved = CooldownRow {
                scope: new_scope.clone(),
                ..row
            };
            if self.record_cooldown(moved) {
                let merged = self
                    .cooldowns
                    .get(&(account_id, new_scope.clone()))
                    .cloned()
                    .expect("record_cooldown just inserted or updated this row");
                rekey.upsert_cooldowns.push(merged);
            }
        }
        rekey
    }

    /// Every agent's pin of the session, released together: the session leaves every account it was on, not
    /// just its main turn's.
    pub fn release_session(&mut self, key_id: &str, session_id: &str) -> Vec<PinKey> {
        let gone: Vec<_> = self
            .pins
            .keys()
            .filter(|k| k.key_id == key_id && k.session_id == session_id)
            .cloned()
            .collect();
        for key in &gone {
            self.take_pin(key);
        }
        gone
    }

    pub fn pins(&self) -> Vec<PinRow> {
        let mut v: Vec<_> = self.pins.values().cloned().collect();
        v.sort_by(|a, b| {
            (&a.key_id, &a.session_id, &a.agent_id, &a.model).cmp(&(
                &b.key_id,
                &b.session_id,
                &b.agent_id,
                &b.model,
            ))
        });
        v
    }

    /// Every pin's key id, in no order: the per-pool count reads these and skips the clone and sort `pins()` pays.
    pub(crate) fn pin_key_ids(&self) -> impl Iterator<Item = &str> + '_ {
        self.pins.values().map(|p| p.key_id.as_str())
    }

    /// Every pin of one key, session and model, under any agent: the cohort-aware hold's own lookup, which
    /// runs per held selection and so skips the clone and sort a listing pays for in `pins()`.
    pub(crate) fn session_pins<'a>(
        &'a self,
        key_id: &'a str,
        session_id: &'a str,
        model: &'a str,
    ) -> impl Iterator<Item = &'a PinRow> {
        self.pins
            .values()
            .filter(move |p| p.key_id == key_id && p.session_id == session_id && p.model == model)
    }

    /// Expired cooldowns, pins past their pool's idle TTL, pins whose key or pool is gone, bucket windows read
    /// longer than `BUCKET_TTL_MS` ago, and everything held against an account the registry no longer has. An id
    /// is reusable, so state the deleted account left behind would otherwise be inherited by whatever is created
    /// under that id next: a two-day cooldown would keep a brand new account out of every pool it joins. A delete
    /// and a create under one id can both land before one reload, so a row is held against the account only while
    /// the snapshot's holder carries the incarnation it names. A pin names its key's creation time too, and one
    /// whose key or account was replaced reads `deleted`: its holder is gone, whatever took the id.
    pub fn purge(&mut self, snapshot: &RegistrySnapshot, now: i64) -> Purged {
        let gone = |account_id: &str, incarnation: u64| {
            snapshot
                .account(account_id)
                .is_none_or(|a| a.incarnation != incarnation)
        };
        let cooldowns: Vec<_> = self
            .cooldowns
            .iter()
            .filter(|((account_id, _), c)| c.until <= now || gone(account_id, c.incarnation))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &cooldowns {
            self.cooldowns.remove(key);
        }
        let pins: Vec<_> = self
            .pins
            .iter()
            .filter_map(|(k, p)| {
                let key_replaced = snapshot
                    .key(&p.key_id)
                    .is_some_and(|key| key.created_at != p.key_generation);
                if gone(&p.account_id, p.incarnation) || key_replaced {
                    return Some((k.clone(), "deleted"));
                }
                let ttl = snapshot
                    .key(&p.key_id)
                    .and_then(|k| snapshot.pool(&k.pool_id))
                    .map(affinity_ttl_ms);
                match ttl {
                    // No key or no pool: nothing serves the pin any more.
                    None => Some((k.clone(), "unserved")),
                    // A zero affinity is a TTL every pin is past: the pool keeps no session on an account.
                    Some(0) => Some((k.clone(), "expired")),
                    Some(ttl) if now - p.last_used_at > ttl => Some((k.clone(), "expired")),
                    Some(_) => None,
                }
            })
            .collect();
        for (key, _) in &pins {
            self.take_pin(key);
        }
        // None of these needs a deletion written: the overage rows cascade with the account and the next holder's
        // first reading replaces one left under another incarnation, the window and credits rows cascade too and
        // the purge tick drops one of another incarnation, and charges, round-robin cursors and shedding rounds
        // are held in memory only. Samples, charges and cursors are keyed by id alone, so the
        // reload drops those of a holder the id has moved past.
        let absent = |account_id: &str| snapshot.account(account_id).is_none();
        self.overage
            .retain(|account_id, evidence| !gone(account_id, evidence.incarnation));
        self.credits
            .retain(|account_id, state| !gone(account_id, state.incarnation));
        self.samples.retain(|account_id, _| !absent(account_id));
        // A bucket names a canonical model, so without the cut every model an account was ever asked for would keep
        // its buckets for the account's life.
        let expired_before = now - BUCKET_TTL_MS;
        for sample in self.samples.values_mut() {
            sample.windows.retain(|window, reading| {
                !is_bucket_window(window) || reading.observed_at >= expired_before
            });
        }
        self.charges.retain(|account_id, _| !absent(account_id));
        self.round_robin.retain(|(pool_id, _, cohort), _| {
            snapshot.pool(pool_id).is_some() && cohort.split(',').all(|id| !absent(id))
        });
        // A pool's marks are read within its own dwell alone, and a pool that left the registry has no round to
        // read. The account's newest round is read by the one pool holding the account, so its stamp stays until
        // that pool's dwell has passed.
        let dwell_ms = |pool: &PoolRow| seconds_ms(pool.shedding.source_dwell_seconds);
        self.shed_rounds.retain(|(pool_id, _), (at, _)| {
            snapshot
                .pool(pool_id)
                .is_some_and(|pool| now - *at < dwell_ms(pool))
        });
        self.newest_shed.retain(|account_id, at| {
            snapshot
                .pools()
                .find(|pool| {
                    snapshot
                        .members(&pool.id)
                        .iter()
                        .any(|m| &m.account_id == account_id)
                })
                .is_some_and(|pool| now - *at < dwell_ms(pool))
        });
        let ttl_of = |key_id: &str| -> Option<i64> {
            snapshot
                .key(key_id)
                .and_then(|k| snapshot.pool(&k.pool_id))
                .map(affinity_ttl_ms)
        };
        let affinity = self
            .affinity
            .write()
            .expect("affinity lock poisoned")
            .purge(now, ttl_of, |account_id| {
                snapshot.account(account_id).map(|a| a.incarnation)
            });
        self.member_spend
            .retain(|(pool_id, account_id, period), _| {
                member_spend_matches(snapshot, pool_id, account_id, *period)
            });
        Purged {
            affinity,
            cooldowns,
            pins,
        }
    }
}

/// A moved pin's lane under the dated id: the relay composes the next request's lane from the model it admits,
/// which is the dated id from now on, so a lane still spelling the old model would read as another cache.
fn relaned(lane: &str, spelling: &str, dated_id: &str) -> String {
    match lane
        .strip_prefix(spelling)
        .filter(|rest| rest.starts_with('|'))
    {
        Some(rest) => format!("{dated_id}{rest}"),
        None => lane.to_string(),
    }
}

/// Whether the snapshot's member for `(pool_id, account_id)` still carries a budget over `period`: a member the
/// pool no longer has, or one with no budget over that period, answers false, which retires the row rather than
/// obstructing on a period nobody configured any more.
fn member_spend_matches(
    snapshot: &RegistrySnapshot,
    pool_id: &str,
    account_id: &str,
    period: BudgetPeriod,
) -> bool {
    snapshot
        .members(pool_id)
        .iter()
        .find(|m| m.account_id == account_id)
        .is_some_and(|m| m.budget(period).is_some())
}

/// The end the member's budgets hold it to at `now`: among the periods whose budget the matching row (summed over
/// that period, its bounds holding `now`) has reached, the latest `period_end`, since a day's end clears nothing
/// while the month's budget stands. `None` for a member with no budget reached.
pub fn budget_deadline(member: &PoolMember, spends: &[&MemberSpend], now: i64) -> Option<i64> {
    member
        .budgets()
        .filter_map(|(period, budget)| {
            spends
                .iter()
                .find(|s| s.period == period && s.period_start <= now && now < s.period_end)
                .filter(|s| s.spent_usd >= budget)
                .map(|s| s.period_end)
        })
        .max()
}

/// The start of the current overage stretch, on first evidence and on every replacement alike: an account out of
/// overage has none, one already in it keeps the start it had, and one just entering falls back to the time the
/// evidence was observed.
fn since_of(current: Option<&OverageEvidence>, evidence: &OverageEvidence) -> Option<i64> {
    if !evidence.in_use {
        return None;
    }
    match current {
        Some(current) if current.in_use => current.since,
        _ => evidence.since.or(Some(evidence.observed_at)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn burning(session: &str, burn: f64) -> (PinRow, Option<f64>) {
        let pin = PinRow {
            key_id: "k".into(),
            session_id: session.into(),
            agent_id: "main".into(),
            model: "m".into(),
            account_id: "a".into(),
            epoch: 1,
            created_at: 0,
            last_used_at: 0,
            key_generation: 0,
            incarnation: 1,
            burn: PinBurn {
                input: burn,
                ..PinBurn::default()
            },
            burn_at: None,
            burn_banded: PinBurn {
                input: burn,
                ..PinBurn::default()
            },
            last_input_tokens: 0,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
            lane: crate::lane_key("m", "standard", "none", "-"),
        };
        (pin, Some(burn))
    }

    fn unpriced(session: &str) -> (PinRow, Option<f64>) {
        (burning(session, 0.0).0, None)
    }

    fn sessions(marks: &[PinKey]) -> Vec<&str> {
        marks.iter().map(|k| k.session_id.as_str()).collect()
    }

    /// A report of one message boundary under `model`, as a completed answer on account "a" files it.
    fn boundary_report(model: &str) -> crate::affinity::AffinityReport {
        crate::affinity::AffinityReport {
            account_id: "a".into(),
            incarnation: 1,
            key_id: "k".into(),
            key_generation: 1,
            canonical_model: model.into(),
            lane: crate::lane_key(model, "standard", "none", "-"),
            root: None,
            static_boundaries: std::sync::Arc::from([]),
            message_boundaries: std::sync::Arc::from(["b1".to_string()]),
            static_sizes: Vec::new(),
            static_ttl_ms: None,
            message_ttl_ms: None,
            cache_touched: false,
            prefix_tokens: 0,
            now: 1_000,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            uniform_ttl_ms: None,
        }
    }

    /// Whether a report filed under `spelling` lands at `dated`, the dated id the map was told it resolves to.
    /// `dated` is no spelling of its own, so the lookup below reads that exact key.
    fn lands_at_the_dated_id(state: &RoutingState, spelling: &str) -> bool {
        let map = state.affinity();
        let mut map = map.write().unwrap();
        map.record(&boundary_report(spelling));
        map.entry("dated", "b1").is_some()
    }

    #[test]
    fn forgetting_an_accounts_sample_forgets_its_credits_state_too() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.record_credits(
            "a",
            CreditsState {
                enabled: false,
                disabled_reason: Some("out_of_credits".into()),
                utilization: None,
                observed_at: 1_000,
                incarnation: 1,
            },
        );
        state.forget_sample("a");
        assert_eq!(state.credits("a"), None);
    }

    #[test]
    fn a_loaded_resolution_files_a_report_under_the_spelling_at_its_dated_id() {
        let state = RoutingState::from_load(RoutingLoad {
            resolutions: vec![ResolutionRow {
                spelling: "spelling".into(),
                dated_id: "dated".into(),
                resolved_at: 0,
            }],
            ..RoutingLoad::default()
        });
        assert!(lands_at_the_dated_id(&state, "spelling"));
    }

    #[test]
    fn a_resolution_applied_after_the_load_files_a_later_report_under_the_spelling_at_its_dated_id()
    {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.apply_resolution("spelling", "dated", 0);
        assert!(lands_at_the_dated_id(&state, "spelling"));
    }

    #[test]
    fn a_round_marks_in_order_to_the_share_and_never_the_last_pin_or_anything_for_a_zero_target() {
        let four = [
            burning("cold-3", 3.0),
            burning("warm-4", 4.0),
            burning("warm-2", 2.0),
            burning("warm-1", 1.0),
        ];
        assert_eq!(
            sessions(&shed_marks(&four, 0.5 * 10.0)),
            ["cold-3", "warm-4"],
            "marked in the round's order until the marked burn, 7 of 10, reaches half"
        );
        assert!(
            shed_marks(&[burning("only", 100.0)], 0.5 * 100.0).is_empty(),
            "a one-pin account sheds nothing"
        );
        assert_eq!(
            sessions(&shed_marks(
                &[burning("cold-1", 1.0), burning("warm-9", 9.0)],
                0.5 * 10.0
            )),
            ["cold-1"],
            "the warm pin is the last unmarked one, whatever the target still asks"
        );
        let idle = [
            burning("a", 0.0),
            burning("b", 0.0),
            burning("c", 0.0),
            burning("d", 0.0),
        ];
        assert!(
            shed_marks(&idle, 0.5 * 0.0).is_empty(),
            "no burn in the window is a zero target"
        );
        assert!(
            shed_marks(&four, 0.0 * 10.0).is_empty(),
            "a share of 0 is a zero target"
        );
    }

    #[test]
    fn an_unpriced_pin_adds_nothing_to_the_marked_burn_is_never_marked_and_still_counts_as_a_pin() {
        let mixed = [
            burning("cold-3", 3.0),
            unpriced("cold-guess"),
            burning("warm-4", 4.0),
            unpriced("warm-guess"),
        ];
        assert_eq!(
            sessions(&shed_marks(&mixed, 0.5 * 7.0)),
            ["cold-3", "warm-4"],
            "the target is half the priced burn; the unpriced pins are skipped, not marked"
        );
        assert_eq!(
            sessions(&shed_marks(
                &[burning("cold-5", 5.0), unpriced("guess")],
                0.5 * 5.0
            )),
            ["cold-5"],
            "the unpriced pin is the last unmarked one, so the priced pin can be marked"
        );
        assert!(
            shed_marks(&[unpriced("a"), unpriced("b"), unpriced("c")], 1.0).is_empty(),
            "a positive target the loop reaches marks no unpriced pin"
        );
    }

    #[test]
    fn a_rate_left_none_prices_a_burn_only_when_the_pin_burns_none_of_that_type() {
        let table = RateTable {
            rows: HashMap::from([(
                "claude-guess".to_string(),
                Rates {
                    input: Some(2.0),
                    cache_read: None,
                    write_5m: Some(0.0),
                    write_1h: Some(0.0),
                    output: Some(1.0),
                },
            )]),
            fallback: RateTable::unit().fallback,
        };
        let input_only = PinBurn {
            input: 3.0,
            ..PinBurn::default()
        };
        assert_eq!(table.price_burn("claude-guess", &input_only), Some(6.0));
        let with_reads = PinBurn {
            input: 3.0,
            cache_read: 1.0,
            ..PinBurn::default()
        };
        assert_eq!(
            table.price_burn("claude-guess", &with_reads),
            None,
            "a burned type without a rate"
        );
        assert_eq!(
            table.price(
                "claude-guess",
                PinUsage {
                    input: 1,
                    ..PinUsage::default()
                }
            ),
            Some(2.0)
        );
        assert_eq!(
            table.price(
                "claude-guess",
                PinUsage {
                    cache_read: 1,
                    ..PinUsage::default()
                }
            ),
            None
        );
        assert_eq!(
            table.price(
                "gpt-5",
                PinUsage {
                    cache_read: 1,
                    ..PinUsage::default()
                }
            ),
            Some(1.0),
            "the fallback"
        );
    }

    #[test]
    fn a_dated_model_id_prices_at_its_undated_row_and_an_unlisted_one_at_the_fallback() {
        let flat = |r: f64| Rates {
            input: Some(r),
            cache_read: Some(r),
            write_5m: Some(r),
            write_1h: Some(r),
            output: Some(r),
        };
        let table = RateTable {
            rows: [
                (
                    "claude-sonnet-4-5".to_string(),
                    Rates {
                        input: Some(3.0),
                        cache_read: Some(0.25),
                        write_5m: Some(3.75),
                        write_1h: Some(6.0),
                        output: Some(15.0),
                    },
                ),
                (
                    "claude-fable-5-1".to_string(),
                    Rates {
                        input: Some(5.0),
                        cache_read: Some(0.5),
                        write_5m: Some(6.25),
                        write_1h: Some(10.0),
                        output: Some(25.0),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            fallback: flat(1.0),
        };
        let usage = PinUsage {
            input: 10,
            cache_read: 100,
            write_5m: 4,
            write_1h: 2,
            output: 3,
        };
        assert_eq!(
            table.price("claude-sonnet-4-5-20250929", usage),
            Some(30.0 + 25.0 + 15.0 + 12.0 + 45.0),
            "the dated id prices at the undated sonnet row"
        );
        assert_eq!(
            table.price("claude-fable-5-1-20260601", usage),
            Some(50.0 + 50.0 + 25.0 + 20.0 + 75.0),
            "and the dated fable id at its own row, not the fallback"
        );
        assert_eq!(
            table.price("claude-opus-5-20260101", usage),
            Some(10.0 + 100.0 + 4.0 + 2.0 + 3.0),
            "a model no row names prices at the fallback"
        );
    }

    #[test]
    fn a_head_sample_for_one_model_keeps_the_buckets_of_another_and_an_older_reading_never_replaces_a_newer_one(
    ) {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        let reading = |utilization: f64, observed_at: i64| WindowReading {
            utilization,
            reset: None,
            observed_at,
        };
        state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 2_000,
                ..Default::default()
            }
            .with("itpm:x", reading(0.4, 2_000)),
        );
        state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 2_100,
                ..Default::default()
            }
            .with("itpm:y", reading(0.6, 2_100)),
        );
        let stored = state.sample("a").expect("a sample was recorded");
        assert_eq!(
            stored.reading("itpm:x").map(|r| r.utilization),
            Some(0.4),
            "the head sample for y kept x's bucket"
        );
        assert_eq!(stored.reading("itpm:y").map(|r| r.utilization), Some(0.6));

        let moved = state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 1_000,
                ..Default::default()
            }
            .with("itpm:x", reading(0.9, 1_000)),
        );
        assert!(
            moved.is_empty(),
            "1_000 is older than the 2_000 x already carries"
        );
        assert_eq!(
            state
                .sample("a")
                .unwrap()
                .reading("itpm:x")
                .map(|r| r.utilization),
            Some(0.4)
        );

        let moved = state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 3_000,
                ..Default::default()
            }
            .with("itpm:x", reading(0.1, 3_000)),
        );
        assert!(!moved.is_empty());
        assert_eq!(
            state
                .sample("a")
                .unwrap()
                .reading("itpm:x")
                .map(|r| r.utilization),
            Some(0.1)
        );
    }

    #[test]
    fn a_bucket_window_past_its_ttl_leaves_at_the_purge_and_the_oldest_leaves_at_the_bound() {
        let reading = |observed_at: i64| WindowReading {
            utilization: 0.5,
            reset: None,
            observed_at,
        };
        // The account is present, so only the bucket cut can empty its map.
        let snapshot = RegistrySnapshot::from_load(
            phoebis_store::RegistryLoad {
                accounts: vec![phoebis_store::AccountRow {
                    id: "a".into(),
                    kind: AccountKind::ApiKey,
                    label: "a".into(),
                    enabled: true,
                    identity: None,
                    lifecycle_generation: 1,
                    incarnation: 1,
                    revision: 1,
                    state: AccountState::Ready,
                }],
                ..Default::default()
            },
            [0; 32],
            0,
            1,
        );
        let now = 20_000 + BUCKET_TTL_MS;
        let mut state = RoutingState::from_load(RoutingLoad::default());
        // Seeded into the map directly: the insert bound alone would keep 64 of them.
        let sample = state.samples.entry("a".into()).or_default();
        for i in 0..10_000_i64 {
            let prefix = ["itpm", "otpm", "rpm"][(i % 3) as usize];
            sample
                .windows
                .insert(format!("{prefix}:model-{i}"), reading(1_000 + i));
        }
        sample.windows.insert(FAST_INPUT.into(), reading(1_000));
        sample.windows.insert(FAST_OUTPUT.into(), reading(1_000));
        sample.windows.insert(FIVE_HOUR.into(), reading(1_000));
        sample.windows.insert("rpm:live".into(), reading(now - 1));
        state.purge(&snapshot, now);
        let windows = &state.sample("a").unwrap().windows;
        assert_eq!(windows.len(), 2, "every expired bucket left");
        assert_eq!(
            windows.keys().map(String::as_str).collect::<Vec<_>>(),
            [FIVE_HOUR, "rpm:live"],
            "the live bucket and the plan window stayed"
        );

        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.record_sample(
            "a",
            UtilizationSample::default().with(FIVE_HOUR, reading(1_000)),
        );
        for i in 0..MAX_BUCKETS_PER_ACCOUNT as i64 {
            state.record_sample(
                "a",
                UtilizationSample::default().with(&format!("rpm:model-{i}"), reading(2_000 + i)),
            );
        }
        state.record_sample(
            "a",
            UtilizationSample::default().with("rpm:model-64", reading(3_000)),
        );
        let sample = state.sample("a").unwrap();
        assert!(
            sample.reading("rpm:model-0").is_none(),
            "the oldest bucket left"
        );
        assert!(sample.reading("rpm:model-1").is_some());
        assert!(sample.reading("rpm:model-64").is_some());
        assert!(
            sample.reading(FIVE_HOUR).is_some(),
            "a plan window never counts toward the bound, however old"
        );
        assert_eq!(sample.windows.len(), MAX_BUCKETS_PER_ACCOUNT + 1);
    }

    #[test]
    fn a_bucket_past_its_reset_is_refunded_and_a_bucket_without_a_reset_lives_a_minute() {
        let sample = UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            "itpm:x",
            WindowReading {
                utilization: 0.9,
                reset: Some(1_400),
                observed_at: 1_000,
            },
        )
        .with(
            "itpm:y",
            WindowReading {
                utilization: 0.3,
                reset: None,
                observed_at: 1_000,
            },
        );
        assert_eq!(
            sample.worst_bucket(1_300).map(|r| r.utilization),
            Some(0.9),
            "x stands and is worse than y before its reset"
        );
        assert_eq!(
            sample.worst_bucket(1_500).map(|r| r.utilization),
            Some(0.3),
            "past x's reset it is refunded, so y answers as the worst bucket standing"
        );
        assert_eq!(
            sample
                .worst_bucket(1_000 + BUCKET_LIFE_MS - 1)
                .map(|r| r.utilization),
            Some(0.3),
            "y names no reset and stands for a minute"
        );
        assert_eq!(
            sample.worst_bucket(1_000 + BUCKET_LIFE_MS),
            None,
            "a minute after y was read nothing stands"
        );
    }

    const CLOSE: f64 = 1e-9;
    const HOUR_MS: i64 = 3_600_000;

    fn read(state: &mut RoutingState, window: &str, utilization: f64, reset: Option<i64>, at: i64) {
        state.record_sample(
            "a",
            UtilizationSample {
                observed_at: at,
                ..Default::default()
            }
            .with(
                window,
                WindowReading {
                    utilization,
                    reset,
                    observed_at: at,
                },
            ),
        );
    }

    fn utilization(state: &RoutingState, window: &str, now: i64) -> f64 {
        state
            .predicted("a", window, now)
            .expect("the account holds a reading of the window")
            .utilization
    }

    #[test]
    fn a_weekly_reading_past_its_reset_reads_zero_with_the_reset_advanced_by_whole_weeks() {
        let reset = 10 * WEEK_MS;
        let observed_at = reset - HOUR_MS;
        let mut state = RoutingState::from_load(RoutingLoad::default());
        read(&mut state, SEVEN_DAY, 0.8, Some(reset), observed_at);
        read(&mut state, SEVEN_DAY_FABLE, 0.9, Some(reset), observed_at);
        for (weeks, now) in [
            (0, reset + 5),
            (1, reset + WEEK_MS + 5),
            (3, reset + 3 * WEEK_MS + 5),
        ] {
            for window in [SEVEN_DAY, SEVEN_DAY_FABLE] {
                assert_eq!(
                    state.predicted("a", window, now),
                    Some(WindowReading {
                        utilization: 0.0,
                        reset: Some(reset + (weeks + 1) * WEEK_MS),
                        observed_at,
                    }),
                    "{window} past its reset by {weeks} whole weeks"
                );
            }
        }
    }

    #[test]
    fn a_five_hour_reading_past_its_reset_reads_zero_with_no_successor() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        read(&mut state, FIVE_HOUR, 0.5, Some(5_000), 1_000);
        state.charge("a", 3_000, &[(FIVE_HOUR, 0.25), (SEVEN_DAY, 0.0)]);
        assert!((utilization(&state, FIVE_HOUR, 4_000) - 0.75).abs() < CLOSE);
        assert_eq!(
            state.predicted("a", FIVE_HOUR, 5_000),
            Some(WindowReading {
                utilization: 0.0,
                reset: None,
                observed_at: 1_000,
            }),
            "at the reset the window reads zero, and the next reading names the reset after it"
        );
        state.charge("a", 6_000, &[(FIVE_HOUR, 0.125), (SEVEN_DAY, 0.0)]);
        assert!(
            (utilization(&state, FIVE_HOUR, 7_000) - 0.125).abs() < CLOSE,
            "a request completed since the reset counts; the one before it does not"
        );
    }

    #[test]
    fn a_reading_before_its_reset_adds_the_routers_own_share_and_a_later_reading_drops_it() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        read(&mut state, FIVE_HOUR, 0.5, Some(HOUR_MS), 1_000);
        read(&mut state, SEVEN_DAY, 0.3, Some(WEEK_MS), 1_000);
        state.charge("a", 2_000, &[(FIVE_HOUR, 0.1), (SEVEN_DAY, 0.1)]);
        assert!((utilization(&state, FIVE_HOUR, 3_000) - 0.6).abs() < CLOSE);
        let moved = state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 2_500,
                ..Default::default()
            }
            .with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.55,
                    reset: Some(HOUR_MS),
                    observed_at: 2_500,
                },
            ),
        );
        assert_eq!(moved, vec![FIVE_HOUR.to_string()]);
        assert!(
            (utilization(&state, FIVE_HOUR, 3_000) - 0.55).abs() < CLOSE,
            "the newer reading carries the charge, so the charge is gone"
        );
        assert!(
            (utilization(&state, SEVEN_DAY, 3_000) - 0.4).abs() < CLOSE,
            "the 7-day window read nothing newer and keeps it"
        );
        state.charge("a", 2_600, &[(FIVE_HOUR, 0.1), (SEVEN_DAY, 0.1)]);
        assert!(
            (utilization(&state, FIVE_HOUR, 3_000) - 0.65).abs() < CLOSE,
            "a charge after the reading reads on top of it"
        );
    }

    #[test]
    fn a_reading_without_a_reset_holds_as_read_until_the_next() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        read(&mut state, FIVE_HOUR, 0.7, None, 1_000);
        assert_eq!(
            state.predicted("a", FIVE_HOUR, 1_000 + 3 * WEEK_MS),
            Some(WindowReading {
                utilization: 0.7,
                reset: None,
                observed_at: 1_000,
            }),
            "no reset named: the reading stands however old it is"
        );
        read(&mut state, FIVE_HOUR, 0.2, None, 2_000);
        assert!((utilization(&state, FIVE_HOUR, 1_000 + 3 * WEEK_MS) - 0.2).abs() < CLOSE);
    }

    #[test]
    fn the_fast_lane_rule_passes_a_plan_window_row_for_a_fast_request_alone() {
        const MODEL: &str = "claude-opus-5-5";
        let own_model = format!("model:{MODEL}");
        for fast in [false, true] {
            for plan in [FIVE_HOUR, SEVEN_DAY, "seven_day_opus"] {
                assert_eq!(
                    cooldown_holds("account", plan, MODEL, fast),
                    !fast,
                    "an account row at {plan}, fast {fast}"
                );
            }
            assert!(
                cooldown_holds("account", "retry_after", MODEL, fast),
                "an account row no plan window names, fast {fast}"
            );
            assert_eq!(
                cooldown_holds("fast", "fast", MODEL, fast),
                fast,
                "the fast lane's own row"
            );
            assert_eq!(
                cooldown_holds("fast_credits", "out_of_credits", MODEL, fast),
                fast,
                "the fast lane's credits row"
            );
            assert!(
                cooldown_holds(&own_model, "retry_after", MODEL, fast),
                "the model's own row, fast {fast}"
            );
            assert!(
                !cooldown_holds("model:claude-sonnet-5", "retry_after", MODEL, fast),
                "another model's row, fast {fast}"
            );
        }
        assert!(!is_plan_window("retry_after") && !is_plan_window("overage"));

        let mut state = RoutingState::from_load(RoutingLoad::default());
        let row = |scope: &str, until: i64, window: &str| CooldownRow {
            account_id: "a".into(),
            scope: scope.into(),
            until,
            set_at: 0,
            reason: "rate_limit".into(),
            window: window.into(),
            source_request_id: None,
            incarnation: 1,
        };
        state.record_cooldown(row("account", 5_000, FIVE_HOUR));
        state.record_cooldown(row("fast", 3_000, "fast"));
        state.record_cooldown(row(&own_model, 4_000, "retry_after"));
        let until = |fast: bool| {
            state
                .blocking_cooldown("a", MODEL, 1_000, fast)
                .map(|r| r.until)
        };
        assert_eq!(
            until(false),
            state.cooldown("a", MODEL, 1_000).map(|r| r.until),
            "a standard request reads what the model's cooldown reads"
        );
        assert_eq!(until(false), Some(5_000));
        assert_eq!(
            until(true),
            Some(4_000),
            "a fast request passes the plan window and reads the later of the fast and the model row"
        );
        state.record_cooldown(row("fast_credits", 4_500, "out_of_credits"));
        let until = |fast: bool| {
            state
                .blocking_cooldown("a", MODEL, 1_000, fast)
                .map(|r| r.until)
        };
        assert_eq!(
            (until(false), until(true)),
            (Some(5_000), Some(4_500)),
            "the credits row holds a fast request alone, beside the fast lane's rate limit"
        );
    }

    #[test]
    fn a_model_row_recovers_on_its_models_own_week_whatever_window_the_row_names() {
        let row = |window: &str| CooldownRow {
            account_id: "a".into(),
            scope: "model:claude-fable-5-1".into(),
            until: 10_000,
            set_at: 1_000,
            reason: "rate_limit".into(),
            window: window.into(),
            source_request_id: None,
            incarnation: 1,
        };
        let sample = |readings: &[(&str, f64)]| {
            readings.iter().fold(
                UtilizationSample {
                    observed_at: 2_000,
                    ..Default::default()
                },
                |sample, (window, utilization)| {
                    sample.with(
                        window,
                        WindowReading {
                            utilization: *utilization,
                            reset: None,
                            observed_at: 2_000,
                        },
                    )
                },
            )
        };
        let recovered = |state: &RoutingState, readings: &[(&str, f64)]| {
            state
                .recovered_cooldowns("a", &sample(readings), 3_000)
                .into_iter()
                .map(|r| r.scope)
                .collect::<Vec<_>>()
        };
        // The spend cap's row keeps the upstream's claim, which names no family's week.
        for window in ["seven_day_overage_included", SEVEN_DAY_FABLE] {
            let mut state = RoutingState::from_load(RoutingLoad::default());
            state.record_cooldown(row(window));
            assert_eq!(
                recovered(&state, &[(SEVEN_DAY_FABLE, 0.5)]),
                ["model:claude-fable-5-1"],
                "the Fable week has allowance again, whatever the row's window {window} says"
            );
            assert!(
                recovered(&state, &[(SEVEN_DAY, 0.5)]).is_empty(),
                "a shared week under 1.0 does not recover the model's row, window {window}"
            );
        }
        // Under the spend cap's own key alone: a row naming the Fable week reads one key for both readings.
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.record_cooldown(row("seven_day_overage_included"));
        assert!(
            recovered(
                &state,
                &[("seven_day_overage_included", 0.5), (SEVEN_DAY_FABLE, 1.0)]
            )
            .is_empty(),
            "a reading under the row's window text says nothing while the Fable week stays spent"
        );
    }
}
